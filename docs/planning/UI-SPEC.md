# MVP UI contract

Version: 0.2 · 2026-11-21

The user approved the desktop and mobile concepts in this conversation. Preserve their information architecture and interactions; terminal sample text is illustrative, not a reproduction requirement for third-party CLI output.

First-release acceptance applies to the macOS desktop application and the iPhone application. Linux desktop and Android adaptations follow after the pilot; preserve shared UI contracts without implementing those platform shells now.

References: [desktop fragment](design/desktop-concept.html), [mobile fragment](design/mobile-concept.html).

## Shared language and visual direction

- Initial interface language: Ukrainian; product code and engineering docs may remain English.
- Quiet terminal-workspace design, light/dark appearance, restrained green accent for connection/selection, readable monospace terminal text.
- Status is always explained by text, not color alone.
- Keep technical infrastructure details out of daily workflows. Show actionable connection/readiness errors when needed.
- Only one controller. Each connected view always knows whether it can type.
- Session names and project groups agree across devices. Open views/tab order are device-specific.
- Use the real xterm.js renderer for application output; do not turn CLI output into styled cards or pretend a CLI is a chat backend.

## Desktop

| Area | MVP behavior |
|---|---|
| Host identity | Work computer name, local/remote connection context, availability |
| Sidebar | Workspace groups and all non-archived sessions, including sessions with closed views |
| Tab bar | Open session views; select, keyboard navigate, reorder, close; plus button creates a session |
| Workspace strip | Workspace identity, connection state, optional local Git branch metadata |
| Main area | Live terminal for the selected session; ordinary keyboard, selection, clipboard, terminal resize |
| Remote-device control | Own paired phone presence; explicit handoff; obvious read-only state while phone controls |
| Footer | Actual runtime state, controller, local/remote connection context |

The working name “Sessions” is a placeholder. Final branding does not block implementation.

Desktop acceptance requirements:

- D1: Switching tabs changes the selected session's output, workspace, and controller without cross-session input leakage.
- D2: Closing a tab detaches only that view. The session remains in the sidebar and can be reopened with the same runtime ID.
- D3: Closing every tab shows an empty-view state while live sessions remain listed.
- D4: Closing the app window leaves the user-level host service and tmux runtime alive.
- D5: A separate Stop session action names the target and confirms termination. It is never attached to the tab-close cross.
- D6: Phone control makes input/resize unavailable on desktop until control returns; it does not hide terminal output.
- D7: Local desktop use and new local sessions work during relay failure, subject to the employee's local permissions and configured policy.
- D8: Git branch is optional read-only workspace metadata, not source-control management. Its absence is not an error.
- D9: Persist local open-tab selection/order and readable font size. Rename updates session metadata; tab close does not rename/archive/delete.

## Mobile

| Screen | MVP behavior |
|---|---|
| Hosts and sessions | Own enrolled computers, availability, project groups, named sessions, real runtime state |
| Terminal | The selected live terminal, workspace/host label, current controller, explicit Take control action |
| Input accessory | Esc, Tab, arrow keys, Ctrl modifier/Ctrl+C; OS keyboard, paste and selection |
| New session | Select own host, registered workspace, launch profile, optional name |
| Paired devices | Current pairing identity, connectivity status, revoke/remove action |

Mobile acceptance requirements:

- M1: A physical phone can read, select, scroll, paste, and enter Ukrainian/English text without duplicated keystrokes or composition corruption.
- M2: Software keyboard appearance does not cover the active input/cursor. Rotation and safe areas are handled.
- M3: A desktop-controlled terminal opens read-only; Take control obtains host acknowledgment before enabling input.
- M4: A read-only phone does not resize the desktop CLI. Taking control can resize the terminal, with a visible and predictable transition.
- M5: Backgrounding/locking the phone is treated as disconnection when needed. Foregrounding reconnects and redraws; no permanent mobile background socket is required.
- M6: History browsing is visibly separate from live terminal state. Missing retained history is labeled honestly; it does not fabricate output.
- M7: Ctrl+C sends an interrupt only while this client owns input. It is distinct from Stop session.
- M8: Reconnect never resubmits unconfirmed terminal input. A draft may remain editable locally, but submission requires a fresh user action.
- M9: Session creation fails clearly for unreachable hosts, missing CLIs, invalid workspaces, or locked credentials; no invisible queue that starts work later.
- M10: Terminal font sizing/zoom and sufficient touch targets support real phone use; do not shrink the entire desktop layout onto the phone.

The mobile concept's “Chat · later” is not a disabled production tab in MVP. Omit it from the shipped navigation. No structured Approve/Reject buttons, agent task-progress inference, or diff cards are included. Native CLI approval screens remain usable through terminal input.

## Required supporting flows

| Flow | Contract |
|---|---|
| First host setup | Corporate sign-in, host enrollment, workspace registration, CLI/tmux/environment diagnostics |
| First phone setup | Sign-in as same owner; QR pairing and trusted-host confirmation |
| New session | Name + workspace + launch profile; one submission creates at most one runtime |
| Reconnect | Show connecting/stale status, restore from host authority, then enable appropriate controls |
| Host unreachable | Last known state with timestamp; no live badge, no claim that process exited |
| Credential/key store locked | Explain that unlock/setup on host is needed; do not silently weaken storage |
| CLI exited | Exit status and retained session record; explicit new run, no automatic resurrection |
| Key changed/unpaired/revoked | Stop remote interaction; require verified re-pairing where applicable |
| Stop session | Confirm exact target and effect; report final state, not just request acceptance |

These complete the happy-path mockups; they do not change their core layout.

## Accessibility and test approach

Native keyboard navigation and focus restoration; semantic tabs/dialogs; distinguishable focus; readable contrast; text labels for connection/control states; platform screen-reader support for application chrome and supported terminal accessibility. Do not announce every output byte as a live-region event.

Test components for routing/state mistakes, then validate actual native WebViews on target desktop systems and a physical phone. Browser-only screenshots and emulators are insufficient evidence for keyboard, key-store, service lifecycle, and background/resume behavior.

The mockups contain demo input boxes and simplified buttons. The implementation must use actual terminal input semantics, support IME composition and control keys, and preserve the accepted interaction model without copying demo handlers into production.
