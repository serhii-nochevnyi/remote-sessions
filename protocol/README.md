# Protocol boundary

The schema and shared fixture define only the bootstrap status response. Rust and TypeScript implementations must match the fixture, checked by their tests. An empty capability list advertises no session or remote-control functionality.

This is not the session wire protocol or a cryptographic handshake. Define those after the P0 terminal and E2EE spikes. Future types distinguish Session, Runtime, View, and InputLease; tab indexes or PIDs are not session identity. Keep plaintext session endpoints disabled until authenticated transport exists.

The private development IPC is currently defined in `crates/host-agent/src/model.rs`. It is intentionally not the remote wire contract or a stable public API.
