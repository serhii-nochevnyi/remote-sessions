import assert from "node:assert/strict";
import test from "node:test";
import type { TestContext } from "node:test";
import xterm from "@xterm/xterm";
import { localClient, parseChunk, parseGeometryEvent, parseResizeResult, parseTerminalStatus, TerminalViewScope } from "./local-client.ts";
import type { GeometryEvent, TerminalSize, TerminalStatus } from "./local-client.ts";
import { GeometryBoundaryError, mountedResize, OrderedGeometry, ResizeProposals, sameTerminal } from "./terminal-geometry.ts";
import { bindTerminalProtocol } from "./terminal-protocol.ts";

const uuid = (id: number) => `11111111-1111-4111-8111-${String(id).padStart(12, "0")}`;
const agent = uuid(1);
const epoch = { agent, owner: uuid(2) };
const terminal = {
  target: { runtime_id: uuid(3), generation: uuid(4), session: "$0", window: "@1", pane: "%2", pid: 42, process_start: 12345 },
  view_id: uuid(5), agent_epoch: agent,
};
const original = { cols: 100, rows: 30 };
const sizes = [{ cols: 120, rows: 40 }, { cols: 140, rows: 45 }, { cols: 85, rows: 27 }] as const;
const fixtureStatus = (revision = 1, size: TerminalSize = sizes[0]) => ({
  terminal, epoch, committed: { revision, size, intent: { epoch, id: uuid(10 + revision) } },
  provider: { size: original, sampled_at_us: 101 },
  observation: "pending",
} satisfies TerminalStatus);
const fixtureEvent = (revision = 1, size: TerminalSize = sizes[0]) => ({
  status: fixtureStatus(revision, size),
  bridge_pty: { size, sampled_at_us: 102 },
  client: { size, sampled_at_us: 103 },
});
const initial = () => parseTerminalStatus({
  ...fixtureStatus(), committed: { revision: 0, size: original, intent: null }, observation: "observed",
});
const committedReply = (status = fixtureStatus()) =>
  ({ result: "committed", admission: status.committed.intent, status });
const committed = (revision: number, size: TerminalSize) =>
  parseResizeResult(committedReply(fixtureStatus(revision, size)));

const malformedStatus = [
  ["terminal", undefined],
  ["terminal.target.runtime_id", null],
  ["terminal.target.runtime_id", 1],
  ["terminal.target.generation", undefined],
  ["terminal.target.generation", "bad"],
  ["terminal.target.session", "%0"],
  ["terminal.target.pane", "@2"],
  ["terminal.target.pid", 0],
  ["terminal.target.process_start", 1.1],
  ["terminal.view_id", undefined],
  ["terminal.view_id", false],
  ["terminal.agent_epoch", []],
  ["terminal.agent_epoch", undefined],
  ["epoch.agent", 12],
  ["epoch.owner", undefined],
  ["epoch.owner", uuid(0).replace(/^.*-/, "")],
  ["committed", {}],
  ["committed.size", undefined],
  ["committed.size.cols", "120"],
  ["committed.size.cols", 501],
  ["committed.size.rows", 1],
  ["committed.size.rows", 201],
  ["committed.revision", -1],
  ["committed.revision", 0],
  ["committed.revision", 0.5],
  ["committed.revision", Number.MAX_SAFE_INTEGER + 1],
  ["committed.revision", undefined],
  ["committed.intent.epoch.owner", null],
  ["committed.intent.id", 0],
  ["provider.size.rows", 0],
  ["provider.sampled_at_us", "101"],
  ["provider.sampled_at_us", 0],
  ["observation", "loading"],
  ["observation", "observed"],
] as const;

function mutate(value: unknown, path: string, field: unknown): unknown {
  const result = JSON.parse(JSON.stringify(value)) as Record<string, unknown>;
  const keys = path.split(".");
  const last = keys.pop()!;
  let scope = result;
  for (const key of keys) scope = scope[key] as Record<string, unknown>;
  if (field === undefined) delete scope[last];
  else scope[last] = field;
  return result;
}

test("production status and resize reply decoders reject each malformed scope and geometry field", () => {
  const status = fixtureStatus();
  const reply = committedReply(status);
  assert.deepEqual(parseTerminalStatus(status), status);
  assert.deepEqual(parseResizeResult(reply), reply);
  const uncertain = { result: "uncertain", terminal, intent: { epoch, id: uuid(20) } };
  assert.deepEqual(parseResizeResult(uncertain), uncertain);
  for (const [path, field] of malformedStatus) {
    assert.throws(() => parseTerminalStatus(mutate(status, path, field)), path);
    assert.throws(() => parseResizeResult(mutate(reply, `status.${path}`, field)), path);
  }
  for (const [path, field] of [
    ["result", "ready"], ["status", undefined], ["status.epoch.owner", uuid(23)], ["status.terminal.agent_epoch", uuid(24)],
    ["admission", undefined], ["admission.id", uuid(25)], ["admission.epoch.agent", uuid(26)],
  ] as const) assert.throws(() => parseResizeResult(mutate(reply, path, field)), path);
  assert.throws(() => parseResizeResult(mutate(uncertain, "intent.epoch.agent", uuid(42))));
});

