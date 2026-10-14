//! The append-only event journal: what happened in each session, recorded by
//! hooks and by agents through `csm_checkpoint`. Rows are never updated;
//! later events (e.g. `goal_done`) supersede earlier ones.

use rusqlite::{params, OptionalExtension, Row};
use serde::Serialize;

use crate::redact::redact;
use crate::store::Store;
use crate::text::{one_line, truncate};
use crate::time::{iso, now_ms};
use crate::{Error, Result};

pub const EVENT_KINDS: &[&str] = &[
    "session_start",
    "compact",
    "prompt",
    "edit",
    "write",
    "delete",
    "command",
    "test",
    "turn_end",
    "session_end",
    "checkpoint",
    "goal",
    "goal_done",
];

/// Kinds recorded automatically by hooks. They age out; agent-authored
/// kinds (checkpoint, goal, goal_done) stay until the project is forgotten.
const PRUNABLE_KINDS: &str =
    "'session_start','compact','prompt','edit','write','delete','command','test','turn_end','session_end'";
pub const KEEP_EVENTS_PER_PROJECT: i64 = 5_000;
pub const KEEP_DAYS: i64 = 90;
const PRUNE_BATCH: i64 = 1_000;

const MAX_SUMMARY_BYTES: usize = 300;
/// Checkpoints and goals are written by the agent as handoffs; give them room.
pub const MAX_HANDOFF_BYTES: usize = 500;
const MAX_DETAIL_BYTES: usize = 2_000;
const MAX_FILES: usize = 50;
const MAX_PATH_BYTES: usize = 300;
/// 5 blockers and 5 next steps at this size fit in MAX_DETAIL_BYTES.
const MAX_ITEM_BYTES: usize = 180;
const MAX_ITEMS: usize = 5;

const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, Default)]
pub struct NewEvent {
    pub project: String,
    pub host: String,
    pub session_id: Option<String>,
    pub agent_id: String,
    pub kind: String,
    pub tool: Option<String>,
    pub summary: String,
    pub files: Vec<String>,
    pub ok: Option<bool>,
    pub detail: Option<String>,
    pub dedup_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub id: i64,
    pub project: String,
    pub host: String,
    pub session_id: Option<String>,
    pub agent_id: String,
    pub kind: String,
    pub tool: Option<String>,
    pub summary: String,
    pub files: Vec<String>,
    pub ok: Option<bool>,
    pub detail: Option<String>,
    #[serde(skip)]
    pub ts: i64,
    pub at: String,
}

/// The most recent session in a project, aggregated from its events.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub host: String,
    pub session_id: String,
    pub first_ts: i64,
    pub last_ts: i64,
    pub prompts: i64,
    pub ended: bool,
}

#[derive(Debug, Default)]
pub struct EventQuery<'a> {
    pub project: Option<&'a str>,
    pub session_id: Option<&'a str>,
    /// Briefing-only exclusion; raw journal/export queries keep every event.
    pub exclude_suggestion_sessions: bool,
    pub kinds: &'a [&'a str],
    /// Leave out events from these sessions (events without one are kept).
    pub exclude_sessions: &'a [&'a str],
    pub since_ts: Option<i64>,
    pub before_id: Option<i64>,
    pub limit: usize,
}

/// What `csm_checkpoint` records.
#[derive(Debug, Default)]
pub struct Checkpoint {
    pub project: String,
    pub host: String,
    pub session_id: Option<String>,
    pub summary: String,
    pub next: Vec<String>,
    pub blockers: Vec<String>,
    pub goal: Option<String>,
    pub goal_done: bool,
}

#[derive(Debug, Serialize)]
pub struct CheckpointResult {
    pub checkpoint_id: i64,
    pub goal_id: Option<i64>,
    pub closed_goal: Option<String>,
}

// Recognize the exact Codex app preamble in recorded root prompts. Every
// root prompt must have it, so a human continuation preserves the session.
// Match project + host + session, and remove the whole session (including
// child events) before query limits. Historical journal rows stay intact.
const EXCLUDE_SUGGESTION_SESSIONS: &str = "
    AND NOT EXISTS (
        SELECT 1 FROM events AS prompt
        WHERE events.host = 'codex' AND prompt.host = events.host
          AND prompt.project = events.project AND prompt.session_id = events.session_id
          AND prompt.kind = 'prompt' AND prompt.agent_id = ''
        GROUP BY prompt.session_id
        HAVING MIN(instr(prompt.summary, 'This is an app-generated request for a suggested next user message. The user did not write this message.') = 1) = 1
    )";

