import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import type { TestContext } from "node:test";
import xterm from "@xterm/xterm";
import { localClient, parseInventory, TerminalDelivery, TerminalViewScope } from "./local-client.ts";
import type { CreateStatus, Inventory, LocalCreate, RuntimeInfo, TerminalScope, TerminalStatus } from "./local-client.ts";
import type { CreateExposure } from "./presentation-preferences.ts";
import { acceptMountedRenderer, adoptCurrentTerminal, attachCurrentTerminal, beginTerminalReconnect, cancelTerminalReconnect,
  canRestoreTerminalFocus, closeView, confirmMountedWriter, demoSessions, focusAfterClose, explicitClosedReconnect,
  explicitTerminalOpen, inventoryOpen, mountedTerminal, mountedTerminalEffect, mountedTerminalEntry, moveView, openView, parseScaffoldStatus,
  refreshTerminalRecord, restoreFullscreenExitFocus, runMountedTerminalStream, tabKey, tabStop, terminalPanel, terminalProofOnly,
  terminalStreamAccess, TerminalAttachmentIdentityError, updateMountedStream, updateTerminalRecord } from "./session-state.ts";
import type { ActiveTerminalEffect, AttachmentMode, BoundTerminal, MountedTerminal, SessionViews, TerminalBindingState, TerminalRecord } from "./session-state.ts";
import { mountedResize, OrderedGeometry, sameTerminal } from "./terminal-geometry.ts";
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
  assert.equal(observing.observation, bound);
  assert.equal(observing.intent, undefined);
  assert.equal(terminalStreamAccess(observing, bound, "ordinary", true), "observing");
  assert.equal(mountedTerminalEffect(observing, { mode: "ordinary", agent: 12 }, true).mode, undefined);
  assert.equal(mountedTerminalEffect(observing, { mode: "ordinary", agent: 12, binding: { ...bound } }, true).mode, undefined);
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
    attachCurrentTerminal("A", bound, mode, intent, () => mode === "ordinary", () => active, wait, client);
  const replacements = [
    { ...terminal, agent_epoch: "replacement" }, { ...terminal, target: { ...terminal.target, generation: "replacement" } },
    { ...terminal, target: { ...terminal.target, session: "$1" } }, { ...terminal, target: { ...terminal.target, window: "@2" } },
    { ...terminal, target: { ...terminal.target, pane: "%3" } }, { ...terminal, target: { ...terminal.target, pid: 43 } },
    { ...terminal, target: { ...terminal.target, process_start: 124 } },
  ];
  for (const mode of ["observer", "ordinary"] as const) {
    for (const replacement of replacements) {
      next = fixtureAttachment("replacement", replacement);
      await assert.rejects(start(mode), /Terminal attachment identity changed/);
      assert.equal(detached[detached.length - 1], "replacement");
    }
  }
  const count = entered.length;
  current = inventory(fallback);
  assert.equal(await start("observer"), undefined);
  assert.equal(await start("ordinary"), undefined);
  assert.equal(entered.length, count);
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
const nativeData = (data: number[], sample?: ReturnType<typeof nativeAttachment>, incarnation = nativeIncarnation, owner = nativeOwner) => ({
  data, closed: false, writable: sample !== undefined, geometry: null,
  status: sample ? { ...sample.status, epoch: { agent: sample.status.terminal.agent_epoch, owner } } : null,
  ...(sample ? { paste_incarnation: incarnation } : {}),
});
const nativeClosure = (closure: object) => ({ data: [], closed: true, writable: false, status: null, geometry: null, closure });

