//! The decision board: standing decisions the user made for a project, such
//! as "saves don't matter during these test runs". The user tells one
//! session; the board makes sure every session in the project knows. The
//! briefing at session start lists it, and a session that is already running
//! is told on its next prompt or tool call after the board changes.
//!
//! Entries are append-only. Withdrawing a decision, or replacing it with a
//! new one, is an entry too, so what changed since a session last looked is
//! every entry with a higher id.

use rusqlite::{params, OptionalExtension, Row};
use serde::Serialize;

use crate::redact::redact;
use crate::store::Store;
use crate::text::{one_line, truncate};
use crate::time::{ago, iso};
use crate::{Error, Result};

/// Small enough to show every decision in full, in briefings and notices.
pub const MAX_DECISION_BYTES: usize = 300;
/// Decisions named in a briefing or a notice; `csm decisions` lists them all.
pub const MAX_SHOWN: usize = 8;
/// A session's "seen up to" marker is dropped this long after it last moved.
const KEEP_SEEN_MS: i64 = 30 * 86_400_000;
const COLUMNS: &str = "id, project, text, withdraws, recorded_by, ts";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decision {
    pub id: i64,
    pub project: String,
    /// Empty for an entry that only withdraws an earlier decision.
    pub text: String,
    /// The earlier decision this entry replaces or withdraws.
    pub withdraws: Option<i64>,
    /// `cli`, `mcp`, or the messaging peer that recorded it (`codex:<session>`).
    pub recorded_by: String,
    pub recorded_at: String,
    #[serde(skip)]
    pub ts: i64,
}

impl Decision {
    /// "#3 Saves don't matter (2h ago, recorded via codex ef8f068b)".
    pub fn line(&self, now: i64) -> String {
        format!("#{} {} ({}, recorded via {})", self.id, self.text, ago(now, self.ts), recorder(&self.recorded_by))
    }

    fn quoted(&self, now: i64) -> String {
        format!("#{} \"{}\" ({}, recorded via {})", self.id, self.text, ago(now, self.ts), recorder(&self.recorded_by))
    }
}

/// What [`Store::decide`] did.
#[derive(Debug)]
pub struct Decided {
    /// The new entry, or the standing decision it duplicates.
    pub entry: Decision,
    pub duplicate: bool,
    /// The decision this one replaced or withdrew.
    pub withdrawn: Option<Decision>,
    pub redacted: usize,
}

impl Decided {
    /// One-line result shared by the CLI and the MCP tool.
    pub fn summary(&self) -> String {
        let mut text = match (&self.withdrawn, self.entry.text.is_empty(), self.duplicate) {
            (_, _, true) => format!("Already on the board as #{}; nothing added.", self.entry.id),
            (Some(old), true, _) => format!("Withdrew standing decision #{}.", old.id),
            (Some(old), false, _) => format!("Recorded standing decision #{}, replacing #{}.", self.entry.id, old.id),
            (None, _, _) => format!("Recorded standing decision #{}.", self.entry.id),
        };
        if !self.duplicate {
            text.push_str(" Every session in this project sees the board at its start, and running ones on their next prompt or tool call.");
        }
        if self.redacted > 0 {
            text.push_str(&format!(" Redacted {} secret(s) before saving.", self.redacted));
        }
        text
    }
}

