import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { TerminalDelivery, TerminalViewScope } from "./local-client.ts";
import type { CreateStatus, Inventory, LocalCreate, RuntimeInfo, TerminalScope, TerminalStatus, localClient } from "./local-client.ts";
import type { CreateExposure } from "./presentation-preferences.ts";
import { attachCurrentTerminal, canRestoreTerminalFocus, closeView, demoSessions, desiredTerminalMode, focusAfterClose,
  inventoryOpen, mountedTerminal, moveView, openView, parseScaffoldStatus, restoreFullscreenExitFocus, tabKey, tabStop,
  updateTerminalEligibility } from "./session-state.ts";
import type { AttachmentMode, BoundTerminal } from "./session-state.ts";
import { OrderedGeometry, ResizeProposals } from "./terminal-geometry.ts";

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

const runtime: RuntimeInfo = {
  runtime_id: "A", session_id: "S", pane_pid: 42, pane_id: "%2", operation_id: "op", workspace_id: "workspace",
  origin: "managed", exited: false, command: "codex", width: 80, height: 24,
};
const terminal: TerminalScope = { view_id: "view", agent_epoch: "agent", target: {
  runtime_id: "A", generation: "engine", session: "$0", window: "@1", pane: "%2", pid: 42, process_start: 123,
} };
const expected = { operationId: "op", sessionId: "S" };
const bound: BoundTerminal = { identity: { ...expected, agentPid: 12 }, terminal };
const inventory = (entry: RuntimeInfo = runtime, agent_pid = 12) => ({ agent_pid, runtimes: [entry] });
const fallback: RuntimeInfo = { ...runtime, origin: "unverified", workspace_id: null, operation_id: null };
const size = { cols: 80, rows: 24 };
const owner = { agent: terminal.agent_epoch, owner: "owner" };
function fixtureAttachment(viewId: string, scope = terminal, epoch: TerminalStatus["epoch"] = null) {
  const status: TerminalStatus = {
    terminal: { ...scope, view_id: viewId }, epoch, committed: { size, revision: 0, intent: null },
    provider: null, observation: "unavailable",
  };
  return { ...size, view_id: viewId, status };
}
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}

test("only the verified pane can keep a mounted view during metadata loss and restoration", () => {
  const live = mountedTerminal("A", inventory(), expected, bound, false);
  const pending = mountedTerminal("A", inventory(fallback), expected, null, false);
  const observing = mountedTerminal("A", inventory(fallback), expected, bound, false);
  const restored = mountedTerminal("A", inventory(), expected, bound, false);
  assert.equal(live.mode, "live");
  assert.equal(pending.mode, "pending");
  assert.equal(observing.mode, "observe");
  assert.equal(restored.mode, "live");
  assert.ok(live.key);
  assert.deepEqual([pending.key, observing.key, restored.key], [undefined, live.key, live.key]);
  assert.equal(mountedTerminal("A", null, expected, bound, false).key, live.key);
  for (const replaced of [
    { agent_pid: 12, runtimes: [] }, inventory({ ...fallback, pane_pid: 43 }), inventory({ ...fallback, pane_id: "%3" }),
    inventory({ ...fallback, session_id: "another" }), inventory({ ...runtime, operation_id: "another" }), inventory(runtime, 13),
  ]) {
    assert.equal(mountedTerminal("A", replaced, expected, bound, false).mode, "missing");
  }
  assert.equal(mountedTerminal("A", inventory(fallback), expected, bound, true).key, undefined);
  assert.notEqual(mountedTerminal("A", inventory(runtime, 13), expected, null, false).key, live.key);
});

