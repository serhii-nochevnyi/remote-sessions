import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import { bindTerminalPaste, bindTerminalProtocol } from "./terminal-protocol";
import { localClient, TerminalViewScope } from "./local-client";
import type { GeometryEvent, RuntimeInfo, TerminalSize, TerminalStatus, TerminalStreamFailure } from "./local-client";
import type { BoundTerminal } from "./session-state";
import { GeometryBoundaryError, OrderedGeometry, ResizeProposals, sameTerminal } from "./terminal-geometry";
import "@xterm/xterm/css/xterm.css";

function geometryText(status: TerminalStatus): string {
  const { cols, rows } = status.committed.size;
  const provider = status.provider;
  const measured = provider ? `розмір процесу ${provider.size.cols}×${provider.size.rows}` : "розмір процесу недоступний";
  const pending = status.observation === "pending" ? " · зміна триває" : "";
  return `Сітка ${cols}×${rows} · ${measured}${pending}`;
}

function streamFailure(error: unknown, stage: TerminalStreamFailure): TerminalStreamFailure {
  if (error instanceof GeometryBoundaryError) return error.reason;
  if (!(error instanceof Error)) return stage;
  switch (error.message) {
    case "Потік перервано": return "stream_closed";
    case "Стан термінала змінився": return "terminal_status_changed";
    case "Розміри відображення не збігаються": return "renderer_grid_mismatch";
    case "Підтвердження розміру застаріло": return "stale_grid_ack";
    case "Не вдалося обробити вивід термінала": return "renderer_write_timeout";
    default: return stage;
  }
}

interface TerminalViewProps {
  readonly runtimeId: string;
  readonly epoch: number;
  readonly label: string;
  readonly fontSize: number;
  readonly runtime: RuntimeInfo | undefined;
  readonly previousBinding: BoundTerminal | null;
  readonly allowWriter: boolean;
  readonly onBinding: (binding: BoundTerminal, state: "bound" | "retired" | "changed") => void;
}

