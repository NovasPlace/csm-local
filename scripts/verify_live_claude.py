"""Exercise native Claude hooks and a reply using an isolated CSM database.

Uses the user's existing authenticated Claude CLI; creates no account and changes
no installed host settings. The child gets only this test MCP server and no
built-in tools. All child processes are hidden on Windows.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import re
import shutil
import subprocess
import threading
import time
import uuid


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    project = root / "project"
    project.mkdir()
    db = root / "test.db"
    env = os.environ.copy()
    for name in ["CSM_DB", "CSM_HOME", "CLAUDE_PROJECT_DIR", "CLAUDECODE"]:
        env.pop(name, None)
    env["CSM_HOME"] = str(root / "data")
    hidden = subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0

    def csm(arguments, payload=None):
        result = subprocess.run([str(binary), "--db", str(db), *arguments], cwd=project, env=env,
                                input=None if payload is None else json.dumps(payload),
                                text=True, encoding="utf-8", capture_output=True, timeout=30,
                                creationflags=hidden)
        if result.returncode:
            raise RuntimeError(result.stderr[:500])
        return json.loads(result.stdout) if result.stdout.strip() else None

    sender_session = str(uuid.uuid4())
    receiver_session = str(uuid.uuid4())
    sender = "codex:" + sender_session
    receiver = "claude:" + receiver_session
    csm(["hook", "codex", "--messaging"], {
        "hook_event_name": "SessionStart", "session_id": sender_session,
        "cwd": str(project), "source": "startup"})
    hook_command = f'"{binary.as_posix()}" hook claude --db "{db.as_posix()}" --messaging'
    settings = {"hooks": {event: [{"hooks": [{"type": "command", "command": hook_command, "timeout": 5}]}]
                           for event in ["SessionStart", "PostToolUse", "Stop", "SessionEnd"]},
                "permissions": {"allow": ["mcp__csm__*"]}}
    mcp = {"mcpServers": {"csm": {"command": str(binary), "args": ["--db", str(db), "mcp", "--project", str(project)]}}}
    (root / "settings.json").write_text(json.dumps(settings), encoding="utf-8")
    (root / "mcp.json").write_text(json.dumps(mcp), encoding="utf-8")
    challenge = "CSM-ROUNDTRIP-" + uuid.uuid4().hex[:12]
    prompt = (
        "This is a bounded local CSM transport verification authorized by Donovan. Do no game work and use no files or shell. "
        "Connect your OWN messaging identity from the CSM hook using csm_connect (including its private binding_token). "
        "Then call csm_peers. A test message will arrive through the inbox hook after you connect. "
        "If none appears, use csm_wait once, at most 25 seconds. For the single message starting CSM-ROUNDTRIP-, "
        "acknowledge its ID and use csm_send to reply to its sender with content exactly 'ECHO ' followed by its content, "
        "reply_to the received ID, request_id 'one-echo'. Never share your binding token. "
        "After sending the one reply, stop and state only whether the message was received from a hook or a tool. "
        "If no message arrives, stop and report that. Do not send any other messages."
    )
    command = [shutil.which("claude") or "claude", "--print", "--verbose", "--output-format", "stream-json",
               "--include-hook-events", "--no-session-persistence", "--session-id", receiver_session,
               "--setting-sources", "", "--settings", str(root / "settings.json"),
               "--strict-mcp-config", "--mcp-config", str(root / "mcp.json"),
               "--tools", "", "--allowedTools", "mcp__csm__*", "--", prompt]
    child = subprocess.Popen(command, cwd=project, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             text=True, encoding="utf-8", errors="replace", creationflags=hidden)
    lines = queue.Queue()
    def pump(stream, kind):
        for line in stream:
            lines.put((kind, line))
    for stream, kind in [(child.stdout, "stdout"), (child.stderr, "stderr")]:
        threading.Thread(target=pump, args=(stream, kind), daemon=True).start()
    sent = False
    hook_evidence = False
    startup_hook_seen = False
    mcp_connected = False
    host_error = None
    events = []
    started = time.monotonic()
    deadline = started + 180
    message = None
    while time.monotonic() < deadline and (child.poll() is None or not lines.empty()):
        try:
            kind, line = lines.get(timeout=.2)
        except queue.Empty:
            continue
        safe = re.sub(r"(?<![0-9a-f])[0-9a-f]{48}(?![0-9a-f])", "[PRIVATE_BINDING_TOKEN]", line)
        events.append({"stream": kind, "line": safe.rstrip()})
        if kind != "stdout":
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("subtype") == "init":
            mcp_connected = any(server.get("name") == "csm" and server.get("status") == "connected"
                                for server in event.get("mcp_servers", []))
        if event.get("subtype") == "hook_response" and event.get("hook_event") == "SessionStart":
            startup_hook_seen = event.get("exit_code") == 0 and "CSM local messaging" in line
        if event.get("is_api_error_message"):
            host_error = event.get("error", "unknown_host_error")
        # Queue only after the model starts using tools, proving this isn't just
        # an initial prompt or startup-briefing injection.
        blocks = event.get("message", {}).get("content", [])
        if not sent and any(b.get("type") == "tool_use" and b.get("name", "").endswith("csm_connect") for b in blocks):
            message = csm(["send", "--from", sender, "--to", receiver,
                           "--request-id", "challenge", challenge])["message"]
            sent = True
        if event.get("subtype") == "hook_response" and challenge in line and "PostToolUse" in line:
            hook_evidence = True
    timed_out = child.poll() is None
    if timed_out:
        child.kill()
    child.wait(timeout=10)
    replies = csm(["inbox", "--peer", sender])["messages"]
    echo = next((m for m in replies if m["content"] == "ECHO " + challenge), None)
    state = csm(["message-status", "--peer", sender, str(message["id"])]) if message else None
    if echo:
        csm(["ack", "--peer", sender, str(echo["id"])])
    report = {
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "native_host": "claude", "other_endpoint": "isolated CSM CLI sender named codex (not a native Codex model)",
        "exit_code": child.returncode, "timed_out": timed_out, "elapsed_seconds": round(time.monotonic()-started, 2),
        "native_startup_hook_loaded": startup_hook_seen, "native_mcp_connected": mcp_connected,
        "host_error": host_error,
        "challenge_sent_after_connect_tool_started": sent,
        "post_tool_hook_response_contains_challenge": hook_evidence,
        "challenge_explicitly_acknowledged": bool(state and state["status"] == "acknowledged"),
        "exact_reply_received": bool(echo),
    }
    report["passed"] = all(report[key] for key in ["challenge_sent_after_connect_tool_started",
        "post_tool_hook_response_contains_challenge", "challenge_explicitly_acknowledged", "exact_reply_received"]) and not timed_out and child.returncode == 0
    (root / "events.sanitized.json").write_text(json.dumps(events, indent=2), encoding="utf-8")
    (root / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
