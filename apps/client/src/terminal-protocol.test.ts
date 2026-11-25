import assert from "node:assert/strict";
import test from "node:test";
import xterm from "@xterm/xterm";
import headless from "@xterm/headless";
import { TerminalViewScope } from "./local-client.ts";
import type { Chunk, GeometryEvent, TerminalSize, TerminalStatus } from "./local-client.ts";
import { OrderedGeometry } from "./terminal-geometry.ts";
import { bindTerminalProtocol } from "./terminal-protocol.ts";

const encode = (text: string) => Array.from(new TextEncoder().encode(text));
const query = "\x1b[c\x1b[>c\x1b[5n\x1b[6n\x1b[?6n\x1b[4$p\x1b[?1004$p\x1b[18t" +
  "\x1bP$qm\x1b\\\x1bP+q544e\x1b\\\x1b]4;1;?;2;?\x07\x1b]10;?\x07\x1b]11;?\x1b\\\x1b]12;?\x07";

type Emulator = InstanceType<typeof xterm.Terminal> | InstanceType<typeof headless.Terminal>;
type LeasedInput = { view: string; bytes: number[]; paste: boolean };
type Color = { css: string; rgba: number };
type Palette = { foreground: Color; background: Color; cursor: Color; ansi: Color[] };

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(done => { resolve = done; });
  return { promise, resolve };
}

function leasedTerminal(terminal: Emulator, view = "current", deliver?: (frame: LeasedInput) => Promise<void>) {
  const leased: LeasedInput[] = [];
  const emitted: string[] = [];
  terminal.onData(data => emitted.push(data));
  let sent = Promise.resolve();
  const scope = new TerminalViewScope({
    async input(view, bytes, paste) {
      const frame = { view, bytes, paste };
      leased.push(frame);
      await deliver?.(frame);
    },
    inputFailure: () => assert.fail("input failed"),
    retire: () => protocol.dispose(),
    reconnect: () => {},
  });
  const protocol = bindTerminalProtocol(terminal, (bytes, paste) => { sent = scope.send(bytes, paste); });
  assert.equal(protocol.inputAvailable, true, "the input source in locked xterm 6.0.0 must be available");
  assert.equal(scope.bind(view), true);
  function consume(chunk: Chunk, render: () => Promise<void>) {
    return protocol.consume(scope, view, chunk, render);
  }
  async function write(data: readonly number[]) {
    await new Promise<void>(resolve => terminal.write(Uint8Array.from(data), resolve));
  }
  async function output(text: string, writable = true) {
    for (const byte of encode(text)) {
      const data = [byte];
      await consume({ closed: false, writable, data }, () => write(data));
    }
  }
  return {
    scope, leased, emitted, consume, write, output,
    setWritable: (writable: boolean) => consume({ closed: false, writable, data: [] }, async () => {}),
    sent: () => sent,
    dispose: () => { scope.retire(); terminal.dispose(); },
  };
}

