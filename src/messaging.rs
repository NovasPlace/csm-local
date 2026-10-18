//! Durable local mailboxes for explicitly addressed coding-agent sessions.
//!
//! All timestamps are Unix milliseconds. The additive schema has its own
//! version so older CSM binaries can still open the memory/journal database.

use std::collections::BTreeSet;

use rusqlite::{params, OptionalExtension, Row};
use serde::Serialize;

use crate::redact::redact;
use crate::store::Store;
use crate::{Error, Result};

pub const MAX_MESSAGE_BYTES: usize = 8 * 1024;
pub const MAX_MAILBOX_BATCH: usize = 100;
pub const PEER_STALE_MS: i64 = 10 * 60 * 1000;
const MINUTE_MS: i64 = 60 * 1000;
const PEER_COLUMNS: &str = "id, project, host, session_id, label, state, last_seen";
const MESSAGE_COLUMNS: &str =
    "id, from_peer, to_peer, content, created_at, expires_at, delivered_at, acknowledged_at, reply_to";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Peer {
    pub id: String,
    pub project: String,
    pub host: String,
    pub session_id: String,
    pub label: String,
    pub state: String,
    pub last_seen: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Message {
    pub id: i64,
    pub from_peer: String,
    pub to_peer: String,
    pub content: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub delivered_at: Option<i64>,
    pub acknowledged_at: Option<i64>,
    pub reply_to: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NewMessage {
    pub from_peer: String,
    pub to_peer: String,
    pub content: String,
    pub reply_to: Option<i64>,
    pub request_id: String,
    pub allow_cross_project: bool,
    pub now: i64,
    pub ttl_seconds: i64,
}

impl Store {
    pub(crate) fn init_messaging(&self) -> Result<()> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'csm_mailbox_meta')",
            [],
            |r| r.get(0),
        )?;
        if exists {
            let version: Option<i64> = self
                .conn
                .query_row("SELECT version FROM csm_mailbox_meta WHERE singleton = 1", [], |r| r.get(0))
                .optional()?;
            match version {
                Some(1) => return Ok(()),
                Some(v) => return Err(Error::Invalid(format!("unsupported mailbox schema v{v}; upgrade csm"))),
                None => {}
            }
        }
        let tx = self.immediate()?;
        tx.execute_batch(
            "
CREATE TABLE IF NOT EXISTS csm_mailbox_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    version INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS csm_peers (
    id TEXT PRIMARY KEY,
    project TEXT NOT NULL,
    host TEXT NOT NULL,
    session_id TEXT NOT NULL,
    label TEXT NOT NULL,
    state TEXT NOT NULL,
    last_seen INTEGER NOT NULL,
    intro_sent_at INTEGER,
    binding_token TEXT NOT NULL,
    UNIQUE(host, session_id)
);
CREATE INDEX IF NOT EXISTS csm_peers_project_seen ON csm_peers(project, last_seen DESC);
CREATE TABLE IF NOT EXISTS csm_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_peer TEXT NOT NULL REFERENCES csm_peers(id),
    to_peer TEXT NOT NULL REFERENCES csm_peers(id),
    content TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    delivered_at INTEGER,
    acknowledged_at INTEGER,
    reply_to INTEGER,
    request_id TEXT NOT NULL,
    ttl_seconds INTEGER NOT NULL,
    allow_cross_project INTEGER NOT NULL,
    UNIQUE(from_peer, request_id)
);
CREATE INDEX IF NOT EXISTS csm_messages_inbox ON csm_messages(to_peer, acknowledged_at, id);
CREATE INDEX IF NOT EXISTS csm_messages_sender_time ON csm_messages(from_peer, created_at);
INSERT OR IGNORE INTO csm_mailbox_meta(singleton, version) VALUES (1, 1);
",
        )?;
        let version: i64 =
            tx.query_row("SELECT version FROM csm_mailbox_meta WHERE singleton = 1", [], |r| r.get(0))?;
        if version != 1 {
            return Err(Error::Invalid(format!("unsupported mailbox schema v{version}; upgrade csm")));
        }
        tx.commit()?;
        Ok(())
    }

    /// Registers a session or updates its heartbeat. A session's project is immutable.
    /// None preserves the current label and heartbeats never reset the introduction flag.
    pub fn register_peer(
        &self,
        project: &str,
        host: &str,
        session_id: &str,
        label: Option<&str>,
        state: &str,
        now: i64,
    ) -> Result<Peer> {
        validate_text("project", project, 4096)?;
        validate_text("host", host, 64)?;
        if !host.bytes().all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c)) {
            return Err(Error::Invalid("host must contain only letters, digits, underscores, dots or hyphens".into()));
        }
        validate_text("session_id", session_id, 256)?;
        validate_state(state)?;
        validate_time(now)?;
        let label = label
            .map(|s| {
                validate_text("label", s, 128)?;
                let s = redact(s).text;
                validate_text("label", &s, 128)?;
                Ok::<_, Error>(s)
            })
            .transpose()?;
        let id = format!("{host}:{session_id}");
        let tx = self.immediate()?;
        if let Some(existing) = self.peer(&id)? {
            if existing.project != project {
                return Err(Error::Invalid("this session is already registered to another project".into()));
            }
            tx.execute(
                "UPDATE csm_peers SET label = COALESCE(?2, label),
                 state = CASE WHEN ?4 >= last_seen THEN ?3 ELSE state END,
                 last_seen = MAX(last_seen, ?4) WHERE id = ?1",
                params![id, label, state, now],
            )?;
        } else {
            tx.execute(
                "INSERT INTO csm_peers(id, project, host, session_id, label, state, last_seen, binding_token)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, lower(hex(randomblob(24))))",
                params![id, project, host, session_id, label.unwrap_or_else(|| format!("{host} session")), state, now],
            )?;
        }
        let peer = self.peer(&id)?.expect("peer inserted in this transaction");
        tx.commit()?;
        Ok(peer)
    }

    pub fn peer(&self, id: &str) -> Result<Option<Peer>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {PEER_COLUMNS} FROM csm_peers WHERE id = ?1"), [id], row_to_peer)
            .optional()?)
    }

    /// A session's private MCP-binding nonce, used only in its own hook context.
    /// Peer listings deliberately never include this value.
    pub fn peer_binding_token(&self, id: &str) -> Result<String> {
        self.conn
            .query_row("SELECT binding_token FROM csm_peers WHERE id = ?1", [id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| Error::Invalid("unknown peer; register the session first".into()))
    }

    pub fn verify_peer_binding(&self, id: &str, token: &str) -> Result<bool> {
        if token.len() != 48 || !token.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Ok(false);
        }
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM csm_peers WHERE id = ?1 AND binding_token = ?2)",
            params![id, token],
            |r| r.get(0),
        )?)
    }

    /// Up to 100 most recently seen peers. By default excludes ended and stale
    /// sessions; included stale sessions have state `stale` (ended stays ended).
    pub fn peers(&self, project: Option<&str>, now: i64, include_stale: bool) -> Result<Vec<Peer>> {
        validate_time(now)?;
        let cutoff = now.saturating_sub(PEER_STALE_MS);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PEER_COLUMNS} FROM csm_peers WHERE (?1 IS NULL OR project = ?1)
             AND (?2 OR (last_seen >= ?3 AND state != 'ended')) ORDER BY last_seen DESC, id LIMIT 100"
        ))?;
        let rows = stmt.query_map(params![project, include_stale, cutoff], row_to_peer)?;
        let mut peers = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        for peer in &mut peers {
            if peer.last_seen < cutoff && peer.state != "ended" {
                peer.state = "stale".into();
            }
        }
        Ok(peers)
    }

    pub fn set_peer_state(&self, id: &str, state: &str, now: i64) -> Result<()> {
        validate_state(state)?;
        validate_time(now)?;
        let changed = self.conn.execute(
            "UPDATE csm_peers SET state = CASE WHEN ?3 >= last_seen THEN ?2 ELSE state END,
             last_seen = MAX(last_seen, ?3) WHERE id = ?1",
            params![id, state, now],
        )?;
        if changed == 0 {
            return Err(Error::Invalid("unknown peer; register the session first".into()));
        }
        Ok(())
    }

    /// Claims the once-per-session onboarding notice atomically across hook processes.
    pub fn claim_peer_intro(&self, id: &str, now: i64) -> Result<bool> {
        validate_time(now)?;
        let tx = self.immediate()?;
        require_peer(self, id)?;
        let changed = tx.execute(
            "UPDATE csm_peers SET intro_sent_at = ?2 WHERE id = ?1 AND intro_sent_at IS NULL",
            params![id, now],
        )?;
        tx.commit()?;
        Ok(changed != 0)
    }

    /// Saves a redacted message. A repeated sender/request_id returns the original
    /// message only when the stored payload, TTL and cross-project intent match.
    pub fn send_message(&self, new: NewMessage) -> Result<Message> {
        validate_time(new.now)?;
        validate_text("request_id", &new.request_id, 128)?;
        if new.content.trim().is_empty() || new.content.len() > MAX_MESSAGE_BYTES || new.content.contains('\0') {
            return Err(Error::Invalid(
                "message content must be nonempty, contain no NUL and fit in 8192 bytes".into(),
            ));
        }
        let content = redact(&new.content).text;
        if content.len() > MAX_MESSAGE_BYTES {
            return Err(Error::Invalid("redacted message exceeds 8192 bytes".into()));
        }
        if !(60..=604_800).contains(&new.ttl_seconds) {
            return Err(Error::Invalid("ttl_seconds must be between 60 and 604800".into()));
        }
        let expires_at = new
            .now
            .checked_add(new.ttl_seconds * 1000)
            .ok_or_else(|| Error::Invalid("message expiry is out of range".into()))?;
        if new.from_peer == new.to_peer {
            return Err(Error::Invalid("cannot send a message to the same session".into()));
        }
        let tx = self.immediate()?;
        let sender = require_peer(self, &new.from_peer)?;
        let recipient = require_peer(self, &new.to_peer)?;
        if sender.project != recipient.project && !new.allow_cross_project {
            return Err(Error::Invalid("cross-project messaging requires allow_cross_project=true".into()));
        }
        let existing = tx
            .query_row(
                &format!(
                    "SELECT {MESSAGE_COLUMNS}, ttl_seconds, allow_cross_project FROM csm_messages
                      WHERE from_peer = ?1 AND request_id = ?2"
                ),
                params![new.from_peer, new.request_id],
                |r| Ok((row_to_message(r)?, r.get::<_, i64>(9)?, r.get::<_, bool>(10)?)),
            )
            .optional()?;
        if let Some((message, ttl, cross_project)) = existing {
            if message.to_peer != new.to_peer
                || message.content != content
                || message.reply_to != new.reply_to
                || ttl != new.ttl_seconds
                || cross_project != new.allow_cross_project
            {
                return Err(Error::Invalid("request_id was already used with a different message payload".into()));
            }
            tx.commit()?;
            return Ok(message);
        }
        if let Some(reply_to) = new.reply_to {
            let valid: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM csm_messages WHERE id = ?1 AND
                 ((from_peer = ?2 AND to_peer = ?3) OR (from_peer = ?3 AND to_peer = ?2)))",
                params![reply_to, new.from_peer, new.to_peer],
                |r| r.get(0),
            )?;
            if !valid {
                return Err(Error::Invalid("reply_to must belong to a message between these two sessions".into()));
            }
        }
        let sent: i64 = tx.query_row(
            "SELECT COUNT(*) FROM csm_messages WHERE from_peer = ?1 AND created_at > ?2",
            params![new.from_peer, new.now - MINUTE_MS],
            |r| r.get(0),
        )?;
        if sent >= 20 {
            return Err(Error::Invalid("sender limit reached: 20 messages per minute".into()));
        }
        let pending: i64 = tx.query_row(
            "SELECT COUNT(*) FROM csm_messages WHERE to_peer = ?1 AND acknowledged_at IS NULL AND expires_at > ?2",
            params![new.to_peer, new.now],
            |r| r.get(0),
        )?;
        if pending >= 100 {
            return Err(Error::Invalid("recipient inbox is full (100 unacknowledged messages)".into()));
        }
        tx.execute(
            "INSERT INTO csm_messages(from_peer, to_peer, content, created_at, expires_at, reply_to,
                                      request_id, ttl_seconds, allow_cross_project)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                new.from_peer,
                new.to_peer,
                content,
                new.now,
                expires_at,
                new.reply_to,
                new.request_id,
                new.ttl_seconds,
                new.allow_cross_project
            ],
        )?;
        let id = tx.last_insert_rowid();
        let message = self.message_status(&new.from_peer, id)?.expect("message inserted in this transaction");
        tx.commit()?;
        Ok(message)
    }

    /// Reads unacknowledged, unexpired messages. Reading alone never claims delivery.
    pub fn pending_messages(&self, peer: &str, limit: usize, now: i64, only_undelivered: bool) -> Result<Vec<Message>> {
        validate_time(now)?;
        require_peer(self, peer)?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM csm_messages WHERE to_peer = ?1 AND acknowledged_at IS NULL
             AND expires_at > ?2 AND (?3 = 0 OR delivered_at IS NULL) ORDER BY id LIMIT ?4"
        ))?;
        let rows =
            stmt.query_map(params![peer, now, only_undelivered, limit.min(MAX_MAILBOX_BATCH) as i64], row_to_message)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Atomically offers the oldest eligible messages, with a 60-second lease.
    /// `delivered_at` records the latest offer, not proof the host consumed it.
    /// An unacknowledged offer can be retried after the lease if a process lost
    /// its output. Agents can explicitly reread messages throughout the lease.
    pub fn claim_messages(&self, peer: &str, limit: usize, now: i64) -> Result<Vec<Message>> {
        validate_time(now)?;
        let tx = self.immediate()?;
        require_peer(self, peer)?;
        let mut messages = {
            let mut stmt = tx.prepare(&format!(
                "SELECT {MESSAGE_COLUMNS} FROM csm_messages WHERE to_peer = ?1 AND acknowledged_at IS NULL
                 AND expires_at > ?2 AND (delivered_at IS NULL OR delivered_at <= ?3) ORDER BY id LIMIT ?4"
            ))?;
            let rows = stmt
                .query_map(params![peer, now, now - MINUTE_MS, limit.min(MAX_MAILBOX_BATCH) as i64], row_to_message)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for message in &mut messages {
            tx.execute("UPDATE csm_messages SET delivered_at = ?2 WHERE id = ?1", params![message.id, now])?;
            message.delivered_at = Some(now);
        }
        tx.commit()?;
        Ok(messages)
    }

    /// Recipient-only, all-or-nothing acknowledgment. Returns newly acknowledged
    /// rows; duplicate IDs and retries are harmless. Ack also records delivery.
    pub fn acknowledge_messages(&self, peer: &str, ids: &[i64], now: i64) -> Result<usize> {
        validate_time(now)?;
        if ids.len() > MAX_MAILBOX_BATCH {
            return Err(Error::Invalid("acknowledge at most 100 message IDs at once".into()));
        }
        let ids: BTreeSet<_> = ids.iter().copied().collect();
        let tx = self.immediate()?;
        require_peer(self, peer)?;
        for id in &ids {
            let valid: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM csm_messages WHERE id = ?1 AND to_peer = ?2)",
                params![id, peer],
                |r| r.get(0),
            )?;
            if !valid {
                return Err(Error::Invalid("message unavailable for this recipient".into()));
            }
        }
        let mut changed = 0;
        for id in ids {
            changed += tx.execute(
                "UPDATE csm_messages SET acknowledged_at = ?2, delivered_at = COALESCE(delivered_at, ?2)
                 WHERE id = ?1 AND acknowledged_at IS NULL",
                params![id, now],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Counts all retained messages touching this project and its peers.
    /// A local message is counted once; acknowledged and expired history remains
    /// included because only an explicit project-forget operation erases it.
    pub fn mailbox_project_counts(&self, project: &str) -> Result<(usize, usize)> {
        Ok(self.conn.query_row(
            "SELECT
                (SELECT COUNT(*) FROM csm_messages
                 WHERE from_peer IN (SELECT id FROM csm_peers WHERE project = ?1)
                    OR to_peer IN (SELECT id FROM csm_peers WHERE project = ?1)),
                (SELECT COUNT(*) FROM csm_peers WHERE project = ?1)",
            [project],
            |r| Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize)),
        )?)
    }

    pub fn message_status(&self, peer: &str, id: i64) -> Result<Option<Message>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {MESSAGE_COLUMNS} FROM csm_messages WHERE id = ?1 AND (from_peer = ?2 OR to_peer = ?2)"
                ),
                params![id, peer],
                row_to_message,
            )
            .optional()?)
    }
}

