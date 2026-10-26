//! File claims: a session states which files it is about to change, for a
//! limited time ("codex ef8f068b has ground_field.gd until 09:20"). Other
//! sessions see the claim when they list their peers, in their briefing, and
//! as a warning just before they edit a claimed file. Nothing is locked; a
//! claim is information, and its holder can be messaged.
//!
//! Claims belong to a messaging identity (`csm_connect`), so there is always
//! someone to ask. A claim ends when it is released, when it expires, or when
//! the session holding it ends.
//!
//! Subagents share their session's MCP server, so the server cannot tell who
//! in a session made a claim; new ones start as nobody's in particular. The
//! `PostToolUse` hook, which knows which subagent made the call, then says:
//! sibling subagents are warned about each other's files, and a subagent's
//! claims pause while it is stopped. Re-claiming never takes a file away from
//! the session or a sibling; the session itself can always take one back.

use rusqlite::{params, Row};
use serde_json::{json, Value};

use crate::capture::rel_path;
use crate::decisions::recorder;
use crate::leases::touch_key;
use crate::messaging::{Peer, PEER_STALE_MS};
use crate::redact::redact;
use crate::store::Store;
use crate::text::{one_line, truncate};
use crate::time::iso;
use crate::{Error, Result};

pub const DEFAULT_MINUTES: i64 = 30;
pub const MAX_MINUTES: i64 = 240;
pub const MAX_FILES: usize = 20;
const MAX_NOTE_BYTES: usize = 200;
/// Claims one session can hold at once.
const MAX_HELD: i64 = 100;
const MINUTE_MS: i64 = 60_000;
const COLUMNS: &str = "id, display, project, peer, note, claimed_at, expires_at, agent_id";
/// A claim is in force until it expires or its session ends, and not while
/// the subagent holding it is stopped.
const IN_FORCE: &str =
    "expires_at > ?1 AND paused_at IS NULL AND peer NOT IN (SELECT id FROM csm_peers WHERE state = 'ended')";
/// What counts toward [`MAX_HELD`]: paused claims too, which hold again
/// when their subagent does.
const HELD: &str = "expires_at > ?1 AND peer NOT IN (SELECT id FROM csm_peers WHERE state = 'ended')";
/// The holder of a claim made through the MCP server until the hook says
/// which subagent made it. It counts as the session's own.
pub const UNATTRIBUTED: &str = "?";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub id: i64,
    /// Absolute, forward slashes.
    pub path: String,
    /// The holder's project.
    pub project: String,
    /// The holder, `host:session`.
    pub peer: String,
    pub note: String,
    pub claimed_at: i64,
    pub expires_at: i64,
    /// The holder's subagent that claimed it; empty for the session itself,
    /// [`UNATTRIBUTED`] before the hook has said.
    pub agent_id: String,
}

impl Claim {
    /// The subagent holding the claim, if it is not the session's own.
    pub fn subagent(&self) -> Option<&str> {
        Some(self.agent_id.as_str()).filter(|a| !a.is_empty() && *a != UNATTRIBUTED)
    }

    /// The path relative to `project`, or in full outside it.
    pub fn file_in(&self, project: &str) -> String {
        rel_path(&[project.to_string()], project, &self.path)
    }

    pub fn minutes_left(&self, now: i64) -> i64 {
        (self.expires_at - now + MINUTE_MS - 1).div_euclid(MINUTE_MS).max(0)
    }

    /// "for 18 more min (until 13:20 UTC)".
    pub fn remaining(&self, now: i64) -> String {
        format!("for {} more min (until {} UTC)", self.minutes_left(now), &iso(self.expires_at)[11..16])
    }

    pub fn json(&self, now: i64) -> Value {
        json!({
            "file": self.file_in(&self.project), "path": self.path, "note": self.note,
            "until": iso(self.expires_at), "minutes_left": self.minutes_left(now),
        })
    }
}

/// What [`Store::claim_files`] did.
#[derive(Debug, Default)]
pub struct Claimed {
    /// Claims the caller now holds, new or extended.
    pub claimed: Vec<Claim>,
    /// Files another session already holds; these were not claimed.
    pub held: Vec<Claim>,
}

