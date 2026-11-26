import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { lstatSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { registerHooks } from "node:module";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";
import { createInterface } from "node:readline";
import test from "node:test";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import headless from "@xterm/headless";
import browser from "@xterm/xterm";
import type { Terminal as BrowserTerminal } from "@xterm/xterm";
import { act } from "react";
import ts from "typescript";
import type { Root } from "react-dom/client";
import { observeTerminalLayout, type LayoutEvent } from "./terminal-layout.ts";
import type { Invoke, Snapshot, registerRecorder } from "./recorder.ts";

const fixtureDir = new URL("./", import.meta.url);
const nativeDir = new URL("src-tauri/", fixtureDir);
const nativeSource = fileURLToPath(new URL("src/lib.rs", nativeDir));
const nativeTest = "recorder::tests::w2_recorder_tracer";
const bridgeKey = Symbol.for("w2 fixture mount");
const syntheticCase = "ukrainian-return";
const expectedHex = "d1970d";

type InvokeArgs = Parameters<Invoke>[1];
type Request = { command: string; args?: InvokeArgs };
type Registration = { calls: Request[] };
type MountOptions = {
  invoke?: (command: string, args?: InvokeArgs, options?: unknown) => unknown;
  transformEntry?: (source: string) => string;
  browserPaste?: boolean;
  suppressCompositionEnd?: boolean;
  traceRegistration?: boolean;
};
type Listener = (event: {
  type: string;
  target: Element;
  stopPropagation: () => void;
  preventDefault: () => void;
}) => void;

class Element {
  id = "";
  parentNode: Element | null = null;
  childNodes: Element[] = [];
  style = { setProperty() {}, removeProperty() {} };
  attributes = new Map<string, string>();
  private listeners = new Map<string, { capture: boolean; callback: Listener }[]>();
  private text = "";
  private classes = new Set<string>();
  classList = {
    add: (...names: string[]) => names.forEach(name => this.classes.add(name)),
    remove: (...names: string[]) => names.forEach(name => this.classes.delete(name)),
    contains: (name: string) => this.classes.has(name),
    toggle: (name: string, enabled = !this.classes.has(name)) => {
      if (enabled) this.classes.add(name);
      else this.classes.delete(name);
      return enabled;
    },
  };
  selectionStart = 0;
  selectionEnd = 0;
  focusCount = 0;
  scrollTop = 0;
  get parentElement(): Element | null { return this.parentNode?.nodeType === 1 ? this.parentNode : null; }
  get children() { return this.childNodes; }
  get childElementCount() { return this.children.length; }
  get lastChild(): Element | null { return this.childNodes.at(-1) ?? null; }
  get isConnected(): boolean { return this.nodeType === 9 || Boolean(this.parentNode?.isConnected); }
  get offsetWidth() { return this.classList.contains("xterm-char-measure-element") ? this.textContent.length * 8 : this.clientWidth; }
  get offsetHeight() { return this.classList.contains("xterm-char-measure-element") ? 16 : this.clientHeight; }
  getBoundingClientRect() {
    return { width: 8, height: 16, left: 0, top: 0, right: 8, bottom: 16 };
  }

  getContext() {
    return {
      fillRect() {}, clearRect() {},
      getImageData: () => ({ data: new Uint8ClampedArray(4) }),
      measureText: () => ({ width: 8, fontBoundingBoxAscent: 12, fontBoundingBoxDescent: 4 }),
    };
  }

  contains(element: Element | null): boolean {
    return element !== null && (element === this || this.childNodes.some(child => child.contains(element)));
  }

  setSelectionRange(start: number, end: number) {
    this.selectionStart = start;
    this.selectionEnd = end;
  }
  select() { this.setSelectionRange(0, this.value.length); }
  remove() { this.parentNode?.removeChild(this); }
  append(...children: Element[]) { children.forEach(child => this.appendChild(child)); }
  replaceChildren(...children: Element[]) {
    this.textContent = "";
    this.append(...children);
  }
  listenerCount(type: string) { return this.listeners.get(type)?.length ?? 0; }

  readonly localName: string;
  readonly ownerDocument: FixtureDocument | null;
  readonly nodeType: number;
  readonly namespaceURI: string;
  clientWidth = 360;
  clientHeight = 160;

  constructor(localName: string, ownerDocument: FixtureDocument | null, nodeType = 1, namespaceURI = "http://www.w3.org/1999/xhtml") {
    this.localName = localName;
    this.ownerDocument = ownerDocument;
    this.nodeType = nodeType;
    this.namespaceURI = namespaceURI;
  }

  private controlValue = "";
  get value() { return this.getAttribute("value") ?? this.controlValue; }
  set value(value: string) { this.controlValue = String(value); }
  selected = false;
  defaultSelected = false;

  get options() { return this.localName === "select" ? this.childNodes : undefined; }
  focus() {
    this.focusCount++;
    const document = this.ownerDocument!;
    if (document.activeElement === this) return;
    document.activeElement.blur();
    document.activeElement = this;
    this.dispatch("focus");
  }

  blur() {
    const document = this.ownerDocument!;
    if (document.activeElement !== this || this === document.body) return;
    document.activeElement = document.body;
    this.dispatch("blur");
  }

  get disabled() { return this.attributes.has("disabled"); }
  get tagName() { return this.localName.toUpperCase(); }
  get nodeName() { return this.tagName; }
  get firstChild(): Element | null { return this.childNodes[0] ?? null; }
  get textContent(): string {
    return this.text + this.childNodes.map(child => child.textContent).join("");
  }

  set textContent(text: string) {
    for (const child of this.childNodes) child.parentNode = null;
    this.childNodes.length = 0;
    this.text = text;
  }

  set nodeValue(value: string) { this.textContent = value; }

  appendChild(child: Element) { this.insertBefore(child, null); }

  insertBefore(child: Element, before: Element | null) {
    if (child.nodeType === 11) {
      for (const node of [...child.childNodes]) this.insertBefore(node, before);
      return;
    }
    child.parentNode?.removeChild(child);
    const index = before === null ? this.childNodes.length : this.childNodes.indexOf(before);
    assert(index >= 0, "insertBefore target belongs to this element");
    this.childNodes.splice(index, 0, child);
    child.parentNode = this;
  }

  removeChild(child: Element) {
    const index = this.childNodes.indexOf(child);
    assert(index >= 0, "removeChild target belongs to this element");
    this.childNodes.splice(index, 1);
    child.parentNode = null;
  }

  setAttribute(name: string, value: string) {
    this.attributes.set(name, String(value));
    if (name === "id") this.id = value;
  }

  removeAttribute(name: string) {
    this.attributes.delete(name);
    if (name === "id") this.id = "";
  }

  getAttribute(name: string) { return this.attributes.get(name) ?? null; }

  addEventListener(type: string, callback: Listener, options?: boolean | AddEventListenerOptions) {
    const capture = typeof options === "boolean" ? options : (options?.capture ?? false);
    const listeners = this.listeners.get(type) ?? [];
    listeners.push({ capture, callback });
    this.listeners.set(type, listeners);
  }

  removeEventListener(type: string, callback: Listener) {
    this.listeners.set(type, (this.listeners.get(type) ?? []).filter(entry => entry.callback !== callback));
  }

  click() {
    if (!this.disabled) this.dispatch("click");
  }

  dispatch(type: string) { this.dispatchEvent(new Event(type, { bubbles: true, cancelable: true })); }

  dispatchEvent(event: Event) {
    const path: Element[] = [];
    for (let node: Element | null = this; node; node = node.parentNode) path.push(node);
    let stopped = false;
    let immediate = false;
    Object.defineProperties(event, {
      target: { value: this },
      stopPropagation: { value: () => { stopped = true; } },
      stopImmediatePropagation: { value: () => { stopped = immediate = true; } },
    });
    for (const capture of [true, false]) {
      for (const element of capture ? path.toReversed() : event.bubbles ? path : [this]) {
        if (stopped) return !event.defaultPrevented;
        for (const listener of element.listeners.get(event.type) ?? []) {
          if (listener.capture !== capture) continue;
          if (!capture && event.type === "compositionend" && this.ownerDocument?.suppressCompositionEnd) continue;
          listener.callback(event as unknown as Parameters<Listener>[0]);
          if (immediate) return !event.defaultPrevented;
        }
      }
    }
    return !event.defaultPrevented;
  }

  find(predicate: (element: Element) => boolean): Element | undefined {
    if (predicate(this)) return this;
    for (const child of this.childNodes) {
      const found = child.find(predicate);
      if (found) return found;
    }
  }
}

class FixtureEventTarget extends EventTarget {
  private counts = new Map<string, Set<EventListenerOrEventListenerObject>>();

  override addEventListener(type: string, callback: EventListenerOrEventListenerObject | null, options?: AddEventListenerOptions | boolean) {
    if (callback) {
      const listeners = this.counts.get(type) ?? new Set();
      listeners.add(callback);
      this.counts.set(type, listeners);
    }
    super.addEventListener(type, callback, options);
  }

  override removeEventListener(type: string, callback: EventListenerOrEventListenerObject | null, options?: EventListenerOptions | boolean) {
    if (callback) this.counts.get(type)?.delete(callback);
    super.removeEventListener(type, callback, options);
  }

  listenerCount(type: string) { return this.counts.get(type)?.size ?? 0; }
}

class FixtureDocument extends Element {
  readonly documentElement = this.createElement("html");
  readonly body = this.createElement("body");
  activeElement = this.body;
  visibilityState = "visible";
  hasFocus() { return true; }
  suppressCompositionEnd = false;
  createDocumentFragment() { return new Element("#fragment", this, 11); }
  defaultView?: object;

  constructor() {
    super("#document", null, 9);
    this.appendChild(this.documentElement);
    this.documentElement.appendChild(this.body);
  }

  createElement(name: string) { return new Element(name, this); }
  createElementNS(namespace: string, name: string) { return new Element(name, this, 1, namespace); }

  createTextNode(text: string) {
    const node = new Element("#text", this, 3);
    node.textContent = text;
    return node;
  }

  getElementById(id: string) { return this.find(node => node.id === id) ?? null; }
}

type TerminalOptions = ConstructorParameters<typeof headless.Terminal>[0];

class FixtureTerminal {
  readonly backend: InstanceType<typeof headless.Terminal> | BrowserTerminal;
  readonly browserTerminal?: BrowserTerminal;
  subscriptions = 0;
  opens: Element[] = [];
  fits = 0;
  addons: object[] = [];
  pastes: string[] = [];
  focuses = 0;
  disposals = 0;
  private observer?: { dispose(): void };
  private fallbackTextarea?: Element;
  readonly data: string[] = [];
  private lastWrite = Promise.resolve();

  constructor(options: TerminalOptions, browserPaste: boolean) {
    if (browserPaste) {
      this.browserTerminal = new browser.Terminal(options);
      this.backend = this.browserTerminal;
      this.observer = this.browserTerminal.onData(text => this.data.push(text));
    } else {
      this.backend = new headless.Terminal(options);
    }
  }

  get options() { return this.backend.options; }
  get cols() { return this.backend.cols; }
  get rows() { return this.backend.rows; }
  get textarea() { return this.browserTerminal?.textarea ?? this.fallbackTextarea; }

  loadAddon(addon: { activate(terminal: BrowserTerminal): void; dispose(): void; fit(): void }) {
    this.addons.push(addon);
    const fit = addon.fit.bind(addon);
    addon.fit = () => {
      this.fits++;
      if (this.browserTerminal) fit();
    };
    this.browserTerminal?.loadAddon(addon);
  }

  open(container: Element) {
    this.opens.push(container);
    if (this.browserTerminal) this.browserTerminal.open(container as unknown as HTMLElement);
    else {
      this.fallbackTextarea = container.ownerDocument!.createElement("textarea");
      container.appendChild(this.fallbackTextarea);
    }
  }

  focus() {
    this.focuses++;
    if (this.browserTerminal) this.browserTerminal.focus();
    else this.fallbackTextarea?.focus();
  }
  scrollToBottom() { this.backend.scrollToBottom(); }

  paste(text: string) {
    assert(this.browserTerminal, "paste must use the browser Terminal");
    this.pastes.push(text);
    this.browserTerminal.paste(text);
  }

  write(data: string | Uint8Array, callback?: () => void) {
    this.lastWrite = new Promise<void>(resolve => {
      this.backend.write(data, () => {
        try {
          callback?.();
        } finally {
          resolve();
        }
      });
    });
  }

  afterWrite() { return this.lastWrite; }
  input(data: string) { this.backend.input(data, true); }

  onData(callback: (data: string) => void) {
    this.subscriptions += 1;
    return this.backend.onData(text => {
      if (!this.browserTerminal) this.data.push(text);
      callback(text);
    });
  }

  dispose() {
    this.disposals++;
    this.observer?.dispose();
    this.backend.dispose();
  }
}

let mountNumber = 0;

function installGlobal(key: PropertyKey, value: unknown) {
  const previous = Object.getOwnPropertyDescriptor(globalThis, key);
  Object.defineProperty(globalThis, key, { value, configurable: true, writable: true });
  return () => {
    if (previous) Object.defineProperty(globalThis, key, previous);
    else Reflect.deleteProperty(globalThis, key);
  };
}

function entryFromHtml() {
  const html = readFileSync(new URL("index.html", fixtureDir), "utf8");
  const root = html.match(/<div\s+id="([^"]+)"\s*>\s*<\/div>/);
  const scripts = [...html.matchAll(/<script\s+type="module"\s+src="([^"]+)"\s*>\s*<\/script>/g)];
  assert(root, "fixture HTML declares a mount root");
  assert.equal(scripts.length, 1, "fixture HTML declares a single module entry");
  const id = root[1];
  assert(id !== undefined, "fixture HTML declares a mount root id");
  const entryPath = scripts[0]?.[1];
  assert(entryPath !== undefined, "fixture HTML declares a module entry path");
  assert(entryPath.startsWith("./"), "fixture HTML loads a local module entry");
  const entry = new URL(entryPath, fixtureDir);
  assert(entry.href.startsWith(fixtureDir.href), "module entry belongs to the fixture directory");
  return { id, entry };
}

