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
import { act } from "react";
import ts from "typescript";
import type { Root } from "react-dom/client";
import type { Invoke, registerRecorder } from "./recorder.ts";

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
};
type Listener = (event: {
  type: string;
  target: Element;
  stopPropagation: () => void;
  preventDefault: () => void;
}) => void;

// Only the DOM operations used by this entry and React DOM. xterm's visual renderer
// is replaced independently, so its input event delivery and write queue stay real.
class Element {
  id = "";
  parentNode: Element | null = null;
  childNodes: Element[] = [];
  style = { setProperty() {}, removeProperty() {} };
  attributes = new Map<string, string>();
  private listeners = new Map<string, { capture: boolean; callback: Listener }[]>();
  private text = "";

  readonly localName: string;
  readonly ownerDocument: FixtureDocument | null;
  readonly nodeType: number;
  readonly namespaceURI: string;
  readonly clientWidth = 360;
  readonly clientHeight = 160;

  constructor(localName: string, ownerDocument: FixtureDocument | null, nodeType = 1, namespaceURI = "http://www.w3.org/1999/xhtml") {
    this.localName = localName;
    this.ownerDocument = ownerDocument;
    this.nodeType = nodeType;
    this.namespaceURI = namespaceURI;
  }

  get disabled() { return this.attributes.has("disabled"); }
  get tagName() { return this.localName.toUpperCase(); }
  get nodeName() { return this.tagName; }
  get firstChild() { return this.childNodes[0] ?? null; }
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
    if (this.disabled) return;
    const path: Element[] = [];
    for (let node: Element | null = this; node; node = node.parentNode) path.push(node);
    let stopped = false;
    const event = {
      type: "click",
      target: this,
      stopPropagation() { stopped = true; },
      preventDefault() {},
    };
    for (const capture of [true, false]) {
      for (const element of capture ? path.toReversed() : path) {
        if (stopped) return;
        for (const listener of element.listeners.get("click") ?? []) {
          if (listener.capture === capture) listener.callback(event);
        }
      }
    }
  }

  find(predicate: (element: Element) => boolean): Element | undefined {
    if (predicate(this)) return this;
    for (const child of this.childNodes) {
      const found = child.find(predicate);
      if (found) return found;
    }
  }
}

class FixtureDocument extends Element {
  readonly documentElement = this.createElement("html");
  readonly body = this.createElement("body");
  readonly activeElement = this.body;
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
  readonly headless: InstanceType<typeof headless.Terminal>;
  subscriptions = 0;
  readonly data: string[] = [];
  private lastWrite = Promise.resolve();

  constructor(options: TerminalOptions) {
    this.headless = new headless.Terminal(options);
  }

  get options() { return this.headless.options; }
  get cols() { return this.headless.cols; }
  get rows() { return this.headless.rows; }
  open() {}
  focus() {}

  write(data: string | Uint8Array, callback?: () => void) {
    this.lastWrite = new Promise<void>(resolve => {
      this.headless.write(data, () => {
        try {
          callback?.();
        } finally {
          resolve();
        }
      });
    });
  }

  afterWrite() { return this.lastWrite; }
  input(data: string) { this.headless.input(data, true); }

  onData(callback: (data: string) => void) {
    this.subscriptions += 1;
    return this.headless.onData(text => {
      this.data.push(text);
      callback(text);
    });
  }

  dispose() { this.headless.dispose(); }
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
  assert(scripts[0][1].startsWith("./"), "fixture HTML loads a local module entry");
  const entry = new URL(scripts[0][1], fixtureDir);
  assert(entry.href.startsWith(fixtureDir.href), "module entry belongs to the fixture directory");
  return { id: root[1], entry };
}

export async function mountFixture(options: MountOptions = {}) {
  const { id, entry } = entryFromHtml();
  const document = new FixtureDocument();
  const container = document.createElement("div");
  container.id = id;
  document.body.appendChild(container);
  const ipc: Request[] = [];
  const registrations: Registration[] = [];
  const roots: Root[] = [];
  const terminals: FixtureTerminal[] = [];
  const window = Object.assign(new EventTarget(), {
    document,
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
    installGlobal("ResizeObserver", class { observe() {} disconnect() {} }),
    installGlobal("getComputedStyle", () => ({ paddingLeft: "0", paddingRight: "0", paddingTop: "0", paddingBottom: "0" })),
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
          super(terminalOptions);
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
    const virtual = new Map<string, string>();
    const hooks = registerHooks({
      resolve(specifier, context, next) {
        if (context.parentURL === entry.href) {
          const adapter = adapters.get(specifier) ?? (specifier.endsWith(".css") ? "export {};" : undefined);
          if (adapter !== undefined) {
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
      act,
      button,
      async click(label: string) { await act(async () => { button(label).click(); }); },
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

type InputMode = "combined" | "split";

function requestFrame(command: string, raw: InvokeArgs) {
  assert(raw && Object.getPrototypeOf(raw) === Object.prototype, "fixture IPC takes object arguments");
  const args = raw as Record<string, unknown>;
  switch (command) {
    case "w2_begin_attempt":
      assert.deepEqual(Object.keys(args), ["caseId"]);
      assert.equal(args.caseId, syntheticCase);
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
    const mounted = await mountFixture({ invoke: native.invoke, transformEntry });
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
    assert.deepEqual(mounted.registrations[0].calls, mounted.ipc, "registered invoke trace matches actual IPC");
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

const registration = "const recorder = registerRecorder(terminal, invoke, onCaptureError);";

function skipRegistration(source: string) {
  return replaceOnce(source, registration, `
    const recorder = {
      start: async () => 1,
      read: async () => ({ bytes: [] }),
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
    await assert.rejects(assertMountedInput(executable, "combined", skipRegistration), /production recorder registration/);
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
    const controls = [
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
      const mutated = replaceOnce(source, signature, `${original[1]}        ${replacement}\n    }`);
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

// Keep string literals opaque and ignore line and nested block comments before
// checking which declarations own the security-sensitive attributes and calls.
function rustTokens(source: string): string[] {
  const pattern = /\s+|\/\/[^\n]*|\/\*|(?:br|cr|r)(#*)"[\s\S]*?"\1|(?:b|c)?"(?:\\[\s\S]|[^"\\])*"|b?'(?:\\.|[^'\\\n])'|[A-Za-z_]\w*|::|==|[^\s]/gy;
  const tokens: string[] = [];
  for (let match = pattern.exec(source); match; match = pattern.exec(source)) {
    const token = match[0];
    if (token === "/*") {
      const comment = /\/\*|\*\//g;
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
  const wrappers = positions(module, "fn", true).filter(index => module[index + 1].startsWith("w2_"));
  assert.deepEqual(wrappers.map(index => module[index + 1]), commands, "exact command wrapper declarations");
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
  assert.deepEqual(attributesBefore(tokens, exports[0]), [rustTokens(iosCfg).join(" ")], "iOS cfg belongs to the run re-export");
  assert.equal(positions(tokens, "fn start_app").length, 0, "handwritten C ABI entry");
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
      b.permissions[1].source = b.permissions[1].source.replace('commands.allow = ["w2_begin_attempt"]',
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
  test("native tracer rejects disconnected command wrappers", disconnectedWrapperControls);
  test("declared fixture boundary matches the iOS-only contract", () => assertBoundary(readBoundary()));
  test("declared fixture boundary rejects broadened declarations", declarationControls);
}
