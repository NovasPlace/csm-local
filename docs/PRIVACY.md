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

Prompt/reply capture defaults on. To disable that text capture, set
`CSM_CAPTURE_TEXT=0` in the environment inherited by the agent and restart it.
This does not disable tool-event capture, already stored text, explicit memory
writes, or checkpoint content. Disable/uninstall the CSM hooks to stop automatic
capture entirely. Desktop apps may require a full exit and relaunch to inherit
changed environment settings.

Automatic journal records are pruned in bounded batches during session starts
using 90-day and 5,000-event-per-project thresholds. This does not guarantee
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
For a restorable database backup, stop agents and CSM processes and copy the data
directory as described in the recovery guide, or use a SQLite-aware backup tool.

## Delete project data

`forget` has a preview followed by an explicit destructive action:

```powershell
& $csm forget --project $project
# Only after reviewing the preview and deciding to delete this project's data:
& $csm forget --project $project --yes
```

This removes saved memories, checkpoints and journal events for that project in
the selected database. It does not erase exports, filesystem backups, agent
transcripts or information already sent to a model provider. If CSM reports that
database scrubbing was incomplete because another connection is busy, close the
other connections and follow its reported instructions. Filesystem/SSD backups
and snapshots can retain historical data independently of CSM.

To remove all CSM files, first uninstall its registrations from every host and
close those hosts. Review the actual directory from `status`, preserve anything
you want to keep, then remove that CSM data directory through File Explorer.
Also review custom database locations and exported files. Do not delete a shared
parent directory or restore an old backup without checking its contents.
