# Install the companion runtime

The skill is text-only. Download [`csm-local-0.3.0-windows-x64.zip`](https://github.com/NovasPlace/csm-local/releases/download/v0.3.0/csm-local-0.3.0-windows-x64.zip) from the free public GitHub release. Checksums and source are available on the [release page](https://github.com/NovasPlace/csm-local/releases/tag/v0.3.0). Use the Agensi agent connection for the purchased skill; copying or exporting a paid skill is not required by this setup.

This release targets Windows x64 with Codex and Claude Code; both were verified in live use (Codex Desktop and Claude Code desktop). No claim is made about every host version or other operating systems. CSM itself needs no cloud account or API key. The coding host keeps its own account and usage requirements.

## Prepare and preview

Use the release's supplied `SHA256SUMS.txt` to check the runtime artifact. Extract the archive into a new local folder. It contains a top-level `csm-local-0.3.0-windows-x64` folder with `csm.exe`, documentation, `LICENSE`, and `notices/THIRD-PARTY-NOTICES.txt`. Adjust the example extraction path to the actual location. Do not run commands from an untrusted archive.

```powershell
$csmRuntimeDir = Join-Path $env:USERPROFILE 'Downloads\csm-local-0.3.0-windows-x64'
$csmRuntime = Join-Path $csmRuntimeDir 'csm.exe'
& $csmRuntime --version
& $csmRuntime setup --dry-run
```

Expect version `0.3.0`. Read the plan before applying it. Setup configures every supported host it finds: Codex (`~/.codex`) and Claude Code (`~/.claude`). Add `--only codex` or `--only claude` when the user wants just one. If a host the user expects is reported missing, start that host once so it creates its configuration folder, then rerun the preview. Setup discovers the host's user configuration directory. Version 0.1.3 supports installation/database paths containing spaces and Unicode. Some shell metacharacters and control characters remain unsupported; resolve any reported path issue before applying. A user-selected absolute `CSM_HOME`, such as `C:\csm`, is supported, but changing that location changes the installation and data paths.

For a default installation, setup copies the executable to `%USERPROFILE%\.csm\bin\csm.exe`. For Codex it registers five hooks in `~/.codex/hooks.json` and adds the `csm` MCP entry to `~/.codex/config.toml`. For Claude Code it registers six hooks in `~/.claude/settings.json` and the `csm` MCP server through the Claude CLI when available, with a configuration-file fallback; close Claude before that direct-file fallback is used. Existing configuration files that it changes are backed up under `~/.csm/backups/`, including `~/.claude.json` before CLI registration. Rerunning setup is intended to leave matching entries unchanged. Review the plan for the actual paths if environment overrides are used.

## Apply the reviewed plan

Apply only when installation/configuration changes are covered by the user's request. Existing explicit authorization is sufficient.

```powershell
& $csmRuntime setup --yes
```

Use the same `--only` scope as the reviewed preview. Restart each configured host. In Codex, use `/hooks` to review and trust the CSM hooks when Codex requests approval. Do not bypass the host's trust controls. Open a fresh chat in the intended project after restarting. The setup command does not add CSM to `PATH`.

For default locations:

```powershell
$csmExe = Join-Path $env:USERPROFILE '.csm\bin\csm.exe'
& $csmExe --version
& $csmExe setup --status
```

Registration status verifies configuration, not model receipt of a briefing. Use normal project work to observe capture and a later fresh chat in that same project to assess automatic recall. Explicit `csm_brief` retrieval proves retrieval only. A hook's zero exit code also does not establish success: hooks suppress failures so they do not interrupt the host.


## Upgrade or diagnose

For upgrades, invoke setup from the newly extracted release executable. Restart hosts afterward so running MCP processes load the new version. On Windows, an active executable can leave an old renamed copy during replacement; check the installed version and reported setup result.

On failure, stop after inspecting the specific error, setup status, and relevant local `hook-errors.log`. Avoid dumping full configuration, backups, or logs into a public conversation. Keep any reported backup path for recovery; do not overwrite live configuration wholesale while a host is changing it. See [privacy and removal](privacy.md).
