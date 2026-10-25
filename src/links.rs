//! `csm link FOLDER --into PROJECT`: sessions started in FOLDER count as part
//! of PROJECT. For example, Claude Code launched in the folder above the game
//! that Codex works in, or in a sibling notes folder.
//!
//! A link covers exactly that folder's project key, not other projects stored
//! below it, so linking a parent folder never sweeps in the repositories
//! inside it.
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

/// Links can point at a folder that is itself linked later; follow at most
/// this many hops (a cycle is refused when linking, this is only a backstop).
const MAX_HOPS: usize = 8;

impl Store {
    /// The project a resolved folder key belongs to: where its link points
    /// (following links of links), or the key itself.
    pub fn canonical_project(&self, key: &str) -> Result<String> {
        let mut current = key.to_string();
        for _ in 0..MAX_HOPS {
            let next: Option<String> = self
                .conn
                .query_row("SELECT project FROM csm_project_links WHERE alias = ?1", [&current], |r| r.get(0))
                .optional()?;
            match next {
                Some(next) if next != current => current = next,
                _ => break,
            }
        }
        Ok(current)
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
            Ok(self.conn.query_row(&format!("SELECT COUNT(*) FROM {table} WHERE project = ?1"), [folder], |r| {
                r.get::<_, i64>(0)
            })? as usize)
        };
        Ok(LinkCounts { memories: count("memories")?, events: count("events")? })
    }

    /// Links `folder` into `project` and moves its data there.
    pub fn link_project(&self, folder: &str, project: &str) -> Result<LinkCounts> {
        let project = self.canonical_project(project)?;
        if project == folder {
            return Err(Error::Invalid(format!("{folder} already is that project (or links back to it)")));
        }
        let tx = self.immediate()?;
        let memories = tx.execute("UPDATE memories SET project = ?2 WHERE project = ?1", params![folder, project])?;
        // Paths in moved events were relative to the folder; make them
        // relative to the project (or absolute when outside it).
        let mut moved: Vec<(i64, String, String)> = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT id, files, summary FROM events WHERE project = ?1")?;
            let rows = stmt.query_map([folder], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            for row in rows {
                moved.push(row?);
            }
        }
        let roots = [project.clone()];
        for (id, files, summary) in &moved {
            let renamed: Vec<(String, String)> = files
                .lines()
                .filter(|f| !f.is_empty())
                .map(|f| (f.to_string(), rel_path(&roots, &project, &absolute(folder, f))))
                .collect();
            let new_files: Vec<&str> = renamed.iter().map(|(_, new)| new.as_str()).collect();
            tx.execute(
                "UPDATE events SET project = ?2, files = ?3, summary = ?4 WHERE id = ?1",
                params![id, project, new_files.join("\n"), rebase_summary(summary, &renamed)],
            )?;
        }
        for table in ["injections", "csm_file_touches", "csm_peers", "csm_file_claims"] {
            tx.execute(&format!("UPDATE {table} SET project = ?2 WHERE project = ?1"), params![folder, project])?;
        }
        let decisions =
            tx.execute("UPDATE csm_decisions SET project = ?2 WHERE project = ?1", params![folder, project])?;
        // Moved decisions keep their older ids, below what sessions in the
        // project have seen: those sessions hear the merged board again.
        tx.execute("DELETE FROM csm_decisions_seen WHERE project = ?1", [folder])?;
        if decisions > 0 {
            tx.execute("DELETE FROM csm_decisions_seen WHERE project = ?1", [&project])?;
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

/// Rewrites the file names in an edit summary such as "Edit a, b (+2 more)",
/// matching whole names only, so `README.md` never rewrites `docs/README.md`.
fn rebase_summary(summary: &str, renamed: &[(String, String)]) -> String {
    if renamed.iter().all(|(old, new)| old == new) {
        return summary.to_string();
    }
    let Some((verb, rest)) = summary.split_once(' ') else {
        return summary.to_string();
    };
    let (list, suffix) = match rest.rfind(" (+") {
        Some(i) if rest.ends_with(" more)") => rest.split_at(i),
        _ => (rest, ""),
    };
    let names: Vec<&str> = list
        .split(", ")
        .map(|name| renamed.iter().find(|(old, _)| old == name).map_or(name, |(_, new)| new.as_str()))
        .collect();
    format!("{verb} {}{suffix}", names.join(", "))
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
        let tools = format!("{LAUNCH}/tools");
        assert_eq!(store.canonical_project(&tools).unwrap(), tools, "a link covers exactly its folder");
        assert_eq!(store.canonical_project("C:/work/Arcade/arcade2").unwrap(), "C:/work/Arcade/arcade2");
        assert_eq!(store.canonical_project(GAME).unwrap(), GAME);
    }

    #[test]
    fn links_refuse_cycles_and_chains_resolve() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.link_project("C:/a", "C:/a").is_err());
        store.link_project("C:/b", "C:/c").unwrap();
        // Linking into an already-linked folder lands in its project.
        store.link_project("C:/a", "C:/b").unwrap();
        assert_eq!(store.canonical_project("C:/a").unwrap(), "C:/c");
        // Moving the target re-points links that pointed at it.
        store.link_project("C:/c", "C:/d").unwrap();
        assert_eq!(store.canonical_project("C:/b").unwrap(), "C:/d");
        assert!(store.link_project("C:/d", "C:/a").is_err(), "C:/a already leads back to C:/d");
        assert_eq!(store.links().unwrap().len(), 3);
        assert!(store.unlink_project("C:/a").unwrap());
        assert_eq!(store.canonical_project("C:/a").unwrap(), "C:/a");
    }

    #[test]
    fn a_parent_folder_links_into_a_project_inside_it_without_sweeping_siblings() {
        let store = Store::open_in_memory().unwrap();
        let parent = "C:/work/Arcade";
        edit(&store, parent, "p1", "arcade-godot/scripts/war.gd");
        edit(&store, LAUNCH, "c1", "notes.md");
        edit(&store, GAME, "x1", "scripts/ai_brain.gd");
        assert_eq!(store.link_project(parent, GAME).unwrap(), LinkCounts { memories: 0, events: 1 });
        assert_eq!(store.canonical_project(parent).unwrap(), GAME);
        assert_eq!(store.canonical_project(LAUNCH).unwrap(), LAUNCH, "the sibling under the parent stays its own");
        assert_eq!(store.canonical_project(GAME).unwrap(), GAME);
        let moved = store.events(&EventQuery { project: Some(GAME), limit: 10, ..Default::default() }).unwrap();
        assert!(moved.iter().any(|e| e.summary == "Edit scripts/war.gd"), "{moved:?}");
        assert_eq!(
            store.events(&EventQuery { project: Some(LAUNCH), limit: 10, ..Default::default() }).unwrap().len(),
            1
        );
    }

    #[test]
    fn nested_projects_are_left_alone_by_a_link_above_them() {
        let store = Store::open_in_memory().unwrap();
        store.link_project("C:/y", "C:/x/sub").unwrap();
        store.link_project("C:/x", "C:/p").unwrap();
        assert_eq!(store.canonical_project("C:/y").unwrap(), "C:/x/sub");
        assert_eq!(store.canonical_project("C:/x/sub").unwrap(), "C:/x/sub");
    }

    #[test]
    fn summaries_are_rebased_by_whole_file_name() {
        let renamed = vec![
            ("README.md".to_string(), "C:/work/launch/README.md".to_string()),
            ("docs/README.md".to_string(), "C:/work/launch/docs/README.md".to_string()),
        ];
        assert_eq!(
            rebase_summary("Edit README.md, docs/README.md (+3 more)", &renamed),
            "Edit C:/work/launch/README.md, C:/work/launch/docs/README.md (+3 more)"
        );
        assert_eq!(rebase_summary("Run: cargo test", &renamed), "Run: cargo test");
        assert_eq!(rebase_summary("Edit a.rs", &[("a.rs".into(), "a.rs".into())]), "Edit a.rs");
    }
}
