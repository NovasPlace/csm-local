# Setup, upgrade and recovery

Setup edits user-scoped settings and registers only CSM's hooks and MCP server.
It backs up files that already exist under `<CSM data directory>\backups\<time>`.
Absent original files have no synthetic backup. Snapshot names include:

- `claude-settings.json` for `.claude\settings.json`
- `claude.json` for `.claude.json`
- `codex-hooks.json` for `.codex\hooks.json`
- `codex-config.toml` for `.codex\config.toml`

Repeated changes sharing a timestamp receive numbered backup names; earlier
snapshots are not overwritten. For Claude CLI registration, the latest
`.claude.json` is backed up just before its first mutating command. A backup
failure stops that CLI call. A CLI failure reports the retained snapshot's path.
Setup is not a transaction across all files: earlier successful steps can remain
applied when a later step fails. Start by reading `setup --status` and a fresh
`setup --dry-run` rather than assuming every step was rolled back.

## Restore an agent configuration

Close the affected agent first. Compare the backup with its current configuration
and preserve the current file separately. Restore the relevant CSM entries or
the entire snapshot only when that will not discard unrelated later changes.
CSM does not automatically overwrite the whole file after a registration failure,
because Claude may have made concurrent updates. Restart the host after recovery.

For initial setup problems, `setup --status` checks registration, while
`status --project <path>` checks the database and records. Hook errors are written
to `hook-errors.log` beside the selected database. Hooks exit cleanly rather than
blocking agent work, so an empty briefing alone is not proof of successful setup.

## Verify a new session

Save an explicit checkpoint in a small test project, close its session, and open
a fresh session in exactly the same project folder. Ask what the last checkpoint
said without pasting its content. Confirm whether it arrived at startup or was
retrieved through a tool. A new projectless chat normally has a new folder and
therefore a different project key. Use an explicit original project path for a
cross-project recall request.

If no checkpoint appears: check the project path, installed version, hook trust,
hook error log, environment settings, and whether the host was fully restarted.
For an MCP-only connection, request `csm_brief`; automatic startup delivery
requires the host hooks as well as MCP registration.

## Back up or restore memory

Do not copy only `csm.db` while writers are active; SQLite can have live `-wal`
and `-shm` sidecar files. Stop all connected agents/CSM processes and copy the
whole data directory to a private destination, or use SQLite's online backup API.
For a custom database, include its directory/sidecars separately. Restore with
all connections closed, and keep the replaced directory until you confirm the
restored database works. Config snapshots are separate from database backups.

## Upgrade

Keep a copy of the previous runtime archive. Run setup from the newly extracted
`csm.exe`, not from the old installed copy. On Windows, an in-use executable can
be renamed aside while the new version is installed. Existing MCP processes
continue running the previous version until restarted. Verify the installed
`--version`, `setup --status`, and a fresh-session handoff after an upgrade.
