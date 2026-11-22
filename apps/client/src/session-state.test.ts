import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { canRestoreTerminalFocus, closeView, demoSessions, focusAfterClose, moveView, openView, parseScaffoldStatus, tabKey, tabStop } from "./session-state.ts";

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

test("native boundary validates the shared status fixture and fails closed", () => {
  const fixture: unknown = JSON.parse(readFileSync(new URL("../../../protocol/fixtures/scaffold-status.json", import.meta.url), "utf8"));
  assert.equal(parseScaffoldStatus(fixture).stage, "scaffold");
  for (const bad of [null, {}, { protocolVersion: 2, appVersion: "0.1.0", stage: "scaffold", capabilities: [] }, { protocolVersion: 1, appVersion: "0.1.0", stage: "scaffold", capabilities: ["remote-exec"] }]) {
    assert.throws(() => parseScaffoldStatus(bad));
  }
});