test("status and geometry events preserve historical intents after ownership changes", () => {
  const epoch = { agent, owner: uuid(23) };
  const event = fixtureEvent();
  const status = { ...event.status, epoch };
  assert.deepEqual(parseTerminalStatus(status), status);
  assert.deepEqual(parseGeometryEvent({ ...event, status }), { ...event, status });
  assert.throws(() => parseResizeResult(committedReply(status)));
});

test("production stream decoder checks event scope, revision and independently confirmed grids", () => {
  const event = fixtureEvent();
  const chunk = { writable: true, closed: false, data: [], status: null, geometry: event };
  assert.deepEqual(parseGeometryEvent(event), event);
  assert.deepEqual(parseChunk(chunk), chunk);
  for (const [path, field] of malformedStatus) {
    assert.throws(() => parseGeometryEvent(mutate(event, `status.${path}`, field)), path);
    assert.throws(() => parseChunk(mutate(chunk, `geometry.status.${path}`, field)), path);
  }
  for (const [path, field] of [
    ["bridge_pty", undefined], ["client.size", undefined], ["client.size.rows", 199], ["client.size.cols", false],
    ["bridge_pty.size.cols", 0], ["client.sampled_at_us", -1],
  ] as const) assert.throws(() => parseGeometryEvent(mutate(event, path, field)), path);
  assert.throws(() => parseChunk({ ...chunk, data: [65] }));
  assert.throws(() => parseChunk({ ...chunk, closed: true }));
  assert.throws(() => parseChunk({ ...chunk, status: event.status }));
});

test("one resize in flight keeps the latest unsent size and waits for its committed grid barrier", () => {
  const routing = new ResizeProposals();
  routing.observe(initial(), true, 0);
  routing.offer(sizes[0]);
  const first = routing.take()!;
  routing.offer(sizes[1]);
  routing.offer(sizes[2]);
  assert.equal(routing.take(), undefined);
  assert.equal(routing.complete(first, committed(1, sizes[0])), true);
  routing.observe(parseTerminalStatus(fixtureStatus()), true, 0);
  assert.equal(routing.take(), undefined);
  routing.observe(parseTerminalStatus(fixtureStatus()), true, 1);
  assert.deepEqual(routing.take()?.size, sizes[2]);
});

test("well-formed stale resize replies cannot complete the current scope or revision", () => {
  const reply = committedReply();
  for (const [path, field] of [
    ["status.terminal.target.runtime_id", uuid(40)], ["status.terminal.target.generation", uuid(41)],
    ["status.terminal.view_id", uuid(42)], ["status.terminal.target.pane", "%3"],
  ] as const) {
    const routing = new ResizeProposals();
    routing.observe(initial(), true, 0);
    routing.offer(sizes[0]);
    assert.equal(routing.complete(routing.take()!, parseResizeResult(mutate(reply, path, field))), false, path);
  }
  const oldAgent = uuid(46);
  const staleEpoch = { agent: oldAgent, owner: uuid(47) };
  const fresh = fixtureStatus();
  const staleAgentReply = parseResizeResult(committedReply({
    ...fresh,
    terminal: { ...terminal, agent_epoch: oldAgent },
    epoch: staleEpoch,
    committed: { ...fresh.committed, intent: { ...fresh.committed.intent, epoch: staleEpoch } },
  }));
  const agentRouting = new ResizeProposals();
  agentRouting.observe(initial(), true, 0);
  agentRouting.offer(sizes[0]);
  assert.equal(agentRouting.complete(agentRouting.take()!, staleAgentReply), false);

  const changedEpoch = { agent, owner: uuid(43) };
  const newOwner = parseTerminalStatus({ ...fixtureStatus(0, original), epoch: changedEpoch,
    committed: { revision: 0, size: original, intent: null }, observation: "observed" });
  const routing = new ResizeProposals();
  routing.observe(initial(), true, 0);
  routing.offer(sizes[0]);
  const old = routing.take()!;
  routing.offer(sizes[2]);
  routing.observe(newOwner, true, 0);
  assert.equal(routing.complete(old, parseResizeResult(reply)), false);
  routing.offer(sizes[1]);
  assert.equal(routing.take(), undefined);
  routing.observe(parseTerminalStatus({ ...fixtureStatus(), epoch: changedEpoch }), true, 1);
  const next = routing.take()!;
  routing.observe(parseTerminalStatus({ ...fixtureStatus(3, sizes[2]), epoch: changedEpoch }), true, 3);
  const stale = fixtureStatus(2, sizes[1]);
  assert.equal(routing.complete(next, parseResizeResult(committedReply({
    ...stale, epoch: changedEpoch, committed: { ...stale.committed, intent: { ...stale.committed.intent, epoch: changedEpoch } },
  }))), false);
  routing.offer(original);
  assert.equal(routing.take()?.revision, 3);
});

