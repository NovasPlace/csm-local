//! Durable local mailbox behavior, recipient boundaries and process races.
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Barrier,
};
use std::thread;

use csm_local::messaging::{NewMessage, PEER_STALE_MS};
use csm_local::store::{NewMemory, Store};

const NOW: i64 = 1_800_000_000_000;
const PROJECT: &str = "C:/test/game";
static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct DbDir(PathBuf);
impl DbDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "csm-mailbox-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> PathBuf {
        self.0.join("mailbox.db")
    }
}
impl Drop for DbDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn register(store: &Store) {
    store.register_peer(PROJECT, "codex", "one", Some("Movement"), "active", NOW).unwrap();
    store.register_peer(PROJECT, "claude", "two", Some("World"), "active", NOW).unwrap();
    store.register_peer(PROJECT, "claude", "three", None, "idle", NOW).unwrap();
}
fn message(request: &str) -> NewMessage {
    NewMessage {
        from_peer: "codex:one".into(),
        to_peer: "claude:two".into(),
        content: "I can own movement.rs; please take terrain.".into(),
        reply_to: None,
        request_id: request.into(),
        allow_cross_project: false,
        now: NOW,
        ttl_seconds: 3600,
    }
}
fn memory() -> NewMemory {
    NewMemory {
        project: PROJECT.into(),
        kind: "fact".into(),
        content: "Preserve the playable controls.".into(),
        tags: vec![],
        importance: 3,
        source: None,
    }
}

