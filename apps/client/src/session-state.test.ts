import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import type { RuntimeInfo } from "./local-client.ts";
import { canRestoreTerminalFocus, closeView, demoSessions, focusAfterClose, mountedTerminal, moveView, openView, parseScaffoldStatus, restoreFullscreenExitFocus, tabKey, tabStop } from "./session-state.ts";

test("closing every view preserves the session catalog and allows reopening", () => {
  const catalogBefore = structuredClone(demoSessions);
  let views = { open: demoSessions.map(s => s.id), active: "claude-auth" as string | null };
  for (const session of demoSessions) {
    const next = closeView(views, session.id);
    views = { open: [...next.open], active: next.active };
  }
  assert.equal(views.active, null);
  assert.equal(views.open.length, 0);
  assert.deepEqual(demoSessions, catalogBefore);
  assert.deepEqual(openView(views, "codex-tests"), { open: ["codex-tests"], active: "codex-tests" });
});

test("selecting an existing view does not duplicate it or accept an unknown session", () => {
  const views = { open: ["claude-auth"], active: "claude-auth" };
  assert.deepEqual(openView(views, "claude-auth"), views);
  assert.deepEqual(openView(views, "unknown"), views);
  assert.equal(closeView({ open: ["claude-auth", "codex-tests"], active: "claude-auth" }, "codex-tests").active, "claude-auth");
});

const threeTabs = { open: ["A", "B", "C"], active: "B" };

test("arrows wrap and Home and End move roving focus without selecting a tab", () => {
  assert.equal(tabStop(threeTabs, null), "B");
  assert.equal(tabStop({ ...threeTabs, active: null }, null), "A");
  assert.deepEqual(tabKey(threeTabs, "A", "ArrowLeft"), { action: "focus", id: "C" });
  assert.deepEqual(tabKey(threeTabs, "C", "ArrowRight"), { action: "focus", id: "A" });
  assert.deepEqual(tabKey(threeTabs, "B", "Home"), { action: "focus", id: "A" });
  const target = tabKey(threeTabs, "B", "End");
  assert.deepEqual(target, { action: "focus", id: "C" });
  assert.equal(tabStop(threeTabs, target?.id ?? null), "C");
  assert.equal(threeTabs.active, "B");
  for (const key of ["Enter", " "]) {
    const activation = tabKey(threeTabs, "C", key);
    assert.ok(activation);
    assert.equal(activation.action, "activate");
    assert.equal(openView(threeTabs, activation.id, threeTabs.open).active, "C");
  }
  assert.equal(tabKey(threeTabs, "B", "ArrowDown"), undefined);
  assert.equal(tabKey(threeTabs, "removed", "End"), undefined);
});

test("adjacent reorder preserves active and focused identities and rejects edge moves", () => {
  const moved = moveView(moveView(threeTabs, "C", -1), "C", -1);
  assert.deepEqual(moved, { open: ["C", "A", "B"], active: "B" });
  assert.equal(tabStop(moved, "C"), "C");
  assert.equal(tabStop(moved, "A"), "A");
  assert.equal(moveView(moved, "C", -1), moved);
  assert.equal(moveView(moved, "B", 1), moved);
  assert.equal(moveView(moved, "removed", 1), moved);
  assert.deepEqual(moveView(moved, "C", 1), { open: ["A", "C", "B"], active: "B" });
});

test("closing a tab falls back to its next neighbor, then its previous one", () => {
  assert.deepEqual(closeView(threeTabs, "A"), { open: ["B", "C"], active: "B" });
  assert.equal(focusAfterClose(threeTabs, "A", "C"), "C");
  assert.equal(focusAfterClose(threeTabs, "A", "A"), "B");
  const closed = closeView(threeTabs, "B");
  assert.deepEqual(closed, { open: ["A", "C"], active: "C" });
  assert.equal(tabStop(closed, focusAfterClose(threeTabs, "B", "A")), "A");
  assert.equal(focusAfterClose(threeTabs, "C", "C"), "B");
  assert.equal(closeView(closed, "C").active, "A");
  const last = { open: ["C"], active: "C" };
  assert.equal(tabStop(closeView(last, "C"), focusAfterClose(last, "C", "C")), null);
  assert.equal(closeView(last, "removed"), last);
});

test("fullscreen exit restores focus only to the current mounted terminal", () => {
  const textarea = { isConnected: true };
  const captured = { runtimeId: "A", epoch: 12, textarea };

  assert.equal(canRestoreTerminalFocus(captured, { ...captured }, true), true);
  assert.equal(canRestoreTerminalFocus(captured, captured, false), false);
  assert.equal(canRestoreTerminalFocus(captured, null, true), false);
  assert.equal(canRestoreTerminalFocus(captured, { ...captured, runtimeId: "B" }, true), false);
  assert.equal(canRestoreTerminalFocus(captured, { ...captured, epoch: 13 }, true), false);
  assert.equal(canRestoreTerminalFocus(captured, { ...captured, textarea: { isConnected: true } }, true), false);
  textarea.isConnected = false;
  assert.equal(canRestoreTerminalFocus(captured, captured, true), false);
});