function browserFixture() {
  const terminal = new xterm.Terminal({ allowProposedApi: true, disableStdin: true });
  const core = (terminal as unknown as { _core: {
    element: { classList: Pick<DOMTokenList, "add" | "remove" | "contains"> };
    textarea: { value: string; focus(): void; blur(): void };
    _handleTextAreaFocus(): void;
    _handleTextAreaBlur(): void;
    _compositionHelper: { keydown(): boolean };
    _keyDown(event: Partial<KeyboardEvent>): void;
    _inputEvent(event: Partial<InputEvent>): boolean;
    coreMouseService: {
      triggerMouseEvent(event: {
        col: number; row: number; x: number; y: number; button: number; action: number;
        ctrl: boolean; alt: boolean; shift: boolean;
      }): boolean;
    };
    _themeService: {
      colors: Palette;
      modifyColors(change: (colors: Palette) => void): void;
      restoreColor(slot?: number): void;
    };
  } })._core;
  const classes = new Set<string>();
  core.element = { classList: {
    add: name => { classes.add(name); },
    remove: name => { classes.delete(name); },
    contains: name => classes.has(name),
  } };
  core.textarea = {
    value: "",
    focus: () => core._handleTextAreaFocus(),
    blur: () => core._handleTextAreaBlur(),
  };
  core._compositionHelper = { keydown: () => true };
  const defaultColor = { css: "#000000", rgba: 0x000000ff };
  const colors: Palette = {
    foreground: defaultColor, background: defaultColor, cursor: defaultColor,
    ansi: Array<Color>(256).fill(defaultColor),
  };
  const restores: (number | undefined)[] = [];
  core._themeService = {
    colors,
    modifyColors: change => change(colors),
    restoreColor(slot) {
      restores.push(slot);
      if (slot === undefined) colors.ansi.fill(defaultColor);
      else if (slot === 256) colors.foreground = defaultColor;
      else if (slot === 257) colors.background = defaultColor;
      else if (slot === 258) colors.cursor = defaultColor;
      else colors.ansi[slot] = defaultColor;
    },
  };
  return {
    terminal, colors, restores,
    focused: () => classes.has("focus"),
    arrowUp: () => core._keyDown({
      key: "ArrowUp", keyCode: 38, getModifierState: () => false, preventDefault() {}, stopPropagation() {},
    }),
    insertText: (data: string) => core._inputEvent({
      data, inputType: "insertText", composed: false, preventDefault() {}, stopPropagation() {},
    }),
    click: (col: number, row: number) => core.coreMouseService.triggerMouseEvent({
      col, row, x: 0, y: 0, button: 0, action: 1, ctrl: false, alt: false, shift: false,
    }),
  };
}

test("fragmented negotiation emits no leased bytes and real key and binary input survive", async t => {
  const browser = browserFixture();
  const terminal = browser.terminal;
  terminal.resize(120, 24);
  const fixture = leasedTerminal(terminal);
  t.after(fixture.dispose);
  await fixture.setWritable(true);
  terminal.focus();
  assert.equal(browser.focused(), true);
  await fixture.output("\x1b[?1004;1;1000h" + query);
  terminal.input("ordinary-looking generated data", false);
  assert.equal(terminal.modes.sendFocusMode, true);
  assert.equal(terminal.modes.applicationCursorKeysMode, true);
  terminal.blur();
  assert.equal(browser.focused(), false);
  terminal.focus();
  assert.equal(browser.focused(), true);
  assert.deepEqual(fixture.emitted, []);
  assert.deepEqual(fixture.leased, []);

  browser.arrowUp();
  assert.equal(browser.click(100, 1), true);

  terminal.input("\x1b[I", true);
  await fixture.output("\x1b[?1004l");
  terminal.blur();
  terminal.focus();
  assert.equal(browser.focused(), true);
  assert.equal(terminal.modes.sendFocusMode, false);
  await fixture.sent();
  assert.deepEqual(fixture.leased, [
    { view: "current", bytes: [27, 79, 65], paste: false },
    { view: "current", bytes: [27, 91, 77, 32, 133, 34], paste: false },
    { view: "current", bytes: [27, 91, 73], paste: false },
  ]);
});

test("headless parse callbacks keep split replies out of the leased sink", async t => {
  const terminal = new headless.Terminal({ allowProposedApi: true });
  const fixture = leasedTerminal(terminal);
  t.after(fixture.dispose);
  await fixture.setWritable(true);
  await fixture.output(query + "ready");
  assert.equal(terminal.buffer.active.getLine(0)?.translateToString(true), "ready");
  assert.deepEqual(fixture.emitted, []);
  assert.deepEqual(fixture.leased, []);
  terminal.input("é\r", true);
  await fixture.sent();
  assert.deepEqual(fixture.leased, [{ view: "current", bytes: [195, 169, 13], paste: false }]);
});

