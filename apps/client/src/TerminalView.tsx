import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { suppressViewerQueries } from "./terminal-protocol";
import { localClient } from "./local-client";
import "@xterm/xterm/css/xterm.css";

export function TerminalView({ runtimeId, epoch, label }: { runtimeId: string; epoch: number; label: string }) {
  const element = useRef<HTMLDivElement>(null);
  const [status, setStatus] = useState("Підключення…");
  const [retry, setRetry] = useState(0);
  useEffect(() => {
    if (!element.current) return;
    const terminal = new Terminal({ disableStdin: true, fontSize: 13, lineHeight: 1.25,
      fontFamily: "Menlo, Monaco, monospace", screenReaderMode: true, scrollback: 2000,
      theme: { background: "#111a21", foreground: "#d9e6e2" } });
    const queryHandlers = suppressViewerQueries(terminal);
    terminal.open(element.current);
    const container = element.current;
    function fitDisplay() {
      const style = getComputedStyle(container);
      const width = container.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
      const height = container.clientHeight - parseFloat(style.paddingTop) - parseFloat(style.paddingBottom);
      terminal.options.fontSize = Math.min(13, Math.max(9, Math.floor(Math.min(width / (terminal.cols * 0.62), height / (terminal.rows * 1.3)))));
    }
    const observer = new ResizeObserver(fitDisplay);
    observer.observe(container);
    // Native input is leased by the host. Do not enable OSC clipboard or link handlers.
    let disposed = false;
    let currentView: string | null = null;
    let writable = false;
    let inputFailed = false;
    let pendingBytes = 0;
    let inputChain = Promise.resolve();
    let finishWrite: (() => void) | undefined;
    function send(data: Uint8Array, paste = false) {
      const view = currentView;
      if (!view || !writable || disposed || inputFailed) return;
      if (pendingBytes + data.length > 65536) {
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
          inputFailed = true; terminal.options.disableStdin = true;
          if (!disposed) setStatus(`Ввід перервано; автоматично не повторюємо. ${String(error)}`);
        } finally { pendingBytes -= data.length; }
      });
    }
    const input = terminal.onData(data => {
      const paste = data.startsWith("\x1b[200~") && data.endsWith("\x1b[201~");
      send(new TextEncoder().encode(paste ? data.slice(6, -6) : data), paste);
    });
    const binaryInput = terminal.onBinary(data => send(Uint8Array.from(data, char => char.charCodeAt(0))));
    let timer: ReturnType<typeof setTimeout> | undefined;
    let wake: (() => void) | undefined;
    const detach = (view: string) => localClient.detach(view).catch(() => undefined);
    const pause = (ms: number) => new Promise<void>(resolve => { wake = resolve; timer = setTimeout(resolve, ms); });
    async function stream() {
      let backoff = 500;
      while (!disposed) {
        let ownedView: string | null = null;
        try {
          setStatus("Підключення…");
          const attachment = await localClient.attach(runtimeId);
          ownedView = attachment.view_id;
          if (disposed) { await detach(ownedView); return; }
          currentView = ownedView;
          terminal.reset();
          terminal.resize(attachment.cols, attachment.rows);
          fitDisplay();
          setStatus("Наживо · отримуємо керування…");
          backoff = 500;
          while (!disposed) {
            const chunk = await localClient.read(ownedView);
            if (disposed || currentView !== ownedView) return;
            if (chunk.closed) throw Error("Потік перервано");
            const gainedControl = chunk.writable && !writable;
            writable = chunk.writable;
            terminal.options.disableStdin = !writable || inputFailed;
            if (!inputFailed) setStatus(writable ? "Наживо · ви керуєте · натисніть у терміналі для вводу" : "Наживо · інший клієнт керує · очікуємо звільнення");
            if (gainedControl && !inputFailed) terminal.focus();
            if (chunk.data.length) {
              // Await parsing before pulling another bounded native chunk.
              await new Promise<void>(resolve => { finishWrite = resolve; terminal.write(Uint8Array.from(chunk.data), resolve); });
            }
          }
        } catch (error) {
          writable = false; terminal.options.disableStdin = true;
          if (!disposed) setStatus(`Перепідключення… ${String(error)}`);
        } finally {
          if (ownedView) await detach(ownedView);
          if (currentView === ownedView) currentView = null;
        }
        if (!disposed) { await pause(backoff); backoff = Math.min(backoff * 2, 4000); }
      }
    }
    void stream();
    return () => {
      disposed = true;
      clearTimeout(timer); wake?.(); finishWrite?.();
      input.dispose(); binaryInput.dispose(); observer.disconnect();
      if (currentView) void detach(currentView);
      queryHandlers.forEach(handler => handler.dispose());
      terminal.dispose();
    };
  }, [runtimeId, epoch, retry]);
  return <><div className="stream-status" role="status"><span>{status}</span><button onClick={() => setRetry(value => value + 1)}>Перепідключити</button></div><div className="terminal-view live-terminal" ref={element} aria-label={label} /></>;
}