#[test]
fn survives_reopening_without_changing_legacy_schema_version() {
    let dir = DbDir::new();
    let id;
    {
        let store = Store::open(&dir.path()).unwrap();
        store.save(memory()).unwrap();
        register(&store);
        id = store.send_message(message("persist")).unwrap().id;
        assert!(store.claim_peer_intro("claude:two", NOW).unwrap());
        assert_eq!(store.stats(None).unwrap().schema_version, 3);
    }
    let store = Store::open(&dir.path()).unwrap();
    assert_eq!(store.count_project(PROJECT).unwrap(), 1);
    assert_eq!(store.pending_messages("claude:two", 10, NOW, true).unwrap()[0].id, id);
    assert!(!store.claim_peer_intro("claude:two", NOW + 1).unwrap());
    let conn = rusqlite::Connection::open(dir.path()).unwrap();
    assert_eq!(conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(), 3);
    assert_eq!(conn.query_row("SELECT version FROM csm_mailbox_meta", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
}

#[test]
fn extension_initializes_on_an_existing_v3_memory_database() {
    let dir = DbDir::new();
    {
        let store = Store::open(&dir.path()).unwrap();
        store.save(memory()).unwrap();
    }
    {
        let conn = rusqlite::Connection::open(dir.path()).unwrap();
        conn.execute_batch("DROP TABLE csm_messages; DROP TABLE csm_peers; DROP TABLE csm_mailbox_meta;").unwrap();
    }
    let store = Store::open(&dir.path()).unwrap();
    register(&store);
    store.send_message(message("upgraded")).unwrap();
    assert_eq!(store.stats(None).unwrap().schema_version, 3);
    assert_eq!(store.count_project(PROJECT).unwrap(), 1);
}

#[test]
fn duplicate_send_is_scoped_to_sender_and_rejects_conflicting_payload() {
    let store = Store::open_in_memory().unwrap();
    register(&store);
    let first = store.send_message(message("same-request")).unwrap();
    let mut retry = message("same-request");
    retry.now += 500;
    assert_eq!(store.send_message(retry.clone()).unwrap(), first);
    retry.content.push('!');
    assert!(store.send_message(retry).is_err());
    let mut ttl_change = message("same-request");
    ttl_change.ttl_seconds += 1;
    assert!(store.send_message(ttl_change).is_err());
    let mut target_change = message("same-request");
    target_change.to_peer = "claude:three".into();
    assert!(store.send_message(target_change).is_err());
    let mut other_sender = message("same-request");
    other_sender.from_peer = "claude:three".into();
    assert_ne!(store.send_message(other_sender).unwrap().id, first.id);
    assert_eq!(store.pending_messages("claude:two", 100, NOW, false).unwrap().len(), 2);
}

#[test]
fn boundaries_reject_unknown_peers_self_send_and_implicit_cross_project() {
    let store = Store::open_in_memory().unwrap();
    register(&store);
    store.register_peer("C:/test/other", "codex", "other", None, "active", NOW).unwrap();
    let mut m = message("x");
    m.from_peer = "codex:missing".into();
    assert!(store.send_message(m).is_err());
    let mut m = message("x");
    m.to_peer = "claude:missing".into();
    assert!(store.send_message(m).is_err());
    let mut m = message("x");
    m.to_peer = m.from_peer.clone();
    assert!(store.send_message(m).is_err());
    let mut m = message("x");
    m.to_peer = "codex:other".into();
    assert!(store.send_message(m.clone()).is_err());
    m.allow_cross_project = true;
    let sent = store.send_message(m).unwrap();
    assert!(store.message_status("claude:two", sent.id).unwrap().is_none());
    assert!(store.message_status("codex:other", sent.id).unwrap().is_some());
    assert!(store.message_status("codex:one", sent.id).unwrap().is_some());
}

#[test]
fn replies_are_limited_to_the_original_two_participants() {
    let store = Store::open_in_memory().unwrap();
    register(&store);
    let original = store.send_message(message("original")).unwrap();
    let mut reply = message("reply");
    reply.from_peer = "claude:two".into();
    reply.to_peer = "codex:one".into();
    reply.reply_to = Some(original.id);
    assert_eq!(store.send_message(reply.clone()).unwrap().reply_to, Some(original.id));
    reply.request_id = "outsider".into();
    reply.from_peer = "claude:three".into();
    assert!(store.send_message(reply).is_err());
    let mut missing = message("missing");
    missing.reply_to = Some(99_999);
    assert!(store.send_message(missing).is_err());
}

#[test]
fn expiry_hides_inbox_without_destroying_sender_status_or_dedup() {
    let store = Store::open_in_memory().unwrap();
    register(&store);
    let mut m = message("expire");
    m.ttl_seconds = 60;
    let sent = store.send_message(m.clone()).unwrap();
    assert_eq!(sent.expires_at, NOW + 60_000);
    assert_eq!(store.pending_messages("claude:two", 10, NOW + 59_999, true).unwrap().len(), 1);
    assert!(store.pending_messages("claude:two", 10, NOW + 60_000, false).unwrap().is_empty());
    assert!(store.claim_messages("claude:two", 10, NOW + 60_000).unwrap().is_empty());
    assert_eq!(store.message_status("codex:one", sent.id).unwrap(), Some(sent.clone()));
    m.now += 120_000;
    assert_eq!(store.send_message(m).unwrap(), sent);
}

#[test]
fn delivery_and_ack_are_separate_and_ack_is_recipient_only_atomic_and_idempotent() {
    let store = Store::open_in_memory().unwrap();
    register(&store);
    let one = store.send_message(message("one")).unwrap();
    let two = store.send_message(message("two")).unwrap();
    assert!(store.acknowledge_messages("codex:one", &[one.id], NOW + 1).is_err());
    assert!(store.acknowledge_messages("claude:three", &[one.id], NOW + 1).is_err());
    assert!(store.acknowledge_messages("claude:two", &[one.id, 99_999], NOW + 1).is_err());
    assert_eq!(store.message_status("claude:two", one.id).unwrap().unwrap().acknowledged_at, None);
    let claimed = store.claim_messages("claude:two", 1, NOW + 2).unwrap();
    assert_eq!(claimed[0].id, one.id);
    assert_eq!(claimed[0].delivered_at, Some(NOW + 2));
    assert_eq!(store.pending_messages("claude:two", 100, NOW + 3, false).unwrap().len(), 2);
    assert_eq!(store.pending_messages("claude:two", 100, NOW + 3, true).unwrap()[0].id, two.id);
    assert_eq!(store.acknowledge_messages("claude:two", &[one.id, one.id, two.id], NOW + 4).unwrap(), 2);
    assert_eq!(store.acknowledge_messages("claude:two", &[one.id, two.id], NOW + 5).unwrap(), 0);
    let status = store.message_status("codex:one", one.id).unwrap().unwrap();
    assert_eq!(status.delivered_at, Some(NOW + 2));
    assert_eq!(status.acknowledged_at, Some(NOW + 4));
    assert_eq!(store.message_status("codex:one", two.id).unwrap().unwrap().delivered_at, Some(NOW + 4));
    assert!(store.pending_messages("claude:two", 100, NOW + 5, false).unwrap().is_empty());
}

#[test]
fn secrets_are_redacted_before_persistence() {
    let dir = DbDir::new();
    let secret = "sk-ant-abcdefghijklmnopqrstuvwxyz0123456789";
    {
        let store = Store::open(&dir.path()).unwrap();
        register(&store);
        let mut m = message("redact");
        m.content = format!("Example credential {secret}; please do not use it.");
        let sent = store.send_message(m).unwrap();
        assert!(!sent.content.contains(secret));
        assert!(sent.content.contains("[REDACTED"));
        let peer = store.register_peer(PROJECT, "claude", "two", Some(secret), "idle", NOW).unwrap();
        assert!(!peer.label.contains(secret));
    }
    let bytes = std::fs::read(dir.path()).unwrap();
    assert!(!bytes.windows(secret.len()).any(|b| b == secret.as_bytes()));
}

#[test]
fn peer_heartbeats_preserve_intro_label_project_and_monotonic_state() {
    let store = Store::open_in_memory().unwrap();
    register(&store);
    assert!(store.claim_peer_intro("codex:one", NOW).unwrap());
    let peer = store.register_peer(PROJECT, "codex", "one", None, "idle", NOW + 1).unwrap();
    assert_eq!(peer.label, "Movement");
    assert_eq!(peer.id, "codex:one");
    assert!(!store.claim_peer_intro("codex:one", NOW + 1).unwrap());
    store.set_peer_state("codex:one", "ended", NOW + 2).unwrap();
    store.set_peer_state("codex:one", "active", NOW).unwrap();
    assert_eq!(store.peer("codex:one").unwrap().unwrap().state, "ended");
    assert!(store.register_peer("other", "codex", "one", None, "active", NOW).is_err());
    assert!(store.set_peer_state("missing", "active", NOW).is_err());
    assert!(store.claim_peer_intro("missing", NOW).is_err());
    assert_eq!(store.peers(Some(PROJECT), NOW, false).unwrap().len(), 2);
    assert!(store.peers(Some(PROJECT), NOW + PEER_STALE_MS + 1, false).unwrap().is_empty());
    let old = store.peers(Some(PROJECT), NOW + PEER_STALE_MS + 1, true).unwrap();
    assert_eq!(old.len(), 3);
    assert_eq!(old.iter().filter(|p| p.state == "stale").count(), 2);
    assert!(store.peers(Some("other"), NOW, true).unwrap().is_empty());
}

#[test]
fn peers_are_bounded_and_message_validation_counts_utf8_bytes() {
    let store = Store::open_in_memory().unwrap();
    register(&store);
    for i in 0..110 {
        store.register_peer(PROJECT, "codex", &format!("peer-{i}"), None, "active", NOW + i).unwrap();
    }
    let peers = store.peers(None, NOW + 200, true).unwrap();
    assert_eq!(peers.len(), 100);
    assert_eq!(peers[0].id, "codex:peer-109");
    for content in [" ".into(), "abc\0def".into(), "é".repeat(4097)] {
        let mut m = message("invalid");
        m.content = content;
        assert!(store.send_message(m).is_err());
    }
    for ttl in [0, 59, 604_801, i64::MAX] {
        let mut m = message("ttl");
        m.ttl_seconds = ttl;
        assert!(store.send_message(m).is_err());
    }
    let mut m = message("overflow");
    m.now = i64::MAX;
    assert!(store.send_message(m).is_err());
    let mut m = message("utf8");
    m.content = "é".repeat(4096);
    store.send_message(m).unwrap();
    assert!(store.pending_messages("claude:two", 0, NOW, false).unwrap().is_empty());
    assert!(store.pending_messages("missing", 1, NOW, false).is_err());
}

#[test]
fn sender_rate_limit_is_transactional_and_retry_does_not_consume_capacity() {
    let store = Store::open_in_memory().unwrap();
    register(&store);
    for i in 0..20 {
        store.send_message(message(&format!("r{i}"))).unwrap();
    }
    assert!(store.send_message(message("overflow")).is_err());
    store.send_message(message("r0")).unwrap();
    let mut later = message("later");
    later.now += 60_000;
    store.send_message(later).unwrap();
}

#[test]
fn inbox_and_reads_are_bounded_oldest_first() {
    let store = Store::open_in_memory().unwrap();
    register(&store);
    let mut ids = vec![];
    for sender in 0..6 {
        store.register_peer(PROJECT, "codex", &format!("sender{sender}"), None, "active", NOW).unwrap();
        if sender == 5 {
            break;
        }
        for i in 0..20 {
            let mut m = message(&format!("{sender}/{i}"));
            m.from_peer = format!("codex:sender{sender}");
            ids.push(store.send_message(m).unwrap().id);
        }
    }
    let mut overflow = message("overflow");
    overflow.from_peer = "codex:sender5".into();
    assert!(store.send_message(overflow.clone()).is_err());
    let pending = store.pending_messages("claude:two", usize::MAX, NOW, false).unwrap();
    assert_eq!(pending.iter().map(|m| m.id).collect::<Vec<_>>(), ids);
    assert_eq!(
        store.pending_messages("claude:two", 2, NOW, false).unwrap().iter().map(|m| m.id).collect::<Vec<_>>(),
        ids[..2]
    );
    store.acknowledge_messages("claude:two", &[ids[0]], NOW + 1).unwrap();
    store.send_message(overflow).unwrap();
    assert_eq!(store.claim_messages("claude:two", usize::MAX, NOW + 2).unwrap().len(), 100);
}

#[test]
fn simultaneous_claimers_deliver_each_message_and_intro_exactly_once() {
    let dir = DbDir::new();
    {
        let store = Store::open(&dir.path()).unwrap();
        register(&store);
        for i in 0..20 {
            store.send_message(message(&format!("m{i}"))).unwrap();
        }
    }
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = dir.path();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                barrier.wait();
                let intro = store.claim_peer_intro("claude:two", NOW + 1).unwrap();
                let messages = store.claim_messages("claude:two", 5, NOW + 1).unwrap();
                (intro, messages.into_iter().map(|m| m.id).collect::<Vec<_>>())
            })
        })
        .collect();
    let mut ids = vec![];
    let mut intros = 0;
    for h in handles {
        let (intro, claimed) = h.join().unwrap();
        intros += usize::from(intro);
        ids.extend(claimed);
    }
    assert_eq!(intros, 1);
    assert_eq!(ids.len(), 20);
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 20);
    let store = Store::open(&dir.path()).unwrap();
    assert!(store.claim_messages("claude:two", 100, NOW + 2).unwrap().is_empty());
    assert_eq!(store.pending_messages("claude:two", 100, NOW + 2, false).unwrap().len(), 20);
}

