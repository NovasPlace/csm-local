# CSM Local 0.1.3

Private project memory and restart handoffs for coding agents.

CSM records a short local activity journal, stores decisions and lessons you ask
your agent to save, and supplies a compact briefing when a new session starts.
It is a local command-line executable with a seven-tool MCP server. It does not
host an AI model or require a CSM account, subscription, or API key.

## Supported release

- Windows x64 runtime. PowerShell is needed for the tested Codex hook path.
- Codex is the verified host: installation, hooks, MCP retrieval, and automatic
  checkpoint delivery in a fresh session have been exercised.
- Claude Code registration and hook-command tests are covered, including a fake
  Claude CLI. Live delivery in a fresh Claude session remains unverified.
- An existing compatible agent installation and its normal usage allowance are
  prerequisites. CSM does not supply Claude or Codex access.
- Default host configuration locations only: `%USERPROFILE%\.codex` and
  `%USERPROFILE%\.claude`, plus `%USERPROFILE%\.claude.json`.
- This release is unsigned. No macOS/Linux binary or Windows ARM64 build is
  included. Source code may have additional platform paths; they are not release
  compatibility claims.

## Install the downloaded Windows runtime

Extract `csm-local-0.1.3-windows-x64.zip` using Windows **Extract All**. Open
PowerShell inside the extracted folder containing `csm.exe`. You do not need
Rust, Node.js, Python, a database server, or administrator access.

```powershell
Get-FileHash -LiteralPath .\csm.exe -Algorithm SHA256
.\csm.exe --version
.\csm.exe setup --only codex --dry-run
.\csm.exe setup --only codex --yes
```

Compare the hash with the included `SHA256SUMS.txt`. A matching hash detects an
unexpected file change; it is not a digital signature.

Setup copies the executable to `%USERPROFILE%\.csm\bin\csm.exe` and registers
hooks/MCP at user scope. It backs up existing configuration files before changing
them. It does not add CSM to PATH. Use the full installed path:

```powershell
$csm = Join-Path $env:USERPROFILE '.csm\bin\csm.exe'
& $csm setup --status
```

Restart Codex, open `/hooks` if prompted, and approve the CSM hooks. Restarting an
already connected MCP server is necessary to load a new runtime version. Existing
simple-path hook commands are preserved by this upgrade.

For Claude Code's currently unverified live-session integration, substitute
`--only claude` in the setup commands. Omitting `--only` detects both hosts. The
installer prefers `claude.exe` on PATH for MCP registration; without that executable
it edits the user JSON config directly. Close Claude before using that fallback.

## Use project memory

Open your project folder first. CSM normally resolves the Git root; outside a Git
repository, the current working folder is the project key. Linked worktrees share
their main repository's key, while submodules are separate projects.

Ask your agent: "Save our decision to keep exports in CSV" or "Save a checkpoint
with what is finished, the remaining work, and any blockers." Important facts
are not automatically promoted out of the activity log.

You can also use the CLI from PowerShell:

```powershell
$csm = Join-Path $env:USERPROFILE '.csm\bin\csm.exe'
$project = (Get-Location).Path
& $csm checkpoint 'CSV export implemented; verification remains.' --next 'Run the export checks.' --project $project
& $csm brief --project $project
& $csm list --project $project
& $csm search 'CSV' --project $project
```

`list` and `search` query saved memories, not the automatic activity journal.
Use `journal` to inspect captured events. A briefing is a bounded selection of
records, not a full transcript or an authoritative description of current files.
Recheck old claims against the project before acting on them.

## Spaces, Unicode, and custom storage

The 0.1.3 installer supports spaces and Unicode in the runtime and database paths.
Some shell metacharacters remain deliberately rejected with an actionable error;
this is not support for every possible Windows filename. If needed, select a
simple data directory through `CSM_HOME` before installation.

```powershell
# Optional: choose storage before running setup. Use absolute paths.
$env:CSM_HOME = Join-Path $env:USERPROFILE 'CSM Local Data'
.\csm.exe --db (Join-Path $env:USERPROFILE 'Memory Data\csm.db') setup --only codex --dry-run
.\csm.exe --db (Join-Path $env:USERPROFILE 'Memory Data\csm.db') setup --only codex --yes
```

Custom database arguments are written into the hook and MCP registrations. When
running manual commands later, pass the same `--db` path (or set `CSM_DB`). If you
choose `CSM_HOME`, retain it for future setup/status/uninstall commands and use
its `bin\csm.exe`, rather than the default path shown elsewhere in this guide.

## Remove hooks and MCP registration

```powershell
$csm = Join-Path $env:USERPROFILE '.csm\bin\csm.exe'
& $csm setup --uninstall --only codex --dry-run
& $csm setup --uninstall --only codex --yes
```

Restart the host afterward. Uninstall intentionally leaves the runtime, database,
and backups. See [privacy and deletion](docs/PRIVACY.md) for data removal and
[recovery](docs/RECOVERY.md) for configuration restoration.

## Limits and distribution

This is the focused Rust CSM Local rebuild, not feature parity with the original
CSM/AgentBook platform. It uses keyword search, has no embeddings or cloud sync,
and does not import the original CSM database. Do not point it at an original CSM
database.

The Agensi skill archive contains text instructions only. This runtime archive
must be supplied separately through an agreed delivery channel. No Agensi
approval, paid listing, or hosted runtime download is implied by these files.
Release details are in [the release notes](docs/RELEASE-NOTES.md).

## Building from source

With Rust and the Windows MSVC build tools installed:

```powershell
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt -- --check
python scripts/build_release.py
```

Python 3.11+ is used only by the release scripts, not by the installed product.
The release build uses the explicit Windows x64 MSVC target with the C++ runtime
linked statically, so it does not require a separate Visual C++ runtime install.
Run `python scripts/package_release.py --help` for packaging instructions.

CSM Local uses the MIT license in [LICENSE](LICENSE). Dependency license texts are
included in the runtime archive's `notices/THIRD-PARTY-NOTICES.txt` and
`notices/RUST-STANDARD-LIBRARY-NOTICES.html`.
