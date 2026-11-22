import { useEffect, useRef, useState } from "react";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import { suppressViewerQueries } from "./terminal-protocol";
import { localClient } from "./local-client";
import type { GeometryEvent, TerminalSize, TerminalStatus } from "./local-client";
import { OrderedGeometry, ResizeProposals } from "./terminal-geometry";
import "@xterm/xterm/css/xterm.css";

function geometryText(status: TerminalStatus): string {
  const { cols, rows } = status.committed.size;
  const provider = status.provider;
  const measured = provider ? `розмір процесу ${provider.size.cols}×${provider.size.rows}` : "розмір процесу недоступний";
  const pending = status.observation === "pending" ? " · зміна триває" : "";
  return `Сітка ${cols}×${rows} · ${measured}${pending}`;
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
    const container = element.current;
    if (!container) return;
    const rendererId = crypto.randomUUID();
    const terminal = new Terminal({ disableStdin: true, fontSize: font.current, lineHeight: 1.25,
      fontFamily: "Menlo, Monaco, monospace", screenReaderMode: true, scrollback: 2000,
      theme: { background: "#111a21", foreground: "#d9e6e2" } });
    const fit = new FitAddon();
    const queryHandlers = suppressViewerQueries(terminal);
    terminal.loadAddon(fit);
    terminal.open(container);
    if (document.activeElement === container) terminal.focus();

    const proposals = new ResizeProposals();
    let routing: OrderedGeometry | undefined;
    let disposed = false;
    let currentView: string | null = null;
    let writable = false;
    let inputFailed = false;
    let resizeUnavailable = false;
    let pendingBytes = 0;
    let inputChain = Promise.resolve();
    let finishWrite: (() => void) | undefined;
    let reconnect: ReturnType<typeof setTimeout> | undefined;
    let frame = 0;
    let layoutReady = false;
    const detach = (view: string) => localClient.detach(view).catch(() => undefined);

    function report() {
      if (!routing || disposed || !currentView || inputFailed) return;
      const control = writable ? "ви керуєте" : "режим перегляду";
      setStatus(`Наживо · ${control} · ${geometryText(routing.status)}${resizeUnavailable ? " · зміна розміру недоступна" : ""}`);
    }

    async function propose() {
      if (disposed || !currentView || !writable || inputFailed || !layoutReady) return;
      const proposal = proposals.take();
      if (!proposal) return;
      try {
        const result = await localClient.resize(proposal.terminal.view_id, proposal.epoch, proposal.size);
        if (disposed || currentView !== proposal.terminal.view_id) return;
        if (proposals.complete(proposal, result)) {
          resizeUnavailable = result.result !== "committed";
          report();
        }
      } catch {
        if (!disposed && currentView === proposal.terminal.view_id && proposals.complete(proposal)) {
          resizeUnavailable = true;
          report();
        }
      } finally {
        if (!disposed && currentView) void propose();
      }
    }

    function layout() {
      if (!routing || disposed || !currentView || !writable || inputFailed || !container!.clientWidth || !container!.clientHeight) return;
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

    function send(data: Uint8Array, paste = false) {
      const view = currentView;
      if (!view || !writable || disposed || inputFailed) return;
      if (pendingBytes + data.length > 65536) {
        proposals.close();
        inputFailed = true; terminal.options.disableStdin = true;
        setStatus("Ввід завеликий. Перепідключіть термінал перед продовженням."); return;
      }
      pendingBytes += data.length;
      inputChain = inputChain.then(async () => {
        try {
          for (let offset = 0; offset < data.length;) {
            if (disposed || inputFailed || currentView !== view) return;
            let end = Math.min(offset + 4096, data.length);
            if (paste && end < data.length) {
              while (end > offset && ((data[end] ?? 0) & 0xc0) === 0x80) end--;
            }
            await localClient.input(view, Array.from(data.subarray(offset, end)), paste);
            offset = end;
          }
        } catch (error) {
          if (!disposed && currentView === view) {
            inputFailed = true;
            proposals.close();
            terminal.options.disableStdin = true;
            setStatus(`Ввід перервано; автоматично не повторюємо. ${String(error)}`);
          }
        } finally { pendingBytes -= data.length; }
      });
    }

    const input = terminal.onData(data => {
      const paste = data.startsWith("\x1b[200~") && data.endsWith("\x1b[201~");
      send(new TextEncoder().encode(paste ? data.slice(6, -6) : data), paste);
    });
    const binaryInput = terminal.onBinary(data => send(Uint8Array.from(data, char => char.charCodeAt(0))));
    const sink = {
      write: (data: readonly number[]) => new Promise<void>((resolve, reject) => {
        const timer = setTimeout(() => {
          finishWrite = undefined;
          reject(Error("Не вдалося обробити вивід термінала"));
        }, 1500);
        const parsed = () => {
          clearTimeout(timer);
          finishWrite = undefined;
          resolve();
        };
        finishWrite = parsed;
        terminal.write(Uint8Array.from(data), parsed);
      }),
      resize: (size: TerminalSize) => {
        terminal.resize(size.cols, size.rows);
        return { cols: terminal.cols, rows: terminal.rows };
      },
      acknowledge: (event: GeometryEvent, size: TerminalSize) =>
        localClient.acknowledgeGrid(event.status.terminal.view_id, {
          renderer_id: rendererId,
          epoch: event.status.epoch,
          revision: event.status.committed.revision,
          size,
          focused: terminal.textarea === document.activeElement,
        }),
    };
    async function stream() {
      try {
        setStatus("Підключення…");
        const attachment = await localClient.attach(runtimeId);
        const view = attachment.view_id;
        if (disposed) { await detach(view); return; }
        currentView = view;
        if (attachment.status.terminal.target.runtime_id !== runtimeId) throw Error("Сесія змінилася");
        routing = new OrderedGeometry(attachment.status);
        terminal.resize(attachment.cols, attachment.rows);
        layoutSoon();
        while (!disposed && currentView === view) {
          const chunk = await localClient.read(view);
          if (disposed || currentView !== view) return;
          await routing.consume(chunk, sink);
          if (disposed || currentView !== view) return;
          writable = chunk.writable && !inputFailed;
          terminal.options.disableStdin = !writable;
          if (proposals.observe(routing.status, writable, routing.applied)) {
            resizeUnavailable = false;
            layoutSoon();
          }
          if (chunk.geometry) resizeUnavailable = false;
          reconnectDelay.current = 500;
          report();
          void propose();
        }
      } catch (error) {
        if (disposed) return;
        const previous = currentView;
        currentView = null;
        writable = false;
        terminal.options.disableStdin = true;
        routing?.close();
        proposals.close();
        if (previous) void detach(previous);
        setStatus(`Перепідключення… ${String(error)}`);
        reconnect = setTimeout(() => setRetry(value => value + 1), reconnectDelay.current);
        reconnectDelay.current = Math.min(reconnectDelay.current * 2, 4000);
      }
    }
    void stream();
    return () => {
      disposed = true;
      updateFont.current = null;
      clearTimeout(reconnect);
      cancelAnimationFrame(frame);
      routing?.close();
      proposals.close();
      finishWrite?.();
      input.dispose();
      binaryInput.dispose();
      observer.disconnect();
      if (currentView) void detach(currentView);
      queryHandlers.forEach(handler => handler.dispose());
      if (terminal.textarea === document.activeElement && container.isConnected) container.focus();
      terminal.dispose();
    };
  }, [runtimeId, epoch, retry]);
  return <><div className="stream-status" role="status"><span>{status}</span><button type="button" onClick={() => {
    element.current?.focus();
    setRetry(value => value + 1);
  }}>Перепідключити</button></div>
    <div className="terminal-view"><div className="live-terminal" ref={element} role="region" aria-label={label} tabIndex={0} /></div></>;
}
