# Agensi submission draft — CSM Project Memory

Status: prepared locally for review. Not submitted or approved. No price or
publishing decision has been made.

## Listing title

CSM Project Memory — Private Checkpoints for Codex on Windows

## Short description

Save project decisions and progress checkpoints, then resume work in a fresh
Codex session using local memory. Includes a guided workflow for setup, recall,
verification and recovery with the CSM Local runtime.

## Description

Stop reconstructing the same project handoff after every new session. This skill
helps your coding agent keep useful facts separate from an activity log, record
what is finished and what remains, and check a saved handoff before continuing.

CSM Local stores records in a local SQLite database. It needs no CSM cloud
account, database service or separate API key. The agent you already use still
needs its normal access and allowance. Recalled context is sent to that agent;
local storage is not a promise that its model provider never receives it.

Included workflow: inspect setup; select the correct project; recall prior
context; save user-authorized decisions; capture checkpoints; verify fresh-session
delivery; inspect/delete project records and recover configuration backups.

Requirements: Windows x64, PowerShell, a compatible Codex installation, and the
separately supplied CSM Local 0.1.3 runtime. The runtime is unsigned. This listing
does not promise Claude live-session compatibility, other operating systems,
semantic/vector search, full transcripts, or migration from the original CSM.

Suggested category: Agents & Orchestration or Productivity.
Suggested tags: codex, windows, project-memory, checkpoints, local-first, mcp.

## Runtime delivery decision before a paid listing

The submission ZIP contains only text. The companion Windows runtime ZIP is
prepared separately; do not place its executable inside the skill upload.
No hosted download URL has been invented and no repository/release has been
published. Confirm with Agensi how the buyer obtains the companion runtime
without another purchase, signup or entitlement step. If that cannot be provided,
do not claim immediate self-contained paid delivery. A clearly disclosed free
runtime-linked listing is another route described in the supplied seller email.

The seller must decide price and publish scope, complete the marketplace's seller
onboarding/payout requirements, and obtain review. The local tests do not replace
Agensi's approval or demonstrate likely sales.

## Draft delivery question for the existing Agensi conversation

"We've prepared a focused Windows/Codex continuity skill and a standalone local
runtime. The skill ZIP is text-only; the runtime is a separate Windows ZIP with
checksums, setup instructions and license notices. What is your approved way to
deliver that runtime to a paid buyer alongside the skill, with no separate
purchase or service access requirement?"

This is a draft only; no message has been sent.

## Rules checked on 2026-10-14

- https://www.agensi.io/security — text skill screening; compiled files may be
  flagged or blocked.
- https://www.agensi.io/learn/skill-md-creator-checklist — one top-level skill
  folder, valid frontmatter, supporting references, no symlinks or binaries.
- https://www.agensi.io/sell — listing/review process and seller requirements.

The supplied Joeri Brons email allows a local runtime and asks a paid listing to
deliver its promised functionality without separate external-service access.
