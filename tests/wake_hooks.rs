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

/// On Windows a child outlives its parent: a wake-hook left behind by a host
/// that quit would wait out its timeout holding the session's lease, and take
/// the wake meant for the session when it resumes.
#[cfg(windows)]
#[test]
fn a_wake_hook_waits_while_its_parent_runs_and_stops_when_it_is_gone() {
    use std::io::{BufRead, BufReader};
    use std::time::Instant;
    let root = std::env::temp_dir().join(format!("csm-wake-orphan-{}-{}", std::process::id(), now()));
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("mail.db");
    let s = Store::open(&db).unwrap();
    s.register_peer("C:/qa", "claude", "orphan", None, "idle", now()).unwrap();
    s.set_wake_enabled(true).unwrap();
    let payload = root.join("payload.json");
    std::fs::write(&payload, json!({"session_id":"orphan","hook_event_name":"Stop"}).to_string()).unwrap();
    let quoted = |p: &std::path::Path| p.display().to_string().replace('\'', "''");
    // The parent starts the hook the way a shell would, says its process id,
    // stays three seconds, and exits without waiting for it.
    let script = format!(
        "$p = Start-Process -FilePath '{}' -ArgumentList '--db \"{}\" wake-hook claude --timeout-seconds 60' \
         -RedirectStandardInput '{}' -NoNewWindow -PassThru; $p.Id; Start-Sleep -Seconds 3",
        quoted(std::path::Path::new(env!("CARGO_BIN_EXE_csm"))),
        quoted(&db),
        quoted(&payload)
    );
    let mut parent = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(parent.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let pid: u32 = line.trim().parse().unwrap_or_else(|_| panic!("no process id from the parent: {line:?}"));
    let running = || {
        let filter = format!("PID eq {pid}");
        let out = Command::new("tasklist").args(["/FI", &filter, "/NH", "/FO", "CSV"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).contains("csm.exe")
    };

    std::thread::sleep(Duration::from_millis(1500));
    assert!(running(), "the hook waits while its parent runs");
    assert!(s.watcher_alive("claude:orphan", now()).unwrap(), "holding the session's lease");
    assert!(parent.wait().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(10);
    while running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    let outlived = running();
    if outlived {
        let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).output();
    }
    assert!(!outlived, "wake-hook {pid} outlived its parent");
    assert!(!s.watcher_alive("claude:orphan", now()).unwrap(), "and gave the lease up");
    drop(s);
    let _ = std::fs::remove_dir_all(root);
}

/// Git Bash on Windows is a launcher that runs the real shell. When the host
/// goes it takes the launcher at most, and the inner shell keeps waiting for
/// the hook: on 11-14 four hooks sat that way holding leases of closed sessions.
#[cfg(windows)]
#[test]
fn a_wake_hook_stops_when_the_outer_of_two_shells_is_gone() {
    use std::time::Instant;
    let tag = format!("csm-wake-shells-{}-{}", std::process::id(), now());
    let root = std::env::temp_dir().join(&tag);
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("mail.db");
    let s = Store::open(&db).unwrap();
    s.register_peer("C:/qa", "claude", "orphan", None, "idle", now()).unwrap();
    s.set_wake_enabled(true).unwrap();
    let payload = root.join("payload.json");
    std::fs::write(&payload, json!({"session_id":"orphan","hook_event_name":"Stop"}).to_string()).unwrap();
    let hook = root.join("hook.cmd");
    let line = format!(
        "@\"{}\" --db \"{}\" wake-hook claude --timeout-seconds 60 < \"{}\"\r\n",
        env!("CARGO_BIN_EXE_csm"),
        db.display(),
        payload.display()
    );
    std::fs::write(&hook, line).unwrap();
    // This test is the host: the outer shell runs the inner one, which runs the hook.
    let mut outer = Command::new("cmd")
        .args(["/d", "/c", "cmd", "/d", "/c"])
        .arg(&hook)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let left = || {
        let script = format!(
            "Get-CimInstance Win32_Process | Where-Object {{ $_.Name -in 'cmd.exe','csm.exe' -and \
             $_.CommandLine -like '*{tag}*' }} | ForEach-Object {{ $_.ProcessId }}"
        );
        let out =
            Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).split_whitespace().map(str::to_owned).collect::<Vec<_>>()
    };

    std::thread::sleep(Duration::from_millis(1500));
    assert!(s.watcher_alive("claude:orphan", now()).unwrap(), "the hook waits while its shells and host run");
    outer.kill().unwrap();
    outer.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut behind = left();
    while !behind.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        behind = left();
    }
    for pid in &behind {
        let _ = Command::new("taskkill").args(["/PID", pid, "/F"]).output();
    }
    assert!(behind.is_empty(), "the inner shell and the hook outlived the outer shell: {behind:?}");
    assert!(!s.watcher_alive("claude:orphan", now()).unwrap(), "and gave the lease up");
    drop(s);
    let _ = std::fs::remove_dir_all(root);
}
