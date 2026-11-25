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

function routeTerminalInput(
  terminal: Pick<Terminal, "options" | "onBinary">,
  send: (bytes: Uint8Array, paste: boolean) => void,
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
    const paste = data.startsWith("\x1b[200~") && data.endsWith("\x1b[201~");
    send(encoder.encode(paste ? data.slice(6, -6) : data), paste);
  };
  if (!Reflect.set(core, "triggerDataEvent", routeUserData) || core.triggerDataEvent !== routeUserData) return;
  const binaryInput = terminal.onBinary(data => {
    if (active && !terminal.options.disableStdin) send(Uint8Array.from(data, char => char.charCodeAt(0)), false);
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
  send: (bytes: Uint8Array, paste: boolean) => void,
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
