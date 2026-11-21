# Remote Sessions development instructions

- Read `README.md`, `.planning/STATE.md`, and `docs/planning/ARCHITECTURE.md` before changes.
- The first pilot targets Mac + iPhone. Linux workstations and Android clients are post-pilot.
- Keep Session, Runtime and View distinct. Closing a tab must never stop a process.
- No remote session execution before owner authentication, trusted pairing and E2EE are implemented and tested. Scaffold endpoints must advertise no runtime capabilities.
- Treat terminal output as untrusted; never inject it into application HTML.
- Native capabilities are explicit and minimal. Do not add filesystem/shell/network permissions speculatively.
- Use `make check` for Rust/frontend/contracts/docs; `make check-native` for Tauri changes and `make check-runtime` for the real macOS launchd lifecycle proof. Report unrun native/device checks honestly.
- Keep lockfiles. Never add credentials, signing assets, toolchain caches, generated native projects or build outputs to Git.
- Update `.planning/STATE.md` with actual evidence. A scaffold does not complete P0's persistence, device-input or E2EE gates.

- ADR-12 supersedes the historical Go stack: host, server and native core use Rust. Read `.planning/architecture/ADR-12-rust-runtime.md`.
- The local P0 CLI is an explicitly authorized development spike using private same-user IPC. Keep remote execution disabled The user subsequently authorized local desktop streaming and input: expose only inventory, managed view attach/read/detach and lease-checked input through native commands. Same-UID private IPC authenticates the local OS user; the host must enforce exclusive expiring writer tokens and input sequencing. This exception does not authorize remote runtime access.