function bytes(data: readonly number[]) { return { data, writable: false, closed: false }; }
function boundary(geometry = parseGeometryEvent(fixtureEvent())) { return { ...bytes([]), geometry }; }

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}

function mountedResizeFixture(t: TestContext) {
  const firstInput = deferred<unknown>();
  const calls: { args: unknown; reply: ReturnType<typeof deferred<unknown>> }[] = [];
  const acknowledgements: { args: unknown; reply: ReturnType<typeof deferred<GeometryEvent>> }[] = [];
  const inputs: unknown[] = [];
  const reports: boolean[] = [];
  const grids: TerminalSize[] = [];
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
  Object.defineProperty(globalThis, "window", {
    configurable: true,
    value: { __TAURI_INTERNALS__: {
      invoke(command: string, args: unknown) {
        switch (command) {
          case "terminal_input":
            inputs.push(args);
            return firstInput.promise;
          case "terminal_resize": {
            const reply = deferred<unknown>();
            calls.push({ args, reply });
            return reply.promise;
          }
          case "terminal_acknowledge_grid": {
            const reply = deferred<GeometryEvent>();
            acknowledgements.push({ args, reply });
            return reply.promise;
          }
          default:
            return assert.fail(`unexpected native command ${command}`);
        }
      },
    } },
  });
  const display = rendererFixture();
  const ordered = new OrderedGeometry(initial());
  let layoutQueued = false;
  let bound = terminal;
  let proposed = Promise.resolve();
  const resize = mountedResize({
    owns: target => sameTerminal(bound, target) && scope.owns(target.view_id),
    canWrite: () => scope.canWrite,
    ordered: () => ordered,
    invoke: localClient.resize,
    scheduleLayout() { layoutQueued = true; },
    cancelLayout() { layoutQueued = false; },
    settled(unavailable) { reports.push(unavailable); },
  });
  const scope = new TerminalViewScope({
    input: (viewId, data, incarnation) => localClient.input({ viewId, data, incarnation }),
    inputFailure: error => assert.fail(String(error)),
    grantRejected() { if (resize.grantRejected()) protocol.pauseInput(); },
    retire() { resize.close(); ordered.close(); protocol.dispose(); },
    reconnect: error => assert.fail(String(error)),
  });
  const protocol = bindTerminalProtocol(display.xtermTerminal, data => { void scope.send(data); });
  assert.ok(scope.bind(terminal.view_id));
  t.after(() => {
    scope.retire();
    display.xtermTerminal.dispose();
    if (previousWindow) Object.defineProperty(globalThis, "window", previousWindow);
    else Reflect.deleteProperty(globalThis, "window");
  });
  const sink = {
    write: display.write,
    resize(size: TerminalSize) {
      grids.push(size);
      display.xtermTerminal.resize(size.cols, size.rows);
      return { cols: display.xtermTerminal.cols, rows: display.xtermTerminal.rows };
    },
    acknowledge: (event: GeometryEvent, size: TerminalSize) => localClient.acknowledgeGrid(terminal.view_id, {
      renderer_id: uuid(30), epoch: event.status.epoch, revision: event.status.committed.revision, size, focused: false,
    }),
  };
  return {
    resize, scope, ordered, calls, acknowledgements, reports, grids,
    pendingLayout: () => layoutQueued,
    lastProposal: () => proposed,
    replaceBinding() { bound = { ...terminal, target: { ...terminal.target, pane: "%3" } }; },
    measure(size: TerminalSize) {
      assert.equal(layoutQueued, true);
      layoutQueued = false;
      return resize.offer(size);
    },
    async sample(status: TerminalStatus | null, incarnation?: string, geometry: GeometryEvent | null = null) {
      const chunk = parseChunk({ ...bytes([]), status, geometry, writable: incarnation !== undefined,
        ...(incarnation ? { paste_incarnation: incarnation } : {}) });
      assert.ok(await protocol.consume(scope, terminal.view_id, chunk, () => ordered.consume(chunk, sink)));
      resize.observe(chunk);
      proposed = resize.propose();
    },
    async rejectGrant(incarnation: string) {
      const sending = scope.send(Uint8Array.of(65));
      await Promise.resolve();
      assert.deepEqual(inputs, [{ viewId: terminal.view_id, data: [65], incarnation }]);
      firstInput.reject({ reason: "stale_input", view_id: terminal.view_id, rejection_id: uuid(31), event_time_us: 10 });
      await sending;
      assert.equal(scope.canWrite, false);
      assert.equal(layoutQueued, false);
    },
  };
}