test("sidebar open and saved restore admit corroborated legacy during another unknown create", () => {
  const legacy: RuntimeInfo = { ...runtime, runtime_id: "legacy", origin: "legacy", workspace_id: null, operation_id: null };
  const unknown: LocalCreate = { reference: "pending", workspace_id: "workspace", profile: "codex", presented: false,
    disposition: { result: "unknown", issue: null } };
  const status: CreateStatus = { operations: [unknown], issue: null };
  const completed: LocalCreate = { ...unknown, reference: "known",
    disposition: { result: "launched", runtime, current: "live", readiness: "unobserved" } };
  const closed: CreateExposure = { ...expected, runtimeId: runtime.runtime_id, operationId: "op",
    reference: completed.reference, state: "closed" };
  for (const source of ["saved", "user"] as const) {
    assert.equal(inventoryOpen(status, legacy, undefined, source), "inventory");
    assert.equal(inventoryOpen(null, legacy, undefined, source), "inventory");
    assert.equal(inventoryOpen(status, fallback, undefined, source), "blocked");
    assert.equal(inventoryOpen(status, runtime, undefined, source), "blocked");
    assert.equal(inventoryOpen(null, runtime, undefined, source), "blocked");
    assert.equal(inventoryOpen(status, { ...runtime, workspace_id: "unrelated" }, undefined, source), "inventory");
    assert.equal(inventoryOpen({ operations: [completed], issue: null }, runtime, undefined, source), completed);
    assert.equal(inventoryOpen({ operations: [completed], issue: null }, { ...runtime, pane_pid: 43 }, undefined, source), "blocked");
    assert.equal(inventoryOpen({ operations: [completed], issue: null }, runtime, closed, source),
      source === "saved" ? "blocked" : completed);
  }
});

