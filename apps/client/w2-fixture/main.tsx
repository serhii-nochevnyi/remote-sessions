import * as React from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import "./fixture.css";
import manifest from "./cases.json" with { type: "json" };
import { observeTerminalLayout, type LayoutEvent } from "./terminal-layout.ts";
import { fixtureCaseId, registerRecorder, type Invoke, type Snapshot } from "./recorder.ts";

const transcript = [
  "\x1b[?2004l\x1b[1;36mOffline terminal fixture\x1b[0m",
  ...Array.from({ length: 24 }, (_, index) => `History line ${index + 1} · Україна`),
  "Selection target: COPY-Ї",
  "> ",
].join("\r\n");

const emptyHex = "(empty)";

const checkpointInterval = 250;
const traceLimit = 96;

type Case = typeof manifest.cases[number];
type TraceEvent = {
  sequence: number;
  event: string;
  elapsedMs: number;
  bytes?: number;
  nativeSequence?: number;
};

type Attempt = {
  id: number;
  selected: Case;
  generation: number;
  startedAt: number;
  composition: boolean;
  phase: "waiting" | "active" | "ended";
  preedit: boolean;
  held: boolean;
  admitted: boolean;
  pending: number;
  acknowledged: boolean;
  trace: TraceEvent[];
  checkpoint?: { sequence: number; snapshot: Snapshot };
};

function hex(bytes: readonly number[]) {
  return bytes.map(byte => byte.toString(16).padStart(2, "0")).join("");
}