async function exerciseResizeGrantGap(t: TestContext, replyFirst: boolean, nextEpoch: typeof epoch, latest: TerminalSize) {
  const fixture = mountedResizeFixture(t);
  const oldGrant = uuid(32);
  const freshGrant = uuid(33);
  await fixture.sample(initial(), oldGrant);
  const first = fixture.measure(sizes[0]);
  const firstCall = { viewId: terminal.view_id, epoch, size: sizes[0] };
  fixture.resize.scheduleLayout();
  const oldLayout = fixture.measure(sizes[1]);
  assert.deepEqual(fixture.calls.map(call => call.args), [firstCall]);
  await oldLayout;
  fixture.resize.scheduleLayout();
  await fixture.rejectGrant(oldGrant);
  await fixture.sample(initial());
  assert.equal(fixture.scope.canWrite, false);
  assert.equal(fixture.pendingLayout(), false);

  async function settlePredecessor() {
    fixture.calls[0]!.reply.resolve(committedReply());
    await first;
    assert.deepEqual(fixture.reports, []);
    assert.equal(fixture.ordered.applied, 0);
    assert.deepEqual(fixture.ordered.status.committed.size, original);
    assert.deepEqual(fixture.grids, []);
    assert.deepEqual(fixture.calls.map(call => call.args), [firstCall]);
  }
  if (replyFirst) await settlePredecessor();
  await fixture.sample({ ...initial(), epoch: nextEpoch }, freshGrant);
  assert.equal(fixture.scope.canWrite, true);
  const intermediate = fixture.measure(sizes[1]);
  assert.deepEqual(fixture.calls.map(call => call.args), [firstCall]);
  await intermediate;
  fixture.resize.scheduleLayout();
  const latestLayout = fixture.measure(latest);
  assert.deepEqual(fixture.calls.map(call => call.args), [firstCall]);
  await latestLayout;
  if (!replyFirst) await settlePredecessor();

  const changed = { ...fixtureStatus(), epoch: nextEpoch };
  await fixture.sample(changed, freshGrant);
  assert.deepEqual(fixture.ordered.status, changed);
  assert.equal(fixture.ordered.applied, 0);
  assert.deepEqual(fixture.grids, []);
  assert.deepEqual(fixture.calls.map(call => call.args), [firstCall]);
  const event = parseGeometryEvent({ ...fixtureEvent(), status: changed });
  const applying = fixture.sample(null, freshGrant, event);
  assert.deepEqual(fixture.grids, [sizes[0]]);
  assert.deepEqual(fixture.acknowledgements.map(ack => ack.args), [{ viewId: terminal.view_id, acknowledgement: {
    renderer_id: uuid(30), epoch: nextEpoch, revision: 1, size: sizes[0], focused: false,
  } }]);
  assert.equal(fixture.ordered.applied, 0);
  await fixture.resize.propose();
  assert.deepEqual(fixture.calls.map(call => call.args), [firstCall]);
  fixture.acknowledgements[0]!.reply.resolve(event);
  await applying;
  assert.equal(fixture.ordered.applied, 1);
  const expected = [firstCall, { viewId: terminal.view_id, epoch: nextEpoch, size: latest }];
  assert.deepEqual(fixture.calls.map(call => call.args), expected);
  const final = {
    terminal, epoch: nextEpoch, committed: { revision: 2, size: latest, intent: { epoch: nextEpoch, id: uuid(34) } },
    provider: null, observation: "unavailable",
  };
  fixture.calls[1]!.reply.resolve({ result: "committed", admission: final.committed.intent, status: final });
  await fixture.lastProposal();
  assert.deepEqual(fixture.grids, [sizes[0]]);
  assert.equal(fixture.ordered.applied, 1);
  const followup = parseGeometryEvent({ ...fixtureEvent(2, latest), status: final });
  const converging = fixture.sample(null, freshGrant, followup);
  fixture.acknowledgements[1]!.reply.resolve(followup);
  await converging;
  assert.equal(fixture.ordered.applied, 2);
  assert.deepEqual(fixture.ordered.status, final);
  assert.deepEqual(fixture.grids, [sizes[0], latest]);
  assert.deepEqual(fixture.calls.map(call => call.args), expected);
}

