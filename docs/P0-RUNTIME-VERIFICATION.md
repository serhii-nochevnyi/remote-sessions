# P0 Rust/tmux lifecycle verification

Date: 2026-11-21. Scope: local runtime/service-lifetime slice, not all of P0.

## Result

A real controlled process launched through the Rust host into tmux survived client detach, abrupt client death, graceful agent shutdown/restart and forced launchd restart. A fresh ordinary PTY attachment displayed ongoing output. Separate service lifetime is demonstrated by actual launchd jobs, not only by separate binaries spawned from one parent.

The [recorded run](P0-RUNTIME-EVIDENCE.json) identifies:

- tmux 3.8, server PID 57727;
- pane PID 57759, unchanged process start time and random process-instance token;
- agent PIDs 57729 → 57795 → 57832;
- continuous probe counter 0 → 11 without restarting the process.

All ten integration assertions passed: private IPC/singleton/bounds, real creation/duplicate rejection, two read-only viewers/no input or resize authority, detach/client loss, SIGTERM restart, forced restart, fresh reattachment, targeted stop/precondition failure, natural exit, and fail-closed behavior when tmux is unavailable.

## Other verification

- Seven Rust unit/contract/HTTP tests passed.
- Rust formatting and Clippy with warnings denied passed.
- Tauri macOS compile passed after the shared-protocol migration.
- TypeScript checks, three frontend tests and production build passed. Existing terminal bundle-size advisory remains.
- Rust HTTP server live `/healthz` smoke and SIGTERM shutdown passed.
- Relative Markdown links verified.

## Environment and cleanup

Project, Git and local toolchains are on Kingston. tmux was installed using Homebrew core; Homebrew also installed/upgraded its required dependencies. No tap trust or OS security settings were changed.

Direct launchd execution from the removable drive stalled before application entry. The test instead installed a disposable copy of the built agent/config in a private temporary directory. This resembles installing service payloads separately from a development checkout and avoids depending on removable-volume launch access. Full product installation and workspace-access permissions remain separate work.

The successful test unloaded both launchd jobs, stopped its synthetic processes and removed the test directory. No login-startup registration, remote deployment, Git commit or remote was created.

## Limits

This was a real deterministic PTY process plus a separate shell runtime. It is not evidence of Claude/Codex interactive UI compatibility or an iPhone test. The client used for attachment is the local CLI; the React/Tauri UI still displays samples. Input ownership, request deduplication after uncertainty, durable metadata, signed installation, remote transport and E2EE are not implemented. Read-only tmux flags are a local spike safeguard, not a substitute for host authorization.

Reproduce with `make check-runtime` after the [toolchain setup](DEVELOPMENT.md). Manual commands are in the [runtime lab guide](RUNTIME-LAB.md).