#[test]
fn simultaneous_duplicate_sends_create_one_row() {
    let dir = DbDir::new();
    {
        let store = Store::open(&dir.path()).unwrap();
        register(&store);
    }
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = dir.path();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                barrier.wait();
                store.send_message(message("concurrent")).unwrap().id
            })
        })
        .collect();
    let mut ids: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 1);
    let store = Store::open(&dir.path()).unwrap();
    assert_eq!(store.pending_messages("claude:two", 100, NOW, false).unwrap().len(), 1);
}

#[test]
fn forgetting_project_removes_mailboxes_and_both_directions_of_cross_project_messages() {
    let dir = DbDir::new();
    let store = Store::open(&dir.path()).unwrap();
    register(&store);
    let other = "C:/test/other";
    store.register_peer(other, "claude", "elsewhere", None, "active", NOW).unwrap();
    assert!(store.project_keys().unwrap().contains(&PROJECT.to_string()));
    let local = store.send_message(message("local")).unwrap();
    let mut out = message("out");
    out.to_peer = "claude:elsewhere".into();
    out.allow_cross_project = true;
    let out = store.send_message(out).unwrap();
    let mut incoming = message("in");
    incoming.from_peer = "claude:elsewhere".into();
    incoming.allow_cross_project = true;
    let incoming = store.send_message(incoming).unwrap();
    assert_eq!(store.mailbox_project_counts(PROJECT).unwrap(), (3, 3));
    assert_eq!(store.mailbox_project_counts(other).unwrap(), (2, 1));
    assert_eq!(store.mailbox_project_counts("missing").unwrap(), (0, 0));
    let forgotten = store.forget_project(PROJECT).unwrap();
    assert_eq!((forgotten.memories, forgotten.events, forgotten.messages, forgotten.peers), (0, 0, 3, 3));
    assert_eq!(store.mailbox_project_counts(PROJECT).unwrap(), (0, 0));
    assert_eq!(store.mailbox_project_counts(other).unwrap(), (0, 1));
    assert!(store.peer("codex:one").unwrap().is_none());
    assert!(store.peer("claude:elsewhere").unwrap().is_some());
    assert_eq!(store.project_keys().unwrap(), vec![other.to_string()]);
    for id in [local.id, out.id, incoming.id] {
        assert!(store.message_status("claude:elsewhere", id).unwrap().is_none());
    }
    assert!(store.pending_messages("claude:elsewhere", 100, NOW, false).unwrap().is_empty());
}

