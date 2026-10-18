# Agensi listing: CSM Project Memory 0.3.0

Local review package. No submission, public download, or message to Agensi has
been made. The 0.1.3 release is preserved. Choose runtime hosting and price
before publishing; insert the verified download link and rebuild afterward.

## Listing fields

**Title**
CSM: Private Memory for Claude Code and Codex

**Skill name**
`csm-project-memory`

**Tagline**
Your coding agents remember where you left off, on your own machine.

**Category**
Agents & Orchestration (confirm against the submission form)

**Tags**
claude-code, codex, memory, mcp, hooks, handoff, local-first, windows

## Description

Keep project decisions and work history across coding sessions. CSM records a
short local activity journal and gives a new session a compact briefing of
previous work, checkpoints, unresolved failures, and saved decisions. Claude
Code and Codex share the same project memory.

- Automatic capture through host hooks; important decisions and lessons are
  saved explicitly when requested.
- Checkpoints with next steps and blockers, available through MCP tools.
- Keyword search and a local SQLite database, without a CSM account or API key.
- Optional addressed messages between authorized local sessions, with explicit
  acknowledgments and retry IDs.
- Installation preview, configuration backups, status checks, and uninstall.

This skill contains instructions. The companion MIT-licensed runtime is a
separate Windows x64 download. Extract it, check the supplied checksum, preview
`setup --dry-run`, then run `setup --yes`. Setup does not add CSM to PATH; use
`.\csm.exe` in the extracted folder or the full installed path.

**Requirements:** Windows x64, tested on Windows 11; a compatible Claude Code
and/or Codex installation, using your existing account and usage allowance.
No Rust, Python, Node.js, database server, or administrator access is needed
to run the supplied executable. The runtime is unsigned.

**Runtime download:** [insert the agreed public download link before publishing].

**Limits:** Search is keyword-based. Capture stores bounded excerpts rather
than full transcripts. Credential redaction uses patterns and is not a
secrecy guarantee. Treat your database as private. Briefings and messages sent
to your coding agent reach that host's model provider under its policies.

Idle wake-up is an **experimental optional feature**, off by default, for
compatible open hosts. Claude requires its native background hook; Codex
Desktop requires a version-dependent local app connection. Closed hosts retain
queued messages. Automated adapter checks do not establish live idle pickup;
see the package verification report. Do not list autonomous idle coordination
as an established core product guarantee.

## Images

Use these from `listing-images/`. Headlines are editorial; terminal panels
are actual release output from isolated demo data. Temporary roots are mapped
to a buyer-style `C:/Users/dev` profile before saving text or rendering images.
A checkpoint is saved through MCP; no real model is used for these fixtures.

| File | Alt text |
| --- | --- |
| `1-cover.png` | A compact CSM project briefing for the next coding session. |
| `2-briefing.png` | Codex startup receives a demo checkpoint saved through MCP. |
| `3-setup-dry-run.png` | The release executable previews configuration changes for both hosts. |
| `4-install.png` | Isolated setup installs with backups, then status reports registrations. |

## Local delivery files

- `csm-project-memory-0.3.0-agensi.zip`: text-only skill upload.
- `csm-local-0.3.0-windows-x64.zip`: companion executable, documentation,
  MIT license, third-party notices, and internal checksums.
- `csm-local-0.3.0-source.zip`: complete source and tests, without private data.
- `SHA256SUMS.txt`, `release-manifest.json`, and `VERIFICATION.json`:
  archive hashes, build metadata, and the extracted-artifact verification.

All 175 source tests, extracted-package checks, and five simulated Codex
app-connection cases pass. See VERIFICATION.json for the package assertion
count. Core memory capture/briefings have been observed on both hosts. Real
idle Claude and Codex desktop chats each woke and explicitly acknowledged a
test message without a new human prompt in the receiving chat. The Codex test
used a real Claude sender. An earlier synthetic self-test timed out and is
retained as unsuccessful; wake remains experimental and version-dependent.
Native host receipts are distinct from automated package assertions.

## Before submitting

1. Choose where to host the companion runtime. No hosting is created by this
   package. Add the download link to this listing and the skill setup reference.
2. Choose free or paid. The earlier draft records an unresolved question about
   paid listings depending on a separate free runtime; confirm the current
   marketplace rules before making a paid offer. No price is assumed here.
3. Rebuild into a fresh directory after final edits, run the release verifier,
   and inspect the final listing images. Submit the text-only skill only after
   Donovan authorizes publication.

The existing 0.1.3 pricing discussion and draft correspondence remain in
commit 2412114; they are not sent or treated as current marketplace approval.
