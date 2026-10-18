# Local session messaging

CSM 0.2 adds addressed, durable messages between active Claude Code and Codex
sessions. The existing chats are the interface; no dashboard or extra terminal
is needed. Messaging is opt-in and independent of saved project memories.

## Delivery behavior

With messaging enabled, CSM checks the inbox after tools, when a user prompt is
submitted, and at session startup/resume. A new message arriving just before a
normal Stop can continue the turn once. A repeated Stop never forces another
continuation. Empty inboxes never keep an agent running.

Ordinary inbox hooks deliver during active work. Without a wake adapter, fully
idle chats keep messages queued until their next supported boundary. Long tool
calls also delay pickup. Version 0.3 provides experimental, separately opted-in
[idle wake adapters](IDLE-WAKE.md) for compatible open hosts. These use the
existing chat and host account; they do not launch a second model session.

Delivery means an envelope was offered to the host. Only `csm_ack` records the
recipient's explicit acknowledgment; neither status proves the requested work
was performed. Unacknowledged envelopes may be offered again after 60 seconds
to recover from dropped hook output. An agent should acknowledge after reading.
Hook previews are bounded; `csm_inbox` returns the full pending message.

## Enable

Run from the new runtime folder:

```powershell
.\csm.exe setup --messaging --dry-run
.\csm.exe setup --messaging --yes
.\csm.exe setup --status
```

Setup preserves other hooks, backs up changed configuration, and widens only
CSM's PostToolUse matcher so read/MCP tools can deliver messages too. Journal
capture still records only the relevant tool categories. Later ordinary setup
runs preserve an existing messaging opt-in. Uninstall removes these hooks along
with CSM's existing registration and leaves data in place.

Restart/reconnect host MCP servers to load the new tools. Codex must review and
trust changed hooks through `/hooks`; setup does not grant this trust. Claude
may reload settings automatically, but restart if new tools/context are absent.
The setup status confirms configuration, not successful live delivery.

## Agent workflow

The session hook gives the agent its exact host, session ID, and private binding
token. `csm_connect` binds that MCP process to that identity; it cannot then
switch to another identity. The token is absent from peer discovery and messages.

1. Connect with the identity supplied by the session hook.
2. Use `csm_peers` to select a specific recipient, optionally naming your own
   session with `csm_connect`'s label. Presence is last-observed activity, not
   proof that an agent is listening now.
3. Send a concise update with `csm_send`, an exact recipient, and a unique
   `request_id`. Retry the same send with the same ID; conflicting reuse fails.
4. Read incoming envelopes, fetch truncated content with `csm_inbox`, and call
   `csm_ack`. Reply only when it helps the authorized work, using `reply_to`.
5. `csm_message_status` checks delivery/acknowledgment. `csm_wait` optionally
   waits up to 25 seconds without holding a database write lock.

Messages from a different project require an explicit `allow_cross_project`
send. This is useful where the same logical project uses separate folder keys.
There is no broadcast, automatic task assignment, or automatic reply loop.
Peer messages remain agent-authored data and cannot supply human approval or
override a session's instructions. Acknowledgment is receipt, not agreement.

## Storage and limits

Messages use separate tables in the existing local SQLite database. Their schema
version is tracked separately; the original memory/journal schema stays at v3
so existing 0.1.3 MCP processes can continue until restarted. Saved memory search
does not include chat messages. The older runtime cannot manage/delete mailbox
records; use 0.2 for project deletion once messaging has been enabled.

Messages are capped at 8 KiB, 20 new messages per sender per minute, and 100
unacknowledged/unexpired messages per inbox. Default expiry is 24 hours, with an
explicit range of one minute to seven days. Expiry stops delivery; it does not
erase historical rows. `forget --project ... --yes` removes that project's peers
and messages in both directions. Back up using SQLite's online backup API while
agents are running, as documented in [recovery](RECOVERY.md).

Obvious credentials in message bodies and labels are redacted. The binding token
prevents accidental MCP impersonation through a discovered peer ID; it is not a
security boundary against another process that can read your local database.
The local CLI deliberately permits the local user to name a sender or recipient.

## Verification boundary

Rust tests exercise persistence, concurrent sends and delivery, retries,
recipient-scoped acknowledgments, expiry, project routing, output bounds, and
both hosts' hook envelopes. Process tests exercise independent stdio MCP clients.
These checks must be distinguished from an actual model reading a hook and
replying. See the [verification report](VERIFICATION.md) for which
host runs succeeded.

Host contracts checked against [Codex hooks](https://learn.chatgpt.com/docs/hooks)
and [Claude Code hooks](https://code.claude.com/docs/en/hooks).