fn suggestion_filter(exclude: bool) -> &'static str {
    if exclude {
        EXCLUDE_SUGGESTION_SESSIONS
    } else {
        ""
    }
}

const EVENT_COLUMNS: &str = "id, project, host, session_id, agent_id, kind, tool, summary, files, ok, detail, ts";

fn row_to_event(r: &Row) -> rusqlite::Result<Event> {
    let files: String = r.get(8)?;
    let ts: i64 = r.get(11)?;
    Ok(Event {
        id: r.get(0)?,
        project: r.get(1)?,
        host: r.get(2)?,
        session_id: r.get(3)?,
        agent_id: r.get(4)?,
        kind: r.get(5)?,
        tool: r.get(6)?,
        summary: r.get(7)?,
        files: files.lines().filter(|l| !l.is_empty()).map(str::to_string).collect(),
        ok: r.get::<_, Option<i64>>(9)?.map(|v| v != 0),
        detail: r.get(10)?,
        ts,
        at: iso(ts),
    })
}

impl Store {
    /// Appends one event. Returns `None` when an event with the same
    /// `(host, session_id, agent_id, dedup_key)` already exists.
    pub fn record(&self, event: NewEvent) -> Result<Option<i64>> {
        self.record_at(event, now_ms())
    }

    pub fn record_at(&self, e: NewEvent, ts: i64) -> Result<Option<i64>> {
        if !EVENT_KINDS.contains(&e.kind.as_str()) {
            return Err(Error::Invalid(format!("unknown event kind: {}", e.kind)));
        }
        if e.project.is_empty() || e.host.is_empty() {
            return Err(Error::Invalid("event needs a project and a host".into()));
        }
        let cap = if matches!(e.kind.as_str(), "checkpoint" | "goal" | "goal_done") {
            MAX_HANDOFF_BYTES
        } else {
            MAX_SUMMARY_BYTES
        };
        let summary = truncate(&one_line(&redact(&e.summary).text), cap);
        let detail = e.detail.map(|d| truncate(redact(&d).text.trim(), MAX_DETAIL_BYTES)).filter(|d| !d.is_empty());
        let mut files: Vec<String> = Vec::new();
        for f in &e.files {
            let f = truncate(&one_line(&redact(f).text), MAX_PATH_BYTES);
            if !f.is_empty() && !files.contains(&f) {
                files.push(f);
            }
            if files.len() == MAX_FILES {
                break;
            }
        }
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO events
                 (project, host, session_id, agent_id, kind, tool, summary, files, ok, detail, dedup_key, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                e.project,
                e.host,
                e.session_id,
                e.agent_id,
                e.kind,
                e.tool,
                summary,
                files.join("\n"),
                e.ok,
                detail,
                e.dedup_key,
                ts
            ],
        )?;
        Ok((n > 0).then(|| self.conn.last_insert_rowid()))
    }

    /// Newest first.
    pub fn events(&self, q: &EventQuery) -> Result<Vec<Event>> {
        let json =
            |items: &[&str]| (!items.is_empty()).then(|| serde_json::to_string(items).expect("strings serialize"));
        let suggestion_filter = suggestion_filter(q.exclude_suggestion_sessions);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM events
             WHERE (?1 IS NULL OR project = ?1)
               AND (?2 IS NULL OR session_id = ?2)
               AND (?3 IS NULL OR kind IN (SELECT value FROM json_each(?3)))
               AND (?4 IS NULL OR session_id IS NULL OR session_id NOT IN (SELECT value FROM json_each(?4)))
               AND (?5 IS NULL OR ts >= ?5)
               AND (?6 IS NULL OR id < ?6)
               {suggestion_filter}
             ORDER BY id DESC
             LIMIT ?7"
        ))?;
        let rows = stmt.query_map(
            params![
                q.project,
                q.session_id,
                json(q.kinds),
                json(q.exclude_sessions),
                q.since_ts,
                q.before_id,
                q.limit.clamp(1, i64::MAX as usize) as i64
            ],
            row_to_event,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Id and dedup key of the newest event of `kind` from one agent in one
    /// session.
    pub fn last_event_key(
        &self,
        host: &str,
        session_id: &str,
        agent_id: &str,
        kind: &str,
    ) -> Result<Option<(i64, Option<String>)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, dedup_key FROM events
                 WHERE host = ?1 AND session_id = ?2 AND agent_id = ?3 AND kind = ?4
                 ORDER BY id DESC LIMIT 1",
                params![host, session_id, agent_id, kind],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn latest_event(&self, project: &str, kinds: &[&str]) -> Result<Option<Event>> {
        Ok(self.events(&EventQuery { project: Some(project), kinds, limit: 1, ..Default::default() })?.pop())
    }

    /// The most recently active session in `project` other than `exclude`,
    /// ignoring sessions that only started and never did anything.
    pub fn last_session(&self, project: &str, exclude: Option<&str>) -> Result<Option<SessionInfo>> {
        Ok(self.recent_sessions(project, exclude, 1)?.pop())
    }

    /// Up to `limit` sessions in `project`, most recently active first.
    pub fn recent_sessions(&self, project: &str, exclude: Option<&str>, limit: usize) -> Result<Vec<SessionInfo>> {
        self.query_recent_sessions(project, exclude, limit, false)
    }

    pub(crate) fn recent_briefing_sessions(
        &self,
        project: &str,
        exclude: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SessionInfo>> {
        self.query_recent_sessions(project, exclude, limit, true)
    }

    fn query_recent_sessions(
        &self,
        project: &str,
        exclude: Option<&str>,
        limit: usize,
        exclude_suggestions: bool,
    ) -> Result<Vec<SessionInfo>> {
        let suggestion_filter = suggestion_filter(exclude_suggestions);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT host, session_id, MIN(ts), MAX(ts), SUM(kind = 'prompt')
             FROM events
             WHERE project = ?1 AND session_id IS NOT NULL
               AND (?2 IS NULL OR session_id <> ?2)
               AND kind NOT IN ('session_start', 'compact', 'session_end')
               {suggestion_filter}
             GROUP BY host, session_id
             ORDER BY MAX(ts) DESC, MAX(id) DESC
             LIMIT ?3",
        ))?;
        let rows = stmt.query_map(params![project, exclude, limit as i64], |r| {
            Ok(SessionInfo {
                host: r.get(0)?,
                session_id: r.get(1)?,
                first_ts: r.get(2)?,
                last_ts: r.get(3)?,
                prompts: r.get(4)?,
                ended: false,
            })
        })?;
        let mut sessions: Vec<SessionInfo> = rows.collect::<rusqlite::Result<_>>()?;
        for s in &mut sessions {
            s.ended = self.session_ended(project, &s.host, &s.session_id)?;
        }
        Ok(sessions)
    }

    /// Ended when its newest event is a session end, so a session resumed
    /// under the same id counts as running again.
    fn session_ended(&self, project: &str, host: &str, session_id: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(CASE WHEN kind = 'session_end' THEN id END), 0)
                  > COALESCE(MAX(CASE WHEN kind <> 'session_end' THEN id END), 0)
             FROM events WHERE project = ?1 AND host = ?2 AND session_id = ?3",
            params![project, host, session_id],
            |r| r.get(0),
        )?)
    }

    /// Records that a briefing was injected, unless one was already injected
    /// for this session and trigger (within `window_ms`, when given). Returns
    /// whether the caller should inject.
    #[allow(clippy::too_many_arguments)]
    pub fn claim_injection(
        &self,
        project: &str,
        host: &str,
        session_id: &str,
        agent_id: &str,
        trigger: &str,
        window_ms: Option<i64>,
        text: &str,
        sources: &[String],
        now: i64,
    ) -> Result<bool> {
        let tx = self.immediate()?;
        let last: Option<i64> = tx.query_row(
            "SELECT MAX(ts) FROM injections WHERE host = ?1 AND session_id = ?2 AND agent_id = ?3 AND trigger = ?4",
            params![host, session_id, agent_id, trigger],
            |r| r.get(0),
        )?;
        if let Some(last) = last {
            if window_ms.is_none_or(|w| now - last < w) {
                return Ok(false);
            }
        }
        tx.execute(
            "INSERT INTO injections (project, host, session_id, agent_id, trigger, bytes, hash, sources, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                project,
                host,
                session_id,
                agent_id,
                trigger,
                text.len() as i64,
                crate::store::content_hash(text),
                sources.join(" "),
                now
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Size of the most recent injection, for `csm status`.
    pub fn last_injection(&self) -> Result<Option<(String, i64, i64)>> {
        Ok(self
            .conn
            .query_row("SELECT host, bytes, ts FROM injections ORDER BY id DESC LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?)
    }

    /// Ages out hook-recorded events: anything older than [`KEEP_DAYS`] and
    /// anything beyond the newest [`KEEP_EVENTS_PER_PROJECT`]. Bounded per
    /// call so it never stalls a hook.
    pub fn prune_events(&self, project: &str, now: i64) -> Result<usize> {
        let cutoff = now - KEEP_DAYS * DAY_MS;
        let n = self.conn.execute(
            &format!(
                "DELETE FROM events WHERE id IN (
                     SELECT id FROM events
                     WHERE project = ?1 AND kind IN ({PRUNABLE_KINDS})
                       AND (ts < ?2 OR id <= (SELECT id FROM events WHERE project = ?1
                                              ORDER BY id DESC LIMIT 1 OFFSET ?3))
                     ORDER BY id LIMIT ?4)"
            ),
            params![project, cutoff, KEEP_EVENTS_PER_PROJECT, PRUNE_BATCH],
        )?;
        self.conn.execute("DELETE FROM injections WHERE ts < ?1", [cutoff])?;
        Ok(n)
    }

    /// Records a checkpoint, and optionally closes the active goal and/or
    /// sets a new one, in one transaction.
    pub fn checkpoint(&self, c: Checkpoint) -> Result<CheckpointResult> {
        if c.summary.trim().is_empty() {
            return Err(Error::Invalid("checkpoint summary is empty".into()));
        }
        let now = now_ms();
        let event = |kind: &str, summary: String, detail: Option<String>| NewEvent {
            project: c.project.clone(),
            host: c.host.clone(),
            session_id: c.session_id.clone(),
            kind: kind.into(),
            summary,
            detail,
            ..Default::default()
        };
        let tx = self.immediate()?;
        let mut closed_goal = None;
        if c.goal_done {
            if let Some(active) = self.active_goal(&c.project)? {
                self.record_at(event("goal_done", active.summary.clone(), None), now)?;
                closed_goal = Some(active.summary);
            }
        }
        let goal_id = match c.goal.as_deref().map(str::trim).filter(|g| !g.is_empty()) {
            Some(goal) => self.record_at(event("goal", goal.to_string(), None), now)?,
            None => None,
        };
        let mut detail = Vec::new();
        for (label, items) in [("blocker", &c.blockers), ("next", &c.next)] {
            for item in items.iter().map(|i| one_line(&redact(i).text)).filter(|i| !i.is_empty()).take(MAX_ITEMS) {
                detail.push(format!("{label}: {}", truncate(&item, MAX_ITEM_BYTES)));
            }
        }
        let detail = (!detail.is_empty()).then(|| detail.join("\n"));
        let checkpoint_id = self
            .record_at(event("checkpoint", c.summary, detail), now)?
            .ok_or_else(|| Error::Invalid("checkpoint was not recorded".into()))?;
        tx.commit()?;
        Ok(CheckpointResult { checkpoint_id, goal_id, closed_goal })
    }

    /// The latest goal, unless a later `goal_done` closed it.
    pub fn active_goal(&self, project: &str) -> Result<Option<Event>> {
        Ok(self.latest_event(project, &["goal", "goal_done"])?.filter(|e| e.kind == "goal"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "C:/work/app";

    fn ev(kind: &str, session: &str, summary: &str) -> NewEvent {
        NewEvent {
            project: P.into(),
            host: "claude".into(),
            session_id: Some(session.into()),
            kind: kind.into(),
            summary: summary.into(),
            ..Default::default()
        }
    }

    #[test]
    fn records_and_reads_back_with_redaction_and_caps() {
        let store = Store::open_in_memory().unwrap();
        let mut e = ev("command", "s1", "Run: deploy --token=ghp_0123456789abcdefghijABCDEFGHIJ012345");
        e.files = vec!["src/a.rs".into(), "src/a.rs".into(), "x".repeat(500)];
        e.detail = Some(format!("{}\nboom", "line\n".repeat(1000)));
        e.ok = Some(false);
        let id = store.record(e).unwrap().unwrap();
        let got = &store.events(&EventQuery { project: Some(P), limit: 10, ..Default::default() }).unwrap()[0];
        assert_eq!(got.id, id);
        assert!(!got.summary.contains("ghp_0123"), "{}", got.summary);
        assert_eq!(got.files.len(), 2);
        assert!(got.files[1].len() <= MAX_PATH_BYTES);
        assert!(got.detail.as_ref().unwrap().len() <= MAX_DETAIL_BYTES);
        assert_eq!(got.ok, Some(false));
    }

    #[test]
    fn rejects_unknown_kinds() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.record(ev("rumor", "s1", "x")).is_err());
    }

    #[test]
    fn dedup_key_suppresses_repeats_per_session() {
        let store = Store::open_in_memory().unwrap();
        let mut e = ev("edit", "s1", "Edit a");
        e.dedup_key = Some("toolu_1".into());
        assert!(store.record(e.clone()).unwrap().is_some());
        assert!(store.record(e.clone()).unwrap().is_none());
        let mut other_session = e.clone();
        other_session.session_id = Some("s2".into());
        assert!(store.record(other_session).unwrap().is_some());
        let mut subagent = e;
        subagent.agent_id = "agent-7".into();
        assert!(store.record(subagent).unwrap().is_some());
    }

    #[test]
    fn last_session_skips_current_and_empty_sessions() {
        let store = Store::open_in_memory().unwrap();
        store.record(ev("session_start", "old", "startup")).unwrap();
        store.record(ev("prompt", "old", "fix the parser")).unwrap();
        store.record(ev("edit", "old", "Edit parser.rs")).unwrap();
        store.record(ev("session_end", "old", "end")).unwrap();
        store.record(ev("session_start", "empty", "startup")).unwrap();
        store.record(ev("session_start", "current", "startup")).unwrap();
        store.record(ev("prompt", "current", "hello")).unwrap();

        let s = store.last_session(P, Some("current")).unwrap().unwrap();
        assert_eq!(s.session_id, "old");
        assert_eq!(s.prompts, 1);
        assert!(s.ended);
        assert!(store.last_session("C:/elsewhere", None).unwrap().is_none());
    }

    #[test]
    fn a_resumed_session_is_running_again() {
        let store = Store::open_in_memory().unwrap();
        store.record(ev("prompt", "s1", "first run")).unwrap();
        store.record(ev("session_end", "s1", "exit")).unwrap();
        assert!(store.last_session(P, None).unwrap().unwrap().ended);
        store.record(ev("session_start", "s1", "resume")).unwrap();
        store.record(ev("prompt", "s1", "second run")).unwrap();
        assert!(!store.last_session(P, None).unwrap().unwrap().ended);
        store.record(ev("session_end", "s1", "exit")).unwrap();
        assert!(store.last_session(P, None).unwrap().unwrap().ended);
    }

    #[test]
    fn excluded_sessions_do_not_eat_the_limit() {
        let store = Store::open_in_memory().unwrap();
        store.record(ev("edit", "other", "Edit README.md")).unwrap();
        for i in 0..20 {
            store.record(ev("edit", "busy", &format!("Edit f{i}"))).unwrap();
        }
        let mut cp = ev("checkpoint", "x", "no session");
        cp.session_id = None;
        store.record(cp).unwrap();
        let got = store
            .events(&EventQuery { project: Some(P), exclude_sessions: &["busy"], limit: 5, ..Default::default() })
            .unwrap();
        let summaries: Vec<_> = got.iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(summaries, ["no session", "Edit README.md"]);
    }

    #[test]
    fn handoffs_get_more_room_than_tool_events() {
        let store = Store::open_in_memory().unwrap();
        let long = "word ".repeat(200);
        let mut cp = ev("checkpoint", "s1", &long);
        cp.session_id = None;
        store.record(cp).unwrap();
        store.record(ev("command", "s1", &long)).unwrap();
        let got = store.events(&EventQuery { project: Some(P), limit: 2, ..Default::default() }).unwrap();
        assert!(got[0].summary.len() <= MAX_SUMMARY_BYTES && got[0].summary.len() > 250);
        assert!(got[1].summary.len() <= MAX_HANDOFF_BYTES && got[1].summary.len() > 450);
    }

    #[test]
    fn checkpoint_keeps_blockers_within_the_detail_cap() {
        let store = Store::open_in_memory().unwrap();
        let long = |label: &str| (0..5).map(|i| format!("{label} {i} {}", "z".repeat(400))).collect::<Vec<_>>();
        store
            .checkpoint(Checkpoint {
                project: P.into(),
                host: "mcp".into(),
                summary: "s".into(),
                next: long("next"),
                blockers: long("blocker"),
                ..Default::default()
            })
            .unwrap();
        let detail = store.latest_event(P, &["checkpoint"]).unwrap().unwrap().detail.unwrap();
        assert_eq!(detail.lines().filter(|l| l.starts_with("blocker: ")).count(), 5, "{detail}");
        assert_eq!(detail.lines().filter(|l| l.starts_with("next: ")).count(), 5, "{detail}");
    }

    #[test]
    fn injection_claims_once_or_per_window() {
        let store = Store::open_in_memory().unwrap();
        let claim = |trigger: &str, window: Option<i64>, now: i64| {
            store.claim_injection(P, "claude", "s1", "", trigger, window, "brief", &[], now).unwrap()
        };
        assert!(claim("startup", None, 1_000));
        assert!(!claim("startup", None, 999_999_999));
        assert!(claim("compact", Some(60_000), 1_000));
        assert!(!claim("compact", Some(60_000), 30_000));
        assert!(claim("compact", Some(60_000), 70_000));
        assert_eq!(store.last_injection().unwrap().unwrap().1, 5);
    }

    #[test]
    fn prune_keeps_agent_authored_events() {
        let store = Store::open_in_memory().unwrap();
        let now = 200 * DAY_MS;
        store.record_at(ev("edit", "s1", "old edit"), now - 100 * DAY_MS).unwrap();
        store.record_at(ev("checkpoint", "s1", "old checkpoint"), now - 100 * DAY_MS).unwrap();
        store.record_at(ev("edit", "s1", "fresh edit"), now).unwrap();
        assert_eq!(store.prune_events(P, now).unwrap(), 1);
        let left: Vec<_> = store
            .events(&EventQuery { project: Some(P), limit: 10, ..Default::default() })
            .unwrap()
            .into_iter()
            .map(|e| e.summary)
            .collect();
        assert_eq!(left, ["fresh edit", "old checkpoint"]);
    }

    #[test]
    fn prune_caps_events_per_project() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..(KEEP_EVENTS_PER_PROJECT + 10) {
            store.record_at(ev("edit", "s1", &format!("e{i}")), 1_000 + i).unwrap();
        }
        assert_eq!(store.prune_events(P, 2_000_000).unwrap(), 10);
        assert_eq!(store.stats(Some(P)).unwrap().events, KEEP_EVENTS_PER_PROJECT);
    }

    #[test]
    fn checkpoint_manages_goals() {
        let store = Store::open_in_memory().unwrap();
        let cp = |summary: &str, goal: Option<&str>, done: bool| {
            store
                .checkpoint(Checkpoint {
                    project: P.into(),
                    host: "mcp".into(),
                    summary: summary.into(),
                    next: vec!["write tests".into(), "  ".into()],
                    blockers: vec!["flaky CI".into()],
                    goal: goal.map(str::to_string),
                    goal_done: done,
                    ..Default::default()
                })
                .unwrap()
        };
        let first = cp("store done", Some("ship v1"), false);
        assert!(first.goal_id.is_some());
        assert_eq!(store.active_goal(P).unwrap().unwrap().summary, "ship v1");

        let second = cp("released", None, true);
        assert_eq!(second.closed_goal.as_deref(), Some("ship v1"));
        assert!(store.active_goal(P).unwrap().is_none());

        let latest = store.latest_event(P, &["checkpoint"]).unwrap().unwrap();
        assert_eq!(latest.summary, "released");
        assert_eq!(latest.detail.as_deref(), Some("blocker: flaky CI\nnext: write tests"));
        assert!(store.checkpoint(Checkpoint { project: P.into(), host: "mcp".into(), ..Default::default() }).is_err());
    }

    #[test]
    fn forget_removes_events_too() {
        let store = Store::open_in_memory().unwrap();
        store.record(ev("edit", "s1", "Edit a")).unwrap();
        store.claim_injection(P, "claude", "s1", "", "startup", None, "x", &[], 1).unwrap();
        let forgotten = store.forget_project(P).unwrap();
        assert_eq!((forgotten.memories, forgotten.events), (0, 1));
        assert_eq!(store.stats(Some(P)).unwrap().events, 0);
        assert!(store.last_injection().unwrap().is_none());
    }
}
