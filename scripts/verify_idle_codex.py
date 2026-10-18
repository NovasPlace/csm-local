"""One bounded, explicitly authorized self-wake test after the current chat ends.

This sends from a labelled synthetic CLI peer, not a second model session.
It does not create a chat or copy the private native app route or binding token.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import time
import uuid


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--binary", type=Path, required=True)
    ap.add_argument("--db", type=Path, required=True)
    ap.add_argument("--recipient", required=True)
    ap.add_argument("--output", type=Path, required=True)
    args = ap.parse_args()
    binary, db, output = args.binary.resolve(), args.db.resolve(), args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    env = os.environ.copy()
    for key in ["CODEX_APP_TOOLS_PIPE_PATH", "CODEX_THREAD_ID", "CODEX_SESSION_ID"]:
        env.pop(key, None)
    hidden = subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0
    report = {"runtime_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "synthetic_cli_sender": True, "second_model_started": False,
              "recipient_idle_before_send": False, "passed": False, "state": "waiting_for_idle"}

    def save():
        (output / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    def sql(query, values=()):
        with sqlite3.connect(db, timeout=2) as connection:
            return connection.execute(query, values).fetchone()

    def csm(*words, payload=None):
        result = subprocess.run([str(binary), "--db", str(db), *words], env=env, cwd=output,
                                input=None if payload is None else json.dumps(payload).encode(),
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10, creationflags=hidden)
        if result.returncode:
            raise RuntimeError("bounded test command failed")
        return json.loads(result.stdout) if result.stdout.strip() else None

    save()
    try:
        deadline = time.monotonic() + 600
        idle_at = None
        while time.monotonic() < deadline:
            peer = sql("SELECT state FROM csm_peers WHERE id=?", (args.recipient,))
            enabled = sql("SELECT enabled FROM csm_wake_config WHERE singleton=1")
            if not enabled or not enabled[0]:
                raise RuntimeError("wake disabled during test")
            if peer and peer[0] == "idle":
                idle_at = idle_at or time.monotonic()
                if time.monotonic() - idle_at >= 3:
                    break
            else:
                idle_at = None
            time.sleep(0.25)
        else:
            raise RuntimeError("recipient did not become idle within ten minutes")
        report["recipient_idle_before_send"] = True
        session = "csm-idle-qa-" + uuid.uuid4().hex
        sender = "codex:" + session
        csm("hook", "codex", "--messaging", payload={"hook_event_name": "SessionStart", "source": "startup",
            "session_id": session, "cwd": str(output)})
        body = ("One-time installation test for the user's authorized CSM idle-wake request. "
                "A synthetic CLI sender waited until this chat was idle before sending. "
                f"Read and acknowledge this test, then check {output / 'report.json'} and its message's wake job. "
                "Report whether this chat woke and acknowledged without a new human prompt. "
                "Do not reply to this synthetic sender or start unrelated work. "
                "The 0.3.0 review bundle is already assembled; keep its documented experimental limits.")
        message = csm("send", "--from", sender, "--to", args.recipient, "--allow-cross-project",
                      "--request-id", "one-shot-" + session, "--ttl-seconds", "600", body)["message"]
        report.update(state="waiting_for_native_wake_and_ack", message_id=message["id"])
        save()
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            row = sql("SELECT j.state,m.acknowledged_at FROM csm_messages m LEFT JOIN csm_wake_jobs j "
                      "ON j.message_id=m.id WHERE m.id=?", (message["id"],))
            report.update(native_wake_state=row[0], explicit_acknowledgment=row[1] is not None)
            if row[1] is not None:
                report["passed"] = row[0] == "accepted"
                break
            if row[0] == "uncertain":
                break
            time.sleep(0.25)
        report["state"] = "completed" if report["passed"] else "not_verified"
        csm("hook", "codex", "--messaging", payload={"hook_event_name": "SessionEnd", "session_id": session,
                                                       "cwd": str(output)})
    except Exception as error:
        report.update(state="failed", error=str(error)[:240])
    save()
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
