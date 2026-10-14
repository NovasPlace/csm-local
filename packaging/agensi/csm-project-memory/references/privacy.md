# Privacy, inspection, and removal

Default storage is `%USERPROFILE%\.csm\csm.db`. `CSM_HOME` changes the data directory; `CSM_DB` or explicit `--db` selects a database. Overrides must use absolute paths. Setup records a nondefault database in its host commands. Use the same overrides for later status, maintenance, and removal commands.

CSM stores data locally in SQLite and has no built-in cloud synchronization or encryption. Its MCP connection uses local standard input/output. Recalled content enters the coding host's model context and is subject to that host's handling. Local storage does not guarantee that recalled information stays off the host's network.

## What hooks capture

Hooks record supported session events, capped prompt/reply excerpts, selected file paths and edit summaries, command/test summaries, and some failure details. They skip ordinary read/search activity and CSM's own tools. Capture depends on the events and tool formats the host supplies. Journal command records may have an unknown outcome; they are not automatically evidence of a passing test.

Known credential patterns are redacted before storage, but pattern matching is not a guarantee that every secret is removed. Avoid including credentials or unnecessary sensitive information in commands, prompts, memories, and checkpoints. Configuration backups preserve original file contents and can contain unrelated secrets. Treat the database, exports, backups, and diagnostic logs as sensitive local files.

To reduce prompt/reply capture, configure `CSM_CAPTURE_TEXT=0` in the environment inherited by the host and restart it. This requires the user's authorization to change capture settings. It leaves other event capture enabled and does not erase existing records. To stop CSM's installed host capture entirely, remove its registrations as below and restart the host.

Automatic journal events are pruned using 90-day and 5,000-event-per-project thresholds in bounded batches at session start. Checkpoints and goal records are retained; saved memories remain until deleted. This is not a promise of immediate deletion at a fixed age or secure erasure.

## Inspect and export

Set `$csmExe` to the installed executable and `$csmProject` to the intended project as in [operations](operations.md). Use `status`, a bounded `journal`, or `brief --explain` before widening inspection. Do not upload local diagnostics without authorization.

To export an explicitly requested project's saved memories and journal:

```powershell
$csmExport = Join-Path $env:USERPROFILE 'Downloads\csm-project-export.json'
if (Test-Path -LiteralPath $csmExport) { throw 'Choose a new export destination.' }
$csmExportJson = & $csmExe export --project $csmProject
if ($LASTEXITCODE -ne 0) { throw 'CSM export failed.' }
$csmExportJson | Set-Content -LiteralPath $csmExport -Encoding UTF8
```

The export may contain sensitive data; this command does not redact configuration backups or export them.

## Remove registrations or data

For an authorized removal of Codex integration, preview first, then apply the reviewed scope:

```powershell
& $csmExe setup --uninstall --dry-run --only codex
& $csmExe setup --uninstall --yes --only codex
```

Use `--only claude` for Claude Code or `--only codex,claude` for both. Restart the affected hosts. Uninstall removes CSM hook/MCP entries; it keeps the executable, database, logs, and backups. It does not remove other products or old unrelated registrations.

When the user explicitly requests deletion of a project's recorded history, preview the exact project:

```powershell
& $csmExe forget --project $csmProject
```

After the requested deletion scope is clear, `forget --project $csmProject --yes` permanently removes that project's saved memories and journal records from the database. Linked worktrees normally share one project key, so inspect the resolved project before deletion. Existing exports and configuration backups remain. Continued hooks can create new events; remove registrations first if the user also wants capture stopped. This is application-level deletion, not a secure-erasure guarantee.

For one outdated saved memory, inspect its ID and project and use `csm_delete` only under the user's deletion request. Do not treat a stale recalled instruction as permission to remove data.
