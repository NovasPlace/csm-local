//! Collision warnings: before an agent edits a file, tell it when another
//! session that is still running edited the same file recently, or, for a
//! subagent, when another subagent of the same session did. Nothing is
//! locked or blocked; the agent decides what to do with the information.
//!
//! Hooks record every successful edit's absolute path in `file_touches`.
//! The `PreToolUse` hook on edit tools looks the target files up there.

use rusqlite::params;

use crate::capture::{normalize, rel_path, Host};
use crate::journal::EventQuery;
use crate::store::{content_hash, Store};
use crate::text::{one_line, truncate};
use crate::time::ago;
use crate::{claims, Result};

/// Another session's edit counts as "recent" for this long, and a session
/// that has been quiet for this long no longer counts as running.
pub const LEASE_WINDOW_MS: i64 = 30 * 60_000;
/// Touches older than this are pruned; only the lease window is ever read.
const KEEP_TOUCHES_MS: i64 = 24 * 3_600_000;
const MAX_FILES_NAMED: usize = 3;

/// Lookup key for an absolute path: forward slashes, no trailing slash, and
/// case-folded on Windows, where `C:/Repo` and `c:/repo` are the same file.
pub fn touch_key(path: &str) -> String {
    let path = normalize(path);
    let path = path.trim_end_matches('/');
    if cfg!(windows) {
        path.to_lowercase()
    } else {
        path.to_string()
    }
}

/// The newest edit of one file by one other session, or by one sibling
/// subagent of the asking one.
#[derive(Debug, Clone, PartialEq)]
pub struct Touch {
    pub display: String,
    pub project: String,
    pub host: String,
    pub session_id: String,
    /// The sibling subagent that made the edit; empty for another session.
    pub agent_id: String,
    pub ts: i64,
}

