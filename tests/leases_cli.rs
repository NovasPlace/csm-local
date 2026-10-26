//! Collision warnings and project links through the real binary, called the
//! way Claude Code and Codex call it.

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
        let root = std::env::temp_dir().join(format!("csm-leases-it-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["home/.claude", "home/.codex", "game/.git", "game/scripts", "launcher"] {
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
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    }

    fn hook(&self, host: &str, payload: Value) -> String {
        let out = self.csm(&["hook", host], &payload.to_string());
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn an_edit_by_one_agent_warns_the_other_before_it_edits_the_same_file() {
    let sb = Sandbox::new("warn");
    let game = sb.path("game");
    let file = format!("{game}/scripts/main.gd");
    sb.hook(
        "codex",
        json!({ "hook_event_name": "UserPromptSubmit", "session_id": "x1", "cwd": game,
                             "turn_id": "t1", "prompt": "add the carrier deck" }),
    );
    let patch = "*** Begin Patch\n*** Update File: scripts/main.gd\n@@\n-a\n+b\n*** End Patch";
    let quiet = sb.hook(
        "codex",
        json!({ "hook_event_name": "PostToolUse", "session_id": "x1", "cwd": game, "turn_id": "t1",
                                         "tool_name": "apply_patch", "tool_use_id": "call_1",
                                         "tool_input": { "command": patch }, "tool_response": "Success" }),
    );
    assert!(quiet.is_empty(), "{quiet}");

    let pre = json!({ "hook_event_name": "PreToolUse", "session_id": "c1", "cwd": game, "tool_name": "Edit",
                      "tool_use_id": "toolu_1", "tool_input": { "file_path": file, "old_string": "a", "new_string": "b" } });
    let out = sb.hook("claude", pre.clone());
    let reply: Value = serde_json::from_str(&out).unwrap_or_else(|_| panic!("not JSON: {out}"));
    assert_eq!(reply["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    let text = reply["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(text.contains("scripts/main.gd was edited just now by another session that is still running (via codex in game, last request: \"add the carrier deck\")"), "{text}");
    assert!(!out.contains("permissionDecision"), "{out}");
    assert!(sb.hook("claude", pre).is_empty(), "warned once per window");
}

#[test]
fn link_moves_a_launch_folder_into_the_game_project() {
    let sb = Sandbox::new("link");
    let (game, launcher) = (sb.path("game"), sb.path("launcher"));
    sb.hook(
        "claude",
        json!({ "hook_event_name": "UserPromptSubmit", "session_id": "c1", "cwd": launcher, "prompt": "fix bays" }),
    );
    sb.hook("claude", json!({ "hook_event_name": "PostToolUse", "session_id": "c1", "cwd": launcher, "tool_name": "Write",
                              "tool_use_id": "w1", "tool_input": { "file_path": format!("{game}/scripts/bay.gd") }, "tool_response": {} }));

    let listing = String::from_utf8(sb.csm(&["link"], "").stdout).unwrap();
    assert!(listing.contains(&format!("csm link \"{launcher}\" --into \"{game}\"")), "{listing}");
    let preview = String::from_utf8(sb.csm(&["link", &launcher, "--into", &game], "").stdout).unwrap();
    assert!(preview.contains("Would link") && preview.contains("2 journal events"), "{preview}");
    let linked = sb.csm(&["link", &launcher, "--into", &game, "--yes"], "");
    assert!(linked.status.success(), "{linked:?}");

    let journal = String::from_utf8(sb.csm(&["journal", "--json"], "").stdout).unwrap();
    let events: Value = serde_json::from_str(&journal).unwrap();
    let summaries: Vec<&str> = events.as_array().unwrap().iter().map(|e| e["summary"].as_str().unwrap()).collect();
    assert!(summaries.contains(&"Write scripts/bay.gd"), "paths rebased into the game: {summaries:?}");

    // New sessions in the launcher land in the game project.
    sb.hook(
        "claude",
        json!({ "hook_event_name": "UserPromptSubmit", "session_id": "c2", "cwd": launcher, "prompt": "next" }),
    );
    let journal = String::from_utf8(sb.csm(&["journal", "--json"], "").stdout).unwrap();
    assert!(journal.contains("\"next\""), "{journal}");
}

#[test]
fn setup_registers_the_collision_hook_for_both_hosts() {
    let sb = Sandbox::new("setup");
    let out = sb.csm(&["setup", "--yes"], "");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    let read = |rel: &str| -> Value { serde_json::from_str(&fs::read_to_string(sb.root.join(rel)).unwrap()).unwrap() };
    assert_eq!(
        read("home/.claude/settings.json")["hooks"]["PreToolUse"][0]["matcher"],
        "Edit|MultiEdit|Write|NotebookEdit"
    );
    assert_eq!(read("home/.codex/hooks.json")["hooks"]["PreToolUse"][0]["matcher"], "^apply_patch$");
    // A subagent's stop and start pause and bring back its file claims; every subagent type counts.
    for event in ["SubagentStop", "SubagentStart"] {
        let group = &read("home/.claude/settings.json")["hooks"][event][0];
        assert!(group.get("matcher").is_none(), "{event}: {group}");
        assert!(group["hooks"][0]["command"].as_str().unwrap().contains(" hook claude"), "{event}: {group}");
        assert!(read("home/.codex/hooks.json")["hooks"].get(event).is_none(), "{event}");
    }
    let status = String::from_utf8(sb.csm(&["setup", "--status"], "").stdout).unwrap();
    assert!(status.contains("Claude Code: hooks 9/9") && status.contains("Codex: hooks 6/6"), "{status}");
}
