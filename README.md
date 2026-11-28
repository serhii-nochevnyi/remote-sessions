# Remote Sessions

Company-operated remote access to the user's own live terminal sessions. First pilot: **Mac + iPhone**.

Repository location: `/Volumes/KINGSTON/remote-sessions`. The earlier Codex output path is a symlink to this directory, not a second checkout.

## Current state

**Installed acceptance:** DEFERRED\
**Follow-up:** OPEN — [GitHub issue #10](https://github.com/serhii-nochevnyi/remote-sessions/issues/10)

The native Mac app can select an existing project folder, check Claude Code or Codex readiness and create a durable local session. The managed runtime opens in one terminal tab with live output, exclusive expiring input leases, and owner-authorized resize. Tabs support zoom, expanded layout, fullscreen and reconnect. Closing a view leaves the process alive. If a create outcome is unknown, refresh or reconnect to its original operation before deciding to launch separately.

Start with the [local desktop workflow and setup](docs/LOCAL-DESKTOP-WORKFLOW.md). The [W1 verification report](docs/W1-VERIFICATION.md) and [machine-readable evidence](docs/W1-EVIDENCE.json) list the installed provider, WebView and physical-device observations still owed. W1, P0 and pilot completion remain held.

The server exposes only loopback liveness and advertises no remote runtime capabilities. Remote execution stays disabled until owner authentication, trusted pairing and end-to-end encryption are implemented and tested.

See the [roadmap](docs/ROADMAP.md), [development setup](docs/DEVELOPMENT.md), [runtime lab](docs/RUNTIME-LAB.md), [desktop workflow decision](docs/decisions/ADR-13-local-desktop-workflow.md) and [Rust decision](docs/STACK.md).

## Stack

- Rust host agent with Tokio and a dedicated tmux service.
- Rust corporate server with Tokio/Axum; currently liveness only.
- Shared Rust protocol crate and client native core.
- Tauri 2 + React + TypeScript + Vite, with separate desktop/phone layouts.
- xterm.js terminal rendering and an implemented, private SQLite catalog under the user's internal macOS application state. PostgreSQL remains the future corporate server storage choice.
- npm workspace and Cargo workspace, both with lockfiles.

The user selected Rust for both agent and server after the initial scaffold. No Go implementation or Go toolchain requirement remains.

## Run

Use Node 24, npm 11 and Rust 1.97.1. The local native lab requires macOS and tmux. iOS work requires full Xcode, signing and device setup.

```sh
npm ci
npm run dev
```

Vite previews the shell only. The native desktop setup above builds the debug app and starts its separate macOS user services. The [P0 desktop report](docs/P0-DESKTOP-VERIFICATION.md) records the earlier spike.

For the isolated Rust toolchain already prepared in this checkout:

```sh
export CARGO_HOME="$PWD/.cache/cargo"
export RUSTUP_HOME="$PWD/.cache/rustup"
export PATH="$CARGO_HOME/bin:$PATH"
make doctor
make check
make check-native
make check-runtime
make check-desktop # Actual local input ownership and native PTY bridge
make check-cli    # Opt-in actual Claude/Codex UI test; no model prompt
```

`make check-runtime` builds the Rust binaries and runs the macOS launchd/PTY lifecycle proof, cleaning up its temporary services and test sessions. Read [runtime lab instructions](docs/RUNTIME-LAB.md) for interactive use. With a regular Rust installation the exports are unnecessary.

`make server` starts `http://127.0.0.1:7331/healthz`. `npm run tauri -- dev` starts the native Mac development shell. These commands do not configure an internet-facing service.

## Layout

```text
apps/client/             React desktop/mobile shell and Tauri integration
crates/client-core/      Native bootstrap boundary
crates/desktop-bridge/   Local PTY views, bounded output and leased input
crates/protocol/         Shared public bootstrap status
crates/host-agent/       Local IPC, managed runtime adapter and diagnostic CLI
crates/server/           Loopback HTTP server scaffold
config/tmux.conf         Dedicated service configuration
scripts/runtime_lab.py   Disposable macOS launchd lab
tests/integration/       Real-process lifecycle proof and deterministic PTY probe
protocol/                Status schema and shared fixture
docs/planning/           Historical v0.2 architecture/plan/UI/mockups
.planning/               Current decisions, phase status and execution notes
```

## Planning and designs

- [Original architecture](docs/planning/ARCHITECTURE.md)
- [Original MVP plan](docs/planning/MVP-PLAN.md)
- [UI contract](docs/planning/UI-SPEC.md)
- [Desktop mockup](docs/planning/design/desktop-concept.html)
- [Mobile mockup](docs/planning/design/mobile-concept.html)

`docs/planning` remains the unchanged v0.2 snapshot. ADR-12 supersedes its Go stack and Go/Rust cryptographic interoperability requirement. The remaining session/ownership/encryption invariants still apply. Mockup HTML files are Codex visualization fragments, not standalone applications.

The private repository is [serhii-nochevnyi/remote-sessions](https://github.com/serhii-nochevnyi/remote-sessions). Development uses GitHub milestones/issues, feature branches and pull requests. Internal `.planning/` artifacts remain local; the reviewed roadmap is published in `docs/ROADMAP.md`. Product name and `dev.remotesessions.app` remain development placeholders; choose a company-owned identifier before signing/pairing rollout.

See the [W1 delivery plan](docs/W1-PLAN.md), [source handoff issue](https://github.com/serhii-nochevnyi/remote-sessions/issues/79) and [GitHub milestone](https://github.com/serhii-nochevnyi/remote-sessions/milestone/1).