test("mixed OSC queries preserve indexed and special color sets in their original slots", async t => {
  const browser = browserFixture();
  const fixture = leasedTerminal(browser.terminal);
  t.after(fixture.dispose);
  await fixture.setWritable(true);

  await fixture.output("\x1b]4;1;?;2;#123456;3;?;4;rgb:aa/bb/cc\x1b\\");
  assert.deepEqual(browser.colors.ansi.slice(1, 5).map(color => color.rgba),
    [0x000000ff, 0x123456ff, 0x000000ff, 0xaabbccff]);
  assert.deepEqual(fixture.leased, []);
  await fixture.output("\x1b]10;?;#102030;?\x07\x1b]11;?;#456789\x07");
  assert.equal(browser.colors.foreground.rgba, 0x000000ff);
  assert.equal(browser.colors.background.rgba, 0x102030ff);
  assert.equal(browser.colors.cursor.rgba, 0x456789ff);
  await fixture.output("\x1b]10;#abcdef\x07\x1b]12;#654321\x07");
  assert.equal(browser.colors.foreground.rgba, 0xabcdefff);
  assert.equal(browser.colors.background.rgba, 0x102030ff);
  assert.equal(browser.colors.cursor.rgba, 0x654321ff);
  assert.deepEqual(fixture.emitted, []);
  assert.deepEqual(fixture.leased, []);

  await fixture.output("\x1b]104;2\x07\x1b]110\x07\x1b]112\x07");
  assert.deepEqual(browser.restores, [2, 256, 258]);
  assert.equal(browser.colors.ansi[2]?.rgba, 0x000000ff);
  assert.equal(browser.colors.ansi[4]?.rgba, 0xaabbccff);
  assert.equal(browser.colors.foreground.rgba, 0x000000ff);
  assert.equal(browser.colors.background.rgba, 0x102030ff);
  assert.equal(browser.colors.cursor.rgba, 0x000000ff);
  await fixture.output("\x1b]104\x1b\\\x1b]111\x07");
  assert.deepEqual(browser.restores, [2, 256, 258, undefined, 257]);
  assert.equal(browser.colors.ansi[4]?.rgba, 0x000000ff);
  assert.equal(browser.colors.background.rgba, 0x000000ff);
  assert.deepEqual(fixture.leased, []);
});

function terminalStatus(view: string, revision: number, size: TerminalSize): TerminalStatus {
  const epoch = { agent: "agent", owner: "owner" };
  return {
    terminal: {
      view_id: view, agent_epoch: epoch.agent,
      target: { runtime_id: "runtime", generation: "generation", session: "$0", window: "@0", pane: "%0",
        pid: 42, process_start: 12345 },
    },
    epoch, committed: { revision, size, intent: revision ? { id: "resize", epoch } : null },
    provider: null, observation: "pending",
  };
}