#[test]
fn unacknowledged_offer_retries_after_lease_and_ack_stops_recovery() {
    let store = Store::open_in_memory().unwrap();
    register(&store);
    let sent = store.send_message(message("lease")).unwrap();
    assert_eq!(store.claim_messages("claude:two", 10, NOW).unwrap()[0].id, sent.id);
    assert!(store.claim_messages("claude:two", 10, NOW + 59_999).unwrap().is_empty());
    assert!(store.pending_messages("claude:two", 10, NOW + 60_000, true).unwrap().is_empty());
    let retried = store.claim_messages("claude:two", 10, NOW + 60_000).unwrap();
    assert_eq!(retried[0].id, sent.id);
    assert_eq!(retried[0].delivered_at, Some(NOW + 60_000));
    assert!(store.claim_messages("claude:two", 10, NOW + 119_999).unwrap().is_empty());
    store.acknowledge_messages("claude:two", &[sent.id], NOW + 120_000).unwrap();
    assert!(store.claim_messages("claude:two", 10, NOW + 120_000).unwrap().is_empty());
}

#[test]
fn lease_at_unix_epoch_is_not_shortened_by_saturating_time_math() {
    let store = Store::open_in_memory().unwrap();
    register(&store);
    let mut m = message("epoch");
    m.now = 0;
    store.send_message(m).unwrap();
    assert_eq!(store.claim_messages("claude:two", 10, 0).unwrap().len(), 1);
    assert!(store.claim_messages("claude:two", 10, 1).unwrap().is_empty());
    assert!(store.claim_messages("claude:two", 10, 59_999).unwrap().is_empty());
    assert_eq!(store.claim_messages("claude:two", 10, 60_000).unwrap().len(), 1);
}

