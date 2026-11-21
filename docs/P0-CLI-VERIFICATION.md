# P0 real CLI terminal compatibility

Date: 2026-11-21. Result: **the requested display, resize and agent-restart reconnection checks passed for both installed CLIs**.

| Check | Claude Code 2.1.330 | Codex CLI 0.171.0 |
| --- | --- | --- |
| Actual interactive CLI launched through Rust host/tmux | Passed | Passed |
| Ordinary read-only PTY attachment decoded by xterm 6 | Passed | Passed |
| 120×32 → 40×18 → 100×30 runtime resize | Passed | Passed |
| Visible `P0_DRAFT Привіт 漢字` after every resize | Passed | Passed |
| Fresh viewer after forced launchd agent restart | Passed | Passed |
| Same runtime/session IDs, PID and process start time | Passed | Passed |
| Further unsubmitted input accepted after restart | Passed | Passed |
| Application's own alternate-screen mode | Inline in this run | Alternate in this run |

[Machine-readable evidence](P0-CLI-EVIDENCE.json) records exact identities and sizes. Agent PID changed 15532 → 16216; Claude remained PID 15551 and Codex remained PID 15849. No model prompt was submitted. Both real CLI processes and the temporary launchd services were stopped after verification.

## What was actually inspected

The driver launched installed binaries in empty temporary workspaces. It accepted Claude's trust prompt only for the directory it had just created, waited for startup screens to settle, and typed a Unicode draft without Enter. Runtime resizing came from the local test driver; read-only clients had no resize authority.

Each view used a new ordinary tmux attachment. Its actual ANSI/UTF-8 bytes were decoded by `@xterm/headless` 6.0.0 and checked for the visible draft and absence of replacement characters. Reconnection used a new emulator, without replaying previous-attachment bytes. The captures were also replayed in browser xterm.js and visually inspected; no browser warnings/errors were captured.

At 40 columns both drafts remain readable. Codex wraps its status and shortens the visible history/header in the 18-row viewport; this is its own narrow-terminal layout, not lost session state. Claude's inline and Codex's alternate-screen layouts both recover through tmux's redraw. The outer tmux attachment itself uses an alternate buffer, which is recorded separately from the application's mode.

[40×18 screenshot](screenshots/real-cli-narrow.jpg) · [Reattached screenshot](screenshots/real-cli-reattached.jpg)

The screenshots are replays of actual captures, not live GUI connections. Raw captures and the preview copy are ignored by Git. Screenshots contain ordinary CLI status UI and are local user-facing evidence.

## Limits and follow-up

- No generated response, tool execution or approval-dialog flow was tested. Claude used restricted/manual mode; Codex used a read-only sandbox and no shared daemon.
- Test-driver input/resize is not a host input-lease API. Product controller fencing and GUI/native streaming remain P1/P2 work.
- Read-only rendering worked with the installed defaults. Explicit terminal query/reply and focus-event behavior still need dedicated coverage. Claude displayed a focus-events-off hint on reattachment; the user's tmux configuration was not modified.
- No phone, remote transport or E2EE test is implied.
- Startup trust UI appears asynchronously after an initial banner. The harness was corrected to wait for a settled screen and verify the selected trust option before confirming; an initial banner alone is not readiness.

Reproduce with `make check-cli` after the [development setup](DEVELOPMENT.md). Use `make preview-cli` for local visual replay. These tests are intentionally not automatic CI because they depend on installed, configured provider CLIs.
