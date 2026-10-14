//! Runs the real `csm` binary the way Claude Code and Codex do: JSON on
//! stdin, cwd set to the user's repository.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("csm-it-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("repo").join(".git")).unwrap();
        fs::create_dir_all(root.join("repo").join("src")).unwrap();
        fs::write(root.join("repo").join("src").join("main.rs"), "fn main() {}\n").unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        Sandbox { root }
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn csm(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_csm"))
            .args(args)
            .current_dir(self.repo())
            .env("CSM_HOME", self.home())
            .env_remove("CSM_DB")
            .env_remove("CLAUDE_PROJECT_DIR")
            .env_remove("CSM_CAPTURE_TEXT")
            .env_remove("CSM_BRIEF_BYTES")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    }

    fn hook(&self, host: &str, payload: serde_json::Value) -> Output {
        self.csm(&["hook", host], &payload.to_string())
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn tree(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        out.push(path.display().to_string());
        if path.is_dir() {
            out.extend(tree(&path));
        }
    }
    out.sort();
    out
}

#[test]
fn hooks_record_and_brief_without_touching_the_repo() {
    let sb = Sandbox::new("roundtrip");
    let before = tree(&sb.repo());
    let cwd = sb.repo().display().to_string();
    let main_rs = sb.repo().join("src").join("main.rs").display().to_string();

    for payload in [
        serde_json::json!({ "hook_event_name": "SessionStart", "session_id": "c1", "cwd": cwd, "source": "startup" }),
        serde_json::json!({ "hook_event_name": "UserPromptSubmit", "session_id": "c1", "cwd": cwd, "prompt": "fix main" }),
        serde_json::json!({ "hook_event_name": "PostToolUse", "session_id": "c1", "cwd": cwd, "tool_name": "Edit",
                            "tool_use_id": "toolu_1", "tool_input": { "file_path": main_rs }, "tool_response": {} }),
        serde_json::json!({ "hook_event_name": "SessionEnd", "session_id": "c1", "cwd": cwd, "reason": "exit" }),
    ] {
        let out = sb.hook("claude", payload);
        assert!(out.status.success(), "{out:?}");
        assert!(out.stdout.is_empty(), "only SessionStart may print: {out:?}");
    }

    let out = sb.hook(
        "codex",
        serde_json::json!({ "hook_event_name": "SessionStart", "session_id": "x1", "cwd": cwd, "source": "startup",
                            "model": "m", "permission_mode": "default", "transcript_path": null }),
    );
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let context = parsed["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(context.contains("Last request: \"fix main\""), "{context}");
    assert!(context.contains("Files changed: src/main.rs"), "{context}");

    assert_eq!(tree(&sb.repo()), before, "hooks must not write into the repository");
    assert!(sb.home().join("csm.db").exists());

    let brief = sb.csm(&["brief"], "");
    assert!(brief.status.success());
    assert!(String::from_utf8_lossy(&brief.stdout).contains("Files changed: src/main.rs"));
}

#[test]
fn hook_failures_exit_zero_silently() {
    let sb = Sandbox::new("failures");
    let cases: [(&[&str], &str); 5] = [
        (&["hook"], "{}"),
        (&["hook", "cursor"], "{}"),
        (&["hook", "claude"], "not json at all"),
        (&["hook", "codex"], ""),
        (&["hook", "claude", "--unexpected", "extra"], "[1,2,3]"),
    ];
    for (args, stdin) in cases {
        let out = sb.csm(args, stdin);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "{args:?}: {out:?}");
        assert!(out.stderr.is_empty(), "{args:?}: {out:?}");
    }
    let log = fs::read_to_string(sb.home().join("hook-errors.log")).unwrap();
    assert!(log.contains("unknown hook host"), "{log}");
    assert!(!log.contains("not json at all"), "payload content leaked into the log: {log}");
}

#[test]
fn relative_csm_home_is_refused_without_writing_into_cwd() {
    let sb = Sandbox::new("relative");
    let before = tree(&sb.repo());
    let out = Command::new(env!("CARGO_BIN_EXE_csm"))
        .args(["hook", "claude"])
        .current_dir(sb.repo())
        .env("CSM_HOME", "relative-dir")
        .env_remove("CSM_DB")
        .env_remove("CLAUDE_PROJECT_DIR")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    assert_eq!(tree(&sb.repo()), before);
}