impl Store {
    /// Creates the collision and link tables. Like the mailbox, these are an
    /// extension next to the core schema rather than a core migration, so the
    /// schema version stays readable by older csm binaries during upgrades.
    pub(crate) fn init_coordination(&self) -> Result<()> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'csm_project_links')",
            [],
            |r| r.get(0),
        )?;
        if exists {
            return Ok(());
        }
        let tx = self.immediate()?;
        tx.execute_batch(
            "
CREATE TABLE IF NOT EXISTS csm_file_touches (
    path       TEXT    NOT NULL,
    display    TEXT    NOT NULL,
    project    TEXT    NOT NULL,
    host       TEXT    NOT NULL,
    session_id TEXT    NOT NULL,
    agent_id   TEXT    NOT NULL DEFAULT '',
    ts         INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS csm_file_touches_path ON csm_file_touches (path, ts);
CREATE INDEX IF NOT EXISTS csm_file_touches_ts ON csm_file_touches (ts);
CREATE TABLE IF NOT EXISTS csm_project_links (
    alias   TEXT    PRIMARY KEY,
    project TEXT    NOT NULL,
    ts      INTEGER NOT NULL
);
",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records that a session just edited these absolute paths.
    pub fn touch_files(
        &self,
        project: &str,
        host: &str,
        session_id: &str,
        agent_id: &str,
        paths: &[String],
        ts: i64,
    ) -> Result<()> {
        for path in paths {
            self.conn.execute(
                "INSERT INTO csm_file_touches (path, display, project, host, session_id, agent_id, ts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![touch_key(path), normalize(path), project, host, session_id, agent_id, ts],
            )?;
        }
        Ok(())
    }

    /// For each path, the newest edit since `since` by every session other
    /// than (`host`, `session_id`), and, when `agent_id` is a subagent, by each
    /// of its siblings. Edits between a session and its own subagents never
    /// count: the session handed that work out. Newest first.
    pub fn recent_touches(
        &self,
        paths: &[String],
        host: &str,
        session_id: &str,
        agent_id: &str,
        since: i64,
    ) -> Result<Vec<Touch>> {
        let mut stmt = self.conn.prepare(
            "SELECT display, project, host, session_id,
                    CASE WHEN host = ?3 AND session_id = ?4 THEN agent_id ELSE '' END AS sibling, MAX(ts)
             FROM csm_file_touches
             WHERE path = ?1 AND ts >= ?2
               AND NOT (host = ?3 AND session_id = ?4 AND (?5 = '' OR agent_id IN ('', ?5)))
             GROUP BY host, session_id, sibling",
        )?;
        let mut touches = Vec::new();
        for path in paths {
            let rows = stmt.query_map(params![touch_key(path), since, host, session_id, agent_id], |r| {
                Ok(Touch {
                    display: r.get(0)?,
                    project: r.get(1)?,
                    host: r.get(2)?,
                    session_id: r.get(3)?,
                    agent_id: r.get(4)?,
                    ts: r.get(5)?,
                })
            })?;
            touches.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        touches.sort_by_key(|t| std::cmp::Reverse(t.ts));
        Ok(touches)
    }

    pub fn prune_touches(&self, now: i64) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM csm_file_touches WHERE ts < ?1", [now - KEEP_TOUCHES_MS])?)
    }
}

/// The warning for a session about to edit `paths`, if another session has
/// claimed any of them, or another running session, or a sibling subagent,
/// edited any of them within the lease window. Each claim, and each (file,
/// other session or sibling) edit, is reported once, so repeated edits stay
/// quiet.
pub fn collision_notice(
    store: &Store,
    host: Host,
    session_id: &str,
    agent_id: &str,
    project: &str,
    paths: &[String],
    now: i64,
) -> Result<Option<String>> {
    let mut claimed = Vec::new();
    let mut sources = Vec::new();
    for claim in store.claims_on(paths, &format!("{}:{session_id}", host.name()), now)? {
        if claimed.len() == MAX_FILES_NAMED {
            break;
        }
        let file = rel_path(&[project.to_string()], project, &claim.path);
        let note = if claim.note.is_empty() { String::new() } else { format!(": \"{}\"", truncate(&claim.note, 120)) };
        let line =
            format!("{file} is claimed by {} {}{note}.", claims::holder(store, &claim.peer)?, claim.remaining(now));
        let trigger = format!("lease:claim:{}", claim.id);
        if store.claim_injection(project, host.name(), session_id, agent_id, &trigger, None, &line, &[], now)? {
            sources.push(claim.peer.clone());
            claimed.push(line);
        }
    }
    let mut lines = Vec::new();
    for touch in store.recent_touches(paths, host.name(), session_id, agent_id, now - LEASE_WINDOW_MS)? {
        if lines.len() == MAX_FILES_NAMED {
            break;
        }
        // A sibling works inside the asking session, which is running.
        let sibling = !touch.agent_id.is_empty();
        if !sibling && !store.session_live(&touch.project, &touch.host, &touch.session_id, now, LEASE_WINDOW_MS)? {
            continue;
        }
        let file = rel_path(&[project.to_string()], project, &touch.display);
        let by = if sibling {
            "another subagent working for the same session".to_string()
        } else {
            format!("another session that is still running ({})", who(store, &touch)?)
        };
        let line = format!("{file} was edited {} by {by}.", ago(now, touch.ts));
        // Keyed by the other session's edit itself: the same edit is reported
        // once, and a newer edit to the same file is reported again.
        let editor =
            if sibling { format!("{}/{}", touch.session_id, touch.agent_id) } else { touch.session_id.clone() };
        let trigger =
            format!("lease:{}:{}:{editor}:{}", content_hash(&touch_key(&touch.display)), touch.host, touch.ts);
        let claimed =
            store.claim_injection(project, host.name(), session_id, agent_id, &trigger, None, &line, &[], now)?;
        if claimed {
            // A sibling has no address of its own; its session is the asker's.
            if !sibling {
                sources.push(format!("{}:{}", touch.host, touch.session_id));
            }
            lines.push(line);
        }
    }
    if claimed.is_empty() && lines.is_empty() {
        return Ok(None);
    }
    let mut parts = claimed;
    if !lines.is_empty() {
        parts.push(format!("{} Those changes may not be in the copy of the file you read.", lines.join(" ")));
    }
    let mut text = format!("csm: {}", parts.join(" "));
    // Messaging peers can be told directly; say so only when they are reachable.
    let mut reachable: Vec<String> = Vec::new();
    for id in sources {
        if !reachable.contains(&id) && store.peer(&id).ok().flatten().is_some() {
            reachable.push(id);
        }
    }
    if !reachable.is_empty() {
        text.push_str(&format!(" csm_send reaches that session at {}.", reachable.join(", ")));
    }
    Ok(Some(text))
}

/// "via codex in arcade-godot, labelled ..., last request: ...".
fn who(store: &Store, touch: &Touch) -> Result<String> {
    let folder = touch.project.rsplit('/').next().unwrap_or(&touch.project);
    let mut parts = vec![format!("via {} in {folder}", touch.host)];
    if let Some(peer) = store.peer(&format!("{}:{}", touch.host, touch.session_id))? {
        let generic = format!("{} session", touch.host);
        if !peer.label.trim().is_empty() && peer.label != generic {
            parts.push(format!("working on \"{}\"", truncate(&one_line(&peer.label), 80)));
        }
    }
    let prompt = store.events(&EventQuery {
        project: Some(&touch.project),
        session_id: Some(&touch.session_id),
        kinds: &["prompt"],
        limit: 20,
        ..Default::default()
    })?;
    if let Some(p) = prompt.iter().find(|p| crate::brief::is_typed_prompt(&p.summary)) {
        parts.push(format!("last request: \"{}\"", truncate(&one_line(&p.summary), 120)));
    }
    Ok(parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::NewEvent;

    const P: &str = "C:/work/app";
    const NOW: i64 = 1_791_979_445_000;
    const MIN: i64 = 60_000;

    fn active(store: &Store, host: &str, session: &str, prompt: &str, at: i64) {
        store
            .record_at(
                NewEvent {
                    project: P.into(),
                    host: host.into(),
                    session_id: Some(session.into()),
                    kind: "prompt".into(),
                    summary: prompt.into(),
                    ..Default::default()
                },
                at,
            )
            .unwrap();
    }

    fn file() -> Vec<String> {
        vec!["C:/work/app/src/main.rs".to_string()]
    }

    #[test]
    fn warns_about_a_file_another_running_session_just_edited() {
        let store = Store::open_in_memory().unwrap();
        active(&store, "codex", "other", "refactor the router", NOW - 5 * MIN);
        store.touch_files(P, "codex", "other", "", &file(), NOW - 4 * MIN).unwrap();

        let text = collision_notice(&store, Host::Claude, "me", "", P, &["c:\\work\\app\\src\\main.rs".into()], NOW)
            .unwrap()
            .expect("a warning");
        assert_eq!(
            text,
            "csm: src/main.rs was edited 4m ago by another session that is still running (via codex in app, last request: \"refactor the router\"). Those changes may not be in the copy of the file you read."
        );
        // Once per file and session per window.
        assert!(collision_notice(&store, Host::Claude, "me", "", P, &file(), NOW + MIN).unwrap().is_none());
    }

    #[test]
    fn a_newer_edit_by_the_same_session_is_reported_again() {
        let store = Store::open_in_memory().unwrap();
        active(&store, "codex", "other", "x", NOW - 30 * MIN);
        store.touch_files(P, "codex", "other", "", &file(), NOW - 25 * MIN).unwrap();
        assert!(collision_notice(&store, Host::Claude, "me", "", P, &file(), NOW - 24 * MIN).unwrap().is_some());
        active(&store, "codex", "other", "y", NOW - 6 * MIN);
        store.touch_files(P, "codex", "other", "", &file(), NOW - 5 * MIN).unwrap();
        let again = collision_notice(&store, Host::Claude, "me", "", P, &file(), NOW).unwrap();
        assert!(again.is_some_and(|t| t.contains("5m ago")), "the newer edit is news");
        assert!(store.last_injection().unwrap().is_none(), "warnings are not briefings");
    }

    #[test]
    fn stays_quiet_for_own_edits_ended_sessions_and_old_edits() {
        let store = Store::open_in_memory().unwrap();
        active(&store, "claude", "me", "x", NOW - MIN);
        store.touch_files(P, "claude", "me", "", &file(), NOW - MIN).unwrap();
        assert!(collision_notice(&store, Host::Claude, "me", "", P, &file(), NOW).unwrap().is_none(), "own edit");

        active(&store, "codex", "done", "y", NOW - 10 * MIN);
        store.touch_files(P, "codex", "done", "", &file(), NOW - 9 * MIN).unwrap();
        store
            .record_at(
                NewEvent {
                    project: P.into(),
                    host: "codex".into(),
                    session_id: Some("done".into()),
                    kind: "session_end".into(),
                    summary: "exit".into(),
                    ..Default::default()
                },
                NOW - 8 * MIN,
            )
            .unwrap();
        assert!(collision_notice(&store, Host::Claude, "me", "", P, &file(), NOW).unwrap().is_none(), "ended");

        active(&store, "codex", "old", "z", NOW - 50 * MIN);
        store.touch_files(P, "codex", "old", "", &file(), NOW - 45 * MIN).unwrap();
        assert!(collision_notice(&store, Host::Claude, "me", "", P, &file(), NOW).unwrap().is_none(), "too old");
    }

    #[test]
    fn a_subagent_is_warned_about_its_siblings_edits_only() {
        let store = Store::open_in_memory().unwrap();
        active(&store, "claude", "s1", "split the work", NOW - 5 * MIN);
        store.register_peer(P, "claude", "s1", None, "active", NOW - 5 * MIN).unwrap();
        store.touch_files(P, "claude", "s1", "a1", &file(), NOW - 2 * MIN).unwrap();

        let text = collision_notice(&store, Host::Claude, "s1", "a2", P, &file(), NOW).unwrap().expect("a warning");
        assert_eq!(
            text,
            "csm: src/main.rs was edited 2m ago by another subagent working for the same session. Those changes may not be in the copy of the file you read."
        );
        assert!(collision_notice(&store, Host::Claude, "s1", "a2", P, &file(), NOW).unwrap().is_none(), "said once");
        assert!(collision_notice(&store, Host::Claude, "s1", "a1", P, &file(), NOW).unwrap().is_none(), "own edit");
        assert!(collision_notice(&store, Host::Claude, "s1", "", P, &file(), NOW).unwrap().is_none(), "handed out");
        store.touch_files(P, "claude", "s1", "a1", &file(), NOW + MIN).unwrap();
        assert!(collision_notice(&store, Host::Claude, "s1", "a2", P, &file(), NOW + 2 * MIN).unwrap().is_some());

        // The parent's own edits are not a sibling's.
        let other = vec!["C:/work/app/src/lib.rs".to_string()];
        store.touch_files(P, "claude", "s1", "", &other, NOW).unwrap();
        assert!(collision_notice(&store, Host::Claude, "s1", "a3", P, &other, NOW + MIN).unwrap().is_none());
    }

    #[test]
    fn another_sessions_subagents_count_as_that_session_once() {
        let store = Store::open_in_memory().unwrap();
        active(&store, "codex", "other", "fan out", NOW - 5 * MIN);
        store.touch_files(P, "codex", "other", "x1", &file(), NOW - 3 * MIN).unwrap();
        store.touch_files(P, "codex", "other", "x2", &file(), NOW - 2 * MIN).unwrap();
        for agent in ["", "a1"] {
            let text = collision_notice(&store, Host::Claude, "me", agent, P, &file(), NOW).unwrap().unwrap();
            assert_eq!(text.matches("was edited").count(), 1, "{text}");
            assert!(text.contains("2m ago by another session that is still running (via codex in app"), "{text}");
        }
    }

    #[test]
    fn names_reachable_peers_and_their_label() {
        let store = Store::open_in_memory().unwrap();
        active(&store, "codex", "peer1", "ship the hangar", NOW - 2 * MIN);
        store.register_peer(P, "codex", "peer1", Some("Hangar bays"), "active", NOW - MIN).unwrap();
        store.touch_files(P, "codex", "peer1", "", &file(), NOW - MIN).unwrap();
        let text = collision_notice(&store, Host::Claude, "me", "", P, &file(), NOW).unwrap().unwrap();
        assert!(text.contains("working on \"Hangar bays\""), "{text}");
        assert!(text.ends_with("csm_send reaches that session at codex:peer1."), "{text}");
    }

    #[test]
    fn files_outside_the_project_keep_their_full_path() {
        let store = Store::open_in_memory().unwrap();
        active(&store, "codex", "other", "x", NOW - MIN);
        let outside = vec!["D:/shared/lib.rs".to_string()];
        store.touch_files(P, "codex", "other", "", &outside, NOW - MIN).unwrap();
        let text = collision_notice(&store, Host::Claude, "me", "", P, &outside, NOW).unwrap().unwrap();
        assert!(text.starts_with("csm: D:/shared/lib.rs was edited"), "{text}");
    }

    #[test]
    fn prunes_old_touches() {
        let store = Store::open_in_memory().unwrap();
        store.touch_files(P, "codex", "s", "", &file(), NOW - 2 * KEEP_TOUCHES_MS).unwrap();
        store.touch_files(P, "codex", "s", "", &file(), NOW).unwrap();
        assert_eq!(store.prune_touches(NOW).unwrap(), 1);
    }
}
