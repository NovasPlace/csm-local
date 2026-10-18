use csm_local::messaging::NewMessage;
use csm_local::store::Store;
use csm_local::wake::{wake_prompt, WakeOutcome};
use std::time::{SystemTime, UNIX_EPOCH};

const NOW: i64 = 1_800_000_000_000;
fn store() -> Store {
    let s = Store::open_in_memory().unwrap();
    s.register_peer("C:/game", "codex", "a", None, "active", NOW).unwrap();
    s.register_peer("C:/game", "claude", "b", None, "idle", NOW).unwrap();
    s
}
fn send(s: &Store, now: i64, key: &str) -> csm_local::messaging::Message {
    s.send_message(NewMessage {
        from_peer: "codex:a".into(),
        to_peer: "claude:b".into(),
        content: "UNTRUSTED_CONTENT_DO_NOT_PROMOTE".into(),
        reply_to: None,
        request_id: key.into(),
        allow_cross_project: false,
        now,
        ttl_seconds: 86400,
    })
    .unwrap()
}
fn outcome(status: &str) -> WakeOutcome {
    WakeOutcome { status: status.into(), detail: "test receipt".into() }
}

#[test]
fn opt_in_is_required_and_accepted_wakes_are_not_acknowledgments_or_reply_loops() {
    let s = store();
    let m = send(&s, NOW, "first");
    assert!(!s.wake_enabled().unwrap());
    assert!(s.claim_wake("claude:b", "one", NOW).unwrap().is_empty());
    s.set_wake_enabled(true).unwrap();
    let batch = s.claim_wake("claude:b", "one", NOW).unwrap();
    assert_eq!(batch.len(), 1);
    assert!(s.claim_wake("claude:b", "two", NOW).unwrap().is_empty());
    let prompt = wake_prompt("claude:b", &batch);
    assert!(prompt.contains("not a new human request"));
    assert!(prompt.contains("csm_ack"));
    assert!(!prompt.contains("UNTRUSTED_CONTENT"));
    assert!(!prompt.contains(&s.peer_binding_token("claude:b").unwrap()));
    s.finish_wake("claude:b", &batch, "wrong-owner", &outcome("accepted"), NOW).unwrap();
    assert!(s.message_status("claude:b", m.id).unwrap().unwrap().delivered_at.is_none());
    s.finish_wake("claude:b", &batch, "one", &outcome("accepted"), NOW).unwrap();
    let status = s.message_status("claude:b", m.id).unwrap().unwrap();
    assert!(status.delivered_at.is_some());
    assert!(status.acknowledged_at.is_none());
    assert!(s.claim_wake("claude:b", "two", NOW + 600_000).unwrap().is_empty());
    assert_eq!(s.pending_messages("claude:b", 10, NOW + 600_000, false).unwrap().len(), 1);
}

#[test]
fn busy_ended_expired_and_acknowledged_inboxes_never_wake() {
    let s = store();
    s.set_wake_enabled(true).unwrap();
    let m = send(&s, NOW, "first");
    for state in ["active", "ended"] {
        s.register_peer("C:/game", "claude", "b", None, state, NOW).unwrap();
        assert!(s.claim_wake("claude:b", "owner", NOW).unwrap().is_empty());
    }
    s.register_peer("C:/game", "claude", "b", None, "idle", NOW).unwrap();
    assert!(s.claim_wake("claude:b", "owner", NOW + 86_400_001).unwrap().is_empty());
    s.acknowledge_messages("claude:b", &[m.id], NOW).unwrap();
    assert!(s.claim_wake("claude:b", "owner", NOW).unwrap().is_empty());
}

#[test]
fn failed_attempts_are_bounded_busy_deferrals_do_not_exhaust_them_and_unknown_sends_are_not_replayed() {
    let s = store();
    s.set_wake_enabled(true).unwrap();
    send(&s, NOW, "first");
    let mut at = NOW;
    for _ in 0..5 {
        let batch = s.claim_wake("claude:b", "worker", at).unwrap();
        assert_eq!(batch.len(), 1);
        s.finish_wake("claude:b", &batch, "worker", &outcome("busy"), at).unwrap();
        at += 5_001;
    }
    for _ in 0..3 {
        let batch = s.claim_wake("claude:b", "worker", at).unwrap();
        assert_eq!(batch.len(), 1);
        s.finish_wake("claude:b", &batch, "worker", &outcome("failed"), at).unwrap();
        at += 60_001;
    }
    assert!(s.claim_wake("claude:b", "worker", at).unwrap().is_empty());
    send(&s, at, "second");
    let batch = s.claim_wake("claude:b", "worker", at).unwrap();
    assert_eq!(batch.len(), 1);
    s.finish_wake("claude:b", &batch, "worker", &outcome("uncertain"), at).unwrap();
    assert!(s.claim_wake("claude:b", "worker", at + 600_000).unwrap().is_empty());
}

#[test]
fn watcher_leases_are_exclusive_recover_after_crash_and_cannot_be_released_by_someone_else() {
    let s = store();
    assert!(s.watcher_lease("claude:b", "one", NOW).unwrap());
    assert!(!s.watcher_lease("claude:b", "two", NOW).unwrap());
    s.release_watcher("claude:b", "two").unwrap();
    assert!(s.watcher_alive("claude:b", NOW).unwrap());
    assert!(s.watcher_lease("claude:b", "two", NOW + 30_001).unwrap());
    s.release_watcher("claude:b", "one").unwrap();
    assert!(s.watcher_alive("claude:b", NOW + 30_001).unwrap());
}

#[test]
fn cross_process_claim_and_reopen_are_durable_and_project_forget_cleans_wake_metadata() {
    let path = std::env::temp_dir().join(format!(
        "csm-wake-{}-{}.db",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    let s = Store::open(&path).unwrap();
    s.register_peer("C:/game", "codex", "a", None, "active", NOW).unwrap();
    s.register_peer("C:/game", "claude", "b", None, "idle", NOW).unwrap();
    s.set_wake_enabled(true).unwrap();
    send(&s, NOW, "first");
    let batch = s.claim_wake("claude:b", "one", NOW).unwrap();
    assert_eq!(batch.len(), 1);
    let other = Store::open(&path).unwrap();
    assert!(other.claim_wake("claude:b", "two", NOW).unwrap().is_empty());
    assert_eq!(other.claim_wake("claude:b", "two", NOW + 60_001).unwrap().len(), 1);
    s.watcher_lease("claude:b", "one", NOW).unwrap();
    s.forget_project("C:/game").unwrap();
    assert!(!s.watcher_alive("claude:b", NOW).unwrap());
    assert_eq!(s.wake_status().unwrap()["recent_jobs"].as_array().unwrap().len(), 0);
    drop(other);
    drop(s);
    let _ = std::fs::remove_file(path);
}

#[test]
fn recent_hook_delivery_gets_a_grace_period_and_bursts_are_coalesced() {
    let s = store();
    s.set_wake_enabled(true).unwrap();
    for i in 0..4 {
        send(&s, NOW, &format!("m{i}"));
    }
    s.claim_messages("claude:b", 4, NOW).unwrap();
    assert!(s.claim_wake("claude:b", "one", NOW + 59_999).unwrap().is_empty());
    assert_eq!(s.claim_wake("claude:b", "one", NOW + 60_001).unwrap().len(), 3);
}