#[test]
fn binding_nonce_is_unique_stable_private_and_required_to_match() {
    let dir = DbDir::new();
    let token;
    {
        let store = Store::open(&dir.path()).unwrap();
        register(&store);
        token = store.peer_binding_token("codex:one").unwrap();
        assert_eq!(token.len(), 48);
        let other = store.peer_binding_token("claude:two").unwrap();
        assert_ne!(token, other);
        assert!(store.verify_peer_binding("codex:one", &token).unwrap());
        assert!(!store.verify_peer_binding("codex:one", &other).unwrap());
        assert!(!store.verify_peer_binding("codex:missing", &token).unwrap());
        assert!(!store.verify_peer_binding("codex:one", "").unwrap());
        assert!(!store.verify_peer_binding("codex:one", &token.to_uppercase()).unwrap());
        assert!(store.peer_binding_token("codex:missing").is_err());
        store.register_peer(PROJECT, "codex", "one", Some("Renamed"), "idle", NOW + 1).unwrap();
        assert_eq!(store.peer_binding_token("codex:one").unwrap(), token);
        let peer_json = serde_json::to_string(&store.peer("codex:one").unwrap()).unwrap();
        let listing_json = serde_json::to_string(&store.peers(None, NOW, true).unwrap()).unwrap();
        assert!(!peer_json.contains(&token));
        assert!(!peer_json.contains("binding_token"));
        assert!(!listing_json.contains(&token));
        assert!(!listing_json.contains(&other));
    }
    let store = Store::open(&dir.path()).unwrap();
    assert_eq!(store.peer_binding_token("codex:one").unwrap(), token);
    assert!(store.verify_peer_binding("codex:one", &token).unwrap());
}