impl Claimed {
    pub fn json(&self, peer: &str, now: i64) -> Value {
        let held: Vec<Value> = self
            .held
            .iter()
            .map(|c| {
                let mut v = c.json(now);
                v["peer"] = Value::String(c.peer.clone());
                v
            })
            .collect();
        json!({
            "peer": peer,
            "claimed": self.claimed.iter().map(|c| c.json(now)).collect::<Vec<_>>(),
            "held_by_others": held,
            "notice": "Advisory: nothing is locked. Other sessions see these claims in csm_peers and are warned before editing a claimed file. Claims end on csm_release, at expiry, or when this session ends; a subagent's pause while it is stopped.",
        })
    }
}

impl Store {
    /// Creates the claims table, an extension next to the core schema.
    pub(crate) fn init_claims(&self) -> Result<()> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'csm_file_claims')",
            [],
            |r| r.get(0),
        )?;
        if exists {
            return self.add_claim_agents();
        }
        let tx = self.immediate()?;
        tx.execute_batch(
            "
CREATE TABLE IF NOT EXISTS csm_file_claims (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    path       TEXT    NOT NULL,
    display    TEXT    NOT NULL,
    project    TEXT    NOT NULL,
    peer       TEXT    NOT NULL,
    note       TEXT    NOT NULL DEFAULT '',
    claimed_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    agent_id   TEXT    NOT NULL DEFAULT '',
    paused_at  INTEGER
);
CREATE INDEX IF NOT EXISTS csm_file_claims_path ON csm_file_claims (path);
CREATE INDEX IF NOT EXISTS csm_file_claims_peer ON csm_file_claims (peer);
",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Columns that came after the table. Older binaries never name them, so
    /// their claims are the session's own and never paused.
    fn add_claim_agents(&self) -> Result<()> {
        const ADDED: [(&str, &str); 2] = [("agent_id", "TEXT NOT NULL DEFAULT ''"), ("paused_at", "INTEGER")];
        let missing = |conn: &rusqlite::Connection| -> Result<Vec<&str>> {
            let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('csm_file_claims')")?;
            let have: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            Ok(ADDED.iter().map(|(name, _)| *name).filter(|name| !have.iter().any(|h| h == name)).collect())
        };
        if missing(&self.conn)?.is_empty() {
            return Ok(());
        }
        let tx = self.immediate()?;
        for name in missing(&tx)? {
            let kind = ADDED.iter().find(|(n, _)| *n == name).map(|(_, kind)| *kind).expect("listed");
            tx.execute_batch(&format!("ALTER TABLE csm_file_claims ADD COLUMN {name} {kind}"))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Claims absolute `paths` for `peer` itself for `minutes`. A file another
    /// session holds is left alone and reported; one the peer already holds
    /// is extended, keeping its note unless a new one is given, and becomes
    /// the session's own if one of its subagents held it.
    pub fn claim_files(
        &self,
        peer: &Peer,
        paths: &[String],
        minutes: i64,
        note: Option<&str>,
        now: i64,
    ) -> Result<Claimed> {
        self.claim(peer, paths, minutes, note, Some(""), now)
    }

    /// As [`Store::claim_files`], for the MCP server, which a session shares
    /// with its subagents and so cannot say who is claiming: a new claim is
    /// [`UNATTRIBUTED`] until the hook says. The caller may not be the holder
    /// of a claim it extends, so that claim keeps its holder and note, and is
    /// only ever lengthened.
    pub fn claim_files_unattributed(
        &self,
        peer: &Peer,
        paths: &[String],
        minutes: i64,
        note: Option<&str>,
        now: i64,
    ) -> Result<Claimed> {
        self.claim(peer, paths, minutes, note, None, now)
    }

    fn claim(
        &self,
        peer: &Peer,
        paths: &[String],
        minutes: i64,
        note: Option<&str>,
        holder: Option<&str>,
        now: i64,
    ) -> Result<Claimed> {
        if paths.is_empty() || paths.len() > MAX_FILES {
            return Err(Error::Invalid(format!("claim 1 to {MAX_FILES} files at a time")));
        }
        if !(1..=MAX_MINUTES).contains(&minutes) {
            return Err(Error::Invalid(format!("minutes must be from 1 to {MAX_MINUTES}")));
        }
        let note = one_line(note.unwrap_or(""));
        if note.len() > MAX_NOTE_BYTES || note.chars().any(char::is_control) {
            return Err(Error::Invalid(format!(
                "note must be at most {MAX_NOTE_BYTES} bytes without control characters"
            )));
        }
        let note = redact(&note).text;
        let expires = now + minutes * MINUTE_MS;
        let tx = self.immediate()?;
        // An ended session's claims are never in force; say so instead of claiming nothing.
        if self.peer(&peer.id)?.is_none_or(|p| p.state == "ended") {
            return Err(Error::Invalid(format!(
                "session {} has ended, so it cannot hold claims; a new session needs its own csm_connect",
                peer.id
            )));
        }
        tx.execute("DELETE FROM csm_file_claims WHERE expires_at <= ?1", [now])?;
        let mut out = Claimed::default();
        let mut keys: Vec<String> = Vec::new();
        for path in paths {
            let key = touch_key(path);
            if keys.contains(&key) {
                continue;
            }
            keys.push(key.clone());
            // A stopped subagent's claim gives way to whoever claims the file next.
            tx.execute("DELETE FROM csm_file_claims WHERE path = ?1 AND paused_at IS NOT NULL", [&key])?;
            let holders = self.claims_where("path = ?2", &key, now)?;
            if let Some(other) = holders.iter().find(|c| c.peer != peer.id) {
                out.held.push(other.clone());
                continue;
            }
            let id = match holders.first() {
                Some(mine) => {
                    tx.execute(
                        "UPDATE csm_file_claims
                         SET expires_at = CASE WHEN ?4 IS NULL THEN MAX(expires_at, ?2) ELSE ?2 END,
                             note = CASE WHEN ?3 = '' OR (?4 IS NULL AND note <> '') THEN note ELSE ?3 END,
                             agent_id = COALESCE(?4, agent_id)
                         WHERE id = ?1",
                        params![mine.id, expires, note, holder],
                    )?;
                    mine.id
                }
                None => {
                    tx.execute(
                        "INSERT INTO csm_file_claims (path, display, project, peer, note, claimed_at, expires_at, agent_id)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![key, path, peer.project, peer.id, note, now, expires, holder.unwrap_or(UNATTRIBUTED)],
                    )?;
                    tx.last_insert_rowid()
                }
            };
            out.claimed.extend(self.claims_where("id = ?2", id, now)?);
        }
        let held: i64 = tx.query_row(
            &format!("SELECT COUNT(*) FROM csm_file_claims WHERE peer = ?2 AND {HELD}"),
            params![now, peer.id],
            |r| r.get(0),
        )?;
        if held > MAX_HELD {
            return Err(Error::Invalid(format!("a session can hold at most {MAX_HELD} file claims")));
        }
        tx.commit()?;
        Ok(out)
    }

    /// Releases `peer`'s claims on absolute `paths`, or all of its claims.
    /// Returns how many ended.
    pub fn release_claims(&self, peer: &str, paths: Option<&[String]>) -> Result<usize> {
        match paths {
            None => Ok(self.conn.execute("DELETE FROM csm_file_claims WHERE peer = ?1", [peer])?),
            Some(paths) => {
                let tx = self.immediate()?;
                let mut released = 0;
                for path in paths {
                    released += tx.execute(
                        "DELETE FROM csm_file_claims WHERE peer = ?1 AND path = ?2",
                        params![peer, touch_key(path)],
                    )?;
                }
                tx.commit()?;
                Ok(released)
            }
        }
    }

    /// Every claim in force, newest first.
    pub fn active_claims(&self, now: i64) -> Result<Vec<Claim>> {
        self.claims_where("1 = ?2", 1, now)
    }

    /// Says whose `peer`'s claims on absolute `paths` are, after a
    /// `csm_claim` call by its subagent `agent_id` (empty for the session
    /// itself). The session takes any of them back, paused or not; a subagent
    /// takes only claims that are nobody's yet, so a file never moves away
    /// from the session or a sibling. Returns, for a subagent, the claims a
    /// sibling kept. (A claim the session kept is not told apart from one an
    /// older MCP server just made, which also reads as the session's.)
    pub fn attribute_claims(&self, peer: &str, agent_id: &str, paths: &[String], now: i64) -> Result<Vec<Claim>> {
        let tx = self.immediate()?;
        let mut kept = Vec::new();
        for path in paths {
            let key = touch_key(path);
            tx.execute(
                "UPDATE csm_file_claims SET agent_id = ?3, paused_at = CASE WHEN ?3 = '' THEN NULL ELSE paused_at END
                 WHERE peer = ?1 AND path = ?2 AND (?3 = '' OR agent_id = ?4)",
                params![peer, key, agent_id, UNATTRIBUTED],
            )?;
            if !agent_id.is_empty() {
                let held = self.claims_where("path = ?2", &key, now)?;
                kept.extend(held.into_iter().filter(|c| c.peer == peer && c.subagent().is_some_and(|a| a != agent_id)));
            }
        }
        tx.commit()?;
        Ok(kept)
    }

    /// Pauses the claims `peer`'s subagent `agent_id` holds, when it stops.
    /// A paused claim is not in force and gives way to the next claim on its
    /// file; it holds again once the subagent does anything more (another
    /// hook kept it going, or it was resumed).
    pub fn pause_agent_claims(&self, peer: &str, agent_id: &str, now: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE csm_file_claims SET paused_at = ?3 WHERE peer = ?1 AND agent_id = ?2 AND paused_at IS NULL",
            params![peer, agent_id, now],
        )?)
    }

    /// Unpauses `peer`'s subagent `agent_id`'s claims, now that it is working again.
    pub fn resume_agent_claims(&self, peer: &str, agent_id: &str) -> Result<usize> {
        // Every tool call of every subagent asks; only write when there is something to bring back.
        let paused: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM csm_file_claims WHERE peer = ?1 AND agent_id = ?2 AND paused_at IS NOT NULL)",
            params![peer, agent_id],
            |r| r.get(0),
        )?;
        if !paused {
            return Ok(0);
        }
        Ok(self.conn.execute(
            "UPDATE csm_file_claims SET paused_at = NULL WHERE peer = ?1 AND agent_id = ?2 AND paused_at IS NOT NULL",
            params![peer, agent_id],
        )?)
    }

    /// Claims in force on any of absolute `paths` by sessions other than
    /// `peer`, and, when `agent_id` is one of its subagents, by its sibling
    /// subagents. A session's own claims and its subagents' never count
    /// against each other: the session handed that work out.
    pub fn claims_on(&self, paths: &[String], peer: &str, agent_id: &str, now: i64) -> Result<Vec<Claim>> {
        let counts =
            |c: &Claim| c.peer != peer || (!agent_id.is_empty() && c.subagent().is_some_and(|a| a != agent_id));
        let mut claims = Vec::new();
        for path in paths {
            claims.extend(self.claims_where("path = ?2", touch_key(path), now)?.into_iter().filter(counts));
        }
        Ok(claims)
    }

    pub fn prune_claims(&self, now: i64) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM csm_file_claims WHERE expires_at <= ?1", [now])?)
    }

    fn claims_where(&self, condition: &str, value: impl rusqlite::ToSql, now: i64) -> Result<Vec<Claim>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM csm_file_claims WHERE {condition} AND {IN_FORCE} ORDER BY id DESC"
        ))?;
        let rows = stmt.query_map(params![now, value], row_to_claim)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// The `csm peers` / `csm_peers` listing: each session with the files it
