# Development setup

## Toolchain and location

The project is at `/Volumes/KINGSTON/remote-sessions`. Source, local Git, dependencies and build outputs were moved together. Original Codex output links resolve through a symlink to this checkout.

Node/npm versions are recorded in `.nvmrc` and root `package.json`. Rust is pinned in `rust-toolchain.toml`. Use `npm ci` and Cargo `--locked`. The former Go scaffold has been replaced by Rust; Go is no longer required.

For the isolated toolchain prepared in this checkout, run from the repository root:

```sh
export CARGO_HOME="$PWD/.cache/cargo"
export RUSTUP_HOME="$PWD/.cache/rustup"
export PATH="$CARGO_HOME/bin:$PATH"
```

These exports affect the current shell only. `.cache` is ignored and is unnecessary with a normal Rust installation. See official [Rust installation](https://rust-lang.github.io/rustup/installation/) and [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/).

## Client

Run `npm ci`, then `npm run dev`. Vite binds loopback. `npm run tauri -- dev` runs the native development shell. Native commands expose bootstrap status plus local inventory, attach/read/detach and lease-checked input. Paths, tmux executable, tokens and input sequence numbers remain native-side. `RS_STATE_DIR` must identify a private same-user lab directory; use [desktop lab instructions](P0-DESKTOP-VERIFICATION.md). No shell/filesystem/network plugin was enabled. Production CSP permits bundled UI and native IPC.

The starter used `create-tauri-app@4.7.5`, React/TypeScript. Its opener plugin/permissions were removed. Full Xcode, a signing team and physical-device setup are still required for `npm run tauri -- ios init` and the iPhone spike. Browser preview is not device evidence.

## Native components

- `cargo run -p remote-sessions-host -- doctor`: discover executable availability, without launching CLI tools or inspecting credentials.
- `make server`: Rust/Axum liveness on `127.0.0.1:7331`, no session/relay endpoints; handles SIGINT/SIGTERM.
- [Runtime lab](RUNTIME-LAB.md): separate tmux and host-agent launchd jobs, private Unix IPC and actual local processes. This is a development spike, not a production install or an authenticated remote API.

## Verification

`make check` runs Rust formatting, Clippy, unit/contract tests, frontend checks/tests/build and local Markdown-link validation. `make check-native` compiles the Tauri shell. `make check-runtime` runs the real macOS service-lifecycle proof and refreshes its evidence JSON. `make check-desktop` verifies real local input, ownership fencing and the native PTY bridge.

The CI file is an inactive template: Linux checks portable Rust/frontend code; macOS compiles Tauri. A hosted runner lacking a logged-in GUI launchd domain cannot replace local lifecycle evidence. No remote workflow has been run.

The bootstrap fixture has matching Rust and TypeScript checks. The runtime spike uses a separate private development protocol, not a settled E2EE framing format. There is no plaintext remote terminal fallback.

## Documentation sources

Context7 was attempted but returned a quota limit. Official docs and installed help were used:

- [tmux manual](https://man.openbsd.org/tmux) and local `man tmux`
- local `man launchd.plist` and `man launchctl`
- [Tokio UnixStream](https://docs.rs/tokio/latest/tokio/net/struct.UnixStream.html)
- [Tokio processes](https://docs.rs/tokio/latest/tokio/process/struct.Command.html)
- [Axum](https://docs.rs/axum/latest/axum/)
- [Clap derive](https://docs.rs/clap/latest/clap/_derive/_tutorial/index.html)
- [Tauri commands](https://v2.tauri.app/develop/calling-rust/)
- [xterm.js security](https://xtermjs.org/docs/guides/security/)
