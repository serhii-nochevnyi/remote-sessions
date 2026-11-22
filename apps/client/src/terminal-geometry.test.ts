import assert from "node:assert/strict";
import test from "node:test";
import { parseChunk, parseGeometryEvent, parseResizeResult, parseTerminalStatus } from "./local-client.ts";
import type { GeometryEvent, TerminalSize } from "./local-client.ts";
import { OrderedGeometry, ResizeProposals } from "./terminal-geometry.ts";

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
});
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
  assert.equal(routing.take(), undefined);
  routing.offer(sizes[0]);
  const next = routing.take()!;
  routing.observe(parseTerminalStatus({ ...fixtureStatus(3, sizes[2]), epoch: changedEpoch }), true, 3);
  const stale = fixtureStatus(2);
  assert.equal(routing.complete(next, parseResizeResult(committedReply({
    ...stale, epoch: changedEpoch, committed: { ...stale.committed, intent: { ...stale.committed.intent, epoch: changedEpoch } },
  }))), false);
});

function bytes(data: number[]) { return { data, writable: false, closed: false }; }
function boundary(geometry = parseGeometryEvent(fixtureEvent())) { return { ...bytes([]), geometry }; }

test("parse callbacks precede the grid and targeted redraw, including split UTF-8 and alternate-screen output", async () => {
  const order: string[] = [];
  const routing = new OrderedGeometry(initial());
  let done: (() => void) | undefined;
  const sink = {
    write: (data: readonly number[]) => new Promise<void>(resolve => {
      order.push(`write:${data[0]}`);
      done = () => { order.push("parsed"); resolve(); };
    }),
    resize: (size: TerminalSize) => { order.push("grid"); return size; },
    acknowledge: async (event: GeometryEvent) => { order.push("redraw"); return event; },
  };
  const writes = [[0xe6, 0xbc], [0xa2, 0x1b], [0x5b, 0x3f, 0x31, 0x30, 0x34], [0x39, 0x68]];
  for (const write of writes) {
    const pending = routing.consume(bytes(write), sink);
    assert.notEqual(order[order.length - 1], "parsed");
    done!();
    await pending;
  }
  await routing.consume(boundary(), sink);
  assert.deepEqual(order, [
    "write:230", "parsed", "write:162", "parsed", "write:91", "parsed", "write:57", "parsed", "grid", "redraw",
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
  await assert.rejects(broken.consume(boundary(), sink));
  await broken.consume(bytes([0x07]), sink);
  assert.deepEqual(order, ["write:27"]);
  await healthy.consume(boundary(), sink);
  await assert.rejects(healthy.consume(boundary(), sink));
  assert.deepEqual(order, ["write:27", "grid", "redraw"]);
  const fresh = new OrderedGeometry(initial());
  await fresh.consume(bytes([0x41]), sink);
  await fresh.consume(boundary(), sink);
  assert.deepEqual(order, ["write:27", "grid", "redraw", "write:65", "grid", "redraw"]);
});

test("wrong-generation, old-owner and late callbacks leave the renderer grid unchanged", async () => {
  const result = fixtureEvent();
  const stale = parseGeometryEvent(mutate(result, "status.terminal.target.generation", uuid(70)));
  let writes = 0;
  let changed = 0;
  let resolve: (() => void) | undefined;
  const sink = {
    write: () => new Promise<void>(done => { writes++; resolve = done; }),
    resize: (size: TerminalSize) => { changed++; return size; },
    acknowledge: async (event: GeometryEvent) => event,
  };
  const routing = new OrderedGeometry(initial());
  await assert.rejects(routing.consume(boundary(stale), sink));
  assert.equal(changed, 0);
  const handoff = new OrderedGeometry(initial());
  await handoff.consume({ ...bytes([]), status: { ...initial(), epoch: { agent, owner: uuid(71) } } }, sink);
  await assert.rejects(handoff.consume(boundary(), sink));
  assert.equal(changed, 0);
  const disconnected = new OrderedGeometry(initial());
  const pending = disconnected.consume(bytes([0x1b]), sink);
  disconnected.close();
  resolve!();
  await pending;
  await disconnected.consume(boundary(), sink);
  assert.equal(changed, 0);
  assert.equal(writes, 1);
});
