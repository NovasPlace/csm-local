//! `csm link FOLDER --into PROJECT`: sessions started in FOLDER (or below it)
//! count as part of PROJECT. For example, Claude Code launched one folder
//! above the game that Codex works in.
//!
//! Linking moves FOLDER's existing memories and journal into PROJECT, so
//! both agents share one briefing, one set of checkpoints and one search.
//! File paths are rewritten so they stay correct relative to the new root.
//! Removing a link affects future sessions only; moved data stays moved.

use rusqlite::{params, OptionalExtension};

use crate::capture::{absolute, rel_path};
use crate::store::Store;
use crate::time::now_ms;
use crate::{Error, Result};

/// What a link moved, or would move.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LinkCounts {
    pub memories: usize,
    pub events: usize,
}

/// SQL matching `project` equal to ?1 or inside it.
const UNDER: &str = "(project = ?1 OR substr(project, 1, length(?1) + 1) = ?1 || '/')";

impl Store {
    /// The project a resolved folder key belongs to: the target of the most
    /// specific link covering it, or the key itself.
    pub fn canonical_project(&self, key: &str) -> Result<String> {
        let linked: Option<String> = self
            .conn
            .query_row(
                "SELECT project FROM csm_project_links
                 WHERE alias = ?1 OR substr(?1, 1, length(alias) + 1) = alias || '/'
                 ORDER BY length(alias) DESC LIMIT 1",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(linked.unwrap_or_else(|| key.to_string()))
    }

    /// Every link, as (folder, project).
    pub fn links(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT alias, project FROM csm_project_links ORDER BY alias")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// What linking `folder` would move.
    pub fn link_preview(&self, folder: &str) -> Result<LinkCounts> {
        let count = |table: &str| -> Result<usize> {
            Ok(self
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM {table} WHERE {UNDER}"), [folder], |r| r.get::<_, i64>(0))?
                as usize)
        };
        Ok(LinkCounts { memories: count("memories")?, events: count("events")? })
    }

    /// Links `folder` into `project` and moves its data there.
    pub fn link_project(&self, folder: &str, project: &str) -> Result<LinkCounts> {
        let project = self.canonical_project(project)?;
        let inside = |a: &str, b: &str| a == b || a.starts_with(&format!("{b}/"));
        if inside(&project, folder) {
            return Err(Error::Invalid(format!(
                "{project} is {folder} or inside it; link a folder into a project outside it"
            )));
        }
        let tx = self.immediate()?;
        let memories =
            tx.execute(&format!("UPDATE memories SET project = ?2 WHERE {UNDER}"), params![folder, project])?;
        // Paths in moved events were relative to their old project root.
        let mut moved: Vec<(i64, String, String, String)> = Vec::new();
        {
            let mut stmt = tx.prepare(&format!("SELECT id, project, files, summary FROM events WHERE {UNDER}"))?;
            let rows = stmt.query_map([folder], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            for row in rows {
                moved.push(row?);
            }
        }
        let roots = [project.clone()];
        for (id, old_project, files, summary) in &moved {
            let mut new_summary = summary.clone();
            let mut new_files = Vec::new();
            for file in files.lines().filter(|f| !f.is_empty()) {
                let rebased = rel_path(&roots, &project, &absolute(old_project, file));
                if rebased != file {
                    new_summary = new_summary.replace(file, &rebased);
                }
                new_files.push(rebased);
            }
            tx.execute(
                "UPDATE events SET project = ?2, files = ?3, summary = ?4 WHERE id = ?1",
                params![id, project, new_files.join("\n"), new_summary],
            )?;
        }
        for table in ["injections", "csm_file_touches", "csm_peers"] {
            tx.execute(&format!("UPDATE {table} SET project = ?2 WHERE {UNDER}"), params![folder, project])?;
        }
        // Links that pointed at the folder now point where it went.
        tx.execute("UPDATE csm_project_links SET project = ?2 WHERE project = ?1", params![folder, project])?;
        tx.execute(
            "INSERT INTO csm_project_links (alias, project, ts) VALUES (?1, ?2, ?3)
             ON CONFLICT (alias) DO UPDATE SET project = excluded.project, ts = excluded.ts",
            params![folder, project, now_ms()],
        )?;
        tx.commit()?;
        Ok(LinkCounts { memories, events: moved.len() })
    }

    /// Removes the link for `folder`. Returns whether there was one.
    pub fn unlink_project(&self, folder: &str) -> Result<bool> {
        Ok(self.conn.execute("DELETE FROM csm_project_links WHERE alias = ?1", [folder])? > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{EventQuery, NewEvent};
    use crate::store::NewMemory;

    const GAME: &str = "C:/work/Arcade/arcade-godot";
    const LAUNCH: &str = "C:/work/Arcade/arcade";

    fn edit(store: &Store, project: &str, session: &str, file: &str) {
        store
            .record(NewEvent {
                project: project.into(),
                host: "claude".into(),
                session_id: Some(session.into()),
                kind: "edit".into(),
                summary: format!("Edit {file}"),
                files: vec![file.into()],
                ..Default::default()
            })
            .unwrap();
    }

    #[test]
    fn linking_moves_history_and_rebases_paths() {
        let store = Store::open_in_memory().unwrap();
        edit(&store, LAUNCH, "c1", &format!("{GAME}/scripts/main.gd"));
        edit(&store, LAUNCH, "c1", "notes/plan.md");
        edit(&store, GAME, "x1", "scripts/ai_brain.gd");
        store
            .save(NewMemory {
                project: LAUNCH.into(),
                kind: "note".into(),
                content: "HANDOFF: war mode".into(),
                tags: vec![],
                importance: 3,
                source: None,
            })
            .unwrap();

        assert_eq!(store.link_preview(LAUNCH).unwrap(), LinkCounts { memories: 1, events: 2 });
        assert_eq!(store.link_project(LAUNCH, GAME).unwrap(), LinkCounts { memories: 1, events: 2 });

        let events = store.events(&EventQuery { project: Some(GAME), limit: 10, ..Default::default() }).unwrap();
        let summaries: Vec<&str> = events.iter().map(|e| e.summary.as_str()).collect();
        assert!(summaries.contains(&"Edit scripts/main.gd"), "{summaries:?}");
        assert!(summaries.contains(&format!("Edit {LAUNCH}/notes/plan.md").as_str()), "{summaries:?}");
        assert_eq!(store.list(crate::store::Filter { project: Some(GAME), kind: None, limit: 5 }).unwrap().len(), 1);

        assert_eq!(store.canonical_project(LAUNCH).unwrap(), GAME);
        assert_eq!(store.canonical_project(&format!("{LAUNCH}/tools")).unwrap(), GAME, "subfolders follow");
        assert_eq!(store.canonical_project("C:/work/Arcade/arcade2").unwrap(), "C:/work/Arcade/arcade2");
        assert_eq!(store.canonical_project(GAME).unwrap(), GAME);
    }

    #[test]
    fn links_cannot_point_into_themselves_and_chains_resolve() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.link_project("C:/a", "C:/a").is_err());
        assert!(store.link_project("C:/a", "C:/a/b").is_err());
        store.link_project("C:/b", "C:/c").unwrap();
        // Linking into an already-linked folder lands in its project.
        store.link_project("C:/a", "C:/b").unwrap();
        assert_eq!(store.canonical_project("C:/a").unwrap(), "C:/c");
        // Moving the target re-points links that pointed at it.
        store.link_project("C:/c", "C:/d").unwrap();
        assert_eq!(store.canonical_project("C:/b").unwrap(), "C:/d");
        assert_eq!(store.links().unwrap().len(), 3);
        assert!(store.unlink_project("C:/a").unwrap());
        assert_eq!(store.canonical_project("C:/a").unwrap(), "C:/a");
    }
}
