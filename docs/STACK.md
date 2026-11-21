# ADR-12: Rust host and corporate server

Status: accepted by the user on 2026-11-21. Supersedes the Go choice in ADR-06 and the Go/Rust interoperability assumption in the v0.2 planning snapshot.

Use Rust for the host agent, corporate server, client native core and shared protocol types. Keep Tauri/React/TypeScript and xterm.js. Tokio handles asynchronous IO; Axum provides the server HTTP boundary. Shared cryptographic code can later use the same audited Rust implementation, but a shared language is not evidence of cryptographic correctness.

Why now: the Go components contain only diagnostics and liveness, so migration is small. A single native language reduces duplicated contracts and future integration work. Rust introduces compiler/dependency complexity and must not lead to speculative abstractions. Keep separate crates/process ownership; the relay does not gain client secrets or terminal-execution access.

The independent tmux service remains essential: Rust memory safety does not make a child process survive service teardown. No runtime is owned by the agent service's process group. There is no plaintext remote-control endpoint.

The original v0.2 docs remain a historical snapshot. Current decisions and runnable instructions take precedence.
