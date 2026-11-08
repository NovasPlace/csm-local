//! The decision board and file claims through the real binary: hooks called
//! the way Claude Code and Codex call them, and a stdio MCP process.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("csm-board-it-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["home", "game/.git", "game/scripts"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        Sandbox { root }
    }

    fn path(&self, rel: &str) -> String {
        self.root.join(rel).display().to_string().replace('\\', "/")
    }

    fn csm(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_csm"))
            .args(args)
            .current_dir(self.root.join("game"))
            .env("USERPROFILE", self.root.join("home"))
            .env("HOME", self.root.join("home"))
            .env("CSM_CLAUDE_CLI", "none")
            .env_remove("CSM_HOME")
            .env_remove("CSM_DB")
            .env_remove("CLAUDE_PROJECT_DIR")
            .env_remove("CSM_CAPTURE_TEXT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    }

    fn text(&self, args: &[&str]) -> String {
        let out = self.csm(args, "");
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    /// A hook call with messaging on; returns the context it gives the agent.
    fn hook(&self, host: &str, payload: Value) -> String {
        let out = self.csm(&["hook", host, "--messaging"], &payload.to_string());
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let stdout = String::from_utf8(out.stdout).unwrap();
        if stdout.is_empty() {
            return stdout;
        }
        let reply: Value = serde_json::from_str(&stdout).unwrap_or_else(|_| panic!("not JSON: {stdout}"));
        assert!(!stdout.contains("permissionDecision"), "{stdout}");
        reply["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or_default().to_string()
    }

    fn event(&self, name: &str, session: &str, extra: Value) -> Value {
        let mut payload = json!({ "hook_event_name": name, "session_id": session, "cwd": self.path("game") });
        payload.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        payload
    }

    /// One `csm mcp` process: initialize, then each tool call in order.
    fn mcp(&self, calls: &[(&str, Value)]) -> Vec<Value> {
        let mut wire = vec![json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {}}).to_string()];
        for (i, (name, args)) in calls.iter().enumerate() {
            wire.push(
                json!({"jsonrpc": "2.0", "id": i + 1, "method": "tools/call", "params": {"name": name, "arguments": args}})
                    .to_string(),
            );
        }
        let out = self.csm(&["mcp"], &(wire.join("\n") + "\n"));
        let replies: Vec<Value> =
            String::from_utf8(out.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        replies[1..]
            .iter()
            .map(|r| {
                assert_eq!(r["result"]["isError"], false, "{r}");
                let text = r["result"]["content"][0]["text"].as_str().unwrap();
                serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
            })
            .collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn binding_token(context: &str) -> String {
    let data: Value = serde_json::from_str(context.rsplit_once('\n').unwrap().1).unwrap();
    data["binding_token"].as_str().unwrap().to_string()
}

fn patch(file: &str) -> Value {
    json!({ "tool_name": "apply_patch", "tool_input": { "command": format!("*** Begin Patch\n*** Update File: {file}\n@@\n-a\n+b\n*** End Patch") } })
}

#[test]
fn a_decision_told_to_one_session_reaches_the_others_and_claims_show_up() {
    let sb = Sandbox::new("flow");
    // Codex is already working; then the user opens a Claude session.
    sb.hook("codex", sb.event("SessionStart", "x1", json!({ "source": "startup" })));
    sb.hook(
        "codex",
        sb.event("UserPromptSubmit", "x1", json!({ "turn_id": "t1", "prompt": "find out why saves change" })),
    );
    let intro = sb.hook("claude", sb.event("SessionStart", "c1", json!({ "source": "startup" })));
    let token = binding_token(&intro);

    // The user tells Claude; Claude records it and claims the file it is changing.
    let replies = sb.mcp(&[
        (
            "csm_connect",
            json!({ "host": "claude", "session_id": "c1", "binding_token": token, "label": "ground rework" }),
        ),
        (
            "csm_decide",
            json!({ "decision": "Saves don't matter: the real-game runs that rewrite them are deliberate." }),
        ),
        (
            "csm_claim",
            json!({ "files": [sb.path("game/scripts/ground_field.gd")], "minutes": 30, "note": "collision rework" }),
        ),
        ("csm_peers", json!({})),
    ]);
    assert!(replies[1].as_str().unwrap().starts_with("Recorded standing decision #1."), "{}", replies[1]);
    assert_eq!(replies[2]["claimed"][0]["file"], "scripts/ground_field.gd");
    let peers = replies[3]["peers"].as_array().unwrap();
    let claude = peers.iter().find(|p| p["id"] == "claude:c1").unwrap();
    assert_eq!(claude["claims"][0]["note"], "collision rework");

    // Codex hears about the decision on its very next tool call, once.
    let tool = |id: &str, file: &str| {
        let mut extra = patch(file);
        extra["turn_id"] = json!("t1");
        extra["tool_use_id"] = json!(id);
        extra["tool_response"] = json!("Success");
        extra
    };
    let told = sb.hook("codex", sb.event("PostToolUse", "x1", tool("call_1", "scripts/hud.gd")));
    assert_eq!(
        told,
        "csm: Standing decisions the user made for this project: #1 \"Saves don't matter: the real-game runs that rewrite them are deliberate.\" (just now, recorded via claude c1). csm_decisions lists the board."
    );
    assert!(sb.hook("codex", sb.event("PostToolUse", "x1", tool("call_2", "scripts/hud.gd"))).is_empty());
    assert!(
        sb.hook("claude", sb.event("UserPromptSubmit", "c1", json!({ "prompt": "go on" }))).is_empty(),
        "own decision"
    );

    // Before Codex edits the claimed file, it is told who has it.
    let mut pre = patch("scripts/ground_field.gd");
    pre["tool_use_id"] = json!("call_3");
    let warning = sb.hook("codex", sb.event("PreToolUse", "x1", pre.clone()));
    assert!(
        warning.starts_with("csm: scripts/ground_field.gd is claimed by claude session c1 (working on \"ground rework\") for 30 more min (until "),
        "{warning}"
    );
    assert!(warning.ends_with("UTC): \"collision rework\". csm_send reaches that session at claude:c1."), "{warning}");
    assert!(sb.hook("codex", sb.event("PreToolUse", "x1", pre)).is_empty(), "warned once per claim");

    // A session that starts now reads both in its briefing and is not told again.
    let briefing = sb.hook("codex", sb.event("SessionStart", "x2", json!({ "source": "startup" })));
    assert!(
        briefing.contains("Standing decisions the user made for this project:\n  #1 Saves don't matter"),
        "{briefing}"
    );
    assert!(briefing.contains("Files claimed by running sessions (csm_peers shows how to reach them):\n  scripts/ground_field.gd by claude c1 for 30 more min"), "{briefing}");
    assert!(sb
        .hook("codex", sb.event("UserPromptSubmit", "x2", json!({ "turn_id": "t9", "prompt": "hi" })))
        .is_empty());

    // The claim ends with the session that held it.
    sb.hook("claude", sb.event("SessionEnd", "c1", json!({ "reason": "prompt_input_exit" })));
    let listing: Value = serde_json::from_str(&sb.text(&["peers", "--include-ended"])).unwrap();
    let ended = listing["peers"].as_array().unwrap().iter().find(|p| p["state"] == "ended").expect("c1 is listed");
    assert_eq!(ended["claims"], json!([]), "{listing}");
}

#[test]
fn the_cli_records_withdraws_exports_and_forgets_decisions() {
    let sb = Sandbox::new("cli");
    assert!(sb.text(&["decide", "AC isn't the default"]).starts_with("Recorded standing decision #1."));
    assert!(sb.text(&["decide", "ac isn't the  default"]).starts_with("Already on the board as #1"));
    assert!(sb
        .text(&["decide", "AC is on for hard mode only", "--withdraw", "1"])
        .starts_with("Recorded standing decision #2, replacing #1."));
    let board = sb.text(&["decisions"]);
    assert!(board.starts_with("#2 AC is on for hard mode only (just now, recorded via cli)"), "{board}");
    assert!(!board.contains("#1"), "{board}");
    assert!(!sb.csm(&["decide", "--withdraw", "1"], "").status.success(), "already replaced");
    assert!(sb.text(&["status"]).contains("standing decisions in this project: 1"));

    let export: Value = serde_json::from_str(&sb.text(&["export"])).unwrap();
    assert_eq!(export["decisions"].as_array().unwrap().len(), 2, "withdrawals are history too");

    let game = sb.path("game");
    assert!(sb.text(&["forget", "--project", &game]).contains("Would also delete 2 decision board entries."));
    assert!(sb.text(&["forget", "--project", &game, "--yes"]).contains("Also deleted 2 decision board entries."));
    assert!(sb.text(&["decisions"]).contains("No standing decisions"));
}

#[test]
fn claims_through_the_cli_report_files_another_session_holds() {
    let sb = Sandbox::new("claims");
    sb.hook("codex", sb.event("SessionStart", "x1", json!({ "source": "startup" })));
    sb.hook("claude", sb.event("SessionStart", "c1", json!({ "source": "startup" })));
    let first: Value =
        serde_json::from_str(&sb.text(&["claim", "--peer", "codex:x1", "scripts/a.gd", "--minutes", "10"])).unwrap();
    assert_eq!(first["claimed"][0]["minutes_left"], 10);
    let second: Value =
        serde_json::from_str(&sb.text(&["claim", "--peer", "claude:c1", "scripts/a.gd", "scripts/b.gd"])).unwrap();
    assert_eq!(second["held_by_others"][0]["peer"], "codex:x1");
    assert_eq!(second["claimed"][0]["file"], "scripts/b.gd");
    let released: Value = serde_json::from_str(&sb.text(&["release", "--peer", "codex:x1"])).unwrap();
    assert_eq!(released["released"], 1);
    assert!(!sb.csm(&["claim", "--peer", "codex:nobody", "scripts/a.gd"], "").status.success());
}