fn require_peer(store: &Store, id: &str) -> Result<Peer> {
    store.peer(id)?.ok_or_else(|| Error::Invalid("unknown peer; register the session first".into()))
}

fn validate_text(field: &str, text: &str, max: usize) -> Result<()> {
    if text.trim().is_empty() || text.len() > max || text.chars().any(char::is_control) {
        return Err(Error::Invalid(format!(
            "{field} must be nonempty, at most {max} bytes and contain no control characters"
        )));
    }
    Ok(())
}

fn validate_state(state: &str) -> Result<()> {
    if !["active", "idle", "ended"].contains(&state) {
        return Err(Error::Invalid("peer state must be active, idle or ended".into()));
    }
    Ok(())
}

fn validate_time(now: i64) -> Result<()> {
    if now < 0 {
        return Err(Error::Invalid("timestamp must be nonnegative Unix milliseconds".into()));
    }
    Ok(())
}

fn row_to_peer(r: &Row<'_>) -> rusqlite::Result<Peer> {
    Ok(Peer {
        id: r.get(0)?,
        project: r.get(1)?,
        host: r.get(2)?,
        session_id: r.get(3)?,
        label: r.get(4)?,
        state: r.get(5)?,
        last_seen: r.get(6)?,
    })
}

fn row_to_message(r: &Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        from_peer: r.get(1)?,
        to_peer: r.get(2)?,
        content: r.get(3)?,
        created_at: r.get(4)?,
        expires_at: r.get(5)?,
        delivered_at: r.get(6)?,
        acknowledged_at: r.get(7)?,
        reply_to: r.get(8)?,
    })
}