impl Store {
    /// Creates the board tables, an extension next to the core schema like
    /// the mailbox, so older csm binaries keep working on the same database.
    pub(crate) fn init_decisions(&self) -> Result<()> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'csm_decisions_seen')",
            [],
            |r| r.get(0),
        )?;
        if exists {
            return Ok(());
        }
        let tx = self.immediate()?;
        tx.execute_batch(
            "
CREATE TABLE IF NOT EXISTS csm_decisions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project     TEXT    NOT NULL,
    text        TEXT    NOT NULL,
    withdraws   INTEGER,
    recorded_by TEXT    NOT NULL,
    ts          INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS csm_decisions_project ON csm_decisions (project, id);
CREATE INDEX IF NOT EXISTS csm_decisions_withdraws ON csm_decisions (withdraws);
CREATE TABLE IF NOT EXISTS csm_decisions_seen (
    host       TEXT    NOT NULL,
    session_id TEXT    NOT NULL,
    project    TEXT    NOT NULL,
    seen       INTEGER NOT NULL,
    ts         INTEGER NOT NULL,
    PRIMARY KEY (host, session_id, project)
);
",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Puts a decision on the project's board, withdraws an earlier one, or
    /// both at once, which replaces the earlier decision with the new one.
    pub fn decide(
        &self,
        project: &str,
        text: Option<&str>,
        withdraw: Option<i64>,
        recorded_by: &str,
        now: i64,
    ) -> Result<Decided> {
        let (text, redacted) = match text {
            Some(text) => {
                let (text, redacted) = clean_decision(text)?;
                (Some(text), redacted)
            }
            None => (None, 0),
        };
        if text.is_none() && withdraw.is_none() {
            return Err(Error::Invalid("give a decision, the id of one to withdraw, or both".into()));
        }
        let tx = self.immediate()?;
        let standing = self.decisions(project)?;
        let withdrawn =
            match withdraw {
                Some(id) => Some(standing.iter().find(|d| d.id == id).cloned().ok_or_else(|| {
                    Error::Invalid(format!("#{id} is not a standing decision on this project's board"))
                })?),
                None => None,
            };
        if let (Some(text), None) = (&text, withdraw) {
            if let Some(existing) = standing.iter().find(|d| d.text.to_lowercase() == text.to_lowercase()) {
                return Ok(Decided { entry: existing.clone(), duplicate: true, withdrawn: None, redacted });
            }
        }
        tx.execute(
            "INSERT INTO csm_decisions (project, text, withdraws, recorded_by, ts) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![project, text.unwrap_or_default(), withdraw, recorded_by, now],
        )?;
        let id = tx.last_insert_rowid();
        let entry = self.decision(id)?.expect("decision inserted in this transaction");
        tx.commit()?;
        Ok(Decided { entry, duplicate: false, withdrawn, redacted })
    }

    /// The board: decisions still standing, newest first.
    pub fn decisions(&self, project: &str) -> Result<Vec<Decision>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM csm_decisions d
             WHERE project = ?1 AND text <> ''
               AND NOT EXISTS (SELECT 1 FROM csm_decisions w WHERE w.withdraws = d.id)
             ORDER BY id DESC"
        ))?;
        let rows = stmt.query_map([project], row_to_decision)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every entry, withdrawals included, oldest first, for export.
    pub fn decision_history(&self, project: Option<&str>) -> Result<Vec<Decision>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLUMNS} FROM csm_decisions WHERE ?1 IS NULL OR project = ?1 ORDER BY id"))?;
        let rows = stmt.query_map([project], row_to_decision)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn decision(&self, id: i64) -> Result<Option<Decision>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {COLUMNS} FROM csm_decisions WHERE id = ?1"), [id], row_to_decision)
            .optional()?)
    }

    /// The newest entry on the project's board, 0 for none.
    pub fn latest_decision(&self, project: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM csm_decisions WHERE project = ?1",
            [project],
            |r| r.get(0),
        )?)
    }

    /// Records that a session has seen the project's board up to entry
    /// `through`. Returns what it had seen before (0 for nothing), or `None`
    /// when it was already up to date. Atomic, so hooks of one session that
    /// fire at the same time never both report the same change.
    pub fn advance_decisions_seen(
        &self,
        host: &str,
        session_id: &str,
        project: &str,
        through: i64,
        now: i64,
    ) -> Result<Option<i64>> {
        let seen = || -> Result<i64> {
            Ok(self
                .conn
                .query_row(
                    "SELECT seen FROM csm_decisions_seen WHERE host = ?1 AND session_id = ?2 AND project = ?3",
                    params![host, session_id, project],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0))
        };
        // Hooks call this on every prompt and tool call: only take the write
        // lock when there is something new, then check again under it.
        if seen()? >= through {
            return Ok(None);
        }
        let tx = self.immediate()?;
        let seen = seen()?;
        if seen >= through {
            return Ok(None);
        }
        tx.execute(
            "INSERT INTO csm_decisions_seen (host, session_id, project, seen, ts) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (host, session_id, project) DO UPDATE SET seen = excluded.seen, ts = excluded.ts",
            params![host, session_id, project, through, now],
        )?;
        tx.commit()?;
        Ok(Some(seen))
    }

    pub fn prune_decisions_seen(&self, now: i64) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM csm_decisions_seen WHERE ts < ?1", [now - KEEP_SEEN_MS])?)
    }
}

