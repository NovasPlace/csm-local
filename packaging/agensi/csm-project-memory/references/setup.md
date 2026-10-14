# Install the companion runtime

The skill is text-only. Obtain `csm-local-0.1.3-windows-x64.zip` separately from the seller through the agreed delivery channel. Use the Agensi agent connection for the purchased skill; copying or exporting a paid skill is not required by this setup.

This release targets Windows x64 and Codex. Claude Code registration is implemented and tested in isolation; live Claude fresh-session verification is pending. No claim is made about every host version or other operating systems. CSM itself needs no cloud account or API key. The coding host keeps its own account and usage requirements.

## Prepare and preview

Use the release's supplied `SHA256SUMS.txt` to check the runtime artifact. Extract the archive into a new local folder. It contains a top-level `csm-local-0.1.3-windows-x64` folder with `csm.exe`, documentation, `LICENSE`, and `notices/THIRD-PARTY-NOTICES.txt`. Adjust the example extraction path to the actual location. Do not run commands from an untrusted archive.

```powershell
$csmRuntimeDir = Join-Path $env:USERPROFILE 'Downloads\csm-local-0.1.3-windows-x64'
$csmRuntime = Join-Path $csmRuntimeDir 'csm.exe'
& $csmRuntime --version
& $csmRuntime setup --dry-run --only codex
```

Expect version `0.1.3`. Read the plan before applying it. If it reports Codex is missing, initialize Codex normally and rerun the preview. Setup discovers the host's user configuration directory. Version 0.1.3 supports installation/database paths containing spaces and Unicode. Some shell metacharacters and control characters remain unsupported; resolve any reported path issue before applying. A user-selected absolute `CSM_HOME`, such as `C:\csm`, is supported, but changing that location changes the installation and data paths.

For a default installation, setup copies the executable to `%USERPROFILE%\.csm\bin\csm.exe`, registers five Codex hooks in `~/.codex/hooks.json`, and adds the `csm` MCP entry to `~/.codex/config.toml`. Existing configuration files that it changes are backed up under `~/.csm/backups/`. Rerunning setup is intended to leave matching entries unchanged. Review the plan for the actual paths if environment overrides are used.

## Apply the reviewed plan

Apply only when installation/configuration changes are covered by the user's request. Existing explicit authorization is sufficient.

```powershell
& $csmRuntime setup --yes --only codex
```

Restart Codex, then use `/hooks` to review and trust the CSM hooks when Codex requests approval. Do not bypass the host's trust controls. Open a fresh chat in the intended project after restarting. The setup command does not add CSM to `PATH`.

For default locations:

```powershell
$csmExe = Join-Path $env:USERPROFILE '.csm\bin\csm.exe'
& $csmExe --version
& $csmExe setup --status
```

Registration status verifies configuration, not model receipt of a briefing. Use normal project work to observe capture and a later fresh chat in that same project to assess automatic recall. Explicit `csm_brief` retrieval proves retrieval only. A hook's zero exit code also does not establish success: hooks suppress failures so they do not interrupt the host.

## Optional Claude Code preview

If the user requests Claude integration, preview with `setup --dry-run --only claude`, then apply with `setup --yes --only claude`. Setup registers six hooks in `~/.claude/settings.json` and the `csm` MCP server through the Claude CLI when available, with a configuration-file fallback. Close Claude before using that direct-file fallback. It snapshots existing configuration before changing it, including `~/.claude.json` before CLI registration. Restart Claude Code afterward. Keep the pending live verification limitation clear.

To include both hosts in an authorized setup, use `--only codex,claude`.

## Upgrade or diagnose

For upgrades, invoke setup from the newly extracted release executable. Restart hosts afterward so running MCP processes load the new version. On Windows, an active executable can leave an old renamed copy during replacement; check the installed version and reported setup result.

On failure, stop after inspecting the specific error, setup status, and relevant local `hook-errors.log`. Avoid dumping full configuration, backups, or logs into a public conversation. Keep any reported backup path for recovery; do not overwrite live configuration wholesale while a host is changing it. See [privacy and removal](privacy.md).
