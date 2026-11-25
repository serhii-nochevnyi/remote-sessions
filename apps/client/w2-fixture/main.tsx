/// <reference types="vite/client" />

import * as React from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { fixtureCaseId, registerRecorder, type Snapshot } from "./recorder.ts";

const transcript = "\x1b[1;36mOffline terminal fixture\x1b[0m\r\nSynthetic input only · ї + Return\r\n\r\n> ";

function Fixture() {
  const viewport = React.useRef<HTMLDivElement>(null);
  const controls = React.useRef<{ terminal: Terminal; recorder: ReturnType<typeof registerRecorder> }>(null);
  const [ready, setReady] = React.useState(false);
  const [active, setActive] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState("");
  const [snapshot, setSnapshot] = React.useState<Snapshot | null>(null);

  React.useEffect(() => {
    const container = viewport.current;
    if (!container) return;
    const terminal = new Terminal({
      cols: 36,
      rows: 8,
      disableStdin: true,
      fontSize: 13,
      lineHeight: 1.25,
      fontFamily: "Menlo, Monaco, monospace",
      screenReaderMode: true,
      cursorBlink: true,
      scrollback: 100,
      theme: { background: "#111a21", foreground: "#d9e6e2" },
    });
    terminal.open(container);

    const fitDisplay = () => {
      const style = getComputedStyle(container);
      const width = container.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
      const height = container.clientHeight - parseFloat(style.paddingTop) - parseFloat(style.paddingBottom);
      terminal.options.fontSize = Math.min(13, Math.max(9, Math.floor(
        Math.min(width / (terminal.cols * 0.62), height / (terminal.rows * 1.3)),
      )));
    };
    const observer = new ResizeObserver(fitDisplay);
    observer.observe(container);
    fitDisplay();

    function onCaptureError(reason: unknown) {
      terminal.options.disableStdin = true;
      setError(String(reason));
    }
    const recorder = registerRecorder(terminal, invoke, onCaptureError);
    const mounted = { terminal, recorder };
    controls.current = mounted;
    terminal.write(transcript, () => {
      if (controls.current === mounted) setReady(true);
    });

    return () => {
      controls.current = null;
      recorder.dispose();
      observer.disconnect();
      terminal.dispose();
    };
  }, []);

  async function start() {
    const mounted = controls.current;
    if (!mounted || !ready || busy) return;
    setBusy(true);
    setActive(false);
    setError("");
    setSnapshot(null);
    mounted.terminal.options.disableStdin = true;
    // Focus inside the tap handler so iOS can open the software keyboard.
    mounted.terminal.focus();
    try {
      await mounted.recorder.start(fixtureCaseId);
      if (controls.current !== mounted) return;
      mounted.terminal.options.disableStdin = false;
      setActive(true);
    } catch (reason) {
      if (controls.current === mounted) setError(String(reason));
    } finally {
      if (controls.current === mounted) setBusy(false);
    }
  }

  async function read() {
    const mounted = controls.current;
    if (!mounted || !active || busy) return;
    setBusy(true);
    try {
      const result = await mounted.recorder.read();
      if (controls.current === mounted) setSnapshot(result);
    } catch (reason) {
      if (controls.current === mounted) setError(String(reason));
    } finally {
      if (controls.current === mounted) setBusy(false);
    }
  }

  const hex = snapshot?.bytes.map(byte => byte.toString(16).padStart(2, "0")).join("");
  return (
    <main>
      <h1>Offline terminal fixture</h1>
      <p>Start the case, type ї, then press Return. Select Read to inspect the captured bytes.</p>
      <button type="button" onClick={start} disabled={!ready || busy}>Start</button>
      <button type="button" onClick={read} disabled={!active || busy}>Read</button>
      <div id="terminal" ref={viewport} aria-label="Synthetic terminal" />
      <label htmlFor="snapshot">Native snapshot (hex)</label>{" "}
      <output id="snapshot">{snapshot ? hex || "(empty)" : "—"}</output>
      {error && <p role="alert">{error}</p>}
    </main>
  );
}

const root = document.getElementById("root");
if (!root) throw new Error("Missing fixture root");
createRoot(root).render(<Fixture />);