/// What changed on the board since this session last looked, as one notice
/// for the host to show the agent, or `None` when nothing did. A session
/// that has never seen the board gets all of it. Changes the session made
/// itself are left out.
pub fn board_notice(store: &Store, host: &str, session_id: &str, project: &str, now: i64) -> Result<Option<String>> {
    let latest = store.latest_decision(project)?;
    let Some(seen) = store.advance_decisions_seen(host, session_id, project, latest, now)? else {
        return Ok(None);
    };
    // Bounded by `latest` too: an entry added since then is the next notice's news.
    let new = |id: i64| id > seen && id <= latest;
    let me = format!("{host}:{session_id}");
    let standing = store.decisions(project)?;
    let added: Vec<&Decision> = standing.iter().rev().filter(|d| new(d.id) && d.recorded_by != me).collect();
    let mut withdrawn = Vec::new();
    for entry in store.decision_history(Some(project))? {
        // Only decisions this session could have seen are news when withdrawn.
        if let Some(old) = entry.withdraws.filter(|old| new(entry.id) && *old <= seen && entry.recorded_by != me) {
            withdrawn.extend(store.decision(old)?);
        }
    }
    if added.is_empty() && withdrawn.is_empty() {
        return Ok(None);
    }
    let mut parts = Vec::new();
    if !added.is_empty() {
        let mut named: Vec<String> = added.iter().take(MAX_SHOWN).map(|d| d.quoted(now)).collect();
        if added.len() > MAX_SHOWN {
            named.push(format!("{} more", added.len() - MAX_SHOWN));
        }
        let label = if seen == 0 { "Standing decisions the user made for this project:" } else { "New:" };
        parts.push(format!("{label} {}.", named.join("; ")));
    }
    if !withdrawn.is_empty() {
        let named: Vec<String> =
            withdrawn.iter().take(MAX_SHOWN).map(|d| format!("#{} \"{}\"", d.id, truncate(&d.text, 120))).collect();
        parts.push(format!("Withdrawn: {}.", named.join("; ")));
    }
    let lead = if seen == 0 { "" } else { "The user's standing decisions for this project changed. " };
    Ok(Some(format!("csm: {lead}{} csm_decisions lists the board.", parts.join(" "))))
}

/// "codex ef8f068b" for a session, otherwise as recorded ("cli", "mcp").
pub(crate) fn recorder(recorded_by: &str) -> String {
    match recorded_by.split_once(':') {
        Some((host, session)) => format!("{host} {}", short_session(session)),
        None => recorded_by.to_string(),
    }
}