export async function mountFixture(options: MountOptions = {}) {
  const { id, entry } = entryFromHtml();
  const document = new FixtureDocument();
  document.suppressCompositionEnd = options.suppressCompositionEnd ?? false;
  const styles: string[] = [];
  const observers: ControlledObserver[] = [];
  class ControlledObserver {
    target?: Element;
    disconnected = false;
    notify: () => void;
    constructor(notify: () => void) { this.notify = notify; observers.push(this); }
    observe(target: Element) { this.target = target; }
    disconnect() { this.disconnected = true; }
  }
  let frameId = 0;
  const frames = new Map<number, FrameRequestCallback>();
  const cancelledFrames: number[] = [];
  const visualViewport = Object.assign(new FixtureEventTarget(), { width: 360, height: 300 });
  const container = document.createElement("div");
  container.id = id;
  document.body.appendChild(container);
  const ipc: Request[] = [];
  const registrations: Registration[] = [];
  const roots: Root[] = [];
  const terminals: FixtureTerminal[] = [];
  const window = Object.assign(new FixtureEventTarget(), {
    document,
    visualViewport,
    innerWidth: 360,
    innerHeight: 640,
    devicePixelRatio: 1,
    getComputedStyle: (element: Element) => ({
      width: `${element.clientWidth}px`, height: `${element.clientHeight}px`,
      paddingLeft: "0", paddingRight: "0", paddingTop: "0", paddingBottom: "0",
      getPropertyValue: (name: string) => {
        if (name === "width") return `${element.clientWidth}px`;
        if (name === "height") return `${element.clientHeight}px`;
        return "0";
      },
    }),
    matchMedia: () => ({ addListener() {}, removeListener() {} }),
    requestAnimationFrame(callback: FrameRequestCallback) {
      frames.set(++frameId, callback);
      return frameId;
    },
    cancelAnimationFrame(id: number) {
      cancelledFrames.push(id);
      frames.delete(id);
    },
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
    HTMLElement: Element,
    HTMLIFrameElement: class extends Element {},
    __TAURI_INTERNALS__: {
      invoke(command: string, args?: InvokeArgs, invokeOptions?: unknown) {
        ipc.push({ command, args: structuredClone(args) });
        assert(options.invoke, "test IPC needs a transport");
        return options.invoke(command, args, invokeOptions);
      },
    },
  });
  document.defaultView = window;
  const restore = [
    installGlobal("window", window),
    installGlobal("document", document),
    installGlobal("IS_REACT_ACT_ENVIRONMENT", true),
    installGlobal("ResizeObserver", ControlledObserver),
    installGlobal("getComputedStyle", window.getComputedStyle),
    installGlobal("requestAnimationFrame", window.requestAnimationFrame),
    installGlobal("cancelAnimationFrame", window.cancelAnimationFrame),
  ];
  let disposed = false;
  async function dispose() {
    if (disposed) return;
    disposed = true;
    try {
      await act(async () => { for (const root of roots) root.unmount(); });
    } finally {
      for (const undo of restore.reverse()) undo();
    }
  }

  try {
    const { createRoot } = await import("react-dom/client");
    restore.push(installGlobal(bridgeKey, {
      createRoot(...args: Parameters<typeof createRoot>) {
        const root = createRoot(...args);
        roots.push(root);
        return root;
      },
      Terminal: class extends FixtureTerminal {
        constructor(terminalOptions: TerminalOptions) {
          super(terminalOptions, options.browserPaste ?? false);
          terminals.push(this);
        }
      },
      register(record: typeof registerRecorder, ...[terminal, invoke, ...args]: Parameters<typeof registerRecorder>) {
        const registration: Registration = { calls: [] };
        registrations.push(registration);
        const observed: Invoke = (...request) => {
          const [command, requestArgs] = request;
          registration.calls.push({ command, args: structuredClone(requestArgs) });
          return invoke(...request);
        };
        return record(terminal, observed, ...args);
      },
    }));

    const scope = String(++mountNumber);
    entry.searchParams.set("mount", scope);
    const recorder = new URL("recorder.ts", fixtureDir).href;
    const accessBridge = 'globalThis[Symbol.for("w2 fixture mount")]';
    const adapters = new Map([
      ["@xterm/xterm", `export const Terminal = ${accessBridge}.Terminal;`],
      ["react-dom/client", `export const createRoot = ${accessBridge}.createRoot;`],
      ["./recorder.ts", `
        import { registerRecorder as actual } from ${JSON.stringify(recorder)};
        export * from ${JSON.stringify(recorder)};
        export function registerRecorder(...args) { return ${accessBridge}.register(actual, ...args); }
      `],
    ]);
    if (options.traceRegistration === false) adapters.delete("./recorder.ts");
    const virtual = new Map<string, string>();
    const hooks = registerHooks({
      resolve(specifier, context, next) {
        if (context.parentURL === entry.href) {
          const adapter = adapters.get(specifier) ?? (specifier.endsWith(".css") ? "export {};" : undefined);
          if (adapter !== undefined) {
            if (specifier.endsWith(".css")) styles.push(specifier);
            const url = new URL(`?mount=${scope}#${encodeURIComponent(specifier)}`, entry).href;
            virtual.set(url, adapter);
            return { url, shortCircuit: true };
          }
        }
        return next(specifier, context);
      },
      load(url, context, next) {
        const adapter = virtual.get(url);
        if (adapter !== undefined) return { format: "module", source: adapter, shortCircuit: true };
        if (url !== entry.href) return next(url, context);
        const source = readFileSync(entry, "utf8");
        const { outputText } = ts.transpileModule(options.transformEntry?.(source) ?? source, {
          fileName: fileURLToPath(entry),
          compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext, jsx: ts.JsxEmit.ReactJSX },
        });
        return { format: "module", source: outputText, shortCircuit: true };
      },
    });
    restore.push(() => hooks.deregister());

    await act(async () => { await import(entry.href); });
    assert.equal(roots.length, 1, "production entry creates one React root");
    assert.equal(terminals.length, 1, "production entry creates one xterm");
    const terminal = terminals[0];
    assert(terminal, "production entry creates an xterm");
    await act(async () => { await terminal.afterWrite(); });

    function button(label: string) {
      const control = container.find(node => node.localName === "button" && node.textContent === label);
      assert(control, `missing rendered ${label} button`);
      return control;
    }

    return {
      document,
      container,
      ipc,
      registrations,
      terminal,
      terminals,
      styles,
      observers,
      window,
      frames,
      cancelledFrames,
      runFrames() {
        for (const [id, callback] of [...frames]) {
          if (frames.delete(id)) callback(0);
        }
      },
      act,
      button,
      async click(label: string) { await act(async () => { button(label).click(); }); },
      async selectCase(caseId: string) {
        const select = document.getElementById("case");
        assert(select, "missing rendered case selector");
        assert(select.options?.some(option => option.value === caseId), `missing case option ${caseId}`);
        await act(async () => {
          select.value = caseId;
          select.dispatch("change");
        });
      },
      async input(text: string) { await act(async () => { terminal.input(text); }); },
      async waitFor(predicate: () => boolean, description: string) {
        const deadline = Date.now() + 5000;
        while (!predicate()) {
          assert(Date.now() < deadline, description);
          await act(async () => { await delay(5); });
        }
      },
      dispose,
    };
  } catch (error) {
    await dispose();
    throw error;
  }
}

function compileNative(source: string, executable: string) {
  const compiler = spawnSync(process.env.W2_RUSTUP ?? "rustup", [
    "run", "1.97.1", "rustc", "--edition=2024", "--test", source, "-o", executable,
  ], { encoding: "utf8", timeout: 30_000 });
  assert.ifError(compiler.error);
  assert.equal(compiler.status, 0, compiler.stderr);
}

type InputMode = "combined" | "split" | "requests";

function requestFrame(command: string, raw: InvokeArgs) {
  assert(raw && Object.getPrototypeOf(raw) === Object.prototype, "fixture IPC takes object arguments");
  const args = raw as Record<string, unknown>;
  switch (command) {
    case "w2_begin_attempt":
      assert.deepEqual(Object.keys(args), ["caseId"]);
      assert(typeof args.caseId === "string" && /^[a-z-]+$/.test(args.caseId));
      return [command, args.caseId].join("\t");
    case "w2_append_input": {
      assert.deepEqual(Object.keys(args).sort(), ["events", "generation", "sequence"]);
      assert(Number.isInteger(args.generation));
      assert(Number.isInteger(args.sequence));
      assert(Array.isArray(args.events) && args.events.every(event => Array.isArray(event)
        && event.every(byte => Number.isInteger(byte) && byte >= 0 && byte <= 255)));
      return [command, args.generation, args.sequence, args.events.map(event => event.join(",")).join(";")].join("\t");
    }
    case "w2_read_attempt":
      assert.deepEqual(Object.keys(args), ["generation"]);
      assert(Number.isInteger(args.generation));
      return [command, args.generation].join("\t");
    default:
      throw new Error(`unexpected IPC command ${command}`);
  }
}

function connectNative(executable: string, mode: InputMode) {
  const child = spawn(executable, ["--exact", nativeTest, "--nocapture"], {
    env: { ...process.env, W2_TRACER_CASE: mode },
    stdio: ["pipe", "pipe", "pipe"],
    timeout: 15_000,
  });
  const lines = createInterface({ input: child.stdout, crlfDelay: Infinity });
  const output: string[] = [];
  const pending: { resolve: (value: unknown) => void; reject: (reason: Error) => void }[] = [];
  let stderr = "";
  let failure: Error | undefined;
  let snapshot: unknown;
  child.stderr.setEncoding("utf8").on("data", chunk => { stderr += chunk; });
  child.on("error", error => { failure = error; });
  child.stdin.on("error", error => { failure = error; });
  const closed = new Promise<number | null>(resolve => child.once("close", code => {
    if (code !== 0 || pending.length) {
      failure ??= new Error(`native tracer closed with code ${code}\n${output.join("\n")}\n${stderr}`);
      for (const request of pending.splice(0)) request.reject(failure);
    }
    lines.close();
    resolve(code);
  }));

  lines.on("line", line => {
    output.push(line);
    const frame = line.indexOf("W2\t");
    if (frame < 0) return;
    const request = pending.shift();
    if (!request) {
      failure = new Error("unexpected native response");
      return;
    }
    try {
      request.resolve(JSON.parse(line.slice(frame + 3)));
    } catch (error) {
      request.reject(new Error("invalid native response", { cause: error }));
    }
  });

  return {
    async invoke(command: string, args?: InvokeArgs) {
      if (failure) throw failure;
      const frame = requestFrame(command, args);
      const response = await new Promise<unknown>((resolve, reject) => {
        pending.push({ resolve, reject });
        child.stdin.write(`${frame}\n`);
      });
      if (command === "w2_read_attempt") snapshot = response;
      return response;
    },
    snapshot: () => snapshot,
    async finish() {
      child.stdin.end();
      assert.equal(await closed, 0, `${output.join("\n")}\n${stderr}`);
      assert.ifError(failure);
      assert(output.includes(`test ${nativeTest} ... ok`), "explicit native tracer pass");
      assert(output.some(line => /^test result: ok\. 1 passed; 0 failed;/.test(line)));
    },
    async abort() {
      child.stdin.end();
      child.kill();
      await closed;
    },
  };
}