/// has claimed. A session holding claims is listed even after it has gone
/// quiet, so a claim never hides behind the stale filter.
pub fn peers_json(store: &Store, project: Option<&str>, now: i64, include_stale: bool) -> Result<Value> {
    let claims = store.active_claims(now)?;
    let mut peers = store.peers(project, now, include_stale)?;
    for claim in &claims {
        if peers.iter().any(|p| p.id == claim.peer) {
            continue;
        }
        if let Some(mut holder) = store.peer(&claim.peer)?.filter(|p| project.is_none_or(|key| p.project == key)) {
            if holder.last_seen < now - PEER_STALE_MS && holder.state != "ended" {
                holder.state = "stale".into();
            }
            peers.push(holder);
        }
    }
    let mut listed = Vec::new();
    for peer in peers {
        let held: Vec<Value> = claims.iter().filter(|c| c.peer == peer.id).map(|c| c.json(now)).collect();
        let mut value = serde_json::to_value(&peer).map_err(|e| Error::Invalid(e.to_string()))?;
        value["claims"] = Value::Array(held);
        listed.push(value);
    }
    Ok(json!({ "peers": listed }))
}

/// "codex session ef8f068b (working on \"Hangar bays\")".
pub(crate) fn holder(store: &Store, peer_id: &str) -> Result<String> {
    let mut name = recorder(peer_id).replacen(' ', " session ", 1);
    if let Some(peer) = store.peer(peer_id)? {
        let generic = format!("{} session", peer.host);
        if !peer.label.trim().is_empty() && peer.label != generic {
            name.push_str(&format!(" (working on \"{}\")", truncate(&one_line(&peer.label), 80)));
        }
    }
    Ok(name)
}