function mountTerminal(
  host: HTMLDivElement,
  publishLayout: (events: readonly LayoutEvent[]) => void,
  publishAttempt: (attempt: Attempt | null) => void,
  publishError: (error: string) => void,
) {
  const document = host.ownerDocument;
  const terminal = new Terminal({
    cols: 36,
    rows: 8,
    disableStdin: true,
    fontSize: 13,
    lineHeight: 1.25,
    fontFamily: "Menlo, Monaco, monospace",
    cursorBlink: true,
    scrollback: 100,
    theme: { background: "#111a21", foreground: "#d9e6e2" },
  });
  const fit = new FitAddon();
  terminal.loadAddon(fit);
  terminal.open(host);
  const textarea = terminal.textarea!;
  const stopLayout = observeTerminalLayout(host, textarea, fit, publishLayout);
  let current: Attempt | null = null;
  let nextId = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let checkpointRead: Promise<Snapshot> | undefined;
  let checkpointContext: { attempt: Attempt; epoch: number } | undefined;
  let pollEpoch = 0;
  let disposed = false;
  let composing = false;
  let settling: ReturnType<typeof setTimeout> | undefined;

  function publish(attempt: Attempt) {
    if (current === attempt) publishAttempt({ ...attempt, trace: [...attempt.trace] });
  }

  function stopPolling() {
    pollEpoch++;
    clearTimeout(timer);
    timer = undefined;
  }

  function hold(attempt: Attempt, reason: string) {
    if (current !== attempt || attempt.held) return;
    attempt.held = true;
    stopPolling();
    publishError(`Held: ${reason}`);
    publish(attempt);
  }

  function record(attempt: Attempt, event: string, bytes?: number, nativeSequence?: number) {
    if (attempt.trace.length === traceLimit) {
      hold(attempt, "Composition trace capacity exceeded");
      return;
    }
    const sequence = attempt.trace.length + 1;
    attempt.trace.push({
      sequence,
      event,
      elapsedMs: performance.now() - attempt.startedAt,
      ...(bytes === undefined ? {} : { bytes }),
      ...(nativeSequence === undefined ? {} : { nativeSequence }),
    });
    publish(attempt);
    return sequence;
  }

  function eligible(attempt: Attempt) {
    return current === attempt && attempt.phase === "active" && attempt.preedit && !attempt.held
      && document.activeElement === textarea && document.visibilityState === "visible";
  }

  function poll(attempt: Attempt) {
    if (!eligible(attempt) || timer !== undefined) return;
    timer = setTimeout(async () => {
      timer = undefined;
      if (!eligible(attempt)) return;
      poll(attempt);
      if (checkpointRead) return;
      const epoch = pollEpoch;
      checkpointContext = { attempt, epoch };
      checkpointRead = recorder.read();
      try {
        const snapshot = await checkpointRead;
        if (!eligible(attempt) || epoch !== pollEpoch) return;
        if (snapshot.generation !== attempt.generation || snapshot.bytes.length
          || snapshot.eventLengths.length || snapshot.nextSequence !== 1 || snapshot.invalid || snapshot.rejected) {
          hold(attempt, "Invalid active checkpoint");
          return;
        }
        const sequence = record(attempt, "active checkpoint", snapshot.bytes.length, snapshot.nextSequence);
        if (sequence !== undefined) {
          attempt.checkpoint = { sequence, snapshot };
          publish(attempt);
        }
      } catch {
        if (eligible(attempt) && epoch === pollEpoch) hold(attempt, "Active checkpoint unavailable");
      } finally {
        checkpointRead = undefined;
        checkpointContext = undefined;
      }
    }, checkpointInterval);
  }

  function compositionStart() {
    const priorCycle = composing || settling !== undefined;
    composing = true;
    const attempt = current;
    if (!attempt) return;
    if (!attempt.composition) {
      if (attempt.pending) record(attempt, "in-flight append uncertain");
      hold(attempt, "Unexpected composition");
      return;
    }
    record(attempt, "start");
    if (priorCycle || attempt.phase !== "waiting" || attempt.pending || textarea.value
      || document.activeElement !== textarea || document.visibilityState !== "visible") {
      hold(attempt, "Invalid composition start");
      return;
    }
    attempt.phase = "active";
  }

  function compositionUpdate(event: CompositionEvent) {
    const attempt = current;
    if (!attempt?.composition) return;
    if (attempt.phase !== "active") {
      hold(attempt, "Unexpected composition update");
      return;
    }
    pollEpoch++;
    attempt.preedit = Boolean(event.data);
    record(attempt, attempt.preedit ? "update" : "empty update");
    if (attempt.preedit) poll(attempt);
    else stopPolling();
  }

  function compositionEnd() {
    composing = false;
    clearTimeout(settling);
    settling = setTimeout(() => {
      settling = setTimeout(() => { settling = undefined; }, 0);
    }, 0);
    const attempt = current;
    if (!attempt?.composition) return;
    stopPolling();
    record(attempt, "end");
    if (!eligible(attempt) || !attempt.checkpoint) hold(attempt, "Composition ended without a valid active checkpoint");
    attempt.phase = "ended";
    attempt.preedit = false;
    publish(attempt);
  }

  function interrupt() {
    if (current?.phase === "active") hold(current, "Composition interrupted");
  }

  function visibilityChanged() {
    if (document.visibilityState !== "visible") interrupt();
  }

  textarea.addEventListener("compositionstart", compositionStart, true);
  textarea.addEventListener("compositionupdate", compositionUpdate, true);
  textarea.addEventListener("compositionend", compositionEnd, true);
  textarea.addEventListener("blur", interrupt, true);
  document.addEventListener("visibilitychange", visibilityChanged);

  const encoder = new TextEncoder();
  const input = {
    onData(deliver: (text: string) => void) {
      return terminal.onData(text => {
        const attempt = current;
        if (!attempt) return;
        if (attempt.composition) {
          if (!text) return;
          const bytes = encoder.encode(text);
          if (attempt.held || attempt.phase !== "ended" || attempt.admitted
            || hex([...bytes]) !== attempt.selected.expected.afterHex) {
            record(attempt, "rejection", bytes.length);
            hold(attempt, "Unexpected composition input");
            return;
          }
          if (record(attempt, "admission", bytes.length) === undefined) return;
          attempt.admitted = true;
        }
        if (!attempt.held) deliver(text);
      });
    },
  };

  const transport: Invoke = async <T,>(command: string, args?: Parameters<Invoke>[1], options?: Parameters<Invoke>[2]) => {
    if (command !== "w2_append_input") {
      const checkpoint = command === "w2_read_attempt" ? checkpointContext : undefined;
      const result = await invoke<T>(command, args, options);
      if (checkpoint && (!eligible(checkpoint.attempt) || checkpoint.epoch !== pollEpoch)) {
        throw new Error("Active checkpoint expired");
      }
      return result;
    }
    const request = args as { generation: number; sequence: number; events: number[][] };
    const attempt = current;
    const requestEvent = request.events[0];
    if (!attempt || request.generation !== attempt.generation || attempt.held
      || (attempt.composition && (attempt.phase !== "ended" || !attempt.admitted || composing
        || request.sequence !== 1 || request.events.length !== 1
        || requestEvent === undefined || hex(requestEvent) !== attempt.selected.expected.afterHex))) {
      throw new Error("Blocked append");
    }
    const bytes = request.events.reduce((total, event) => total + event.length, 0);
    if (attempt.composition && attempt.acknowledged) throw new Error("Composition was already delivered");
    if (attempt.composition && record(attempt, "append dispatch", bytes, request.sequence) === undefined) {
      throw new Error("Composition trace capacity exceeded");
    }
    attempt.pending++;
    try {
      const ack = await invoke<T>(command, args, options);
      if (current === attempt) {
        if (ack === null) {
          attempt.acknowledged = true;
          if (attempt.composition || attempt.held) record(attempt, "append acknowledgment", bytes, request.sequence);
        } else if (attempt.held) record(attempt, "append delivery unconfirmed", bytes, request.sequence);
      }
      return ack;
    } catch (reason) {
      if (attempt.held) record(attempt, "append delivery unconfirmed", bytes, request.sequence);
      throw reason;
    } finally {
      attempt.pending--;
    }
  };

  function onCaptureError(reason: unknown) {
    if (!current) return;
    if (!current.composition) terminal.options.disableStdin = true;
    hold(current, String(reason));
  }

  const recorder = registerRecorder(input, transport, onCaptureError);

  function retire() {
    current = null;
    stopPolling();
    terminal.options.disableStdin = true;
    publishAttempt(null);
  }

  return {
    terminal,
    retire,
    async start(selected: Case) {
      retire();
      terminal.scrollToBottom();
      const id = ++nextId;
      const generation = await recorder.start(selected.id);
      if (disposed || id !== nextId) return false;
      const attempt: Attempt = {
        id, selected, generation, startedAt: performance.now(), composition: selected.id.startsWith("composition-"),
        phase: "waiting", preedit: false, held: false, admitted: false, acknowledged: false, pending: 0, trace: [],
      };
      current = attempt;
      if (composing || settling !== undefined) hold(attempt, "Previous composition is still ending");
      else {
        if (attempt.composition) {
          textarea.value = "";
          textarea.setSelectionRange(0, 0);
        }
        terminal.options.disableStdin = false;
      }
      publish(attempt);
      return true;
    },
    async read() {
      const attempt = current;
      if (!attempt || (attempt.composition && !attempt.held && attempt.phase !== "ended")) return null;
      try {
        if (checkpointRead) await checkpointRead.catch(() => {});
        if (current !== attempt) return null;
        const snapshot = await recorder.read();
        if (current !== attempt) return null;
        if (attempt.composition) {
          const expected = attempt.selected.expected.afterHex;
          if (!attempt.checkpoint || attempt.phase !== "ended" || snapshot.generation !== attempt.generation
            || snapshot.invalid || snapshot.rejected || hex(snapshot.bytes) !== expected
            || attempt.admitted !== Boolean(expected) || attempt.acknowledged !== Boolean(expected)
            || snapshot.nextSequence !== (expected ? 2 : 1)
            || snapshot.eventLengths.length !== (expected ? 1 : 0)) {
            hold(attempt, "Composition result could not be confirmed");
          }
          record(attempt, "final snapshot", snapshot.bytes.length, snapshot.nextSequence);
        }
        return snapshot;
      } catch {
        if (current !== attempt) return null;
        hold(attempt, "Native snapshot unavailable");
        return "unavailable" as const;
      }
    },
    dispose() {
      disposed = true;
      current = null;
      stopPolling();
      clearTimeout(settling);
      textarea.removeEventListener("compositionstart", compositionStart, true);
      textarea.removeEventListener("compositionupdate", compositionUpdate, true);
      textarea.removeEventListener("compositionend", compositionEnd, true);
      textarea.removeEventListener("blur", interrupt, true);
      document.removeEventListener("visibilitychange", visibilityChanged);
      stopLayout();
      recorder.dispose();
      terminal.dispose();
    },
  };
}