async function assertMountedInput(
  executable: string,
  mode: InputMode,
  transformEntry?: MountOptions["transformEntry"],
) {
  const native = connectNative(executable, mode);
  let fixture: Awaited<ReturnType<typeof mountFixture>> | undefined;
  try {
    const mounted = await mountFixture({
      invoke: native.invoke,
      ...(transformEntry === undefined ? {} : { transformEntry }),
    });
    fixture = mounted;
    assert.deepEqual(mounted.ipc, [], "startup transcript emits no IPC or input");
    assert.deepEqual(mounted.terminal.data, [], "startup transcript emits no xterm input");
    const readout = mounted.document.getElementById("snapshot");
    assert(readout, "missing native snapshot readout");
    assert.equal(readout.textContent, "—");

    await mounted.click("Start");
    await mounted.waitFor(() => !mounted.button("Read").disabled, "Start enables Read");
    await mounted.act(async () => {
      if (mode === "combined") mounted.terminal.input("ї\r");
      else {
        mounted.terminal.input("ї");
        mounted.terminal.input("\r");
      }
      mounted.button("Read").click();
    });
    await mounted.waitFor(() => readout.textContent !== "—", "Read renders its native snapshot");

    const events = mode === "combined" ? [[0xd1, 0x97, 0x0d]] : [[0xd1, 0x97], [0x0d]];
    assert.equal(mounted.registrations.length, 1, "production recorder registration");
    const registration = mounted.registrations[0];
    assert(registration, "production recorder registration exists");
    assert.equal(mounted.terminal.subscriptions, 1, "one xterm onData subscription");
    assert.deepEqual(mounted.terminal.data, mode === "combined" ? ["ї\r"] : ["ї", "\r"], "actual xterm input events");
    assert.deepEqual(mounted.ipc, [
      { command: "w2_begin_attempt", args: { caseId: syntheticCase } },
      ...events.map((event, index) => ({
        command: "w2_append_input",
        args: { generation: 1, sequence: index + 1, events: [event] },
      })),
      { command: "w2_read_attempt", args: { generation: 1 } },
    ], "actual IPC trace");
    assert.deepEqual(registration.calls, mounted.ipc, "registered invoke trace matches actual IPC");
    assert.deepEqual(native.snapshot(), {
      caseId: syntheticCase,
      generation: 1,
      nextSequence: events.length + 1,
      bytes: [0xd1, 0x97, 0x0d],
      eventLengths: events.map(event => event.length),
      rejected: 0,
      invalid: false,
    });
    assert.equal(readout.textContent, expectedHex, "rendered native snapshot");
    await native.finish();
  } catch (error) {
    await native.abort();
    throw error;
  } finally {
    await fixture?.dispose();
  }
}

function replaceOnce(source: string, original: string | RegExp, replacement: string) {
  const found = typeof original === "string" ? source.includes(original) : source.search(original) >= 0;
  assert(found, `mutation target must exist: ${original}`);
  return source.replace(original, () => replacement);
}

const registration = "const recorder = registerRecorder(input, transport, onCaptureError);";

function skipRegistration(source: string) {
  return replaceOnce(source, registration, `
    const recorder = {
      start: async () => 1,
      read: async () => ({ bytes: [], eventLengths: [] }),
      dispose() {},
    };
  `);
}

function bypassRegistrationTransport(source: string) {
  return replaceOnce(source, registration, `
    const unused = registerRecorder({ onData: () => ({ dispose() {} }) }, invoke, onCaptureError);
    let generation = 0;
    let sequence = 0;
    let pending = Promise.resolve();
    const subscription = terminal.onData(text => {
      const events = [Array.from(new TextEncoder().encode(text))];
      pending = pending.then(() => invoke("w2_append_input", {
        generation, sequence: ++sequence, events,
      }));
    });
    const recorder = {
      async start() {
        generation = await invoke("w2_begin_attempt", { caseId: "ukrainian-return" });
        return generation;
      },
      async read() {
        await pending;
        return invoke("w2_read_attempt", { generation });
      },
      dispose() { subscription.dispose(); unused.dispose(); },
    };
  `);
}

