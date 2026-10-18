use csm_local::messaging::NewMessage;
use csm_local::store::Store;
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}
#[test]
fn independent_background_hook_process_only_signals_wake_for_real_addressed_mail() {
    let root = std::env::temp_dir().join(format!("csm-wake-hook-{}-{}", std::process::id(), now()));
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("mail.db");
    let s = Store::open(&db).unwrap();
    s.register_peer("C:/qa", "codex", "sender", None, "active", now()).unwrap();
    s.register_peer("C:/qa", "claude", "receiver", None, "idle", now()).unwrap();
    s.set_wake_enabled(true).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_csm"))
        .arg("--db")
        .arg(&db)
        .args(["wake-hook", "claude", "--timeout-seconds", "5"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(json!({"session_id":"receiver","hook_event_name":"Stop"}).to_string().as_bytes())
        .unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let m = s
        .send_message(NewMessage {
            from_peer: "codex:sender".into(),
            to_peer: "claude:receiver".into(),
            request_id: "wake-once".into(),
            content: "SENSITIVE_UNTRUSTED_BODY".into(),
            reply_to: None,
            allow_cross_project: false,
            now: now(),
            ttl_seconds: 86400,
        })
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let reminder = String::from_utf8(output.stderr).unwrap();
    assert!(reminder.contains("csm_inbox"));
    assert!(!reminder.contains("SENSITIVE_UNTRUSTED_BODY"));
    let state = s.message_status("claude:receiver", m.id).unwrap().unwrap();
    assert!(state.delivered_at.is_some());
    assert!(state.acknowledged_at.is_none());
    let output = Command::new(env!("CARGO_BIN_EXE_csm"))
        .arg("--db")
        .arg(&db)
        .args(["wake-hook", "claude", "--timeout-seconds", "1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|mut c| {
            c.stdin.take().unwrap().write_all(json!({"session_id":"receiver"}).to_string().as_bytes()).unwrap();
            c.wait_with_output().unwrap()
        })
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    drop(s);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn malformed_unknown_and_subagent_wake_hooks_never_exit_two() {
    for (host, input) in
        [("codex", json!({})), ("claude", json!({})), ("claude", json!({"agent_id":"sub","session_id":"x"}))]
    {
        let mut child = Command::new(env!("CARGO_BIN_EXE_csm"))
            .args(["wake-hook", host, "--db", "relative.db", "--timeout-seconds", "1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(0));
        assert!(out.stdout.is_empty());
        assert!(out.stderr.is_empty());
    }
}

#[test]
fn wake_status_never_contains_private_routes_or_binding_tokens() {
    let s = Store::open_in_memory().unwrap();
    s.register_peer("C:/qa", "codex", "receiver", None, "idle", now()).unwrap();
    let pipe = if cfg!(windows) { r"\\.\pipe\csm-private-test" } else { "/tmp/csm-private-test" };
    s.register_codex_route("codex:receiver", pipe, now()).unwrap();
    let value: Value = s.wake_status().unwrap();
    let text = value.to_string();
    assert!(!text.contains(pipe));
    assert!(!text.contains(&s.peer_binding_token("codex:receiver").unwrap()));
    assert!(s.register_codex_route("codex:receiver", "https://remote.example", now()).is_err());
}