test("fullscreen exit hands keyboard focus to the newer dialog target after native focus", async () => {
  const order: string[] = [];
  const terminal = { focus: () => assert.fail("old terminal received focus") };
  const dialog = { focus: (options?: FocusOptions) => {
    assert.deepEqual(options, { preventScroll: true });
    order.push("dialog");
  } };
  let current: Pick<HTMLElement, "focus"> = terminal;
  const settle = () => restoreFullscreenExitFocus(() => current, async () => {
    order.push("webview");
    return true;
  }, () => true);
  current = dialog;
  await settle();
  assert.deepEqual(order, ["webview", "dialog"]);
});

test("fullscreen exit skips DOM focus when the target or window changes during native focus", async () => {
  const stale = { focus: () => assert.fail("stale target received focus") };
  const newer = { focus: () => assert.fail("newer target was changed by the older handoff") };
  let current: Pick<HTMLElement, "focus"> | null = stale;
  await restoreFullscreenExitFocus(() => current, async () => {
    current = newer;
    return true;
  }, () => true);
  current = stale;
  await restoreFullscreenExitFocus(() => current, async () => {
    current = null;
    return true;
  }, () => true);
  await restoreFullscreenExitFocus(() => stale, async () => false, () => true);
  await restoreFullscreenExitFocus(() => stale, async () => true, () => false);
  await restoreFullscreenExitFocus(() => null, async () => assert.fail("native focus without a target"), () => true);
});

test("native boundary validates the shared status fixture and fails closed", () => {
  const fixture: unknown = JSON.parse(readFileSync(new URL("../../../protocol/fixtures/scaffold-status.json", import.meta.url), "utf8"));
  assert.equal(parseScaffoldStatus(fixture).stage, "scaffold");
  for (const bad of [null, {}, { protocolVersion: 2, appVersion: "0.1.0", stage: "scaffold", capabilities: [] }, { protocolVersion: 1, appVersion: "0.1.0", stage: "scaffold", capabilities: ["remote-exec"] }]) {
    assert.throws(() => parseScaffoldStatus(bad));
  }
});

test("only the current checked target can keep its renderer during metadata loss and restoration", () => {
  const runtime = { runtime_id: "A", session_id: "S", pane_pid: 42, pane_id: "%2", operation_id: "op", exited: false,
    command: "codex", width: 80, height: 24 };
  const terminal = { view_id: "view", agent_epoch: "agent", target: {
    runtime_id: "A", generation: "engine", session: "$0", window: "@1", pane: "%2", pid: 42, process_start: 123,
  } };
  const bound = { agentPid: 12, terminal };
  const inventory = (entry: RuntimeInfo = runtime, agent_pid = 12) => ({ agent_pid, runtimes: [entry] });
  const fallback = { ...runtime, operation_id: null };
  const expected = { operationId: "op", sessionId: "S" };
  const live = mountedTerminal("A", inventory(), expected, bound, undefined, false);
  const pending = mountedTerminal("A", inventory(fallback), expected, bound, undefined, true);
  const observing = mountedTerminal("A", inventory(fallback), expected, bound, terminal, true);
  const restored = mountedTerminal("A", inventory(), expected, bound, terminal, true);
  assert.equal(live.mode, "live");
  assert.equal(pending.mode, "pending");
  assert.equal(observing.mode, "observe");
  assert.equal(restored.mode, "live");
  assert.equal(mountedTerminal("A", inventory(), expected, bound, undefined, true).mode, "pending");
  assert.equal(mountedTerminal("A", null, expected, bound, undefined, true).key, live.key);
  assert.ok(live.key);
  assert.deepEqual([pending.key, observing.key, restored.key], [live.key, live.key, live.key]);
  assert.equal(mountedTerminal("A", inventory(fallback), expected, null, undefined, true).key, undefined);
  assert.equal(mountedTerminal("A", { agent_pid: 12, runtimes: [] }, expected, bound, terminal, true).mode, "missing");
  assert.equal(mountedTerminal("A", inventory({ ...fallback, pane_pid: 43 }), expected, bound, terminal, true).mode, "missing");
  assert.equal(mountedTerminal("A", inventory({ ...fallback, session_id: "another" }), expected, bound, terminal, true).mode, "missing");
  assert.equal(mountedTerminal("A", inventory({ ...runtime, operation_id: "another" }), expected, bound, terminal, true).mode, "missing");
  assert.equal(mountedTerminal("A", inventory(runtime, 13), expected, bound, terminal, true).mode, "missing");
  assert.equal(mountedTerminal("A", inventory(fallback), expected, bound, null, true).mode, "missing");
  assert.notEqual(mountedTerminal("A", inventory(runtime, 13), expected, null, undefined, false).key, live.key);
  for (const changed of [
    { ...terminal, view_id: "replacement" },
    { ...terminal, agent_epoch: "replacement" },
    { ...terminal, target: { ...terminal.target, generation: "replacement" } },
    { ...terminal, target: { ...terminal.target, pane: "%3" } },
    { ...terminal, target: { ...terminal.target, process_start: 124 } },
  ]) assert.equal(mountedTerminal("A", inventory(fallback), expected, bound, changed, true).mode, "missing");
});
