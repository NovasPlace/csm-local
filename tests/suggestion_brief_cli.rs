//! App-generated suggestion sessions stay in the journal but out of every briefing entry point.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use csm_local::journal::NewEvent;
use csm_local::project;
use csm_local::store::Store;
use serde_json::{json, Value};

const SUGGESTION_PREFIX: &str =
    "This is an app-generated request for a suggested next user message. The user did not write this message.";
const REAL_REQUEST: &str = "Preserve the actual installer work in the briefing";
const REAL_REPLY: &str = "Verified the actual installer work";
const SUGGESTION_REPLY: &str = "Synthetic suggestion reply that must stay out of briefings";

struct Sandbox {
    root: PathBuf,
    temp: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let temp = fs::canonicalize(std::env::temp_dir()).unwrap();
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = temp.join(format!("csm-suggestion-brief-{}-{nonce}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::create_dir_all(root.join("repo/.git")).unwrap();
        fs::create_dir(root.join("home")).unwrap();
        Self { root, temp }
    }

    fn db(&self) -> PathBuf {
        self.root.join("csm.db")
    }

    fn csm(&self, args: &[&str], input: &str) -> String {
        let mut child = Command::new(env!("CARGO_BIN_EXE_csm"))
            .arg("--db")
            .arg(self.db())
            .args(args)
            .current_dir(self.root.join("repo"))
            .env("HOME", self.root.join("home"))
            .env("USERPROFILE", self.root.join("home"))
            .env("CSM_HOME", self.root.join("home"))
            .env_remove("CSM_DB")
            .env_remove("CLAUDE_PROJECT_DIR")
            .env_remove("CSM_CAPTURE_TEXT")
            .env_remove("CSM_BRIEF_BYTES")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        assert!(out.stderr.is_empty(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Ok(root) = fs::canonicalize(&self.root) {
            if root.parent() == Some(self.temp.as_path())
                && root.file_name().is_some_and(|name| name.to_string_lossy().starts_with("csm-suggestion-brief-"))
            {
                let _ = fs::remove_dir_all(root);
            }
        }
    }
}

#[test]
fn cli_mcp_and_hook_brief_real_work_without_deleting_suggestions() {
    let sb = Sandbox::new();
    let project = project::resolve(&sb.root.join("repo"));
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
    let store = Store::open(&sb.db()).unwrap();
    let seed = |session: &str, prompt: &str, reply: &str, ts: i64| {
        for (offset, (kind, summary)) in
            [("prompt", prompt), ("turn_end", reply), ("session_end", "exit")].into_iter().enumerate()
        {
            store
                .record_at(
                    NewEvent {
                        project: project.clone(),
                        host: "codex".into(),
                        session_id: Some(session.into()),
                        kind: kind.into(),
                        summary: summary.into(),
                        ..Default::default()
                    },
                    ts + offset as i64,
                )
                .unwrap();
        }
    };
    seed("real-work", REAL_REQUEST, REAL_REPLY, now - 7_200_000);
    // More than the previous-session lookback: filtering must happen before its limit.
    for i in 0..12 {
        seed(&format!("suggestion-{i}"), SUGGESTION_PREFIX, SUGGESTION_REPLY, now - 3_600_000 + i * 1_000);
    }
    drop(store);

    let cli = sb.csm(&["brief"], "");
    let requests = format!(
        "{}\n{}\n{}\n",
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "regression", "version": "1" }
        }}),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "csm_brief", "arguments": {} } })
    );
    let mcp = sb.csm(&["mcp"], &requests);
    let reply: Value = serde_json::from_str(mcp.lines().last().unwrap()).unwrap();
    assert_eq!(reply["id"], 2, "{reply}");
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let mcp_brief = reply["result"]["content"][0]["text"].as_str().unwrap();

    let hook = sb.csm(
        &["hook", "codex"],
        &json!({ "hook_event_name": "SessionStart", "session_id": "new-real-session", "cwd": project, "source": "startup" })
            .to_string(),
    );
    let hook_reply: Value = serde_json::from_str(&hook).unwrap();
    let hook_brief = hook_reply["hookSpecificOutput"]["additionalContext"].as_str().unwrap();

    let journal: Value = serde_json::from_str(&sb.csm(&["journal", "--json", "--limit", "100"], "")).unwrap();
    let events = journal.as_array().unwrap();
    assert_eq!(events.len(), 40, "39 seeded events plus the new SessionStart must remain");
    assert_eq!(events.iter().filter(|e| e["summary"] == SUGGESTION_PREFIX).count(), 12);
    assert_eq!(events.iter().filter(|e| e["summary"] == SUGGESTION_REPLY).count(), 12);

    for (entry, text) in [("CLI", cli.as_str()), ("MCP", mcp_brief), ("hook", hook_brief)] {
        assert!(text.contains(REAL_REQUEST) && text.contains(REAL_REPLY), "{entry} lost actual work: {text}");
        assert!(!text.contains(SUGGESTION_PREFIX), "{entry} included a suggestion prompt: {text}");
        assert!(!text.contains(SUGGESTION_REPLY), "{entry} included a suggestion reply: {text}");
    }
}
