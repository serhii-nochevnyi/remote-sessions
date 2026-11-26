import assert from "node:assert/strict";
import test from "node:test";
import { localClient, parseInventory, parseAttachment, parseChunk, TerminalViewScope } from "./local-client.ts";
import type { GeometryEvent, TerminalStatus } from "./local-client.ts";
import { OrderedGeometry } from "./terminal-geometry.ts";
import { closeView, openView } from "./session-state.ts";
const id = "11111111-1111-4111-8111-111111111111";
test("real view identities survive disconnection and close only removes the view", () => {
  const inventory = parseInventory({ agent_pid: 12, runtimes: [{ runtime_id: id, session_id: id, command: "codex", pane_pid: 15, exited: false, width: 100, height: 30 }] });
  const views = openView({ open: [], active: null }, id, inventory.runtimes.map(r => r.runtime_id));
  assert.deepEqual(views, { open: [id], active: id });
  assert.deepEqual(closeView(views, id), { open: [], active: null });
  assert.equal(inventory.runtimes[0]?.pane_pid, 15);
  assert.deepEqual(openView(views, "unknown", [id]), views);
});
test("native stream boundaries reject invalid IDs, dimensions and byte floods", () => {
  assert.throws(() => parseAttachment({ view_id: "../shell", cols: 100, rows: 30 }));
  assert.throws(() => parseAttachment({ view_id: id, cols: 10000, rows: 30 }));
  assert.throws(() => parseChunk({ data: new Array<number>(8193).fill(1), writable: false, closed: false }));
  assert.throws(() => parseChunk({ data: [256], writable: false, closed: false }));
  assert.throws(() => parseInventory({ agent_pid: 0, runtimes: [] }));
  assert.deepEqual(parseChunk({ data: [0xe6, 0xbc], writable: false, closed: false }).data, [0xe6, 0xbc]);
});

const typedClosure = {
  closed: true, writable: false, data: [], status: null, geometry: null,
  closure: { reason: "overflow", high_water: 16 },
};