#[test]
fn concurrent_senders_cannot_exceed_the_per_session_rate_limit() {
    let dir = DbDir::new();
    {
        let store = Store::open(&dir.path()).unwrap();
        register(&store);
    }
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|worker| {
            let path = dir.path();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                barrier.wait();
                let mut accepted = 0;
                for item in 0..3 {
                    match store.send_message(message(&format!("{worker}-{item}"))) {
                        Ok(_) => accepted += 1,
                        Err(e) => assert!(e.to_string().contains("20 messages per minute")),
                    }
                }
                accepted
            })
        })
        .collect();
    assert_eq!(handles.into_iter().map(|h| h.join().unwrap()).sum::<usize>(), 20);
    let store = Store::open(&dir.path()).unwrap();
    assert_eq!(store.pending_messages("claude:two", 100, NOW, false).unwrap().len(), 20);
}

#[test]
fn sender_rate_limit_also_applies_at_unix_epoch() {
    let store = Store::open_in_memory().unwrap();
    register(&store);
    for i in 0..20 {
        let mut m = message(&format!("epoch-{i}"));
        m.now = 0;
        store.send_message(m).unwrap();
    }
    let mut overflow = message("epoch-overflow");
    overflow.now = 0;
    assert!(store.send_message(overflow).is_err());
    let mut later = message("epoch-later");
    later.now = 60_000;
    store.send_message(later).unwrap();
}

#[test]
fn expired_and_acknowledged_history_is_retained_until_project_forget() {
    let dir = DbDir::new();
    let (expired, acknowledged);
    {
        let store = Store::open(&dir.path()).unwrap();
        register(&store);
        let mut expiring = message("retained-expired");
        expiring.ttl_seconds = 60;
        expired = store.send_message(expiring).unwrap().id;
        acknowledged = store.send_message(message("retained-acknowledged")).unwrap().id;
        store.acknowledge_messages("claude:two", &[acknowledged], NOW + 1).unwrap();
    }
    let store = Store::open(&dir.path()).unwrap();
    assert!(store.pending_messages("claude:two", 100, NOW + 3_600_000, false).unwrap().is_empty());
    assert!(store.claim_messages("claude:two", 100, NOW + 3_600_000).unwrap().is_empty());
    assert_eq!(store.mailbox_project_counts(PROJECT).unwrap(), (2, 3));
    assert!(store.message_status("codex:one", expired).unwrap().is_some());
    assert_eq!(store.message_status("codex:one", acknowledged).unwrap().unwrap().acknowledged_at, Some(NOW + 1));
    let forgotten = store.forget_project(PROJECT).unwrap();
    assert_eq!((forgotten.messages, forgotten.peers), (2, 3));
    assert_eq!(store.mailbox_project_counts(PROJECT).unwrap(), (0, 0));
}
