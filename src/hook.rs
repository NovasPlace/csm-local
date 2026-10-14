//! `csm hook <claude|codex>`: reads one hook payload from stdin, records it in
//! the journal, and on SessionStart prints the briefing.
//!
//! A hook must never get in the host's way. This one always exits 0, prints
//! nothing except a SessionStart context payload, never emits a decision,
//! and writes failures to a small log file instead of stderr.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{json, Value};

use crate::brief::{self, BriefRequest};
use crate::capture::tool_event;
pub use crate::capture::Host;
use crate::journal::NewEvent;
use crate::redact::redact;
use crate::store::{content_hash, Store};
use crate::text::{one_line, truncate};
use crate::time::{iso, now_ms};
use crate::{paths, project, Error, Result};

const BUSY_TIMEOUT: Duration = Duration::from_millis(1500);
const MAX_PROMPT_BYTES: usize = 300;
const MAX_REPLY_BYTES: usize = 400;
const MAX_LOG_BYTES: u64 = 256 * 1024;
/// Re-injection after compaction or resume is allowed again after this long;
/// it only exists to absorb duplicate firings of the same hook.
const REINJECT_WINDOW_MS: i64 = 60_000;

pub struct HookEnv {
    /// `CLAUDE_PROJECT_DIR`: Claude's stable project root (payload `cwd` can
    /// change mid-session).
    pub project_dir: Option<String>,
    /// Whether prompt and reply text is kept (redacted and capped).
    pub capture_text: bool,
    pub budget: usize,
    pub now: i64,
}

impl HookEnv {
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        HookEnv {
            project_dir: var("CLAUDE_PROJECT_DIR"),
            capture_text: !matches!(
                var("CSM_CAPTURE_TEXT").map(|v| v.to_ascii_lowercase()).as_deref(),
                Some("0" | "false" | "off" | "no")
            ),
            budget: var("CSM_BRIEF_BYTES").and_then(|v| v.parse().ok()).unwrap_or(brief::DEFAULT_BUDGET),
            now: now_ms(),
        }
    }
}

/// Entry point for `csm hook`. `args` are the other command-line words:
/// the host name and optionally `--db <path>` / `--db=<path>`. Anything else
/// is ignored rather than rejected, since rejecting would mean failing.
pub fn main(args: &[String]) -> ExitCode {
    std::panic::set_hook(Box::new(|_| {}));
    let mut host_name = String::new();
    let mut db = None;
    let mut words = args.iter();
    while let Some(word) = words.next() {
        if word == "--db" {
            db = words.next().map(PathBuf::from);
        } else if let Some(path) = word.strip_prefix("--db=") {
            db = Some(PathBuf::from(path));
        } else if host_name.is_empty() && !word.starts_with('-') {
            host_name = word.clone();
        }
    }
    let failure = match std::panic::catch_unwind(|| run(&host_name, db)) {
        Ok(Ok(Some(out))) => {
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(out.as_bytes());
            let _ = stdout.flush();
            None
        }
        Ok(Ok(None)) => None,
        Ok(Err(e)) => Some(e.to_string()),
        Err(_) => Some("panic".to_string()),
    };
    if let Some(message) = failure {
        log_failure(&host_name, &message);
    }
    ExitCode::SUCCESS
}

fn run(host_name: &str, db: Option<PathBuf>) -> Result<Option<String>> {
    let host = Host::parse(host_name).ok_or_else(|| Error::Invalid(format!("unknown hook host {host_name:?}")))?;
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let payload = parse_input(&input).ok_or_else(|| Error::Invalid("hook input is not a JSON object".into()))?;
    let db = match db {
        // The hook runs in the user's repository: never resolve a relative
        // path against it.
        Some(path) if !path.is_absolute() => return Err(Error::Invalid("--db must be an absolute path".into())),
        Some(path) => path,
        None => paths::default_db_path()?,
    };
    let store = Store::open_with_timeout(&db, BUSY_TIMEOUT)?;
    handle(&store, host, &payload, &HookEnv::from_env())
}

/// Lenient decoding: tolerates a UTF-8 BOM and invalid UTF-8.
pub fn parse_input(bytes: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_start_matches('\u{feff}').trim();
    serde_json::from_str(text).ok().filter(Value::is_object)
}