export function TerminalView({ runtimeId, epoch, label, fontSize, runtime, previousBinding, allowWriter, onBinding }: TerminalViewProps) {
  const element = useRef<HTMLDivElement>(null);
  const font = useRef(fontSize);
  const updateFont = useRef<((size: number) => void) | null>(null);
  const reconnectDelay = useRef(500);
  const runtimeRef = useRef(runtime);
  const previousRef = useRef(previousBinding);
  const writable = useRef(allowWriter);
  const updateEligibility = useRef<((eligible: boolean) => void) | null>(null);
  const [status, setStatus] = useState("Підключення…");
  const [retry, setRetry] = useState(0);
  useLayoutEffect(() => {
    runtimeRef.current = runtime;
    previousRef.current = previousBinding;
  }, [runtime, previousBinding]);
  useLayoutEffect(() => {
    if (allowWriter && !writable.current && !updateEligibility.current) setRetry(value => value + 1);
    writable.current = allowWriter;
    updateEligibility.current?.(allowWriter);
    return () => {
      writable.current = false;
      updateEligibility.current?.(false);
    };
  }, [allowWriter]);
  useEffect(() => {
    font.current = fontSize;
    updateFont.current?.(fontSize);
  }, [fontSize]);
  useEffect(() => {
    const mountedElement = element.current;
    if (!mountedElement || !writable.current) return;
    const container = mountedElement;
    const rendererId = crypto.randomUUID();
    const terminal = new Terminal({ disableStdin: true, fontSize: font.current, lineHeight: 1.25,
      fontFamily: "Menlo, Monaco, monospace", screenReaderMode: true, scrollback: 2000,
      theme: { background: "#111a21", foreground: "#d9e6e2" } });
    const fit = new FitAddon();
    terminal.loadAddon(fit);
    terminal.open(container);
    if (document.activeElement === container) terminal.focus();

    const proposals = new ResizeProposals();
    let routing: OrderedGeometry | undefined;
    let resizeUnavailable = false;
    let inputIssue: string | undefined;
    let finishWrite: (() => void) | undefined;
    let reconnect: ReturnType<typeof setTimeout> | undefined;
    let frame = 0;
    let layoutReady = false;
    let binding: BoundTerminal | undefined;
    let identityChanged = false;
    const detach = (view: string, failure?: TerminalStreamFailure) => localClient.detach(view, failure).catch(() => undefined);

    function report() {
      if (!routing || !scope.viewId) return;
      const paste = scope.pasteMessage;
      if (inputIssue) {
        setStatus(paste ? `${inputIssue} · ${paste}` : inputIssue);
        return;
      }
      const control = !protocol.inputAvailable ? "ввід недоступний" : scope.canWrite ? "ви керуєте" : "режим перегляду";
      setStatus(`Наживо · ${control} · ${geometryText(routing.status)}${resizeUnavailable ? " · зміна розміру недоступна" : ""}${paste ? ` · ${paste}` : ""}`);
    }

    async function propose() {
      if (!scope.canWrite || !layoutReady) return;
      const proposal = proposals.take();
      if (!proposal) return;
      try {
        const result = await localClient.resize(proposal.terminal.view_id, proposal.epoch, proposal.size);
        if (!scope.owns(proposal.terminal.view_id)) return;
        if (proposals.complete(proposal, result)) {
          resizeUnavailable = result.result !== "committed";
          report();
        }
      } catch {
        if (scope.owns(proposal.terminal.view_id) && proposals.complete(proposal)) {
          resizeUnavailable = true;
          report();
        }
      } finally {
        if (scope.viewId) void propose();
      }
    }

    function layout() {
      if (!routing || !scope.canWrite || !container.clientWidth || !container.clientHeight) return;
      const size = fit.proposeDimensions();
      if (!size || !Number.isFinite(size.cols) || !Number.isFinite(size.rows)) return;
      proposals.offer({ cols: Math.min(500, Math.max(2, size.cols)), rows: Math.min(200, Math.max(2, size.rows)) });
      layoutReady = true;
      void propose();
    }

    function layoutSoon() {
      layoutReady = false;
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(layout);
    }

    updateFont.current = size => {
      terminal.options.fontSize = size;
      layoutSoon();
    };
    const observer = new ResizeObserver(layoutSoon);
    observer.observe(container);

    let streamStage: TerminalStreamFailure = "attachment_setup";
    let failure: TerminalStreamFailure | undefined;
    const scope = new TerminalViewScope({
      input: (viewId, data, incarnation) => localClient.input({ viewId, data, incarnation }),
      paste: {
        stage: localClient.pasteStage,
        commit: localClient.pasteCommit,
        ack: localClient.pasteAck,
        changed() {
          if (!scope.canWrite) protocol.pauseInput();
          report();
        },
      },
      inputFailure(error) {
        proposals.close();
        protocol.pauseInput();
        inputIssue = error === undefined
          ? "Ввід завеликий. Перепідключіть термінал перед продовженням."
          : `Ввід перервано; автоматично не повторюємо. ${String(error)}`;
        report();
      },
      retire(view) {
        if (binding) onBinding(binding, identityChanged ? "changed" : "retired");
        container.removeAttribute("data-terminal-attached");
        protocol.dispose();
        clipboard.dispose();
        updateFont.current = null;
        updateEligibility.current = null;
        cancelAnimationFrame(frame);
        routing?.close();
        proposals.close();
        finishWrite?.();
        observer.disconnect();
        if (view) void detach(view, failure);
        if (terminal.textarea === document.activeElement && container.isConnected) container.focus();
        terminal.dispose();
      },
      reconnect(error) {
        setStatus(`Перепідключення… ${String(error)}`);
        if (identityChanged || !writable.current) return;
        reconnect = setTimeout(() => {
          if (writable.current) setRetry(value => value + 1);
        }, reconnectDelay.current);
        reconnectDelay.current = Math.min(reconnectDelay.current * 2, 4000);
      },
    });
    const protocol = bindTerminalProtocol(terminal, bytes => { void scope.send(bytes); });
    const clipboard = bindTerminalPaste(container, bytes => { void scope.paste(bytes); });
    updateEligibility.current = eligible => {
      scope.setInputEligible(eligible);
      layoutReady = false;
      protocol.pauseInput();
      if (routing) proposals.observe(routing.status, false, routing.applied);
      report();
    };
    const sink = {
      write: (data: readonly number[]) => new Promise<void>((resolve, reject) => {
        const timer = setTimeout(() => {
          if (finishWrite === parsed) finishWrite = undefined;
          reject(Error("Не вдалося обробити вивід термінала"));
        }, 1500);
        const parsed = () => {
          clearTimeout(timer);
          if (finishWrite === parsed) finishWrite = undefined;
          resolve();
        };
        finishWrite = parsed;
        terminal.write(Uint8Array.from(data), parsed);
      }),
      resize: (size: TerminalSize) => {
        terminal.resize(size.cols, size.rows);
        return { cols: terminal.cols, rows: terminal.rows };
      },
      acknowledge: (event: GeometryEvent, size: TerminalSize) => {
        streamStage = "grid_ack_response";
        return localClient.acknowledgeGrid(event.status.terminal.view_id, {
          renderer_id: rendererId,
          epoch: event.status.epoch,
          revision: event.status.committed.revision,
          size,
          focused: terminal.textarea === document.activeElement,
        });
      },
    };
    async function stream() {
      try {
        setStatus("Підключення…");
        const attachment = await localClient.attach(runtimeId);
        const view = attachment.view_id;
        if (!scope.bind(view)) { await detach(view); return; }
        const terminalScope = attachment.status.terminal;
        binding = { agentPid: epoch, terminal: terminalScope };
        const target = terminalScope.target;
        const current = runtimeRef.current;
        const previous = previousRef.current;
        const sameTarget = !previous || sameTerminal({
          ...previous.terminal, view_id: view,
          agent_epoch: previous.agentPid === epoch ? previous.terminal.agent_epoch : terminalScope.agent_epoch,
        }, terminalScope);
        if (!writable.current || !current || target.runtime_id !== runtimeId || current.pane_pid !== target.pid ||
            (current.pane_id && current.pane_id !== target.pane) || !sameTarget) {
          identityChanged = true;
          throw Error("Сесія змінилася");
        }
        const ordered = new OrderedGeometry(attachment.status);
        routing = ordered;
        terminal.resize(attachment.cols, attachment.rows);
        container.setAttribute("data-terminal-attached", "");
        onBinding(binding, "bound");
        layoutSoon();
        while (scope.owns(view)) {
          streamStage = "read_response";
          const checkEligibility = scope.captureReadEligibility();
          const chunk = await localClient.read(view);
          if (!scope.owns(view)) return;
          if (chunk.closure) {
            failure = "stream_closed";
            scope.acceptClosure(view, chunk.closure);
            return;
          }
          streamStage = "geometry_routing";
          if ((chunk.status && !sameTerminal(terminalScope, chunk.status.terminal)) ||
              (chunk.geometry && !sameTerminal(terminalScope, chunk.geometry.status.terminal))) {
            identityChanged = true;
            throw Error("Стан термінала змінився");
          }
          if (!await protocol.consume(scope, view, checkEligibility(chunk), () => ordered.consume(chunk, sink))) return;
          if (proposals.observe(ordered.status, scope.canWrite, ordered.applied)) {
            resizeUnavailable = false;
            layoutSoon();
          }
          if (chunk.geometry) resizeUnavailable = false;
          reconnectDelay.current = 500;
          report();
          void propose();
        }
      } catch (error) {
        if (!scope.active) return;
        if (String(error) === "Terminal attachment identity changed") identityChanged = true;
        failure = streamFailure(error, streamStage);
        scope.retire({ error });
      }
    }
    void stream();
    return () => {
      clearTimeout(reconnect);
      scope.retire();
    };
  }, [runtimeId, epoch, retry, onBinding]);
  return <><div className="stream-status" role="status"><span>{status}</span><button type="button" disabled={!allowWriter} onClick={() => {
    element.current?.focus();
    setRetry(value => value + 1);
  }}>Перепідключити</button></div>
    <div className="terminal-view"><div className="live-terminal" ref={element} role="region" aria-label={label} tabIndex={0} /></div></>;
}
