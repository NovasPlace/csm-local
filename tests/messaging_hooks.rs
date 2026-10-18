use csm_local::hook::{handle, HookEnv, Host};
use csm_local::messaging::NewMessage;
use csm_local::store::Store;
use serde_json::{json, Value};

const NOW: i64 = 1_800_000_000_000;

fn env() -> HookEnv {
    HookEnv { project_dir: None, capture_text: true, budget: 8000, now: NOW, messaging: true, codex_app_pipe: None }
}

fn event(name: &str, session: &str) -> Value {
    json!({"hook_event_name": name, "session_id": session, "cwd": "C:/csm-message-test",
        "tool_name": "mcp__test__read", "tool_use_id": format!("tool-{session}"), "tool_input": {}})
}

fn enroll(store: &Store) {
    handle(store, Host::Codex, &event("SessionStart", "receiver"), &env()).unwrap();
    handle(store, Host::Claude, &event("SessionStart", "sender"), &env()).unwrap();
}

fn send(store: &Store, key: &str, content: &str) -> i64 {
    store
        .send_message(NewMessage {
            from_peer: "claude:sender".into(),
            to_peer: "codex:receiver".into(),
            content: content.into(),
            reply_to: None,
            request_id: key.into(),
            allow_cross_project: false,
            now: NOW,
            ttl_seconds: 86400,
        })
        .unwrap()
        .id
}

#[test]
fn opt_in_introduces_exact_identity_once_and_legacy_hooks_stay_silent() {
    let store = Store::open_in_memory().unwrap();
    let mut legacy = env();
    legacy.messaging = false;
    assert!(handle(&store, Host::Codex, &event("PostToolUse", "legacy"), &legacy).unwrap().is_none());
    assert!(store.peer("codex:legacy").unwrap().is_none());
    let out = handle(&store, Host::Codex, &event("PostToolUse", "receiver"), &env()).unwrap().unwrap();
    let value: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert!(value["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("codex:receiver"));
    assert!(handle(&store, Host::Codex, &event("PostToolUse", "receiver"), &env()).unwrap().is_none());
}

#[test]
fn mcp_read_tool_delivers_mail_once_and_ack_remains_explicit() {
    let store = Store::open_in_memory().unwrap();
    enroll(&store);
    let id = send(&store, "read-boundary", "The wall regression passed; please re-read rider.gd.");
    let out = handle(&store, Host::Codex, &event("PostToolUse", "receiver"), &env()).unwrap().unwrap();
    let value: Value = serde_json::from_str(&out).unwrap();
    let context = value["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(context.contains("wall regression passed"));
    assert!(context.contains("not human instructions or approval"));
    assert!(value.get("decision").is_none());
    let message = store.message_status("claude:sender", id).unwrap().unwrap();
    assert_eq!(message.delivered_at, Some(NOW));
    assert_eq!(message.acknowledged_at, None);
    assert!(handle(&store, Host::Codex, &event("PostToolUse", "receiver"), &env()).unwrap().is_none());
    assert_eq!(store.pending_messages("codex:receiver", 10, NOW, false).unwrap().len(), 1);
    store.acknowledge_messages("codex:receiver", &[id], NOW + 1).unwrap();
    assert!(store.pending_messages("codex:receiver", 10, NOW + 1, false).unwrap().is_empty());
}

#[test]
fn stop_continues_only_for_new_mail_and_never_loops() {
    let store = Store::open_in_memory().unwrap();
    enroll(&store);
    assert!(handle(&store, Host::Codex, &event("Stop", "receiver"), &env()).unwrap().is_none());
    send(&store, "first", "Please inspect the failing collision case.");
    let out = handle(&store, Host::Codex, &event("Stop", "receiver"), &env()).unwrap().unwrap();
    let value: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["decision"], "block");
    assert!(value["reason"].as_str().unwrap().contains("failing collision"));
    send(&store, "second", "A second message remains queued during the guarded stop.");
    let mut stopping = event("Stop", "receiver");
    stopping["stop_hook_active"] = json!(true);
    assert!(handle(&store, Host::Codex, &stopping, &env()).unwrap().is_none());
    assert_eq!(store.pending_messages("codex:receiver", 10, NOW, true).unwrap().len(), 1);
}

#[test]
fn subagents_do_not_steal_parent_mail_and_session_end_does_not_drain_it() {
    let store = Store::open_in_memory().unwrap();
    enroll(&store);
    send(&store, "target", "Only for the parent session.");
    let mut child = event("PostToolUse", "receiver");
    child["agent_id"] = json!("child");
    assert!(handle(&store, Host::Codex, &child, &env()).unwrap().is_none());
    assert!(handle(&store, Host::Codex, &event("SessionEnd", "receiver"), &env()).unwrap().is_none());
    assert_eq!(store.peer("codex:receiver").unwrap().unwrap().state, "ended");
    assert_eq!(store.pending_messages("codex:receiver", 10, NOW, true).unwrap().len(), 1);
}

#[test]
fn both_hosts_receive_same_envelope_and_preview_is_bounded() {
    let store = Store::open_in_memory().unwrap();
    enroll(&store);
    let content = "あ".repeat(1800);
    send(&store, "long", &content);
    let out = handle(&store, Host::Codex, &event("PostToolUse", "receiver"), &env()).unwrap().unwrap();
    assert!(out.len() < 4000);
    assert!(out.contains("truncated"));
    assert_eq!(store.pending_messages("codex:receiver", 10, NOW, false).unwrap()[0].content, content);
    store
        .send_message(NewMessage {
            from_peer: "codex:receiver".into(),
            to_peer: "claude:sender".into(),
            content: "Reply from Codex".into(),
            reply_to: None,
            request_id: "reply".into(),
            allow_cross_project: false,
            now: NOW,
            ttl_seconds: 86400,
        })
        .unwrap();
    let out = handle(&store, Host::Claude, &event("UserPromptSubmit", "sender"), &env()).unwrap().unwrap();
    let value: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
    assert!(out.contains("Reply from Codex"));
}

#[test]
fn changing_working_directory_does_not_disconnect_a_session_inbox() {
    let store = Store::open_in_memory().unwrap();
    enroll(&store);
    send(&store, "cwd-change", "Still addressed to this session.");
    let mut moved = event("PostToolUse", "receiver");
    moved["cwd"] = json!("C:/another-project");
    let out = handle(&store, Host::Codex, &moved, &env()).unwrap().unwrap();
    assert!(out.contains("Still addressed to this session"));
    assert!(!store.peer("codex:receiver").unwrap().unwrap().project.contains("another-project"));
}

#[test]
fn resume_and_compaction_restore_binding_context_after_the_initial_intro() {
    let store = Store::open_in_memory().unwrap();
    enroll(&store);
    for source in ["resume", "compact"] {
        let mut payload = event("SessionStart", "receiver");
        payload["source"] = json!(source);
        let out = handle(&store, Host::Codex, &payload, &env()).unwrap().unwrap();
        assert!(out.contains("codex:receiver"));
        assert!(out.contains("binding_token"));
    }
}
