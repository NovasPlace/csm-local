//! MCP boundary tests, including independent live stdio processes on a temporary DB.
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use csm_local::mcp::Server;
use csm_local::messaging::NewMessage;
use csm_local::store::Store;
use serde_json::{json, Value};

const PROJECT: &str = "C:/work/game";
fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}
fn seed(store: &Store) {
    store.register_peer(PROJECT, "codex", "a", None, "active", now()).unwrap();
    store.register_peer(PROJECT, "claude", "b", None, "idle", now()).unwrap();
    store.register_peer("C:/work/other", "claude", "c", None, "active", now()).unwrap();
}
fn server() -> (Server, String, String) {
    let store = Store::open_in_memory().unwrap();
    seed(&store);
    let codex_token = store.peer_binding_token("codex:a").unwrap();
    let claude_token = store.peer_binding_token("claude:b").unwrap();
    (Server::new(store, PROJECT.into()), codex_token, claude_token)
}
fn tool(server: &mut Server, name: &str, arguments: Value) -> (String, bool) {
    let response = server
        .handle(&json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":name,"arguments":arguments}}))
        .unwrap();
    (
        response["result"]["content"][0]["text"].as_str().unwrap().into(),
        response["result"]["isError"].as_bool().unwrap(),
    )
}
fn ok(server: &mut Server, name: &str, arguments: Value) -> Value {
    let (text, error) = tool(server, name, arguments);
    assert!(!error, "{name}: {text}");
    serde_json::from_str(&text).unwrap()
}

#[test]
fn connection_requires_existing_exact_identity_and_cannot_switch() {
    let (mut s, codex_token, claude_token) = server();
    assert_eq!(ok(&mut s, "csm_peers", json!({}))["peers"].as_array().unwrap().len(), 2);
    for name in ["csm_send", "csm_inbox", "csm_ack", "csm_message_status", "csm_wait"] {
        let (text, error) = tool(&mut s, name, json!({}));
        assert!(error && text.contains("csm_connect"), "{name}: {text}");
    }
    let (text, error) =
        tool(&mut s, "csm_connect", json!({"host":"codex","session_id":"unknown","binding_token":"unknown"}));
    assert!(error && text.contains("hook-registered"));
    let (text, error) = tool(&mut s, "csm_connect", json!({"host":"claude","session_id":"b"}));
    assert!(error && text.contains("binding_token"));
    let (text, error) =
        tool(&mut s, "csm_connect", json!({"host":"claude","session_id":"b","binding_token":codex_token}));
    assert!(error && text.contains("Invalid session binding token"));
    let identity = ok(
        &mut s,
        "csm_connect",
        json!({"host":"codex","session_id":"a","binding_token":codex_token,"label":"gait inspection"}),
    );
    assert_eq!(identity["peer"]["id"], "codex:a");
    assert_eq!(identity["peer"]["label"], "gait inspection");
    assert!(!identity.to_string().contains(&codex_token));
    assert!(!ok(&mut s, "csm_peers", json!({"all_projects":true})).to_string().contains(&codex_token));
    ok(&mut s, "csm_connect", json!({"host":"codex","session_id":"a","binding_token":codex_token}));
    let (text, error) =
        tool(&mut s, "csm_connect", json!({"host":"claude","session_id":"b","binding_token":claude_token}));
    assert!(error && text.contains("already bound"));
}

#[test]
fn messaging_validates_inputs_and_keeps_sender_bound() {
    let (mut s, codex_token, _) = server();
    ok(&mut s, "csm_connect", json!({"host":"codex","session_id":"a","binding_token":codex_token}));
    let (text, error) = tool(&mut s, "csm_send", json!({"to":"claude:b","content":"hello"}));
    assert!(error && text.contains("request_id"));
    for args in [json!({"limit":0}), json!({"limit":"10"}), json!({"limit":101})] {
        assert!(tool(&mut s, "csm_inbox", args).1);
    }
    for args in [json!({"timeout_seconds":0}), json!({"timeout_seconds":26})] {
        assert!(tool(&mut s, "csm_wait", args).1);
    }
    for args in [json!({"ids":[]}), json!({"ids":["1"]}), json!({"ids":[-1]})] {
        assert!(tool(&mut s, "csm_ack", args).1);
    }
    assert!(tool(&mut s, "csm_send", json!({"to":"claude:b","content":"x","request_id":"short","ttl_seconds":1})).1);
    assert!(tool(&mut s, "csm_send", json!({"to":"claude:c","content":"x","request_id":"cross"})).1);
    let sent = ok(
        &mut s,
        "csm_send",
        json!({"to":"claude:c","content":"x","request_id":"cross","allow_cross_project":true,"from":"claude:b","project":"C:/work/other"}),
    );
    assert_eq!(sent["message"]["from_peer"], "codex:a");
    assert_eq!(sent["status"], "pending");
}

#[test]
fn inbox_includes_hook_delivery_until_explicit_ack_and_wait_times_out() {
    let store = Store::open_in_memory().unwrap();
    seed(&store);
    let message = store
        .send_message(NewMessage {
            from_peer: "codex:a".into(),
            to_peer: "claude:b".into(),
            content: "Inspect leg placement".into(),
            reply_to: None,
            request_id: "delivered".into(),
            allow_cross_project: false,
            now: now(),
            ttl_seconds: 86400,
        })
        .unwrap();
    store.claim_messages("claude:b", 10, now()).unwrap();
    let claude_token = store.peer_binding_token("claude:b").unwrap();
    let mut s = Server::new(store, PROJECT.into());
    let peer = ok(&mut s, "csm_connect", json!({"host":"claude","session_id":"b","binding_token":claude_token}));
    assert_eq!(peer["peer"]["state"], "idle");
    let inbox = ok(&mut s, "csm_inbox", json!({}));
    assert_eq!(inbox["messages"][0]["id"], message.id);
    assert!(inbox["messages"][0]["delivered_at"].is_number());
    assert!(inbox["messages"][0]["acknowledged_at"].is_null());
    assert_eq!(ok(&mut s, "csm_inbox", json!({"include_delivered":false}))["messages"], json!([]));
    let started = Instant::now();
    assert_eq!(ok(&mut s, "csm_wait", json!({"timeout_seconds":1}))["messages"][0]["id"], message.id);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(ok(&mut s, "csm_ack", json!({"ids":[message.id]}))["acknowledged"], 1);
    let started = Instant::now();
    assert_eq!(ok(&mut s, "csm_wait", json!({"timeout_seconds":1}))["messages"], json!([]));
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert!(started.elapsed() < Duration::from_secs(3));
}

struct Process {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    sequence: u64,
}
impl Process {
    fn start(db: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_csm"))
            .arg("--db")
            .arg(db)
            .args(["mcp", "--project", PROJECT])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self { child, input, output, sequence: 0 }
    }
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.sequence += 1;
        writeln!(self.input, "{}", json!({"jsonrpc":"2.0","id":self.sequence,"method":method,"params":params}))
            .unwrap();
        self.input.flush().unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], self.sequence);
        response["result"].clone()
    }
    fn tool(&mut self, name: &str, args: Value) -> Value {
        let result = self.request("tools/call", json!({"name":name,"arguments":args}));
        assert_eq!(result["isError"], false, "{result}");
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn two_stdio_mcp_processes_coordinate_over_one_temporary_database() {
    let dir = Temp(std::env::temp_dir().join(format!("csm-messaging-mcp-{}-{}", std::process::id(), now())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let db = dir.0.join("csm.db");
    let store = Store::open(&db).unwrap();
    seed(&store);
    let codex_token = store.peer_binding_token("codex:a").unwrap();
    let claude_token = store.peer_binding_token("claude:b").unwrap();
    drop(store);
    let mut codex = Process::start(&db);
    let mut claude = Process::start(&db);
    let init = codex.request("initialize", json!({"protocolVersion":"2025-06-18"}));
    assert!(init["instructions"].as_str().unwrap().contains("not human permission"));
    let schema = codex.request("tools/list", json!({}));
    assert_eq!(schema["tools"].as_array().unwrap().len(), 14);
    codex.tool("csm_connect", json!({"host":"codex","session_id":"a","binding_token":codex_token}));
    claude.tool("csm_connect", json!({"host":"claude","session_id":"b","binding_token":claude_token}));
    let sent = codex
        .tool("csm_send", json!({"to":"claude:b","content":"Can you inspect the landing?","request_id":"wire-send"}));
    let id = sent["message"]["id"].as_i64().unwrap();
    let read = claude.tool("csm_wait", json!({"timeout_seconds":1}));
    assert_eq!(read["messages"][0]["id"], id);
    assert_eq!(codex.tool("csm_message_status", json!({"id":id}))["status"], "pending");
    claude.tool("csm_ack", json!({"ids":[id]}));
    assert_eq!(codex.tool("csm_message_status", json!({"id":id}))["status"], "acknowledged");
    let reply = claude.tool("csm_send", json!({"to":"codex:a","content":"Landing inspected; feet stay above the deck.","request_id":"wire-reply","reply_to":id}));
    assert_eq!(codex.tool("csm_inbox", json!({}))["messages"][0]["id"], reply["message"]["id"]);
}