async function verifyObserverHandoff(predecessorUnknown: boolean) {
  const delivery = new TerminalDelivery();
  const oldSend = deferred<void>();
  const oldDetached = deferred<void>();
  const detachedViews: string[] = [];
  const calls: unknown[] = [];
  const frames: number[][] = [];
  const retries: unknown[] = [];
  let currentInventory = inventory();
  let previous: BoundTerminal | null = null;
  let closed = false;
  let detached = Promise.resolve();
  let viewNumber = 0;
  const intent = () => mountedTerminal("A", currentInventory, expected, previous, closed).intent;
  const client: Pick<typeof localClient, "attach" | "detach"> = {
    async attach(id, options) {
      calls.push(["attach", id, options]);
      return fixtureAttachment(`view-${++viewNumber}`);
    },
    async detach(view) {
      detachedViews.push(view);
      if (view === "view-1") await oldDetached.promise;
    },
  };
  const start = async (mode: AttachmentMode) => {
    const proposals = new ResizeProposals();
    const scope = new TerminalViewScope({
      async input(view, data, incarnation) {
        calls.push(["input", view, data, incarnation]);
        if (view === "view-1") await oldSend.promise;
      },
      inputFailure: error => assert.fail(String(error)),
      paste: {
        stage: () => assert.fail("paste effect during handoff"), commit: () => assert.fail("paste commit during handoff"),
        ack: () => assert.fail("paste acknowledgement during handoff"), changed: () => {},
      },
      retire: view => {
        proposals.close();
        if (view) detached = client.detach(view);
      },
      reconnect: error => retries.push(error),
    }, delivery);
    scope.setInputEligible(mode === "ordinary");
    const admitted = await attachCurrentTerminal("A", previous, mode, intent, () => delivery.mayRestore,
      () => scope.active, detached, client);
    assert.ok(admitted);
    const { attachment, binding } = admitted;
    previous = binding;
    assert.ok(scope.bind(attachment.view_id));
    const ordered = new OrderedGeometry(attachment.status);
    const sample = (data: number[], incarnation?: string) => {
      const chunk = { closed: false, data, writable: incarnation !== undefined,
        status: { ...attachment.status, epoch: incarnation ? owner : null },
        ...(incarnation ? { paste_incarnation: incarnation } : {}) };
      return scope.consume(attachment.view_id, chunk, () => ordered.consume(chunk, {
        write: async bytes => { frames.push([...bytes]); },
        resize: () => assert.fail("unexpected geometry change"),
        acknowledge: () => assert.fail("unexpected geometry acknowledgement"),
      }));
    };
    return { binding, scope, sample, proposals, updateEligibility: (desired: AttachmentMode | undefined) =>
      updateTerminalEligibility(scope, mode, desired) };
  };

  const old = await start("ordinary");
  await old.sample([1], "old-grant");
  const sending = old.scope.send(Uint8Array.of(5));
  const queuedKey = old.scope.send(Uint8Array.of(6));
  const queuedPaste = old.scope.paste(Uint8Array.of(7));
  old.proposals.observe({ ...fixtureAttachment("view-1").status, epoch: owner }, true, 0);
  old.proposals.offer({ cols: 100, rows: 30 });
  await Promise.resolve();
  currentInventory = inventory(fallback);
  const observerMode = desiredTerminalMode(intent(), "ordinary", delivery.mayRestore);
  assert.equal(observerMode, "observer");
  old.updateEligibility(observerMode);
  assert.equal(old.proposals.take(), undefined);
  const observing = start("observer");
  await Promise.resolve();
  assert.equal(calls.length, 2, "replacement waited for predecessor detach");
  oldDetached.resolve();
  const observer = await observing;
  await observer.sample([2], "unexpected-incarnation");
  await observer.scope.send(Uint8Array.of(8));
  await observer.scope.paste(Uint8Array.of(9));
  assert.equal(observer.scope.canWrite, false);
  observer.scope.acceptClosure("view-2", { reason: "overflow", high_water: 16 });
  assert.equal(retries.length, 1);
  assert.equal(desiredTerminalMode(intent(), undefined, delivery.mayRestore), "observer");
  const redrawn = await start("observer");
  await redrawn.sample([3]);
  assert.deepEqual(redrawn.binding.terminal.target, old.binding.terminal.target);
  assert.deepEqual(frames, [[1], [2], [3]]);

  currentInventory = inventory();
  const callCount = calls.length;
  assert.equal(desiredTerminalMode(intent(), "observer", delivery.mayRestore), "observer");
  assert.equal(await attachCurrentTerminal("A", redrawn.binding, "ordinary", intent, () => delivery.mayRestore,
    () => true, detached, client), undefined);
  assert.equal(calls.length, callCount);
  if (predecessorUnknown) oldSend.reject(Error("input delivery unknown"));
  else oldSend.resolve();
  await Promise.all([sending, queuedKey, queuedPaste]);
  const restoredMode = desiredTerminalMode(intent(), "observer", delivery.mayRestore);
  if (predecessorUnknown) {
    assert.equal(restoredMode, "observer");
    redrawn.updateEligibility(restoredMode);
    assert.equal(await attachCurrentTerminal("A", redrawn.binding, "ordinary", intent, () => delivery.mayRestore,
      () => true, detached, client), undefined);
    assert(await redrawn.sample([4], "different-incarnation"));
    assert.equal(redrawn.scope.canWrite, false);
    await redrawn.scope.send(Uint8Array.of(10));
    await redrawn.scope.paste(Uint8Array.of(11));
    assert.equal(calls.length, callCount);
    assert.deepEqual(detachedViews, ["view-1", "view-2"]);
    return;
  }
  assert.equal(restoredMode, "ordinary");
  redrawn.updateEligibility(restoredMode);
  const restored = await start("ordinary");
  await restored.scope.send(Uint8Array.of(10));
  await restored.scope.paste(Uint8Array.of(11));
  assert.equal(restored.proposals.take(), undefined);
  await restored.sample([4]);
  assert.equal(restored.scope.canWrite, false);
  await restored.sample([5], "fresh-grant");
  assert.equal(restored.scope.canWrite, true);
  await restored.scope.send(Uint8Array.of(12));
  assert.deepEqual(detachedViews, ["view-1", "view-2", "view-3"]);
  assert.deepEqual(calls, [
    ["attach", "A", undefined], ["input", "view-1", [5], "old-grant"],
    ["attach", "A", { observer: true }], ["attach", "A", { observer: true }],
    ["attach", "A", { restoration: { previous: redrawn.binding.terminal, agent_pid: 12, session_id: "S", operation_id: "op" } }],
    ["input", "view-4", [12], "fresh-grant"],
  ]);
  closed = true;
  restored.updateEligibility(desiredTerminalMode(intent(), "ordinary", delivery.mayRestore));
  assert.equal(restored.scope.active, false);
  assert.equal(detachedViews[detachedViews.length - 1], "view-4");
}

for (const predecessorUnknown of [false, true]) {
  test(`catalog impairment and observer overflow ${predecessorUnknown ? "retain an unknown predecessor hold" : "restore a fresh view without replay"}`,
    () => verifyObserverHandoff(predecessorUnknown));
}

