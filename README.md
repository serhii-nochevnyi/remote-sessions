# Remote Sessions

Company-operated remote access to the user's own live terminal sessions. First pilot: **Mac + iPhone**.

Repository location: `/Volumes/KINGSTON/remote-sessions`. The earlier Codex output path is a symlink to this directory, not a second checkout.

## Current state

P0 scaffold plus a **local desktop terminal and tmux runtime spike**. The Rust CLI can create actual managed processes, list/snapshot them, attach a read-only terminal and explicitly stop a selected runtime. tmux and the agent run as separate macOS user services in the development lab.

The native React/Tauri app discovers real local sessions, opens tabs, streams terminal output and reconnects. Active views acquire exclusive, expiring host input leases for text, paste, Enter and arrow keys. Closing a view releases control and leaves the process alive. Pairing, relay and E2EE are not implemented. The server exposes only loopback liveness and advertises no remote capabilities.

See [current progress](.planning/STATE.md), [runtime lab](docs/RUNTIME-LAB.md), [development setup](docs/DEVELOPMENT.md), and the [Rust decision](.planning/architecture/ADR-12-rust-runtime.md).

## Stack

- Rust host agent with Tokio and a dedicated tmux service.
- Rust corporate server with Tokio/Axum; currently liveness only.
- Shared Rust protocol crate and client native core.
- Tauri 2 + React + TypeScript + Vite, with separate desktop/phone layouts.
- xterm.js renderer; SQLite/PostgreSQL remain planned persistence choices.
- npm workspace and Cargo workspace, both with lockfiles.

The user selected Rust for both agent and server after the initial scaffold. No Go implementation or Go toolchain requirement remains.

## Run

Prerequisites: Node 24, npm 11, Rust 1.97.1; macOS and tmux for the lifecycle proof. Full Xcode/signing/device setup is required for iOS work.

```sh
npm ci
npm run dev
```

Vite previews the shell only. Real local sessions require the native desktop app. See [desktop setup and evidence](docs/P0-DESKTOP-VERIFICATION.md); browser and physical iPhone access are not implemented.

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

Local Git remains on `main`, with no remote and no commits. Product name and `dev.remotesessions.app` remain development placeholders; choose a company-owned identifier before signing/pairing rollout.