test("parse completion orders the grid and target redraw while genuine input remains independent", { timeout: 3000 }, async t => {
  const browser = browserFixture();
  const terminal = browser.terminal;
  const inputEntered = deferred<void>();
  const completeInput = deferred<void>();
  const fixture = leasedTerminal(terminal, "current", async frame => {
    if (frame.bytes.length !== 1 || frame.bytes[0] !== 0x78) return;
    inputEntered.resolve();
    await completeInput.promise;
  });
  t.after(() => {
    completeInput.resolve();
    fixture.dispose();
  });
  const size = { cols: 91, rows: 27 };
  const sample = { size, sampled_at_us: 1 };
  const event: GeometryEvent = {
    status: terminalStatus("current", 1, size), bridge_pty: sample, client: sample,
  };
  const ordered = new OrderedGeometry(terminalStatus("current", 0, { cols: terminal.cols, rows: terminal.rows }));
  const parserEntered = deferred<void>();
  const resumeParser = deferred<boolean>();
  const ackEntered = deferred<void>();
  const acknowledge = deferred<GeometryEvent>();
  const delayed = terminal.parser.registerCsiHandler({ final: "n" }, () => {
    parserEntered.resolve();
    return resumeParser.promise;
  });
  t.after(() => delayed.dispose());
  const order: string[] = [];
  const sink = {
    async write(data: readonly number[]) {
      await fixture.write(data);
      order.push("parsed");
    },
    resize(grid: TerminalSize) {
      order.push("grid");
      terminal.resize(grid.cols, grid.rows);
      return { cols: terminal.cols, rows: terminal.rows };
    },
    acknowledge(received: GeometryEvent, grid: TerminalSize) {
      order.push("ack");
      assert.equal(received.status.terminal.view_id, "current");
      assert.deepEqual(grid, size);
      assert.equal(browser.focused(), true);
      ackEntered.resolve();
      return acknowledge.promise;
    },
  };
  const consume = (data: readonly number[], writable: boolean, geometry?: GeometryEvent) => {
    const chunk: Chunk = { closed: false, writable, data, geometry: geometry ?? null };
    return fixture.consume(chunk, () => ordered.consume(chunk, sink));
  };
  await fixture.setWritable(true);
  terminal.focus();
  await consume(encode("\x1b[?104"), true);
  await consume([0x39, 0x68, ...encode("\x1b[?1004h"), 0xe6, 0xbc], true);
  await consume([0xa2, ...encode("\x1bP$q")], true);
  await consume(encode("m\x1b\\\x1b]11;"), true);
  await consume(encode("?\x07\x1b[6"), true);
  const pending = consume(encode("n\x1b]11;?;#456789\x07"), true);
  await parserEntered.promise;
  assert.equal(order.length, 5);
  assert.equal(terminal.buffer.active.type, "alternate");
  assert.equal(terminal.buffer.active.getLine(0)?.translateToString(true), "漢");
  assert.equal(terminal.modes.sendFocusMode, true);
  assert.equal(browser.colors.cursor.rgba, 0x000000ff);
  browser.arrowUp();
  assert.equal(browser.insertText("ї"), true);
  // @contract: Deferred IME commits enter the same xterm source with wasUserInput=true.
  await new Promise<void>(resolve => setImmediate(() => {
    terminal.input("漢", true);
    resolve();
  }));
  terminal.blur();
  assert.equal(browser.focused(), false);
  terminal.focus();
  assert.equal(browser.focused(), true);
  await fixture.sent();
  const duringParse = [[27, 91, 65], encode("ї"), encode("漢")];
  assert.deepEqual(fixture.leased.map(frame => frame.bytes), duringParse);

  resumeParser.resolve(false);
  await pending;
  assert.equal(browser.colors.background.rgba, 0x000000ff);
  assert.equal(browser.colors.cursor.rgba, 0x456789ff);
  assert.deepEqual(fixture.emitted, ["\x1b[A", "ї", "漢"]);
  assert.deepEqual(fixture.leased.map(frame => frame.bytes), duringParse);
  delayed.dispose();
  assert.equal(browser.insertText("x"), true);
  await inputEntered.promise;
  assert.equal(browser.insertText("queued before revocation"), true);
  const resizing = consume([], false, event);
  await ackEntered.promise;
  assert.equal(fixture.scope.canWrite, false);
  assert.equal(terminal.options.disableStdin, true);
  browser.arrowUp();
  assert.equal(browser.insertText("blocked"), true);
  completeInput.resolve();
  await fixture.sent();
  const accepted = [...duringParse, encode("x")];
  assert.deepEqual(fixture.leased.map(frame => frame.bytes), accepted);
  acknowledge.resolve(event);
  await resizing;
  assert.equal(fixture.scope.canWrite, false);
  const redraw = [
    encode("\x1b[?1049l\x1b[?10"),
    [...encode("49h\x1b[H"), 0xd0],
    [0xbd, ...encode("овий\x1bP$q")],
    encode("m\x1b\\\x1b[6"),
    encode("n\x1b]10;"),
    encode("?\x07"),
  ];
  for (const data of redraw) await consume(data, false);
  browser.arrowUp();
  await fixture.sent();
  assert.equal(ordered.applied, 1);
  assert.equal(terminal.buffer.active.getLine(0)?.translateToString(true), "новий");
  assert.equal(browser.focused(), true);
  assert.deepEqual(order, [...Array<string>(6).fill("parsed"), "grid", "ack", ...redraw.map(() => "parsed")]);
  assert.deepEqual(fixture.leased.map(frame => frame.bytes), accepted);

  await fixture.setWritable(true);
  terminal.blur();
  terminal.focus();
  browser.arrowUp();
  await fixture.sent();
  assert.equal(browser.focused(), true);
  assert.deepEqual(fixture.leased.map(frame => frame.bytes), [...accepted, [27, 91, 65]]);
});

