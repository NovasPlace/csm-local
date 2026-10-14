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
`VERIFICATION.md`. Claude's live fresh-session delivery is still pending; fake
CLI and shell tests do not establish that live integration. Marketplace review,
runtime distribution approval, publication and sales have not occurred.