/// Records one hook event. Returns the text to print, if any.
pub fn handle(store: &Store, host: Host, payload: &Value, env: &HookEnv) -> Result<Option<String>> {
    let field = |key: &str| payload.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let Some(event_name) = field("hook_event_name") else {
        return Ok(None);
    };
    // Without a session an event cannot be attributed; skip rather than guess.
    let Some(session_id) = field("session_id") else {
        return Ok(None);
    };
    let dir = match host {
        Host::Claude => env.project_dir.as_deref().or(field("cwd")),
        Host::Codex => field("cwd"),
    };
    let Some(dir) = dir else {
        return Ok(None);
    };
    let project = project::resolve(Path::new(dir));
    let agent_id = field("agent_id").unwrap_or("").to_string();
    let base = NewEvent {
        project: project.clone(),
        host: host.name().into(),
        session_id: Some(session_id.into()),
        agent_id: agent_id.clone(),
        ..Default::default()
    };
    let turn = field("turn_id").or(field("prompt_id"));
    // Redact the whole text first: cutting it could split a secret so the
    // patterns no longer recognize it.
    let captured = |key: &str, max: usize| match field(key) {
        Some(text) if env.capture_text => truncate(&one_line(&redact(text).text), max),
        _ => String::new(),
    };

    match event_name {
        "SessionStart" => {
            let source = field("source").unwrap_or("startup");
            let compact = source == "compact";
            let start = NewEvent {
                kind: if compact { "compact" } else { "session_start" }.into(),
                summary: source.into(),
                dedup_key: (!compact).then(|| format!("start:{source}")),
                ..base
            };
            // The briefing only reads, so a failed write must not cost the
            // session its briefing.
            let recorded = store.record_at(start, env.now);
            if let Err(e) = &recorded {
                log_failure(host.name(), &e.to_string());
            }
            // Subagents work for a parent that already has the briefing.
            if !agent_id.is_empty() {
                return Ok(None);
            }
            let out = session_start_context(store, host, &project, session_id, source, env)?;
            if recorded.is_ok() {
                let _ = store.prune_events(&project, env.now);
            }
            Ok(out)
        }
        "UserPromptSubmit" => {
            // Codex delivers prompts typed while a turn runs under that
            // turn's id, so the id alone does not identify a prompt, and the
            // same words can legitimately come again later in the turn.
            let summary = captured("prompt", MAX_PROMPT_BYTES);
            if let Some(dedup_key) = turn_key(store, host, session_id, &agent_id, "prompt", turn, field("prompt"))? {
                store.record_at(NewEvent { kind: "prompt".into(), summary, dedup_key, ..base }, env.now)?;
            }
            Ok(None)
        }
        "PostToolUse" | "PostToolUseFailure" => {
            let mut roots = vec![project.clone()];
            roots.extend(project::root_aliases(Path::new(dir)).into_iter().filter(|a| *a != project));
            if let Some(t) = tool_event(host, payload, &roots, event_name == "PostToolUseFailure") {
                let tool = NewEvent {
                    kind: t.kind.into(),
                    tool: Some(t.tool),
                    summary: t.summary,
                    files: t.files,
                    ok: t.ok,
                    detail: t.detail,
                    dedup_key: field("tool_use_id").map(|id| format!("tool:{id}")),
                    ..base
                };
                store.record_at(tool, env.now)?;
            }
            Ok(None)
        }
        // csm never blocks a Stop, so `stop_hook_active` (another hook made
        // the turn continue) is just one more reply, and the latest wins.
        "Stop" | "SubagentStop" => {
            let summary = captured("last_assistant_message", MAX_REPLY_BYTES);
            let reply = field("last_assistant_message");
            if let Some(dedup_key) = turn_key(store, host, session_id, &agent_id, "turn_end", turn, reply)? {
                store.record_at(NewEvent { kind: "turn_end".into(), summary, dedup_key, ..base }, env.now)?;
            }
            Ok(None)
        }
        // A turn that ended without a normal Stop: Codex `Interrupt`, Claude
        // `StopFailure` (API error).
        "Interrupt" | "StopFailure" => {
            let how = if event_name == "Interrupt" { "interrupted" } else { "error" };
            let dedup_key = turn.map(|t| format!("{how}:{t}"));
            let end = NewEvent { kind: "turn_end".into(), detail: Some(how.into()), dedup_key, ..base };
            store.record_at(end, env.now)?;
            Ok(None)
        }
        "SessionEnd" => {
            // No dedup key: a session resumed under the same id ends again.
            let end = NewEvent { kind: "session_end".into(), summary: field("reason").unwrap_or("end").into(), ..base };
            store.record_at(end, env.now)?;
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// Dedup key for a prompt or turn end within a turn: `Ok(None)` when this
/// is a repeat firing of the newest such event (same turn, same text);
/// otherwise a key that is unique to this position in the session, so the
/// same text arriving again after something else (A, B, A) is kept. Without
/// a turn id there is nothing to dedupe against.
fn turn_key(
    store: &Store,
    host: Host,
    session_id: &str,
    agent_id: &str,
    kind: &str,
    turn: Option<&str>,
    text: Option<&str>,
) -> Result<Option<Option<String>>> {
    let Some(turn) = turn else {
        return Ok(Some(None));
    };
    let stem = format!("{kind}:{turn}:{}", content_hash(text.unwrap_or("")));
    let newest = store.last_event_key(host.name(), session_id, agent_id, kind)?;
    if let Some((_, Some(key))) = &newest {
        if key.starts_with(&format!("{stem}:")) {
            return Ok(None);
        }
    }
    let position = newest.map_or(0, |(id, _)| id);
    Ok(Some(Some(format!("{stem}:{position}"))))
}

fn session_start_context(
    store: &Store,
    host: Host,
    project: &str,
    session_id: &str,
    source: &str,
    env: &HookEnv,
) -> Result<Option<String>> {
    let request = BriefRequest {
        project,
        current_session: Some(session_id),
        exclude_active: false,
        budget: env.budget,
        now: env.now,
    };
    let Some(briefing) = brief::build(store, &request)? else {
        return Ok(None);
    };
    let window = matches!(source, "compact" | "resume").then_some(REINJECT_WINDOW_MS);
    // If the guard cannot be written, a rare duplicate briefing beats none.
    let claimed = store
        .claim_injection(
            project,
            host.name(),
            session_id,
            "",
            source,
            window,
            &briefing.text,
            &briefing.sources,
            env.now,
        )
        .unwrap_or_else(|e| {
            log_failure(host.name(), &e.to_string());
            true
        });
    if !claimed {
        return Ok(None);
    }
    let out = json!({
        "hookSpecificOutput": { "hookEventName": "SessionStart", "additionalContext": briefing.text }
    });
    Ok(Some(out.to_string()))
}

/// Appends one line (time, host, error class) to the hook log. Never
/// includes payload content.
fn log_failure(host: &str, message: &str) {
    let Ok(path) = paths::hook_log_path() else {
        return;
    };
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let line = format!("{} {} {}\n", iso(now_ms()), truncate(&one_line(host), 16), truncate(&one_line(message), 300));
    let _ = OpenOptions::new().create(true).append(true).open(&path).and_then(|mut f| f.write_all(line.as_bytes()));
}

/// The most recent hook failure, for `csm status`.
pub fn last_failure() -> Option<String> {
    let text = std::fs::read_to_string(paths::hook_log_path().ok()?).ok()?;
    text.lines().last().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::EventQuery;

    const DIR: &str = "C:/work/app";

    fn env(now: i64) -> HookEnv {
        HookEnv { project_dir: None, capture_text: true, budget: brief::DEFAULT_BUDGET, now }
    }

    fn send(store: &Store, host: Host, payload: Value, now: i64) -> Option<String> {
        handle(store, host, &payload, &env(now)).unwrap()
    }

    fn project() -> String {
        project::resolve(Path::new(DIR))
    }

    fn events(store: &Store) -> Vec<crate::journal::Event> {
        store.events(&EventQuery { limit: 100, ..Default::default() }).unwrap()
    }

    /// A Claude session that edits a file and fails a test, then ends.
    fn claude_session(store: &Store, session: &str, now: i64) {
        let base = json!({ "session_id": session, "cwd": DIR, "transcript_path": null });
        let with = |extra: Value| {
            let mut v = base.clone();
            v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            v
        };
        send(store, Host::Claude, with(json!({ "hook_event_name": "SessionStart", "source": "startup" })), now);
        send(
            store,
            Host::Claude,
            with(json!({ "hook_event_name": "UserPromptSubmit", "prompt": "make the parser handle unicode" })),
            now + 1,
        );
        send(
            store,
            Host::Claude,
            with(json!({ "hook_event_name": "PostToolUse", "tool_name": "Edit", "tool_use_id": "toolu_1",
            "tool_input": { "file_path": format!("{DIR}/src/parser.rs") }, "tool_response": {} })),
            now + 2,
        );
        send(
            store,
            Host::Claude,
            with(json!({ "hook_event_name": "PostToolUseFailure", "tool_name": "Bash", "tool_use_id": "toolu_2",
            "tool_input": { "command": "cargo test" }, "error": "Exit code 101\ntest parse_unicode ... FAILED", "is_interrupt": false })),
            now + 3,
        );
        send(
            store,
            Host::Claude,
            with(json!({ "hook_event_name": "Stop", "stop_hook_active": false,
            "last_assistant_message": "Parser updated; parse_unicode still fails." })),
            now + 4,
        );
        send(
            store,
            Host::Claude,
            with(json!({ "hook_event_name": "SessionEnd", "reason": "prompt_input_exit" })),
            now + 5,
        );
    }

    #[test]
    fn records_a_session_and_briefs_the_next_one() {
        let store = Store::open_in_memory().unwrap();
        let t0 = 1_791_900_000_000;
        claude_session(&store, "s1", t0);
        let kinds: Vec<String> = events(&store).into_iter().rev().map(|e| e.kind).collect();
        assert_eq!(kinds, ["session_start", "prompt", "edit", "test", "turn_end", "session_end"]);

        let out = send(
            &store,
            Host::Codex,
            json!({ "hook_event_name": "SessionStart", "session_id": "s2", "cwd": DIR, "source": "startup",
                    "model": "gpt", "permission_mode": "default", "transcript_path": null }),
            t0 + 3_700_000,
        )
        .expect("briefing for the next session");
        let parsed: Value = serde_json::from_str(&out).unwrap();
        let obj = parsed.as_object().unwrap();
        assert_eq!(obj.keys().collect::<Vec<_>>(), ["hookSpecificOutput"]);
        let specific = obj["hookSpecificOutput"].as_object().unwrap();
        let mut keys: Vec<_> = specific.keys().collect();
        keys.sort();
        assert_eq!(keys, ["additionalContext", "hookEventName"]);
        assert_eq!(specific["hookEventName"], "SessionStart");
        let context = specific["additionalContext"].as_str().unwrap();
        assert!(context.contains("Previous session (via claude, ended 1h ago, 1 prompt):"), "{context}");
        assert!(context.contains("Last request: \"make the parser handle unicode\""));
        assert!(context.contains("Files changed: src/parser.rs"));
        assert!(context.contains("Last recorded failure (1h ago, via claude): cargo test"));
        assert!(context.contains("test parse_unicode ... FAILED"));
    }

    #[test]
    fn injects_once_per_session_and_again_after_compaction() {
        let store = Store::open_in_memory().unwrap();
        let t0 = 1_791_900_000_000;
        claude_session(&store, "s1", t0);
        let start = |source: &str, now: i64| {
            send(
                &store,
                Host::Claude,
                json!({ "hook_event_name": "SessionStart", "session_id": "s2", "cwd": DIR, "source": source }),
                now,
            )
        };
        assert!(start("startup", t0 + 10_000).is_some());
        assert!(start("startup", t0 + 11_000).is_none(), "duplicate firing");
        assert!(start("compact", t0 + 20_000).is_some());
        assert!(start("compact", t0 + 21_000).is_none(), "duplicate compaction firing");
        assert!(start("compact", t0 + 200_000).is_some(), "a later compaction");
    }

    #[test]
    fn subagents_and_empty_projects_get_no_briefing() {
        let store = Store::open_in_memory().unwrap();
        let fresh = json!({ "hook_event_name": "SessionStart", "session_id": "s1", "cwd": DIR, "source": "startup" });
        assert!(send(&store, Host::Claude, fresh, 1).is_none());
        claude_session(&store, "s1", 10);
        let sub = json!({ "hook_event_name": "SessionStart", "session_id": "s2", "agent_id": "a1", "cwd": DIR, "source": "startup" });
        assert!(send(&store, Host::Codex, sub, 100).is_none());
    }

    #[test]
    fn skips_events_it_cannot_attribute() {
        let store = Store::open_in_memory().unwrap();
        for payload in [
            json!({ "hook_event_name": "UserPromptSubmit", "cwd": DIR, "prompt": "no session" }),
            json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1", "prompt": "no cwd" }),
            json!({ "session_id": "s1", "cwd": DIR }),
            json!({ "hook_event_name": "Notification", "session_id": "s1", "cwd": DIR }),
        ] {
            assert!(send(&store, Host::Codex, payload, 1).is_none());
        }
        assert!(events(&store).is_empty());
    }

    #[test]
    fn claude_project_dir_wins_over_cwd() {
        let store = Store::open_in_memory().unwrap();
        let env = HookEnv { project_dir: Some(DIR.into()), ..env(1) };
        let payload = json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1", "cwd": "C:/elsewhere/sub", "prompt": "hi" });
        handle(&store, Host::Claude, &payload, &env).unwrap();
        assert_eq!(events(&store)[0].project, project());
        handle(&store, Host::Codex, &payload, &env).unwrap();
        assert_ne!(events(&store)[0].project, project(), "codex uses its own cwd");
    }

    #[test]
    fn respects_capture_text_off() {
        let store = Store::open_in_memory().unwrap();
        let env = HookEnv { capture_text: false, ..env(1) };
        let prompt =
            json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1", "cwd": DIR, "prompt": "secret plans" });
        handle(&store, Host::Codex, &prompt, &env).unwrap();
        let stop = json!({ "hook_event_name": "Stop", "session_id": "s1", "cwd": DIR, "last_assistant_message": "reply", "stop_hook_active": false });
        handle(&store, Host::Codex, &stop, &env).unwrap();
        let all = events(&store);
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|e| e.summary.is_empty()));
    }

    #[test]
    fn keeps_every_prompt_in_a_codex_turn() {
        let store = Store::open_in_memory().unwrap();
        let prompt = |text: &str| json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1", "cwd": DIR, "turn_id": "turn-1", "prompt": text });
        send(&store, Host::Codex, prompt("keep going"), 1);
        send(&store, Host::Codex, prompt("stop, revert the schema change"), 2);
        send(&store, Host::Codex, prompt("stop, revert the schema change"), 3);
        send(&store, Host::Codex, prompt("keep going"), 4);
        let summaries: Vec<String> = events(&store).into_iter().map(|e| e.summary).collect();
        assert_eq!(summaries, ["keep going", "stop, revert the schema change", "keep going"]);
    }

    #[test]
    fn a_repeated_final_reply_after_a_continuation_is_kept() {
        let store = Store::open_in_memory().unwrap();
        let stop = |reply: &str, active: bool| {
            json!({ "hook_event_name": "Stop", "session_id": "s1", "cwd": DIR, "turn_id": "t1",
                    "last_assistant_message": reply, "stop_hook_active": active })
        };
        send(&store, Host::Codex, stop("All tasks complete.", false), 1);
        send(&store, Host::Codex, stop("Found a failing test in db.rs; fixing it now.", true), 2);
        send(&store, Host::Codex, stop("All tasks complete.", true), 3);
        assert_eq!(events(&store)[0].summary, "All tasks complete.");
        assert_eq!(events(&store).len(), 3);
    }

    #[test]
    fn the_final_reply_wins_when_another_hook_continues_the_turn() {
        let store = Store::open_in_memory().unwrap();
        let stop = |reply: &str, active: bool| {
            json!({ "hook_event_name": "Stop", "session_id": "s1", "cwd": DIR, "prompt_id": "p1",
                    "last_assistant_message": reply, "stop_hook_active": active })
        };
        send(
            &store,
            Host::Claude,
            json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1", "cwd": DIR, "prompt": "ship it" }),
            1,
        );
        send(&store, Host::Claude, stop("Done, all green.", false), 2);
        send(&store, Host::Claude, stop("Tests failed in parser.rs; stopped without shipping.", true), 3);
        let b = brief::build(
            &store,
            &BriefRequest { project: &project(), current_session: None, exclude_active: false, budget: 4000, now: 10 },
        )
        .unwrap()
        .unwrap();
        assert!(b.text.contains("Last reply: \"Tests failed in parser.rs; stopped without shipping.\""), "{}", b.text);
    }

    #[test]
    fn interrupted_turns_and_repeated_session_ends_are_recorded() {
        let store = Store::open_in_memory().unwrap();
        send(
            &store,
            Host::Codex,
            json!({ "hook_event_name": "Interrupt", "session_id": "s1", "cwd": DIR, "turn_id": "t1" }),
            1,
        );
        send(
            &store,
            Host::Codex,
            json!({ "hook_event_name": "SessionEnd", "session_id": "s1", "cwd": DIR, "reason": "other" }),
            2,
        );
        send(
            &store,
            Host::Codex,
            json!({ "hook_event_name": "SessionEnd", "session_id": "s1", "cwd": DIR, "reason": "other" }),
            3,
        );
        let all = events(&store);
        assert_eq!(all.iter().filter(|e| e.kind == "session_end").count(), 2);
        let interrupt = all.iter().find(|e| e.kind == "turn_end").unwrap();
        assert_eq!(interrupt.detail.as_deref(), Some("interrupted"));
    }

    #[test]
    fn long_secrets_in_prompts_are_redacted_before_being_cut() {
        let store = Store::open_in_memory().unwrap();
        let key: Vec<String> =
            (0..30).map(|i| format!("MIIEpAIBAAKCAQEAx3k9{i:04}LmNoPqRsTuVwXyZ0123456789ab")).collect();
        let prompt = format!(
            "please use this key: -----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----",
            key.join("\n")
        );
        send(
            &store,
            Host::Claude,
            json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1", "cwd": DIR, "prompt": prompt }),
            1,
        );
        let reply = format!("{} ghp_R4nd0mT0k3nV4lu3Abcdefghij0123456789", "x".repeat(380));
        send(
            &store,
            Host::Claude,
            json!({ "hook_event_name": "Stop", "session_id": "s1", "cwd": DIR, "last_assistant_message": reply }),
            2,
        );
        for e in events(&store) {
            assert!(!e.summary.contains("MIIEpAIB"), "{}", e.summary);
            assert!(!e.summary.contains("R4nd0m"), "{}", e.summary);
        }
    }

    #[test]
    fn codex_subagent_tool_calls_are_deduplicated_per_agent() {
        let store = Store::open_in_memory().unwrap();
        let call = |agent: &str| {
            json!({ "hook_event_name": "PostToolUse", "session_id": "root", "agent_id": agent, "cwd": DIR, "turn_id": "t",
                    "tool_name": "apply_patch", "tool_use_id": "call_1", "tool_input": { "command": "*** Update File: a.rs" }, "tool_response": "" })
        };
        send(&store, Host::Codex, call("a1"), 1);
        send(&store, Host::Codex, call("a1"), 2);
        send(&store, Host::Codex, call("a2"), 3);
        assert_eq!(events(&store).len(), 2);
    }

    #[test]
    fn output_never_steers_the_host() {
        let store = Store::open_in_memory().unwrap();
        claude_session(&store, "s1", 1_791_900_000_000);
        let out = send(
            &store,
            Host::Claude,
            json!({ "hook_event_name": "SessionStart", "session_id": "s9", "cwd": DIR }),
            1_791_900_100_000,
        )
        .unwrap();
        for key in ["\"decision\"", "\"permissionDecision\"", "\"continue\"", "\"systemMessage\"", "\"stopReason\""] {
            assert!(!out.contains(key), "{key} in {out}");
        }
    }

    #[test]
    fn parses_bom_and_rejects_non_objects() {
        assert!(parse_input(b"\xEF\xBB\xBF{\"a\":1}").is_some());
        assert!(parse_input(b"  {\"a\":1}\r\n").is_some());
        assert!(parse_input(b"[1,2]").is_none());
        assert!(parse_input(b"not json").is_none());
        assert!(parse_input(b"{\"a\":\"\xFF\"}").is_some());
    }
}
