# Development waves — Mac + iPhone MVP

Historical product scope and acceptance IDs remain in [MVP-PLAN](planning/MVP-PLAN.md) and [UI-SPEC](planning/UI-SPEC.md). [ADR-12](STACK.md) supersedes the Go stack.

**Installed acceptance:** DEFERRED\
**Follow-up:** OPEN — [GitHub issue #10](https://github.com/serhii-nochevnyi/remote-sessions/issues/10)

## Baseline: what is already proven

Rust host/server/native core; independent macOS tmux and agent service lifetimes; real local desktop tabs and live output; same-runtime reconnect; leased local input; Unicode/paste/Enter; selection in Claude; unsent Codex draft continuity. Evidence: [desktop](P0-DESKTOP-VERIFICATION.md), [runtime](P0-RUNTIME-VERIFICATION.md), [CLI](P0-CLI-VERIFICATION.md).

The historical P0 reports cover the local spike. W1 source adds the private SQLite catalog and durable native operations. Physical iPhone usability, E2EE/key-store feasibility, model response and approval-task completion, production installation, remote transport and release readiness remain outside the historical P0 results.

## Operating model

Each wave follows **investigate → accept decisions/ADR → decompose → implement small vertical slices → verify → review → record evidence and re-estimate**. The entire product has a direction now; only the next wave receives detailed executable tickets. Later investigations consume actual results rather than frozen assumptions.

A wave is done only when its user scenario and failure cases pass, not when its files compile. Each task must name its contracts, touched files, dependencies, negative cases and runnable checks. Parallel work is allowed only with stable interfaces and no conflicting file ownership. Do not use wave numbers as automatic permission to publish, enroll devices or deploy infrastructure.

## Wave map

| Wave | User-visible result | Dependencies / scope | Exit evidence |
|---|---|---|---|
| W1 — Local desktop workflow | Create Claude/Codex in a selected project; new tab opens; readable font, zoom, expanded/fullscreen terminal | Current spike. Native workspace picker, fixed launch profiles, durable creation/reconciliation, owner-authorized resize, preferences | Both provider launch flows in the native app; cancel/failure/double-submit/lost-reply/crash cases; no duplicate runtime; zoom/resize without process loss or cross-tab input |
| W2 — iPhone feasibility | Demonstrate that the chosen shell can provide usable terminal input on a physical iPhone | Can start alongside W1 when Xcode/signing/device access exist. Bundled real terminal fixtures and a native input recorder, no plaintext remote runtime path | Ukrainian/English keyboard/IME, paste, selection, scrolling, cursor with keyboard, rotation, suspend/foreground; explicit Tauri continue/replace-shell decision |
| W3 — Reliable local product | Open the installed Mac app and work without manually preparing a temporary lab | W1; W2 findings must not contradict shared client contracts. Durable session history/rename/stop, stable agent/tmux installation, diagnostics, restored tabs, explicit controller policy | Native install/restart/recovery; precise exit/unreachable states; user configuration preserved; independent second-client races; slow viewer cannot stall another session or grow memory without bound |
| W4 — Endpoint trust and E2EE feasibility | A paired test endpoint exchanges authenticated encrypted traffic and rejects an untrusted endpoint | Stable W3 session/control contracts; can research earlier. Rust crypto selection, Keychain, pairing transcript, key pinning, fresh connection epochs, compatibility vectors | Tampered/replayed/key-substituted frames rejected; secrets confined to endpoints; key-lock/reset behavior; opaque relay harness; closes remaining P0 cryptographic gate |
| W5 — Corporate remote access | A trusted client reaches its own Mac through the company relay | W3 + W4. Corporate OIDC/enrollment, host directory, PostgreSQL, TLS and outbound tunnels, authorization expiry/revocation, bounded encrypted relay | Two identities isolated; relay logs/traffic contain no known terminal plaintext; relay restart/network partition/revocation; local Mac operation survives relay outage |
| W6 — End-to-end iPhone workflow | Start on Mac, continue the same runtime on iPhone, return control to Mac | W2 + W5 + stable desktop ownership UI. Mobile session/create/pairing/device flows, explicit handoff, keyboard-aware resize, fresh-redraw reconnect | Physical device over real E2EE relay; Wi-Fi/mobile-network switch, background/lock/foreground, stale owner rejection, no input replay; UI-SPEC M1–M10 |
| W7 — Installable pilot | A small internal cohort can install, recover and use the product with documented support boundaries | W6; release/security review and signing/distribution access. Signed Mac/iOS artifacts, server operations, upgrade/rollback, safe uninstall, bounded metadata observability | A01–A15 release matrix; installation/recovery exercised by another person; no unresolved critical/high authorization, key-trust, process-loss or duplicate-input defect |

## Dependencies and permitted concurrency

```mermaid
flowchart LR
  B[Verified local spike] --> W1[W1 Local desktop]
  B --> W2[W2 Physical iPhone spike]
  W1 --> W3[W3 Reliable local product]
  W3 --> W4[W4 Trust and E2EE]
  W3 --> W5[W5 Corporate relay]
  W4 --> W5
  W2 --> W6[W6 Remote iPhone workflow]
  W5 --> W6
  W6 --> W7[W7 Pilot release]
```

W2 is an early feasibility lane, not an excuse to delay platform evidence until the relay is finished. Crypto research and IdP/signing discovery may begin early; remote runtime access cannot ship or be used for real work before trust/E2EE gates. If only one developer is active, finish one implementation slice at a time and use parallelism for independent research or device setup.

The old P0–P5 names are scope categories, not a second execution schedule: W1 covers part of P1/P2; W2 covers the phone portion of P0; W3 completes local P1/P2 hardening; W4 covers the security part of P0 and prerequisite P3 work; W5 covers P3; W6 covers P4; W7 covers P5. Completed spikes stay completed; this does not restart the project.

## Wave 1 local desktop source handoff

The W1 source implements the intended local Mac workflow. Its acceptance matrix remains deferred. [Local desktop workflow](LOCAL-DESKTOP-WORKFLOW.md) · [W1 delivery plan](W1-PLAN.md) · [W1 verification](W1-VERIFICATION.md)

- Native folder picker with cancel semantics; bounded, typed workspace identity; project path/name shown before launch.
- Host-resolved Claude/Codex profiles with readiness errors; provider authentication and approval/sandbox defaults remain on the Mac.
- Durable request identity and reconcile-before-retry across spawn/acknowledgement crashes. An uncertain result is shown explicitly; reconnect cannot silently launch a new process.
- Create dialog with pending/error states, one successful runtime opening one tab, focus restored to the new terminal, session retained when its tab closes.
- Explicit readable font size, zoom/reset controls and keyboard shortcuts; expandable workspace and native fullscreen; presentation changes do not remount a live attachment.
- Layout/zoom derives rows and columns. Only the acknowledged owner may change the tmux runtime size; spectators change only their local display. Resize is coalesced and fenced across detach/reconnect/control changes.
- Persist non-secret view preferences separately from host-authoritative session state. No split panes, terminal recording, Shell profile or mobile feature scope in W1.

### Wave 1 acceptance gate

| Case | Required result |
|---|---|
| Native create for each provider | Correct project and installed executable; new runtime ID and selected tab |
| Cancel picker / unavailable CLI / inaccessible folder | No process created; clear recovery action |
| Double submission / lost reply / agent crash around spawn | One operation reconciles to at most one runtime; no automatic respawn |
| Font increase/decrease/reset, expand, fullscreen | Readable input cursor and predictable grid with focus and input on the same session |
| Spectator resize / stale lease | Actual host rejection of each attempted mutation with the current owner's four grids and target revision unchanged. Spectator readback is witnessed separately. |
| Close/reopen, app restart, agent restart | Runtime identity survives where tmux survives; UI restores preferences and reports state honestly |
| Rapid switching and output pressure | Two live tabs, each with correctly routed output and draft input through rapid switching. Bounded queues and responsive control under pressure. |
| Actual native acceptance | Separate real CLI and Mac WebView workflows for Claude Code and Codex, including separate menu, one-tab creation, exact provider/mode paste, safety and no-replay witnesses, with build and capture provenance |

Relevant existing acceptance IDs: A01, A02, A04 (local ownership subset), A06 (agent subset), A11, A13, A15; D1–D4, D7, D9. Remote/device portions remain later gates, not implicitly passed here.

## Wave 1 delivery status

The accepted local workflow decision selected SQLite-backed operation identity and restricted native launch. The active resize decision fences authority at admission and allows propagation of previously admitted effects. The current baseline includes the nine source repair areas. The source handoff was projected separately after Gate 2. It follows current-head checks, source review and applicable aggregate integration and architecture review under recorded delegation. [Local workflow decision](decisions/ADR-13-local-desktop-workflow.md) · [Source handoff issue](https://github.com/serhii-nochevnyi/remote-sessions/issues/79) · [W1 milestone](https://github.com/serhii-nochevnyi/remote-sessions/milestone/1)

The open installed follow-up retains the complete R1–R7 matrix, the applicable signed physical-device prerequisite and remaining positive paste preparation. Before acceptance, complete the preparation, bind finalized build inputs and gather fresh, independent observations. Prompt counts are unknown and W1, P0 and pilot completion remain held. Source merge leaves the follow-up open. [W1 verification](W1-VERIFICATION.md)

## Scheduling and external inputs

Do not treat the historical 10–17.5 engineer-week estimate as a fresh forecast: it predates the current Rust spike and does not account for the unresolved device/security gates. Estimate W1 after its checked decomposition, record actual cycle time, and re-estimate the next wave at every exit. Do not divide effort mechanically by the number of agents.

Before W2: physical iPhone, supported Xcode, signing team/device access. Before W4/W5: Keychain behavior, corporate IdP and enrollment policy, server hosting/domain/TLS ownership, revocation requirements. Before W7: distribution/signing ownership, pilot participants and review availability. These are tracked dependencies; they do not block local W1 planning.

## Scope after pilot

Linux host/desktop and Android follow pilot feedback. Structured agent chat, approval cards, diff views, recordings and split terminals need separate product decisions. Keep extension boundaries without building those capabilities now.
