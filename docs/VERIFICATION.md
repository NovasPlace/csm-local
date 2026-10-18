# CSM 0.3.0 verification — 2026-10-18

Automated source tests: 175 passed, zero failed. Clippy with warnings denied
and formatting checks passed. The Windows x64 MSVC runtime uses a static CRT;
PE imports are checked for standalone deployment.

The package verifier exercises the extracted executable with isolated Unicode
and space-containing Windows paths: dry-run, install, backups, status,
idempotence, fourteen MCP tools, separate-process messaging, retries,
acknowledgment, native background-hook signaling, worker lifecycle, disable,
and uninstall preserving unrelated settings and saved data. The extracted-package
checks pass. See the external VERIFICATION.json shipped beside the archives for
the exact assertion count and hashes; lifecycle polling can vary that count.

The Codex named-pipe contract suite covers idle acceptance, busy deferral,
missing tools, oversized frames, and lost send receipts. The fake host never
invokes a model. A real read-only Codex Desktop probe deferred this active
chat without submitting a message.

Core capture, automatic briefings, and MCP retrieval have been observed on
Codex Desktop and Claude Code desktop. This does not establish every host
version. A Claude desktop chat was confirmed idle with an armed watcher on the
development 0.3 build.
A real message triggered its native background hook; the recipient read it,
explicitly acknowledged it, and replied that no new human prompt was involved.
The earlier isolated CLI fixture failed authentication; that failure is retained
separately. A later real Claude-to-Codex test also passed: a message to this idle
Codex chat triggered the native app wake, and this same chat read and explicitly
acknowledged it without a new human prompt. An earlier synthetic self-test timed
out with an uncertain receipt and is retained as unsuccessful. It was not
replayed automatically. One successful wake does not establish reliability
across host versions or recovery from every app delay.

Idle wake is experimental, off by default, and version-dependent. Windows 11
x64 is the tested operating system. Other Windows setups, ARM64, macOS,
Linux and marketplace acceptance are not covered. The package verifier does not
publish or contact GitHub; the separate upload check verifies hosted asset
hashes and public downloads. No private database or user settings are included
in the archives.

Runtime SHA-256: `69d825e639a2a3c4cd3153323cf062aa6b5a80f16a7b1e98b508f6fbee39fba3`.
