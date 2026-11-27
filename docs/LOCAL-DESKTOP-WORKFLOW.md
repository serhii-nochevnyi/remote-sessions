# Local desktop workflow

**Installed acceptance:** DEFERRED\
**Follow-up:** OPEN — [GitHub issue #10](https://github.com/serhii-nochevnyi/remote-sessions/issues/10)

The native Mac app can create and use managed local Claude Code and Codex sessions. This page describes the implementation in the W1 source baseline. Installed Mac, provider, WebView and signed physical-device observations remain unknown. The first pilot targets Mac + iPhone. The corporate server serves loopback liveness and advertises no remote runtime capabilities. Remote execution stays disabled until owner authentication, trusted pairing and end-to-end encryption are implemented and tested. [W1 verification](W1-VERIFICATION.md) · [Machine-readable evidence](W1-EVIDENCE.json) · [W1 delivery plan](W1-PLAN.md)

## Set up the Mac development app

Use a logged-in macOS desktop, the repository's pinned Node, npm and Rust toolchains, the macOS native build prerequisites, and exactly tmux 3.8 for writable W1 setup. Install the provider CLIs you intend to use and keep their normal account, trust, approval and sandbox settings. Start the disposable jobs from a shell whose PATH can resolve tmux 3.8 and the provider executables or interpreters. The app asks the services to check readiness for the selected folder and preset. [Development setup](DEVELOPMENT.md) · [Runtime lab](RUNTIME-LAB.md) · [Current stack](STACK.md)

From the checkout root, prepare the debug app and use the ordinary W1 desktop lab.

```sh
npm ci
make build-runtime
npm run tauri -- build --debug --bundles app
python3 scripts/desktop_lab.py up --w1
python3 scripts/desktop_lab.py run --w1
```

The lab starts separate tmux and host-agent launchd jobs and seeds one legacy fixture. `run --w1` starts the bundled debug app with that lab's private IPC state and tmux path. To reconnect, close the app and use the same `run --w1` command while the jobs remain up. After closing the app, `python3 scripts/desktop_lab.py down --w1` explicitly stops the disposable jobs and their lab runtimes. The same teardown reconciles interrupted lab startup where job identity and absence can be established. `--with-clis` belongs to the earlier P0 lab. [Desktop lab source](../scripts/desktop_lab.py) · [Launchd lab source](../scripts/runtime_lab.py)

`up --w1` checks that the resolved binary reports `tmux 3.8` before creating lab state or starting jobs. After attempting both jobs, it separately checks that the server on the lab's private socket reports exactly `3.8` before checking agent and catalog readiness. A version check failure reports the required version, the observed value or why the check was unavailable, and the selected binary path or private socket. Put tmux 3.8 first on PATH. If jobs were attempted, run `python3 scripts/desktop_lab.py down --w1` from the same checkout to reconcile the retained startup record before retrying. Setup neither upgrades tmux nor stops a foreign server. The host still applies its sender guard when granting write access. Read-only observation and the catalog-free P0 lab retain their existing paths.

## Create and use a session

1. Open the new-session dialog. Choose an existing project in the native folder picker and select **Claude Code** or **Codex**. The host records an opaque workspace ID and revalidates directory identity and service access before launch. The dialog requires a current ready result for that exact selection. If you cancel before submitting, close the dialog. If the folder changed or the service cannot reach it, reselect an accessible folder. If the provider is unavailable, fix its installation or service PATH and recheck readiness.
2. Choose Create once the pair is ready. The native client persists the pending request before submission. The host binds its key to the selected inputs and reserved session/runtime IDs, then persists the may-have-launched boundary before asking tmux to start the runtime. Equal retries reconcile the same operation and the host refuses conflicting key reuse. A launch result opens and selects one tab by runtime ID and returns focus to the terminal. Provider startup and an early exit have distinct states.
3. When create is **unknown**, refresh its status or reconnect to the original operation before retrying. The host reconciles against the reserved tmux generation and exact managed identity. Unknown never spawns a replacement automatically. A deliberate separate launch uses a new operation and leaves the first outcome visible.
4. Select a managed tab to stream output. The host checks exclusive expiring writer leases and input sequence for the live view. Use the toolbar or Cmd plus, minus and zero to increase, decrease or reset the font. The default is 14 px within a 10–28 px range. Use expand/restore for content layout and the separate fullscreen control for the native window. Keyboard tab navigation and explicit reorder update per-device presentation. The app restores validated open tabs, selection, ordering, font and expanded preferences against inventory. Transient inventory failure preserves saved preferences. Fullscreen and writer grants are transient.
5. Close a tab to detach its View and release its authority. Reopen a surviving Runtime from inventory to get a fresh attachment and redraw with no input replay. The provider process retains its own unsent draft while it survives. A Session is the durable user-facing record, a Runtime is one execution attempt, and a View is a device-local terminal attachment. App and agent restarts can replace views while tmux keeps the runtime alive. Legacy managed sessions retain their original identities wherever the live target can be corroborated.

The native commands expose local inventory, restricted workspace selection, provider readiness, durable create and status, managed terminal attach/read/status/detach, lease-checked input and guarded paste, owner resize and renderer acknowledgement, and window fullscreen. Same-user IPC authenticates the local OS account. The host resolves executables and launch environment for the two fixed profiles. WebView requests carry registered workspace and profile IDs. Terminal bytes go through xterm and stay out of application HTML. [Native commands](../apps/client/src-tauri/src/lib.rs) · [Desktop presentation](../apps/client/src/App.tsx) · [Create dialog](../apps/client/src/CreateSessionDialog.tsx) · [Local workflow decision](decisions/ADR-13-local-desktop-workflow.md)

## Catalog and terminal boundaries

The host implements a private, versioned SQLite catalog. Its directory is under the current OS user's `~/Library/Application Support` on the internal macOS data volume. The W1 lab uses `dev.remotesessions.w1.<lab-id>/catalog.sqlite3` there, with separate short-lived socket state under the private temporary directory. Source and projects can live elsewhere, including KINGSTON where the actual services have access. The host verifies rollback journal mode `DELETE`, `synchronous=EXTRA`, macOS fullfsync, foreign keys, trusted schema off, a bounded busy timeout and the expected SQLite version. Unreadable, corrupt or unsupported catalogs surface an actionable fault while preserving state. Exact already-live observation can continue read-only during catalog faults. PostgreSQL remains the future corporate server storage choice. [Catalog source](../crates/host-agent/src/catalog.rs)

**D-08 and D-09.** Dedicated stock tmux uses manual window sizing and ignore-size attachments. Serialized host admission checks the live exclusive view lease, exact runtime/server/window/pane identity, a separate monotonic resize sequence and the 2–500 column and 2–200 row bounds. One resize is in flight per runtime, and each view retains its latest unsent proposal. An acknowledged conditional engine epoch transition fences the exact predecessor before a successor is acknowledged. A delayed obsolete setter or stale command reaching its guard after retirement cannot mutate the committed target. The engine checks a fixed non-yielding predicate immediately before exact-target mutation. Already-admitted geometry can propagate after lease expiry or handoff.

A timeout or surviving-server agent restart requires confirmed fencing and fresh exact-target readback before reopening mutation admission. Unresolved authority stays pending or unavailable. Reconcile first, discard obsolete proposals, and let the current owner propose fresh desired geometry without replay or inverse compensation. Track host admission and measure four grids independently.

| Grid | Observation |
| --- | --- |
| tmux virtual target | Exact committed dimensions and scoped target revision |
| Provider TTY | Kernel sample with provider pane, process and TTY identity revalidated around the sample |
| Native bridge | Actual bridge PTY/client dimensions for the checked attachment |
| xterm | Actual dimensions of the current WebView renderer |

A matching sample describes that instant. After proposals stop and the engine progresses, the four grids should converge. The healthy path preserves the runtime, process, attachment and xterm instance. It orders bridge resize/readback, xterm write completion and grid changes, followed by targeted redraw in the current view. Stale owner, agent, view and revision replies are discarded. Spectators follow the committed grid and can zoom or pan locally. If a view loses coherence, recover that view with a fresh attachment and redraw. Installed acceptance separately requires actual host rejection of spectator and stale-lease resize attempts, with the legitimate owner's four grids and target revision unchanged. Spectator readback has its own witness. [Host engine](../crates/host-agent/src/engine.rs) · [Desktop bridge](../crates/desktop-bridge/src/lib.rs) · [Terminal view](../apps/client/src/TerminalView.tsx) · [Private-socket resize probe](../crates/desktop-bridge/examples/probe.rs)

**D-11.** Output queues are bounded per view. Pressure that stalls a view can retire it without blocking healthy observers or the runtime. Recover with a fresh attachment and redraw. Ordered terminal parsing keeps renderer-generated replies separate from ordinary user input. Queued input remains bound to the captured writer grant. Paste has a separate live-lease admission boundary at the serialized staged commit. A permitted command is bound to the original stamped pane and consumed marker. An already-admitted one-shot effect can finish after expiry on that pane. While disposition is pending, handoff and new input wait. Uncertain delivery quarantines that pane across restart and geometry retirement, with no replay. Fresh eligible panes have their own authority. Installed proof requires independent command, byte, mode, receipt and cleanup observations. [Host paste guard](../crates/host-agent/src/engine.rs) · [Terminal protocol](../apps/client/src/TerminalView.tsx)

### Source repair baseline

The source handoff includes the nine repair areas from Plans 10–18.

| Area | Implemented boundary |
| --- | --- |
| Catalog fault observation | Corroborate a live target for read-only observation, fail closed on writer/create authority, and acquire the singleton before catalog startup. |
| Attachment and EOF | Check attachment identity before publication. Deliver verified final output in order before EOF within bounded retention. |
| Geometry deadline | Accept an exact in-order geometry acknowledgement even after a newer commit and keep the original drain deadline through supersession. |
| Captured input grant | Keep queued input scoped to the captured view and writer grant through the native send. |
| Pane quarantine | Preserve uncertainty against the original pane across geometry retirement and keep cleanup scoped to its named paste buffer. |
| Readiness and presentation | Match readiness to the current folder/profile. Record restart-safe presentation before exposure and preserve close intent and same-target read-only fallback. |
| Resize capture and expiry | Use a real private socket and exact checked target for the diagnostic. Correlate expiry refusal to the attempted resize and separate it from prior propagation. |
| Recoverable lab jobs | Record attempted launchd jobs before startup and reconcile interrupted startup or teardown against checked job identity. |
| Build-bound report ordering | Record picker-cancel requests independently, require closure before replacement attachment/redraw, and bind reports to controlled-build artifacts. |

## Reproduce the helper procedures after the acceptance replan

Issue #10 retains the full installed/device matrix and remaining positive paste preparation. Before acceptance, a fresh checked preparation and acceptance replan binds the implemented commands and finalized clean product commit/tree. Prepare and commit the outstanding paste harness first. Build each mode in its own clean checkout with fresh output paths and retain the producer digest independently from the report input. Repeat affected builds and runs if their inputs or exercised artifacts change. Keep the tested product revision distinct from the later evidence publication revision.

The recovery helper uses a running ordinary W1 lab, the bundled debug app and a logged-in Mac terminal. Create a new report in an existing private destination outside the checkout. Optionally add `--kingston "$KINGSTON_PARENT"` with an owner-approved existing parent on `/Volumes/KINGSTON` for a disposable workspace. The recovery helper coordinates its separate fixture jobs. To use a fixture app from the harness, the interface is `run --w1 --fixture --client-state NAME` with the private client directory name the harness supplies. Use `down --w1 --fixture` for recorded fixture cleanup. Use the ordinary teardown separately for the ordinary lab. [Recovery helper](../tests/integration/w1_recovery.py)

Use the real CLI compatibility helper for a separate report and raw captures. It exercises the installed CLI providers directly. Native WebView startup, safe menu browsing and unsubmitted drafts require their own observations. The historical lab page describes the CLI harness and old evidence. Keep raw captures private. [CLI helper](../tests/integration/cli_compatibility.py) · [Historical CLI report](P0-CLI-VERIFICATION.md)

```sh
python3 tests/integration/w1_recovery.py --report "$PRIVATE_DIR/recovery.json"
python3 tests/integration/cli_compatibility.py --report "$PRIVATE_DIR/cli.json" --captures "$PRIVATE_DIR/cli-captures"
```

For pressure evidence, `python3 tests/integration/w1_terminal_pressure.py --describe-native` prints the installed operator sequence and record schema. Use an absolute `PRIVATE_DIR` outside the checkout owned by the current user with mode 0700, and fresh paths for records, bundles and reports. In each acceptance checkout, use the corresponding build and report pair below. Set `BUILD_RECORD_SHA256` from the independently retained producer completion line for that mode. The installed operator supplies same-run observations at the new `--native-evidence` path. [Pressure helper](../tests/integration/w1_terminal_pressure.py)

Bridge mode builds and runs the host and probe without app bundles.

```sh
python3 tests/integration/w1_terminal_pressure.py --controlled-build bridge \
  --build-record "$PRIVATE_DIR/bridge-build.json"
python3 tests/integration/w1_terminal_pressure.py --report "$PRIVATE_DIR/bridge-report.json" \
  --build-record "$PRIVATE_DIR/bridge-build.json" --build-record-sha256 "$BUILD_RECORD_SHA256"
```

Installed mode adds owner and victim debug bundles and same-run installed operator evidence.

```sh
python3 tests/integration/w1_terminal_pressure.py --controlled-build installed \
  --build-record "$PRIVATE_DIR/installed-build.json" \
  --owner-app "$PRIVATE_DIR/Owner.app" --victim-app "$PRIVATE_DIR/Victim.app"
python3 tests/integration/w1_terminal_pressure.py --report "$PRIVATE_DIR/installed-report.json" \
  --build-record "$PRIVATE_DIR/installed-build.json" --build-record-sha256 "$BUILD_RECORD_SHA256" \
  --owner-app "$PRIVATE_DIR/Owner.app" --victim-app "$PRIVATE_DIR/Victim.app" \
  --native-evidence "$PRIVATE_DIR/installed-operator.json"
```

The parser also supports a `--controlled-build paste` record for host, probe and an owner bundle. The `--paste` route requires the mode-matched record and digest, `--owner-app` and `--paste-native`. Its positive workload raises `ProvenanceHold`, so positive paste evidence stays on HOLD. `--test-paste-evidence` is absent. The follow-up retains the exact disposable sender route, safe reopen of its durable fixture, live artifact-bound workload, positive evidence consumer and false-positive fixtures. A stock refusal, test-sink bytes or tmux exit status cannot earn installed paste acceptance.

## Installed acceptance inventory

| Gate | Evidence owed |
| --- | --- |
| R1 | Native double submission, equal/concurrent and conflicting operations, crash and reply recovery, early exit, unknown outcomes and legacy continuity. |
| R2 | Private SQLite settings, fault handling, migration and rollback, retained operations and panes. |
| R3 | Native picker and service-side access, safe negative cases and both providers on internal and accessible KINGSTON projects. |
| R4 | Fenced owner resize, independent four-grid convergence and redraw, separate spectator readback and rejected spectator/stale requests, reconnect with no replay. |
| R5 | Startup and steady pressure, stalled-observer isolation, healthy progress, control/resize, redraw and two-session independence. |
| R6 | Ordered terminal protocol, both providers' short and multi-chunk normal and bracketed paste, paste safety and no replay, and remaining positive harness preparation. |
| R7 | Distinct provider CLI and native WebView workflows and menus, one tab per native create, rapid concurrent tab switching, presentation and separate restarts, provenance, and legacy and signed-device regressions. |

The verification report lists every canonical subcase and its owed witness. Native outcomes and prompt counts remain unknown. W1, P0 and pilot completion are held. [Full held case inventory](W1-VERIFICATION.md) · [GitHub issue #10](https://github.com/serhii-nochevnyi/remote-sessions/issues/10)