#[test]
fn parallel_hook_processes_on_a_fresh_database_lose_nothing() {
    let sb = Sandbox::new("parallel");
    let cwd = sb.repo().display().to_string();
    let children: Vec<_> = (0..12)
        .map(|i| {
            let payload = serde_json::json!({
                "hook_event_name": "PostToolUse", "session_id": "p1", "cwd": cwd, "tool_name": "Bash",
                "tool_use_id": format!("toolu_{i}"), "tool_input": { "command": format!("touch out-{i}.txt") }, "tool_response": {}
            });
            let mut child = Command::new(env!("CARGO_BIN_EXE_csm"))
                .args(["hook", "claude"])
                .current_dir(sb.repo())
                .env("CSM_HOME", sb.home())
                .env_remove("CSM_DB")
                .env_remove("CLAUDE_PROJECT_DIR")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(payload.to_string().as_bytes()).unwrap();
            child
        })
        .collect();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(0));
    }
    let journal = sb.csm(&["journal", "--json", "--limit", "100"], "");
    let events: serde_json::Value = serde_json::from_slice(&journal.stdout).unwrap();
    assert_eq!(events.as_array().unwrap().len(), 12, "{events}");
    assert!(!sb.home().join("hook-errors.log").exists(), "{:?}", fs::read_to_string(sb.home().join("hook-errors.log")));
}

#[test]
fn db_flag_works_before_or_after_hook_and_never_exits_two() {
    let sb = Sandbox::new("dbflag");
    let cwd = sb.repo().display().to_string();
    let prompt =
        serde_json::json!({ "hook_event_name": "UserPromptSubmit", "session_id": "d1", "cwd": cwd, "prompt": "hi" })
            .to_string();
    let custom = sb.root.join("custom.db");
    let custom_arg = custom.display().to_string();
    let eq_arg = format!("--db={}", sb.root.join("eq.db").display());
    let after = sb.root.join("after.db").display().to_string();
    let cases: Vec<Vec<&str>> = vec![
        vec!["--db", &custom_arg, "hook", "claude"],
        vec![&eq_arg, "hook", "codex"],
        vec!["hook", "claude", "--db", &after],
        vec!["--db", &custom_arg, "hook"],
        vec!["--db", "relative.db", "hook", "claude"],
        vec!["--db", "hook", "claude"],
        vec!["--db", "C:/My", "Docs/x.db", "hook", "claude"],
        vec!["--db", &custom_arg, "--db", &custom_arg, "hook", "claude"],
    ];
    for args in cases {
        let out = sb.csm(&args, &prompt);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{args:?}: {out:?}");
    }
    for db in ["custom.db", "eq.db", "after.db"] {
        assert!(sb.root.join(db).exists(), "{db} not created");
    }
    assert!(!sb.repo().join("relative.db").exists(), "relative --db wrote into the repo");
    assert!(!sb.home().join("csm.db").exists(), "default database used despite --db");

    for args in [
        vec!["--", "hook", "claude"],
        vec!["-h", "hook", "claude"],
        vec!["--version", "hook", "claude"],
        vec!["hook", "claude", "--help"],
    ] {
        let out = sb.csm(&args, &prompt);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{args:?}: {out:?}");
    }
}

#[test]
fn forget_finds_a_deleted_repo_typed_in_a_different_case() {
    let sb = Sandbox::new("forget");
    assert!(sb.csm(&["save", "remember the deploy hostname"], "").status.success());
    let repo = sb.repo();
    fs::remove_dir_all(&repo).unwrap();
    let typed = repo.display().to_string().to_lowercase();
    let out = Command::new(env!("CARGO_BIN_EXE_csm"))
        .args(["forget", "--project", &typed, "--yes"])
        .current_dir(&sb.root)
        .env("CSM_HOME", sb.home())
        .env_remove("CSM_DB")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    if cfg!(windows) {
        assert!(stdout.contains("Permanently deleted 1 memories"), "{stdout}");
    }
}

#[test]
fn forget_never_falls_back_to_a_parent_project() {
    let sb = Sandbox::new("forget-parent");
    let parent = sb.root.display().to_string();
    assert!(sb.csm(&["save", "parent project note", "--project", &parent], "").status.success());
    let gone = sb.root.join("deleted-child").display().to_string();
    let out = Command::new(env!("CARGO_BIN_EXE_csm"))
        .args(["forget", "--project", &gone, "--yes"])
        .current_dir(&sb.root)
        .env("CSM_HOME", sb.home())
        .env_remove("CSM_DB")
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("related stored projects"), "{out:?}");
    let list = sb.csm(&["list", "--project", &parent], "");
    assert!(String::from_utf8_lossy(&list.stdout).contains("parent project note"), "the parent's data was deleted");
}

#[test]
fn unknown_cli_commands_still_fail_normally() {
    let sb = Sandbox::new("cli");
    let out = sb.csm(&["frobnicate"], "");
    assert!(!out.status.success());
}
