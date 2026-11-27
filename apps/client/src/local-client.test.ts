import assert from "node:assert/strict";
import test from "node:test";
import { localClient, parseInventory, parseAttachment, parseChunk, parseCreateSubmission, TerminalViewScope } from "./local-client.ts";
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
  let reject!: (error: unknown) => void;
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

test("inventory distinguishes corroborated legacy, verified managed and unverified wire rows", () => {
  const base = { runtime_id: id, session_id: id, command: "fixture", pane_pid: 42, pane_id: "%2",
    exited: false, width: 100, height: 30 };
  const parse = (fields: object) => parseInventory({ agent_pid: 12, runtimes: [{ ...base, ...fields }] }).runtimes[0];
  const unverified = { origin: "managed", workspace_id: null, operation_id: null };
  assert.equal(parse({ ...unverified, origin: "legacy" })?.origin, "legacy");
  assert.equal(parse(unverified)?.origin, "unverified");
  assert.equal(parse({ ...unverified, workspace_id: id, operation_id: freshId })?.origin, "managed");
  for (const fields of [{ ...unverified, workspace_id: id }, { ...unverified, operation_id: freshId }, { origin: "managed" }]) {
    assert.throws(() => parse(fields));
  }
  assert.throws(() => parseCreateSubmission({
    result: "recorded",
    operation: {
      reference: id, workspace_id: id, profile: "codex", presented: false,
      disposition: { result: "launched", current: "live", readiness: "unobserved", runtime: { ...base, ...unverified } },
    },
  }));
});

const nativeRejection = {
  reason: "stale_input", view_id: id, rejection_id: "33333333-3333-4333-8333-333333333333",
  event_time_us: 1_800_000_000_000_000,
};

for (const successorFirst of [false, true]) {
  for (const [label, rejection, recover] of [
    ["definite native rejection", nativeRejection, true],
    ["transport uncertainty", Error("Input outcome unknown"), false],
    ["host error with the same message", "Керування змінилося", false],
    ["serialized string lookalike", JSON.stringify(nativeRejection), false],
    ["wrong view", { ...nativeRejection, view_id: freshId }, false],
    ["invalid identity", { ...nativeRejection, rejection_id: "00000000-0000-0000-0000-000000000000" }, false],
    ["invalid event time", { ...nativeRejection, event_time_us: 0 }, false],
    ["unrecognized reason", { ...nativeRejection, reason: "attachment_changed" }, false],
    ["extra fields", { ...nativeRejection, incarnation: id }, false],
  ] as const) {
    test(`${label} with successor ${successorFirst ? "already observed" : "observed after rejection"}`, async t => {
      const first = deferred<void>();
      const sent: unknown[] = [];
      const failures: unknown[] = [];
      const revoked: boolean[] = [];
      const original = Object.getOwnPropertyDescriptor(globalThis, "window");
      Object.defineProperty(globalThis, "window", {
        configurable: true,
        value: { __TAURI_INTERNALS__: {
          invoke(command: string, args: unknown) {
            assert.equal(command, "terminal_input");
            sent.push(args);
            return sent.length === 1 ? first.promise : Promise.resolve();
          },
        } },
      });
      t.after(() => {
        if (original) Object.defineProperty(globalThis, "window", original);
        else Reflect.deleteProperty(globalThis, "window");
      });
      const scope = new TerminalViewScope({
        input: (viewId, data, incarnation) => localClient.input({ viewId, data, incarnation }),
        inputFailure: error => failures.push(error),
        grantRejected: () => { revoked.push(scope.canWrite); },
        paste: {
          stage: () => assert.fail("obsolete paste staged"),
          commit: () => assert.fail("obsolete paste committed"),
          ack: () => assert.fail("obsolete paste acknowledged"),
          changed: () => {},
        },
        retire: () => {},
        reconnect: () => {},
      });
      const successor = { ...liveChunk, paste_incarnation: freshId };
      assert.ok(scope.bind(id));
      await scope.consume(id, liveChunk, async () => {});
      const sending = scope.send(new Uint8Array(4097).fill(1));
      const queuedKey = scope.send(Uint8Array.of(2));
      const queuedPaste = scope.paste(Uint8Array.of(3));
      await Promise.resolve();
      assert.equal(sent.length, 1);
      if (successorFirst) await scope.consume(id, successor, async () => {});
      first.reject(rejection);
      await Promise.all([sending, queuedKey, queuedPaste]);
      assert.equal(scope.canWrite, recover && successorFirst);
      assert.equal(sent.length, 1);
      assert.deepEqual(revoked, recover ? [successorFirst] : []);
      assert.deepEqual(failures, recover ? [] : [rejection]);
      if (!successorFirst) {
        for (let repeat = 0; repeat < 2; repeat++) {
          await scope.consume(id, liveChunk, async () => {});
          assert.equal(scope.canWrite, false);
        }
      }
      await scope.consume(id, successor, async () => {});
      assert.equal(scope.canWrite, recover);
      await scope.send(Uint8Array.of(4));
      assert.deepEqual(sent, [
        { viewId: id, data: new Array(4096).fill(1), incarnation: id },
        ...(recover ? [{ viewId: id, data: [4], incarnation: freshId }] : []),
      ]);
    });
  }
}

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