test("reattachment guards reject replacement, revoked eligibility and closed views before output", async () => {
  let current: Inventory = inventory();
  let active = true;
  let closed = false;
  let next = fixtureAttachment("fresh");
  const entered: string[] = [];
  const detached: string[] = [];
  let attachReply = async () => next;
  const client: Pick<typeof localClient, "attach" | "detach"> = {
    attach(id) { entered.push(id); return attachReply(); },
    async detach(view) { detached.push(view); },
  };
  const intent = () => mountedTerminal("A", current, expected, bound, closed).intent;
  const start = (mode: AttachmentMode, wait = Promise.resolve()) =>
    attachCurrentTerminal("A", bound, mode, intent, () => true, () => active, wait, client);
  const replacements = [
    { ...terminal, agent_epoch: "replacement" }, { ...terminal, target: { ...terminal.target, generation: "replacement" } },
    { ...terminal, target: { ...terminal.target, session: "$1" } }, { ...terminal, target: { ...terminal.target, window: "@2" } },
    { ...terminal, target: { ...terminal.target, pane: "%3" } }, { ...terminal, target: { ...terminal.target, pid: 43 } },
    { ...terminal, target: { ...terminal.target, process_start: 124 } },
  ];
  for (const mode of ["observer", "ordinary"] as const) {
    current = mode === "observer" ? inventory(fallback) : inventory();
    for (const replacement of replacements) {
      next = fixtureAttachment("replacement", replacement);
      await assert.rejects(start(mode), /Terminal attachment identity changed/);
      assert.equal(detached[detached.length - 1], "replacement");
    }
  }
  const count = entered.length;
  current = inventory(runtime, 13);
  assert.equal(await start("ordinary"), undefined);
  assert.equal(entered.length, count);

  current = inventory();
  const retiring = deferred<void>();
  const wait = start("ordinary", retiring.promise);
  closed = true;
  retiring.resolve();
  assert.equal(await wait, undefined);
  assert.equal(entered.length, count);
  closed = false;

  const enteredNative = deferred<void>();
  const reply = deferred<ReturnType<typeof fixtureAttachment>>();
  attachReply = () => { enteredNative.resolve(); return reply.promise; };
  const attaching = start("ordinary");
  await enteredNative.promise;
  current = inventory(fallback);
  reply.resolve(fixtureAttachment("revoked"));
  assert.equal(await attaching, undefined);
  assert.equal(detached[detached.length - 1], "revoked");
  active = false;
  assert.equal(await start("observer"), undefined);
  assert.equal(entered.length, count + 1);
});

test("the sidebar, saved restore and mounted renderer use the checked production paths", () => {
  const app = readFileSync(new URL("./App.tsx", import.meta.url), "utf8");
  const view = readFileSync(new URL("./TerminalView.tsx", import.meta.url), "utf8");
  const native = readFileSync(new URL("../src-tauri/src/lib.rs", import.meta.url), "utf8");
  const bridge = readFileSync(new URL("../../../crates/desktop-bridge/src/lib.rs", import.meta.url), "utf8");
  assert.match(app, /inventoryOpen\(status, latest, store.exposure\(id\), "user"\)/);
  assert.match(app, /inventoryOpen\(status, observed, exposure, "saved"\)/);
  assert.match(app, /restoreSaved\(null\);\s*void localClient.createStatus\(\)/);
  assert.match(app, /<TerminalView[\s\S]+previousBinding=\{previousBinding\}[\s\S]+currentAttachment=\{currentAttachment\}/);
  assert.match(view, /updateEligibility.current\?\.\(mode\)/);
  assert.match(view, /updateEligibility.current = desired => updateTerminalEligibility\(scope, attachmentMode, desired\)/);
  assert.match(view, /if \(view\) detached.current = detach\(view, failure\)/);
  assert.match(view, /if \(currentAttachment\(runtimeId, previousRef.current\)\) setRetry/);
  assert.match(view, /attachCurrentTerminal\(runtimeId, previous, attachmentMode,[\s\S]+detached.current, localClient\)/);
  assert.match(view, /if \(!candidate\)[\s\S]+scope.bind\(view\)[\s\S]+localClient.read\(view\)/);
  assert.match(native, /bridge.attach_observer\(&runtime_id\)[\s\S]+bridge.restore_attachment\(&runtime_id, expected\)/);
  assert.match(bridge, /mode.check_opened\(&status.terminal\)[\s\S]+let status = prove_attachment\(&view, &status\).await\?[\s\S]+mode.prepare_control\(&view, &status\)[\s\S]+control.start\(&view\)/);
});