function Fixture() {
  const viewport = React.useRef<HTMLDivElement>(null);
  const controls = React.useRef<ReturnType<typeof mountTerminal>>(null);
  const [caseId, setCaseId] = React.useState(fixtureCaseId);
  const selected = manifest.cases.find(entry => entry.id === caseId)!;
  const [ready, setReady] = React.useState(false);
  const [active, setActive] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState("");
  const [snapshot, setSnapshot] = React.useState<Snapshot | "unavailable" | null>(null);
  const [layout, setLayout] = React.useState<readonly LayoutEvent[]>([]);
  const [attempt, setAttempt] = React.useState<Attempt | null>(null);

  React.useEffect(() => {
    const host = viewport.current;
    if (!host) return;
    const mounted = mountTerminal(host, setLayout, setAttempt, setError);
    controls.current = mounted;
    mounted.terminal.write(transcript, () => {
      if (controls.current === mounted) setReady(true);
    });
    return () => {
      controls.current = null;
      mounted.dispose();
    };
  }, []);

  function selectCase(event: React.ChangeEvent<HTMLSelectElement>) {
    controls.current?.retire();
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
    try {
      if (await mounted.start(selected) && controls.current === mounted) setActive(true);
    } catch (reason) {
      if (controls.current === mounted) setError(`Held: ${String(reason)}`);
    } finally {
      if (controls.current === mounted) setBusy(false);
    }
  }

  async function read() {
    const mounted = controls.current;
    if (!mounted || !active || busy) return;
    setBusy(true);
    try {
      const result = await mounted.read();
      if (controls.current === mounted && result) setSnapshot(result);
    } finally {
      if (controls.current === mounted) setBusy(false);
    }
  }

  const readout = snapshot === "unavailable" ? "Unavailable" : snapshot ? hex(snapshot.bytes) || emptyHex : "—";
  const checkpoint = attempt?.checkpoint;
  return (
    <main>
      <h1>Offline terminal fixture</h1>
      <label htmlFor="case">Interaction case</label>{" "}
      <select id="case" value={caseId} onChange={selectCase} disabled={busy}>
        {manifest.cases.map(entry => <option key={entry.id} value={entry.id}>{entry.title}</option>)}
      </select>
      <button type="button" onClick={start} disabled={!ready || busy}>{active || error ? "Restart" : "Start"}</button>
      <button type="button" onClick={read} disabled={!active || busy || (attempt?.composition && !attempt.held && attempt.phase !== "ended")}>Read</button>
      <button type="button" disabled={!ready} onClick={() => controls.current?.terminal.focus()}>Focus</button>
      {selected.pasteText !== undefined && (
        <button type="button" disabled={!active || busy || Boolean(error)} onClick={() => {
          const terminal = controls.current?.terminal;
          terminal?.paste(selected.pasteText);
        }}>Paste sample</button>
      )}
      <div className="terminal-region">
        <div id="terminal" className="terminal-host" ref={viewport} aria-label="Synthetic terminal" />
        {selected.id.startsWith("composition-") && (
          <div className="checkpoint">
            <label htmlFor="checkpoint">Active native checkpoint</label>{" "}
            <output id="checkpoint">{checkpoint && attempt
              ? `${attempt.selected.id} · attempt ${attempt.id} · generation ${attempt.generation} · event ${checkpoint.sequence} · active and focused · ${checkpoint.snapshot.eventLengths.length} accepted events · ${checkpoint.snapshot.bytes.length} bytes · ${hex(checkpoint.snapshot.bytes) || emptyHex}`
              : attempt?.held ? "Unavailable" : "Pending"}</output>
          </div>
        )}
      </div>
      <label htmlFor="snapshot">Native snapshot (hex)</label>{" "}
      <output id="snapshot">{readout}</output>
      {snapshot && snapshot !== "unavailable" && (
        <span id="snapshot-counts"> · {snapshot.eventLengths.length} accepted events · {snapshot.bytes.length} bytes</span>
      )}
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
      {Boolean(attempt?.trace.length) && <details>
        <summary>Attempt event trace</summary>
        <pre id="composition-trace">{JSON.stringify({
          caseId: attempt!.selected.id, attemptId: attempt!.id, generation: attempt!.generation, events: attempt!.trace,
        })}</pre>
      </details>}
      <details>
        <summary>Viewport and focus events</summary>
        <pre id="layout-events">{JSON.stringify(layout)}</pre>
      </details>
    </main>
  );
}

const root = document.getElementById("root");
if (!root) throw new Error("Missing fixture root");
createRoot(root).render(<Fixture />);
