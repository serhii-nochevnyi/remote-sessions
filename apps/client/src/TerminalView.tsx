import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { RefObject } from "react";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import { bindTerminalPaste, bindTerminalProtocol } from "./terminal-protocol";
import { localClient, TerminalViewScope } from "./local-client";
import type { GeometryEvent, TerminalDelivery, TerminalSize, TerminalStatus, TerminalStreamFailure } from "./local-client";
import { attachCurrentTerminal, desiredTerminalMode, explicitClosedReconnect, runMountedTerminalStream, TerminalAttachmentIdentityError,
  terminalStreamAccess, updateMountedStream } from "./session-state";
import type { AttachmentMode, BoundTerminal, MountedTerminal, TerminalBindingState } from "./session-state";
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

interface TerminalViewProps {
  readonly runtimeId: string;
  readonly label: string;
  readonly fontSize: number;
  readonly mounted: MountedTerminal;
  readonly previousBinding: BoundTerminal | null;
  readonly attempt: number;
  readonly delivery: TerminalDelivery;
  readonly detached: RefObject<Promise<void>>;
  readonly currentAttachment: (id: string, previous: BoundTerminal | null, attempt: number) => MountedTerminal;
  readonly onBinding: (binding: BoundTerminal, state: TerminalBindingState, previous: BoundTerminal | null, attempt: number) => boolean;
  readonly onReconnectClosed: (id: string, binding: BoundTerminal, attempt: number) => void;
}