test("resize grant recovery keeps one physical invoke", { timeout: 2000 }, async t => {
  await t.test("same owner", sub => exerciseResizeGrantGap(sub, false, epoch, sizes[2]));
  await t.test("new owner", sub => exerciseResizeGrantGap(sub, false, { agent, owner: uuid(35) }, sizes[2]));
});

test("resize grant recovery waits for same-view ordered grid", { timeout: 2000 }, async t => {
  await t.test("same owner returns to the previous grid", sub => exerciseResizeGrantGap(sub, true, epoch, original));
  await t.test("new owner returns to the previous grid", sub => exerciseResizeGrantGap(sub, true, { agent, owner: uuid(35) }, original));
});

test("unknown resize drops old layout and requires reconciled geometry or a new grant", { timeout: 2000 }, async t => {
  await t.test("same grant stays closed after transport failure", async sub => {
    const fixture = mountedResizeFixture(sub);
    const incarnation = uuid(36);
    await fixture.sample(initial(), incarnation);
    const pending = fixture.measure(sizes[0]);
    fixture.resize.scheduleLayout();
    await fixture.measure(sizes[1]);
    fixture.calls[0]!.reply.reject(Error("resize result unknown"));
    await pending;
    assert.deepEqual(fixture.reports, [true]);
    await fixture.sample(initial(), incarnation);
    assert.equal(fixture.pendingLayout(), false);
    fixture.resize.scheduleLayout();
    await fixture.measure(sizes[0]);
    assert.equal(fixture.calls.length, 1);

    const event = parseGeometryEvent(fixtureEvent());
    const applying = fixture.sample(null, incarnation, event);
    assert.equal(fixture.calls.length, 1);
    fixture.acknowledgements[0]!.reply.resolve(event);
    await applying;
    assert.equal(fixture.ordered.applied, 1);
    assert.equal(fixture.pendingLayout(), true);
    assert.equal(fixture.calls.length, 1);
    const fresh = fixture.measure(sizes[2]);
    assert.deepEqual(fixture.calls[1]?.args, { viewId: terminal.view_id, epoch, size: sizes[2] });
    fixture.calls[1]!.reply.resolve(committedReply(fixtureStatus(2, sizes[2])));
    await fresh;
  });

  for (const result of ["uncertain", "rejected"] as const) {
    await t.test(`old ${result} reply preserves the newer grant`, async sub => {
      const fixture = mountedResizeFixture(sub);
      const oldGrant = uuid(36);
      await fixture.sample(initial(), oldGrant);
      const pending = fixture.measure(sizes[0]);
      fixture.resize.scheduleLayout();
      await fixture.measure(sizes[1]);
      await fixture.rejectGrant(oldGrant);
      await fixture.sample(initial(), uuid(37));
      fixture.calls[0]!.reply.resolve({ result, terminal, intent: { epoch, id: uuid(38) } });
      await pending;
      assert.equal(fixture.scope.canWrite, true);
      assert.equal(fixture.pendingLayout(), true);
      assert.deepEqual(fixture.reports, []);
      assert.deepEqual(fixture.grids, []);
      assert.equal(fixture.calls.length, 1);
      const fresh = fixture.measure(sizes[2]);
      assert.deepEqual(fixture.calls[1]?.args, { viewId: terminal.view_id, epoch, size: sizes[2] });
      fixture.calls[1]!.reply.resolve(committedReply(fixtureStatus(1, sizes[2])));
      await fresh;
    });
  }
});

test("stale resize scope and disposal fence late outcomes", { timeout: 2000 }, async t => {
  for (const stale of ["reply target", "mounted binding", "disposed view"] as const) {
    await t.test(stale, async sub => {
      const fixture = mountedResizeFixture(sub);
      const incarnation = uuid(39);
      await fixture.sample(initial(), incarnation);
      const pending = fixture.measure(sizes[0]);
      fixture.resize.scheduleLayout();
      await fixture.measure(sizes[1]);
      if (stale === "disposed view") fixture.scope.retire();
      if (stale === "mounted binding") fixture.replaceBinding();
      const status = fixtureStatus();
      fixture.calls[0]!.reply.resolve(committedReply(stale === "reply target" ? {
        ...status, terminal: { ...terminal, target: { ...terminal.target, pane: "%3" } },
      } : status));
      await pending;
      if (stale === "reply target") await fixture.sample(initial(), incarnation);
      await fixture.resize.propose();
      assert.equal(fixture.calls.length, 1);
      assert.deepEqual(fixture.reports, []);
      assert.deepEqual(fixture.grids, []);
      assert.deepEqual(fixture.acknowledgements, []);
      assert.equal(fixture.ordered.applied, 0);
      assert.deepEqual(fixture.ordered.status.committed.size, original);
    });
  }
});

