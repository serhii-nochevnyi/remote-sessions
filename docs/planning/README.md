# Remote Sessions — MVP planning baseline

Version: 0.2 · 2026-11-21 · Working product name

This is a greenfield project. There is no source repository, implementation, or deployment yet. This package records the architecture and implementation roadmap requested after approval of the desktop and mobile concepts. It does not claim that the software, security properties, or integrations have been built or tested.

## Read in this order

1. [Architecture](ARCHITECTURE.md): responsibilities, decisions, session lifecycle, and trust boundaries.
2. [UI contract](UI-SPEC.md): the approved desktop and mobile experience, including required states missing from the happy-path mockups.
3. [MVP implementation plan](MVP-PLAN.md): phases, dependencies, deliverables, and acceptance criteria.

The [desktop concept](design/desktop-concept.html) and [mobile concept](design/mobile-concept.html) preserve the exact interactive fragments shown in the conversation. They are design references for the Codex visualization viewer, not standalone applications or production source. Their simulated terminal output and actions are illustrative.

## Product outcome

An employee starts Claude Code, Codex CLI, or a shell on their work computer, closes the desktop window, and continues the same live terminal session from their own paired phone through a company-operated server. Session execution remains on the work computer.

The release delivers desktop session tabs and the mobile session list/terminal. The mobile chat concept is a later structured-agent integration and is explicitly outside this MVP.

## Status of decisions

- **Confirmed by the conversation:** workstation execution; Mac/Linux architecture; corporate relay; mandatory E2EE; own sessions only; user-level service; desktop and mobile UI direction.
- **Implementation baseline selected for this plan:** Go agent/server; tmux session engine; Tauri 2 + React/TypeScript installed clients; xterm.js; local SQLite and server PostgreSQL. These are engineering choices, not claims of prior user selection.
- **Confirmed release scope:** macOS desktop/host and iPhone mobile only for the first MVP. The user explicitly selected Mac + iPhone; Linux workstation support and Android follow after the pilot.
- **Early verification gates:** real-device mobile terminal usability; tmux attachment/redraw and resize behavior; interoperable E2EE and key storage; service restarts without terminating sessions.

Platform assumptions for estimating the pilot: Apple Silicon Mac and a physical iPhone. Exact OS versions and dependency versions are pinned in Phase 0, using actual supported environments. Architecture retains platform adapters for later Linux workstations and Android clients; their implementation and acceptance testing are outside MVP. The corporate relay may still run on Linux: deferred Linux support refers to employee workstations and the desktop client, not server hosting.

## Delivery sequence

| Phase | Outcome |
|---|---|
| P0 | Establish the new project and prove the risky technical paths |
| P1 | Durable local sessions and the host API |
| P2 | Desktop interface matching the approved tabs concept |
| P3 | Corporate identity, pairing, E2EE, and relay |
| P4 | Mobile interface and seamless reconnect/handoff |
| P5 | Installable pilot, failure testing, and release acceptance |

Each phase follows research → plan → implement → verify → review. Detailed execution plans are written immediately before their phase, using the previous phase's evidence. This document is a roadmap, not a fabricated set of already-executed GSD plans or tracker tickets.

## Deferred configuration inputs

These do not block architecture or planning: final product name; repository host/name; corporate OIDC provider; relay DNS/certificate/network exposure; Apple signing and distribution account; pilot participants. They must be supplied before their dependent delivery phase. Linux workstation packaging is a post-pilot input.

## Planning review

Reviewed for consistency across architecture, UI, and phases: chat is excluded from MVP; tab close is not process termination; all session-content operations are E2EE; local work survives relay outages; runtime persistence is bounded by workstation availability; server logs exclude terminal content; mobile control uses the host's ownership gate. Local document links and mockup copies are checked when producing this package. Runtime and security verification remain planned work.
