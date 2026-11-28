import assert from "node:assert/strict";
import test from "node:test";
import xterm from "@xterm/xterm";
import headless from "@xterm/headless";
import { TerminalDelivery, TerminalViewScope, parseChunk, pasteIssueText } from "./local-client.ts";
import type { Chunk, GeometryEvent, TerminalSize, TerminalStatus } from "./local-client.ts";
import { OrderedGeometry } from "./terminal-geometry.ts";
import { bindTerminalPaste, bindTerminalProtocol, encodeTerminalPaste, TERMINAL_INPUT_BUDGET } from "./terminal-protocol.ts";

const encode = (text: string) => Array.from(new TextEncoder().encode(text));
const query = "\x1b[c\x1b[>c\x1b[5n\x1b[6n\x1b[?6n\x1b[4$p\x1b[?1004$p\x1b[18t" +
  "\x1bP$qm\x1b\\\x1bP+q544e\x1b\\\x1b]4;1;?;2;?\x07\x1b]10;?\x07\x1b]11;?\x1b\\\x1b]12;?\x07";

type Emulator = InstanceType<typeof xterm.Terminal> | InstanceType<typeof headless.Terminal>;
type LeasedInput = { view: string; bytes: number[] };
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
    async input(view, bytes) {
      const frame = { view, bytes };
      leased.push(frame);
      await deliver?.(frame);
    },
    inputFailure: () => assert.fail("input failed"),
    retire: () => protocol.dispose(),
    reconnect: () => {},
  });
  const protocol = bindTerminalProtocol(terminal, bytes => { sent = scope.send(bytes); });
  assert.equal(protocol.inputAvailable, true, "the input source in locked xterm 6.0.0 must be available");
  assert.equal(scope.bind(view), true);
  function consume(chunk: Chunk, render: () => Promise<void>) {
    return protocol.consume(scope, view, chunk.writable ? { ...chunk, paste_incarnation: firstIncarnation } : chunk, render);
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
    { view: "current", bytes: [27, 79, 65] },
    { view: "current", bytes: [27, 91, 77, 32, 133, 34] },
    { view: "current", bytes: [27, 91, 73] },
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
  assert.deepEqual(fixture.leased, [{ view: "current", bytes: [195, 169, 13] }]);
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
  assert.deepEqual(old.leased, [{ view: "old", bytes: encode("first") }]);
  assert.deepEqual(fresh.leased, []);

  replacement.arrowUp();
  nextTerminal.blur();
  assert.equal(replacement.focused(), false);
  nextTerminal.focus();
  assert.equal(replacement.focused(), true);
  assert.equal(replacement.click(2, 3), true);
  nextTerminal.input("Привіт\r", true);
  await fresh.sent();
  assert.deepEqual(fresh.leased, [
    { view: "fresh", bytes: [27, 91, 65] },
    { view: "fresh", bytes: [27, 91, 77, 32, 35, 36] },
    { view: "fresh", bytes: encode("Привіт\r") },
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

type ViewCallbacks = ConstructorParameters<typeof TerminalViewScope>[0];
type PasteCallbacks = NonNullable<ViewCallbacks["paste"]>;
type Gesture = Parameters<PasteCallbacks["stage"]>[0];
type StageArgs = Parameters<PasteCallbacks["stage"]>;
type Call = { kind: "input"; viewId: string; data: number[]; incarnation: string }
  | { kind: "stage"; gesture: Gesture; offset: number; data: number[] }
  | { kind: "commit"; gesture: Gesture; bytes: number }
  | { kind: "ack"; gesture: Gesture; receipt: string };

const firstIncarnation = "10000000-0000-4000-8000-000000000001";
const nextIncarnation = "10000000-0000-4000-8000-000000000002";
const receiptId = "20000000-0000-4000-8000-000000000001";
const readOnly: Chunk = { writable: false, closed: false, data: [] };

function pasteFixture(
  options: Partial<PasteCallbacks & Pick<ViewCallbacks, "input" | "inputFailure">> = {},
  delivery?: TerminalDelivery,
  viewId = "view",
) {
  const calls: Call[] = [];
  const protocolSends: { data: number[]; work: Promise<void> }[] = [];
  const browser = browserFixture();
  let nativeIncarnation = firstIncarnation;
  const scope = new TerminalViewScope({
    async input(viewId, data, incarnation) {
      calls.push({ kind: "input", viewId, data, incarnation });
      await options.input?.(viewId, data, incarnation);
    },
    paste: {
      async stage(gesture, offset, data) {
        calls.push({ kind: "stage", gesture, offset, data });
        if (gesture.incarnation !== nativeIncarnation) return { result: "stale" };
        return options.stage ? options.stage(gesture, offset, data) : { result: "staged" };
      },
      async commit(gesture, bytes) {
        calls.push({ kind: "commit", gesture, bytes });
        if (gesture.incarnation !== nativeIncarnation) return { result: "stale" };
        return options.commit ? options.commit(gesture, bytes) : { result: "committed", receipt: receiptId };
      },
      async ack(gesture, receipt) {
        calls.push({ kind: "ack", gesture, receipt });
        assert.equal(receipt, receiptId);
        return options.ack ? options.ack(gesture, receipt) : { result: "acknowledged", writable: true };
      },
      changed() {
        if (!scope.canWrite) protocol.pauseInput();
        options.changed?.();
      },
    },
    inputFailure: options.inputFailure ?? (() => assert.fail("Unexpected ordinary input failure")),
    reconnect: () => {},
    retire: () => protocol.dispose(),
  }, delivery);
  const protocol = bindTerminalProtocol(browser.terminal, bytes => {
    protocolSends.push({ data: Array.from(bytes), work: scope.send(bytes) });
  });
  assert(scope.bind(viewId));
  const consume = (chunk: Chunk) => protocol.consume(scope, viewId, chunk, async () => {});
  return {
    browser, scope, calls, consume, protocolSends,
    enableMouse: () => new Promise<void>(resolve => browser.terminal.write("\x1b[?1000h", resolve)),
    turnOwner(incarnation: string) { nativeIncarnation = incarnation; },
    publish: (incarnation: string) => consume({ ...readOnly, writable: true, paste_incarnation: incarnation }),
    paste(text: string) { return scope.paste(encodeTerminalPaste(text)); },
    dispose() { scope.retire(); browser.terminal.dispose(); },
  };
}

class ClipboardEventPath extends EventTarget {
  private readonly capture = new EventTarget();
  readonly element = new EventTarget();
  readonly textarea = new EventTarget();

  override addEventListener(type: string, listener: EventListenerOrEventListenerObject | null, options?: boolean | AddEventListenerOptions) {
    const capture = options === true || (typeof options === "object" && options.capture);
    (capture ? this.capture : this.element).addEventListener(type, listener);
  }

  override removeEventListener(type: string, listener: EventListenerOrEventListenerObject | null, options?: boolean | EventListenerOptions) {
    const capture = options === true || (typeof options === "object" && options.capture);
    (capture ? this.capture : this.element).removeEventListener(type, listener);
  }

  paste(target: EventTarget, text: string) {
    const event = new Event("paste", { cancelable: true, bubbles: true });
    const requested: string[] = [];
    Object.defineProperty(event, "clipboardData", { value: {
      getData(type: string) {
        requested.push(type);
        return type === "text/plain" ? text : "<p>not plain text</p>";
      },
    } });
    let stopped = false;
    const stop = event.stopImmediatePropagation.bind(event);
    event.stopImmediatePropagation = () => { stopped = true; stop(); };
    this.capture.dispatchEvent(event);
    if (!stopped) target.dispatchEvent(event);
    if (!stopped && target === this.textarea) this.element.dispatchEvent(event);
    return { prevented: event.defaultPrevented, stopped, requested };
  }
}

async function checkClipboardCapture(bracketed: boolean) {
  const f = pasteFixture();
  try {
    await f.publish(firstIncarnation);
    const terminal = f.browser.terminal;
    await new Promise<void>(resolve => terminal.write(bracketed ? "\x1b[?2004h" : "\x1b[?2004l", resolve));
    assert.equal(terminal.modes.bracketedPasteMode, bracketed);
    const path = new ClipboardEventPath();
    let leaked = 0;
    const xtermListener = (event: Event) => {
      leaked++;
      terminal.paste((event as ClipboardEvent).clipboardData!.getData("text/plain"));
      event.stopImmediatePropagation();
    };
    path.element.addEventListener("paste", xtermListener);
    path.textarea.addEventListener("paste", xtermListener);
    let queued = Promise.resolve();
    const capture = bindTerminalPaste(path, bytes => { queued = f.scope.paste(bytes); });
    for (const target of [path.textarea, path.element]) {
      assert.deepEqual(path.paste(target, "друк\r\n🙂"), {
        prevented: true, stopped: true, requested: ["text/plain"],
      });
      await queued;
    }
    for (const text of ["", "x".repeat(TERMINAL_INPUT_BUDGET + 1)]) {
      assert.equal(path.paste(path.element, text).prevented, true);
      await queued;
    }
    await f.consume(readOnly);
    assert.equal(path.paste(path.textarea, "read-only").stopped, true);
    await queued;
    assert.equal(leaked, 0);
    assert.deepEqual(f.calls.map(call => call.kind), ["stage", "commit", "ack", "stage", "commit", "ack"]);
    assert.deepEqual(f.calls.filter(call => call.kind === "stage").map(call => call.data),
      [encode("друк\n🙂"), encode("друк\n🙂")]);
    assert.deepEqual(f.calls.filter(call => call.kind === "stage").map(call => call.gesture.operation), [1, 2]);
    capture.dispose();
    assert.equal(path.paste(path.element, "after disposal").prevented, false);
    assert.equal(leaked, 1);
    assert.equal(f.calls.length, 6);
  } finally {
    f.dispose();
  }
}

test("clipboard capture precedes xterm listeners in normal mode", () => checkClipboardCapture(false));
test("clipboard capture precedes xterm listeners in bracketed mode", () => checkClipboardCapture(true));

const mouseBytes = [27, 91, 77, 32, 35, 36];

for (const outcome of ["acknowledged", "lost"] as const) {
  test(`queued ordinary text and binary retain their captured lease when the predecessor reply is ${outcome}`, async () => {
    const entered = deferred<void>();
    const reply = deferred<void>();
    const lostReply = Error("Predecessor reply lost");
    const failures: unknown[] = [];
    const delivery = new TerminalDelivery();
    const f = pasteFixture({
      async input(_view, _data, incarnation) {
        if (incarnation === firstIncarnation) {
          entered.resolve();
          await reply.promise;
          if (outcome === "lost") throw lostReply;
        } else {
          assert.equal(incarnation, nextIncarnation);
        }
      },
      inputFailure(error) { failures.push(error); },
    }, delivery);
    try {
      await f.publish(firstIncarnation);
      await f.enableMouse();
      const preceding = f.scope.send(Uint8Array.of(65));
      await entered.promise;
      f.browser.insertText("старий");
      assert(f.browser.click(2, 3));
      assert.deepEqual(f.protocolSends.map(send => send.data), [encode("старий"), mouseBytes]);
      const expected: Call[] = [{ kind: "input", viewId: "view", data: [65], incarnation: firstIncarnation }];
      assert.deepEqual(f.calls, expected);

      f.turnOwner(nextIncarnation);
      assert(await f.publish(nextIncarnation));
      assert(f.scope.canWrite);
      const fresh = "x".repeat(4095) + "Ж";
      const freshBytes = encode(fresh);
      f.browser.insertText(fresh);
      assert(f.browser.click(2, 3));
      assert.deepEqual(f.protocolSends.map(send => send.data),
        [encode("старий"), mouseBytes, freshBytes, mouseBytes]);
      assert.deepEqual(f.calls, expected);
      reply.resolve();
      await Promise.all([preceding, ...f.protocolSends.map(send => send.work)]);

      if (outcome === "acknowledged") {
        expected.push(...[freshBytes.slice(0, 4096), [0x96], mouseBytes].map<Call>(data => ({
          kind: "input", viewId: "view", data, incarnation: nextIncarnation,
        })));
        assert.deepEqual(failures, []);
        assert(f.scope.canWrite);
        assert(delivery.mayRestore);
      } else {
        assert.equal(failures.length, 1);
        assert.equal(failures[0], lostReply);
        assert(!f.scope.canWrite);
        assert(delivery.held);
        assert(!delivery.mayRestore);
        assert.deepEqual(f.calls, expected);

        const laterIncarnation = "10000000-0000-4000-8000-000000000003";
        for (const incarnation of [nextIncarnation, laterIncarnation]) {
          f.turnOwner(incarnation);
          assert(await f.publish(incarnation));
          await attemptHeldInput(f);
        }
        assert.equal(failures.length, 1);
      }
      assert.deepEqual(f.calls, expected);
    } finally {
      reply.resolve();
      f.dispose();
    }
  });
}

test("queued paste keeps its lease on same-view reacquisition without a loss event", async () => {
  for (const observed of [true, false]) {
    const entered = deferred<void>();
    const keyboard = deferred<void>();
    const f = pasteFixture({ input: async () => { entered.resolve(); await keyboard.promise; } });
    try {
      await f.publish(firstIncarnation);
      const before = f.scope.send(Uint8Array.of(65));
      const stale = f.paste("old");
      await entered.promise;
      f.turnOwner(nextIncarnation);
      if (observed) await f.publish(nextIncarnation);
      keyboard.resolve();
      await Promise.all([before, stale]);
      assert.deepEqual(f.calls.map(call => call.kind), observed ? ["input"] : ["input", "stage"]);
      const attempted = f.calls.find(call => call.kind === "stage");
      if (attempted) assert.equal(attempted.gesture.incarnation, firstIncarnation);
      assert.equal(f.scope.pasteMessage, pasteIssueText.stale);
      if (!observed) await f.publish(nextIncarnation);
      await f.paste("fresh");
      const tail = f.calls.slice(-3);
      assert.deepEqual(tail.map(call => call.kind), ["stage", "commit", "ack"]);
      for (const call of tail) {
        assert(call.kind !== "input");
        assert.deepEqual(call.gesture, { viewId: "view", incarnation: nextIncarnation, operation: 2 });
      }
    } finally {
      keyboard.resolve();
      f.dispose();
    }
  }
});

test("lease loss between paste chunks prevents commit", async () => {
  for (const change of ["read-only", "successor", "withheld", "retired"]) {
    const entered = deferred<void>();
    const response = deferred<{ result: "staged" }>();
    const f = pasteFixture({ async stage(...[gesture, offset]: StageArgs) {
      if (gesture.operation === 1 && offset === 0) { entered.resolve(); return response.promise; }
      return { result: "staged" };
    } });
    try {
      await f.publish(firstIncarnation);
      const pending = f.paste("a".repeat(4096) + "☃");
      await entered.promise;
      f.turnOwner(nextIncarnation);
      if (change === "read-only") await f.consume(readOnly);
      if (change === "successor") await f.publish(nextIncarnation);
      if (change === "retired") f.scope.retire();
      response.resolve({ result: "staged" });
      await pending;
      const attempts = f.calls.filter(call => call.kind === "stage");
      assert.deepEqual(attempts.map(call => call.offset), change === "withheld" ? [0, 4096] : [0]);
      for (const call of attempts) assert.equal(call.gesture.incarnation, firstIncarnation);
      assert.equal(f.calls.length, attempts.length);
      if (change === "retired") {
        assert.equal(f.scope.pasteMessage, undefined);
      } else {
        assert.equal(f.scope.pasteMessage, pasteIssueText.stale);
        await f.publish(nextIncarnation);
        await f.paste("fresh");
        const commit = f.calls.find(call => call.kind === "commit");
        assert.equal(commit?.gesture.incarnation, nextIncarnation);
      }
    } finally {
      response.resolve({ result: "staged" });
      f.dispose();
    }
  }
});

test("paste chunks reassemble UTF-8 after CRLF normalization", async () => {
  const f = pasteFixture();
  try {
    await f.publish(firstIncarnation);
    const unicode = "Ж\r\n🙂\r日本語\n\u0009";
    const boundary = "a".repeat(4095);
    const exactCap = "x".repeat(TERMINAL_INPUT_BUDGET);
    for (const [input, expected] of [
      [unicode, "Ж\n🙂\n日本語\n\t"],
      [boundary + unicode, boundary + "Ж\n🙂\n日本語\n\t"],
      [exactCap, exactCap],
    ] as const) {
      f.calls.length = 0;
      await f.paste(input);
      const expectedBytes = encode(expected);
      const chunks = f.calls.filter(call => call.kind === "stage");
      assert.deepEqual(chunks.flatMap(call => call.data), expectedBytes);
      assert.deepEqual(chunks.map(call => call.offset), chunks.map((_, index) => index * 4096));
      assert(chunks.every(call => call.data.length > 0 && call.data.length <= 4096));
      if (input.startsWith(boundary)) {
        assert.equal(chunks[0]?.data[4095], 0xd0);
        assert.equal(chunks[1]?.data[0], 0x96);
      }
      const commits = f.calls.filter(call => call.kind === "commit");
      assert.deepEqual(commits.map(call => call.bytes), [expectedBytes.length]);
      assert.deepEqual(f.calls.map(call => call.kind).slice(-2), ["commit", "ack"]);
      assert(!f.calls.some(call => call.kind === "input"));
    }
    assert.equal(encodeTerminalPaste("🙂".repeat(TERMINAL_INPUT_BUDGET / 4 + 1)), null);
    assert.equal(encodeTerminalPaste(exactCap + "x"), null);
    assert.deepEqual(Array.from(encodeTerminalPaste("\r\n".repeat(TERMINAL_INPUT_BUDGET))!), Array.from({ length: TERMINAL_INPUT_BUDGET }, () => 10));
  } finally {
    f.dispose();
  }
});

test("paste and keyboard share one FIFO input queue", async () => {
  const entered = deferred<void>();
  const keyboard = deferred<void>();
  const firstCommitEntered = deferred<Gesture>();
  const firstReceipt = deferred<{ result: "committed"; receipt: string }>();
  const commitEntered = deferred<Gesture>();
  const knownReceipt = deferred<{ result: "committed"; receipt: string }>();
  const f = pasteFixture({
    input: async (_view, bytes) => {
      if (bytes[0] === 65) { entered.resolve(); await keyboard.promise; }
    },
    async commit(gesture) {
      if (gesture.operation === 1) { firstCommitEntered.resolve(gesture); return firstReceipt.promise; }
      if (gesture.operation === 2) return { result: "unavailable" };
      if (gesture.operation === 3) { commitEntered.resolve(gesture); return knownReceipt.promise; }
      return { result: "committed", receipt: receiptId };
    },
    async ack(gesture) { return { result: "acknowledged", writable: gesture.operation !== 3 }; },
  });
  try {
    await f.publish(firstIncarnation);
    const before = f.scope.send(Uint8Array.of(65));
    await entered.promise;
    await f.paste("x".repeat(TERMINAL_INPUT_BUDGET));
    assert.equal(f.scope.pasteMessage, pasteIssueText.oversize);
    const paste = f.paste("🙂\n");
    const after = f.scope.send(Uint8Array.of(66));
    keyboard.resolve();
    const first = await firstCommitEntered.promise;
    const firstCompletion = {
      ...readOnly,
      paste_completion: { incarnation: first.incarnation, operation: first.operation },
    };
    await f.consume(firstCompletion);
    assert(f.browser.terminal.options.disableStdin);
    firstReceipt.resolve({ result: "committed", receipt: receiptId });
    await Promise.all([before, paste, after]);
    assert.deepEqual(f.calls.map(call => call.kind), ["input", "stage", "commit", "ack", "input"]);
    await f.publish(firstIncarnation);
    assert(!f.browser.terminal.options.disableStdin);
    await f.paste("unavailable");
    assert.equal(f.scope.pasteMessage, pasteIssueText.unavailable);
    await f.scope.send(Uint8Array.of(67));
    assert.deepEqual(f.calls.slice(-3).map(call => call.kind), ["stage", "commit", "input"]);

    const inFlight = f.paste("✍");
    const expiredKey = f.scope.send(Uint8Array.of(68));
    const { incarnation, operation } = await commitEntered.promise;
    await f.consume(readOnly);
    knownReceipt.resolve({ result: "committed", receipt: receiptId });
    await Promise.all([inFlight, expiredKey]);
    assert.deepEqual(f.calls.slice(-3).map(call => call.kind), ["stage", "commit", "ack"]);
    assert.deepEqual(f.calls.filter(call => call.kind === "input").map(call => call.data), [[65], [66], [67]]);
    await f.consume({ ...readOnly, paste_completion: { incarnation, operation } });
    await f.publish(firstIncarnation);
    assert(!f.scope.canWrite);
  } finally {
    keyboard.resolve();
    firstReceipt.resolve({ result: "committed", receipt: receiptId });
    knownReceipt.resolve({ result: "committed", receipt: receiptId });
    f.dispose();
  }
});

test("definite paste rejection preserves queued keyboard input after a concurrent read", async () => {
  const entered = deferred<void>();
  const rejected = deferred<{ result: "unavailable" }>();
  const f = pasteFixture({
    commit() { entered.resolve(); return rejected.promise; },
  });
  try {
    await f.publish(firstIncarnation);
    const paste = f.paste("draft");
    const queuedInput = f.scope.send(Uint8Array.of(65));
    await entered.promise;

    await f.consume({
      ...readOnly,
      writable: true,
      paste_incarnation: firstIncarnation,
    });
    assert.equal(f.scope.canWrite, true);

    rejected.resolve({ result: "unavailable" });
    await Promise.all([paste, queuedInput]);
    assert.deepEqual(f.calls.map(call => call.kind), ["stage", "commit", "input"]);
    assert.equal(f.scope.canWrite, true);
  } finally {
    rejected.resolve({ result: "unavailable" });
    f.dispose();
  }
});

test("a confirmed paste can acknowledge after the ordinary queue is disabled", async () => {
  const submitted = deferred<void>();
  const receipt = deferred<{ result: "committed"; receipt: string }>();
  let inputFailures = 0;
  const f = pasteFixture({
    commit() { submitted.resolve(); return receipt.promise; },
    inputFailure() { inputFailures++; },
  });
  try {
    await f.publish(firstIncarnation);
    const paste = f.paste("draft");
    await submitted.promise;
    await f.scope.send(new Uint8Array(TERMINAL_INPUT_BUDGET));
    assert.equal(inputFailures, 1);
    assert(!f.scope.canWrite);
    receipt.resolve({ result: "committed", receipt: receiptId });
    await paste;
    assert.deepEqual(f.calls.map(call => call.kind), ["stage", "commit", "ack"]);
    assert(!f.scope.canWrite);
  } finally {
    receipt.resolve({ result: "committed", receipt: receiptId });
    f.dispose();
  }
});

test("a successor snapshot retires the original paste completion binding", async () => {
  const submitted = deferred<void>();
  const receipt = deferred<{ result: "committed"; receipt: string }>();
  const f = pasteFixture({
    commit() { submitted.resolve(); return receipt.promise; },
  });
  try {
    await f.publish(firstIncarnation);
    const paste = f.paste("draft");
    await submitted.promise;
    f.turnOwner(nextIncarnation);
    await f.publish(nextIncarnation);
    await f.consume(readOnly);
    receipt.resolve({ result: "committed", receipt: receiptId });
    await paste;
    assert.deepEqual(f.calls.map(call => call.kind), ["stage", "commit"]);
    assert.equal(f.scope.pasteMessage, pasteIssueText.unknown);
  } finally {
    receipt.resolve({ result: "committed", receipt: receiptId });
    f.dispose();
  }
});

function priorSendHold() {
  const hold = parseChunk({
    closed: false, writable: false, data: [], status: null, geometry: null,
    paste_admission: "prior_send_unknown",
  });
  assert.equal(hold.paste_admission, "prior_send_unknown");
  return hold;
}

async function attemptHeldInput(f: ReturnType<typeof pasteFixture>) {
  assert(f.browser.terminal.options.disableStdin);
  assert(!f.scope.canWrite);
  const count = f.protocolSends.length;
  f.browser.insertText("held");
  f.browser.click(2, 3);
  await Promise.all([
    ...f.protocolSends.slice(count).map(send => send.work),
    f.scope.send(Uint8Array.from(encode("held"))),
    f.scope.send(Uint8Array.from(mouseBytes)),
    f.scope.paste(Uint8Array.from(encode("held paste"))),
  ]);
  assert.equal(f.protocolSends.length, count);
}

for (const holdFirst of [false, true]) {
  test(`stage stop preserves pre-submission feedback when ${holdFirst ? "the hold read" : "the stage reply"} arrives first`, async () => {
    const entered = deferred<void>();
    const reply = deferred<{ result: "unknown" }>();
    const f = pasteFixture({
      stage() { entered.resolve(); return reply.promise; },
    });
    try {
      await f.publish(firstIncarnation);
      await f.enableMouse();
      const first = f.paste("original");
      await entered.promise;
      f.browser.insertText("queued text");
      assert(f.browser.click(2, 3));
      const second = f.paste("queued paste");
      assert.deepEqual(f.protocolSends.map(send => send.data), [encode("queued text"), mouseBytes]);
      const calls: Call[] = [{
        kind: "stage", gesture: { viewId: "view", incarnation: firstIncarnation, operation: 1 },
        offset: 0, data: encode("original"),
      }];
      assert.deepEqual(f.calls, calls);
      const hold = priorSendHold();
      if (holdFirst) {
        assert(await f.consume(hold));
        assert(f.browser.terminal.options.disableStdin);
        assert(!f.scope.canWrite);
      }
      reply.resolve({ result: "unknown" });
      await Promise.all([first, second, ...f.protocolSends.map(send => send.work)]);
      const feedback = pasteIssueText.stageStopped;
      assert.match(feedback, /Це вставлення не було надіслано/);
      assert.match(feedback, /через непідтверджену доставку попереднього вводу/);
      assert.equal(f.scope.pasteMessage, feedback);
      assert(f.browser.terminal.options.disableStdin);
      assert(!f.scope.canWrite);
      if (!holdFirst) assert(await f.consume(hold));
      assert.equal(f.scope.pasteMessage, feedback);
      await attemptHeldInput(f);
      f.turnOwner(nextIncarnation);
      assert(await f.publish(nextIncarnation));
      await attemptHeldInput(f);
      assert.equal(f.scope.pasteMessage, feedback);
      assert.deepEqual(f.calls, calls);
    } finally {
      reply.resolve({ result: "unknown" });
      f.dispose();
    }
  });
}

test("lost paste commit reply reports unknown without retry", async () => {
  const f = pasteFixture({ async commit() { throw Error("IPC response lost"); } });
  try {
    await f.publish(firstIncarnation);
    await f.enableMouse();
    const original = "неповторюваний текст";
    const sent = f.paste(original);
    const queued = f.paste("queued before the reply");
    const later = f.scope.send(Uint8Array.of(65));
    await Promise.all([sent, queued, later]);
    assert.equal(f.scope.pasteMessage, pasteIssueText.unknown);
    assert.notEqual(f.scope.pasteMessage, pasteIssueText.stageStopped);
    const gesture = { viewId: "view", incarnation: firstIncarnation, operation: 1 };
    const calls: Call[] = [
      { kind: "stage", gesture, offset: 0, data: encode(original) },
      { kind: "commit", gesture, bytes: encode(original).length },
    ];
    assert.deepEqual(f.calls, calls);
    assert(f.browser.terminal.options.disableStdin);
    assert(!f.scope.canWrite);
    assert(await f.consume(priorSendHold()));
    assert.equal(f.scope.pasteMessage, pasteIssueText.unknown);
    await attemptHeldInput(f);
    assert.deepEqual(f.calls, calls);
    f.scope.retire();
    const reconnect = pasteFixture();
    try {
      assert(await reconnect.consume(priorSendHold()));
      assert.equal(reconnect.scope.pasteMessage, pasteIssueText.unknown);
      assert(reconnect.browser.terminal.options.disableStdin);
      assert(!reconnect.scope.canWrite);
      await reconnect.scope.send(Uint8Array.of(1));
      await reconnect.paste("held pane");
      assert.deepEqual(reconnect.calls, []);
    } finally {
      reconnect.dispose();
    }
  } finally {
    f.dispose();
  }
});

test("a retired paste receipt cannot authorize the restored logical view", async () => {
  const delivery = new TerminalDelivery();
  const submitted = deferred<void>();
  const receipt = deferred<{ result: "committed"; receipt: string }>();
  const prior = pasteFixture({ commit() { submitted.resolve(); return receipt.promise; } }, delivery);
  const observer = pasteFixture({}, delivery, "observer");
  try {
    await prior.publish(firstIncarnation);
    const paste = prior.paste("held draft");
    await submitted.promise;
    const queuedKey = prior.scope.send(Uint8Array.of(65));
    const queuedPaste = prior.paste("queued draft");
    assert.equal(delivery.mayRestore, false);
    prior.scope.retire();

    observer.scope.setInputEligible(false);
    assert(await observer.publish(nextIncarnation));
    assert(observer.browser.terminal.options.disableStdin);
    receipt.resolve({ result: "committed", receipt: receiptId });
    await Promise.all([paste, queuedKey, queuedPaste]);
    assert.deepEqual(prior.calls.map(call => call.kind), ["stage", "commit"]);
    assert.equal(delivery.mayRestore, false);
    assert.equal(observer.scope.pasteMessage, pasteIssueText.unknown);

    observer.scope.setInputEligible(true);
    await observer.publish(nextIncarnation);
    await attemptHeldInput(observer);
    assert.deepEqual(observer.calls, []);
  } finally {
    receipt.resolve({ result: "committed", receipt: receiptId });
    prior.dispose();
    observer.dispose();
  }
});