test("each write callback precedes grid application and the matching ACK", async () => {
  const order: string[] = [];
  const routing = new OrderedGeometry(initial());
  let done: (() => void) | undefined;
  const sink = {
    write: (data: readonly number[]) => new Promise<void>(resolve => {
      order.push(`write:${data[0]}`);
      done = () => { order.push("parsed"); resolve(); };
    }),
    resize: (size: TerminalSize) => { order.push("grid"); return size; },
    acknowledge: async (event: GeometryEvent, size: TerminalSize) => {
      assert.equal(event.status.terminal.view_id, terminal.view_id);
      assert.equal(event.status.committed.revision, 1);
      assert.deepEqual(size, sizes[0]);
      order.push("ack");
      return event;
    },
  };
  const writes = [[0xe6, 0xbc, 0xa2, 0x1b], [0x5b, 0x3f, 0x31, 0x30, 0x34], [0x39, 0x68]];
  for (const write of writes) {
    const pending = routing.consume(bytes(write), sink);
    assert.notEqual(order[order.length - 1], "parsed");
    done!();
    await pending;
  }
  await routing.consume(boundary(), sink);
  assert.deepEqual(order, [
    "write:230", "parsed", "write:91", "parsed", "write:57", "parsed", "grid", "ack",
  ]);
  assert.equal(routing.applied, 1);
});

test("partial parser boundaries and stale revisions dispose only one stream with no replay", async () => {
  const order: string[] = [];
  const sink = {
    write: async (data: readonly number[]) => { order.push(`write:${data[0]}`); },
    resize: (size: TerminalSize) => { order.push("grid"); return size; },
    acknowledge: async (event: GeometryEvent) => { order.push("redraw"); return event; },
  };
  const broken = new OrderedGeometry(initial());
  const healthy = new OrderedGeometry(initial());
  await broken.consume(bytes([0x1b, 0x5d, 0x30]), sink);
  await assert.rejects(broken.consume(boundary(), sink), {
    name: "Error", message: "Не вдалося узгодити межу зміни розміру", reason: "geometry_output_incomplete",
  });
  await broken.consume(bytes([0x07]), sink);
  assert.deepEqual(order, ["write:27"]);
  await healthy.consume(boundary(), sink);
  await assert.rejects(healthy.consume(boundary(), sink), { reason: "geometry_revision_not_new" });
  assert.deepEqual(order, ["write:27", "grid", "redraw"]);
  const fresh = new OrderedGeometry(initial());
  await fresh.consume(bytes([0x41]), sink);
  await fresh.consume(boundary(), sink);
  assert.deepEqual(order, ["write:27", "grid", "redraw", "write:65", "grid", "redraw"]);
});

test("geometry failures report the first predicate and never resize or ACK", async () => {
  const sink = {
    write: async () => {},
    resize: () => assert.fail("rejected geometry resized"),
    acknowledge: async () => assert.fail("rejected geometry was acknowledged"),
  };
  const status = parseTerminalStatus(fixtureStatus(2, sizes[1]));
  const currentEvent = fixtureEvent(2, sizes[1]);
  const wrongView = parseGeometryEvent(mutate(currentEvent, "status.terminal.view_id", uuid(69)));
  const wrongGeneration = parseGeometryEvent(mutate(currentEvent, "status.terminal.target.generation", uuid(70)));
  const wrongSize = parseGeometryEvent(fixtureEvent(2, sizes[2]));
  const wrongIntent = parseGeometryEvent(mutate(currentEvent, "status.committed.intent.id", uuid(95)));
  const wrongIntentOwner = parseGeometryEvent(mutate(currentEvent, "status.committed.intent.epoch.owner", uuid(94)));
  for (const event of [wrongView, wrongGeneration, wrongSize, wrongIntent, wrongIntentOwner, parseGeometryEvent(fixtureEvent())]) {
    const routing = new OrderedGeometry(initial());
    await routing.consume({ ...bytes([]), status }, sink);
    await assert.rejects(routing.consume(boundary(event), sink), { reason: "geometry_status_rejected" });
  }
  const handoff = new OrderedGeometry(initial());
  await handoff.consume({ ...bytes([]), status: { ...initial(), epoch: { agent, owner: uuid(71) } } }, sink);
  await assert.rejects(handoff.consume(boundary(), sink), { reason: "geometry_status_rejected" });

  const routing = new OrderedGeometry(status);
  await routing.consume(bytes([0x1b]), sink);
  await assert.rejects(routing.consume(boundary(wrongView), sink), { reason: "geometry_revision_not_new" });
  const incomplete = new OrderedGeometry(initial());
  await incomplete.consume(bytes([0x1b]), sink);
  await assert.rejects(incomplete.consume(boundary(wrongView), sink), { reason: "geometry_status_rejected" });
});