export function TerminalView({ runtimeId, label, fontSize, mounted, previousBinding, attempt, delivery, detached, currentAttachment, onBinding,
  onReconnectClosed }: TerminalViewProps) {
  const element = useRef<HTMLDivElement>(null);
  const font = useRef(fontSize);
  const updateFont = useRef<((size: number) => void) | null>(null);
  const reconnectDelay = useRef(500);
  const previousRef = useRef(previousBinding);
  const activeMode = useRef<AttachmentMode | undefined>(undefined);
  const activeAgent = useRef<number | undefined>(undefined);
  const updateEligibility = useRef<((reconnect?: boolean) => void) | null>(null);
  const retainedRenderer = useRef<(() => void) | null>(null);
  const [status, setStatus] = useState("Підключення…");
  const [retry, setRetry] = useState(0);
  const [, refreshDelivery] = useState(0);
  const { intent, outputOnly } = mounted;
  const mode = intent ? desiredTerminalMode(intent, activeMode.current, delivery.mayRestore) : outputOnly ? activeMode.current : undefined;
  const agent = intent?.identity.agentPid ?? (outputOnly ? activeAgent.current : undefined);
  useLayoutEffect(() => () => { retainedRenderer.current?.(); }, []);
  useLayoutEffect(() => {
    previousRef.current = previousBinding;
  }, [previousBinding]);
  useLayoutEffect(() => {
    updateEligibility.current?.();
  }, [mounted, attempt]);
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
    if (!mountedElement || !mode || agent === undefined) return;
    const previous = previousRef.current;
    const reopening = retainedRenderer.current !== null;
    if (reopening && !currentAttachment(runtimeId, previous, attempt).intent?.reconnect) return;
    const attachmentMode = mode;
    activeMode.current = attachmentMode;
    activeAgent.current = agent;
    const container = mountedElement;
    const rendererId = crypto.randomUUID();
    const terminal = new Terminal({ disableStdin: true, fontSize: font.current, lineHeight: 1.25,
      fontFamily: "Menlo, Monaco, monospace", screenReaderMode: true, scrollback: 2000,
      theme: { background: "#111a21", foreground: "#d9e6e2" } });
    const fit = new FitAddon();
    terminal.loadAddon(fit);

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
    let retained = false;
    let disposed = false;
    let stale = false;
    let clipboard: ReturnType<typeof bindTerminalPaste> | undefined;
    const detach = (view: string, failure?: TerminalStreamFailure) => localClient.detach(view, failure).catch(() => undefined);
    const current = () => currentAttachment(runtimeId, binding ?? previous, attempt);
    const currentIntent = () => {
      const intent = current().intent;
      return !stale && intent?.identity.agentPid === agent ? intent : undefined;
    };
    const access = (): ReturnType<typeof terminalStreamAccess> => binding && !stale
      ? terminalStreamAccess(current(), binding, attachmentMode, delivery.mayRestore) : "stale";

    function report() {
      if (current().mode === "draining") {
        setStatus("Процес завершився · читаємо останній вивід…");
        return;
      }
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
        if (!scope.owns(proposal.terminal.view_id) || access() !== "attached") return;
        if (proposals.complete(proposal, result)) {
          resizeUnavailable = result.result !== "committed";
          report();
        }
      } catch {
        if (scope.owns(proposal.terminal.view_id) && access() === "attached" && proposals.complete(proposal)) {
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

    const setRendererFont = (size: number) => {
      terminal.options.fontSize = size;
      layoutSoon();
    };
    const observer = new ResizeObserver(layoutSoon);
    function openRenderer() {
      terminal.options.fontSize = font.current;
      terminal.open(container);
      if (document.activeElement === container) terminal.focus();
      updateFont.current = setRendererFont;
      observer.observe(container);
      clipboard = bindTerminalPaste(container, bytes => { void scope.paste(bytes); });
    }

    function revokeMutations() {
      proposals.close();
      protocol.pauseInput();
      layoutReady = false;
      cancelAnimationFrame(frame);
    }

    function disposeRenderer() {
      if (disposed) return;
      disposed = true;
      protocol.dispose();
      if (updateFont.current === setRendererFont) updateFont.current = null;
      if (terminal.textarea && terminal.textarea === document.activeElement && container.isConnected) container.focus();
      terminal.dispose();
    }

    let streamStage: TerminalStreamFailure = "attachment_setup";
    let failure: TerminalStreamFailure | undefined;
    const scope = new TerminalViewScope({
      input: (viewId, data, incarnation) => localClient.input({ viewId, data, incarnation }),
      inputEligible: () => attachmentMode === "ordinary" && access() === "attached",
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
        const draining = current().mode === "draining";
        if (!retained) {
          if (binding) onBinding(binding, identityChanged ? "changed" : draining ? "ended" : "retired", binding, attempt);
          else if (previous && (identityChanged || reopening)) onBinding(previous, identityChanged ? "changed" : "view-closed", previous, attempt);
          activeMode.current = undefined;
          activeAgent.current = undefined;
        }
        container.removeAttribute("data-terminal-attached");
        updateEligibility.current = null;
        routing?.close();
        revokeMutations();
        finishWrite?.();
        observer.disconnect();
        clipboard?.dispose();
        if (view) detached.current = detach(view, failure);
        if (retained) retainedRenderer.current = disposeRenderer;
        else disposeRenderer();
      },
      reconnect(error) {
        setStatus(`Перепідключення… ${String(error)}`);
        if ((reopening && !binding) || identityChanged || !currentIntent()) return;
        reconnect = setTimeout(() => {
          if (currentIntent()) setRetry(value => value + 1);
        }, reconnectDelay.current);
        reconnectDelay.current = Math.min(reconnectDelay.current * 2, 4000);
      },
    }, delivery);
    scope.setInputEligible(attachmentMode === "ordinary");
    const protocol = bindTerminalProtocol(terminal, bytes => { void scope.send(bytes); });
    if (!reopening) openRenderer();
    const streamEligibility = { scope, access, revokeMutations };
    updateEligibility.current = (reconnect = false) => {
      if (reconnect) {
        stale = true;
        scope.setInputEligible(false);
        scope.retire();
      } else if (binding) {
        if (updateMountedStream(streamEligibility) === "draining") report();
      } else if (desiredTerminalMode(currentIntent(), attachmentMode, delivery.mayRestore) !== attachmentMode) {
        scope.setInputEligible(false);
        scope.retire();
      }
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
        const candidate = await attachCurrentTerminal(runtimeId, previous, attachmentMode, currentIntent,
          () => delivery.mayRestore, () => scope.active, detached.current, localClient);
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
        if (!onBinding(candidate.binding, "bound", previous, attempt)) {
          scope.retire();
          return;
        }
        binding = candidate.binding;
        previousRef.current = binding;
        if (reopening) {
          retainedRenderer.current?.();
          retainedRenderer.current = null;
          openRenderer();
        }
        const ordered = new OrderedGeometry(attachment.status);
        routing = ordered;
        terminal.resize(attachment.cols, attachment.rows);
        container.setAttribute("data-terminal-attached", "");
        layoutSoon();
        await runMountedTerminalStream({
          ...streamEligibility, binding, routing: ordered, sink,
          read(view) {
            streamStage = "read_response";
            return localClient.read(view);
          },
          consume(view, chunk, render) {
            streamStage = "geometry_routing";
            return protocol.consume(scope, view, chunk, render);
          },
          rendered(chunk) {
            if (access() === "attached" && proposals.observe(ordered.status, scope.canWrite, ordered.applied)) {
              resizeUnavailable = false;
              layoutSoon();
            }
            if (chunk.geometry) resizeUnavailable = false;
            reconnectDelay.current = 500;
            report();
            void propose();
          },
          eof(state) {
            retained = onBinding(candidate.binding, state, candidate.binding, attempt);
          },
          closed() { failure = "stream_closed"; },
          identityChanged() { identityChanged = true; },
        });
      } catch (error) {
        if (!scope.active) return;
        if (binding && updateMountedStream(streamEligibility) === "stale") return;
        if (error instanceof TerminalAttachmentIdentityError) identityChanged = error.replacement;
        failure = streamFailure(error, streamStage);
        scope.retire({ error });
      }
    }
    void stream();
    return () => {
      clearTimeout(reconnect);
      stale = true;
      scope.retire();
      if (!retained) disposeRenderer();
      activeMode.current = undefined;
      activeAgent.current = undefined;
    };
  }, [runtimeId, mode, agent, attempt, retry, currentAttachment, delivery, detached, onBinding]);
  const displayStatus = mounted.mode === "completed" ? "Процес завершився · режим перегляду"
    : mounted.mode === "view-closed" ? intent ? "Перепідключення…" : "Потік термінала закрито · режим перегляду"
    : intent || outputOnly ? status : "Очікуємо підтвердження термінала…";
  return <><div className="stream-status" role="status"><span>{displayStatus}</span><button type="button"
    disabled={intent?.reconnect === true || (!intent && !explicitClosedReconnect(mounted))} onClick={() => {
    const current = currentAttachment(runtimeId, previousRef.current, attempt);
    const closedBinding = explicitClosedReconnect(current);
    if (closedBinding) {
      element.current?.focus();
      onReconnectClosed(runtimeId, closedBinding, attempt);
      return;
    }
    if (!current.intent || current.intent.reconnect) return;
    element.current?.focus();
    updateEligibility.current?.(true);
    setRetry(value => value + 1);
  }}>Перепідключити</button></div>
    <div className="terminal-view"><div className="live-terminal" ref={element} role="region" aria-label={label} tabIndex={0} /></div></>;
}
