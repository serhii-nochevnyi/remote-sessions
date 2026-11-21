# P0 scaffold verification

Historical bootstrap evidence; the later Rust migration/runtime result is recorded in [runtime verification](P0-RUNTIME-VERIFICATION.md).

Date: 2026-11-21. Scope: initial repository/bootstrap slice only; P0 is not complete.

## Local evidence

| Check | Result |
| --- | --- |
| Go vet, race tests and build | Passed |
| Live loopback HTTP smoke and SIGTERM shutdown | Passed: `/healthz` returned scaffold metadata; process exited with status 0 |
| TypeScript strict checks and three behavior/contract tests | Passed |
| Vite production build | Passed; large-chunk advisory remains for the bundled terminal renderer |
| Rust shared contract test | Passed |
| Tauri macOS `cargo check --locked` | Passed |
| Local Markdown file links | Passed |
| Desktop browser inspection | Tabs switch, close and reopen without removing demo catalog entries |
| Phone-layout browser inspection | Session list opens a session; terminal fits the narrow layout |
| Browser console | No captured warning/error messages at final inspection |
| Imported planning snapshot | Original six files preserved byte-for-byte |

Screenshots show the actual scaffold with illustrative data: [desktop](screenshots/scaffold-desktop.jpg) and [phone-layout preview](screenshots/scaffold-phone-preview.jpg). The latter is a narrow browser layout, not iOS runtime evidence.

Toolchain: Go 1.26.1, Node 24.21.0, npm 11.19.0, Rust 1.97.1. An isolated Rust toolchain was installed under ignored `.cache`; user shell configuration was not changed. Full Xcode and tmux were not available during bootstrap. The diagnostic found Claude Code and Codex executable paths but did not launch them or inspect authentication.

## Evidence limits

Native type checking is not an app launch, signed release build or physical-device test. The client only displays static samples and exposes build metadata. The server only provides loopback liveness. CI is a local template and has not run on a remote provider. No remote, commit, user service or deployment was created.

Remaining P0 gates: tmux/service-lifetime proof, physical iPhone usability and lifecycle, and Go/Rust encryption/key-store interoperability. See [current roadmap](ROADMAP.md).