async function mountedTracer() {
  const scratch = mkdtempSync(join(tmpdir(), "w2-mounted-"));
  try {
    const executable = join(scratch, "recorder");
    compileNative(nativeSource, executable);
    await assert.rejects(assertMountedInput(executable, "combined", skipRegistration), /production recorder registration|rendered native snapshot/);
    await assert.rejects(assertMountedInput(executable, "combined", bypassRegistrationTransport), /registered invoke trace matches actual IPC/);
    await assertMountedInput(executable, "combined");
    await assertMountedInput(executable, "split");
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
}

function disconnectedWrapperControls() {
  const scratch = mkdtempSync(join(tmpdir(), "w2-wrapper-controls-"));
  try {
    const source = readFileSync(nativeSource, "utf8");
    const controls: [string, string, string][] = [
      ["w2_begin_attempt", "let _ = (state, case_id); Ok(0)", "begin wrapper must start generation 1"],
      ["w2_append_input", "let _ = (state, generation, sequence, events); Ok(())", "append wrapper must commit the event"],
      ["w2_read_attempt", 'let _ = (state, generation); Err("disconnected read control")', "read wrapper must return the active snapshot"],
    ];
    const env = { ...process.env };
    delete env.W2_TRACER_CASE;
    const clean = join(scratch, "clean");
    compileNative(nativeSource, clean);
    for (const [command, replacement, assertion] of controls) {
      const signature = new RegExp(`(^    fn ${command}\\([^]*? \\{\\n)[^]*?^    \\}`, "m");
      const original = source.match(signature);
      assert(original, `wrapper to disconnect: ${command}`);
      const prefix = original[1];
      assert(prefix !== undefined, `wrapper to disconnect: ${command}`);
      const mutated = replaceOnce(source, signature, `${prefix}        ${replacement}\n    }`);
      const path = join(scratch, `${command}.rs`);
      const executable = join(scratch, command);
      try {
        writeFileSync(path, mutated);
        compileNative(path, executable);
        const result = spawnSync(executable, ["--exact", nativeTest], { env, encoding: "utf8", timeout: 5000 });
        assert.ifError(result.error);
        assert.equal(result.status, 101, result.stdout + result.stderr);
        assert(result.stdout.includes(`test ${nativeTest} ... FAILED`));
        assert(result.stdout.includes(assertion), `operation assertion for ${command}\n${result.stdout}`);
      } finally {
        rmSync(path, { force: true });
        rmSync(executable, { force: true });
      }
      const result = spawnSync(clean, ["--exact", nativeTest], { env, encoding: "utf8", timeout: 5000 });
      assert.ifError(result.error);
      assert.equal(result.status, 0, result.stdout + result.stderr);
      assert(result.stdout.includes(`test ${nativeTest} ... ok`));
    }
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
}

const requiredVectors = {
  "english-return": { text: "ok\r", beforeHex: "", afterHex: "6f6b0d" },
  "ukrainian-return": { text: "ї\r", beforeHex: "", afterHex: "d1970d" },
  "composition-commit": { text: "é", beforeHex: "", afterHex: "c3a9" },
  "composition-replacement": { text: "è", beforeHex: "", afterHex: "c3a8" },
  "composition-cancel": { text: "", beforeHex: "", afterHex: "" },
  "paste-short": { text: "Їж", beforeHex: "", afterHex: "d087d0b6", paste: true },
  "paste-multiline": { text: "ї\né\r\n界", beforeHex: "", afterHex: "d1970dc3a90de7958c", paste: true },
  "touch-selection-copy": { text: "", beforeHex: "", afterHex: "" },
  "touch-scroll": { text: "", beforeHex: "", afterHex: "" },
  "keyboard-open-cursor": { text: "", beforeHex: "", afterHex: "" },
  rotation: { text: "", beforeHex: "", afterHex: "" },
  "app-switch": { text: "a", beforeHex: "61", afterHex: "62" },
  "lock-foreground": { text: "c", beforeHex: "63", afterHex: "64" },
};

type CaseId = keyof typeof requiredVectors;
const requiredIds = Object.keys(requiredVectors) as CaseId[];
const pasteIds = ["paste-short", "paste-multiline"] as const;

type CaseManifest = typeof import("./cases.json");
function readManifest(): CaseManifest {
  return JSON.parse(readFileSync(new URL("cases.json", fixtureDir), "utf8"));
}

async function withNativeExecutable(run: (executable: string) => void | Promise<void>) {
  const scratch = mkdtempSync(join(tmpdir(), "w2-vectors-"));
  try {
    const executable = join(scratch, "recorder");
    compileNative(nativeSource, executable);
    await run(executable);
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
}

function assertManifestVectors() {
  const manifest = readManifest();
  assert.equal(manifest.version, 1);
  assert.deepEqual(manifest.cases.map(entry => entry.id).sort(), requiredIds.toSorted(), "exact unique mandatory IDs");
  for (const entry of manifest.cases) {
    const expected = requiredVectors[entry.id as CaseId];
    assert(entry.title.trim().length > 0);
    for (const steps of [entry.actions, entry.observations]) {
      assert(Array.isArray(steps) && steps.length >= 1 && steps.length <= 5, "finite case steps");
      for (const step of steps) assert(step.trim().length > 0 && step.length <= 240, "bounded nonempty case step");
    }
    assert.deepEqual(entry.terminalMode, { bracketedPaste: false });
    assert.deepEqual(entry.expected, { beforeHex: expected.beforeHex, afterHex: expected.afterHex });
    if (entry.id.startsWith("composition-")) {
      assert.equal(entry.expected.beforeHex, "", "zero preedit bytes");
      assert(entry.observations.some(step => /preedit.*zero recorded bytes/i.test(step)));
    }
    assert.equal(entry.pasteText, "paste" in expected ? expected.text : undefined);
  }
  return withNativeExecutable(executable => {
    const vectors = manifest.cases.map(({ id, expected }) => [id, expected.beforeHex, expected.afterHex].join("\t"));
    const name = "recorder::tests::w2_recorder_manifest_vectors";
    const result = spawnSync(executable, ["--exact", name], {
      env: { ...process.env, W2_MANIFEST_VECTORS: vectors.join("\n") },
      encoding: "utf8",
      timeout: 5_000,
    });
    assert.ifError(result.error);
    assert.equal(result.status, 0, result.stdout + result.stderr);
    assert(result.stdout.includes(`test ${name} ... ok`));
    assert.match(result.stdout, /^test result: ok\. 1 passed; 0 failed;/m);
  });
}

type Mounted = Awaited<ReturnType<typeof mountFixture>>;

function field(fixture: Mounted, id: string) {
  const element = fixture.document.getElementById(id);
  assert(element, `missing rendered field ${id}`);
  return element;
}

function controlledClock() {
  const realSet = globalThis.setTimeout;
  const realClear = globalThis.clearTimeout;
  let now = 0;
  let order = 0;
  const work = new Map<object, { run: () => void; due: number; order: number }>();
  const restoreSet = installGlobal("setTimeout", (callback: () => void, ms = 0) => {
    if (ms !== 0 && ms !== 250) return realSet(callback, ms);
    const handle = {};
    work.set(handle, { run: callback, due: now + ms, order: ++order });
    return handle;
  });
  const restoreClear = installGlobal("clearTimeout", (handle: ReturnType<typeof setTimeout>) => {
    if (!work.delete(handle)) realClear(handle);
  });
  return {
    advance(ms: number) {
      const target = now + ms;
      for (;;) {
        const next = [...work].filter(([, task]) => task.due <= target)
          .sort(([, a], [, b]) => a.due - b.due || a.order - b.order)[0];
        if (!next) break;
        work.delete(next[0]);
        now = next[1].due;
        next[1].run();
      }
      now = target;
    },
    dispose() { restoreClear(); restoreSet(); },
  };
}

class FixtureInputEvent extends Event {
  inputType: string;
  data: string;
  isComposing: boolean;
  constructor(inputType: string, data: string, isComposing: boolean) {
    super("input", { bubbles: true, composed: true, cancelable: true });
    this.inputType = inputType;
    this.data = data;
    this.isComposing = isComposing;
  }
}

async function compositionDriver(fixture: Mounted) {
  await fixture.click("Focus");
  const textarea = fixture.terminal.textarea as unknown as Element;
  assert.equal(fixture.document.activeElement, textarea);
  const clock = controlledClock();
  function change(value: string, type: string, isComposing: boolean) {
    textarea.value = value;
    textarea.setSelectionRange(value.length, value.length);
    textarea.dispatchEvent(new FixtureInputEvent(type, value, isComposing));
  }
  function composition(type: string, data: string) {
    textarea.dispatchEvent(Object.assign(new Event(type, { bubbles: true, composed: true }), { data }));
  }
  return {
    textarea,
    clock,
    start() {
      textarea.value = "";
      textarea.setSelectionRange(0, 0);
      composition("compositionstart", "");
    },
    update(text: string) {
      composition("compositionupdate", text);
      change(text, "insertCompositionText", true);
    },
    premature(text: string) {
      textarea.dispatchEvent(new FixtureInputEvent("insertText", text, true));
    },
    end(value: string) {
      composition("compositionend", "end event data is not the committed value");
      change(value, value ? "insertFromComposition" : "deleteCompositionText", false);
    },
  };
}

function trace(fixture: Mounted) {
  return JSON.parse(field(fixture, "composition-trace").textContent) as {
    caseId: string;
    attemptId: number;
    generation: number;
    events: { event: string; sequence: number; bytes?: number; nativeSequence?: number }[];
  };
}

function appends(fixture: Mounted) {
  return fixture.ipc.filter(call => call.command === "w2_append_input");
}

function reads(fixture: Mounted) {
  return fixture.ipc.filter(call => call.command === "w2_read_attempt");
}

async function startCase(fixture: Mounted, id: CaseId) {
  const focuses = fixture.terminal.focuses;
  await fixture.selectCase(id);
  await fixture.click("Start");
  await fixture.waitFor(() => Boolean(fixture.container.find(node => node.localName === "button" && node.textContent === "Restart" && !node.disabled)), "Start completes");
  assert.equal(fixture.terminal.focuses, focuses, "starting a case does not request focus");
}

async function finalRead(fixture: Mounted) {
  await fixture.click("Read");
  await fixture.waitFor(() => field(fixture, "snapshot").textContent !== "—", "Read completes");
}

function assertTrace(fixture: Mounted, caseId: CaseId, generation: number, events: string[], attemptId = 1) {
  const actual = trace(fixture);
  assert.deepEqual([actual.caseId, actual.attemptId, actual.generation], [caseId, attemptId, generation]);
  assert.deepEqual(actual.events.map(entry => entry.event), events, "composition trace phases");
  assert.deepEqual(actual.events.map(entry => entry.sequence), events.map((_, i) => i + 1));
  return actual.events;
}

async function compositionVector(
  fixture: Mounted,
  native: ReturnType<typeof connectNative>,
  id: CaseId,
  generation: number,
  options: { hostile?: boolean; keepPreedit?: boolean; overlap?: () => void; repeat?: boolean; attemptId?: number; earlyRead?: () => void } = {},
) {
  const vector = requiredVectors[id];
  const driver = await compositionDriver(fixture);
  const initialFocuses = driver.textarea.focusCount;
  const initialReads = reads(fixture).length;
  const initialAppends = appends(fixture).length;
  const initialData = fixture.terminal.data.length;
  try {
    await act(async () => {
      driver.start();
      driver.clock.advance(250);
    });
    assert.equal(reads(fixture).length, initialReads, "no read before preedit");
    await act(async () => {
      driver.update("draft");
      if (id === "composition-replacement") driver.update("replaced draft");
      driver.clock.advance(250);
    });
    if (options.overlap) {
      await act(async () => { driver.clock.advance(250); });
      assert.equal(reads(fixture).length, initialReads + 1, "checkpoint read must not overlap");
      options.overlap();
    }
    await fixture.waitFor(() => field(fixture, "checkpoint").textContent.includes("active and focused"), "focused checkpoint");
    if (options.repeat) {
      const first = field(fixture, "checkpoint").textContent;
      await act(async () => { driver.clock.advance(250); });
      await fixture.waitFor(() => field(fixture, "checkpoint").textContent !== first, "a later tick refreshes the checkpoint");
    }
    const checkpoint = field(fixture, "checkpoint").textContent;
    assert.match(checkpoint, new RegExp(`generation ${generation} .*active and focused .*· [(]empty[)]$`));
    assert.equal(fixture.document.activeElement, driver.textarea);
    assert.equal(driver.textarea.focusCount, initialFocuses, "checkpoint does not request focus");
    assert.equal(fixture.terminal.textarea, driver.textarea);
    assert.deepEqual(native.snapshot(), {
      caseId: id, generation, nextSequence: 1, bytes: [], eventLengths: [], rejected: 0, invalid: false,
    });
    assert.equal(fixture.terminal.data.length, initialData);
    assert.equal(appends(fixture).length, initialAppends);

    await act(async () => {
      if (options.hostile) {
        driver.premature("unexpected");
        assert.deepEqual(fixture.terminal.data.slice(initialData), ["unexpected"]);
        assert.equal(appends(fixture).length, initialAppends);
      }
      driver.end(options.keepPreedit ? driver.textarea.value : vector.text);
      assert.equal(fixture.terminal.data.length, initialData + Number(Boolean(options.hostile)));
      assert.equal(appends(fixture).length, initialAppends);
    });
    const checkpointReadCount = reads(fixture).length;
    if (options.earlyRead) {
      assert.equal(fixture.button("Read").disabled, false);
      await fixture.click("Read");
      assert.equal(reads(fixture).length, checkpointReadCount, "final Read waits for xterm");
      assert.equal(field(fixture, "snapshot").textContent, "—");
    }
    await act(async () => { driver.clock.advance(0); });

    assert.deepEqual(fixture.terminal.data.slice(initialData),
      [...(options.hostile ? ["unexpected"] : []), ...(vector.text ? [vector.text] : [])], "public composition emissions");
    if (options.earlyRead) {
      if (vector.text) {
        assert.equal(appends(fixture).length, initialAppends + 1, "guarded composition appends before final Read");
        assert.equal(reads(fixture).length, checkpointReadCount, "final Read waits for append acknowledgment");
        assert.equal(field(fixture, "snapshot").textContent, "—");
      }
      await act(async () => { options.earlyRead!(); });
      await fixture.waitFor(() => field(fixture, "snapshot").textContent !== "—", "early Read completes");
    } else await finalRead(fixture);
    const accepted = vector.text && !options.hostile;
    const bytes = accepted ? [...Buffer.from(vector.afterHex, "hex")] : [];
    assert.deepEqual(appends(fixture).slice(initialAppends), accepted ? [
      { command: "w2_append_input", args: { generation, sequence: 1, events: [bytes] } },
    ] : [], "guarded composition appends");
    assert.deepEqual(reads(fixture).slice(initialReads), Array.from({ length: options.repeat ? 3 : 2 }, () => ({
      command: "w2_read_attempt", args: { generation },
    })));
    assert.deepEqual(native.snapshot(), {
      caseId: id, generation, nextSequence: accepted ? 2 : 1, bytes,
      eventLengths: accepted ? [bytes.length] : [], rejected: 0, invalid: false,
    });
    assert.equal(field(fixture, "snapshot").textContent, accepted ? vector.afterHex : "(empty)");
    assert.equal(field(fixture, "checkpoint").textContent, checkpoint);
    const held = fixture.container.find(node => node.getAttribute("role") === "alert");
    assert.equal(Boolean(held?.textContent.startsWith("Held:")), Boolean(options.hostile), "unexpected input holds the attempt");
    const events = assertTrace(fixture, id, generation, [
      "start", "update", ...(id === "composition-replacement" ? ["update"] : []), "active checkpoint",
      ...(options.repeat ? ["active checkpoint"] : []), ...(options.hostile ? ["rejection"] : []), "end",
      ...(options.hostile ? ["rejection"] : accepted ? ["admission", "append dispatch", "append acknowledgment"] : []),
      "final snapshot",
    ], options.attemptId);
    if (accepted) {
      for (const event of events.filter(entry => entry.event.startsWith("append"))) {
        assert.equal(event.bytes, bytes.length);
        assert.equal(event.nativeSequence, 1);
      }
    }
  } finally {
    driver.clock.dispose();
  }
}

async function exerciseCases(
  executable: string,
  ids: readonly CaseId[],
  transformEntry?: MountOptions["transformEntry"],
  controls: { suppressCompositionEnd?: boolean; keepPreedit?: boolean } = {},
) {
  const native = connectNative(executable, "requests");
  const manifest = readManifest();
  try {
    for (const [index, id] of ids.entries()) {
      const entry = manifest.cases.find(candidate => candidate.id === id);
      assert(entry);
      const vector = requiredVectors[id];
      const usingPaste = "paste" in vector;
      const generation = index + 1;
      const fixture = await mountFixture({
        invoke: native.invoke,
        ...(transformEntry === undefined ? {} : { transformEntry }),
        browserPaste: usingPaste || id.startsWith("composition-"),
        ...(controls.suppressCompositionEnd === undefined ? {} : { suppressCompositionEnd: controls.suppressCompositionEnd }),
        traceRegistration: false,
      });
      try {
        assert.equal(fixture.ipc.length, 0, "transcript generates no IPC");
        const field = (id: string) => {
          const element = fixture.document.getElementById(id);
          assert(element, `missing case field ${id}`);
          return element;
        };

        assert.deepEqual(field("case").options?.map(option => option.value).sort(),
          requiredIds.toSorted(), "exact rendered case options");
        await fixture.selectCase(id);
        assert.deepEqual(field("case-actions").childNodes.map(node => node.textContent), entry.actions);
        assert.deepEqual(field("case-observations").childNodes.map(node => node.textContent), entry.observations);
        assert.equal(field("expected-before").textContent, vector.beforeHex || "(empty)");
        assert.equal(field("expected-after").textContent, vector.afterHex || "(empty)");
        assert.equal(field("snapshot").textContent, "—");
        if (usingPaste) assert.equal(fixture.terminal.browserTerminal?.modes.bracketedPasteMode, false);

        await fixture.click("Start");
        assert.deepEqual(fixture.ipc, [{ command: "w2_begin_attempt", args: { caseId: id } }], "one selected begin");
        if (id.startsWith("composition-")) {
          await fixture.waitFor(() => Boolean(fixture.container.find(node => node.localName === "button" && node.textContent === "Restart" && !node.disabled)), "Start completes");
          await compositionVector(fixture, native, id, generation,
            controls.keepPreedit === undefined ? {} : { keepPreedit: controls.keepPreedit });
          continue;
        }
        await fixture.waitFor(() => !fixture.button("Read").disabled, "Start enables Read");

        const snapshot = (bytes: number[]): Snapshot => ({
          caseId: id, generation, nextSequence: bytes.length ? 2 : 1, bytes,
          eventLengths: bytes.length ? [bytes.length] : [], rejected: 0, invalid: false,
        });
        async function checkRead(hex: string, bytes: number[]) {
          const firstCall = fixture.ipc.length;
          await fixture.click("Read");
          await fixture.waitFor(() => !fixture.button("Read").disabled, "Read completes");
          assert.deepEqual(fixture.ipc.slice(firstCall).filter(call => call.command === "w2_read_attempt"),
            [{ command: "w2_read_attempt", args: { generation } }], "read the fresh generation");
          assert.deepEqual(native.snapshot(), snapshot(bytes), "native recorder snapshot");
          assert.equal(field("snapshot").textContent, hex || "(empty)", "rendered native snapshot");
        }
        await checkRead("", []);

        const hex = vector.beforeHex || vector.afterHex;
        const bytes = [...Buffer.from(hex, "hex")];
        if (usingPaste) {
          await fixture.click("Paste sample");
          assert.deepEqual(fixture.terminal.pastes, [vector.text], "paste control delivers the raw vector");
        } else if (vector.text) {
          await fixture.input(vector.text);
        }
        await checkRead(hex, bytes);
        const appends = fixture.ipc.filter(call => call.command === "w2_append_input");
        assert.deepEqual(appends, bytes.length ? [{
          command: "w2_append_input", args: { generation, sequence: 1, events: [bytes] },
        }] : [], "production append bytes");
        if (usingPaste) {
          assert.equal(appends.length, 1, "paste emits one append");
          const recorded = (native.snapshot() as Snapshot).bytes;
          assert.equal(recorded.filter(byte => byte === 0x0d).length, id === "paste-multiline" ? 2 : 0);
          assert(!recorded.includes(0x0a) && !recorded.includes(0x1b), "paste has no LF or bracket markers");
        }
      } finally {
        await fixture.dispose();
      }
    }
    await native.finish();
  } catch (error) {
    await native.abort();
    throw error;
  }
}

function pasteVectorsMatch() {
  return withNativeExecutable(async executable => {
    await assert.rejects(exerciseCases(executable, pasteIds, source => replaceOnce(source, registration,
      'const recorder = registerRecorder({ onData: () => ({ dispose() {} }) }, invoke, onCaptureError);')),
    /native recorder snapshot|production append bytes/);
    await exerciseCases(executable, pasteIds);
  });
}

function productionCaseControls() {
  return withNativeExecutable(async executable => {
    const optionList = "manifest.cases.map(entry => <option";
    await assert.rejects(exerciseCases(executable, requiredIds, source => replaceOnce(source, optionList,
      'manifest.cases.filter(entry => entry.id !== "english-return").map(entry => <option')), /exact rendered case options/);
    await assert.rejects(exerciseCases(executable, requiredIds, source => replaceOnce(source, "onClick={start}",
      "onClick={() => {}}")), /one selected begin/);
    await assert.rejects(exerciseCases(executable, requiredIds, source => replaceOnce(source,
      "selected.expected.afterHex || emptyHex", "manifest.cases[1].expected.afterHex || emptyHex")), /6f6b0d/);
    await assert.rejects(exerciseCases(executable, ["composition-commit"], undefined,
      { suppressCompositionEnd: true }), /public composition emissions/);
    await assert.rejects(exerciseCases(executable, ["composition-cancel"], undefined,
      { keepPreedit: true }), /public composition emissions/);
    await exerciseCases(executable, requiredIds);
  });
}

async function viewportUpdates() {
  const document = new FixtureDocument();
  const host = document.createElement("div");
  const textarea = document.createElement("textarea");
  host.appendChild(textarea);
  document.body.appendChild(host);
  const visualViewport = Object.assign(new EventTarget(), { width: 360, height: 200 });
  const frames = new Map<number, FrameRequestCallback>();
  let nextFrame = 0;
  document.defaultView = Object.assign(new EventTarget(), {
    visualViewport,
    requestAnimationFrame(callback: FrameRequestCallback) {
      frames.set(++nextFrame, callback);
      return nextFrame;
    },
    cancelAnimationFrame(id: number) { frames.delete(id); },
  });
  let notify = () => {};
  let disconnected = false;
  const restore = installGlobal("ResizeObserver", class {
    constructor(update: () => void) { notify = update; }
    observe(target: Element) { assert.equal(target, host); }
    disconnect() { disconnected = true; }
  });
  const fitted: object[] = [];
  const terminal = { fit() { fitted.push(this); } };
  let diagnostics: readonly LayoutEvent[] = [];
  const stop = observeTerminalLayout(host as unknown as HTMLElement, textarea as unknown as HTMLTextAreaElement,
    terminal, events => { diagnostics = events; });
  function flush() {
    assert.equal(frames.size, 1);
    for (const [id, callback] of [...frames]) {
      frames.delete(id);
      callback(0);
    }
  }
  try {
    host.clientWidth = 0;
    notify();
    visualViewport.dispatchEvent(new Event("resize"));
    flush();
    assert.deepEqual(fitted, []);
    host.clientWidth = 360;
    for (let i = 0; i < 30; i++) {
      notify();
      visualViewport.dispatchEvent(Object.assign(new Event("scroll"), { data: "secret payload" }));
    }
    flush();
    assert.deepEqual(fitted, [terminal]);
    assert.equal(textarea.focusCount, 0);
    assert.equal(diagnostics.length, 24);
    assert(diagnostics.every(event => !event.focused && event.visible && event.height === 200));
    assert(!JSON.stringify(diagnostics).includes("secret payload"));
    notify();
    assert.equal(frames.size, 1);
  } finally {
    stop();
    restore();
  }
  assert(disconnected);
  const last = diagnostics;
  notify();
  visualViewport.dispatchEvent(new Event("resize"));
  (document.defaultView as EventTarget).dispatchEvent(new Event("orientationchange"));
  assert.equal(diagnostics, last);
  assert.equal(frames.size, 0);
}

async function checkMountedLayout(transformEntry?: MountOptions["transformEntry"]) {
  const fixture = await mountFixture({
    ...(transformEntry === undefined ? {} : { transformEntry }),
    browserPaste: true,
  });
  const { terminal, window, document } = fixture;
  try {
    const host = field(fixture, "terminal");
    assert(fixture.styles.includes("./fixture.css"), "fixture loads its layout stylesheet");
    assert.match(readFileSync(new URL("fixture.css", fixtureDir), "utf8"), /[.]terminal-host\s*\{/);
    assert.equal(host.getAttribute("class"), "terminal-host");
    assert(terminal.opens.length === 1 && terminal.opens[0] === host, "terminal opened in the rendered host");
    assert.equal(terminal.browserTerminal?.element?.parentElement, host);
    assert.equal(terminal.addons.length, 1, "terminal has a fit addon");
    const observer = fixture.observers.find(entry => entry.target === host);
    assert(observer, "layout observes the rendered host");
    const notify = observer.notify;
    const rows = () => {
      const buffer = terminal.backend.buffer.active;
      return Array.from({ length: buffer.length }, (_, index) => buffer.getLine(index)?.translateToString(true))
        .filter(Boolean).join("\n");
    };
    const transcript = rows();
    assert.match(transcript, /COPY-Ї/);
    const textarea = terminal.textarea as unknown as Element;
    async function resize() {
      await act(async () => {
        notify();
        window.visualViewport.dispatchEvent(new Event("resize"));
        window.dispatchEvent(new Event("resize"));
        window.dispatchEvent(new Event("orientationchange"));
        fixture.runFrames();
      });
    }
    host.clientWidth = 0;
    await resize();
    assert.equal(terminal.fits, 0);
    host.clientWidth = 360;
    await resize();
    assert.equal(terminal.fits, 1);
    assert(terminal.cols > 36, "fit addon resized the attached terminal");
    await fixture.selectCase("rotation");
    window.visualViewport.height = 240;
    await resize();
    assert.equal(terminal.fits, 2);
    assert.match(field(fixture, "layout-events").textContent, /orientation change/);
    assert.equal(field(fixture, "terminal"), host);
    assert.equal(fixture.terminals.length, 1, "layout and case updates keep one mounted terminal");
    assert.equal(terminal.opens.length, 1);
    assert.equal(terminal.subscriptions, 1);
    assert.equal(terminal.disposals, 0);
    assert.equal(terminal.focuses, 0);
    assert.equal(rows(), transcript);
    await fixture.click("Focus");
    assert.equal(terminal.focuses, 1);
    assert.equal(document.activeElement, textarea);
    assert.equal(textarea.focusCount, 1);
    await resize();
    assert.equal(terminal.fits, 3);
    assert.equal(document.activeElement, textarea);
    assert.equal(textarea.focusCount, 1);

    fixture.runFrames();
    await act(async () => { observer.notify(); });
    const pending = [...fixture.frames.keys()];
    assert(pending.length > 0);
    await fixture.dispose();
    assert(observer.disconnected);
    assert(pending.every(id => fixture.cancelledFrames.includes(id)));
    assert.equal(window.listenerCount("resize"), 0);
    assert.equal(window.listenerCount("orientationchange"), 0);
    assert.equal(window.visualViewport.listenerCount("resize"), 0);
    assert.equal(window.visualViewport.listenerCount("scroll"), 0);
    const pendingAfterDispose = fixture.frames.size;
    observer.notify();
    window.visualViewport.dispatchEvent(new Event("resize"));
    window.dispatchEvent(new Event("resize"));
    window.dispatchEvent(new Event("orientationchange"));
    assert.equal(fixture.frames.size, pendingAfterDispose);
    assert.equal(terminal.disposals, 1);
    assert.equal(textarea.listenerCount("compositionstart"), 0);
    assert.equal(textarea.listenerCount("blur"), 0);

  } finally {
    await fixture.dispose();
  }
}

async function mountedLayout() {
  await assert.rejects(checkMountedLayout(source => replaceOnce(source,
    "observeTerminalLayout(host, textarea, fit, publishLayout)", "() => {}")), /layout observes the rendered host/);
  await assert.rejects(checkMountedLayout(source => replaceOnce(source,
    "const host = viewport.current;", "const host = document.getElementById('root') as HTMLDivElement;")),
  /terminal opened in the rendered host/);
  for (const dependency of ["caseId", "layout.length > 1"]) {
    await assert.rejects(checkMountedLayout(source => replaceOnce(source, "}, []);", `}, [${dependency}]);`)),
      /mounted terminal|one xterm|terminal opened|transcript|found|fits|Expected values|fit addon resized/);
  }
  await checkMountedLayout();
}

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>(done => { resolve = done; });
  return { promise, resolve };
}

async function checkCheckpoint(executable: string, transformEntry?: MountOptions["transformEntry"]) {
  const native = connectNative(executable, "requests");
  const gate = deferred();
  let delayRead = true;
  const appendGate = deferred();
  const fixture = await mountFixture({
    browserPaste: true,
    ...(transformEntry === undefined ? {} : { transformEntry }),
    async invoke(command, args) {
      const result = await native.invoke(command, args);
      if (delayRead && command === "w2_read_attempt") await gate.promise;
      if (command === "w2_append_input") await appendGate.promise;
      return result;
    },
  });
  try {
    await startCase(fixture, "composition-commit");
    await compositionVector(fixture, native, "composition-commit", 1, {
      overlap() { delayRead = false; gate.resolve(); },
      earlyRead: appendGate.resolve,
    });
    await startCase(fixture, "composition-cancel");
    await compositionVector(fixture, native, "composition-cancel", 2, { attemptId: 2, repeat: true, earlyRead() {} });
    await startCase(fixture, "composition-commit");
    await compositionVector(fixture, native, "composition-commit", 3, { hostile: true, attemptId: 3 });
    await startCase(fixture, "composition-commit");
    await compositionVector(fixture, native, "composition-commit", 4, { attemptId: 4 });
    await native.finish();
  } catch (error) {
    await native.abort();
    throw error;
  } finally {
    gate.resolve();
    appendGate.resolve();
    await fixture.dispose();
  }
}

async function checkLateRead(executable: string, stop: "end" | "blur" | "hide" | "case" | "restart" | "unmount") {
  const native = connectNative(executable, "requests");
  const gate = deferred();
  const returned = deferred();
  let readsStarted = 0;
  const fixture = await mountFixture({
    browserPaste: true,
    async invoke(command, args) {
      const delayCheckpoint = command === "w2_read_attempt" && ++readsStarted === 1;
      const result = await native.invoke(command, args);
      if (delayCheckpoint) {
        await gate.promise;
        returned.resolve();
      }
      return result;
    },
  });
  await startCase(fixture, "composition-commit");
  const driver = await compositionDriver(fixture);
  try {
    await act(async () => {
      driver.start();
      driver.update("draft");
      driver.clock.advance(250);
    });
    assert.equal(reads(fixture).length, 1);
    if (stop === "unmount") await fixture.dispose();
    else if (stop === "case") await fixture.selectCase("rotation");
    else if (stop === "restart") {
      await fixture.click("Restart");
      await fixture.waitFor(() => fixture.ipc.filter(call => call.command === "w2_begin_attempt").length === 2, "fresh attempt starts");
    } else await act(async () => {
      if (stop === "end") driver.end("é");
      if (stop === "blur") driver.textarea.blur();
      if (stop === "hide") {
        fixture.document.visibilityState = "hidden";
        fixture.document.dispatch("visibilitychange");
      }
    });
    const checkpoint = fixture.document.getElementById("checkpoint");
    const prior = checkpoint?.textContent;
    const priorTrace = fixture.document.getElementById("composition-trace")?.textContent;
    await act(async () => {
      gate.resolve();
      await returned.promise;
    });
    assert.equal(checkpoint?.textContent, prior, "stale checkpoint cannot publish");
    assert.equal(fixture.document.getElementById("composition-trace")?.textContent, priorTrace, "stale read cannot change the trace");
    await act(async () => { driver.clock.advance(1000); });
    assert.equal(reads(fixture).length, stop === "hide" ? 2 : 1, "stale read cannot schedule another poll");
    assert.equal(appends(fixture).length, 0);
    if (stop === "hide") {
      await fixture.waitFor(() => field(fixture, "retained-snapshot").textContent === "(empty)", "retired native snapshot is visible");
      await act(async () => {
        driver.end("é");
        driver.clock.advance(10);
        fixture.document.visibilityState = "visible";
        fixture.document.dispatch("visibilitychange");
      });
      await fixture.click("Start fresh and refocus");
      await fixture.waitFor(() => Boolean(fixture.container.find(node =>
        node.localName === "button" && node.textContent === "Restart" && !node.disabled)), "recovered composition attempt starts");
      await act(async () => {
        driver.update("stale preedit");
        driver.end("stale commit");
        driver.clock.advance(250);
      });
      assert.equal(field(fixture, "checkpoint").textContent, "Pending", "fresh attempt cannot reuse the retired checkpoint");
      assert.equal(fixture.document.getElementById("composition-trace"), null, "orphaned composition events cannot enter the fresh trace");
      assert.equal(appends(fixture).length, 0, "retired composition cannot append to the fresh generation");
      await fixture.waitFor(() => fixture.ipc.filter(call => call.command === "w2_begin_attempt").length === 2, "fresh composition begins natively");
    }
    await native.finish();
  } catch (error) {
    await native.abort();
    throw error;
  } finally {
    gate.resolve();
    await fixture.dispose();
    driver.clock.dispose();
  }
}

async function checkPrematureBeforePoll(executable: string) {
  const native = connectNative(executable, "requests");
  const fixture = await mountFixture({ browserPaste: true, invoke: native.invoke });
  await startCase(fixture, "composition-commit");
  const driver = await compositionDriver(fixture);
  try {
    await act(async () => {
      driver.start();
      driver.update("draft");
      driver.premature("before first poll");
      driver.end("é");
      driver.clock.advance(500);
    });
    assert.deepEqual(fixture.terminal.data, ["before first poll", "é"]);
    assert.deepEqual(reads(fixture), []);
    assert.deepEqual(appends(fixture), []);
    await finalRead(fixture);
    assert.equal(field(fixture, "snapshot").textContent, "(empty)");
    assert.match(fixture.container.find(node => node.getAttribute("role") === "alert")?.textContent ?? "", /^Held:/);
    assertTrace(fixture, "composition-commit", 1, ["start", "update", "rejection", "end", "rejection", "final snapshot"]);
    await native.finish();
  } catch (error) {
    await native.abort();
    throw error;
  } finally {
    await fixture.dispose();
    driver.clock.dispose();
  }
}

async function checkUnexpectedCompositionWithAppend(executable: string) {
  const native = connectNative(executable, "requests");
  const gate = deferred();
  const fixture = await mountFixture({
    browserPaste: true,
    async invoke(command, args) {
      const result = await native.invoke(command, args);
      if (command === "w2_append_input") await gate.promise;
      return result;
    },
  });
  await startCase(fixture, "english-return");
  const driver = await compositionDriver(fixture);
  try {
    await act(async () => {
      driver.premature("ok");
      driver.start();
      driver.update("draft");
      driver.premature("unexpected");
      driver.end("é");
      driver.clock.advance(250);
    });
    assert.deepEqual(fixture.terminal.data, ["ok", "unexpected", "é"]);
    assert.deepEqual(appends(fixture), [
      { command: "w2_append_input", args: { generation: 1, sequence: 1, events: [[111, 107]] } },
    ]);
    assertTrace(fixture, "english-return", 1, ["in-flight append uncertain"]);
    assert.match(fixture.container.find(node => node.getAttribute("role") === "alert")?.textContent ?? "", /^Held:/);
    gate.resolve();
    await finalRead(fixture);
    assertTrace(fixture, "english-return", 1, ["in-flight append uncertain", "append acknowledgment"]);
    assert.equal(field(fixture, "snapshot").textContent, "6f6b");
    assert.deepEqual((native.snapshot() as Snapshot).bytes, [111, 107]);
    await native.finish();
  } catch (error) {
    await native.abort();
    throw error;
  } finally {
    gate.resolve();
    await fixture.dispose();
    driver.clock.dispose();
  }
}

async function checkpointControls() {
  await withNativeExecutable(async executable => {
    for (const bypass of ["return;", "return deliver(text);"]) {
      await assert.rejects(checkCheckpoint(executable, source => replaceOnce(source,
        "const bytes = encoder.encode(text);", `if (attempt.phase === "active") ${bypass}\n          const bytes = encoder.encode(text);`)),
      /unexpected input holds|guarded composition appends|composition trace phases/);
    }
    await assert.rejects(checkCheckpoint(executable, source => replaceOnce(source,
      'const recorder = registerRecorder(input, transport, onCaptureError);',
      'const recorder = registerRecorder({ onData: () => ({ dispose() {} }) }, transport, onCaptureError);')),
    /guarded composition appends/);
    await assert.rejects(checkCheckpoint(executable, source => replaceOnce(source,
      'const read = recorder.read(() => eligible(attempt) && epoch === pollEpoch);',
      'const read = Promise.reject(new Error("disconnected read"));')),
    /checkpoint read must not overlap/);
    await assert.rejects(checkCheckpoint(executable, source => replaceOnce(source,
      'const sequence = record(attempt, "active checkpoint"', 'textarea.focus(); const sequence = record(attempt, "active checkpoint"')),
    /checkpoint does not request focus/);
    await checkPrematureBeforePoll(executable);
    await checkUnexpectedCompositionWithAppend(executable);
    await checkCheckpoint(executable);
    for (const stop of ["end", "blur", "hide", "case", "restart", "unmount"] as const) {
      await checkLateRead(executable, stop);
    }
  });
}

async function checkForegroundRecovery(
  executable: string,
  id: "app-switch" | "lock-foreground",
  transformEntry?: MountOptions["transformEntry"],
  readback: "available" | "unavailable" | "cached" = "available",
) {
  const native = connectNative(executable, "requests");
  const late = deferred();
  let delayAppend = readback !== "cached";
  let failRead = readback === "unavailable";
  const vector = requiredVectors[id];
  const before = [...Buffer.from(vector.beforeHex, "hex")];
  const after = [...Buffer.from(vector.afterHex, "hex")];
  const fixture = await mountFixture({
    browserPaste: true,
    ...(transformEntry === undefined ? {} : { transformEntry }),
    async invoke(command, args) {
      const generation = args && typeof args === "object" && "generation" in args ? args.generation : undefined;
      if (failRead && command === "w2_read_attempt" && generation === 1) {
        throw new Error("Interrupted read unavailable");
      }
      const reply = await native.invoke(command, args);
      if (delayAppend && command === "w2_append_input" && generation === 1) {
        await late.promise;
        if (readback === "cached") throw new Error("Interrupted append acknowledgment unavailable");
      }
      return reply;
    },
  });
  const recoveryLabel = "Start fresh and refocus";
  async function visibility(state: string) {
    await fixture.act(async () => {
      fixture.document.visibilityState = state;
      fixture.document.dispatch("visibilitychange");
    });
  }
  try {
    await startCase(fixture, id);
    await fixture.click("Focus");
    const focuses = fixture.terminal.focuses;
    await fixture.input(String.fromCharCode(...before));
    const oldAppends = [
      { command: "w2_append_input", args: { generation: 1, sequence: 1, events: [before] } },
    ];
    if (readback === "cached") {
      await finalRead(fixture);
      assert.equal(field(fixture, "snapshot").textContent, vector.beforeHex);
      delayAppend = true;
      failRead = true;
      const unconfirmed = "x";
      await fixture.input(unconfirmed);
      oldAppends.push({ command: "w2_append_input", args: { generation: 1, sequence: 2, events: [[unconfirmed.charCodeAt(0)]] } });
    }
    await fixture.input("queued old input");
    assert.equal(appends(fixture).length, oldAppends.length, "old input is queued behind a pending acknowledgment");

    await visibility("hidden");
    const retained = fixture.document.getElementById("retained-snapshot");
    assert(retained, "visibility retires the attempt and publishes retained state");
    const retainedHex = readback === "unavailable" ? "Unavailable" : vector.beforeHex;
    await fixture.waitFor(() => retained.textContent === retainedHex, "retained state shows the accepted prefix or explicit unavailability");
    assert.equal(Boolean(fixture.document.getElementById("retained-uncertainty")), readback !== "available");
    assert.match(fixture.container.textContent, new RegExp(`Interrupted attempt 1 · ${id} · generation 1 · Invalid`));
    assert(fixture.button(recoveryLabel).disabled, "recovery is disabled while suspended");
    await fixture.input("hidden input");
    assert.equal(appends(fixture).length, oldAppends.length, "no sends while suspended");

    await visibility("visible");
    await fixture.input("input before deliberate recovery");
    assert.equal(appends(fixture).length, oldAppends.length, "foregrounding does not admit input");
    assert.equal(fixture.ipc.filter(call => call.command === "w2_begin_attempt").length, 1, "foregrounding does not start a generation");
    await fixture.click(recoveryLabel);
    await fixture.waitFor(() => fixture.ipc.filter(call => call.command === "w2_begin_attempt").length === 2,
      "recovery control begins a fresh generation");
    await fixture.waitFor(() => !fixture.button("Read").disabled, "fresh attempt enables Read");
    assert.equal(fixture.terminal.focuses, focuses + 1, "recovery explicitly refocuses");
    assert.equal(appends(fixture).length, oldAppends.length, "recovery invokes no append for queued input");
    await finalRead(fixture);
    assert.equal(field(fixture, "snapshot").textContent, "(empty)", "fresh generation starts empty");
    assert.equal(retained.textContent, retainedHex, "starting fresh preserves the old readout");

    await fixture.act(async () => { late.resolve(); });
    await fixture.input(String.fromCharCode(...after));
    await fixture.click("Read");
    await fixture.waitFor(() => !fixture.button("Read").disabled, "fresh read completes");
    assert.deepEqual(appends(fixture), [
      ...oldAppends,
      { command: "w2_append_input", args: { generation: 2, sequence: 1, events: [after] } },
    ], "append invocations contain only the original and fresh input without replay");
    assert.equal(field(fixture, "snapshot").textContent, vector.afterHex);
    assert.equal(retained.textContent, retainedHex, "late callback cannot alter retained evidence");
    assert.deepEqual((native.snapshot() as Snapshot).bytes, after);
    await native.finish();
  } catch (error) {
    await native.abort();
    throw error;
  } finally {
    late.resolve();
    await fixture.dispose();
  }
  assert.equal(fixture.document.listenerCount("visibilitychange"), 0, "visibility listeners are disposed");
}

async function checkLateBegin(executable: string) {
  const native = connectNative(executable, "requests");
  const late = deferred();
  let delayBegin = true;
  const fixture = await mountFixture({
    async invoke(command, args) {
      const reply = await native.invoke(command, args);
      if (delayBegin && command === "w2_begin_attempt") {
        delayBegin = false;
        await late.promise;
      }
      return reply;
    },
  });
  async function visibility(state: string) {
    await fixture.act(async () => {
      fixture.document.visibilityState = state;
      fixture.document.dispatch("visibilitychange");
    });
  }
  try {
    await fixture.waitFor(() => {
      const start = fixture.container.find(node => node.localName === "button" && node.textContent === "Start");
      return Boolean(start && !start.disabled);
    }, "fixture becomes ready before a delayed start");
    await fixture.click("Start");
    await fixture.waitFor(() => fixture.ipc.some(call => call.command === "w2_begin_attempt"), "native start is pending");
    await visibility("hidden");
    await visibility("visible");
    late.resolve();
    await fixture.waitFor(() => {
      const start = fixture.container.find(node => node.localName === "button" && node.textContent === "Start");
      return Boolean(start && !start.disabled);
    }, "visibility change retires the delayed start");
    assert.equal(Boolean(fixture.container.find(node => node.localName === "button" && node.textContent === "Restart")), false,
      "a pre-background start cannot activate after foregrounding");
    assert.equal(fixture.button("Read").disabled, true);
    await fixture.input("abandoned");
    assert.equal(appends(fixture).length, 0, "the retired start admits no input");

    await fixture.click("Start");
    await fixture.waitFor(() => Boolean(fixture.container.find(node =>
      node.localName === "button" && node.textContent === "Restart" && !node.disabled)), "a deliberate new start completes");
    await fixture.input("ї");
    await fixture.waitFor(() => appends(fixture).length === 1, "fresh input reaches the native recorder");
    assert.deepEqual(appends(fixture), [
      { command: "w2_append_input", args: { generation: 2, sequence: 1, events: [[0xd1, 0x97]] } },
    ], "only the deliberate foreground generation records input");
    await native.finish();
  } catch (error) {
    await native.abort();
    throw error;
  } finally {
    late.resolve();
    await fixture.dispose();
  }
}

async function checkCompositionRetirement(executable: string, endBeforeHide: boolean) {
  const native = connectNative(executable, "requests");
  const fixture = await mountFixture({ browserPaste: true, invoke: native.invoke });
  await startCase(fixture, "composition-commit");
  const driver = await compositionDriver(fixture);
  const compositionView = field(fixture, "terminal").find(node => node.classList.contains("composition-view"));
  assert(compositionView, "xterm mounts its preedit view");
  async function visibility(state: string) {
    await act(async () => {
      fixture.document.visibilityState = state;
      fixture.document.dispatch("visibilitychange");
    });
  }
  try {
    await act(async () => {
      driver.start();
      driver.update("draft");
      driver.clock.advance(250);
    });
    await fixture.waitFor(() => field(fixture, "checkpoint").textContent.includes("active and focused"), "focused checkpoint");
    assert(compositionView.classList.contains("active"));
    await act(async () => {
      driver.update("abandoned");
      if (endBeforeHide) driver.end("é");
    });
    if (endBeforeHide) {
      await fixture.click("Read");
      assert.equal(reads(fixture).length, 1, "early Read is still settling before hide");
    }
    await visibility("hidden");
    await fixture.waitFor(() => field(fixture, "retained-snapshot").textContent === "(empty)", "retained interruption result");
    await visibility("visible");
    assert.equal(field(fixture, "snapshot").textContent, "—", "retired Read cannot publish");
    assert.equal(reads(fixture).length, 2, "only checkpoint and interruption read the retired generation");

    await fixture.click("Start fresh and refocus");
    assert(fixture.terminal.options.disableStdin, "stdin is disabled while old xterm callbacks drain");
    assert.equal(fixture.ipc.filter(call => call.command === "w2_begin_attempt").length, 1);
    assert(!compositionView.classList.contains("active"), "recovery ends xterm preedit without an OS event");
    assert.equal(driver.textarea.value, "");
    assert.equal(fixture.document.activeElement, driver.textarea);
    await act(async () => { driver.clock.advance(0); });
    await fixture.waitFor(() => Boolean(fixture.container.find(node =>
      node.localName === "button" && node.textContent === "Restart" && !node.disabled)), "recovery opens a new native generation");
    await act(async () => {
      driver.end("old commit");
      driver.clock.advance(250);
    });
    assert.equal(appends(fixture).length, 0, "late old callbacks contribute zero bytes");
    assert.equal(fixture.terminal.data.length, 0, "xterm emits no retired input");
    assert.equal(driver.textarea.value, "", "late composition input leaves no retired text in xterm");
    assert.equal(field(fixture, "snapshot").textContent, "—");
    assert.equal(field(fixture, "retained-snapshot").textContent, "(empty)");
    await compositionVector(fixture, native, "composition-commit", 2, { attemptId: 2 });
    assert.equal(field(fixture, "retained-snapshot").textContent, "(empty)");
    await native.finish();
  } catch (error) {
    await native.abort();
    throw error;
  } finally {
    await fixture.dispose();
    driver.clock.dispose();
  }
}

async function foregroundRecovery() {
  await withNativeExecutable(async executable => {
    await checkCompositionRetirement(executable, false);
    await checkCompositionRetirement(executable, true);
    await checkLateBegin(executable);
    await checkForegroundRecovery(executable, "app-switch");
    await checkForegroundRecovery(executable, "lock-foreground");
    await checkForegroundRecovery(executable, "lock-foreground", undefined, "unavailable");
    await checkForegroundRecovery(executable, "lock-foreground", undefined, "cached");
    await assert.rejects(checkForegroundRecovery(executable, "app-switch", source => replaceOnce(source,
      'document.addEventListener("visibilitychange", visibilityChanged);', "")),
    /visibility retires the attempt and publishes retained state/);
    await assert.rejects(checkForegroundRecovery(executable, "app-switch", source => replaceOnce(source,
      'onClick={start}', 'onClick={recovering ? undefined : start}')),
    /recovery control begins a fresh generation/);
    await assert.rejects(checkForegroundRecovery(executable, "app-switch", source => replaceOnce(source,
      'terminal.options.disableStdin = false;', 'terminal.options.disableStdin = false; if (refocus) terminal.paste("queued old input");')),
    /recovery invokes no append for queued input/);
  });
}


async function touchCopyVerification() {
  await withNativeExecutable(async executable => {
    const native = connectNative(executable, "requests");
    const fixture = await mountFixture({ browserPaste: true, invoke: native.invoke });
    try {
      await startCase(fixture, "touch-selection-copy");
      await fixture.click("Focus");
      const terminal = fixture.terminal.browserTerminal!;
      const textarea = fixture.terminal.textarea as unknown as Element;
      const token = "COPY-Ї";
      const buffer = terminal.buffer.active;
      const lines = Array.from({ length: buffer.length }, (_, row) => buffer.getLine(row)?.translateToString(true));
      const row = lines.findIndex(line => line?.includes(token));
      assert(row >= 0, "copy target is visible in the xterm buffer");
      terminal.select(lines[row]!.indexOf(token), row, token.length);
      const clipboard = new Map<string, string>();
      const clipboardData = { setData: clipboard.set.bind(clipboard), getData: clipboard.get.bind(clipboard) };
      const clipboardEvent = (type: string) =>
        Object.assign(new Event(type, { bubbles: true, cancelable: true }), { clipboardData });
      textarea.dispatchEvent(clipboardEvent("copy"));
      assert.equal(clipboard.get("text/plain"), token, "xterm copies the selected token");
      const verify = field(fixture, "verify-copied-token");
      assert(!field(fixture, "terminal").contains(verify));
      assert(fixture.container.find(node => node.localName === "label" && node.textContent === "Verify copied token"));
      await act(async () => {
        verify.focus();
        verify.dispatchEvent(clipboardEvent("paste"));
      });
      assert.equal(field(fixture, "copy-verification").textContent, "Match");
      clipboard.set("text/plain", `${token} `);
      await act(async () => { verify.dispatchEvent(clipboardEvent("paste")); });
      assert.equal(field(fixture, "copy-verification").textContent, "No match");
      assert.equal(verify.value, "", "clipboard text is discarded");
      clipboard.set("text/plain", token);
      await act(async () => { verify.dispatchEvent(clipboardEvent("paste")); });
      await finalRead(fixture);
      assert.equal(field(fixture, "copy-verification").textContent, "Match");
      assert.equal(field(fixture, "snapshot").textContent, "(empty)");
      assert.match(field(fixture, "snapshot-counts").textContent, /0 accepted events · 0 bytes/);
      assert.deepEqual(fixture.ipc, [
        { command: "w2_begin_attempt", args: { caseId: "touch-selection-copy" } },
        { command: "w2_read_attempt", args: { generation: 1 } },
      ]);
      assert.deepEqual(native.snapshot(), {
        caseId: "touch-selection-copy", generation: 1, nextSequence: 1,
        bytes: [], eventLengths: [], rejected: 0, invalid: false,
      });
      await fixture.click("Restart");
      await fixture.waitFor(() => field(fixture, "copy-verification").textContent === "", "restart clears copy verification");
      await native.finish();
    } catch (error) {
      await native.abort();
      throw error;
    } finally {
      await fixture.dispose();
    }
  });
}

const commands = ["w2_begin_attempt", "w2_append_input", "w2_read_attempt"];
const grants = commands.map(command => `allow-${command.replaceAll("_", "-")}`);
const iconPath = "../../src-tauri/icons/32x32.png";
const iosCfg = '#[cfg(target_os = "ios")]';
const mobileEntry = "#[cfg_attr(mobile, tauri::mobile_entry_point)]";
const commandAttribute = '#[cfg_attr(target_os = "ios", tauri::command)]';
const targetGuard = 'if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios")';

function parseToml(source: string) {
  const parsed = spawnSync("python3", ["-c",
    "import json, sys, tomllib; print(json.dumps(tomllib.loads(sys.stdin.read())))",
  ], { input: source, encoding: "utf8", timeout: 5000 });
  assert.ifError(parsed.error);
  assert.equal(parsed.status, 0, parsed.stderr);
  return JSON.parse(parsed.stdout);
}

function readEntries(directory: URL) {
  const base = fileURLToPath(directory);
  try {
    assert(lstatSync(base).isDirectory(), `declaration path must be a directory: ${base}`);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    return [];
  }
  return readdirSync(base, { withFileTypes: true, recursive: true }).map(entry => {
    const absolute = join(entry.parentPath, entry.name);
    return {
      path: relative(base, absolute),
      kind: entry.isDirectory() ? "directory" : entry.isFile() ? "file" : "other",
      source: entry.isFile() ? readFileSync(absolute, "utf8") : "",
    };
  });
}

function readBoundary() {
  const macDir = new URL("../../src-tauri/", nativeDir);
  return {
    manifest: readFileSync(new URL("Cargo.toml", nativeDir), "utf8"),
    buildScript: readFileSync(new URL("build.rs", nativeDir), "utf8"),
    library: readFileSync(nativeSource, "utf8"),
    config: JSON.parse(readFileSync(new URL("tauri.conf.json", nativeDir), "utf8")),
    overlays: readdirSync(nativeDir).filter(name => name !== "tauri.conf.json"
      && /^(?:tauri(?:[.-][\w.-]+)?\.conf\.(?:json5?|toml)|tauri(?:[.-][\w.-]+)?\.toml)$/i.test(name)),
    capabilities: readEntries(new URL("capabilities/", nativeDir)),
    permissions: readEntries(new URL("permissions/", nativeDir)),
    icon: readFileSync(new URL(iconPath, nativeDir)),
    macLibrary: readFileSync(new URL("src/lib.rs", macDir), "utf8"),
    macCapabilities: readEntries(new URL("capabilities/", macDir)),
    macPermissions: readEntries(new URL("permissions/", macDir)),
    macConfig: JSON.parse(readFileSync(new URL("tauri.conf.json", macDir), "utf8")),
  };
}

function rustTokens(source: string): string[] {
  const pattern = /\s+|\/\/[^\n]*|\/\*|(?:br|cr|r)(#*)"[\s\S]*?"\1|(?:b|c)?"(?:\\[\s\S]|[^"\\])*"|b?'(?:\\.|[^'\\\n])'|[A-Za-z_]\w*|::|==|[^\s]/gy;
  const tokens: string[] = [];
  for (let match = pattern.exec(source); match; match = pattern.exec(source)) {
    const token = match[0];
    if (token === "/*") {
      const comment = /\x2f\x2a|\x2a\x2f/g;
      comment.lastIndex = pattern.lastIndex;
      let depth = 1;
      while (depth) {
        const marker = comment.exec(source);
        assert(marker, "unterminated Rust comment");
        depth += marker[0] === "/*" ? 1 : -1;
      }
      pattern.lastIndex = comment.lastIndex;
    } else if (!token.startsWith("//") && !/^\s/.test(token)) {
      tokens.push(token);
    }
  }
  return tokens;
}

function positions(tokens: string[], pattern: string, topLevel = false) {
  const wanted = rustTokens(pattern);
  const indices: number[] = [];
  let depth = 0;
  for (let index = 0; index < tokens.length; index++) {
    if ((!topLevel || depth === 0) && wanted.every((token, offset) => tokens[index + offset] === token)) {
      indices.push(index);
    }
    if (tokens[index] === "{") depth += 1;
    if (tokens[index] === "}") depth -= 1;
  }
  return indices;
}

function blockEnd(tokens: string[], start: number, open: string, close: string) {
  assert.equal(tokens[start], open);
  let depth = 1;
  for (let index = start + 1; index < tokens.length; index++) {
    if (tokens[index] === open) depth += 1;
    if (tokens[index] === close && --depth === 0) return index;
  }
  assert.fail(`unclosed Rust delimiter ${open}`);
}

function bodyAt(tokens: string[], position: number, declaration: string) {
  const brace = position + rustTokens(declaration).length - 1;
  return tokens.slice(brace + 1, blockEnd(tokens, brace, "{", "}"));
}

function attributesBefore(tokens: string[], position: number) {
  const attributes: string[] = [];
  while (tokens[position - 1] === "]") {
    let open = position - 2;
    let depth = 1;
    for (; open >= 0; open--) {
      if (tokens[open] === "]") depth += 1;
      if (tokens[open] === "[" && --depth === 0) break;
    }
    if (open < 1 || tokens[open - 1] !== "#") break;
    attributes.unshift(tokens.slice(open - 1, position).join(" "));
    position = open - 1;
  }
  return attributes;
}

function arrayDeclarations(tokens: string[], prefix: string) {
  return positions(tokens, prefix).map(start => {
    const bracket = start + rustTokens(prefix).length - 1;
    const values = tokens.slice(bracket + 1, blockEnd(tokens, bracket, "[", "]"));
    if (values.at(-1) === ",") values.pop();
    return values.length ? values.join(" ").split(",").map(value => value.trim()) : [];
  });
}

function assertBuildBoundary(source: string) {
  const tokens = rustTokens(source);
  const declaration = "fn main() {";
  const [main, ...otherMains] = positions(tokens, declaration, true);
  assert(main !== undefined && !otherMains.length, "fixture build entry");
  const body = bodyAt(tokens, main, declaration);
  const guard = `${targetGuard} {`;
  const [target, ...otherGuards] = positions(body, guard, true);
  assert(target !== undefined && !otherGuards.length, "positive iOS build target guard");
  const guarded = bodyAt(body, target, guard);
  assert.equal(positions(guarded, "tauri_build::try_build(", true).length, 1,
    "Tauri build call belongs to the positive iOS target guard");
  assert.deepEqual(arrayDeclarations(guarded, "tauri_build::AppManifest::new().commands(&["),
    [commands.map(command => JSON.stringify(command))], "app manifest command list");
  assert.deepEqual(tokens, rustTokens(`
    fn main() {
      println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_OS");
      ${guard}
        let manifest = tauri_build::AppManifest::new().commands(&[${commands.map(command => `${JSON.stringify(command)},`).join("")}]);
        tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
          .expect("failed to build the iOS recorder app");
      }
    }
  `), "Tauri build call belongs to the positive iOS target guard");
}

function assertLibraryBoundary(source: string) {
  const tokens = rustTokens(source);
  const recorderDecl = "mod recorder {";
  const [recorder, ...otherRecorders] = positions(tokens, recorderDecl, true);
  assert(recorder !== undefined && !otherRecorders.length, "inline recorder module");
  assert.deepEqual(attributesBefore(tokens, recorder),
    [rustTokens('#[cfg(any(target_os = "ios", test))]').join(" ")], "recorder module target guard");
  const module = bodyAt(tokens, recorder, recorderDecl);
  const wrappers = positions(module, "fn", true).filter(index => module[index + 1]?.startsWith("w2_") ?? false);
  assert.deepEqual(wrappers.map(index => module[index + 1] ?? ""), commands, "exact command wrapper declarations");
  for (const wrapper of wrappers) {
    assert.deepEqual(attributesBefore(module, wrapper), [rustTokens(commandAttribute).join(" ")],
      `iOS command attribute belongs to ${module[wrapper + 1]}`);
  }
  assert.equal(positions(tokens, "tauri::command").length, commands.length, "additional command annotations");

  const runDecl = "pub fn run() {";
  const [run, ...otherRuns] = positions(module, runDecl, true);
  assert(run !== undefined && !otherRuns.length, "recorder run declaration");
  const runAttributes = attributesBefore(module, run);
  assert(runAttributes.includes(rustTokens(iosCfg).join(" ")), "iOS cfg belongs to recorder run");
  assert.deepEqual(runAttributes, [iosCfg, mobileEntry].map(text => rustTokens(text).join(" ")),
    "mobile entry annotation belongs to recorder run");

  const runBody = bodyAt(module, run, runDecl);
  const handler = "tauri::generate_handler![";
  assert.deepEqual(arrayDeclarations(runBody, handler), [commands], "exact registry on iOS run");
  assert.deepEqual(arrayDeclarations(tokens, handler), [commands], "additional fixture registry");
  const registeredBody = `
    tauri::Builder::default()
      .manage(Mutex::new(Recorder::default()))
      .invoke_handler(${handler}${commands.join(",")},])
      .run(tauri::generate_context!())
      .expect("failed to run the offline terminal fixture");
  `;
  assert.deepEqual(runBody, rustTokens(registeredBody), "iOS run manages the recorder and installs its handler");

  const exports = positions(tokens, "pub use", true);
  assert.deepEqual(exports, positions(tokens, "pub use recorder::run;", true), "standalone run re-export");
  assert.equal(exports.length, 1, "standalone run re-export");
  const runExport = exports[0];
  assert(runExport !== undefined, "standalone run export exists");
  assert.deepEqual(attributesBefore(tokens, runExport), [rustTokens(iosCfg).join(" ")], "iOS cfg belongs to the run re-export");
  for (const declaration of ["fn start_app", "#[unsafe(export_name =", "#[export_name ="]) {
    assert.equal(positions(tokens, declaration).length, 0, "handwritten C ABI entry");
  }
}

function assertPermissions(entries: ReturnType<typeof readEntries>) {
  const files = entries.filter(entry => entry.kind !== "directory");
  if (!entries.length) return;
  assert.deepEqual(entries.filter(entry => entry.kind === "directory").map(entry => entry.path),
    ["autogenerated"], "generated permission directory");
  const paths = commands.map(command => join("autogenerated", `${command}.toml`));
  assert.deepEqual(files.map(entry => entry.path).sort(), paths.toSorted(), "generated permission files");
  for (const file of files) {
    assert.equal(file.kind, "file", "generated permission is a regular file");
    const command = commands[paths.indexOf(file.path)];
    assert(command !== undefined, `generated permission matches a command: ${file.path}`);
    const slug = command.replaceAll("_", "-");
    assert.deepEqual(parseToml(file.source), {
      permission: [
        {
          identifier: `allow-${slug}`,
          description: `Enables the ${command} command without any pre-configured scope.`,
          commands: { allow: [command] },
        },
        {
          identifier: `deny-${slug}`,
          description: `Denies the ${command} command without any pre-configured scope.`,
          commands: { deny: [command] },
        },
      ],
    }, `exact generated permission definitions for ${command}`);
  }
}

function assertBoundary(input: ReturnType<typeof readBoundary>) {
  const manifest = parseToml(input.manifest);
  const packageFields = ["name", "version", "edition", "rust-version"];
  assert.deepEqual(Object.keys(manifest).sort(), ["build-dependencies", "lib", "package", "target", "workspace"],
    "standalone manifest sections");
  assert.deepEqual(Object.keys(manifest.package).sort(), packageFields.toSorted(), "standalone package fields");
  assert.equal(manifest.package["rust-version"], "1.97.1", "pinned Rust version");
  assert.equal(manifest.package.edition, "2024", "standalone Rust edition");
  assert.deepEqual(manifest.workspace, { resolver: "3" }, "standalone Cargo workspace");
  assert.deepEqual(manifest.lib, {
    name: "remote_sessions_w2_fixture", "crate-type": ["staticlib", "cdylib", "rlib"],
  }, "mobile library crate types");
  assert.deepEqual(manifest["build-dependencies"], { "tauri-build": "=2.7.6" }, "standalone build dependency");
  assert.deepEqual(manifest.target, {
    'cfg(target_os = "ios")': { dependencies: { serde: { version: "1", features: ["derive"] }, tauri: "=2.12.7" } },
  }, "only iOS recorder dependencies");

  assertBuildBoundary(input.buildScript);
  assertLibraryBoundary(input.library);
  assert.deepEqual(input.overlays, [], "platform or debug configuration overlays");
  assert.deepEqual(input.capabilities, [], "additional capability sources");
  assertPermissions(input.permissions);

  const config = input.config;
  assert.deepEqual(Object.keys(config).sort(), ["$schema", "app", "build", "bundle", "identifier", "productName", "version"],
    "fixture config fields");
  assert.equal(config.identifier, "dev.remotesessions.w2fixture", "standalone bundle identifier");
  assert.equal(config.build.frontendDist, "../dist", "bundled fixture distribution");
  assert.deepEqual(config.build, {
    frontendDist: "../dist",
    beforeBuildCommand: { cwd: "..", script: "node ../../../node_modules/vite/bin/vite.js build" },
  }, "bundled build hook and absence of development settings");
  assert.deepEqual(Object.keys(config.app).sort(), ["security", "windows"], "fixture app fields");
  assert.deepEqual(config.app.windows, [{
    label: "w2-fixture", title: "Offline Terminal Fixture", url: "index.html",
  }], "single bundled local window");
  assert.deepEqual(Object.keys(config.app.security).sort(), ["capabilities", "csp"], "fixture security fields");
  assert.deepEqual(config.app.security.capabilities, [{
    identifier: "w2-fixture", platforms: ["iOS"], local: true, windows: ["w2-fixture"], permissions: grants,
  }], "exact local iOS capability and grants");

  const csp = config.app.security.csp.split(";").map((directive: string) => directive.trim()).filter(Boolean).sort();
  assert.deepEqual(csp, [
    "base-uri 'none'", "connect-src ipc: http://ipc.localhost", "default-src 'none'", "form-action 'none'",
    "script-src 'self'", "style-src 'self' 'unsafe-inline'",
  ], "bundled-only CSP");
  assert.deepEqual(config.bundle, { active: true, icon: [iconPath] }, "fixture icon reference");
  assert.deepEqual([...input.icon.subarray(0, 8)], [137, 80, 78, 71, 13, 10, 26, 10], "existing PNG icon");
  assert.equal(Buffer.from(input.icon.subarray(12, 16)).toString("ascii"), "IHDR", "PNG icon header");
  assert.equal(input.icon[25], 6, "RGBA bundle icon");

  const macHandlers = arrayDeclarations(rustTokens(input.macLibrary), "tauri::generate_handler![");
  assert(macHandlers.length > 0, "production Mac invoke registry");
  assert(!macHandlers.flat().some(handler => /\bw2_/.test(handler)), "production Mac registry excludes W2");
  assert(input.macCapabilities.length > 0, "production Mac capability");
  const macAcl = [JSON.stringify(input.macConfig.app.security), ...input.macCapabilities.map(file => file.source),
    ...input.macPermissions.map(file => file.source)];
  assert(!macAcl.some(source => /\b(?:allow-w2-|deny-w2-|w2_)/.test(source)), "production Mac permissions exclude W2");
}

function declarationControls() {
  const live = readBoundary();
  const permissionFiles = commands.map(command => {
    const slug = command.replaceAll("_", "-");
    return {
      path: join("autogenerated", `${command}.toml`), kind: "file",
      source: `
        [[permission]]
        identifier = "allow-${slug}"
        description = "Enables the ${command} command without any pre-configured scope."
        commands.allow = ["${command}"]
        [[permission]]
        identifier = "deny-${slug}"
        description = "Denies the ${command} command without any pre-configured scope."
        commands.deny = ["${command}"]
      `,
    };
  });
  const permissions = [{ path: "autogenerated", kind: "directory", source: "" }, ...permissionFiles];
  assertBoundary({ ...live, permissions });

  type Boundary = typeof live;
  type Control = [string, RegExp, (boundary: Boundary) => void];
  const controls: Control[] = [
    ["development URL", /absence of development settings/, b => { b.config.build.devUrl = "http://localhost:1420"; }],
    ["development hook", /absence of development settings/, b => { b.config.build.beforeDevCommand = "npm run dev"; }],
    ["platform overlay", /configuration overlays/, b => { b.overlays.push("tauri.ios.conf.json"); }],
    ["debug overlay", /configuration overlays/, b => { b.overlays.push("tauri.debug.conf.json"); }],
    ["network egress", /bundled-only CSP/, b => { b.config.app.security.csp += "; img-src https:"; }],
    ["debug CSP", /fixture security fields/, b => { b.config.app.security.devCsp = "default-src *"; }],
    ["remote window", /single bundled local window/, b => { b.config.app.windows[0].url = "https://example.test"; }],
    ["remote grants", /exact local iOS capability/, b => {
      b.config.app.security.capabilities[0].remote = { urls: ["https://example.test"] };
    }],
    ["wildcard windows", /exact local iOS capability/, b => { b.config.app.security.capabilities[0].windows = ["*"]; }],
    ["wildcard WebViews", /exact local iOS capability/, b => { b.config.app.security.capabilities[0].webviews = ["*"]; }],
    ["unscoped platforms", /exact local iOS capability/, b => { delete b.config.app.security.capabilities[0].platforms; }],
    ["extra grant", /exact local iOS capability/, b => { b.config.app.security.capabilities[0].permissions.push("core:default"); }],
    ["plugins", /fixture config fields/, b => { b.config.plugins = { shell: {} }; }],
    ["separate capability", /additional capability sources/, b => {
      b.capabilities.push({ path: "extra.json", kind: "file", source: "{}" });
    }],
    ["extra permission definition", /generated permission files/, b => {
      b.permissions = [...permissions, { path: "default.toml", kind: "file", source: "[default]\npermissions = []" }];
    }],
    ["broadened generated permission", /exact generated permission definitions/, b => {
      b.permissions = structuredClone(permissions);
      const begin = b.permissions[1];
      assert(begin !== undefined, "begin permission exists");
      begin.source = begin.source.replace('commands.allow = ["w2_begin_attempt"]',
        'commands.allow = ["w2_begin_attempt", "arbitrary"]');
    }],
    ["host dependency", /only iOS recorder dependencies/, b => {
      b.manifest += '\n[target.\'cfg(target_os = "macos")\'.dependencies]\nhost = {path = "../../host"}\n';
    }],
    ["extra manifest command", /app manifest command list/, b => {
      b.buildScript = replaceOnce(b.buildScript, '"w2_read_attempt",', '"w2_read_attempt", "extra",');
    }],
    ["removed build guard", /positive iOS build target guard/, b => {
      b.buildScript = replaceOnce(b.buildScript, targetGuard, "if true");
    }],
    ["misplaced build guard", /Tauri build call belongs/, b => {
      b.buildScript = replaceOnce(b.buildScript, `${targetGuard} {`, `${targetGuard} {}\n    {`);
      b.buildScript += `\n// ${targetGuard} { tauri_build::try_build(...) }\n`;
    }],
    ["extra wrapper", /exact command wrapper declarations/, b => {
      b.library = replaceOnce(b.library, "mod recorder {", `mod recorder {\n    ${commandAttribute}\n    fn w2_extra() {}`);
    }],
    ["extra registration", /exact registry on iOS run/, b => {
      b.library = replaceOnce(b.library, "                w2_read_attempt,", "                w2_read_attempt,\n                w2_extra,");
    }],
    ["removed run guard", /iOS cfg belongs to recorder run/, b => {
      b.library = replaceOnce(b.library, `${iosCfg}\n    ${mobileEntry}`, `// ${iosCfg}\n    ${mobileEntry}`);
    }],
    ["removed re-export guard", /iOS cfg belongs to the run re-export/, b => {
      b.library = replaceOnce(b.library, `${iosCfg}\npub use recorder::run;`, "pub use recorder::run;");
    }],
    ["removed mobile entry attribution", /mobile entry annotation belongs to recorder run/, b => {
      b.library = replaceOnce(b.library, mobileEntry, `/* ${mobileEntry} */`);
    }],
    ["misplaced mobile entry attribution", /mobile entry annotation belongs to recorder run/, b => {
      b.library = replaceOnce(b.library, `${iosCfg}\n    ${mobileEntry}`,
        `${iosCfg}\n    ${mobileEntry}\n    fn unrelated() {}\n\n    ${iosCfg}`);
    }],
    ["handwritten start_app", /handwritten C ABI entry/, b => { b.library += '\npub extern "C" fn start_app() {}\n'; }],
    ["handwritten start_app export", /handwritten C ABI entry/, b => {
      b.library += '\n#[unsafe(export_name = "start_app")]\npub extern "C" fn alternate_entry() {}\n';
    }],
    ["Mac registration", /production Mac registry excludes W2/, b => {
      b.macLibrary = replaceOnce(b.macLibrary, "tauri::generate_handler![", "tauri::generate_handler![w2_read_attempt,");
    }],
    ["Mac grant", /production Mac permissions exclude W2/, b => {
      const capability = b.macCapabilities.find(file => file.kind === "file");
      assert(capability);
      capability.source = '{"permissions":["allow-w2-read-attempt"]}';
    }],
  ];
  for (const [name, message, alter] of controls) {
    const candidate = structuredClone(live);
    alter(candidate);
    assert.throws(() => assertBoundary(candidate), { message }, name);
  }
}

if (import.meta.main) {
  test("xterm input reaches the native recorder", mountedTracer);
  test("mandatory cases use independent vectors and exact native case IDs", assertManifestVectors);
  test("paste vectors match xterm newline normalization", pasteVectorsMatch);
  test("production case controls launch every mandatory manifest vector", productionCaseControls);
  test("viewport updates keep one terminal and dispose listeners", viewportUpdates);
  test("mounted fixture wires layout without remounting its terminal", mountedLayout);
  test("mounted composition checkpoint reads native bytes before commit without moving focus", checkpointControls);
  test("foreground restart preserves evidence without replay", foregroundRecovery);
  test("touch copy verifies the pasted token with an empty same-generation read", touchCopyVerification);
  test("native tracer rejects disconnected command wrappers", disconnectedWrapperControls);
  test("declared fixture boundary matches the iOS-only contract", () => assertBoundary(readBoundary()));
  test("declared fixture boundary rejects broadened declarations", declarationControls);
}
