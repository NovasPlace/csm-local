use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, ErrorCode, Row, Transaction, TransactionBehavior};
use serde::Serialize;

use crate::redact::redact;
use crate::time::{iso, now_ms};
use crate::{Error, Result};

pub const KINDS: &[&str] = &["note", "lesson", "decision", "fact"];
pub const MAX_CONTENT_CHARS: usize = 20_000;

/// Applied in order; `PRAGMA user_version` records how many have run. Kinds
/// are validated in Rust rather than with CHECK constraints so adding one
/// never needs a table rebuild.
const MIGRATIONS: &[&str] = &[
    "
CREATE TABLE IF NOT EXISTS memories (
    id           INTEGER PRIMARY KEY,
    project      TEXT    NOT NULL,
    kind         TEXT    NOT NULL,
    content      TEXT    NOT NULL,
    tags         TEXT    NOT NULL DEFAULT '',
    importance   INTEGER NOT NULL DEFAULT 3,
    source       TEXT,
    content_hash TEXT    NOT NULL,
    created_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS memories_project_created ON memories (project, created_at DESC);
CREATE INDEX IF NOT EXISTS memories_project_hash ON memories (project, content_hash);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5 (
    content, tags,
    content = 'memories', content_rowid = 'id',
    tokenize = 'porter unicode61'
);
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts (rowid, content, tags) VALUES (new.id, new.content, new.tags);
END;
CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts (memories_fts, rowid, content, tags) VALUES ('delete', old.id, old.content, old.tags);
END;
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts (memories_fts, rowid, content, tags) VALUES ('delete', old.id, old.content, old.tags);
    INSERT INTO memories_fts (rowid, content, tags) VALUES (new.id, new.content, new.tags);
END;
",
    "
CREATE TABLE IF NOT EXISTS events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    project    TEXT    NOT NULL,
    host       TEXT    NOT NULL,
    session_id TEXT,
    agent_id   TEXT    NOT NULL DEFAULT '',
    kind       TEXT    NOT NULL,
    tool       TEXT,
    summary    TEXT    NOT NULL,
    files      TEXT    NOT NULL DEFAULT '',
    ok         INTEGER,
    detail     TEXT,
    dedup_key  TEXT,
    ts         INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS events_dedup
    ON events (host, session_id, agent_id, dedup_key) WHERE dedup_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS events_project ON events (project, id);
CREATE INDEX IF NOT EXISTS events_project_kind ON events (project, kind, id);
CREATE INDEX IF NOT EXISTS events_session ON events (session_id, id);

CREATE TABLE IF NOT EXISTS injections (
    id         INTEGER PRIMARY KEY,
    project    TEXT    NOT NULL,
    host       TEXT    NOT NULL,
    session_id TEXT    NOT NULL,
    agent_id   TEXT    NOT NULL DEFAULT '',
    trigger    TEXT    NOT NULL,
    bytes      INTEGER NOT NULL,
    hash       TEXT    NOT NULL,
    sources    TEXT    NOT NULL DEFAULT '',
    ts         INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS injections_session ON injections (host, session_id, agent_id, trigger, ts);
",
    // Deleting a memory removes its terms from the search index right away
    // instead of leaving them in old index segments on disk.
    "
INSERT INTO memories_fts (memories_fts, rank) VALUES ('secure-delete', 1);
INSERT INTO memories_fts (memories_fts) VALUES ('optimize');
",
];

const COLUMNS: &str = "m.id, m.project, m.kind, m.content, m.tags, m.importance, m.source, m.created_at";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Memory {
    pub id: i64,
    pub project: String,
    pub kind: String,
    pub content: String,
    pub tags: Vec<String>,
    pub importance: i64,
    pub source: Option<String>,
    pub created_at: String,
}

impl Memory {
    /// One-line rendering shared by the CLI and the MCP tools.
    pub fn line(&self, show_project: bool) -> String {
        let mut out = format!("#{} [{}] {} {}", self.id, self.kind, &self.created_at[..10], self.content);
        if !self.tags.is_empty() {
            out.push_str(&format!("  (tags: {})", self.tags.join(", ")));
        }
        if show_project {
            out.push_str(&format!("  @ {}", self.project));
        }
        out
    }
}

pub struct NewMemory {
    pub project: String,
    pub kind: String,
    pub content: String,
    pub tags: Vec<String>,
    pub importance: i64,
    pub source: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Saved {
    pub memory: Memory,
    pub duplicate: bool,
    pub redacted: usize,
}

#[derive(Debug, Serialize)]
pub struct Hit {
    #[serde(flatten)]
    pub memory: Memory,
    pub score: f64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Forgotten {
    pub memories: usize,
    pub events: usize,
    pub messages: usize,
    pub peers: usize,
    /// False when another csm process kept the write-ahead log busy, so
    /// copies of the deleted text may remain in it until that process exits.
    pub scrubbed: bool,
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub schema_version: i64,
    pub memories: i64,
    pub projects: i64,
    pub project_memories: Option<i64>,
    /// Journal events in the requested project, or in total without one.
    pub events: i64,
}

/// Narrows a query. `None` fields match everything.
#[derive(Debug, Clone, Copy)]
pub struct Filter<'a> {
    pub project: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub limit: usize,
}

pub struct Store {
    pub(crate) conn: Connection,
    path: Option<PathBuf>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_timeout(path, Duration::from_secs(5))
    }

    /// Opens the database, waiting at most `busy` for other writers.
    pub fn open_with_timeout(path: &Path, busy: Duration) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        // Before anything else, so a contended first open waits instead of failing.
        conn.busy_timeout(busy)?;
        enable_wal(&conn, busy)?;
        Self::init(conn, Some(path.to_path_buf()))
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?, None)
    }

    fn init(conn: Connection, path: Option<PathBuf>) -> Result<Self> {
        // Zero freed pages so deleted memories do not linger on disk.
        conn.query_row("PRAGMA secure_delete = ON", [], |_| Ok(()))?;
        let store = Store { conn, path };
        store.migrate()?;
        store.init_messaging()?;
        store.init_wake()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let latest = MIGRATIONS.len() as i64;
        if self.schema_version()? == latest {
            return Ok(());
        }
        // Several hook processes can open a fresh database at once: take the
        // write lock first, then re-check what still needs applying.
        let tx = self.immediate()?;
        let version = self.schema_version()?;
        if version > latest {
            return Err(Error::Invalid(format!(
                "database schema v{version} is newer than this csm supports (v{latest}); upgrade csm"
            )));
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            tx.execute_batch(sql)?;
            tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
        }
        tx.commit()?;
        Ok(())
    }

    fn schema_version(&self) -> Result<i64> {
        Ok(self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    /// Starts a write transaction that holds the database lock from the start.
    pub(crate) fn immediate(&self) -> Result<Transaction<'_>> {
        Ok(Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn save(&self, new: NewMemory) -> Result<Saved> {
        if !KINDS.contains(&new.kind.as_str()) {
            return Err(Error::Invalid(format!("kind must be one of: {}", KINDS.join(", "))));
        }
        if !(1..=5).contains(&new.importance) {
            return Err(Error::Invalid("importance must be between 1 and 5".into()));
        }
        let trimmed = new.content.trim();
        if trimmed.is_empty() {
            return Err(Error::Invalid("content is empty".into()));
        }
        if trimmed.chars().count() > MAX_CONTENT_CHARS {
            return Err(Error::Invalid(format!("content is longer than {MAX_CONTENT_CHARS} characters")));
        }

        let content = redact(trimmed);
        let tags = redact(&normalize_tags(&new.tags).join(","));
        let source = new.source.as_deref().map(redact);
        let redacted = content.count + tags.count + source.as_ref().map_or(0, |s| s.count);

        let hash = content_hash(&content.text);
        let tx = self.immediate()?;
        if let Some(existing) = self.find_duplicate(&new.project, &hash, &content.text)? {
            return Ok(Saved { memory: existing, duplicate: true, redacted });
        }

        tx.execute(
            "INSERT INTO memories (project, kind, content, tags, importance, source, content_hash, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                new.project,
                new.kind,
                content.text,
                tags.text,
                new.importance,
                source.map(|s| s.text),
                hash,
                now_ms()
            ],
        )?;
        let id = tx.last_insert_rowid();
        let memory = self.get(id)?.ok_or_else(|| Error::Invalid(format!("memory #{id} vanished after insert")))?;
        tx.commit()?;
        Ok(Saved { memory, duplicate: false, redacted })
    }

    fn find_duplicate(&self, project: &str, hash: &str, content: &str) -> Result<Option<Memory>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLUMNS} FROM memories m WHERE m.project = ?1 AND m.content_hash = ?2"))?;
        let wanted = normalize_whitespace(content);
        let rows = stmt.query_map(params![project, hash], row_to_memory)?;
        for row in rows {
            let memory = row?;
            if normalize_whitespace(&memory.content) == wanted {
                return Ok(Some(memory));
            }
        }
        Ok(None)
    }

    pub fn get(&self, id: i64) -> Result<Option<Memory>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {COLUMNS} FROM memories m WHERE m.id = ?1"))?;
        let mut rows = stmt.query_map([id], row_to_memory)?;
        Ok(rows.next().transpose()?)
    }

    /// Full-text search ranked by BM25, nudged by importance.
    pub fn search(&self, query: &str, filter: Filter) -> Result<Vec<Hit>> {
        let Some(fts) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let limit = filter.limit.clamp(1, 100);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS}, bm25(memories_fts)
             FROM memories_fts JOIN memories m ON m.id = memories_fts.rowid
             WHERE memories_fts MATCH ?1
               AND (?2 IS NULL OR m.project = ?2)
               AND (?3 IS NULL OR m.kind = ?3)
             ORDER BY bm25(memories_fts)
             LIMIT ?4"
        ))?;
        let rows = stmt.query_map(params![fts, filter.project, filter.kind, (limit * 4) as i64], |r| {
            Ok((row_to_memory(r)?, r.get::<_, f64>(8)?))
        })?;
        let mut hits = Vec::new();
        for row in rows {
            let (memory, rank) = row?;
            let weight = 0.8 + 0.1 * memory.importance as f64;
            hits.push(Hit { memory, score: -rank * weight });
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(limit);
        for hit in &mut hits {
            hit.score = (hit.score * 1000.0).round() / 1000.0;
        }
        Ok(hits)
    }

    /// Decisions, lessons and anything marked important, most important first.
    pub fn key_memories(&self, project: &str, limit: usize) -> Result<Vec<Memory>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM memories m
             WHERE m.project = ?1 AND (m.kind IN ('decision', 'lesson') OR m.importance >= 4)
             ORDER BY m.importance DESC, m.created_at DESC, m.id DESC
             LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![project, limit as i64], row_to_memory)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Newest first.
    pub fn list(&self, filter: Filter) -> Result<Vec<Memory>> {
        self.list_limited(filter, filter.limit.clamp(1, 1000) as i64)
    }

    /// Every matching memory, newest first, for export.
    pub fn export(&self, project: Option<&str>) -> Result<Vec<Memory>> {
        self.list_limited(Filter { project, kind: None, limit: 0 }, -1)
    }

    fn list_limited(&self, filter: Filter, limit: i64) -> Result<Vec<Memory>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM memories m
             WHERE (?1 IS NULL OR m.project = ?1) AND (?2 IS NULL OR m.kind = ?2)
             ORDER BY m.created_at DESC, m.id DESC
             LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![filter.project, filter.kind, limit], row_to_memory)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Deletes one memory. With `project`, only a memory in that project.
    pub fn delete(&self, id: i64, project: Option<&str>) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM memories WHERE id = ?1 AND (?2 IS NULL OR project = ?2)", params![id, project])?;
        if n > 0 {
            // Best effort: the deleted text is already unreachable.
            let _ = self.flush_wal();
        }
        Ok(n > 0)
    }

    pub fn count_project(&self, project: &str) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM memories WHERE project = ?1", [project], |r| r.get(0))?)
    }

    /// Every project key that has stored memories, events or messaging peers.
    pub fn project_keys(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT project FROM memories UNION SELECT project FROM events UNION SELECT project FROM csm_peers ORDER BY 1",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Permanently removes every memory, journal event and mailbox for a project,
    /// compacts the file and flushes the write-ahead log, so the deleted
    /// text is not left behind on disk.
    pub fn forget_project(&self, project: &str) -> Result<Forgotten> {
        let tx = self.immediate()?;
        let memories = tx.execute("DELETE FROM memories WHERE project = ?1", [project])?;
        let events = tx.execute("DELETE FROM events WHERE project = ?1", [project])?;
        tx.execute("DELETE FROM injections WHERE project = ?1", [project])?;
        // Remove both directions, including explicitly allowed cross-project messages.
        let messages = tx.execute(
            "DELETE FROM csm_messages WHERE from_peer IN (SELECT id FROM csm_peers WHERE project = ?1)
             OR to_peer IN (SELECT id FROM csm_peers WHERE project = ?1)",
            [project],
        )?;
        let peers = tx.execute("DELETE FROM csm_peers WHERE project = ?1", [project])?;
        tx.commit()?;
        self.conn.execute_batch("INSERT INTO memories_fts (memories_fts) VALUES ('optimize'); VACUUM;")?;
        let scrubbed = self.flush_wal()?;
        Ok(Forgotten { memories, events, messages, peers, scrubbed })
    }

    /// Copies the write-ahead log into the database and truncates it, so
    /// frames holding deleted text are gone. Returns false when another
    /// connection was mid-transaction and the log could not be reset.
    fn flush_wal(&self) -> Result<bool> {
        if self.path.is_none() {
            return Ok(true);
        }
        let busy: i64 = self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        Ok(busy == 0)
    }

    pub fn stats(&self, project: Option<&str>) -> Result<Stats> {
        let schema_version = self.schema_version()?;
        let (memories, projects) =
            self.conn.query_row("SELECT COUNT(*), COUNT(DISTINCT project) FROM memories", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        let events =
            self.conn
                .query_row("SELECT COUNT(*) FROM events WHERE ?1 IS NULL OR project = ?1", [project], |r| r.get(0))?;
        let project_memories = project.map(|p| self.count_project(p)).transpose()?;
        Ok(Stats { schema_version, memories, projects, project_memories, events })
    }
}

/// Switches a database to WAL so hooks, the MCP server and the CLI can write
/// concurrently. WAL is persistent, so this only does work on a fresh file.
///
/// Several processes converting the same fresh file at once deadlock on the
/// lock upgrade, and SQLite then fails fast with SQLITE_BUSY instead of
/// calling the busy handler, so retry by hand until the deadline. A database
/// that cannot use WAL at all (in-memory, temp) just keeps its mode.
fn enable_wal(conn: &Connection, wait: Duration) -> Result<()> {
    let deadline = Instant::now() + wait;
    loop {
        let mode = conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0)).and_then(|mode| {
            if mode.eq_ignore_ascii_case("wal") {
                Ok(mode)
            } else {
                conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0))
            }
        });
        match mode {
            Ok(_) => return Ok(()),
            Err(e) if is_busy(&e) && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(e.into()),
        }
    }
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _) if matches!(f.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn row_to_memory(r: &Row) -> rusqlite::Result<Memory> {
    let tags: String = r.get(4)?;
    Ok(Memory {
        id: r.get(0)?,
        project: r.get(1)?,
        kind: r.get(2)?,
        content: r.get(3)?,
        tags: tags.split(',').filter(|t| !t.is_empty()).map(str::to_string).collect(),
        importance: r.get(5)?,
        source: r.get(6)?,
        created_at: iso(r.get(7)?),
    })
}

fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tag in tags.iter().flat_map(|t| t.split(',')) {
        let tag = tag.trim().to_lowercase();
        if !tag.is_empty() && !out.contains(&tag) {
            out.push(tag);
        }
    }
    out
}

fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// FNV-1a over whitespace-normalized content. Only a prefilter for exact
/// duplicate detection; matches are confirmed by comparing the text.
pub(crate) fn content_hash(content: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in normalize_whitespace(content).bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Turns free text into an FTS5 query: each word quoted, OR-ed together, so
/// user input can never be parsed as FTS syntax.
fn fts_query(query: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in query.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let term = format!("\"{}\"", word.to_lowercase());
        if !word.is_empty() && !terms.contains(&term) {
            terms.push(term);
        }
    }
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "C:/work/app";
    const OTHER: &str = "C:/work/other";

    fn new(project: &str, kind: &str, content: &str) -> NewMemory {
        NewMemory {
            project: project.into(),
            kind: kind.into(),
            content: content.into(),
            tags: vec![],
            importance: 3,
            source: None,
        }
    }

    fn all(project: Option<&str>) -> Filter<'_> {
        Filter { project, kind: None, limit: 50 }
    }

    #[test]
    fn saves_and_reads_back() {
        let store = Store::open_in_memory().unwrap();
        let mut m = new(P, "decision", "  Use SQLite FTS5 for search  ");
        m.tags = vec!["Search, storage".into(), "search".into()];
        m.importance = 5;
        let saved = store.save(m).unwrap();
        assert!(!saved.duplicate);
        let got = store.get(saved.memory.id).unwrap().unwrap();
        assert_eq!(got.content, "Use SQLite FTS5 for search");
        assert_eq!(got.tags, vec!["search", "storage"]);
        assert_eq!(got.importance, 5);
        assert_eq!(got.created_at.len(), 20);
    }

    #[test]
    fn rejects_bad_input() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.save(new(P, "rumor", "x")).is_err());
        assert!(store.save(new(P, "note", "   ")).is_err());
        let mut m = new(P, "note", "x");
        m.importance = 9;
        assert!(store.save(m).is_err());
        assert!(store.save(new(P, "note", &"a".repeat(MAX_CONTENT_CHARS + 1))).is_err());
    }

    #[test]
    fn detects_exact_duplicates_per_project() {
        let store = Store::open_in_memory().unwrap();
        let first = store.save(new(P, "note", "build with cargo")).unwrap();
        let again = store.save(new(P, "note", "build   with\ncargo")).unwrap();
        assert!(again.duplicate);
        assert_eq!(again.memory.id, first.memory.id);
        let elsewhere = store.save(new(OTHER, "note", "build with cargo")).unwrap();
        assert!(!elsewhere.duplicate);
    }

    #[test]
    fn redacts_before_storing() {
        let store = Store::open_in_memory().unwrap();
        let saved = store.save(new(P, "fact", "staging key sk-proj-AbCdEfGhIjKlMnOpQrStUv12")).unwrap();
        assert_eq!(saved.redacted, 1);
        assert!(!saved.memory.content.contains("AbCdEf"));
        assert!(store.search("AbCdEfGhIjKlMnOpQrStUv12", all(None)).unwrap().is_empty());
    }

    #[test]
    fn search_ranks_and_scopes_by_project() {
        let store = Store::open_in_memory().unwrap();
        store.save(new(P, "note", "the parser handles unicode identifiers")).unwrap();
        store.save(new(P, "lesson", "parser tests must run with --release because parsers are slow")).unwrap();
        store.save(new(OTHER, "note", "parser in the other project")).unwrap();
        store.save(new(P, "note", "unrelated deployment note")).unwrap();

        let hits = store.search("parsers", all(Some(P))).unwrap();
        assert_eq!(hits.len(), 2, "porter stemming should match parser/parsers");
        assert!(hits.iter().all(|h| h.memory.project == P));
        assert!(hits[0].score >= hits[1].score);

        let everywhere = store.search("parser", all(None)).unwrap();
        assert_eq!(everywhere.len(), 3);

        let lessons = store.search("parser", Filter { kind: Some("lesson"), ..all(Some(P)) }).unwrap();
        assert_eq!(lessons.len(), 1);
    }

    #[test]
    fn search_treats_fts_syntax_as_plain_words() {
        let store = Store::open_in_memory().unwrap();
        store.save(new(P, "note", "use NEAR for proximity")).unwrap();
        for q in [r#""unbalanced"#, "NEAR(", "a OR AND NOT", "col:*", "***", ""] {
            store.search(q, all(None)).unwrap();
        }
        assert_eq!(store.search("near(", all(None)).unwrap().len(), 1);
    }

    #[test]
    fn importance_breaks_ties() {
        let store = Store::open_in_memory().unwrap();
        let mut low = new(P, "note", "rotate the signing certificate yearly");
        low.importance = 1;
        let mut high = new(P, "note", "rotate the signing certificate monthly");
        high.importance = 5;
        store.save(low).unwrap();
        let high_id = store.save(high).unwrap().memory.id;
        let hits = store.search("rotate certificate", all(Some(P))).unwrap();
        assert_eq!(hits[0].memory.id, high_id);
    }

    #[test]
    fn lists_newest_first_with_limit() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..5 {
            store.save(new(P, "note", &format!("note {i}"))).unwrap();
        }
        let listed = store.list(Filter { limit: 3, ..all(Some(P)) }).unwrap();
        assert_eq!(listed.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(), ["note 4", "note 3", "note 2"]);
        assert_eq!(store.export(Some(P)).unwrap().len(), 5);
    }

    #[test]
    fn delete_respects_project_scope() {
        let store = Store::open_in_memory().unwrap();
        let id = store.save(new(P, "note", "keep me scoped")).unwrap().memory.id;
        assert!(!store.delete(id, Some(OTHER)).unwrap());
        assert!(store.delete(id, Some(P)).unwrap());
        assert!(store.get(id).unwrap().is_none());
        assert!(store.search("scoped", all(None)).unwrap().is_empty());
    }

    #[test]
    fn forget_removes_only_that_project() {
        let store = Store::open_in_memory().unwrap();
        store.save(new(P, "note", "alpha")).unwrap();
        store.save(new(P, "note", "beta")).unwrap();
        store.save(new(OTHER, "note", "alpha elsewhere")).unwrap();
        assert_eq!(
            store.forget_project(P).unwrap(),
            Forgotten { memories: 2, events: 0, messages: 0, peers: 0, scrubbed: true }
        );
        let stats = store.stats(Some(P)).unwrap();
        assert_eq!((stats.memories, stats.projects, stats.project_memories), (1, 1, Some(0)));
        assert_eq!(store.search("alpha", all(None)).unwrap().len(), 1);
    }

    fn file_contains(path: &Path, needle: &str) -> bool {
        std::fs::read(path).is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle.as_bytes()))
    }

    #[test]
    fn deleted_text_leaves_no_trace_on_disk_even_with_another_connection_open() {
        let dir = std::env::temp_dir().join(format!("csm-store-scrub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("csm.db");
        let wal = dir.join("csm.db-wal");
        // Stands in for a long-running `csm mcp` server.
        let other = Store::open(&path).unwrap();
        other.save(new(P, "note", "unrelated")).unwrap();

        let store = Store::open(&path).unwrap();
        let id = store.save(new(P, "note", "vault phrase qzvxkwjtrm pelicanmarmalade")).unwrap().memory.id;
        assert!(store.delete(id, Some(P)).unwrap());
        assert!(
            !file_contains(&path, "qzvxkwjtrm") && !file_contains(&wal, "qzvxkwjtrm"),
            "single delete left the text"
        );

        store.save(new(P, "note", "orbit phrase zzqqxxwwvv")).unwrap();
        store
            .record(crate::journal::NewEvent {
                project: P.into(),
                host: "claude".into(),
                kind: "prompt".into(),
                summary: "remember kumquatgalaxy".into(),
                ..Default::default()
            })
            .unwrap();
        let forgotten = store.forget_project(P).unwrap();
        assert_eq!((forgotten.memories, forgotten.events, forgotten.scrubbed), (2, 1, true));
        for needle in ["zzqqxxwwvv", "kumquatgalaxy", "unrelated"] {
            assert!(!file_contains(&path, needle), "{needle} left in csm.db");
            assert!(!file_contains(&wal, needle), "{needle} left in csm.db-wal");
        }
        drop(other);
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn in_memory_style_databases_open_without_waiting() {
        let started = std::time::Instant::now();
        Store::open(Path::new(":memory:")).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn upgrades_a_version_one_database() {
        let dir = std::env::temp_dir().join(format!("csm-store-upgrade-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("csm.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute_batch("PRAGMA user_version = 1").unwrap();
            conn.execute(
                "INSERT INTO memories (project, kind, content, content_hash, created_at) VALUES (?1, 'fact', 'old fact', 'h', 1)",
                [P],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.stats(None).unwrap().schema_version, MIGRATIONS.len() as i64);
        assert_eq!(store.search("old fact", all(Some(P))).unwrap().len(), 1);
        drop(store);

        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA user_version = 99").unwrap();
        drop(conn);
        assert!(Store::open(&path).is_err(), "a newer schema is refused");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reopens_existing_file() {
        let dir = std::env::temp_dir().join(format!("csm-store-test-{}", std::process::id()));
        let path = dir.join("nested").join("csm.db");
        {
            let store = Store::open(&path).unwrap();
            store.save(new(P, "fact", "persisted across opens")).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.search("persisted", all(Some(P))).unwrap().len(), 1);
        assert_eq!(store.stats(None).unwrap().schema_version, MIGRATIONS.len() as i64);
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
