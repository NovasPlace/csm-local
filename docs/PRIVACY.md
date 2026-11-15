# Local data and privacy

CSM itself has no network client, hosted storage, telemetry, or account system.
Its database is local SQLite at `%USERPROFILE%\.csm\csm.db` by default. `CSM_HOME`
changes the storage directory; `CSM_DB` or `--db` can select a different database.
CSM does not encrypt the database. Protect it with your normal filesystem and
device security, and include it in your own backup policy if needed.

Briefings and tool results are passed to your chosen coding agent. That agent
may send this context to its model provider under its own settings and policies.
Local storage does not mean the recalled information never reaches an AI service.

## What is recorded

With hooks enabled, CSM records session events and short excerpts of prompts,
final replies, relevant commands, edits, file paths, and reported failures.
These are not full transcripts. Saved memories and checkpoints contain the
content the user or agent deliberately supplies. Common credential patterns are
redacted, but redaction is best-effort and is not a guarantee that every secret
will be recognized. Avoid saving secrets or confidential material unnecessarily.

Each record also carries the name of the model the session was running as, when
known: Codex names it in every hook event, and for Claude Code the hook reads it
from the end of the session's own transcript file, which is where a model
switched mid-session shows up. Only the model name is taken from the transcript;
nothing else in it is read into the database.

Opt-in messaging also stores session addresses, private connection tokens,
message bodies, routing metadata, and delivery/acknowledgment timestamps in
separate local tables. Message context reaches the recipient's model provider
under that host's policies. Tokens are absent from peer discovery and message
results. Message expiry stops delivery but does not delete history; project
`forget` deletes its peers and messages in both directions.

Experimental idle wake also stores private local app routes and bounded job receipts.
`wake status` excludes those route paths and binding tokens. `wake disable` stops
future notifications; an accepted host turn may already be running.

Prompt/reply capture defaults on. To disable that text capture, set
`CSM_CAPTURE_TEXT=0` in the environment inherited by the agent and restart it.
This does not disable tool-event capture, already stored text, explicit memory
writes, or checkpoint content. Disable/uninstall the CSM hooks to stop automatic
capture entirely. Desktop apps may require a full exit and relaunch to inherit
changed environment settings.

When the user types a request, CSM can show the agent up to three earlier
requests from other sessions in the same project that share its rarer words,
with the reply each got and what the user said next, once per session. It can
also show up to three of the project's saved memories that match the request,
and, before an agent edits a file inside the project, up to three that name
that file: each memory once per session, and again after the session compacts.
That text goes to the agent's model provider like a briefing. When a reply
reports a result after an unrun code change, or one like a result the user
pushed back on, CSM sends the agent back once to say what it verified. Set
`CSM_RECALL=0` or `CSM_CLAIM_CHECK=0` in the agent's environment to turn either
off; `CSM_RECALL=0` turns off the earlier requests and both kinds of memory.

When a session has taken four requests and changed code since it last saved a
memory, a checkpoint or a decision, CSM sends the agent back once to save what a
later session could not see in the code, or to say there is nothing; it then
counts again from there. What the agent saves is its own writing, redacted like
any other memory. The hooks note that a save happened, not what was saved. Set
`CSM_SAVE_NUDGE=0` to turn it off.

A project can have one project card, a memory of kind `card` of at most 1,000
bytes that says what the project is and how to build, test and run it. Every
briefing in that project opens with it, so it reaches the agent's model provider
at each session start. Saving a new card writes over the last one; `csm delete`
removes it.

After a compaction or a resume, the briefing a session gets describes that
session itself in place of the one before it: its last request and reply, up to
two earlier requests, the project files it changed and its last commands, from
the journal. That part has up to 1,500 bytes of its own on top of the briefing
budget.

Automatic journal records are pruned in bounded batches during session starts
using 90-day and 5,000-event-per-project thresholds; requests and replies age
out at 90 days and do not count toward the 5,000. This does not guarantee
immediate deletion at a fixed age. Saved memories and agent-authored goals/checkpoints
are retained until explicitly deleted. App-generated suggestion sessions remain
in raw records but are filtered from briefings when their identifying prompts
are present.

## Inspect and export

Use an explicit project path to avoid mixing projects:

```powershell
$csm = Join-Path $env:USERPROFILE '.csm\bin\csm.exe'
$project = (Get-Location).Path
& $csm status --project $project
& $csm list --project $project
& $csm journal --project $project
# Export contains project text; choose a private destination.
& $csm export --project $project | Set-Content -LiteralPath .\csm-export.json -Encoding utf8
```

Export is JSON for inspection and portability; this release has no import command.
It includes memories, journal records and decision board entries, not mailbox tables. Use `peers`,
`inbox`, and `message-status` to inspect messaging. A database backup includes all
tables, including retained message history.
For a restorable database backup, stop agents and CSM processes and copy the data
directory as described in the recovery guide, or use a SQLite-aware backup tool.

## Delete project data

`forget` has a preview followed by an explicit destructive action:

```powershell
& $csm forget --project $project
# Only after reviewing the preview and deciding to delete this project's data:
& $csm forget --project $project --yes
```

This removes saved memories, checkpoints, journal events, standing decisions,
file claims, messaging peers, and messages to or from that project in the
selected database. Use version 0.2 or
later to include mailbox data. It does not erase exports, filesystem backups, agent
transcripts or information already sent to a model provider. If CSM reports that
database scrubbing was incomplete because another connection is busy, close the
other connections and follow its reported instructions. Filesystem/SSD backups
and snapshots can retain historical data independently of CSM.

To remove all CSM files, first uninstall its registrations from every host and
close those hosts. Review the actual directory from `status`, preserve anything
you want to keep, then remove that CSM data directory through File Explorer.
Also review custom database locations and exported files. Do not delete a shared
parent directory or restore an old backup without checking its contents.