function mountedNativeFixture(t: TestContext, row = nativeRuntime) {
  let latest: Inventory | null = null;
  let replyInventory = inventory(row);
  let views: SessionViews = { open: [runtimeId, secondTab], active: runtimeId };
  let record: TerminalRecord | undefined;
  let selection = 0;
  let retained: { binding: BoundTerminal; dispose: () => void } | undefined;
  let attemptScope: TerminalViewScope | undefined;
  let activeEffect: ActiveTerminalEffect | undefined;
  let syncMountedEffect: (() => void) | undefined;
  let detached = Promise.resolve();
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
        if (command === "terminal_attach" && latest?.runtimes.some(row => row.origin === "unverified")) {
          throw Error("Attachment not authorized");
        }
        const step = steps.get(command)?.shift();
        if (step) return step(args);
        if (command === "local_inventory") return replyInventory;
        if (command === "terminal_detach" || command === "terminal_confirm_writer") return;
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
  const binding = () => record?.binding ?? null;
  const selected = () => terminalPanel(views, runtimeId, latest, identity, record, false, selection);
  const entry = () => mountedTerminalEntry(selected(), retained?.binding, delivery.mayRestore);
  function clearRetained() {
    retained?.dispose();
    retained = undefined;
  }
  function changeSelection() {
    selection++;
    record = cancelTerminalReconnect(record);
  }
  async function publish(row: RuntimeInfo | null, pid = 12) {
    replyInventory = row ? inventory(row, pid) : { agent_pid: pid, runtimes: [] };
    latest = parseInventory(replyInventory);
    if (record) record = refreshTerminalRecord(record, latest);
    if (!selected().key) clearRetained();
    return selected();
  }
  async function connect(expectedMode?: AttachmentMode) {
    const panel = selected();
    const { mode, agent } = mountedTerminalEffect(panel, activeEffect, delivery.mayRestore);
    if (!mode || agent === undefined || entry() !== mode) return;
    if (expectedMode) assert.equal(mode, expectedMode);
    const effect: ActiveTerminalEffect = { mode, agent };
    activeEffect = effect;
    const previous = binding();
    const started = record?.attempt ?? 0;
    const generation = selection;
    const retainedAtEntry = retained;
    const current = (expected: BoundTerminal | null = accepted ?? previous): MountedTerminal =>
      (record?.attempt ?? 0) === started && binding() === expected &&
        (selection === generation || selected().mode === "draining")
        ? selected() : { mode: "missing" };
    function updateBinding(next: BoundTerminal, state: TerminalBindingState | "rendered", prior: BoundTerminal | null) {
      if (!views.open.includes(runtimeId)) return false;
      const updated = updateTerminalRecord(record, current(prior), next, state, prior, started, selection);
      if (!updated) return false;
      record = updated;
      if (!selected().key) clearRetained();
      return true;
    }
    let accepted: BoundTerminal | undefined;
    let proofOnly = false;
    let rendererOpened = false;
    let replacement = false;
    let confirmation: ReturnType<typeof confirmMountedWriter> | undefined;
    const detachments: Promise<void>[] = [];
    const displayed = new xterm.Terminal({ ...size, allowProposedApi: true, disableStdin: true });
    let layoutScheduled = false;
    const frames: number[][] = [];
    const failures: unknown[] = [];
    let closed = 0;
    let eof = 0;
    let disposals = 0;
    let detachmentFailure: "stream_closed" | undefined;
    let beforeRender: (() => Promise<void>) | undefined;
    let beforeConsume: (() => Promise<void>) | undefined;
    const dispose = () => {
      if (disposals) return;
      disposals++;
      protocol.dispose();
      displayed.dispose();
    };
    const access = (): ReturnType<typeof terminalStreamAccess> => accepted
      ? terminalStreamAccess(current(), accepted, mode, delivery.mayRestore) : "stale";
    let routing: OrderedGeometry | undefined;
    const resize = mountedResize({
      owns: terminal => accepted !== undefined && sameTerminal(accepted.terminal, terminal) &&
        scope.owns(terminal.view_id) && access() !== "stale",
      canWrite: () => scope.canWrite,
      ordered: () => routing,
      invoke: localClient.resize,
      scheduleLayout: () => { layoutScheduled = true; },
      cancelLayout: () => { layoutScheduled = false; },
      settled() {},
    });
    const scope = new TerminalViewScope({
      input: (viewId, data, incarnation) => localClient.input({ viewId, data, incarnation }),
      inputEligible: () => rendererOpened && mode === "ordinary" && access() === "attached",
      inputFailure: error => failures.push(error),
      paste: { stage: localClient.pasteStage, commit: localClient.pasteCommit, ack: localClient.pasteAck, changed() {} },
      reconnect: error => failures.push(error),
      retire(view) {
        confirmation?.stop();
        resize.close();
        protocol.pauseInput();
        if (view) {
          const pending = localClient.detach(view, detachmentFailure);
          detached = pending;
          detachments.push(pending);
        }
        if (!eof) {
          if (activeEffect === effect) activeEffect = undefined;
          if (!proofOnly) {
            if (accepted) updateBinding(accepted, replacement ? "changed" : current().mode === "draining" ? "ended" : "retired", accepted);
            else if (previous && retainedAtEntry) updateBinding(previous, replacement ? "changed" : "view-closed", previous);
          }
          dispose();
        }
      },
    }, delivery);
    attemptScope = scope;
    const protocol = bindTerminalProtocol(displayed, bytes => { void scope.send(bytes); });
    cleanup.push(async () => {
      scope.retire();
      dispose();
      await Promise.all(detachments);
    });
    let candidate: Awaited<ReturnType<typeof adoptCurrentTerminal>>;
    try {
      candidate = await adoptCurrentTerminal(runtimeId, previous, mode, current, () => delivery.mayRestore, scope, next => {
        proofOnly = terminalProofOnly(current(), mode, delivery.mayRestore);
        if (!updateBinding(next, proofOnly ? "proved" : "bound", previous)) return false;
        effect.binding = accepted = next;
        return true;
      }, detached, localClient);
    } catch (error) {
      replacement = error instanceof TerminalAttachmentIdentityError && error.replacement;
      scope.retire();
      throw error;
    }
    if (!candidate) {
      scope.retire();
      return;
    }
    if (proofOnly) scope.retire();
    const { attachment, binding: bound } = candidate;
    const view = attachment.view_id;
    const ordered = new OrderedGeometry(attachment.status);
    routing = ordered;
    const eligibility = {
      scope, access,
      revokeMutations() { resize.suspend(); protocol.pauseInput(); },
      identityChanged() { replacement = true; },
    };
    const recovering = deferred<void>();
    const confirmationState = confirmMountedWriter(candidate, mode, current, () => delivery.mayRestore,
      eligibility, recovering.resolve, localClient);
    confirmation = confirmationState;
    const update = () => updateMountedStream(eligibility, confirmationState);
    syncMountedEffect = () => {
      const refreshed = selected();
      const next = mountedTerminalEffect(refreshed, activeEffect, delivery.mayRestore);
      update();
      if (refreshed.key !== panel.key || next.mode !== mode || next.agent !== agent || (record?.attempt ?? 0) !== started) {
        scope.retire();
      }
    };
    let streaming: Promise<void> | undefined;
    async function read() {
      try {
        await runMountedTerminalStream({
          ...eligibility, binding: bound, routing: ordered, confirmation: confirmationState,
          inventory: localClient.inventory,
          acceptRenderer() {
            if (rendererOpened) return true;
            if (!acceptMountedRenderer(current(), bound, effect.mode, delivery.mayRestore, retainedAtEntry?.binding, retained?.binding,
              () => updateBinding(bound, "rendered", bound))) return false;
            clearRetained();
            rendererOpened = true;
            resize.scheduleLayout();
            return true;
          },
          read: localClient.read,
          async consume(view, chunk, render) {
            return protocol.consume(scope, view, chunk, async () => {
              if (beforeConsume) await beforeConsume();
              await render();
            });
          },
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
          rendered: chunk => {
            if (access() === "attached") resize.observe(chunk);
            void resize.propose();
          },
          eof(next) {
            if (updateBinding(bound, next, bound)) {
              eof++;
              retained = { binding: bound, dispose };
            }
          },
          closed() { closed++; detachmentFailure = "stream_closed"; },
        });
      } catch (error) {
        if (scope.active) scope.retire({ error });
      }
      await Promise.all(detachments);
    }
    return {
      binding: bound, scope, displayed, frames, failures, proofOnly,
      confirmation: confirmationState, recovering: recovering.promise,
      update,
      run() { return streaming ??= read(); },
      scheduleLayout: resize.scheduleLayout,
      layoutScheduled: () => layoutScheduled,
      async flushResize() {
        if (!layoutScheduled || !scope.canWrite) return;
        layoutScheduled = false;
        await resize.offer({ cols: 100, rows: 30 });
      },
      beforeRender(callback: () => Promise<void>) { beforeRender = callback; },
      beforeConsume(callback: () => Promise<void>) { beforeConsume = callback; },
      completed: () => eof,
      closed: () => closed,
      disposed: () => disposals !== 0,
      disposals: () => disposals,
      rendererOpened: () => rendererOpened,
    };
  }
  return {
    calls, counts, park, publish, selected, entry, connect, delivery,
    async refreshMounted(row: RuntimeInfo | null, pid = 12) {
      const panel = await publish(row, pid);
      syncMountedEffect?.();
      return panel;
    },
    effect: () => mountedTerminalEffect(selected(), activeEffect, delivery.mayRestore),
    binding, state: () => record?.binding ? record.state : undefined,
    record: () => record, attemptScope: () => attemptScope,
    requestReconnect(route?: { row: RuntimeInfo; status: CreateStatus | null; exposure?: CreateExposure }) {
      const prior = binding();
      if (!prior) return false;
      if (route && !explicitTerminalOpen(route.status, route.row, route.exposure, prior,
        record?.binding ? record.state : undefined).retry) return false;
      const next = beginTerminalReconnect(selected(), prior, selection + 1);
      if (!next) return false;
      changeSelection();
      record = next;
      return true;
    },
    select(id: string) {
      if (id !== views.active) changeSelection();
      views = openView(views, id, views.open);
    },
    close() { changeSelection(); views = closeView(views, runtimeId); clearRetained(); },
    retireAttempt: changeSelection,
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
  await mounted.confirmation.settled;
  return mounted;
}

function mutatingCommands(fixture: MountedFixture) {
  return fixture.calls.filter(({ command }) => /input|paste|resize|acknowledge_grid|create_session|stop/.test(command));
}

function writerAssertions(view: string, row = nativeRuntime, agent = nativeAgent, agentPid = 12) {
  return { terminal: nativeAttachment(view, agent).status.terminal, expected: {
    agent_pid: agentPid, session_id: row.session_id, operation_id: row.operation_id, workspace_id: row.workspace_id,
  } };
}

test("mounted ordinary attach observes during confirmation and waits for a later scoped grant", async t => {
  const fixture = mountedNativeFixture(t);
  await fixture.publish(nativeRuntime);
  const proof = fixture.park("terminal_attach");
  const confirmation = fixture.park("terminal_confirm_writer");
  const opening = fixture.connect("ordinary");
  assert.deepEqual(await proof.entered, { runtimeId });
  assert.equal(fixture.binding(), null);
  assert.equal(fixture.counts("terminal_confirm_writer").length, 0);
  proof.resolve(nativeAttachment(firstView));
  assert.deepEqual(await confirmation.entered, writerAssertions(firstView));
  const mounted = await opening;
  assert.ok(mounted);
  assert.deepEqual(fixture.binding()?.terminal, writerAssertions(firstView).terminal);

  const observation = fixture.park("terminal_read");
  const captured = fixture.park("terminal_read");
  const grant = fixture.park("terminal_read");
  const ending = fixture.park("terminal_read");
  const streaming = mounted.run();
  assert.deepEqual(await observation.entered, { viewId: firstView });
  observation.resolve(nativeData([65]));
  await captured.entered;
  assert.deepEqual(mounted.frames, [[65]]);
  assert.equal(mounted.confirmation.pending, true);
  assert.equal(mounted.scope.canWrite, false);
  mounted.scheduleLayout();
  await mounted.scope.send(Uint8Array.of(1));
  await mounted.scope.paste(Uint8Array.of(2));
  await mounted.flushResize();
  assert.deepEqual(mutatingCommands(fixture), []);

  confirmation.resolve(undefined);
  await mounted.confirmation.settled;
  assert.equal(mounted.confirmation.confirmed, true);
  assert.equal(mounted.scope.canWrite, false);
  captured.resolve(nativeData([66], nativeAttachment(firstView)));
  await grant.entered;
  assert.deepEqual(mounted.frames, [[65], [66]]);
  assert.equal(mounted.scope.canWrite, false);
  grant.resolve(nativeData([], nativeAttachment(firstView)));
  await ending.entered;
  assert.equal(mounted.scope.canWrite, true);
  const input = fixture.park("terminal_input");
  const fresh = mounted.scope.send(Uint8Array.of(3));
  assert.deepEqual(await input.entered, { viewId: firstView, data: [3], incarnation: nativeIncarnation });
  input.resolve(undefined);
  await fresh;
  ending.resolve(nativeClosure({ reason: "eof" }));
  await streaming;
  assert.deepEqual(fixture.counts("terminal_confirm_writer"), [writerAssertions(firstView)]);
  assert.deepEqual(mutatingCommands(fixture).map(call => call.command), ["terminal_input"]);
});

test("mounted restore closes before proof returns and detaches without confirmation", async t => {
  const fixture = mountedNativeFixture(t);
  await fixture.publish(nativeRuntime);
  const original = await connectFixture(fixture, "ordinary");
  original.scope.retire();
  const proof = fixture.park("terminal_attach");
  const restoring = fixture.connect("ordinary");
  assert.deepEqual(await proof.entered, { runtimeId, restoration: {
    previous: original.binding.terminal, agent_pid: 12, session_id: nativeRuntime.session_id,
    operation_id: nativeRuntime.operation_id,
  } });
  assert.deepEqual(fixture.counts("terminal_confirm_writer"), [writerAssertions(firstView)]);
  fixture.close();
  proof.resolve(nativeAttachment(observedView));
  assert.equal(await restoring, undefined);
  assert.equal(fixture.binding(), original.binding);
  assert.equal(fixture.selected().mode, "missing");
  assert.deepEqual(fixture.counts("terminal_detach"), [
    { viewId: firstView, failure: undefined }, { viewId: observedView, failure: undefined },
  ]);
  assert.deepEqual(fixture.counts("terminal_confirm_writer"), [writerAssertions(firstView)]);
  assert.equal(fixture.counts("terminal_read").length, 0);
  assert.deepEqual(mutatingCommands(fixture), []);
});

for (const outcome of ["accepted", "rejected", "replaced", "closed"] as const) {
  test(`mounted exact restore handles a ${outcome} confirmation without reviving stale views`, async t => {
    const fixture = mountedNativeFixture(t);
    await fixture.publish(nativeRuntime);
    const original = await connectFixture(fixture, "ordinary");
    original.scope.retire();
    const proof = fixture.park("terminal_attach");
    const confirmation = fixture.park("terminal_confirm_writer");
    const restoring = fixture.connect("ordinary");
    assert.ok(await proof.entered);
    proof.resolve(nativeAttachment(observedView));
    assert.deepEqual(await confirmation.entered, writerAssertions(observedView));
    const restored = await restoring;
    assert.ok(restored);
    assert.equal(fixture.binding()?.terminal.view_id, observedView);
    assert.equal(restored.scope.canWrite, false);
    if (outcome === "closed") fixture.close();
    if (outcome === "replaced") fixture.retireAttempt();
    if (outcome === "rejected") {
      const read = fixture.park("terminal_read");
      const streaming = restored.run();
      await read.entered;
      confirmation.reject(Error("candidate rejected"));
      await restored.recovering;
      read.resolve(nativeClosure({ reason: "detached" }));
      await streaming;
      assert.equal(restored.closed(), 1);
    } else {
      confirmation.resolve(undefined);
      await restored.confirmation.settled;
    }
    assert.equal(restored.scope.active, outcome === "accepted");
    assert.equal(restored.scope.canWrite, false);
    assert.deepEqual(fixture.counts("terminal_confirm_writer"), [writerAssertions(firstView), writerAssertions(observedView)]);
    assert.equal(fixture.counts("terminal_detach").length, outcome === "accepted" ? 1 : 2);
    assert.deepEqual(mutatingCommands(fixture), []);
  });
}

test("mounted restore does not confirm after a logical delivery hold appears during proof", async t => {
  const fixture = mountedNativeFixture(t);
  await fixture.publish(nativeRuntime);
  const original = await connectFixture(fixture, "ordinary");
  original.scope.retire();
  const proof = fixture.park("terminal_attach");
  const restoring = fixture.connect("ordinary");
  await proof.entered;
  fixture.delivery.holdInput(Error("delivery unknown"));
  proof.resolve(nativeAttachment(observedView));
  assert.equal(await restoring, undefined);
  const observer = await connectFixture(fixture, "observer", nativeAttachment(finalView));
  assert.equal(observer.scope.canWrite, false);
  assert.deepEqual(fixture.counts("terminal_confirm_writer"), [writerAssertions(firstView)]);
  assert.deepEqual(fixture.counts("terminal_detach"), [
    { viewId: firstView, failure: undefined }, { viewId: observedView, failure: undefined },
  ]);
  assert.deepEqual(mutatingCommands(fixture), []);
});

test("a bound view that exits during confirmation corroborates a read-only drain", async t => {
  const fixture = mountedNativeFixture(t);
  await fixture.publish(nativeRuntime);
  const proof = fixture.park("terminal_attach");
  const confirmation = fixture.park("terminal_confirm_writer");
  const opening = fixture.connect("ordinary");
  await proof.entered;
  proof.resolve(nativeAttachment(firstView));
  await confirmation.entered;
  const mounted = await opening;
  assert.ok(mounted);
  const final = fixture.park("terminal_read");
  const eof = fixture.park("terminal_read");
  const streaming = mounted.run();
  await final.entered;
  await fixture.publish({ ...nativeRuntime, exited: true, pane_pid: null });
  assert.equal(fixture.selected().mode, "draining");
  confirmation.reject(Error("Terminal has exited"));
  await mounted.recovering;
  final.resolve(nativeData([65]));
  await eof.entered;
  assert.equal(mounted.scope.canWrite, false);
  await mounted.scope.send(Uint8Array.of(2));
  await mounted.scope.paste(Uint8Array.of(3));
  eof.resolve(nativeClosure({ reason: "eof" }));
  await streaming;
  assert.deepEqual(mounted.frames, [[65]]);
  assert.equal(mounted.completed(), 1);
  assert.equal(mounted.disposed(), false);
  assert.deepEqual(fixture.counts("terminal_confirm_writer"), [writerAssertions(firstView)]);
  assert.deepEqual(mutatingCommands(fixture), []);
});

async function pendingWriter(fixture: MountedFixture) {
  const proof = fixture.park("terminal_attach");
  const confirmation = fixture.park("terminal_confirm_writer");
  const opening = fixture.connect("ordinary");
  assert.deepEqual(await proof.entered, { runtimeId });
  proof.resolve(nativeAttachment(firstView));
  assert.deepEqual(await confirmation.entered, writerAssertions(firstView));
  const mounted = await opening;
  assert.ok(mounted);
  return { mounted, confirmation };
}

async function attemptMutations(mounted: NonNullable<Awaited<ReturnType<MountedFixture["connect"]>>>) {
  await mounted.scope.send(Uint8Array.of(88));
  await mounted.scope.paste(Uint8Array.of(89));
  mounted.scheduleLayout();
  await mounted.flushResize();
}

const degradedRuntime = { ...nativeRuntime, operation_id: null, workspace_id: null };

test("catalog impairment keeps the adopted effect, read-only FIFO and historical grid acknowledgement", async t => {
  const fixture = mountedNativeFixture(t);
  assert.equal((await fixture.publish(degradedRuntime)).mode, "pending");
  assert.equal(await fixture.connect(), undefined);
  await fixture.publish(nativeRuntime);
  const mounted = await connectFixture(fixture, "ordinary");
  const renderer = mounted.displayed;
  const effect = fixture.effect();
  const attempt = fixture.record()?.attempt;
  const live = fixture.park("terminal_read");
  const issued = fixture.park("terminal_read");
  const warm = fixture.park("terminal_read");
  const parkedWhileImpaired = fixture.park("terminal_read");
  const fresh = fixture.park("terminal_read");
  const eof = fixture.park("terminal_read");
  const ack = fixture.park("terminal_acknowledge_grid");
  const stream = mounted.run();
  const attachment = nativeAttachment(firstView);
  await live.entered;
  live.resolve(nativeData([65], attachment));
  await issued.entered;
  assert.equal(mounted.scope.canWrite, true);
  assert.equal(mounted.layoutScheduled(), true);

  const dimensions = { cols: 100, rows: 30 };
  const epoch = { agent: nativeAgent, owner: nativeOwner };
  const intent = { id: fixtureId(22), epoch };
  const committed = { size: dimensions, revision: 1, intent };
  const status = { ...attachment.status, epoch, committed };
  const grid = { size: dimensions, sampled_at_us: 1 };
  const geometry = { status, bridge_pty: grid, client: grid };
  const resize = fixture.park("terminal_resize");
  const admitted = mounted.flushResize();
  assert.deepEqual(await resize.entered, { viewId: firstView, epoch, size: dimensions, incarnation: nativeIncarnation });

  const observing = await fixture.refreshMounted(degradedRuntime);
  assert.equal(observing.mode, "observe");
  assert.equal(observing.key, mounted.binding.mountKey);
  assert.equal(observing.observation, mounted.binding);
  assert.equal(observing.intent, undefined);
  assert.deepEqual(fixture.effect(), effect);
  assert.equal(fixture.record()?.attempt, attempt);
  assert.equal(fixture.binding(), mounted.binding);
  assert.equal(fixture.entry(), undefined);
  assert.equal(await fixture.connect(), undefined);
  assert.equal(fixture.requestReconnect(), false);
  assert.equal(mounted.scope.canWrite, false);
  assert.equal(mounted.layoutScheduled(), false);
  await attemptMutations(mounted);
  resize.resolve({ result: "committed", admission: intent, status });
  await admitted;

  issued.resolve({ ...nativeData([]), geometry });
  assert.deepEqual(await ack.entered, { viewId: firstView, acknowledgement: {
    renderer_id: fixtureId(16), epoch, revision: 1, size: dimensions, focused: false,
  } });
  ack.resolve(geometry);
  assert.deepEqual(await warm.entered, { viewId: firstView });
  warm.resolve({ ...nativeData([66]), status });
  assert.deepEqual(await parkedWhileImpaired.entered, { viewId: firstView });
  assert.equal(mounted.displayed, renderer);
  assert.equal(mounted.disposals(), 0);
  assert.equal(mounted.scope.viewId, firstView);
  assert.equal(renderer.buffer.active.getLine(0)?.translateToString(true), "AB");
  assert.deepEqual([renderer.cols, renderer.rows], [dimensions.cols, dimensions.rows]);
  await attemptMutations(mounted);
  assert.deepEqual(fixture.counts("terminal_attach"), [{ runtimeId }]);
  assert.deepEqual(fixture.counts("terminal_detach"), []);
  assert.deepEqual(mutatingCommands(fixture).map(call => call.command), ["terminal_resize", "terminal_acknowledge_grid"]);

  await fixture.refreshMounted(nativeRuntime);
  assert.deepEqual(fixture.effect(), effect);
  assert.equal(mounted.scope.canWrite, false);
  parkedWhileImpaired.resolve({ ...nativeData([67], attachment, newIncarnation), status });
  await fresh.entered;
  assert.equal(mounted.scope.canWrite, false);
  await attemptMutations(mounted);
  fresh.resolve({ ...nativeData([68], attachment), status });
  await eof.entered;
  assert.equal(mounted.scope.canWrite, true);
  const input = fixture.park("terminal_input");
  const sending = mounted.scope.send(Uint8Array.of(69));
  assert.deepEqual(await input.entered, { viewId: firstView, data: [69], incarnation: nativeIncarnation });
  input.resolve(undefined);
  await sending;
  eof.resolve(nativeClosure({ reason: "eof" }));
  await stream;
  assert.deepEqual(mounted.frames, [[65], [66], [67], [68]]);
  assert.equal(mounted.completed(), 1);
  assert.equal(fixture.state(), "view-closed");
  assert.equal(mounted.disposals(), 0);
  assert.deepEqual(fixture.counts("terminal_detach"), [{ viewId: firstView, failure: undefined }]);
  assert.deepEqual(fixture.counts("terminal_confirm_writer"), [writerAssertions(firstView)]);
});

for (const confirmed of [true, false]) {
  test(`writer confirmation ${confirmed ? "success" : "failure"} racing catalog impairment grants no writes`, async t => {
    const fixture = mountedNativeFixture(t);
    await fixture.publish(nativeRuntime);
    const { mounted, confirmation } = await pendingWriter(fixture);
    const read = fixture.park("terminal_read");
    const parked = fixture.park("terminal_read");
    const streaming = mounted.run();
    await read.entered;
    await fixture.refreshMounted(degradedRuntime);
    read.resolve(nativeData([65], nativeAttachment(firstView)));
    await parked.entered;
    assert.deepEqual(mounted.frames, [[65]]);
    if (confirmed) confirmation.resolve(undefined);
    else confirmation.reject(Error("Terminal inventory binding changed"));
    await mounted.confirmation.settled;
    assert.equal(mounted.confirmation.confirmed, confirmed);
    await attemptMutations(mounted);
    if (confirmed) {
      assert.equal(mounted.scope.active, true);
      assert.equal(mounted.scope.canWrite, false);
      await fixture.refreshMounted(nativeRuntime);
      const next = fixture.park("terminal_read");
      const eof = fixture.park("terminal_read");
      parked.resolve(nativeData([66], nativeAttachment(firstView)));
      await next.entered;
      assert.equal(mounted.scope.canWrite, false);
      next.resolve(nativeData([67], nativeAttachment(firstView)));
      await eof.entered;
      assert.equal(mounted.scope.canWrite, true);
      eof.resolve(nativeClosure({ reason: "eof" }));
    } else {
      assert.equal(mounted.scope.active, false);
      await fixture.refreshMounted(nativeRuntime);
      parked.resolve(nativeData([66], nativeAttachment(firstView)));
    }
    await streaming;
    assert.deepEqual(mounted.frames, confirmed ? [[65], [66], [67]] : [[65]]);
    assert.equal(mounted.scope.canWrite, false);
    assert.deepEqual(fixture.counts("terminal_confirm_writer"), [writerAssertions(firstView)]);
    assert.deepEqual(fixture.counts("terminal_attach"), [{ runtimeId }]);
    assert.equal(fixture.counts("terminal_detach").length, 1);
    assert.deepEqual(mutatingCommands(fixture), []);
  });
}

test("warm observation drops queued mutations and preserves an uncertain predecessor hold", async t => {
  const fixture = mountedNativeFixture(t);
  await fixture.publish(nativeRuntime);
  const mounted = await connectFixture(fixture, "ordinary");
  const live = fixture.park("terminal_read");
  const impaired = fixture.park("terminal_read");
  const waiting = fixture.park("terminal_read");
  const fresh = fixture.park("terminal_read");
  const eof = fixture.park("terminal_read");
  const stream = mounted.run();
  const attachment = nativeAttachment(firstView);
  await live.entered;
  live.resolve(nativeData([65], attachment));
  await impaired.entered;
  const input = fixture.park("terminal_input");
  const sending = mounted.scope.send(Uint8Array.of(1));
  assert.deepEqual(await input.entered, { viewId: firstView, data: [1], incarnation: nativeIncarnation });
  const queuedKey = mounted.scope.send(Uint8Array.of(2));
  const queuedPaste = mounted.scope.paste(Uint8Array.of(3));
  await fixture.refreshMounted(degradedRuntime);
  impaired.resolve(nativeData([66], attachment));
  await waiting.entered;
  await fixture.refreshMounted(nativeRuntime);
  assert.equal(fixture.delivery.mayRestore, false);
  waiting.resolve(nativeData([67], attachment));
  await fresh.entered;
  await attemptMutations(mounted);
  input.reject(Error("input delivery unknown"));
  await Promise.all([sending, queuedKey, queuedPaste]);
  assert.equal(fixture.delivery.held, true);
  fresh.resolve(nativeData([68], attachment, newIncarnation));
  await eof.entered;
  await attemptMutations(mounted);
  assert.equal(mounted.scope.canWrite, false);
  assert.deepEqual(mounted.frames, [[65], [66], [67], [68]]);
  assert.deepEqual(fixture.counts("terminal_detach"), []);
  assert.equal((await fixture.refreshMounted(degradedRuntime)).mode, "observe");
  eof.resolve(nativeClosure({ reason: "eof" }));
  await stream;
  assert.equal(mounted.completed(), 1);
  assert.equal(mounted.disposals(), 0);
  assert.equal(fixture.state(), "view-closed");
  assert.equal(fixture.requestReconnect(), false);
  assert.deepEqual(fixture.counts("terminal_attach"), [{ runtimeId }]);
  assert.deepEqual(mutatingCommands(fixture).map(call => call.command), ["terminal_input"]);
});

for (const boundary of [
  "missing", "session", "operation", "workspace", "pane", "pid", "agent", "epoch", "generation", "process",
  "proof", "typed closure", "close",
] as const) {
  test(`warm observation retires on ${boundary}`, async t => {
    const fixture = mountedNativeFixture(t);
    await fixture.publish(nativeRuntime);
    const mounted = await connectFixture(fixture, "ordinary");
    const first = fixture.park("terminal_read");
    const parked = fixture.park("terminal_read");
    const stream = mounted.run();
    await first.entered;
    await fixture.refreshMounted(degradedRuntime);
    first.resolve(nativeData([65]));
    await parked.entered;
    const conflicts = {
      missing: null, session: { ...degradedRuntime, session_id: secondTab },
      operation: { ...nativeRuntime, operation_id: secondTab }, workspace: { ...nativeRuntime, workspace_id: secondTab },
      pane: { ...degradedRuntime, pane_id: "%3" }, pid: { ...degradedRuntime, pane_pid: 43 },
      agent: degradedRuntime,
    };
    if (boundary === "proof") parked.reject(Error("Bound terminal proof unavailable"));
    else if (boundary === "typed closure") parked.resolve(nativeClosure({ reason: "detached" }));
    else if (boundary === "epoch" || boundary === "generation" || boundary === "process") {
      const agent = boundary === "epoch" ? restartedAgent : nativeAgent;
      const target = { ...nativeTarget, ...(boundary === "generation" ? { generation: fixtureId(23) } : {}),
        ...(boundary === "process" ? { process_start: 999 } : {}) };
      parked.resolve(nativeData([66], nativeAttachment(firstView, agent, target)));
    }
    else {
      if (boundary === "close") { fixture.close(); mounted.update(); }
      else await fixture.refreshMounted(conflicts[boundary], boundary === "agent" ? 13 : 12);
      parked.resolve(nativeData([66], nativeAttachment(firstView)));
    }
    await stream;
    assert.equal(mounted.scope.active, false);
    assert.equal(mounted.disposals(), 1);
    assert.equal(mounted.completed(), 0);
    assert.deepEqual(mounted.frames, [[65]]);
    assert.deepEqual(fixture.counts("terminal_attach"), [{ runtimeId }]);
    assert.equal(fixture.counts("terminal_detach").length, 1);
    assert.deepEqual(mutatingCommands(fixture), []);
  });
}

test("mounted candidate drains native EOF during writer confirmation with live WebView inventory", async t => {
  const fixture = mountedNativeFixture(t);
  await fixture.publish(nativeRuntime);
  const { mounted, confirmation } = await pendingWriter(fixture);
  const first = fixture.park("terminal_read");
  const empty = fixture.park("terminal_read");
  const tail = fixture.park("terminal_read");
  const eof = fixture.park("terminal_read");
  const streaming = mounted.run();
  assert.deepEqual(await first.entered, { viewId: firstView });
  const initial = fixture.park("local_inventory");
  confirmation.reject(Error("Terminal candidate is no longer current"));
  await initial.entered;
  assert.equal(fixture.selected().mode, "live");
  first.resolve(nativeData([65]));
  await attemptMutations(mounted);
  assert.deepEqual(mounted.frames, []);
  assert.deepEqual(fixture.counts("terminal_read"), [{ viewId: firstView }]);
  assert.deepEqual(fixture.counts("terminal_detach"), []);
  initial.resolve(inventory(nativeRuntime));

  await empty.entered;
  empty.resolve(nativeData([]));
  await tail.entered;
  tail.resolve({ ...nativeData([66, 27, 91, 54, 110]), status: nativeAttachment(firstView).status });
  await eof.entered;
  const final = fixture.park("local_inventory");
  eof.resolve(nativeClosure({ reason: "eof" }));
  await final.entered;
  assert.deepEqual(mounted.frames, []);
  assert.equal(mounted.completed(), 0);
  assert.equal(fixture.selected().mode, "live");
  assert.deepEqual(fixture.counts("terminal_detach"), []);
  const queuedRender = deferred<void>();
  const finishRendering = deferred<void>();
  mounted.beforeRender(() => {
    queuedRender.resolve();
    return finishRendering.promise;
  });
  final.resolve(inventory({ ...nativeRuntime, exited: true, pane_pid: null }));
  await queuedRender.promise;
  assert.deepEqual(mounted.frames, [[65]]);
  assert.equal(mounted.completed(), 0);
  assert.deepEqual(fixture.counts("terminal_detach"), []);
  await attemptMutations(mounted);
  finishRendering.resolve();
  await streaming;
  assert.deepEqual(mounted.frames, [[65], [66, 27, 91, 54, 110]]);
  assert.equal(mounted.displayed.buffer.active.getLine(0)?.translateToString(true), "AB");
  assert.equal(mounted.completed(), 1);
  assert.equal(mounted.closed(), 0);
  assert.equal(mounted.disposed(), false);
  assert.equal(fixture.selected().mode, "completed");
  assert.deepEqual(fixture.calls.map(call => call.command), [
    "terminal_attach", "terminal_confirm_writer", "terminal_read", "local_inventory",
    "terminal_read", "terminal_read", "terminal_read", "local_inventory", "terminal_detach",
  ]);
  assert.deepEqual(fixture.counts("terminal_detach"), [{ viewId: firstView, failure: undefined }]);
  assert.deepEqual(mutatingCommands(fixture), []);
});

for (const scheduled of [false, true]) {
  const name = scheduled ? "mounted denial fences output already waiting for renderer scheduling"
    : "mounted candidate fences queued output after native inventory changes with live WebView inventory";
  test(name, async t => {
    const fixture = mountedNativeFixture(t);
    await fixture.publish(nativeRuntime);
    const { mounted, confirmation } = await pendingWriter(fixture);
    const read = fixture.park("terminal_read");
    const atRendererSchedule = deferred<void>();
    const rendererMayResume = deferred<void>();
    if (scheduled) {
      mounted.beforeConsume(() => {
        atRendererSchedule.resolve();
        return rendererMayResume.promise;
      });
    }
    const streaming = mounted.run();
    await read.entered;
    if (scheduled) {
      read.resolve(nativeData([90]));
      await atRendererSchedule.promise;
    }
    const initial = fixture.park("local_inventory");
    confirmation.reject(Error("Terminal inventory binding changed"));
    await initial.entered;
    if (scheduled) rendererMayResume.resolve();
    else read.resolve(nativeData([90]));
    await attemptMutations(mounted);
    assert.equal(fixture.selected().mode, "live");
    assert.deepEqual(mounted.frames, []);
    assert.deepEqual(fixture.counts("terminal_detach"), []);
    initial.resolve(inventory({ ...nativeRuntime, workspace_id: fixtureId(24) }));
    await streaming;
    assert.deepEqual(mounted.frames, []);
    assert.equal(mounted.rendererOpened(), false);
    assert.equal(mounted.completed(), 0);
    assert.equal(mounted.disposed(), true);
    assert.equal(fixture.state(), "changed");
    assert.equal(fixture.counts("terminal_read").length, 1);
    assert.deepEqual(fixture.counts("terminal_detach"), [{ viewId: firstView, failure: undefined }]);
    assert.deepEqual(mutatingCommands(fixture), []);
  });
}

test("mounted candidate holds a typed EOF received before writer confirmation rejects", async t => {
  const fixture = mountedNativeFixture(t);
  await fixture.publish(nativeRuntime);
  const { mounted, confirmation } = await pendingWriter(fixture);
  const preEof = fixture.park("terminal_read");
  const eof = fixture.park("terminal_read");
  const streaming = mounted.run();
  await preEof.entered;
  preEof.resolve(nativeData([65]));
  await eof.entered;
  eof.resolve(nativeClosure({ reason: "eof" }));
  await attemptMutations(mounted);
  assert.deepEqual(mounted.frames, [[65]]);
  assert.equal(mounted.completed(), 0);
  assert.deepEqual(fixture.counts("terminal_detach"), []);
  const first = fixture.park("local_inventory");
  const final = fixture.park("local_inventory");
  confirmation.reject(Error("Terminal candidate is no longer current"));
  await first.entered;
  const exited = inventory({ ...nativeRuntime, exited: true, pane_pid: null });
  first.resolve(exited);
  await final.entered;
  assert.equal(mounted.completed(), 0);
  final.resolve(exited);
  await streaming;
  assert.equal(mounted.completed(), 1);
  assert.equal(fixture.selected().mode, "completed");
  assert.deepEqual(mounted.frames, [[65]]);
  assert.equal(fixture.counts("terminal_read").length, 2);
  assert.equal(fixture.counts("terminal_detach").length, 1);
  assert.deepEqual(mutatingCommands(fixture), []);
});

for (const boundary of [
  "close", "replacement", "malformed inventory", "duplicate runtime", "failed inventory", "unverified inventory", "missing inventory",
  "malformed read", "failed read", "non-EOF close", "writable read", "changed status", "geometry boundary",
  "inconsistent grid", "disproved release", "frame budget", "deadline",
] as const) {
  test(`mounted denied confirmation fences output on ${boundary}`, async t => {
    const fixture = mountedNativeFixture(t);
    await fixture.publish(nativeRuntime);
    const { mounted, confirmation } = await pendingWriter(fixture);
    const read = fixture.park("terminal_read");
    const initial = fixture.park("local_inventory");
    const streaming = mounted.run();
    await read.entered;
    if (boundary === "deadline") t.mock.timers.enable({ apis: ["setTimeout"] });
    confirmation.reject(Error("Terminal candidate denied"));
    await initial.entered;
    if (boundary === "close") fixture.close();
    if (boundary === "replacement") await fixture.publish({ ...nativeRuntime, pane_id: "%3", pane_pid: 43 });
    if (boundary === "close" || boundary === "replacement") {
      assert.equal(mounted.update(), "stale");
      assert.equal(fixture.counts("terminal_detach").length, 1);
    }
    if (boundary === "failed inventory") initial.reject(Error("native inventory unavailable"));
    else if (boundary === "malformed inventory") initial.resolve({ agent_pid: 12, runtimes: [{}] });
    else if (boundary === "duplicate runtime") initial.resolve({ agent_pid: 12, runtimes: [nativeRuntime, nativeRuntime] });
    else if (boundary === "unverified inventory") initial.resolve(inventory({ ...nativeRuntime, operation_id: null, workspace_id: null }));
    else if (boundary === "missing inventory") initial.resolve({ agent_pid: 12, runtimes: [] });
    else initial.resolve(inventory(nativeRuntime));
    if (boundary === "failed read") read.reject(Error("native read unavailable"));
    else if (boundary === "malformed read") read.resolve({ data: [256] });
    else if (boundary === "non-EOF close") read.resolve(nativeClosure({ reason: "overflow", high_water: 16 }));
    else if (boundary === "writable read") read.resolve(nativeData([65], nativeAttachment(firstView)));
    else if (boundary === "changed status") read.resolve({
      ...nativeData([65]), status: nativeAttachment(firstView, nativeAgent, { ...nativeTarget, process_start: 999 }).status,
    });
    else if (boundary === "geometry boundary") {
      const status = nativeAttachment(firstView).status;
      const epoch = { agent: nativeAgent, owner: nativeOwner };
      const grid = { size, sampled_at_us: 1 };
      read.resolve({ ...nativeData([]), geometry: {
        status: { ...status, epoch, committed: { size, revision: 1, intent: { epoch, id: fixtureId(24) } } },
        bridge_pty: grid,
        client: grid,
      } });
    } else if (boundary === "inconsistent grid") {
      const status = nativeAttachment(firstView).status;
      read.resolve({ ...nativeData([65]), status: {
        ...status, committed: { ...status.committed, size: { ...size, rows: size.rows + 1 } },
      } });
    } else if (boundary === "frame budget") {
      const excess = Array.from({ length: 17 }, () => fixture.park("terminal_read"));
      read.resolve(nativeData([65]));
      for (const next of excess) {
        await next.entered;
        next.resolve(nativeData([65]));
      }
    } else if (boundary === "deadline" || boundary === "disproved release") {
      const next = fixture.park("terminal_read");
      read.resolve(nativeData([65]));
      await next.entered;
      if (boundary === "deadline") {
        t.mock.timers.tick(10_000);
        assert.equal(mounted.scope.active, false);
        next.resolve(nativeData([]));
      } else {
        const final = fixture.park("local_inventory");
        next.resolve(nativeClosure({ reason: "eof" }));
        await final.entered;
        assert.deepEqual(mounted.frames, []);
        final.resolve(inventory({ ...nativeRuntime, operation_id: fixtureId(24) }));
      }
    } else read.resolve(nativeData([65]));
    await streaming;
    await attemptMutations(mounted);
    assert.equal(mounted.completed(), 0);
    assert.equal(mounted.scope.active, false);
    assert.equal(mounted.disposed(), true);
    assert.deepEqual(mounted.frames, []);
    assert.equal(fixture.counts("terminal_detach").length, 1);
    assert.deepEqual(mutatingCommands(fixture), []);
  });
}

for (const outcome of ["success", "failure"] as const) {
  test(`mounted writer ignores late ${outcome} after abandoning its reader`, async t => {
    const fixture = mountedNativeFixture(t);
    await fixture.publish(nativeRuntime);
    const { mounted, confirmation } = await pendingWriter(fixture);
    const read = fixture.park("terminal_read");
    const streaming = mounted.run();
    await read.entered;
    fixture.select(secondTab);
    mounted.update();
    if (outcome === "success") confirmation.resolve(undefined);
    else confirmation.reject(Error("late rejection"));
    await mounted.confirmation.settled;
    read.resolve(nativeData([65], nativeAttachment(firstView)));
    await streaming;
    assert.equal(mounted.confirmation.confirmed, false);
    assert.equal(mounted.scope.active, false);
    assert.deepEqual(fixture.counts("local_inventory"), []);
    assert.equal(fixture.counts("terminal_detach").length, 1);
    assert.deepEqual(mounted.frames, []);
    assert.deepEqual(mutatingCommands(fixture), []);
  });
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
    original.scheduleLayout();

    const rollover = await fixture.publish(nativeRuntime, 13);
    assert.equal(rollover.mode, "rollover");
    assert.equal(rollover.key, original.binding.mountKey);
    assert.equal(original.scope.canWrite, false);
    await original.scope.send(Uint8Array.of(4));
    await original.flushResize();
    assert.equal(original.update(), "stale");
    assert.equal(original.layoutScheduled(), false);
    assert.equal(original.disposed(), true);
    assert.deepEqual(fixture.counts("terminal_detach"), [{ viewId: firstView, failure: undefined }]);
    assert.equal(fixture.binding(), original.binding);

    const observer = await connectFixture(fixture, "observer", nativeAttachment(observedView, restartedAgent));
    assert.deepEqual(fixture.counts("terminal_attach"), [{ runtimeId }, { runtimeId, observer: true }]);
    assert.deepEqual(fixture.counts("terminal_confirm_writer"), [writerAssertions(firstView)]);
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
    assert.deepEqual(fixture.counts("terminal_confirm_writer"), [writerAssertions(firstView),
      writerAssertions(finalView, nativeRuntime, restartedAgent, 13)]);
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
  mounted.scheduleLayout();
  const exited = { ...nativeRuntime, exited: true, pane_pid: null };
  const panel = await fixture.publish(exited);
  assert.equal(panel.mode, "draining");
  assert.equal(panel.outputOnly, mounted.binding);
  assert.equal(panel.intent, undefined);
  assert.equal(mounted.scope.canWrite, false);
  await mounted.flushResize();
  assert.equal(mounted.update(), "draining");
  assert.equal(mounted.layoutScheduled(), false);
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

async function closeLiveTerminalView(fixture: MountedFixture, holdDelivery = false) {
  const mounted = await connectFixture(fixture, "ordinary");
  const first = fixture.park("terminal_read");
  const final = fixture.park("terminal_read");
  const eof = fixture.park("terminal_read");
  const streaming = mounted.run();
  await first.entered;
  first.resolve(nativeData([65], nativeAttachment(firstView)));
  await final.entered;
  let finishInput: (() => Promise<unknown>) | undefined;
  if (holdDelivery) {
    const input = fixture.park("terminal_input");
    const sent = mounted.scope.send(Uint8Array.of(1));
    await input.entered;
    const queued = mounted.scope.send(Uint8Array.of(2));
    const paste = mounted.scope.paste(Uint8Array.of(3));
    finishInput = () => {
      input.reject(Error("delivery unknown"));
      return Promise.all([sent, queued, paste]);
    };
  }
  final.resolve(nativeData([66, 27, 91, 54, 110]));
  await eof.entered;
  eof.resolve(nativeClosure({ reason: "eof" }));
  await streaming;
  await finishInput?.();
  assert.equal(mounted.completed(), 1);
  assert.equal(mounted.closed(), 0);
  assert.equal(mounted.disposed(), false);
  assert.equal(mounted.scope.canWrite, false);
  assert.equal(mounted.displayed.options.disableStdin, true);
  assert.deepEqual(mounted.frames, [[65], [66, 27, 91, 54, 110]]);
  assert.equal(mounted.displayed.buffer.active.getLine(0)?.translateToString(true), "AB");
  assert.deepEqual(fixture.counts("terminal_detach"), [{ viewId: firstView, failure: undefined }]);
  assert.equal(fixture.counts("terminal_read").length, 3);
  return mounted;
}

const reconnectRoutes = ["Reconnect button", "same sidebar row with create status", "same sidebar row with inventory origin"] as const;
type ReconnectRoute = typeof reconnectRoutes[number];

function requestRetainedReconnect(fixture: MountedFixture, route: ReconnectRoute, row: RuntimeInfo) {
  if (route === "Reconnect button") {
    assert.equal(explicitClosedReconnect(fixture.selected()), fixture.binding());
    return fixture.requestReconnect();
  }
  if (route === "same sidebar row with inventory origin") return fixture.requestReconnect({ row, status: null });
  const operation: LocalCreate = {
    reference: createdRef, workspace_id: fixtureId(3), presented: true, profile: "codex",
    disposition: { result: "launched", current: "live", readiness: "unobserved", runtime: row },
  };
  return fixture.requestReconnect({
    row, status: { operations: [operation], issue: null },
    exposure: { runtimeId, sessionId: row.session_id, operationId: fixtureId(4), reference: createdRef, state: "presented" },
  });
}

function restoration(previous: BoundTerminal, agentPid: number, row: RuntimeInfo) {
  return { runtimeId, restoration: {
    previous: previous.terminal, agent_pid: agentPid, session_id: row.session_id, operation_id: row.operation_id,
  } };
}

for (const successor of [false, true]) {
  for (const held of [false, true]) {
    for (const route of reconnectRoutes) {
      const agentLabel = successor ? "successor-agent" : "same-agent";
      const outcome = held ? "keeps unknown delivery read-only" : "restores ordinary input";
      test(`mounted retained ${agentLabel} reconnect ${outcome} from the ${route}`, async t => {
        const row: RuntimeInfo = route === "same sidebar row with inventory origin"
          ? { ...nativeRuntime, origin: "legacy", operation_id: null, workspace_id: null } : nativeRuntime;
        const fixture = mountedNativeFixture(t, row);
        await fixture.publish(row);
        const original = await closeLiveTerminalView(fixture, held);
        const initialMutations = mutatingCommands(fixture);
        const pid = successor ? 13 : 12;
        const agent = successor ? restartedAgent : nativeAgent;
        const owner = successor ? fixtureId(25) : nativeOwner;
        await fixture.publish(row, pid);
        assert.equal(fixture.selected().mode, "view-closed");
        assert.equal(fixture.entry(), undefined);
        assert.equal(await fixture.connect(), undefined);
        fixture.select(secondTab);
        assert.equal(fixture.selected().outputOnly, original.binding);
        assert.equal(fixture.requestReconnect(), false);
        fixture.select(runtimeId);
        assert.equal(fixture.entry(), undefined);
        assert.equal(fixture.counts("terminal_attach").length, 1);
        assert.equal(requestRetainedReconnect(fixture, route, row), true);
        const attempt = fixture.record()?.attempt;
        assert.equal(fixture.selected().intent?.reconnect, true);

        let previous = original.binding;
        let view = observedView;
        let retiringObserver: ReturnType<MountedFixture["park"]> | undefined;
        if (successor && !held) {
          assert.equal(fixture.entry(), "observer");
          const proof = fixture.park("terminal_attach");
          const fresh = fixture.park("local_inventory");
          retiringObserver = fixture.park("terminal_detach");
          const opening = fixture.connect();
          assert.deepEqual(await proof.entered, { runtimeId, observer: true });
          proof.resolve(nativeAttachment(observedView, agent));
          await fresh.entered;
          assert.equal(fixture.binding(), original.binding);
          assert.equal(original.disposals(), 0);
          fresh.resolve(inventory(row, pid));
          const observer = await opening;
          assert.ok(observer);
          assert.equal(observer.proofOnly, true);
          assert.equal(observer.rendererOpened(), false);
          assert.deepEqual(await retiringObserver.entered, { viewId: observedView, failure: undefined });
          assert.equal(fixture.record()?.attempt, attempt);
          assert.equal(fixture.selected().intent?.reconnect, true);
          assert.equal(fixture.selected().outputOnly, original.binding);
          assert.deepEqual(observer.binding.terminal.target, original.binding.terminal.target);
          assert.equal(observer.binding.terminal.agent_epoch, restartedAgent);
          assert.equal(fixture.binding(), observer.binding);
          assert.equal(fixture.state(), "proved");
          assert.equal(original.disposals(), 0);
          assert.equal(fixture.counts("terminal_confirm_writer").length, 1);
          assert.equal(fixture.counts("terminal_read").length, 3);
          previous = observer.binding;
          view = finalView;
        }

        assert.equal(fixture.entry(), held ? "observer" : "ordinary");
        const proof = fixture.park("terminal_attach");
        const fresh = fixture.park("local_inventory");
        const confirmation = held ? undefined : fixture.park("terminal_confirm_writer");
        const opening = fixture.connect();
        if (retiringObserver) {
          await Promise.resolve();
          assert.equal(fixture.counts("terminal_attach").length, 2);
          retiringObserver.resolve(undefined);
        }
        assert.deepEqual(await proof.entered, held ? { runtimeId, observer: true } : restoration(previous, pid, row));
        proof.resolve(nativeAttachment(view, agent));
        await fresh.entered;
        assert.equal(fixture.selected().intent?.reconnect, true);
        assert.equal(fixture.selected().outputOnly, original.binding);
        assert.equal(original.displayed.buffer.active.getLine(0)?.translateToString(true), "AB");
        assert.equal(original.disposals(), 0);
        fresh.resolve(inventory(row, pid));
        const reopened = await opening;
        assert.ok(reopened);
        assert.notEqual(reopened.displayed, original.displayed);
        assert.equal(reopened.rendererOpened(), false);
        assert.equal(original.disposals(), 0);
        assert.equal(fixture.record()?.attempt, attempt);
        assert.equal(fixture.selected().intent?.reconnect, true);
        assert.equal(fixture.binding(), reopened.binding);
        assert.deepEqual(reopened.binding.terminal.target, original.binding.terminal.target);
        assert.equal(reopened.binding.terminal.view_id, view);
        assert.equal(reopened.binding.terminal.agent_epoch, agent);
        assert.equal(reopened.binding.identity.agentPid, pid);
        assert.equal(reopened.scope.canWrite, false);

        const first = fixture.park("terminal_read");
        const next = fixture.park("terminal_read");
        const finish = fixture.park("terminal_read");
        const streaming = reopened.run();
        assert.deepEqual(await first.entered, { viewId: view });
        await attemptMutations(reopened);
        assert.deepEqual(mutatingCommands(fixture), initialMutations);
        if (held) {
          first.resolve(nativeData([67]));
          await next.entered;
          assert.equal(fixture.delivery.held, true);
          assert.equal(reopened.scope.canWrite, false);
          await attemptMutations(reopened);
          next.resolve(nativeData([]));
        } else {
          assert.deepEqual(await confirmation?.entered, writerAssertions(view, row, agent, pid));
          confirmation?.resolve(undefined);
          await reopened.confirmation.settled;
          assert.equal(reopened.confirmation.confirmed, true);
          assert.equal(reopened.scope.canWrite, false);
          first.resolve(nativeData([], nativeAttachment(view, agent), newIncarnation, owner));
          await next.entered;
          assert.equal(reopened.scope.canWrite, false);
          assert.equal(original.disposals(), 0);
          assert.equal(fixture.selected().intent?.reconnect, true);
          await attemptMutations(reopened);
          assert.deepEqual(mutatingCommands(fixture), initialMutations);
          next.resolve(nativeData([], nativeAttachment(view, agent), newIncarnation, owner));
        }
        await finish.entered;
        assert.equal(original.disposals(), 1);
        assert.equal(reopened.rendererOpened(), true);
        assert.equal(fixture.selected().mode, "live");
        assert.equal(fixture.selected().intent?.reconnect, undefined);
        assert.equal(fixture.selected().outputOnly, undefined);
        assert.equal(reopened.scope.canWrite, !held);
        if (!held) {
          const input = fixture.park("terminal_input");
          const sent = reopened.scope.send(Uint8Array.of(70));
          assert.deepEqual(await input.entered, { viewId: view, data: [70], incarnation: newIncarnation });
          input.resolve(undefined);
          await sent;
        }
        const expectedMutationCount = initialMutations.length + (held ? 0 : 1);
        assert.equal(mutatingCommands(fixture).length, expectedMutationCount);
        assert.deepEqual(fixture.counts("terminal_confirm_writer"), [
          writerAssertions(firstView, row), ...(held ? [] : [writerAssertions(view, row, agent, pid)]),
        ]);
        assert.deepEqual(fixture.counts("terminal_attach"), [
          { runtimeId }, ...(successor && !held ? [{ runtimeId, observer: true }] : []),
          held ? { runtimeId, observer: true } : restoration(previous, pid, row),
        ]);
        assert.deepEqual(fixture.calls.slice(fixture.calls.findIndex(call => call.command === "terminal_detach") + 1).map(call => call.command), [
          ...(successor && !held ? ["terminal_attach", "local_inventory", "terminal_detach"] : []),
          "terminal_attach", "local_inventory", ...(held ? [] : ["terminal_confirm_writer"]),
          "terminal_read", "terminal_read", "terminal_read", ...(held ? [] : ["terminal_input"]),
        ]);
        finish.resolve(nativeClosure({ reason: "eof" }));
        await streaming;
        assert.equal(reopened.completed(), 1);
        assert.deepEqual(reopened.frames, held ? [[67]] : []);
        assert.equal(reopened.displayed.buffer.active.getLine(0)?.translateToString(true) ?? "", held ? "C" : "");
        assert.equal(original.disposals(), 1);
        assert.equal(mutatingCommands(fixture).length, expectedMutationCount);
      });
    }
  }
}

test("native view EOF before confirmed runtime exit retains the final screen without a fresh attach", async t => {
  const fixture = mountedNativeFixture(t);
  await fixture.publish(nativeRuntime);
  const mounted = await closeLiveTerminalView(fixture);
  assert.equal(fixture.selected().mode, "view-closed");
  assert.equal((await fixture.publish({ ...nativeRuntime, workspace_id: null, operation_id: null })).mode, "view-closed");
  assert.equal(fixture.requestReconnect(), false);
  const exited = { ...nativeRuntime, exited: true, pane_pid: null };
  assert.equal((await fixture.publish(exited)).mode, "completed");
  assert.equal(fixture.requestReconnect(), false);
  assert.equal(await fixture.connect("ordinary"), undefined);
  assert.equal(fixture.selected().outputOnly, mounted.binding);
  assert.equal(mounted.displayed.buffer.active.getLine(0)?.translateToString(true), "AB");
  assert.equal(fixture.counts("terminal_attach").length, 1);
  assert.deepEqual(mutatingCommands(fixture), []);
});

for (const stage of ["same-agent ordinary", "successor observer", "successor ordinary"] as const) {
  for (const boundary of [
    "close", "tab cancellation", "stale attempt", "runtime exit", "missing inventory",
    "stale fresh inventory", "inventory replacement", "server replacement", "failed proof",
  ] as const) {
    test(`retained reconnect drops the ${stage} candidate on ${boundary}`, async t => {
      const fixture = mountedNativeFixture(t);
      await fixture.publish(nativeRuntime);
      const original = await closeLiveTerminalView(fixture);
      const pid = stage === "same-agent ordinary" ? 12 : 13;
      const agent = pid === 12 ? nativeAgent : restartedAgent;
      await fixture.publish(nativeRuntime, pid);
      assert.equal(fixture.entry(), undefined);
      assert.equal(fixture.requestReconnect(), true);
      const observer = stage === "successor ordinary"
        ? await connectFixture(fixture, "observer", nativeAttachment(observedView, restartedAgent)) : undefined;
      const prior = observer?.binding ?? original.binding;
      const view = observer ? finalView : observedView;
      const mode = stage === "successor observer" ? "observer" : "ordinary";
      assert.equal(fixture.entry(), mode);
      assert.equal(original.disposals(), 0);

      const proof = fixture.park("terminal_attach");
      const reconnecting = fixture.connect();
      assert.deepEqual(await proof.entered, mode === "observer" ? { runtimeId, observer: true } : restoration(prior, pid, nativeRuntime));
      const before = fixture.counts("terminal_detach").length;
      let newer: TerminalRecord | undefined;
      if (boundary === "server replacement") {
        proof.resolve(nativeAttachment(view, agent, { ...nativeTarget, generation: fixtureId(23) }));
        await assert.rejects(reconnecting, /Terminal attachment identity changed/);
      } else if (boundary === "failed proof") {
        proof.reject(Error("native proof unavailable"));
        await assert.rejects(reconnecting, /native proof unavailable/);
      } else {
        const fresh = fixture.park("local_inventory");
        proof.resolve(nativeAttachment(view, agent));
        await fresh.entered;
        if (boundary === "close") fixture.close();
        if (boundary === "tab cancellation") {
          fixture.select(secondTab);
          fixture.select(runtimeId);
        }
        if (boundary === "stale attempt") {
          fixture.retireAttempt();
          assert.equal(fixture.requestReconnect(), true);
          newer = fixture.record();
        }
        if (boundary === "runtime exit") await fixture.publish({ ...nativeRuntime, exited: true, pane_pid: null }, pid);
        if (boundary === "missing inventory") await fixture.publish(null, pid);
        if (boundary === "inventory replacement") await fixture.publish({ ...nativeRuntime, pane_id: "%3", pane_pid: 43 }, pid);
        fresh.resolve(inventory(nativeRuntime, boundary === "stale fresh inventory" ? 14 : pid));
        assert.equal(await reconnecting, undefined);
      }
      const removeScreen = boundary === "close" || boundary === "inventory replacement" || boundary === "server replacement";
      assert.equal(original.disposals(), Number(removeScreen));
      assert.equal(fixture.selected().key, removeScreen ? undefined : original.binding.mountKey);
      assert.equal(fixture.selected().intent?.reconnect, newer ? true : undefined);
      if (newer) assert.equal(fixture.record(), newer);
      if (!removeScreen) assert.equal(original.displayed.buffer.active.getLine(0)?.translateToString(true), "AB");
      if (boundary === "failed proof" || boundary === "stale fresh inventory") assert.equal(fixture.requestReconnect(), true);
      assert.equal(fixture.counts("terminal_detach").length, before + (boundary === "failed proof" ? 0 : 1));
      assert.equal(fixture.counts("terminal_read").length, 3);
      assert.deepEqual(fixture.counts("terminal_confirm_writer"), [writerAssertions(firstView)]);
      assert.deepEqual(mutatingCommands(fixture), []);
    });
  }
}

for (const held of [false, true]) {
  for (const boundary of ["close", "tab change", "superseded attempt", "replacement"] as const) {
    test(`retained successor ${held ? "observer" : "ordinary"} ignores late render on ${boundary}`, async t => {
      const fixture = mountedNativeFixture(t);
      await fixture.publish(nativeRuntime);
      const original = await closeLiveTerminalView(fixture, held);
      const initialMutations = mutatingCommands(fixture);
      await fixture.publish(nativeRuntime, 13);
      assert.equal(fixture.requestReconnect(), true);
      const observer = await connectFixture(fixture, "observer", nativeAttachment(observedView, restartedAgent));
      const view = held ? observedView : finalView;
      let reopened = observer;
      let confirmation: ReturnType<MountedFixture["park"]> | undefined;
      if (!held) {
        const proof = fixture.park("terminal_attach");
        confirmation = fixture.park("terminal_confirm_writer");
        const opening = fixture.connect();
        assert.deepEqual(await proof.entered, restoration(observer.binding, 13, nativeRuntime));
        proof.resolve(nativeAttachment(view, restartedAgent));
        const boundCandidate = await opening;
        assert.ok(boundCandidate);
        reopened = boundCandidate;
        await confirmation.entered;
      }
      const read = fixture.park("terminal_read");
      const entered = deferred<void>();
      const release = deferred<void>();
      reopened.beforeConsume(() => { entered.resolve(); return release.promise; });
      const streaming = reopened.run();
      assert.deepEqual(await read.entered, { viewId: view });
      read.resolve(nativeData([65]));
      await entered.promise;
      assert.equal(original.disposals(), 0);
      assert.equal(fixture.selected().intent?.reconnect, true);
      let newer: TerminalRecord | undefined;
      if (boundary === "close") fixture.close();
      if (boundary === "tab change") fixture.select(secondTab);
      if (boundary === "replacement") await fixture.publish({ ...nativeRuntime, pane_id: "%3", pane_pid: 43 }, 13);
      if (boundary === "superseded attempt") {
        fixture.retireAttempt();
        assert.equal(fixture.requestReconnect(), true);
        newer = fixture.record();
      }
      confirmation?.resolve(undefined);
      await reopened.confirmation.settled;
      release.resolve();
      await streaming;
      assert.equal(original.disposals(), Number(boundary === "close" || boundary === "replacement"));
      assert.equal(reopened.rendererOpened(), false);
      assert.equal(reopened.scope.active, false);
      assert.equal(reopened.confirmation.confirmed, false);
      assert.deepEqual(reopened.frames, []);
      assert.equal(fixture.counts("terminal_detach").filter(args => (args as { viewId: string }).viewId === view).length, 1);
      if (newer) assert.equal(fixture.record(), newer);
      await attemptMutations(reopened);
      assert.deepEqual(mutatingCommands(fixture), initialMutations);
    });
  }
}

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
  const controller = readFileSync(new URL("./session-state.ts", import.meta.url), "utf8");
  const native = readFileSync(new URL("../src-tauri/src/lib.rs", import.meta.url), "utf8");
  const bridge = readFileSync(new URL("../../../crates/desktop-bridge/src/lib.rs", import.meta.url), "utf8");
  assert.match(app, /!requiresTerminalReopen\(record\?\.state\) && currentAttachment[\s\S]+explicitTerminalOpen\(status, latest, store.exposure\(id\)/);
  assert.match(app, /showCreated\(decision, "user"\)[\s\S]+approveExternal\(latest\)[\s\S]+if \(retry && latestRecord\?\.state === "view-closed"\) reconnectClosedTerminal\(id, latestRecord.binding, latestRecord.attempt\)[\s\S]+if \(retry && latestRecord\?\.state === "retired"\)/);
  assert.match(app, /beginTerminalReconnect\(currentAttachment\(id, binding, attempt\)[\s\S]+onReconnectClosed=\{reconnectClosedTerminal\}/);
  assert.match(app, /inventoryOpen\(status, observed, exposure, "saved"\)/);
  assert.match(app, /restoreSaved\(null\);\s*void localClient.createStatus\(\)/);
  assert.match(app, /terminalPanel\(views, id, inventory,[\s\S]+terminalPanel\(store.current.views, id, inventoryRef.current/);
  assert.match(app, /await localClient.inventory\(\)[\s\S]+inventoryRef.current = inventory;[\s\S]+refreshTerminalRecord\(record, inventory\)[\s\S]+setRuntimes\(inventory.runtimes\)/);
  assert.match(app, /updateTerminalRecord\(current, currentAttachment\(id, previous, attempt\)/);
  assert.match(app, /<TerminalView key=\{props.mounted.key\}[\s\S]+panels.map\([\s\S]+<section key=\{id\}[\s\S]+hidden=\{id !== activeId\}[\s\S]+<PanelTerminal[\s\S]+previousBinding=\{previousBinding\}[\s\S]+currentAttachment=\{currentAttachment\}/);
  assert.match(view, /const \{ mode, agent \} = mountedTerminalEffect\(mounted, activeEffect.current, delivery.mayRestore\)/);
  assert.match(view, /activeEffect.current = effect;[\s\S]+effect.binding = binding = accepted;[\s\S]+\[runtimeId, mode, agent, attempt, retry, currentAttachment, delivery, detached, onBinding\]/);
  assert.match(view, /updateEligibility.current\?\.\(\)[\s\S]+updateMountedStream\(streamEligibility, confirmation\)/);
  assert.match(view, /inputEligible: \(\) => rendererOpened && attachmentMode === "ordinary" && access\(\) === "attached"/);
  assert.match(view, /if \(view\) detached.current = detach\(view, failure\)/);
  assert.match(view, /const current = \(\) => currentAttachment\(runtimeId, binding \?\? previous, attempt\)[\s\S]+if \(currentIntent\(\)\) setRetry/);
  assert.match(view, /useEffect\(\(\) => \{\s*const mountedElement[\s\S]+if \(mountedTerminalEntry\(currentAttachment\(runtimeId, previous, attempt\),\s*retainedAtEntry\?\.binding, delivery\.mayRestore\) !== mode\) return;\s*const attachmentMode = mode;/);
  assert.match(view, /adoptCurrentTerminal\(runtimeId, previous, attachmentMode,[\s\S]+terminalProofOnly\(current\(\), attachmentMode[\s\S]+onBinding\(accepted, proofOnly \? "proved" : "bound", previous, attempt\)[\s\S]+detached.current, localClient\)/);
  assert.match(view, /if \(proofOnly\) \{\s*scope.retire\(\);\s*return;\s*\}[\s\S]+confirmMountedWriter\(candidate,[\s\S]+await runMountedTerminalStream\(\{[\s\S]+localClient.read\(view\)[\s\S]+protocol.consume\(scope, view, chunk, render\)/);
  assert.match(view, /acceptMountedRenderer\(current\(\), candidate.binding,[\s\S]+onBinding\(candidate.binding, "rendered", candidate.binding, attempt\)[\s\S]+retainedAtEntry\?\.dispose\(\)[\s\S]+openRenderer\(\)[\s\S]+eof\(state\) \{\s+retained = onBinding\(candidate.binding, state/);
  assert.match(view, /mounted.mode === "completed" \? "Процес завершився · режим перегляду"[\s\S]+mounted.mode === "view-closed" && !intent \? "Потік термінала закрито · режим перегляду"/);
  assert.match(view, /disabled=\{intent\?\.reconnect === true \|\| \(!intent && !explicitClosedReconnect\(mounted\)\)\}[\s\S]+explicitClosedReconnect\(current\)[\s\S]+onReconnectClosed\(runtimeId, closedBinding, attempt\)/);
  assert.match(controller, /await attachCurrentTerminal\([\s\S]+scope.bind\(terminal.view_id\)[\s\S]+accept\(binding\)/);
  assert.match(controller, /if \(!armed && confirmed && canConfirm\(\)\) \{[\s\S]+scope.setInputEligible\(true\)[\s\S]+await client.confirmWriter\(terminal,[\s\S]+confirmed = true;\s+updateEligibility\(access\)/);
  assert.match(native, /#\[cfg\(target_os = "macos"\)\]\s*mod desktop[\s\S]+bridge.attach_observer\(&runtime_id\)[\s\S]+bridge.restore_attachment\(&runtime_id, expected\)[\s\S]+bridge.prepare_attachment\(&runtime_id\)[\s\S]+terminal_confirm_writer\([\s\S]+\.confirm_writer\(terminal, expected\)/);
  assert.match(native, /#\[cfg\(target_os = "macos"\)\]\s*let builder = \{[\s\S]+desktop::terminal_attach,\s*desktop::terminal_confirm_writer,[\s\S]+#\[cfg\(not\(target_os = "macos"\)\)\]\s*let builder = builder.invoke_handler\(tauri::generate_handler!\[scaffold_status\]\)/);
  assert.match(bridge, /mode.check_opened\(&status.terminal\)[\s\S]+prove_attachment\(&view, &status\).await\?[\s\S]+mode.prepare_control\(&view, &status\)[\s\S]+candidate.proof = Some[\s\S]+else if matches!\(mode, AttachmentMode::Ordinary\) \{\s*start_control\(&view\)/);
  assert.match(bridge, /pub async fn confirm_writer\([\s\S]+let \(current, _\) = views.writer_candidate\(&terminal, expected\)\?[\s\S]+views.forget_writer_candidate\(&terminal\);\s*start_control\(&view\)/);
});