/// The first eight characters of a session id, as the journal prints them.
pub(crate) fn short_session(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

/// One line, redacted, within the size limit. Returns the redaction count.
fn clean_decision(text: &str) -> Result<(String, usize)> {
    let text = one_line(text);
    if text.is_empty() || text.chars().any(char::is_control) {
        return Err(Error::Invalid("a decision must be nonempty text without control characters".into()));
    }
    if text.len() > MAX_DECISION_BYTES {
        return Err(Error::Invalid(format!(
            "a decision must fit in {MAX_DECISION_BYTES} bytes; state it in a sentence or two"
        )));
    }
    let redacted = redact(&text);
    Ok((redacted.text, redacted.count))
}

fn row_to_decision(r: &Row<'_>) -> rusqlite::Result<Decision> {
    let ts: i64 = r.get(5)?;
    Ok(Decision {
        id: r.get(0)?,
        project: r.get(1)?,
        text: r.get(2)?,
        withdraws: r.get(3)?,
        recorded_by: r.get(4)?,
        recorded_at: iso(ts),
        ts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "C:/work/game";
    const NOW: i64 = 1_791_979_445_000;
    const MIN: i64 = 60_000;

    fn decide(store: &Store, text: &str, by: &str, at: i64) -> i64 {
        store.decide(P, Some(text), None, by, at).unwrap().entry.id
    }

    #[test]
    fn records_withdraws_and_replaces_decisions() {
        let store = Store::open_in_memory().unwrap();
        let saves = decide(&store, "Saves don't matter: the real-game runs are deliberate.", "claude:aaaa1111", NOW);
        let ac = decide(&store, "AC isn't the default", "cli", NOW + 1);
        let again = store.decide(P, Some("  ac ISN'T the\ndefault "), None, "codex:x", NOW + 2).unwrap();
        assert!(again.duplicate && again.entry.id == ac, "{again:?}");

        let replaced = store.decide(P, Some("AC is the default on hard mode only"), Some(ac), "cli", NOW + 3).unwrap();
        assert_eq!(replaced.withdrawn.as_ref().map(|d| d.id), Some(ac));
        assert!(replaced
            .summary()
            .starts_with(&format!("Recorded standing decision #{}, replacing #{ac}.", replaced.entry.id)));
        let board: Vec<String> = store.decisions(P).unwrap().into_iter().map(|d| d.text).collect();
        assert_eq!(
            board,
            ["AC is the default on hard mode only", "Saves don't matter: the real-game runs are deliberate."]
        );

        let gone = store.decide(P, None, Some(saves), "cli", NOW + 4).unwrap();
        assert_eq!(gone.summary().split(". ").next().unwrap(), format!("Withdrew standing decision #{saves}"));
        assert_eq!(store.decisions(P).unwrap().len(), 1);
        assert_eq!(store.decision_history(Some(P)).unwrap().len(), 4);
        assert!(store.decide(P, None, Some(saves), "cli", NOW + 5).is_err(), "already withdrawn");
        assert!(store.decide("C:/other", None, Some(replaced.entry.id), "cli", NOW).is_err(), "other project");
        assert!(store.decide(P, None, None, "cli", NOW).is_err());
        assert!(store.decide(P, Some("   "), None, "cli", NOW).is_err());
        assert!(store.decide(P, Some(&"x".repeat(MAX_DECISION_BYTES + 1)), None, "cli", NOW).is_err());
    }

    #[test]
    fn redacts_secrets_in_decisions() {
        let store = Store::open_in_memory().unwrap();
        let d =
            store.decide(P, Some("Use the staging key sk-proj-AbCdEfGhIjKlMnOpQrStUv12"), None, "cli", NOW).unwrap();
        assert_eq!(d.redacted, 1);
        assert!(!d.entry.text.contains("AbCdEf"), "{}", d.entry.text);
    }

    #[test]
    fn a_running_session_hears_each_change_once() {
        let store = Store::open_in_memory().unwrap();
        assert!(board_notice(&store, "codex", "s1", P, NOW).unwrap().is_none(), "empty board");
        let saves = decide(&store, "Saves don't matter", "claude:aaaa1111-2222", NOW - 2 * MIN);

        let first = board_notice(&store, "codex", "s1", P, NOW).unwrap().expect("the whole board");
        assert_eq!(
            first,
            format!("csm: Standing decisions the user made for this project: #{saves} \"Saves don't matter\" (2m ago, recorded via claude aaaa1111). csm_decisions lists the board.")
        );
        assert!(board_notice(&store, "codex", "s1", P, NOW + MIN).unwrap().is_none(), "said once");

        let ac = decide(&store, "AC isn't the default", "cli", NOW + 2 * MIN);
        store.decide(P, None, Some(saves), "cli", NOW + 3 * MIN).unwrap();
        let change = board_notice(&store, "codex", "s1", P, NOW + 4 * MIN).unwrap().expect("a change");
        assert_eq!(
            change,
            format!("csm: The user's standing decisions for this project changed. New: #{ac} \"AC isn't the default\" (2m ago, recorded via cli). Withdrawn: #{saves} \"Saves don't matter\". csm_decisions lists the board.")
        );

        // Another session that never looked gets the board as it stands now.
        let fresh = board_notice(&store, "claude", "s2", P, NOW + 4 * MIN).unwrap().unwrap();
        assert!(fresh.contains("AC isn't the default") && !fresh.contains("Saves"), "{fresh}");
    }

    #[test]
    fn a_session_is_not_told_about_its_own_changes() {
        let store = Store::open_in_memory().unwrap();
        store.decide(P, Some("Saves don't matter"), None, "codex:s1", NOW).unwrap();
        assert!(board_notice(&store, "codex", "s1", P, NOW).unwrap().is_none());
        assert!(board_notice(&store, "claude", "s2", P, NOW).unwrap().is_some());
        // Seen markers are per project.
        store.decide("C:/other", Some("Ship Fridays"), None, "cli", NOW).unwrap();
        assert!(board_notice(&store, "claude", "s2", P, NOW).unwrap().is_none());
    }

    #[test]
    fn a_decision_withdrawn_before_the_session_saw_it_is_not_news() {
        let store = Store::open_in_memory().unwrap();
        decide(&store, "Keep", "cli", NOW);
        board_notice(&store, "codex", "s1", P, NOW).unwrap();
        let brief = decide(&store, "Short-lived", "cli", NOW + 1);
        store.decide(P, None, Some(brief), "cli", NOW + 2).unwrap();
        assert!(board_notice(&store, "codex", "s1", P, NOW + 3).unwrap().is_none());
    }

    #[test]
    fn prunes_old_seen_markers() {
        let store = Store::open_in_memory().unwrap();
        decide(&store, "Keep", "cli", NOW);
        store.advance_decisions_seen("codex", "old", P, 1, NOW - 2 * KEEP_SEEN_MS).unwrap();
        store.advance_decisions_seen("codex", "new", P, 1, NOW).unwrap();
        assert_eq!(store.prune_decisions_seen(NOW).unwrap(), 1);
    }

    #[test]
    fn a_decision_recorded_while_a_notice_is_built_is_announced_once() {
        let store = Store::open_in_memory().unwrap();
        decide(&store, "Keep", "cli", NOW);
        // Stands in for a `csm decide` in another process landing mid-notice.
        store
            .conn
            .execute_batch(&format!(
                "CREATE TRIGGER sneak AFTER INSERT ON csm_decisions_seen BEGIN
                 INSERT INTO csm_decisions (project, text, recorded_by, ts) VALUES ('{P}', 'Sneaked in', 'cli', {NOW});
                 END;"
            ))
            .unwrap();
        let first = board_notice(&store, "codex", "s1", P, NOW).unwrap().unwrap();
        store.conn.execute_batch("DROP TRIGGER sneak").unwrap();
        let second = board_notice(&store, "codex", "s1", P, NOW).unwrap().unwrap();
        assert!(first.contains("Keep") && !first.contains("Sneaked"), "{first}");
        assert!(second.contains("Sneaked") && !second.contains("Keep"), "{second}");
    }

    #[test]
    fn linking_a_folder_retells_the_merged_board() {
        let store = Store::open_in_memory().unwrap();
        let folder = "C:/work/launcher";
        store.decide(folder, Some("Decided in the launcher"), None, "cli", NOW).unwrap();
        decide(&store, "Decided in the game", "cli", NOW + 1);
        board_notice(&store, "codex", "s1", P, NOW + 2).unwrap();
        store.link_project(folder, P).unwrap();
        let told = board_notice(&store, "codex", "s1", P, NOW + 3).unwrap().expect("the merged board");
        assert!(told.contains("Decided in the launcher"), "{told}");
    }
}
