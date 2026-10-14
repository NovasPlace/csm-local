//! Many hook processes hit the same database at once: parallel tool calls,
//! two hosts, subagents. A fresh database must survive that.

use std::sync::{Arc, Barrier};
use std::thread;

use csm_local::journal::{EventQuery, NewEvent};
use csm_local::store::Store;

#[test]
fn concurrent_first_opens_and_writes_all_succeed() {
    let dir = std::env::temp_dir().join(format!("csm-concurrency-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("csm.db");
    let workers = 12;
    let per_worker = 20;
    let barrier = Arc::new(Barrier::new(workers));

    let handles: Vec<_> = (0..workers)
        .map(|w| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let store = Store::open_with_timeout(&path, std::time::Duration::from_secs(10)).expect("open");
                for i in 0..per_worker {
                    store
                        .record(NewEvent {
                            project: "C:/work/app".into(),
                            host: "claude".into(),
                            session_id: Some(format!("s{w}")),
                            kind: "edit".into(),
                            summary: format!("Edit {w}-{i}"),
                            dedup_key: Some(format!("tool:{i}")),
                            ..Default::default()
                        })
                        .expect("record");
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let store = Store::open(&path).unwrap();
    let events = store.events(&EventQuery { limit: 10_000, ..Default::default() }).unwrap();
    assert_eq!(events.len(), workers * per_worker);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}
