# Experimental idle inbox wake-up

CSM 0.3 can notify an existing open idle chat when new mail arrives. This is an
experimental host integration, separate from reliable local mailbox storage.
Normal setup leaves it off. Existing authorization for the collaboration still
applies; peer messages cannot grant new permission or bypass host approvals.

## Enable or stop

From the extracted runtime folder, preview and install the native hooks, then
enable the local worker policy:

```powershell
.\csm.exe setup --messaging --wakeup --dry-run
.\csm.exe setup --messaging --wakeup --yes
$csm = Join-Path $env:USERPROFILE '.csm\bin\csm.exe'
& $csm wake enable
& $csm wake status
```

Reload the configured hosts once. In Codex, review changed hooks through
`/hooks` if prompted. A running MCP process retains its old version until
reconnected. Enabling wake starts a hidden worker with no console window.

```powershell
& $csm wake disable
```

Disabling stops watchers and future offers. It does not cancel a turn already
accepted by a host or delete queued messages. Uninstalling all CSM registrations
also disables the policy and retains your database.

## Host behavior

Claude Code needs a version supporting the native `asyncRewake` command hook.
A Stop arms a background inbox watcher; exit code 2 asks Claude to wake using a
system reminder. The first reload/resume and Stop are required to arm an
already open session. The watcher expires after about seven days and is
rearmed at later Stop boundaries. Other errors exit without waking the model.
A registered hook or live watcher alone does not prove a model picked up mail.

Codex Desktop needs its local app-tools connection in the hook environment.
CSM captures that local pipe when the session reaches a supported boundary.
The worker checks the app's current thread status and offers a notification
only when it reports idle. Active or closed chats are deferred. CLI-only Codex
and hosts without this private app connection retain queued mail. This app
bridge is version-dependent and is not a public stable Codex API.

Neither adapter signs in, supplies API keys, accepts permission dialogs, types
into terminals, or resumes a competing CLI instance of the same conversation.
Your existing host account and normal model usage apply to awakened turns.

## Delivery and recovery

The wake notification contains message IDs, not copied peer instructions or
private binding tokens. The recipient reads `csm_inbox`, then calls `csm_ack`
for messages it has read. Host acceptance, delivery, and acknowledgment are
separate records; none proves the requested task was completed.

Exclusive leases prevent duplicate workers. Batches contain at most three
messages, with a five-second cooldown and at most twenty accepted wakes per
recipient in five minutes. Busy deferrals do not exhaust retries. Known failed
connections retry at most three times; uncertain sends and accepted offers
are not replayed automatically just because an acknowledgment is absent.
Normal inbox hooks remain available for recovery.

`wake status` reports policy, route/watcher counts, and bounded job receipts.
It excludes pipe paths and connection tokens. The database still contains
private local route metadata; treat it and backups as private. No external
network listener or cloud service is added.

See [verification](VERIFICATION.md) for automated checks and live-host limits.
Host behavior was checked against [Claude hooks](https://code.claude.com/docs/en/hooks)
and [Codex hooks](https://learn.chatgpt.com/docs/hooks).