test("late write and ACK callbacks stay inside the disposed attachment", async () => {
  let writes = 0;
  let changed = 0;
  let resolve: (() => void) | undefined;
  const sink = {
    write: () => new Promise<void>(done => { writes++; resolve = done; }),
    resize: (size: TerminalSize) => { changed++; return size; },
    acknowledge: async (event: GeometryEvent) => event,
  };
  const disconnected = new OrderedGeometry(initial());
  const pending = disconnected.consume(bytes([0x1b]), sink);
  disconnected.close();
  assert.equal(changed, 0);
  assert.equal(writes, 1);

  const old = new OrderedGeometry(initial());
  let finishAck: ((event: GeometryEvent) => void) | undefined;
  const outstanding = old.consume(boundary(), {
    ...sink,
    acknowledge: () => new Promise(resolve => { finishAck = resolve; }),
  });
  assert.equal(changed, 1);
  old.close();
  const nextTerminal = { ...terminal, view_id: uuid(72) };
  const fresh = new OrderedGeometry({ ...initial(), terminal: nextTerminal });
  const freshEvent = parseGeometryEvent(mutate(fixtureEvent(2), "status.terminal.view_id", nextTerminal.view_id));
  await fresh.consume(boundary(freshEvent), sink);
  resolve!();
  await pending;
  await disconnected.consume(boundary(), sink);
  finishAck!(parseGeometryEvent(fixtureEvent()));
  await outstanding;
  assert.equal(changed, 2);
  assert.equal(disconnected.applied, 0);
  assert.equal(old.applied, 0);
  assert.equal(fresh.applied, 2);
  assert.equal(fresh.status.terminal.view_id, nextTerminal.view_id);
});

const encode = (text: string) => Array.from(new TextEncoder().encode(text));
const followup = encode("\x1b[2C!");

const renderCases = [
  { name: "CSI", chunks: [encode("A\x1b[2"), encode("CB")], premature: 1, rendered: "A  B" },
  { name: "CSI ignore", chunks: [encode("A\x1b[2 ?>"), encode("CB")], premature: 1, rendered: "AB" },
  { name: "ESC intermediate", chunks: [encode("A\x1b("), encode("BB")], premature: 1, rendered: "AB" },
  { name: "OSC with split ESC-ST", chunks: [encode("A\x1b]2;title"), [0x1b], encode("\\B")],
    premature: 2, rendered: "AB", controls: ["osc:title"] },
  { name: "OSC with BEL", chunks: [encode("A\x1b]2;title"), encode("\x07B")],
    premature: 1, rendered: "AB", controls: ["osc:title"] },
  { name: "DCS with decoded C1 ST", chunks: [encode("A\x1bP1qpayload"), [0xc2, 0x9c, 0x42]],
    premature: 1, rendered: "AB", controls: ["dcs:payload"] },
  { name: "DCS ignore", chunks: [encode("A\x1bP1?qunsupported"), encode("\x1b\\B")], premature: 1, rendered: "AB" },
  { name: "SOS, PM and APC strings", chunks: [encode("A\x1bXignore\x1b^ignore\x1b_ignore"), [0xc2, 0x9c, 0x42]],
    premature: 1, rendered: "AB" },
  { name: "OSC interrupted by decoded C1 CSI followed by BEL",
    chunks: [encode("A\x1b]2;ignored"), [0xc2, 0x9b, 0x07], encode("2CB")], premature: 2, rendered: "A  B" },
  { name: "OSC cancelled by CAN", chunks: [encode("A\x1b]2;discard"), [0x18, 0x42]],
    premature: 1, rendered: "AB" },
  { name: "DCS cancelled by SUB", chunks: [encode("A\x1bP1qdiscard"), [0x1a, 0x42]],
    premature: 1, rendered: "AB" },
  { name: "malformed C0 with a noncontinuation", chunks: [[0x41], [0xc0, 0x1b, 0x5b], encode("2CB")],
    premature: 1, extraPrefix: [0xc0], rendered: "A  B" },
  { name: "malformed C0 with an overlong continuation", chunks: [encode("A\x1b["), [0xc0, 0x80, 0x32], encode("CB")],
    premature: 1, extraPrefix: [0xc0], rendered: "A  B" },
  { name: "E1 80 / 80 combined by native framing", chunks: [[0x41], [0xe1, 0x80, 0x80, 0x42]],
    premature: 1, extraPrefix: [0xe1, 0x80], rendered: "AကB" },
  { name: "a cancelled scalar before another starter", chunks: [[0x41, 0xe1, 0x80], [0xc2, 0x9b, 0x32, 0x43, 0x42]],
    premature: 1, rendered: "A  B" },
] satisfies {
  name: string; chunks: number[][]; premature: number; extraPrefix?: number[];
  rendered: string; controls?: string[];
}[];