test("terminal read decodes only exact typed closure shapes and preserves legacy reads", () => {
  for (const closure of [typedClosure.closure, { reason: "eof" }, { reason: "detached" }, { reason: "geometry_timeout" }]) {
    const value = { ...typedClosure, closure };
    assert.deepEqual(parseChunk(value), value);
  }
  const basic = { closed: true, writable: false, data: [] };
  assert.deepEqual(parseChunk(basic), basic);
  const live = { closed: false, writable: true, data: [65], status: null, geometry: null };
  assert.deepEqual(parseChunk(live), live);
  for (const closure of [
    null, [], {}, "overflow", { reason: "unsupported" }, { reason: "overflow" },
    ...[0, 15, 17, 16.5, "16", null].map(high_water => ({ reason: "overflow", high_water })),
    { reason: "overflow", high_water: 16, extra: false }, { reason: "eof", high_water: 16 },
  ]) {
    assert.throws(() => parseChunk({ ...typedClosure, closure }));
  }
  for (const invalid of [
    { closed: false }, { writable: true }, { data: [65] }, { status: {} },
    { geometry: {} }, { geometry: undefined }, { extra: true },
  ]) {
    assert.throws(() => parseChunk({ ...typedClosure, ...invalid }));
  }
  assert.throws(() => parseChunk({ ...basic, closure: typedClosure.closure }));
  assert.throws(() => parseChunk({ ...live, extra: true }));
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}

const freshId = "22222222-2222-4222-8222-222222222222";
const target = { runtime_id: id, generation: id, session: "$0", window: "@1", pane: "%2", pid: 42, process_start: 12345 };
const owner = { agent: id, owner: id };
function terminalStatus(view_id: string, revision: number): TerminalStatus {
  return {
    terminal: { view_id, target, agent_epoch: id }, epoch: owner,
    committed: {
      revision, size: { cols: 100 + revision, rows: 30 },
      intent: revision ? { id, epoch: owner } : null,
    },
    provider: null, observation: "pending",
  };
}
const liveChunk = { closed: false, writable: true, data: [], paste_incarnation: id };

test("ordinary input sends the captured grant with each native invoke", async t => {
  const calls: unknown[] = [];
  const original = Object.getOwnPropertyDescriptor(globalThis, "window");
  Object.defineProperty(globalThis, "window", {
    configurable: true,
    value: { __TAURI_INTERNALS__: {
      invoke(command: string, args: Record<string, unknown>) { calls.push([command, args]); },
    } },
  });
  t.after(() => {
    if (original) Object.defineProperty(globalThis, "window", original);
    else Reflect.deleteProperty(globalThis, "window");
  });
  const scope = new TerminalViewScope({
    input: (viewId, data, incarnation) => localClient.input({ viewId, data, incarnation }),
    inputFailure: error => assert.fail(String(error)),
    retire: () => {},
    reconnect: () => {},
  });
  assert(scope.bind(id));
  await scope.consume(id, { closed: false, writable: true, data: [] }, async () => {});
  await scope.send(Uint8Array.of(1));
  assert.deepEqual(calls, []);
  await scope.consume(id, liveChunk, async () => {});
  const first = new Uint8Array(4096).fill(0xff);
  const frame = new Uint8Array(4097);
  frame.set(first);
  frame[4096] = 0x80;
  await scope.send(frame);
  await scope.consume(id, { ...liveChunk, paste_incarnation: freshId }, async () => {});
  await scope.send(Uint8Array.of(27, 2));
  assert.deepEqual(calls, [
    ["terminal_input", { viewId: id, data: Array.from(first), incarnation: id }],
    ["terminal_input", { viewId: id, data: [0x80], incarnation: id }],
    ["terminal_input", { viewId: id, data: [27, 2], incarnation: freshId }],
  ]);
});

for (const held of ["write", "ack"] as const) {
  for (const inputOutcome of ["accepted", "uncertain"] as const) {
    test(`typed Full retires a held ${held} and ${inputOutcome} old input before fresh redraw`, async () => {
      const oldRouting = new OrderedGeometry(terminalStatus(id, 0));
      const freshRouting = new OrderedGeometry(terminalStatus(freshId, 0));
      const write = deferred<void>();
      const ack = deferred<GeometryEvent>();
      const submitted = deferred<void>();
      const sends: [string, number[], string][] = [];
      const retired: (string | null)[] = [];
      const retries: unknown[] = [];
      const statuses: unknown[] = [];
      const frames: number[][] = [];
      const acknowledged: string[] = [];
      const send = (view: string, bytes: number[], incarnation: string) => {
        sends.push([view, bytes, incarnation]);
        return view === id ? submitted.promise : Promise.resolve();
      };
      const old = new TerminalViewScope({
        input: send,
        inputFailure: error => statuses.push(error),
        retire: view => { retired.push(view); oldRouting.close(); },
        reconnect: error => retries.push(error),
      });
      const fresh = new TerminalViewScope({
        input: send,
        inputFailure: error => statuses.push(error),
        retire: () => freshRouting.close(),
        reconnect: error => retries.push(error),
      });
      assert(old.bind(id));
      await old.consume(id, liveChunk, async () => {});
      const first = old.send(Uint8Array.of(1));
      const queued = old.send(Uint8Array.of(2));
      await Promise.resolve();
      assert.deepEqual(sends, [[id, [1], id]]);

      const sample = { size: { cols: 101, rows: 30 }, sampled_at_us: 1 };
      const geometry = { status: terminalStatus(id, 1), bridge_pty: sample, client: sample };
      const heldChunk = held === "write" ? { ...liveChunk, data: [42] } : { ...liveChunk, geometry };
      const oldRead = old.consume(id, heldChunk, () => oldRouting.consume(heldChunk, {
        write: () => write.promise,
        resize: size => size,
        acknowledge: event => { acknowledged.push(event.status.terminal.view_id); return ack.promise; },
      }));
      const closure = parseChunk(typedClosure).closure;
      assert(closure);
      old.acceptClosure(id, closure);
      old.acceptClosure(id, closure);
      old.retire();
      assert.deepEqual(retired, [id]);
      assert.equal(retries.length, 1);
      assert(!old.canWrite);

      assert(fresh.bind(freshId));
      assert.equal(freshRouting.status.terminal.target.runtime_id, oldRouting.status.terminal.target.runtime_id);
      const redraw = { ...liveChunk, data: [70, 82, 69, 83, 72], paste_incarnation: freshId };
      assert(await fresh.consume(freshId, redraw, () => freshRouting.consume(redraw, {
        write: async bytes => { frames.push([...bytes]); },
        resize: () => assert.fail("unexpected resize"),
        acknowledge: () => assert.fail("unexpected ACK"),
      })));
      write.resolve();
      ack.resolve(geometry);
      if (inputOutcome === "accepted") submitted.resolve();
      else submitted.reject(Error("input outcome unknown"));
      assert.equal(await oldRead, false);
      await Promise.all([first, queued]);
      assert.equal(oldRouting.applied, 0);
      assert.equal(freshRouting.applied, 0);
      assert.deepEqual(acknowledged, held === "ack" ? [id] : []);
      assert.deepEqual(frames, [redraw.data]);
      assert.deepEqual(statuses, []);
      assert(fresh.canWrite);
      assert.equal(retries.length, 1);
      await fresh.send(Uint8Array.of(3));
      assert.deepEqual(sends, [[id, [1], id], [freshId, [3], freshId]]);
    });
  }
}

test("native status reads the existing view and decodes its exact target without granting input", async t => {
  const calls: unknown[] = [];
  const original = Object.getOwnPropertyDescriptor(globalThis, "window");
  let response: unknown = terminalStatus(id, 0);
  Object.defineProperty(globalThis, "window", {
    configurable: true,
    value: { __TAURI_INTERNALS__: {
      invoke(command: string, args: unknown) {
        calls.push([command, args]);
        return response;
      },
    } },
  });
  t.after(() => {
    if (original) Object.defineProperty(globalThis, "window", original);
    else Reflect.deleteProperty(globalThis, "window");
  });
  assert.deepEqual(await localClient.terminalStatus(id), response);
  response = { ...terminalStatus(id, 0), terminal: {
    ...terminalStatus(id, 0).terminal, target: { ...target, process_start: 0 },
  } };
  await assert.rejects(localClient.terminalStatus(id), /Некоректне числове значення/);
  assert.deepEqual(calls, [
    ["terminal_status", { viewId: id }],
    ["terminal_status", { viewId: id }],
  ]);
});

test("read-only verification preserves output and drops queued input until a new native grant arrives", async () => {
  const sent: [string, number[], string][] = [];
  const writing = deferred<void>();
  const rendering = deferred<void>();
  const scope = new TerminalViewScope({
    input: async (view, bytes, incarnation) => {
      sent.push([view, bytes, incarnation]);
      if (sent.length === 1) await writing.promise;
    },
    inputFailure: error => assert.fail(String(error)),
    retire: () => assert.fail("read-only observation retired the view"),
    reconnect: () => assert.fail("read-only observation reconnected the view"),
  });
  const canWrite = () => scope.canWrite;
  assert.ok(scope.bind(id));
  assert.ok(await scope.consume(id, liveChunk, async () => {}));
  const priorRead = scope.captureReadEligibility();
  const sending = scope.send(Uint8Array.of(1));
  await Promise.resolve();
  const obsolete = scope.send(Uint8Array.of(2));
  const output: number[] = [];
  const reading = scope.consume(id, { ...liveChunk, data: [65] }, async () => {
    await rendering.promise;
    output.push(65);
  });
  scope.setInputEligible(false);
  assert.equal(canWrite(), false);
  assert.ok(scope.owns(id));
  await scope.send(Uint8Array.of(3));
  scope.setInputEligible(true);
  rendering.resolve();
  assert.ok(await reading);
  assert.deepEqual(output, [65]);
  assert.equal(canWrite(), false);
  assert.ok(await scope.consume(id, priorRead(liveChunk), async () => {}));
  assert.equal(canWrite(), false);
  writing.resolve();
  await Promise.all([sending, obsolete]);

  assert.ok(await scope.consume(id, { ...liveChunk, paste_incarnation: freshId }, async () => {}));
  assert.equal(canWrite(), true);
  await scope.send(Uint8Array.of(4));
  assert.deepEqual(sent, [[id, [1], id], [id, [4], freshId]]);

  assert.ok(await scope.consume(id, { closed: false, writable: false, data: [], paste_admission: "prior_send_unknown" }, async () => {}));
  scope.setInputEligible(false);
  scope.setInputEligible(true);
  assert.ok(await scope.consume(id, liveChunk, async () => {}));
  assert.equal(canWrite(), false);
  await scope.send(Uint8Array.of(5));
  assert.equal(sent.length, 2);
});
