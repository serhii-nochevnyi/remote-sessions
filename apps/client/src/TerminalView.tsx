import { useEffect, useRef, useState } from "react";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import { bindTerminalProtocol } from "./terminal-protocol";
import { localClient, TerminalViewScope } from "./local-client";
import type { GeometryEvent, TerminalSize, TerminalStatus, TerminalStreamFailure } from "./local-client";
import { GeometryBoundaryError, OrderedGeometry, ResizeProposals } from "./terminal-geometry";
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

export function TerminalView({ runtimeId, epoch, label, fontSize }: { runtimeId: string; epoch: number; label: string; fontSize: number }) {
  const element = useRef<HTMLDivElement>(null);
  const font = useRef(fontSize);
  const updateFont = useRef<((size: number) => void) | null>(null);
  const reconnectDelay = useRef(500);
  const [status, setStatus] = useState("Підключення…");
  const [retry, setRetry] = useState(0);
  useEffect(() => {
    font.current = fontSize;
    updateFont.current?.(fontSize);
  }, [fontSize]);
  useEffect(() => {
    const mountedElement = element.current;
    if (!mountedElement) return;
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
    let finishWrite: (() => void) | undefined;
    let reconnect: ReturnType<typeof setTimeout> | undefined;
    let frame = 0;
    let layoutReady = false;
    const detach = (view: string, failure?: TerminalStreamFailure) => localClient.detach(view, failure).catch(() => undefined);

    function report() {
      if (!routing || !scope.viewId || scope.failedInput) return;
      const control = !protocol.inputAvailable ? "ввід недоступний" : scope.canWrite ? "ви керуєте" : "режим перегляду";
      setStatus(`Наживо · ${control} · ${geometryText(routing.status)}${resizeUnavailable ? " · зміна розміру недоступна" : ""}`);
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
      input: localClient.input,
      inputFailure(error) {
        proposals.close();
        protocol.pauseInput();
        setStatus(error === undefined
          ? "Ввід завеликий. Перепідключіть термінал перед продовженням."
          : `Ввід перервано; автоматично не повторюємо. ${String(error)}`);
      },
      retire(view) {
        container.removeAttribute("data-terminal-attached");
        protocol.dispose();
        updateFont.current = null;
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
        reconnect = setTimeout(() => setRetry(value => value + 1), reconnectDelay.current);
        reconnectDelay.current = Math.min(reconnectDelay.current * 2, 4000);
      },
    });
    const protocol = bindTerminalProtocol(terminal, (bytes, paste) => { void scope.send(bytes, paste); });
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
        if (attachment.status.terminal.target.runtime_id !== runtimeId) throw Error("Сесія змінилася");
        const ordered = new OrderedGeometry(attachment.status);
        routing = ordered;
        terminal.resize(attachment.cols, attachment.rows);
        container.setAttribute("data-terminal-attached", "");
        layoutSoon();
        while (scope.owns(view)) {
          streamStage = "read_response";
          const chunk = await localClient.read(view);
          if (!scope.owns(view)) return;
          if (chunk.closure) {
            failure = "stream_closed";
            scope.acceptClosure(view, chunk.closure);
            return;
          }
          streamStage = "geometry_routing";
          if (!await protocol.consume(scope, view, chunk, () => ordered.consume(chunk, sink))) return;
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
        failure = streamFailure(error, streamStage);
        scope.retire({ error });
      }
    }
    void stream();
    return () => {
      clearTimeout(reconnect);
      scope.retire();
    };
  }, [runtimeId, epoch, retry]);
  return <><div className="stream-status" role="status"><span>{status}</span><button type="button" onClick={() => {
    element.current?.focus();
    setRetry(value => value + 1);
  }}>Перепідключити</button></div>
    <div className="terminal-view"><div className="live-terminal" ref={element} role="region" aria-label={label} tabIndex={0} /></div></>;
}