function rendererFixture() {
  const xtermTerminal = new xterm.Terminal({ cols: original.cols, rows: original.rows, allowProposedApi: true });
  const controls: string[] = [];
  xtermTerminal.parser.registerOscHandler(2, data => { controls.push(`osc:${data}`); return true; });
  xtermTerminal.parser.registerDcsHandler({ final: "q" }, data => { controls.push(`dcs:${data}`); return true; });
  return {
    xtermTerminal,
    controls,
    write: (data: readonly number[]) => new Promise<void>(done => xtermTerminal.write(Uint8Array.from(data), done)),
    line: () => xtermTerminal.buffer.active.getLine(0)?.translateToString(true),
  };
}

test("xterm's unfinished scalar after a misframed E1 80 / 80 split rejects the grid", { timeout: 2000 }, async t => {
  const renderer = rendererFixture();
  t.after(() => renderer.xtermTerminal.dispose());
  const routing = new OrderedGeometry(initial());
  const sink = {
    write: renderer.write,
    resize: () => assert.fail("incomplete output resized"),
    acknowledge: async () => assert.fail("incomplete output was acknowledged"),
  };
  await routing.consume(bytes([0x41, 0xe1, 0x80]), sink);
  await routing.consume(bytes([0x80]), sink);
  assert.equal(renderer.line(), "A");
  await assert.rejects(routing.consume(boundary(), sink), { reason: "geometry_output_incomplete" });
});

for (const { name, chunks, premature, extraPrefix, rendered, controls = [] } of renderCases) {
  test(`xterm 6.0.0 renders ${name} across the ordered geometry boundary`, { timeout: 2000 }, async t => {
    const partial = rendererFixture();
    const renderer = rendererFixture();
    const contiguous = rendererFixture();
    t.after(() => {
      partial.xtermTerminal.dispose();
      renderer.xtermTerminal.dispose();
      contiguous.xtermTerminal.dispose();
    });
    const broken = new OrderedGeometry(initial());
    const rejectingSink = {
      write: partial.write,
      resize: () => assert.fail("incomplete output resized"),
      acknowledge: async () => assert.fail("incomplete output was acknowledged"),
    };
    for (const data of chunks.slice(0, premature)) await broken.consume(bytes(data), rejectingSink);
    if (extraPrefix) await broken.consume(bytes(extraPrefix), rejectingSink);
    await assert.rejects(broken.consume(boundary(), rejectingSink), (error: unknown) =>
      error instanceof GeometryBoundaryError && error.reason === "geometry_output_incomplete");

    const order: string[] = [];
    const event = parseGeometryEvent(fixtureEvent());
    const routing = new OrderedGeometry(initial());
    const sink = {
      write: async (data: readonly number[]) => { await renderer.write(data); order.push("write callback"); },
      resize: (size: TerminalSize) => {
        assert.equal(renderer.line(), rendered);
        assert.deepEqual(renderer.controls, controls);
        renderer.xtermTerminal.resize(size.cols, size.rows);
        order.push("resize");
        return { cols: renderer.xtermTerminal.cols, rows: renderer.xtermTerminal.rows };
      },
      acknowledge: async (received: GeometryEvent, size: TerminalSize) => {
        assert.equal(received, event);
        assert.deepEqual(size, event.status.committed.size);
        order.push("ack");
        return received;
      },
    };
    for (const data of chunks) await routing.consume(bytes(data), sink);
    assert.equal(routing.applied, 0);
    await routing.consume(boundary(event), sink);
    assert.equal(routing.applied, event.status.committed.revision);
    await routing.consume(bytes(followup), sink);
    assert.deepEqual(order, [...chunks.map(() => "write callback"), "resize", "ack", "write callback"]);
    await contiguous.write([...chunks.flat(), ...followup]);
    assert.equal(renderer.line(), `${rendered}  !`);
    assert.equal(renderer.line(), contiguous.line());
    assert.deepEqual(renderer.controls, contiguous.controls);
  });
}
