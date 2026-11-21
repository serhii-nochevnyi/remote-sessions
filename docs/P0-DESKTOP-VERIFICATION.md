# Local desktop terminal verification

Date: 2026-11-21. macOS, Rust/Tauri, tmux 3.8. Scope: managed local sessions only.

## Run on this Mac

From `/Volumes/KINGSTON/remote-sessions`, activate the Rust environment described in [development setup](DEVELOPMENT.md), then:

```sh
make build-runtime
npm run tauri -- build --debug --bundles app
python3 scripts/desktop_lab.py up --with-clis
python3 scripts/desktop_lab.py run
```

If `.cache/desktop-lab.json` already exists, use `run` directly. `up` creates isolated Python progress, Claude Code and Codex sessions. No model prompt is submitted. The native launcher supplies the private lab directory; Finder launching the bare debug bundle without configuration does not connect to a lab.

Open a session from the left sidebar. The active terminal acquires control automatically: **«ви керуєте»** means typing, paste, Enter, arrows and Ctrl combinations are enabled. Click inside the terminal to focus it. Another controlling client makes this view read-only until release or lease expiry (up to 15 seconds after a crash). Closing a tab or the app leaves processes running. Reopening a tab creates a fresh viewer of the same runtime. Reconnect never replays input.

To stop the disposable lab, first close the app, then:

```sh
python3 scripts/desktop_lab.py down
```

This explicitly stops these test sessions and their two launchd services. Logs remain in the temporary directory. No automatic login installation is configured.

## Evidence

- `make check`: Rust fmt/Clippy/unit/contract tests; frontend typecheck, 6 tests, build and documentation links.
- Native Tauri debug `.app` build and strict client Clippy.
- `make check-desktop`: actual Unicode, paste, arrow bytes, Enter, exact received input, exclusive ownership, duplicate rejection, stale token rejection after forced agent restart, unchanged process PID, three native bridge attach/read/input/detach cycles. Each automated lab is removed afterwards.
- Native Mac GUI: real Claude Code and Codex screens; tab switching, close/reopen, manual reconnect and agent forced restart with continuing Python output and unchanged runtime/PID. [Machine-readable evidence](P0-DESKTOP-EVIDENCE.json).
- Native GUI input: exact Ukrainian paste + Enter into the probe; Claude Down/Up changed selection without accepting trust; Codex draft survived agent restart and accepted another character. Test draft cleared with Ctrl-U; no provider prompt submitted. Interactive app/lab left open for the user.
- [Native screenshot](evidence/desktop-live.jpg). No mock terminal output is used by the application.

## Boundaries

Local OS-user authentication and in-memory leases are a development slice, not remote pairing or durable ownership. No WebView create/stop, arbitrary shell/path, remote socket or clipboard escape handlers are exposed. Output is rendered by xterm, not injected into HTML. View close kills only the tmux client. Process dimensions remain authoritative; display font adapts without resizing the running process.

A missed input acknowledgement is an unknown outcome: input stops and requires reconnect, without automatic retry. The guarantee is no replay by this client, not durable exactly-once delivery. Full transcript replay, durable tab history, workspace profiles, provider task/approval completion, IME coverage, physical iPhone input, relay and E2EE remain unverified/unimplemented. CLI workspace trust/approval prompts are displayed for the user to decide. Viewer terminal-negotiation queries are suppressed rather than injected into the application; full viewer query/reply negotiation remains future work.

## References

Context7 was attempted and quota-limited; installed package types/help and primary documentation were used: [Tauri commands](https://v2.tauri.app/develop/calling-rust/), [portable-pty](https://docs.rs/portable-pty/latest/portable_pty/), [tmux manual](https://man.openbsd.org/tmux).
