import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import { bindTerminalPaste, bindTerminalProtocol } from "./terminal-protocol";
import { localClient, TerminalViewScope } from "./local-client";
import type { GeometryEvent, TerminalDelivery, TerminalSize, TerminalStatus, TerminalStreamFailure } from "./local-client";
import { attachCurrentTerminal, desiredTerminalMode, updateTerminalEligibility } from "./session-state";
import type { AttachmentMode, BoundTerminal, TerminalAttachmentIntent } from "./session-state";
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
  readonly label: string;
  readonly fontSize: number;
  readonly intent: TerminalAttachmentIntent | undefined;
  readonly previousBinding: BoundTerminal | null;
  readonly delivery: TerminalDelivery;
  readonly currentAttachment: (id: string, previous: BoundTerminal | null) => TerminalAttachmentIntent | undefined;
  readonly onBinding: (binding: BoundTerminal, state: "bound" | "retired" | "changed") => void;
}

export function TerminalView({ runtimeId, label, fontSize, intent, previousBinding, delivery, currentAttachment, onBinding }: TerminalViewProps) {
  const element = useRef<HTMLDivElement>(null);
  const font = useRef(fontSize);
  const updateFont = useRef<((size: number) => void) | null>(null);
  const reconnectDelay = useRef(500);
  const previousRef = useRef(previousBinding);
  const activeMode = useRef<AttachmentMode | undefined>(undefined);
  const detached = useRef(Promise.resolve());
  const updateEligibility = useRef<((mode: AttachmentMode | undefined) => void) | null>(null);
  const [status, setStatus] = useState("Підключення…");
  const [retry, setRetry] = useState(0);
  const [, refreshDelivery] = useState(0);
  const mode = desiredTerminalMode(intent, activeMode.current, delivery.mayRestore);
  useLayoutEffect(() => {
    previousRef.current = previousBinding;
  }, [previousBinding]);
  useLayoutEffect(() => {
    updateEligibility.current?.(mode);
  }, [mode]);
  useLayoutEffect(() => {
    delivery.onChange = () => {
      if (activeMode.current !== "ordinary") refreshDelivery(value => value + 1);
    };
    return () => { delivery.onChange = undefined; };
  }, [delivery]);
  useEffect(() => {
    font.current = fontSize;
    updateFont.current?.(fontSize);
  }, [fontSize]);
  useEffect(() => {
    const mountedElement = element.current;
    if (!mountedElement || !mode) return;
    const attachmentMode = mode;
    activeMode.current = attachmentMode;
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
      const deliveryIssue = delivery.inputIssue;
      const issue = deliveryIssue === undefined ? inputIssue
        : `Ввід перервано; автоматично не повторюємо.${deliveryIssue ? ` ${deliveryIssue}` : ""}`;
      if (issue) {
        setStatus(paste ? `${issue} · ${paste}` : issue);
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
      inputEligible: () => attachmentMode === "ordinary" &&
        currentAttachment(runtimeId, binding ?? previousRef.current)?.mode === "live",
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
        if (error === undefined && !delivery.held) inputIssue = "Ввід завеликий. Перепідключіть термінал перед продовженням.";
        report();
      },
      grantRejected() {
        if (!scope.canWrite) {
          proposals.close();
          protocol.pauseInput();
        }
        report();
      },
      retire(view) {
        if (binding) onBinding(binding, identityChanged ? "changed" : "retired");
        else if (identityChanged && previousRef.current) onBinding(previousRef.current, "changed");
        activeMode.current = undefined;
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
        if (view) detached.current = detach(view, failure);
        if (terminal.textarea === document.activeElement && container.isConnected) container.focus();
        terminal.dispose();
      },
      reconnect(error) {
        setStatus(`Перепідключення… ${String(error)}`);
        if (identityChanged) return;
        reconnect = setTimeout(() => {
          if (currentAttachment(runtimeId, previousRef.current)) setRetry(value => value + 1);
        }, reconnectDelay.current);
        reconnectDelay.current = Math.min(reconnectDelay.current * 2, 4000);
      },
    }, delivery);
    scope.setInputEligible(attachmentMode === "ordinary");
    const protocol = bindTerminalProtocol(terminal, bytes => { void scope.send(bytes); });
    const clipboard = bindTerminalPaste(container, bytes => { void scope.paste(bytes); });
    updateEligibility.current = desired => updateTerminalEligibility(scope, attachmentMode, desired);
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
        const previous = previousRef.current;
        const candidate = await attachCurrentTerminal(runtimeId, previous, attachmentMode,
          () => currentAttachment(runtimeId, previous), () => delivery.mayRestore, () => scope.active, detached.current, localClient);
        if (!candidate) {
          if (scope.active) scope.retire({ error: Error("Очікуємо підтвердження термінала") });
          return;
        }
        const { attachment } = candidate;
        const view = attachment.view_id;
        if (!candidate.isCurrent() || !scope.bind(view)) {
          await detach(view);
          if (scope.active) scope.retire({ error: Error("Очікуємо підтвердження термінала") });
          return;
        }
        const terminalScope = attachment.status.terminal;
        binding = candidate.binding;
        previousRef.current = binding;
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
          if (desiredTerminalMode(currentAttachment(runtimeId, candidate.binding), attachmentMode, delivery.mayRestore) !== attachmentMode) {
            scope.retire();
            return;
          }
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
        if ((error instanceof Error ? error.message : String(error)) === "Terminal attachment identity changed") identityChanged = true;
        failure = streamFailure(error, streamStage);
        scope.retire({ error });
      }
    }
    void stream();
    return () => {
      clearTimeout(reconnect);
      scope.retire();
    };
  }, [runtimeId, mode, retry, currentAttachment, delivery, onBinding]);
  return <><div className="stream-status" role="status"><span>{intent ? status : "Очікуємо підтвердження термінала…"}</span><button type="button" disabled={!intent} onClick={() => {
    element.current?.focus();
    updateEligibility.current?.(undefined);
    setRetry(value => value + 1);
  }}>Перепідключити</button></div>
    <div className="terminal-view"><div className="live-terminal" ref={element} role="region" aria-label={label} tabIndex={0} /></div></>;
}
