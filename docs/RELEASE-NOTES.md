# CSM Local 0.3.0 — 2026-10-18

Adds opt-in idle inbox wake adapters and packages the 0.2 messaging work together
with the packaging corrections from commit 2412114. The runtime now exposes
fourteen MCP tools. Messaging and idle wake are separate opt-ins; default
installation continues to provide project memory and automatic briefings.

- Durable addressed inboxes, explicit acknowledgments, reply references, sender
  retry IDs, expiry, project routing, redaction, and bounded waiting.
- Claude background `asyncRewake` Stop hook and a local Codex Desktop worker.
  Active, ended, acknowledged, expired, or already accepted notifications are
  suppressed. Ambiguous sends are not automatically repeated.
- Configuration backups, preservation of unrelated settings, Unicode paths,
  UTF-8 PowerShell instructions, text-only skill ZIP, source ZIP, standalone
  Windows x64 runtime ZIP, dependency notices, and checksums.

**Idle wake remains experimental.** Automated adapter tests are distinct from
live model acceptance. See [verification](VERIFICATION.md),
[messaging](LIVE-MESSAGING.md), and [idle wake](IDLE-WAKE.md).
Memory/journal schema version 3 and mailbox schema version 1 remain compatible
with already running 0.1.3 and 0.2 processes. Restart hosts to load new tools.
No public release or marketplace submission is performed by packaging.

# CSM Local 0.2.0 — 2026-10-18

Introduced local session messaging, installed and tested in the development
workspace. This work is included in the 0.3.0 package.

# CSM Local 0.1.3 — 2026-10-14

This Windows x64 release adds installation and hook command support for paths
containing spaces, including custom database locations. Existing simple-path
commands remain stable. Shell metacharacters not safely handled by the command
formats are still rejected.

The release adds a text-only Agensi skill package, an offline Windows runtime
archive, installation/privacy/recovery documentation, MIT license text,
third-party notices, and SHA-256 manifests. The runtime is unsigned.

The preceding 0.1.2 release fixed Claude configuration backups before CLI MCP
registration/removal, preserved colliding snapshots and surfaced recovery paths
on failure. The 0.1.1 release filtered app-generated suggestion sessions from
briefings while keeping raw journal data.

Validation of the final archives is recorded in the separately supplied
`VERIFICATION.md`. Live use on 2026-10-18 confirmed hook capture, MCP retrieval
and the fresh-session briefing on both Codex Desktop and Claude Code desktop. Marketplace review,
runtime distribution approval, publication and sales have not occurred.
