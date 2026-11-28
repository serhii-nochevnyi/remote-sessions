import type { Terminal } from "@xterm/xterm";
import type { Chunk, TerminalViewScope } from "./local-client.ts";

// @invariant: tmux is the application's terminal protocol endpoint.
function suppressViewerQueries(terminal: Pick<Terminal, "parser">) {
  return [
    terminal.parser.registerCsiHandler({ final: "c" }, () => true),
    terminal.parser.registerCsiHandler({ prefix: ">", final: "c" }, () => true),
    terminal.parser.registerCsiHandler({ final: "n" }, () => true),
    terminal.parser.registerCsiHandler({ prefix: "?", final: "n" }, () => true),
    terminal.parser.registerCsiHandler({ intermediates: "$", final: "p" }, () => true),
    terminal.parser.registerCsiHandler({ prefix: "?", intermediates: "$", final: "p" }, () => true),
    terminal.parser.registerCsiHandler({ final: "t" }, params => [14, 16, 18, 20, 21].includes(Number(params[0]))),
    terminal.parser.registerDcsHandler({ intermediates: "$", final: "q" }, () => true),
    terminal.parser.registerDcsHandler({ intermediates: "+", final: "q" }, () => true),
    // @contract: Delegate mixed operands in order and drop their reports at the input source.
    terminal.parser.registerOscHandler(4, data => {
      const operands = data.split(";");
      return operands.length % 2 === 0 && operands.every((operand, index) =>
        index % 2 === 0 ? /^\d+$/.test(operand) && Number(operand) <= 255 : operand === "?");
    }),
    // @contract: xterm applies special-color operands from the starting slot through cursor color (12).
    ...[10, 11, 12].map(id => terminal.parser.registerOscHandler(id, data =>
      data.split(";", 13 - id).every(operand => operand === "?"))),
  ];
}

type XtermInputSource = {
  triggerDataEvent(data: string, wasUserInput?: boolean): void;
};

export const TERMINAL_INPUT_BUDGET = 65536;
const TERMINAL_INPUT_CHUNK = 4096;

export function* terminalInputChunks(bytes: Uint8Array) {
  for (let offset = 0; offset < bytes.length; offset += TERMINAL_INPUT_CHUNK) {
    yield { offset, bytes: bytes.subarray(offset, offset + TERMINAL_INPUT_CHUNK) };
  }
}

export function encodeTerminalPaste(plainText: string): Uint8Array | null {
  if (plainText.length > TERMINAL_INPUT_BUDGET * 2) return null;
  const text = plainText.replace(/\r\n?/g, "\n");
  if (text.length > TERMINAL_INPUT_BUDGET) return null;
  const bytes = new Uint8Array(Math.min(text.length * 3, TERMINAL_INPUT_BUDGET));
  const { read, written } = new TextEncoder().encodeInto(text, bytes);
  return read === text.length ? bytes.subarray(0, written) : null;
}

export function bindTerminalPaste(container: EventTarget, send: (bytes: Uint8Array | null) => void) {
  const capture = (event: Event) => {
    event.preventDefault();
    event.stopImmediatePropagation();
    const text = (event as ClipboardEvent).clipboardData?.getData("text/plain");
    if (text) send(encodeTerminalPaste(text));
  };
  container.addEventListener("paste", capture, true);
  return { dispose: () => container.removeEventListener("paste", capture, true) };
}

function routeTerminalInput(
  terminal: Pick<Terminal, "options" | "onBinary">,
  send: (bytes: Uint8Array) => void,
) {
  // @contract: xterm 6.0.0 keeps the text origin on triggerDataEvent through keyboard and IME handling.
  const core = (terminal as { _core?: { coreService?: XtermInputSource } })._core?.coreService;
  if (typeof core?.triggerDataEvent !== "function") return;
  const triggerDataEvent = core.triggerDataEvent;
  const encoder = new TextEncoder();
  let active = true;
  const routeUserData: XtermInputSource["triggerDataEvent"] = (data, wasUserInput) => {
    if (!active || wasUserInput !== true || terminal.options.disableStdin) return;
    triggerDataEvent.call(core, data, true);
    if (!active || terminal.options.disableStdin) return;
    send(encoder.encode(data));
  };
  if (!Reflect.set(core, "triggerDataEvent", routeUserData) || core.triggerDataEvent !== routeUserData) return;
  const binaryInput = terminal.onBinary(data => {
    if (active && !terminal.options.disableStdin) send(Uint8Array.from(data, char => char.charCodeAt(0)));
  });
  return {
    dispose() {
      active = false;
      binaryInput.dispose();
      if (core.triggerDataEvent === routeUserData) core.triggerDataEvent = triggerDataEvent;
    },
  };
}

export function bindTerminalProtocol(
  terminal: Pick<Terminal, "parser" | "options" | "onBinary">,
  send: (bytes: Uint8Array) => void,
) {
  function pauseInput() {
    terminal.options.disableStdin = true;
  }

  pauseInput();
  const queryHandlers = suppressViewerQueries(terminal);
  const input = routeTerminalInput(terminal, send);
  let disposed = false;

  return {
    inputAvailable: input !== undefined,
    pauseInput,
    async consume(scope: TerminalViewScope, view: string, chunk: Chunk, render: () => Promise<void>) {
      if (disposed || !scope.owns(view)) return false;
      if (!chunk.writable) {
        pauseInput();
        if (!await scope.consume(view, chunk, async () => {}) || disposed) return false;
        await render();
        return !disposed && scope.owns(view);
      }
      if (!await scope.consume(view, chunk, render) || disposed) return false;
      terminal.options.disableStdin = !input || !scope.canWrite;
      return true;
    },
    dispose() {
      if (disposed) return;
      disposed = true;
      pauseInput();
      input?.dispose();
      queryHandlers.forEach(handler => handler.dispose());
    },
  };
}
