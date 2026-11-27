import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import type { TestContext } from "node:test";
import xterm from "@xterm/xterm";
import { localClient, TerminalDelivery, TerminalViewScope } from "./local-client.ts";
import type { CreateStatus, Inventory, LocalCreate, RuntimeInfo, TerminalScope, TerminalStatus } from "./local-client.ts";
import type { CreateExposure } from "./presentation-preferences.ts";
import { attachCurrentTerminal, canRestoreTerminalFocus, closeView, demoSessions, desiredTerminalMode, focusAfterClose,
  explicitTerminalOpen, inventoryBindingState, inventoryOpen, mountedTerminal, moveView, openView, parseScaffoldStatus,
  restoreFullscreenExitFocus, runMountedTerminalStream, tabKey, tabStop, terminalPanel, terminalStreamAccess,
  updateMountedStream } from "./session-state.ts";
import type { AttachmentMode, BoundTerminal, MountedTerminal, SessionViews, TerminalBindingState } from "./session-state.ts";
import { OrderedGeometry, ResizeProposals } from "./terminal-geometry.ts";
import { bindTerminalProtocol } from "./terminal-protocol.ts";

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
const bound: BoundTerminal = { identity: { ...expected, agentPid: 12, workspaceId: "workspace" }, terminal, mountKey: "A:12" };
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
    inventory({ ...fallback, session_id: "another" }), inventory({ ...runtime, operation_id: "another" }),
  ]) {
    assert.equal(mountedTerminal("A", replaced, expected, bound, false).mode, "missing");
  }
  assert.equal(mountedTerminal("A", inventory(runtime, 13), expected, bound, false).mode, "rollover");
  assert.equal(mountedTerminal("A", inventory(fallback, 13), expected, bound, false).mode, "pending");
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
  const client: Pick<typeof localClient, "attach" | "detach" | "inventory"> = {
    async inventory() { return currentInventory; },
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
    return { binding, scope, sample, proposals, updateEligibility: () =>
      updateMountedStream({ scope, revokeMutations: () => proposals.close(),
        access: () => terminalStreamAccess(mountedTerminal("A", currentInventory, expected, previous, closed),
          binding, mode, delivery.mayRestore) }) };
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
  old.updateEligibility();
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
    redrawn.updateEligibility();
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
  redrawn.updateEligibility();
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
  restored.updateEligibility();
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
  const client: Pick<typeof localClient, "attach" | "detach" | "inventory"> = {
    async inventory() { return current; },
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

const fixtureId = (number: number) => `10000000-0000-4000-8000-${String(number).padStart(12, "0")}`;
const nativeRuntime: RuntimeInfo = {
  ...runtime, runtime_id: fixtureId(1), session_id: fixtureId(2), workspace_id: fixtureId(3), operation_id: fixtureId(4),
};
const nativeTarget = { ...terminal.target, runtime_id: nativeRuntime.runtime_id, generation: fixtureId(5) };
const nativeAgent = fixtureId(6);
const restartedAgent = fixtureId(7);
const firstView = fixtureId(8);
const observedView = fixtureId(9);
const finalView = fixtureId(10);
const nativeIncarnation = fixtureId(11);
const newIncarnation = fixtureId(12);
const nativeOwner = fixtureId(13);
const secondTab = fixtureId(14);
const createdRef = fixtureId(15);
const runtimeId = nativeRuntime.runtime_id;
const nativeAttachment = (view: string, agent = nativeAgent, target = nativeTarget) =>
  fixtureAttachment(view, { view_id: view, agent_epoch: agent, target });
const nativeData = (data: number[], sample?: ReturnType<typeof nativeAttachment>, incarnation = nativeIncarnation) => ({
  data, closed: false, writable: sample !== undefined, geometry: null,
  status: sample ? { ...sample.status, epoch: { agent: sample.status.terminal.agent_epoch, owner: nativeOwner } } : null,
  ...(sample ? { paste_incarnation: incarnation } : {}),
});
const nativeClosure = (closure: object) => ({ data: [], closed: true, writable: false, status: null, geometry: null, closure });

function mountedNativeFixture(t: TestContext, row = nativeRuntime) {
  let latest: Inventory | null = null;
  let replyInventory = inventory(row);
  let views: SessionViews = { open: [runtimeId, secondTab], active: runtimeId };
  let binding: BoundTerminal | null = null;
  let state: TerminalBindingState | undefined;
  let attempt = 0;
  const identity = { sessionId: row.session_id, operationId: row.operation_id ?? null };
  const calls: { command: string; args: unknown }[] = [];
  const steps = new Map<string, ((args: unknown) => Promise<unknown>)[]>();
  const cleanup: (() => Promise<void>)[] = [];
  const delivery = new TerminalDelivery();
  const counts = (command: string) => calls.filter(call => call.command === command).map(call => call.args);
  const original = Object.getOwnPropertyDescriptor(globalThis, "window");
  Object.defineProperty(globalThis, "window", {
    configurable: true,
    value: { __TAURI_INTERNALS__: {
      async invoke(command: string, args: unknown) {
        calls.push({ command, args });
        const step = steps.get(command)?.shift();
        if (step) return step(args);
        if (command === "local_inventory") return replyInventory;
        if (command === "terminal_detach") return;
        assert.fail(`unexpected native invoke ${command}`);
      },
    } },
  });
  t.after(async () => {
    try {
      for (const close of cleanup.reverse()) await close();
    } finally {
      if (original) Object.defineProperty(globalThis, "window", original);
      else Reflect.deleteProperty(globalThis, "window");
    }
  });
  function park(command: string) {
    const entered = deferred<unknown>();
    const reply = deferred<unknown>();
    const queue = steps.get(command) ?? [];
    queue.push(args => { entered.resolve(args); return reply.promise; });
    steps.set(command, queue);
    return { entered: entered.promise, resolve: reply.resolve, reject: reply.reject };
  }
  const selected = () => terminalPanel(views, runtimeId, latest, identity, binding, false, state);
  async function publish(row: RuntimeInfo, pid = 12) {
    replyInventory = inventory(row, pid);
    latest = await localClient.inventory();
    if (binding && state) state = inventoryBindingState(binding, state, latest);
    return selected();
  }
  async function connect(mode: AttachmentMode) {
    const previous = binding;
    const started = ++attempt;
    const current = (): MountedTerminal => started === attempt && (binding === previous || binding === accepted)
      ? selected() : { mode: "missing" };
    let accepted: BoundTerminal | undefined;
    const detachments: Promise<void>[] = [];
    const displayed = new xterm.Terminal({ ...size, allowProposedApi: true, disableStdin: true });
    const proposals = new ResizeProposals();
    const frames: number[][] = [];
    const failures: unknown[] = [];
    let closed = 0;
    let completed = 0;
    let disposed = false;
    let detachmentFailure: "stream_closed" | undefined;
    let beforeRender: (() => Promise<void>) | undefined;
    const dispose = () => {
      if (disposed) return;
      disposed = true;
      protocol.dispose();
      displayed.dispose();
    };
    const access = (): ReturnType<typeof terminalStreamAccess> => accepted
      ? terminalStreamAccess(current(), accepted, mode, delivery.mayRestore) : "stale";
    const scope = new TerminalViewScope({
      input: (viewId, data, incarnation) => localClient.input({ viewId, data, incarnation }),
      inputEligible: () => mode === "ordinary" && access() === "attached",
      inputFailure: error => failures.push(error),
      paste: { stage: localClient.pasteStage, commit: localClient.pasteCommit, ack: localClient.pasteAck, changed() {} },
      reconnect: error => failures.push(error),
      retire(view) {
        proposals.close();
        protocol.pauseInput();
        if (view) detachments.push(localClient.detach(view, detachmentFailure));
        if (binding === accepted && started === attempt && state !== "completed" && state !== "changed") {
          state = state === "draining" ? "ended" : "retired";
        }
        if (completed === 0) dispose();
      },
    }, delivery);
    const protocol = bindTerminalProtocol(displayed, bytes => { void scope.send(bytes); });
    scope.setInputEligible(mode === "ordinary");
    cleanup.push(async () => {
      scope.retire();
      dispose();
      await Promise.all(detachments);
    });
    const candidate = await attachCurrentTerminal(runtimeId, previous, mode, () => current().intent,
      () => delivery.mayRestore, () => scope.active, Promise.resolve(), localClient);
    if (!candidate) {
      scope.retire();
      return;
    }
    assert.ok(candidate.isCurrent());
    const { attachment } = candidate;
    const view = attachment.view_id;
    assert.ok(scope.bind(view));
    accepted = candidate.binding;
    binding = accepted;
    state = "bound";
    const routing = new OrderedGeometry(attachment.status);
    const eligibility = { scope, access, revokeMutations() { proposals.close(); protocol.pauseInput(); } };
    return {
      binding: accepted, scope, displayed, frames, failures, proposals,
      update: () => updateMountedStream(eligibility),
      async run() {
        try {
          await runMountedTerminalStream({
            ...eligibility, binding: candidate.binding, routing,
            read: localClient.read,
            consume: (view, chunk, render) => protocol.consume(scope, view, chunk, render),
            sink: {
              async write(bytes) {
                frames.push([...bytes]);
                await beforeRender?.();
                await new Promise<void>(resolve => displayed.write(Uint8Array.from(bytes), resolve));
              },
              resize: next => { displayed.resize(next.cols, next.rows); return next; },
              acknowledge: (event, next) => localClient.acknowledgeGrid(view, {
                renderer_id: fixtureId(16), epoch: event.status.epoch, revision: event.status.committed.revision,
                size: next, focused: false,
              }),
            },
            rendered: () => {
              if (access() === "attached") proposals.observe(routing.status, scope.canWrite, routing.applied);
            },
            completed() { completed++; state = "completed"; },
            closed() { closed++; detachmentFailure = "stream_closed"; },
            identityChanged() { state = "changed"; },
          });
        } catch (error) {
          if (scope.active) scope.retire({ error });
        }
        await Promise.all(detachments);
      },
      async flushResize() {
        const proposal = scope.canWrite ? proposals.take() : undefined;
        if (proposal) await localClient.resize(proposal.terminal.view_id, proposal.epoch, proposal.size);
      },
      beforeRender(callback: () => Promise<void>) { beforeRender = callback; },
      completed: () => completed,
      closed: () => closed,
      disposed: () => disposed,
    };
  }
  return {
    calls, counts, park, publish, selected, connect, delivery,
    binding: () => binding, state: () => state,
    select(id: string) { views = openView(views, id, views.open); },
    close() { views = closeView(views, runtimeId); },
    retireAttempt() { attempt++; },
    nextInventory(row: RuntimeInfo, pid: number) { replyInventory = inventory(row, pid); },
  };
}

type MountedFixture = ReturnType<typeof mountedNativeFixture>;
async function connectFixture(fixture: MountedFixture, mode: AttachmentMode, sample = nativeAttachment(firstView)) {
  const proof = fixture.park("terminal_attach");
  const opening = fixture.connect(mode);
  assert.ok(await proof.entered);
  proof.resolve(sample);
  const mounted = await opening;
  assert.ok(mounted);
  return mounted;
}

function mutatingCommands(fixture: MountedFixture) {
  return fixture.calls.filter(({ command }) => /input|paste|resize|acknowledge_grid|create_session|stop/.test(command));
}

for (const unknown of [false, true]) {
  test(`inventory-first restart proves the original pane and ${unknown ? "keeps an unknown delivery held" : "restores fresh control without replay"}`, async t => {
    const fixture = mountedNativeFixture(t);
    await fixture.publish(nativeRuntime);
    const original = await connectFixture(fixture, "ordinary");
    const live = fixture.park("terminal_read");
    const parked = fixture.park("terminal_read");
    const originalRead = original.run();
    assert.deepEqual(await live.entered, { viewId: firstView });
    live.resolve(nativeData([65], nativeAttachment(firstView)));
    assert.deepEqual(await parked.entered, { viewId: firstView });
    assert.equal(original.scope.canWrite, true);
    const predecessor = fixture.park("terminal_input");
    const sending = original.scope.send(Uint8Array.of(1));
    assert.deepEqual(await predecessor.entered, { viewId: firstView, data: [1], incarnation: nativeIncarnation });
    const queuedKey = original.scope.send(Uint8Array.of(2));
    const queuedPaste = original.scope.paste(Uint8Array.of(3));
    original.proposals.offer({ cols: 100, rows: 30 });

    const rollover = await fixture.publish(nativeRuntime, 13);
    assert.equal(rollover.mode, "rollover");
    assert.equal(rollover.key, original.binding.mountKey);
    assert.equal(original.scope.canWrite, false);
    await original.scope.send(Uint8Array.of(4));
    await original.flushResize();
    assert.equal(original.update(), "stale");
    assert.equal(original.proposals.take(), undefined);
    assert.equal(original.disposed(), true);
    assert.deepEqual(fixture.counts("terminal_detach"), [{ viewId: firstView, failure: undefined }]);
    assert.equal(fixture.binding(), original.binding);

    const observer = await connectFixture(fixture, "observer", nativeAttachment(observedView, restartedAgent));
    assert.deepEqual(fixture.counts("terminal_attach"), [{ runtimeId }, { runtimeId, observer: true }]);
    assert.deepEqual(fixture.calls.slice(-2).map(call => call.command), ["terminal_attach", "local_inventory"]);
    assert.notEqual(observer.binding.terminal.view_id, original.binding.terminal.view_id);
    assert.notEqual(observer.binding.terminal.agent_epoch, original.binding.terminal.agent_epoch);
    assert.deepEqual(observer.binding.terminal.target, original.binding.terminal.target);
    assert.equal(observer.binding.identity.agentPid, 13);
    if (unknown) parked.reject(Error("predecessor detached"));
    else parked.resolve(nativeData([90], nativeAttachment(firstView)));
    await originalRead;
    assert.deepEqual(original.frames, [[65]]);
    assert.equal(fixture.binding(), observer.binding);
    assert.equal(fixture.state(), "bound");
    assert.deepEqual(mutatingCommands(fixture).map(call => call.command), ["terminal_input"]);

    if (unknown) predecessor.reject(Error("delivery unknown"));
    else predecessor.resolve(undefined);
    await Promise.all([sending, queuedKey, queuedPaste]);
    if (unknown) {
      assert.equal(fixture.delivery.held, true);
      const read = fixture.park("terminal_read");
      const ended = fixture.park("terminal_read");
      const streaming = observer.run();
      assert.deepEqual(await read.entered, { viewId: observedView });
      read.resolve(nativeData([66], nativeAttachment(observedView, restartedAgent), newIncarnation));
      await ended.entered;
      assert.deepEqual(observer.frames, [[66]]);
      assert.equal(observer.scope.canWrite, false);
      await observer.scope.send(Uint8Array.of(5));
      await observer.scope.paste(Uint8Array.of(6));
      ended.resolve(nativeClosure({ reason: "detached" }));
      await streaming;
      assert.deepEqual(mutatingCommands(fixture).map(call => call.command), ["terminal_input"]);
      return;
    }

    assert.equal(observer.update(), "stale");
    const restored = await connectFixture(fixture, "ordinary", nativeAttachment(finalView, restartedAgent));
    assert.deepEqual(fixture.counts("terminal_attach")[2], { runtimeId, restoration: {
      previous: observer.binding.terminal, agent_pid: 13, session_id: nativeRuntime.session_id,
      operation_id: nativeRuntime.operation_id,
    } });
    assert.equal(restored.scope.canWrite, false);
    const granted = fixture.park("terminal_read");
    const ended = fixture.park("terminal_read");
    const streaming = restored.run();
    await granted.entered;
    granted.resolve(nativeData([67], nativeAttachment(finalView, restartedAgent), newIncarnation));
    await ended.entered;
    const fresh = fixture.park("terminal_input");
    const freshSend = restored.scope.send(Uint8Array.of(7));
    assert.deepEqual(await fresh.entered, { viewId: finalView, data: [7], incarnation: newIncarnation });
    fresh.resolve(undefined);
    await freshSend;
    ended.resolve(nativeClosure({ reason: "eof" }));
    await streaming;
    assert.deepEqual(fixture.counts("terminal_input"), [
      { viewId: firstView, data: [1], incarnation: nativeIncarnation },
      { viewId: finalView, data: [7], incarnation: newIncarnation },
    ]);
    assert.deepEqual(mutatingCommands(fixture).map(call => call.command), ["terminal_input", "terminal_input"]);
  });
}

for (const backedByStatus of [false, true]) {
  test(`explicit same-row retired reopen retains the target through ${backedByStatus ? "create status" : "inventory origin"}`, async t => {
    const row: RuntimeInfo = backedByStatus ? nativeRuntime : {
      ...nativeRuntime, origin: "legacy", operation_id: null, workspace_id: null,
    };
    const fixture = mountedNativeFixture(t, row);
    await fixture.publish(row);
    const original = await connectFixture(fixture, "ordinary");
    original.update();
    original.scope.retire();
    assert.equal(fixture.state(), "retired");
    const operation: LocalCreate = {
      reference: createdRef, workspace_id: fixtureId(3), presented: true, profile: "codex",
      disposition: { result: "launched", current: "live", readiness: "unobserved", runtime: row },
    };
    const exposure: CreateExposure | undefined = backedByStatus ? {
      runtimeId, sessionId: row.session_id, operationId: fixtureId(4), reference: createdRef, state: "presented",
    } : undefined;
    const status = backedByStatus ? { operations: [operation], issue: null } : null;
    assert.deepEqual(explicitTerminalOpen(status, row, exposure, fixture.binding(), fixture.state()), {
      decision: backedByStatus ? operation : "inventory", retry: true,
    });
    for (const changed of [
      { ...row, session_id: fixtureId(20) }, { ...row, pane_id: "%3" }, { ...row, pane_pid: 43 },
      { ...row, workspace_id: fixtureId(21) },
    ]) {
      assert.deepEqual(explicitTerminalOpen(status, changed, exposure, fixture.binding(), fixture.state()),
        { decision: "blocked", retry: false });
    }
    const replacement = fixture.park("terminal_attach");
    const refused = fixture.connect("ordinary");
    assert.ok(await replacement.entered);
    replacement.resolve(nativeAttachment(finalView, nativeAgent, { ...nativeTarget, process_start: 124 }));
    await assert.rejects(refused, /Terminal attachment identity changed/);
    assert.equal(fixture.binding(), original.binding);
    const reopened = await connectFixture(fixture, "ordinary", nativeAttachment(observedView));
    assert.deepEqual(reopened.binding.terminal.target, original.binding.terminal.target);
    assert.deepEqual(fixture.counts("terminal_attach")[2], { runtimeId, restoration: {
      previous: original.binding.terminal, agent_pid: 12, session_id: row.session_id, operation_id: row.operation_id,
    } });
    assert.equal(reopened.scope.canWrite, false);
    assert.deepEqual(mutatingCommands(fixture), []);
  });
}

test("rollover rejects replacement proofs, stale inventory and canceled mounted attempts", async t => {
  const fixture = mountedNativeFixture(t);
  await fixture.publish(nativeRuntime);
  const original = await connectFixture(fixture, "ordinary");
  await fixture.publish(nativeRuntime, 13);
  original.update();
  const replacements = [
    { ...nativeTarget, runtime_id: secondTab }, { ...nativeTarget, generation: fixtureId(23) },
    { ...nativeTarget, session: "$2" }, { ...nativeTarget, window: "@2" },
    { ...nativeTarget, pane: "%3" }, { ...nativeTarget, pid: 43 }, { ...nativeTarget, process_start: 124 },
  ];
  const probe = async (sample: ReturnType<typeof nativeAttachment>, intervene?: () => unknown) => {
    const proof = fixture.park("terminal_attach");
    const request = fixture.connect("observer");
    assert.deepEqual(await proof.entered, { runtimeId, observer: true });
    await intervene?.();
    proof.resolve(sample);
    return request;
  };
  for (const replacement of replacements) {
    await assert.rejects(probe(nativeAttachment(observedView, restartedAgent, replacement)), /Terminal attachment identity changed/);
    assert.equal(fixture.binding(), original.binding);
  }
  await assert.rejects(probe(nativeAttachment(firstView, restartedAgent)), /Terminal attachment identity changed/);
  await assert.rejects(probe(nativeAttachment(observedView, nativeAgent)), /Terminal attachment identity changed/);
  assert.equal(await probe(nativeAttachment(observedView, restartedAgent), () => fixture.nextInventory(nativeRuntime, 14)), undefined);
  assert.equal(await probe(nativeAttachment(observedView, restartedAgent), () => fixture.publish(nativeRuntime, 14)), undefined);
  await fixture.publish(nativeRuntime, 13);
  assert.equal(await probe(nativeAttachment(observedView, restartedAgent), () => fixture.publish({
    ...nativeRuntime, origin: "managed", operation_id: null, workspace_id: null,
  }, 13)), undefined);
  await fixture.publish(nativeRuntime, 13);
  assert.equal(await probe(nativeAttachment(observedView, restartedAgent), () => fixture.select(secondTab)), undefined);
  fixture.select(runtimeId);
  assert.equal(await probe(nativeAttachment(observedView, restartedAgent), () => fixture.retireAttempt()), undefined);
  const finalProof = fixture.park("terminal_attach");
  const finalSample = fixture.park("local_inventory");
  const canceled = fixture.connect("observer");
  await finalProof.entered;
  finalProof.resolve(nativeAttachment(observedView, restartedAgent));
  await finalSample.entered;
  fixture.close();
  finalSample.resolve(inventory(nativeRuntime, 13));
  assert.equal(await canceled, undefined);
  assert.equal(fixture.binding(), original.binding);
  assert.equal(fixture.selected().key, undefined);
  assert.equal(fixture.counts("terminal_detach").length, replacements.length + 8);
  assert.equal(fixture.counts("terminal_detach").filter(args => (args as { viewId: string }).viewId === firstView).length, 1);
  assert.equal(fixture.counts("terminal_read").length, 0);
  assert.deepEqual(mutatingCommands(fixture), []);
});

test("parked read after inventory exit renders the final FIFO and one EOF on the retained tab", async t => {
  const fixture = mountedNativeFixture(t);
  await fixture.publish(nativeRuntime);
  const mounted = await connectFixture(fixture, "ordinary");
  const first = fixture.park("terminal_read");
  const finalFirst = fixture.park("terminal_read");
  const finalSecond = fixture.park("terminal_read");
  const eof = fixture.park("terminal_read");
  const streaming = mounted.run();
  await first.entered;
  first.resolve(nativeData([65], nativeAttachment(firstView)));
  await finalFirst.entered;
  const input = fixture.park("terminal_input");
  const sending = mounted.scope.send(Uint8Array.of(1));
  await input.entered;
  const queuedKey = mounted.scope.send(Uint8Array.of(2));
  const queuedPaste = mounted.scope.paste(Uint8Array.of(3));
  mounted.proposals.offer({ cols: 100, rows: 30 });
  const exited = { ...nativeRuntime, exited: true, pane_pid: null };
  const panel = await fixture.publish(exited);
  assert.equal(panel.mode, "draining");
  assert.equal(panel.outputOnly, mounted.binding);
  assert.equal(panel.intent, undefined);
  assert.equal(mounted.scope.canWrite, false);
  await mounted.flushResize();
  assert.equal(mounted.update(), "draining");
  assert.equal(mounted.proposals.take(), undefined);
  fixture.select(secondTab);
  assert.equal(fixture.selected().key, panel.key);

  const writing = deferred<void>();
  const rendered = deferred<void>();
  mounted.beforeRender(() => { rendered.resolve(); return writing.promise; });
  finalFirst.resolve(nativeData([66], nativeAttachment(firstView)));
  await rendered.promise;
  assert.equal(fixture.counts("terminal_read").length, 2);
  assert.equal(mounted.displayed.options.disableStdin, true);
  assert.equal(mounted.scope.canWrite, false);
  mounted.beforeRender(async () => {});
  writing.resolve();
  await finalSecond.entered;
  finalSecond.resolve(nativeData([67, 27, 91, 54, 110]));
  await eof.entered;
  eof.resolve(nativeClosure({ reason: "eof" }));
  await streaming;
  input.resolve(undefined);
  await Promise.all([sending, queuedKey, queuedPaste]);
  assert.deepEqual(mounted.frames, [[65], [66], [67, 27, 91, 54, 110]]);
  assert.equal(mounted.displayed.buffer.active.getLine(0)?.translateToString(true), "ABC");
  assert.equal(mounted.completed(), 1);
  assert.equal(mounted.closed(), 0);
  assert.equal(mounted.disposed(), false);
  assert.equal(mounted.scope.active, false);
  assert.equal(fixture.selected().mode, "completed");
  fixture.select(runtimeId);
  assert.equal(fixture.selected().key, panel.key);
  assert.equal(fixture.selected().outputOnly, mounted.binding);
  assert.equal(fixture.selected().intent, undefined);
  assert.deepEqual(fixture.counts("terminal_detach"), [{ viewId: firstView, failure: undefined }]);
  assert.equal(fixture.counts("terminal_read").length, 4);
  assert.equal(fixture.counts("terminal_attach").length, 1);
  assert.deepEqual(mutatingCommands(fixture).map(call => call.command), ["terminal_input"]);
});

test("typed EOF before inventory exit keeps the screen and cannot reopen the stream", async t => {
  const fixture = mountedNativeFixture(t);
  await fixture.publish(nativeRuntime);
  const mounted = await connectFixture(fixture, "ordinary");
  const read = fixture.park("terminal_read");
  const streaming = mounted.run();
  await read.entered;
  read.resolve(nativeClosure({ reason: "eof" }));
  await streaming;
  assert.equal(fixture.selected().mode, "completed");
  assert.equal(mounted.disposed(), false);
  assert.equal((await fixture.publish({ ...nativeRuntime, exited: true, pane_pid: null })).key, mounted.binding.mountKey);
  assert.equal(await fixture.connect("ordinary"), undefined);
  assert.equal(fixture.selected().mode, "completed");
  assert.deepEqual(mutatingCommands(fixture), []);
});

for (const boundary of ["close", "replacement", "runtime identity", "detached", "overflow", "geometry timeout", "geometry event"] as const) {
  test(`parked final read honors ${boundary} without completion or a fresh attach`, async t => {
    const fixture = mountedNativeFixture(t);
    await fixture.publish(nativeRuntime);
    const mounted = await connectFixture(fixture, "ordinary");
    const read = fixture.park("terminal_read");
    const streaming = mounted.run();
    await read.entered;
    await fixture.publish({ ...nativeRuntime, exited: true, pane_pid: null });
    if (boundary === "close") fixture.close();
    if (boundary === "replacement") await fixture.publish({ ...nativeRuntime, pane_id: "%3", pane_pid: 43 });
    const sample = nativeAttachment(firstView);
    if (boundary === "detached") read.resolve(nativeClosure({ reason: "detached" }));
    else if (boundary === "overflow") read.resolve(nativeClosure({ reason: "overflow", high_water: 16 }));
    else if (boundary === "geometry timeout") read.resolve(nativeClosure({ reason: "geometry_timeout" }));
    else if (boundary === "runtime identity") {
      read.resolve(nativeData([88], nativeAttachment(firstView, nativeAgent, { ...nativeTarget, process_start: 999 })));
    } else if (boundary === "geometry event") {
      const epoch = { agent: nativeAgent, owner: nativeOwner };
      const grid = { size, sampled_at_us: 1 };
      const status = { ...sample.status, epoch, committed: { size, revision: 1, intent: { epoch, id: fixtureId(24) } } };
      read.resolve({ ...nativeData([]), geometry: { status, bridge_pty: grid, client: grid } });
    } else read.resolve(nativeData([88], sample));
    await streaming;
    assert.equal(mounted.disposed(), true);
    assert.equal(mounted.completed(), 0);
    assert.deepEqual(mounted.frames, []);
    assert.equal(fixture.selected().intent, undefined);
    assert.equal(fixture.selected().key, undefined);
    assert.equal(fixture.counts("terminal_read").length, 1);
    assert.equal(fixture.counts("terminal_attach").length, 1);
    assert.equal(fixture.counts("terminal_detach").length, 1);
    assert.deepEqual(mutatingCommands(fixture), []);
  });
}

test("unbound, retired or uncorroborated exited rows cannot drain or invoke attach", async t => {
  const exited = { ...nativeRuntime, exited: true, pane_pid: null };
  const fixture = mountedNativeFixture(t);
  assert.equal((await fixture.publish(exited)).mode, "exited");
  assert.equal(await fixture.connect("ordinary"), undefined);
  const expected = { sessionId: nativeRuntime.session_id, operationId: fixtureId(4) };
  const exact = { identity: { ...expected, workspaceId: fixtureId(3), agentPid: 12 }, mountKey: "retained",
    terminal: nativeAttachment(firstView).status.terminal };
  for (const sample of [
    inventory({ ...exited, pane_id: "%3" }), inventory({ ...exited, pane_pid: 43 }),
    inventory({ ...exited, session_id: secondTab }), inventory({ ...exited, workspace_id: secondTab }),
    inventory({ ...exited, origin: "unverified", workspace_id: null, operation_id: null }), inventory(exited, 13),
  ]) {
    const mounted = mountedTerminal(runtimeId, sample, expected, exact, false);
    assert.equal(mounted.key, undefined);
    assert.equal(mounted.intent, undefined);
    assert.equal(mounted.outputOnly, undefined);
  }
  assert.equal(mountedTerminal(runtimeId, inventory(exited), expected, exact, false, "retired").key, undefined);
  assert.equal(fixture.counts("terminal_attach").length, 0);
});

test("the sidebar, saved restore and mounted renderer use the checked production paths", () => {
  const app = readFileSync(new URL("./App.tsx", import.meta.url), "utf8");
  const view = readFileSync(new URL("./TerminalView.tsx", import.meta.url), "utf8");
  const native = readFileSync(new URL("../src-tauri/src/lib.rs", import.meta.url), "utf8");
  const bridge = readFileSync(new URL("../../../crates/desktop-bridge/src/lib.rs", import.meta.url), "utf8");
  assert.match(app, /record\?\.state !== "retired" && currentAttachment[\s\S]+explicitTerminalOpen\(status, latest, store.exposure\(id\)/);
  assert.match(app, /showCreated\(decision, "user"\)[\s\S]+approveExternal\(latest\)[\s\S]+if \(retry && latestRecord\?\.state === "retired"\)/);
  assert.match(app, /inventoryOpen\(status, observed, exposure, "saved"\)/);
  assert.match(app, /restoreSaved\(null\);\s*void localClient.createStatus\(\)/);
  assert.match(app, /terminalPanel\(views, id, inventory,[\s\S]+terminalPanel\(store.current.views, id, inventoryRef.current/);
  assert.match(app, /<TerminalView key=\{props.mounted.key\}[\s\S]+panels.map\([\s\S]+<section key=\{id\}[\s\S]+hidden=\{id !== activeId\}[\s\S]+<PanelTerminal[\s\S]+previousBinding=\{previousBinding\}[\s\S]+currentAttachment=\{currentAttachment\}/);
  assert.match(view, /updateEligibility.current\?\.\(\)[\s\S]+updateMountedStream\(streamEligibility\)/);
  assert.match(view, /inputEligible: \(\) => attachmentMode === "ordinary" && access\(\) === "attached"/);
  assert.match(view, /if \(view\) detached.current = detach\(view, failure\)/);
  assert.match(view, /const current = \(\) => currentAttachment\(runtimeId, binding \?\? previous, attempt\)[\s\S]+if \(currentIntent\(\)\) setRetry/);
  assert.match(view, /attachCurrentTerminal\(runtimeId, previous, attachmentMode,[\s\S]+detached.current, localClient\)/);
  assert.match(view, /if \(!candidate\)[\s\S]+scope.bind\(view\)[\s\S]+runMountedTerminalStream\(\{[\s\S]+localClient.read\(view\)[\s\S]+protocol.consume\(scope, view, chunk, render\)/);
  assert.match(view, /if \(!completed\) disposeRenderer\(\)[\s\S]+completed = onBinding\(candidate.binding, "completed"/);
  assert.match(native, /bridge.attach_observer\(&runtime_id\)[\s\S]+bridge.restore_attachment\(&runtime_id, expected\)/);
  assert.match(bridge, /mode.check_opened\(&status.terminal\)[\s\S]+let status = prove_attachment\(&view, &status\).await\?[\s\S]+mode.prepare_control\(&view, &status\)[\s\S]+control.start\(&view\)/);
});