test("a typed rejection from another scoped invoke cannot retire this grant", async t => {
  const original = Object.getOwnPropertyDescriptor(globalThis, "window");
  Object.defineProperty(globalThis, "window", {
    configurable: true,
    value: { __TAURI_INTERNALS__: { invoke() { return Promise.reject(nativeRejection); } } },
  });
  t.after(() => {
    if (original) Object.defineProperty(globalThis, "window", original);
    else Reflect.deleteProperty(globalThis, "window");
  });
  let rejection: unknown;
  await assert.rejects(localClient.input({ viewId: id, incarnation: id, data: [1] }), error => {
    rejection = error;
    return error instanceof Error && error.message === "Керування змінилося";
  });
  for (const [view, incarnation] of [[freshId, id], [id, freshId]] as const) {
    const failures: unknown[] = [];
    const scope = new TerminalViewScope({
      async input() { throw rejection; },
      inputFailure: error => failures.push(error),
      grantRejected: () => assert.fail("reused rejection retired the current grant"),
      retire: () => {},
      reconnect: () => {},
    });
    assert(scope.bind(view));
    await scope.consume(view, { ...liveChunk, paste_incarnation: incarnation }, async () => {});
    await scope.send(Uint8Array.of(2));
    await scope.consume(view, { ...liveChunk, paste_incarnation: nativeRejection.rejection_id }, async () => {});
    assert.deepEqual(failures, [rejection]);
    assert.equal(scope.canWrite, false);
  }
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

test("native attach keeps the default command and forwards observer and restoration selectors", async t => {
  const calls: unknown[] = [];
  const original = Object.getOwnPropertyDescriptor(globalThis, "window");
  const status = terminalStatus(freshId, 0);
  const payload = { view_id: freshId, cols: 100, rows: 30, status };
  Object.defineProperty(globalThis, "window", {
    configurable: true,
    value: { __TAURI_INTERNALS__: {
      invoke(command: string, args: unknown) { calls.push([command, args]); return payload; },
    } },
  });
  t.after(() => {
    if (original) Object.defineProperty(globalThis, "window", original);
    else Reflect.deleteProperty(globalThis, "window");
  });
  const restoration = { previous: terminalStatus(id, 0).terminal, agent_pid: 12, session_id: id, operation_id: id };
  assert.deepEqual(await localClient.attach(id), payload);
  assert.deepEqual(await localClient.attach(id, { observer: true }), payload);
  assert.deepEqual(await localClient.attach(id, { restoration }), payload);
  assert.deepEqual(calls, [
    ["terminal_attach", { runtimeId: id }],
    ["terminal_attach", { runtimeId: id, observer: true }],
    ["terminal_attach", { runtimeId: id, restoration }],
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
