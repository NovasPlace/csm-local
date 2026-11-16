# CSM Local

Private project memory and coordination for coding agents.

CSM gives Claude Code and Codex sessions a memory that stays on your computer. It
records a short local activity journal, stores what your agents save, and
supplies a compact briefing when a session starts, resumes or compacts. Sessions
working in the same project see each other's edits, file claims and your standing
decisions, and can message each other. It is a local command-line executable with
an MCP server. It does not host an AI model or require a CSM account,
subscription, or API key.

**Versions.** The download below is release 0.3.0. This branch is 0.4.0-dev and
does more than the release: [What it does](#what-it-does) marks each part added
since 0.3.0. To use those parts before the next release,
[build from source](#building-from-source).

## Download

[Download CSM 0.3.0 for Windows x64](https://github.com/NovasPlace/csm-local/releases/download/v0.3.0/csm-local-0.3.0-windows-x64.zip)

[All release files and SHA-256 checksums](https://github.com/NovasPlace/csm-local/releases/tag/v0.3.0) ·
[Source repository](https://github.com/NovasPlace/csm-local)

The free MIT-licensed runtime is unsigned and was tested on Windows 11 x64.
Extract the ZIP, then follow the installation steps below. Your existing Claude
Code or Codex account is required; CSM does not supply model access.

## What it does

Everything below works through the host's hooks and CSM's MCP tools. Parts marked
**since 0.3.0** are on this branch and are not in the 0.3.0 download.

### Activity journal

Hooks record what each session does: your requests and the replies, file edits,
commands and test runs with their outcome, and session starts, ends and
compactions. Obvious credentials are redacted before anything is stored. Tool
events are kept for 90 days, up to 5,000 per project; requests and replies are
kept for 90 days. `csm journal` shows it.

Each record carries the model that wrote it, when CSM knows it. The model is read
again at every hook event, so a model switched mid-session is picked up, and a
subagent is recorded with its own model. *Since 0.3.0.*

### Session briefing

A new session starts with a briefing of at most 4,000 bytes (`CSM_BRIEF_BYTES`,
from 1,000 to 7,000). In order, it holds:

- the project card (*since 0.3.0*)
- the project's current goal
- your standing decisions (*since 0.3.0*)
- the last checkpoint, with its next steps and blockers
- the last failed command or test that has not passed since
- results you said were not so (*since 0.3.0*)
- files that running sessions have claimed (*since 0.3.0*)
- the previous session: its last request and reply, the files it changed and its
  last commands
- saved memories marked as decisions, lessons or important
- recent changes by other sessions, including sessions started in other folders
  (*since 0.3.0*)

When room is short the least important sections go first. The card, the goal,
the decisions and the checkpoint are kept, and a long checkpoint is shortened
before a decision is dropped.

After a compaction or a resume, the briefing describes the session itself in
place of the one before it: its last request and reply, the project files it
changed, up to two earlier requests and its last commands. That part has up to
1,500 bytes of its own on top of the budget, so a full decision board cannot
crowd it out. *Since 0.3.0.*

`csm brief` and `csm_brief` show the briefing on request.

### Saved memories

Agents save notes, lessons, decisions and facts with `csm_save`. `csm_search`
(keyword search), `csm_list` and `csm_delete` manage them, and the same text is
not saved twice.

Since 0.3.0 a session no longer has to search for them:

- **Project card.** One memory of kind `card` per project, at most 1,000 bytes,
  says what the project is, where things live and how to build, test and run it.
  Every briefing opens with it, and saving a new one replaces the last.
- **Recall on a request.** When you type a request, up to three saved memories
  that match it are shown to the agent. A match shares at least two words that
  the memory repeats or opens with, one of them particular to the project.
- **Recall before an edit.** Before an agent edits a file inside the project, up
  to three memories that name that file are shown.
- **Once each.** A memory is shown once per session, not when the briefing
  already showed it, and may come up again after a compaction. `CSM_RECALL=0`
  turns recall off.
- **Save nudge.** A session that has taken four requests and changed code since
  it last saved a memory, a checkpoint or a decision is sent back once, at the
  end of a turn, to save what a later session could not see in the code, or to
  say there is nothing. `CSM_SAVE_NUDGE=0` turns it off.

### Checkpoints and goals

`csm_checkpoint` records where the work stands for the next session: a summary,
up to five next steps and up to five blockers. It can also set or close the
project's goal. On this branch a summary may be 1,500 bytes (0.3.0 kept the
first 500), and anything over a limit is refused with its size, not cut.

### Standing decisions

*Since 0.3.0.* A decision you tell one session reaches every session in the
project. `csm_decide` puts it on the project's board. Each session's briefing
lists the board, and a running session is told about a new or withdrawn decision
on its next prompt or tool call, once. Entries are at most 300 bytes, are never
edited in place, and say who recorded them. Withdrawing takes a decision back or
replaces it. `csm_decisions` lists the whole board.

### Earlier requests and reported results

*Since 0.3.0.*

- **Related earlier requests.** When you type a request of three words or more,
  the agent is shown up to three earlier requests from other sessions in the
  project that share its rarer words, each with the reply it got and what you
  said next. Each is shown to a session once. `CSM_RECALL=0` turns it off.
- **Results you said were not so.** A reply that claimed a result and that you
  answered with "not fixed" or the like is listed in other sessions' briefings
  for 14 days.
- **Claim check.** When a reply reports a result after a code change and nothing
  ran after the last change, or the claim resembles one you pushed back on in
  the last 30 days, the agent is sent back once per request to say what it
  verified. `CSM_CLAIM_CHECK=0` turns it off.

### Working in parallel

*Since 0.3.0.*

- **Collision warnings.** Before an agent edits a file that another running
  session edited in the last 30 minutes, it is told who edited it, how long ago,
  what that session was last asked, and how to message it when it has an
  address. Once per file and session in that window.
- **File claims.** `csm_claim` holds files for 1 to 240 minutes. Claims show on
  their holder in `csm_peers`, in briefings, and in the warning before another
  session edits a claimed file. They end with `csm_release`, at expiry, or when
  the session ends.
- **Advisory only.** Nothing is locked and no edit is blocked. A warning arrives
  with the edit it is about.
- **Edits from other folders.** A briefing lists edits to the project's files by
  sessions started in other folders. `csm link FOLDER --into PROJECT --yes`
  makes sessions started in FOLDER count as PROJECT and moves FOLDER's history
  there; without `--yes` it only shows what would move. `csm unlink` stops the
  counting.

### Subagents

*Since 0.3.0, on Claude Code.* Subagents share their session's ID and MCP server,
so CSM tells them apart through the hooks.

- **Decisions.** A subagent starts without its parent's briefing. Its first
  hooked tool call shows it the whole decision board, and later changes reach it
  once each, tracked apart from its parent.
- **Collisions between siblings.** A subagent about to edit a file that a sibling
  subagent of the same session edited in the last 30 minutes is warned.
- **Claims of its own.** A claim a subagent makes is its own. Its siblings are
  warned about it, it pauses when the subagent stops and holds again if the
  subagent resumes, and a sibling's re-claim cannot take it over, shorten it or
  change its note.
- **No noise within a session.** A session and its own subagents are never warned
  about each other, since the session handed that work out. Other sessions see a
  subagent's edits and claims as its session's.
- **Memories before an edit.** A subagent is shown the memories that name the
  file it is about to edit, once per subagent.
- **Its own model.** A subagent's records carry the model it ran on.

The session briefing, recall on a request, the claim check and the save nudge
apply to the main agent only: a subagent's prompt comes from its parent, and
only the main agent answers you.

### Messaging and idle wake

Optional messaging lets authorized Claude Code and Codex sessions discover each
other, exchange addressed messages, and acknowledge receipt. Enable it with
`.\csm.exe setup --messaging --yes`, then reload the hosts. See
[delivery and coordination](docs/LIVE-MESSAGING.md). Since 0.3.0, `csm_peers`
lists the sessions seen in the last day that have not ended, each with its last
request, its current model and the files it has claimed.

Version 0.3 also includes **experimental, opt-in idle wake-up**. Claude uses its
native background hook; Codex Desktop uses a local app connection. Ordinary
installs leave wake-up off. Open chats and a compatible host are required;
closed hosts keep mail queued. See [idle wake setup and limits](docs/IDLE-WAKE.md)
and [what was verified](docs/VERIFICATION.md). Since 0.3.0, a waiting Claude wake
hook stops once its host has gone, including on Windows, where the shell that
started it can outlive the host.

### Privacy and controls

Everything is stored in one SQLite database under `%USERPROFILE%\.csm`. CSM
syncs nothing and sends nothing anywhere. What it shows an agent does become part
of that agent's context, so it reaches the agent's model provider like any other
prompt text.

| Setting | Effect |
|---|---|
| `CSM_CAPTURE_TEXT=0` | Requests and replies are recorded without their words |
| `CSM_RECALL=0` | No related earlier requests and no memory recall |
| `CSM_CLAIM_CHECK=0` | No claim check |
| `CSM_SAVE_NUDGE=0` | No save nudge |
| `CSM_BRIEF_BYTES` | Briefing size, 1,000 to 7,000 bytes |

Set them in the environment the agent inherits, then restart it. `csm export`
writes a project's memories and journal as JSON, and `csm forget` permanently
deletes a project's records. See [privacy and deletion](docs/PRIVACY.md).

### Tools and commands

The MCP server on this branch has eighteen tools (fourteen in 0.3.0):
`csm_brief`, `csm_search`, `csm_save`, `csm_list`, `csm_delete`,
`csm_checkpoint`, `csm_status`, `csm_decide`, `csm_decisions`, `csm_claim`,
`csm_release`, `csm_connect`, `csm_peers`, `csm_send`, `csm_inbox`, `csm_ack`,
`csm_message_status` and `csm_wait`.

The command line has the same operations and a few of its own: `save`, `search`,
`list`, `delete`, `brief`, `journal`, `checkpoint`, `decide`, `decisions`,
`status`, `export`, `forget`, `peers`, `send`, `inbox`, `ack`, `message-status`,
`wait`, `claim`, `release`, `wake`, `link`, `unlink` and `setup`.

### What has been tried

The parts added since 0.3.0 are covered by the test suite (`cargo test`). In live
Claude Code sessions these have run: briefings with the project card and the
resumed-session part, collision and claim warnings (including between sibling
subagents), related earlier requests, memory recall, the claim check and model
stamps. The save nudge has run against test and replayed input only. Under
Codex, memory recall, the project card, the save nudge and the resumed-session
briefing have been run through the installed hooks with Codex-shaped input. None
of the parts added since 0.3.0 has been seen in a live Codex session yet. The
statements under [Supported release](#supported-release) are about 0.3.0.


## Supported release

- Windows x64 runtime. PowerShell is needed for the tested Codex hook path.
- Codex and Claude Code are both verified hosts. On each, installation, hook
  capture, MCP retrieval, and the automatic briefing in a fresh session have been
  exercised in live use (Claude Code desktop and Codex Desktop, 2026-10-18).
- An existing compatible agent installation and its normal usage allowance are
  prerequisites. CSM does not supply Claude or Codex access.
- Default host configuration locations only: `%USERPROFILE%\.codex` and
  `%USERPROFILE%\.claude`, plus `%USERPROFILE%\.claude.json`.
- This release is unsigned. No macOS/Linux binary or Windows ARM64 build is
  included. Source code may have additional platform paths; they are not release
  compatibility claims.

## Install the downloaded Windows runtime

Extract `csm-local-0.3.0-windows-x64.zip` using Windows **Extract All**. Open
PowerShell inside the extracted folder containing `csm.exe`. You do not need
Rust, Node.js, Python, a database server, or administrator access.

```powershell
Get-FileHash -LiteralPath .\csm.exe -Algorithm SHA256
.\csm.exe --version
.\csm.exe setup --dry-run
.\csm.exe setup --yes
```

Setup finds Codex and Claude Code automatically and configures each one present.
Add `--only codex` or `--only claude` to limit it to one host.

Compare the hash with the included `SHA256SUMS.txt`. A matching hash detects an
unexpected file change; it is not a digital signature.

Setup copies the executable to `%USERPROFILE%\.csm\bin\csm.exe` and registers
hooks/MCP at user scope. It backs up existing configuration files before changing
them. It does not add CSM to PATH. Use the full installed path:

```powershell
$csm = Join-Path $env:USERPROFILE '.csm\bin\csm.exe'
& $csm setup --status
```

Restart Codex and Claude Code. In Codex, open `/hooks` if prompted and approve the
CSM hooks. Restarting an already connected MCP server is necessary to load a new
runtime version. Existing simple-path hook commands are preserved by this upgrade.

For Claude Code, the installer prefers `claude.exe` on PATH for MCP registration;
without that executable it edits the user JSON config directly. Close Claude
before using that fallback.

## Use project memory

Open your project folder first. CSM normally resolves the Git root; outside a Git
repository, the current working folder is the project key. Linked worktrees share
their main repository's key, while submodules are separate projects.

Ask your agent: "Save our decision to keep exports in CSV" or "Save a checkpoint
with what is finished, the remaining work, and any blockers." Important facts
are not automatically promoted out of the activity log. On this branch a session
that has worked a while without saving anything is asked once to do so (the
save nudge above); what it saves is still its own choice.

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

The Agensi skill archive contains text instructions only. Download the companion
runtime separately from the public GitHub release linked above. GitHub hosting
does not imply Agensi approval or a paid marketplace listing.
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

To use this branch without packaging a release, build it and run setup from the
build. It reports version 0.4.0-dev. Where CSM is already set up, setup replaces
the installed runtime and leaves the hook and MCP registrations as they are.
Restart the hosts afterward so their MCP servers load the new build.

```powershell
cargo build --release
.\target\release\csm.exe --version
.\target\release\csm.exe setup --dry-run
.\target\release\csm.exe setup --yes
```

CSM Local uses the MIT license in [LICENSE](LICENSE). Dependency license texts are
included in the runtime archive's `notices/THIRD-PARTY-NOTICES.txt` and
`notices/RUST-STANDARD-LIBRARY-NOTICES.html`.