test("a fresh attachment drops old parser completion and queued input", { timeout: 3000 }, async t => {
  const original = browserFixture();
  const terminal = original.terminal;
  const inFlight = deferred<void>();
  const firstDelivered = deferred<void>();
  const parserEntered = deferred<void>();
  const resumeParser = deferred<boolean>();
  const delayed = terminal.parser.registerCsiHandler({ final: "z" }, () => {
    parserEntered.resolve();
    return resumeParser.promise;
  });
  const old = leasedTerminal(terminal, "old", () => {
    firstDelivered.resolve();
    return inFlight.promise;
  });
  t.after(() => {
    resumeParser.resolve(true);
    inFlight.resolve();
    delayed.dispose();
    old.dispose();
  });
  await old.setWritable(true);
  terminal.focus();
  assert.equal(original.focused(), true);
  await old.output("\x1b]10;?");
  terminal.input("first", true);
  await firstDelivered.promise;
  terminal.input("queued", true);
  const data = encode("\x07\x1b[z");
  const pending = old.consume({ closed: false, writable: true, data }, () => old.write(data));
  await parserEntered.promise;
  old.scope.retire();
  assert.equal(terminal.options.disableStdin, true);
  assert.equal(await old.setWritable(true), false);

  const replacement = browserFixture();
  const nextTerminal = replacement.terminal;
  const fresh = leasedTerminal(nextTerminal, "fresh");
  t.after(fresh.dispose);
  await fresh.setWritable(true);
  await fresh.output("\x1b[?1004;1000hready" + query);
  assert.equal(nextTerminal.buffer.active.getLine(0)?.translateToString(true), "ready");
  nextTerminal.focus();
  assert.equal(replacement.focused(), true);
  terminal.input("stale", true);
  resumeParser.resolve(true);
  inFlight.resolve();
  assert.equal(await pending, false);
  assert.equal(terminal.options.disableStdin, true);
  await old.sent();
  assert.deepEqual(old.leased, [{ view: "old", bytes: encode("first"), paste: false }]);
  assert.deepEqual(fresh.leased, []);

  replacement.arrowUp();
  nextTerminal.blur();
  assert.equal(replacement.focused(), false);
  nextTerminal.focus();
  assert.equal(replacement.focused(), true);
  assert.equal(replacement.click(2, 3), true);
  nextTerminal.input("\x1b[200~Привіт\r\x1b[201~", true);
  await fresh.sent();
  assert.deepEqual(fresh.leased, [
    { view: "fresh", bytes: [27, 91, 65], paste: false },
    { view: "fresh", bytes: [27, 91, 77, 32, 35, 36], paste: false },
    { view: "fresh", bytes: encode("Привіт\r"), paste: true },
  ]);
});

test("an unavailable xterm input hook leaves the leased route disconnected after a writable read", async t => {
  const headlessTerminal = new headless.Terminal({ allowProposedApi: true });
  t.after(() => headlessTerminal.dispose());
  for (const coreService of [undefined, Object.freeze({ triggerDataEvent() {} })]) {
    const terminal = {
      _core: { coreService },
      parser: headlessTerminal.parser,
      options: { disableStdin: false },
      onBinary: () => assert.fail("binary input subscribed without a compatible source"),
    };
    const scope = new TerminalViewScope({
      input: () => assert.fail("unexpected leased input"),
      inputFailure: () => assert.fail("input failed"),
      retire: () => protocol.dispose(),
      reconnect: () => {},
    });
    const protocol = bindTerminalProtocol(terminal, () => assert.fail("unexpected user input"));
    try {
      assert.equal(protocol.inputAvailable, false);
      assert.equal(scope.bind("current"), true);
      let rendered = false;
      const chunk = { closed: false, writable: true, data: [0x78] };
      assert.equal(await protocol.consume(scope, "current", chunk, async () => { rendered = true; }), true);
      assert.equal(rendered, true);
      assert.equal(terminal.options.disableStdin, true);
    } finally {
      scope.retire();
    }
  }
});
