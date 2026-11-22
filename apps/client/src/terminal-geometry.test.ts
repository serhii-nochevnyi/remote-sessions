import assert from "node:assert/strict";
import test from "node:test";
import xterm from "@xterm/xterm";
import { parseChunk, parseGeometryEvent, parseResizeResult, parseTerminalStatus } from "./local-client.ts";
import type { GeometryEvent, TerminalSize } from "./local-client.ts";
import { GeometryBoundaryError, OrderedGeometry, ResizeProposals } from "./terminal-geometry.ts";

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

function bytes(data: readonly number[]) { return { data, writable: false, closed: false }; }
function boundary(geometry = parseGeometryEvent(fixtureEvent())) { return { ...bytes([]), geometry }; }

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
