//! CLI messaging against disposable databases, never the user's CSM database.
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use csm_local::store::Store;
use serde_json::Value;

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture {
    root: PathBuf,
    db: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "csm-messaging-cli-{}-{}-{}",
            std::process::id(),
            now(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("csm.db");
        let store = Store::open(&db).unwrap();
        for (host, session, project) in
            [("codex", "a", "C:/work/game"), ("claude", "b", "C:/work/game"), ("claude", "c", "C:/work/other")]
        {
            store.register_peer(project, host, session, None, "active", now()).unwrap();
        }
        Self { root, db }
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_csm"));
        cmd.arg("--db").arg(&self.db).current_dir(&self.root);
        cmd
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        serde_json::from_slice(&output.stdout).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

#[test]
fn explicit_cli_send_read_retry_ack_and_reply() {
    let f = Fixture::new();
    let peers = f.ok(&["peers", "--all-projects"]);
    assert_eq!(peers["peers"].as_array().unwrap().len(), 3);
    let args = [
        "send",
        "--from",
        "codex:a",
        "--to",
        "claude:b",
        "--request-id",
        "request-1",
        "Inspect the gait at gameplay distance",
    ];
    let sent = f.ok(&args);
    assert_eq!(sent["status"], "pending");
    let id = sent["message"]["id"].as_i64().unwrap().to_string();
    assert_eq!(f.ok(&args)["message"]["id"], sent["message"]["id"]);
    for _ in 0..2 {
        let inbox = f.ok(&["inbox", "--peer", "claude:b"]);
        assert_eq!(inbox["messages"].as_array().unwrap().len(), 1);
        assert!(inbox["messages"][0]["delivered_at"].is_null());
        assert!(inbox["messages"][0]["acknowledged_at"].is_null());
        assert!(inbox["coordination_notice"].as_str().unwrap().contains("not human permission"));
    }
    assert_eq!(f.ok(&["ack", "--peer", "claude:b", &id])["acknowledged"], 1);
    assert_eq!(f.ok(&["ack", "--peer", "claude:b", &id])["acknowledged"], 0);
    assert_eq!(f.ok(&["message-status", "--peer", "codex:a", &id])["status"], "acknowledged");
    assert_eq!(f.ok(&["inbox", "--peer", "claude:b"])["messages"].as_array().unwrap().len(), 0);
    let reply = f.ok(&[
        "send",
        "--from",
        "claude:b",
        "--to",
        "codex:a",
        "--request-id",
        "reply-1",
        "--reply-to",
        &id,
        "The full side cycle is ready",
    ]);
    assert_eq!(reply["message"]["reply_to"].as_i64().unwrap().to_string(), id);
}

#[test]
fn cli_rejects_unknown_identity_cross_project_and_unauthorized_status() {
    let f = Fixture::new();
    let unknown = f.run(&["inbox", "--peer", "claude:unknown"]);
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("hook-registered"));
    let blocked = f.run(&["send", "--from", "codex:a", "--to", "claude:c", "--request-id", "cross", "hello"]);
    assert!(!blocked.status.success());
    let sent = f.ok(&[
        "send",
        "--from",
        "codex:a",
        "--to",
        "claude:c",
        "--allow-cross-project",
        "--request-id",
        "cross",
        "hello",
    ]);
    let id = sent["message"]["id"].as_i64().unwrap().to_string();
    assert!(!f.run(&["message-status", "--peer", "claude:b", &id]).status.success());
    assert!(!f.run(&["ack", "--peer", "claude:b", &id]).status.success());
    assert!(!f.run(&["inbox", "--peer", "claude:b", "--limit", "0"]).status.success());
    assert!(!f.run(&["wait", "--peer", "claude:b", "--timeout-seconds", "26"]).status.success());
}

#[test]
fn cli_wait_accepts_concurrent_send_without_locking_or_acknowledging() {
    let f = Fixture::new();
    let mut wait = f.command();
    wait.args(["wait", "--peer", "claude:b", "--timeout-seconds", "3"]);
    let waiting = std::thread::spawn(move || wait.output().unwrap());
    std::thread::sleep(Duration::from_millis(200));
    let sent = f.ok(&[
        "send",
        "--from",
        "codex:a",
        "--to",
        "claude:b",
        "--request-id",
        "during-wait",
        "Arrived while waiting",
    ]);
    let received = waiting.join().unwrap();
    assert!(received.status.success(), "{}", String::from_utf8_lossy(&received.stderr));
    let received: Value = serde_json::from_slice(&received.stdout).unwrap();
    assert_eq!(received["messages"][0]["id"], sent["message"]["id"]);
    assert!(received["messages"][0]["acknowledged_at"].is_null());
    assert!(received["messages"][0]["delivered_at"].is_null());
}

#[test]
fn mailbox_only_forget_preview_counts_messages_and_peers_before_cleanup() {
    let f = Fixture::new();
    let internal =
        f.ok(&["send", "--from", "codex:a", "--to", "claude:b", "--request-id", "internal", "Local coordination"]);
    let cross = f.ok(&[
        "send",
        "--from",
        "claude:c",
        "--to",
        "codex:a",
        "--allow-cross-project",
        "--request-id",
        "cross",
        "Cross-project coordination",
    ]);
    let preview = f.run(&["forget", "--project", "C:/work/game"]);
    assert!(preview.status.success(), "{}", String::from_utf8_lossy(&preview.stderr));
    let text = String::from_utf8_lossy(&preview.stdout);
    assert!(text.contains("Would permanently delete 0 memories and 0 journal events"), "{text}");
    assert!(text.contains("Would also delete 2 messages and 2 messaging peers."), "{text}");
    assert!(!text.contains("Nothing is stored"), "{text}");
    let store = Store::open(&f.db).unwrap();
    assert_eq!(store.mailbox_project_counts("C:/work/game").unwrap(), (2, 2));
    assert!(store.message_status("codex:a", internal["message"]["id"].as_i64().unwrap()).unwrap().is_some());
    assert!(store.message_status("claude:c", cross["message"]["id"].as_i64().unwrap()).unwrap().is_some());
    drop(store);
    let removed = f.run(&["forget", "--project", "C:/work/game", "--yes"]);
    assert!(removed.status.success(), "{}", String::from_utf8_lossy(&removed.stderr));
    let text = String::from_utf8_lossy(&removed.stdout);
    assert!(text.contains("Permanently deleted 0 memories and 0 journal events"), "{text}");
    assert!(text.contains("Also deleted 2 messages and 2 messaging peers."), "{text}");
    assert!(!text.contains("Nothing is stored"), "{text}");
    let store = Store::open(&f.db).unwrap();
    assert_eq!(store.mailbox_project_counts("C:/work/game").unwrap(), (0, 0));
    assert_eq!(store.mailbox_project_counts("C:/work/other").unwrap(), (0, 1));
    assert!(store.peer("claude:c").unwrap().is_some());
    assert!(store.message_status("claude:c", cross["message"]["id"].as_i64().unwrap()).unwrap().is_none());
}
