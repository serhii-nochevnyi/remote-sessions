# Local tmux runtime lab

This P0 slice proves local process lifetime, not remote connectivity or production readiness. Source lives on Kingston. The lab copies the built agent and tmux config to a private temporary service directory, then bootstraps **two separate launchd jobs** in the logged-in user's GUI domain. No LaunchAgents file is installed for automatic login startup.

The tmux job runs foreground with its own socket/config. The host never starts a tmux server: all adapter calls forbid automatic startup. Stopping/restarting the host can therefore drop requests/views but cannot tear down tmux's process group. No global tmux server or user tmux configuration is modified.

## Reproduce the proof

With Cargo configured as described in [development setup](DEVELOPMENT.md):

```sh
brew install tmux
make check-runtime
```

The test creates a deterministic real process inside a tmux PTY. It checks continuing output, stable runtime/session IDs, pane PID, process start time, instance token and tmux PID across read-only client detach/loss, SIGTERM agent restart, forced launchctl restart and fresh attachment. It also checks invalid requests, duplicate creation, explicit stop isolation, natural exit and missing tmux. Its services, sessions and temporary files are removed on normal completion or assertion failure.

The fixture does not call a model provider or execute user project code. The separate [real CLI compatibility test](P0-CLI-VERIFICATION.md) now covers startup, Unicode drafts, resize and reconnection. Streamed responses, writable product ownership, terminal query responses and physical iPhone remain open.

## Manual development use

```sh
make build-runtime
python3 scripts/runtime_lab.py up
```

Copy `state_dir` from the printed JSON. For example:

```sh
RS_STATE=/private/tmp/rs-p0-EXAMPLE
./target/debug/host-agent --state-dir "$RS_STATE" create --cwd /tmp -- /bin/zsh -i
./target/debug/host-agent --state-dir "$RS_STATE" list
./target/debug/host-agent --state-dir "$RS_STATE" attach RUNTIME_UUID
./target/debug/host-agent --state-dir "$RS_STATE" snapshot RUNTIME_UUID
./target/debug/host-agent --state-dir "$RS_STATE" stop RUNTIME_UUID --expected-pane-id PANE_ID
python3 scripts/runtime_lab.py down
```

Substitute the returned runtime UUID and pane ID. Attachment is **read-only**, cannot resize the runtime and detaches with Ctrl-b d. Input ownership and the GUI PTY bridge belong to the next local-runtime slice. `down` explicitly unloads both lab jobs and ends all lab runtimes; it retains the temporary logs and deletes the manifest. It is not the same action as closing a view.

When using manual `up`, the running service uses a snapshot of the binary/config; rebuild and use `down`/`up` to install a new snapshot. Session commands take an existing absolute working directory and executable, passing arguments directly without shell-string concatenation. Arguments ending in a tmux command separator are rejected. If a response is lost, inspect inventory before retrying; this spike does not claim production request deduplication.

## Boundaries and limits

- Private same-UID Unix socket, 0700 state directory, 0600 IPC socket, singleton agent lock; no TCP session API.
- Maximum 16 managed runtimes, 64 KiB request frames, bounded command/IPC waits. No input or automatic command replay.
- Session and runtime UUIDs are distinct. Inventory is reconstructed from managed tmux names after agent restart; PIDs are observations (null after process exit), not identities.
- Exited panes remain observable until explicit stop; no automatic runtime resurrection.
- No SQLite session history yet. If tmux dies or the computer reboots, the runtime is gone; the agent reports unavailability rather than pretending to resume.
- A same-account attacker remains outside this boundary. Do not expose this development protocol to remote clients.
- The initial attempt to execute launchd binaries/config directly from Kingston stalled during process startup. Temporary installed service payloads avoid coupling the proof to removable-volume launch access. Product installation paths and macOS permissions for workspaces on removable volumes still need packaging validation; no OS security setting was changed.

## Real provider CLI compatibility

`make check-cli` is an opt-in local test requiring installed Claude Code and Codex. It creates empty workspaces, trusts only its own newly created Claude workspace, runs Claude in manual/restricted mode and Codex with a read-only sandbox, and submits no model prompt. The CLIs can perform their normal startup/account checks and may record temporary-folder trust/history in their normal host configuration. No credentials are copied or changed.

The test driver alone sends literal draft text and resizes tmux. Those are not exposed as product API capabilities. It validates eight real attachment captures with `@xterm/headless` 6.0.0, then writes sanitized evidence. Raw captures stay in ignored `.cache/cli-captures` because provider status UI can contain account details. `make preview-cli` serves an isolated xterm replay page on loopback for visual inspection; this page is not part of the production client build.