fn row_to_claim(r: &Row<'_>) -> rusqlite::Result<Claim> {
    Ok(Claim {
        id: r.get(0)?,
        path: r.get(1)?,
        project: r.get(2)?,
        peer: r.get(3)?,
        note: r.get(4)?,
        claimed_at: r.get(5)?,
        expires_at: r.get(6)?,
        agent_id: r.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "C:/work/game";
    const NOW: i64 = 1_791_979_445_000;
    const MIN: i64 = 60_000;

    fn peer(store: &Store, id: &str, at: i64) -> Peer {
        let (host, session) = id.split_once(':').unwrap();
        store.register_peer(P, host, session, None, "active", at).unwrap()
    }

    fn files(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| format!("{P}/{n}")).collect()
    }

    #[test]
    fn one_session_holds_a_file_and_others_are_told_who() {
        let store = Store::open_in_memory().unwrap();
        let codex = peer(&store, "codex:ef8f068b-1111", NOW);
        let claude = peer(&store, "claude:c1", NOW);
        let first = store
            .claim_files(
                &codex,
                &files(&["scripts/ground_field.gd", "scripts/ground_field.gd"]),
                20,
                Some("collision rework"),
                NOW,
            )
            .unwrap();
        assert_eq!(first.claimed.len(), 1, "duplicates collapse");
        assert_eq!(first.claimed[0].remaining(NOW), "for 20 more min (until 12:24 UTC)");

        let second = store
            .claim_files(&claude, &files(&["Scripts\\Ground_Field.gd", "scripts/hud.gd"]), 30, None, NOW + MIN)
            .unwrap();
        if cfg!(windows) {
            assert_eq!(second.held.len(), 1, "same file, other spelling");
            assert_eq!(second.held[0].peer, "codex:ef8f068b-1111");
            assert_eq!(second.claimed.len(), 1);
        }
        assert_eq!(store.claims_on(&files(&["scripts/ground_field.gd"]), "claude:c1", "", NOW + MIN).unwrap().len(), 1);
        assert!(store
            .claims_on(&files(&["scripts/ground_field.gd"]), "codex:ef8f068b-1111", "", NOW)
            .unwrap()
            .is_empty());
        assert_eq!(holder(&store, "codex:ef8f068b-1111").unwrap(), "codex session ef8f068b");
    }

    #[test]
    fn reclaiming_extends_and_release_ends_claims() {
        let store = Store::open_in_memory().unwrap();
        let codex = peer(&store, "codex:a", NOW);
        store.claim_files(&codex, &files(&["a.gd", "b.gd"]), 10, Some("first"), NOW).unwrap();
        let again = store.claim_files(&codex, &files(&["a.gd"]), 60, None, NOW + MIN).unwrap();
        assert_eq!((again.claimed[0].minutes_left(NOW + MIN), again.claimed[0].note.as_str()), (60, "first"));
        assert_eq!(store.release_claims("codex:a", Some(&files(&["a.gd"]))).unwrap(), 1);
        assert_eq!(store.active_claims(NOW).unwrap().len(), 1);
        assert_eq!(store.release_claims("codex:a", None).unwrap(), 1);
        assert!(store.active_claims(NOW).unwrap().is_empty());
    }

    #[test]
    fn a_subagents_claims_count_against_its_siblings_only() {
        let store = Store::open_in_memory().unwrap();
        let session = peer(&store, "claude:s1", NOW);
        let parser = files(&["src/parser.rs"]);
        let made = store.claim_files_unattributed(&session, &parser, 30, Some("parser rework"), NOW).unwrap();
        assert_eq!(made.claimed[0].subagent(), None, "the session's until the hook says");
        assert!(store.attribute_claims("claude:s1", "a1", &parser, NOW).unwrap().is_empty());

        let on = |agent: &str| store.claims_on(&parser, "claude:s1", agent, NOW).unwrap().len();
        assert_eq!((on("a2"), on("a1"), on("")), (1, 0, 0), "a sibling's, its own, the parent's");
        assert_eq!(store.claims_on(&parser, "codex:x", "", NOW).unwrap()[0].agent_id, "a1");

        // A sibling's re-claim extends a1's claim without taking it, and says so.
        store.claim_files_unattributed(&session, &parser, 30, None, NOW + MIN).unwrap();
        let kept = store.attribute_claims("claude:s1", "a2", &parser, NOW + MIN).unwrap();
        assert_eq!(kept.iter().map(|c| c.agent_id.as_str()).collect::<Vec<_>>(), ["a1"]);
        assert_eq!((on("a2"), on("a1")), (1, 0));

        // The session itself takes it back, through the hook or the CLI.
        store.attribute_claims("claude:s1", "", &parser, NOW).unwrap();
        assert_eq!(on("a2"), 0);
        store.claim_files_unattributed(&session, &files(&["src/lexer.rs"]), 30, None, NOW).unwrap();
        store.attribute_claims("claude:s1", "a1", &files(&["src/lexer.rs"]), NOW).unwrap();
        store.claim_files(&session, &files(&["src/lexer.rs"]), 30, None, NOW).unwrap();
        assert_eq!(store.claims_on(&files(&["src/lexer.rs"]), "claude:s1", "a2", NOW).unwrap().len(), 0);
    }

    #[test]
    fn a_subagent_cannot_take_the_sessions_claim_and_lose_it_at_its_stop() {
        let store = Store::open_in_memory().unwrap();
        let session = peer(&store, "claude:s1", NOW);
        let x = files(&["src/x.rs"]);
        store.claim_files_unattributed(&session, &x, 60, Some("parent rework"), NOW).unwrap();
        store.attribute_claims("claude:s1", "", &x, NOW).unwrap();
        // a1 claims the same file through the shared MCP server.
        store.claim_files_unattributed(&session, &x, 30, None, NOW + MIN).unwrap();
        store.attribute_claims("claude:s1", "a1", &x, NOW + MIN).unwrap();
        assert_eq!(store.pause_agent_claims("claude:s1", "a1", NOW + 2 * MIN).unwrap(), 0);
        let held = store.claims_on(&x, "codex:x", "", NOW + 2 * MIN).unwrap();
        assert_eq!((held.len(), held[0].note.as_str(), held[0].subagent()), (1, "parent rework", None));
    }

    #[test]
    fn a_stopped_subagents_claims_pause_and_come_back_or_give_way() {
        let store = Store::open_in_memory().unwrap();
        let session = peer(&store, "claude:s1", NOW);
        let codex = peer(&store, "codex:x", NOW);
        let (x, y) = (files(&["src/x.rs"]), files(&["src/y.rs"]));
        store.claim_files_unattributed(&session, &[x[0].clone(), y[0].clone()], 30, None, NOW).unwrap();
        store.attribute_claims("claude:s1", "a1", &[x[0].clone(), y[0].clone()], NOW).unwrap();

        assert_eq!(store.pause_agent_claims("claude:s1", "a1", NOW + MIN).unwrap(), 2);
        assert!(store.active_claims(NOW + MIN).unwrap().is_empty(), "not in force while stopped");
        assert!(store.claims_on(&x, "claude:s1", "a2", NOW + MIN).unwrap().is_empty());
        // Another session claims y meanwhile; a1's paused claim on it gives way.
        assert!(store.claim_files(&codex, &y, 30, None, NOW + MIN).unwrap().held.is_empty());

        assert_eq!(store.resume_agent_claims("claude:s1", "a1").unwrap(), 1, "x comes back; y went to codex");
        assert_eq!(store.resume_agent_claims("claude:s1", "a1").unwrap(), 0);
        assert_eq!(store.claims_on(&x, "claude:s1", "a2", NOW + 2 * MIN).unwrap().len(), 1);
        assert_eq!(store.claims_on(&y, "claude:s1", "a1", NOW + 2 * MIN).unwrap()[0].peer, "codex:x");
    }

    #[test]
    fn a_reclaim_through_mcp_never_shortens_or_renotes_someone_elses_claim() {
        let store = Store::open_in_memory().unwrap();
        let session = peer(&store, "claude:s1", NOW);
        let x = files(&["src/x.rs"]);
        store.claim_files_unattributed(&session, &x, 120, Some("a1 rework"), NOW).unwrap();
        store.attribute_claims("claude:s1", "a1", &x, NOW).unwrap();
        store.claim_files_unattributed(&session, &x, 1, Some("a2 tweak"), NOW + MIN).unwrap();
        let kept = store.attribute_claims("claude:s1", "a2", &x, NOW + MIN).unwrap();
        assert_eq!((kept[0].note.as_str(), kept[0].minutes_left(NOW + MIN)), ("a1 rework", 119));
        // The session's own claim through the CLI sets exactly what it asks for.
        store.claim_files(&session, &x, 5, Some("parent"), NOW + MIN).unwrap();
        let mine = &store.active_claims(NOW + MIN).unwrap()[0];
        assert_eq!((mine.note.as_str(), mine.minutes_left(NOW + MIN), mine.subagent()), ("parent", 5, None));
    }

    #[test]
    fn paused_claims_count_toward_the_limit() {
        let store = Store::open_in_memory().unwrap();
        let session = peer(&store, "claude:s1", NOW);
        for chunk in 0..5 {
            let batch: Vec<String> = (0..MAX_FILES).map(|i| format!("{P}/f{chunk}-{i}")).collect();
            store.claim_files_unattributed(&session, &batch, 30, None, NOW).unwrap();
            store.attribute_claims("claude:s1", "a1", &batch, NOW).unwrap();
        }
        store.pause_agent_claims("claude:s1", "a1", NOW).unwrap();
        assert!(store.claim_files(&session, &files(&["one-more"]), 30, None, NOW).is_err(), "a1's 100 come back");
    }

    #[test]
    fn the_session_taking_a_claim_back_unpauses_it() {
        let store = Store::open_in_memory().unwrap();
        let session = peer(&store, "claude:s1", NOW);
        let x = files(&["src/x.rs"]);
        store.claim_files_unattributed(&session, &x, 30, None, NOW).unwrap();
        store.attribute_claims("claude:s1", "a1", &x, NOW).unwrap();
        store.pause_agent_claims("claude:s1", "a1", NOW).unwrap();
        // An MCP server from before pausing extends the paused row rather than replacing it.
        store.conn.execute("UPDATE csm_file_claims SET expires_at = ?1", [NOW + 120 * MIN]).unwrap();
        store.attribute_claims("claude:s1", "", &x, NOW).unwrap();
        assert_eq!(store.claims_on(&x, "codex:x", "", NOW).unwrap()[0].subagent(), None);
    }

    #[test]
    fn a_claims_table_from_before_subagents_gains_the_columns() {
        let store = Store::open_in_memory().unwrap();
        let codex = peer(&store, "codex:a", NOW);
        store.conn.execute_batch("ALTER TABLE csm_file_claims DROP COLUMN agent_id").unwrap();
        store.conn.execute_batch("ALTER TABLE csm_file_claims DROP COLUMN paused_at").unwrap();
        store
            .conn
            .execute(
                "INSERT INTO csm_file_claims (path, display, project, peer, note, claimed_at, expires_at)
                 VALUES ('x', 'C:/work/game/a.gd', ?1, 'codex:a', '', ?2, ?3)",
                params![P, NOW, NOW + 30 * MIN],
            )
            .unwrap();
        store.init_claims().unwrap();
        store.init_claims().unwrap();
        assert_eq!(store.active_claims(NOW).unwrap()[0].agent_id, "");
        assert_eq!(store.claim_files(&codex, &files(&["b.gd"]), 5, None, NOW).unwrap().claimed.len(), 1);

        // A table from the first build with agent ids, before pausing.
        store.conn.execute_batch("ALTER TABLE csm_file_claims DROP COLUMN paused_at").unwrap();
        store.init_claims().unwrap();
        assert_eq!(store.active_claims(NOW).unwrap().len(), 2);
        assert_eq!(store.pause_agent_claims("codex:a", "", NOW).unwrap(), 2);
    }

    #[test]
    fn claims_lapse_at_expiry_and_when_the_session_ends() {
        let store = Store::open_in_memory().unwrap();
        let codex = peer(&store, "codex:a", NOW);
        let claude = peer(&store, "claude:b", NOW);
        store.claim_files(&codex, &files(&["a.gd"]), 5, None, NOW).unwrap();
        assert!(store.active_claims(NOW + 5 * MIN).unwrap().is_empty(), "expired");
        assert!(store.claim_files(&claude, &files(&["a.gd"]), 5, None, NOW + 5 * MIN).unwrap().held.is_empty());

        store.claim_files(&codex, &files(&["b.gd"]), 30, None, NOW).unwrap();
        store.set_peer_state("codex:a", "ended", NOW + MIN).unwrap();
        assert!(store.claims_on(&files(&["b.gd"]), "claude:b", "", NOW + MIN).unwrap().is_empty(), "ended");
        let refused = store.claim_files(&codex, &files(&["c.gd"]), 5, None, NOW + MIN).unwrap_err();
        assert!(refused.to_string().contains("has ended"), "{refused}");
        assert_eq!(store.prune_claims(NOW + 10 * MIN).unwrap(), 1, "the lapsed claim on a.gd");
    }

    #[test]
    fn validates_claims() {
        let store = Store::open_in_memory().unwrap();
        let codex = peer(&store, "codex:a", NOW);
        assert!(store.claim_files(&codex, &[], 5, None, NOW).is_err());
        assert!(store.claim_files(&codex, &files(&["a"]), 0, None, NOW).is_err());
        assert!(store.claim_files(&codex, &files(&["a"]), MAX_MINUTES + 1, None, NOW).is_err());
        assert!(store.claim_files(&codex, &files(&["a"]), 5, Some(&"n".repeat(MAX_NOTE_BYTES + 1)), NOW).is_err());
        let many: Vec<String> = (0..=MAX_FILES).map(|i| format!("{P}/f{i}")).collect();
        assert!(store.claim_files(&codex, &many, 5, None, NOW).is_err());
        for chunk in 0..5 {
            let batch: Vec<String> = (0..MAX_FILES).map(|i| format!("{P}/f{chunk}-{i}")).collect();
            store.claim_files(&codex, &batch, 5, None, NOW).unwrap();
        }
        assert!(store.claim_files(&codex, &files(&["one-too-many"]), 5, None, NOW).is_err());
        let noted = store
            .claim_files(&codex, &files(&["f0-0"]), 5, Some("token ghp_R4nd0mT0k3nV4lu3Abcdefghij0123456789"), NOW)
            .unwrap();
        assert!(!noted.claimed[0].note.contains("R4nd0m"), "{}", noted.claimed[0].note);
    }

    #[test]
    fn the_peer_listing_shows_claims_and_keeps_quiet_holders() {
        let store = Store::open_in_memory().unwrap();
        let codex = peer(&store, "codex:a", NOW - 20 * MIN);
        peer(&store, "claude:b", NOW);
        peer(&store, "claude:quiet", NOW - 20 * MIN);
        store.claim_files(&codex, &files(&["scripts/ground_field.gd"]), 60, Some("rework"), NOW - 20 * MIN).unwrap();

        let listing = peers_json(&store, Some(P), NOW, false).unwrap();
        let peers = listing["peers"].as_array().unwrap();
        let ids: Vec<&str> = peers.iter().map(|p| p["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["claude:b", "codex:a"], "a stale holder stays listed; a stale peer without claims does not");
        let holder = &peers[1];
        assert_eq!(holder["state"], "stale");
        assert_eq!(holder["claims"][0]["file"], "scripts/ground_field.gd");
        assert_eq!(holder["claims"][0]["minutes_left"], 40);
        assert_eq!(holder["claims"][0]["note"], "rework");
        assert_eq!(peers[0]["claims"], json!([]));
        assert_eq!(peers_json(&store, Some(P), NOW, true).unwrap()["peers"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn ended_sessions_never_crowd_a_live_one_out_of_the_listing() {
        let store = Store::open_in_memory().unwrap();
        peer(&store, "codex:live", NOW - 5 * MIN);
        for i in 0..100 {
            let id = format!("claude:done-{i}");
            peer(&store, &id, NOW - MIN);
            store.set_peer_state(&id, "ended", NOW - MIN).unwrap();
        }
        let listing = peers_json(&store, Some(P), NOW, false).unwrap();
        let ids: Vec<&str> = listing["peers"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["codex:live"]);
    }

    #[test]
    fn forgetting_a_project_drops_every_claim_on_its_files() {
        let store = Store::open_in_memory().unwrap();
        let outsider = store.register_peer("C:/work/launcher", "claude", "o", None, "active", NOW).unwrap();
        store.claim_files(&outsider, &files(&["scripts/a.gd"]), 30, None, NOW).unwrap();
        store.claim_files(&outsider, &["C:/work/gamebox/x.gd".to_string()], 30, None, NOW).unwrap();
        store.forget_project(P).unwrap();
        let left: Vec<String> = store.active_claims(NOW).unwrap().into_iter().map(|c| c.path).collect();
        assert_eq!(left, ["C:/work/gamebox/x.gd"], "a sibling folder whose name starts the same is another project");
    }
}
