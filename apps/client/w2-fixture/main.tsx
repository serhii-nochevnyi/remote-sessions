import * as React from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import manifest from "./cases.json" with { type: "json" };
import { fixtureCaseId, registerRecorder, type Snapshot } from "./recorder.ts";

const transcript = [
  "\x1b[?2004l\x1b[1;36mOffline terminal fixture\x1b[0m",
  ...Array.from({ length: 24 }, (_, index) => `History line ${index + 1} · Україна`),
  "Selection target: COPY-Ї",
  "> ",
].join("\r\n");

const emptyHex = "(empty)";

function Fixture() {
  const viewport = React.useRef<HTMLDivElement>(null);
  const controls = React.useRef<{ terminal: Terminal; recorder: ReturnType<typeof registerRecorder> }>(null);
  const [caseId, setCaseId] = React.useState(fixtureCaseId);
  const selected = manifest.cases.find(entry => entry.id === caseId)!;
  const [ready, setReady] = React.useState(false);
  const [active, setActive] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState("");
  const [snapshot, setSnapshot] = React.useState<Snapshot | "unavailable" | null>(null);

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
      setError(`Invalid attempt: ${String(reason)}`);
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

  function selectCase(event: React.ChangeEvent<HTMLSelectElement>) {
    if (controls.current) controls.current.terminal.options.disableStdin = true;
    setCaseId(event.currentTarget.value);
    setActive(false);
    setSnapshot(null);
    setError("");
  }

  async function start() {
    const mounted = controls.current;
    if (!mounted || !ready || busy) return;
    setBusy(true);
    setActive(false);
    setError("");
    setSnapshot(null);
    mounted.terminal.options.disableStdin = true;
    mounted.terminal.scrollToBottom();
    mounted.terminal.focus();
    try {
      await mounted.recorder.start(selected.id);
      if (controls.current !== mounted) return;
      mounted.terminal.options.disableStdin = false;
      setActive(true);
    } catch (reason) {
      if (controls.current === mounted) setError(`Invalid attempt: ${String(reason)}`);
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
    } catch {
      if (controls.current === mounted) setSnapshot("unavailable");
    } finally {
      if (controls.current === mounted) setBusy(false);
    }
  }

  const readout = snapshot === "unavailable" ? "Unavailable" : snapshot
    ? snapshot.bytes.map(byte => byte.toString(16).padStart(2, "0")).join("") || emptyHex
    : "—";
  return (
    <main>
      <h1>Offline terminal fixture</h1>
      <label htmlFor="case">Interaction case</label>{" "}
      <select id="case" value={caseId} onChange={selectCase} disabled={busy}>
        {manifest.cases.map(entry => <option key={entry.id} value={entry.id}>{entry.title}</option>)}
      </select>
      <button type="button" onClick={start} disabled={!ready || busy}>{active || error ? "Restart" : "Start"}</button>
      <button type="button" onClick={read} disabled={!active || busy}>Read</button>
      {selected.pasteText !== undefined && (
        <button type="button" disabled={!active || busy || Boolean(error)} onClick={() => {
          const terminal = controls.current?.terminal;
          terminal?.focus();
          terminal?.paste(selected.pasteText);
        }}>Paste sample</button>
      )}
      <div id="terminal" ref={viewport} aria-label="Synthetic terminal" />
      <label htmlFor="snapshot">Native snapshot (hex)</label>{" "}
      <output id="snapshot">{readout}</output>
      <section aria-label="Case instructions">
        <p>Start a fresh attempt, follow the steps, then select Read to compare the captured and expected bytes.</p>
        <ol id="case-actions">{selected.actions.map(action => <li key={action}>{action}</li>)}</ol>
        <ul id="case-observations" aria-label="Required observations">
          {selected.observations.map(observation => <li key={observation}>{observation}</li>)}
        </ul>
        <dl>
          <dt>Expected before action (hex)</dt>
          <dd><output id="expected-before">{selected.expected.beforeHex || emptyHex}</output></dd>
          <dt>Expected after action (hex)</dt>
          <dd><output id="expected-after">{selected.expected.afterHex || emptyHex}</output></dd>
        </dl>
      </section>
      {error && <p role="alert">{error}</p>}
    </main>
  );
}

const root = document.getElementById("root");
if (!root) throw new Error("Missing fixture root");
createRoot(root).render(<Fixture />);
