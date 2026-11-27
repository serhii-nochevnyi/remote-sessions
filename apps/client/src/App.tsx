import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { ComponentProps, KeyboardEvent } from "react";
import { isTauri } from "@tauri-apps/api/core";
import { TerminalView } from "./TerminalView";
import { TerminalToolbar } from "./TerminalToolbar";
import type { FullscreenExitFocus } from "./TerminalToolbar";
import { CreateSessionDialog } from "./CreateSessionDialog";
import { SessionTabs } from "./SessionTabs";
import { canRestoreTerminalFocus, explicitTerminalOpen, inventoryBindingState, inventoryOpen, moveView, openView,
  restoreFullscreenExitFocus, sameRuntime, terminalPanel, viewAvailability } from "./session-state.ts";
import type { BoundTerminal, MountedTerminal, SessionViews, TerminalBindingState, TerminalFocusTarget } from "./session-state.ts";
import { createdRuntime, PresentationStore, zoomFont, zoomShortcut } from "./presentation-preferences.ts";
import type { ZoomAction } from "./presentation-preferences.ts";
import { localClient, runtimeTitle, TerminalDelivery } from "./local-client";
import type { CreateStatus, Inventory, LocalCreate, RuntimeInfo } from "./local-client";
import "./App.css";

interface FullscreenFocusIntent {
  readonly button: HTMLButtonElement;
  readonly original: HTMLTextAreaElement | undefined;
  keepTerminal: boolean;
  target: HTMLElement | null;
}

type AttachmentState = { binding: BoundTerminal; state: TerminalBindingState; attempt: number }
  | { binding: null; state: "closed"; attempt: number };

function PanelTerminal(props: Omit<ComponentProps<typeof TerminalView>, "delivery" | "detached">) {
  const [delivery] = useState(() => new TerminalDelivery());
  const detached = useRef(Promise.resolve());
  return props.mounted.key ? <TerminalView key={props.mounted.key} {...props} delivery={delivery} detached={detached} /> : null;
}

export default function App() {
  const [createOpen, setCreateOpen] = useState(false);
  const createButton = useRef<HTMLButtonElement>(null);
  const sidebar = useRef<HTMLElement>(null);
  const createDialogActive = useRef(false);
  const dialogRevision = useRef(0);
  const selectionRevision = useRef(0);
  const canCreate = isTauri() && navigator.userAgent.includes("Macintosh");
  const [store] = useState(() => new PresentationStore());
  const [preferences, setPreferences] = useState(store.current);
  const { views, fontSize, expanded } = preferences;
  const [runtimes, setRuntimes] = useState<readonly RuntimeInfo[]>([]);
  const remembered = useRef(new Map<string, RuntimeInfo>());
  const inventoryRef = useRef<Inventory | null>(null);
  const [external, setExternal] = useState<ReadonlyMap<string, { operationId: string | null; sessionId: string }>>(() => new Map());
  const externalRef = useRef(external);
  const [attachments, setAttachments] = useState<ReadonlyMap<string, AttachmentState>>(() => new Map());
  const attachmentsRef = useRef(attachments);
  const updateAttachment = useCallback((id: string, value: AttachmentState | null) => {
    const next = new Map(attachmentsRef.current);
    if (value) next.set(id, value);
    else next.delete(id);
    attachmentsRef.current = next;
    setAttachments(next);
  }, []);
  const focusRuntime = useRef<string | null>(null);
  const app = useRef<HTMLDivElement>(null);
  const panel = useRef<HTMLElement>(null);
  const liveScope = useRef<{ runtimeId: string; epoch: number } | null>(null);
  const fullscreenFocus = useRef<FullscreenFocusIntent | null>(null);
  const [online, setOnline] = useState(false);
  const [agentPid, setAgentPid] = useState(0);
  const [message, setMessage] = useState(isTauri() ? "Підключення до Mac…" : "Для живих сесій відкрийте десктопний застосунок.");
  const [notice, setNotice] = useState("Локальні сесії · процеси працюють незалежно від вкладок.");
  const [closeFault, setCloseFault] = useState<{ runtimeId: string; message: string } | null>(null);
  const activeId = views.active;
  const activeRuntime = online ? runtimes.find(runtime => runtime.runtime_id === activeId) : undefined;
  const expected = activeId ? preferences.exposures?.find(record => record.runtimeId === activeId) ?? external.get(activeId) : undefined;
  const inventory = online ? { agent_pid: agentPid, runtimes } : null;
  const panels = views.open.map(id => {
    const exposure = preferences.exposures?.find(record => record.runtimeId === id);
    const attachment = attachments.get(id);
    const previousBinding = attachment?.binding ?? null;
    const closed = exposure?.state === "closed" || attachment?.state === "closed";
    const mounted = terminalPanel(views, id, inventory, exposure ?? external.get(id),
      previousBinding, closed, attachment?.state === "closed" ? undefined : attachment?.state);
    return { id, mounted, previousBinding, attempt: attachment?.attempt ?? 0 };
  });
  const activePanel = panels.find(panel => panel.id === activeId);
  const terminalKey = activePanel?.mounted.key;
  const terminalEpoch = activePanel?.previousBinding?.identity.agentPid ?? agentPid;

  function availability(id: string) {
    return viewAvailability(online ? runtimes : null, id,
      preferences.exposures?.find(record => record.runtimeId === id)?.operationId ?? external.get(id)?.operationId ?? undefined);
  }
  const connection = online ? agentPid : 0;
  // @invariant: A newer DOM focus choice revokes terminal focus while AppKit still settles the exit.
  const cancelTerminalFocusRestore = useCallback(() => {
    if (fullscreenFocus.current) fullscreenFocus.current.keepTerminal = false;
  }, []);
  useLayoutEffect(() => {
    liveScope.current = activeId && terminalKey ? { runtimeId: activeId, epoch: terminalEpoch } : null;
    return cancelTerminalFocusRestore;
  }, [activeId, terminalKey, terminalEpoch, cancelTerminalFocusRestore]);
  const updatePreferences = useCallback((update: Parameters<PresentationStore["update"]>[0]) => {
    store.update(update);
    setPreferences(store.current);
  }, [store]);
  const setViews = useCallback((update: (views: SessionViews) => SessionViews) => {
    updatePreferences(value => {
      const next = update(value.views);
      return next === value.views ? value : { ...value, views: next };
    });
  }, [updatePreferences]);
  const currentAttachment = useCallback((id: string, previous: BoundTerminal | null, attempt: number): MountedTerminal => {
    const record = attachmentsRef.current.get(id);
    if ((record?.binding ?? null) !== previous || (record?.attempt ?? 0) !== attempt) return { mode: "missing" };
    const exposure = store.exposure(id);
    return terminalPanel(store.current.views, id, inventoryRef.current, exposure ?? externalRef.current.get(id),
      previous, exposure?.state === "closed" || record?.state === "closed", record?.state === "closed" ? undefined : record?.state);
  }, [store]);
  const onBinding = useCallback((binding: BoundTerminal, state: TerminalBindingState, previous: BoundTerminal | null, attempt: number) => {
    const id = binding.terminal.target.runtime_id;
    const current = attachmentsRef.current.get(id);
    if (!store.current.views.open.includes(id) || store.exposure(id)?.state === "closed" ||
        current?.state === "closed" || current?.state === "changed" || current?.state === "ended" ||
        (current?.binding ?? null) !== previous || (current?.attempt ?? 0) !== attempt) return false;
    if (state === "bound" && (!currentAttachment(id, previous, attempt).intent ||
        inventoryRef.current?.agent_pid !== binding.identity.agentPid)) return false;
    updateAttachment(id, { binding, state, attempt });
    return true;
  }, [store, currentAttachment, updateAttachment]);
  useEffect(() => {
    if (!isTauri()) return;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function refresh() {
      try {
        const inventory = await localClient.inventory();
        if (disposed) return;
        inventoryRef.current = inventory;
        for (const [id, record] of attachmentsRef.current) {
          if (!record.binding) continue;
          const state = inventoryBindingState(record.binding, record.state, inventory);
          if (state !== record.state) updateAttachment(id, { ...record, state });
        }
        for (const runtime of inventory.runtimes) {
          const expected = store.exposure(runtime.runtime_id)?.operationId ?? remembered.current.get(runtime.runtime_id)?.operation_id;
          if (!expected || expected === runtime.operation_id) remembered.current.set(runtime.runtime_id, runtime);
        }
        setRuntimes(inventory.runtimes); setAgentPid(inventory.agent_pid); setOnline(true); setMessage("Mac підключено");
      } catch (error) {
        if (disposed) return;
        inventoryRef.current = null;
        setOnline(false); setMessage(`Агент недоступний · повторюємо підключення. ${String(error)}`);
      }
      if (!disposed) timer = setTimeout(() => void refresh(), 1000);
    }
    void refresh();
    return () => { disposed = true; clearTimeout(timer); };
  }, [store, updateAttachment]);
  const focusTerminal = useCallback(() => {
    cancelTerminalFocusRestore();
    (panel.current?.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea") ?? panel.current)?.focus();
  }, [cancelTerminalFocusRestore]);
  function currentTerminal(): TerminalFocusTarget<HTMLTextAreaElement> | null {
    const textarea = panel.current?.querySelector<HTMLTextAreaElement>(".live-terminal[data-terminal-attached] .xterm-helper-textarea");
    return liveScope.current && textarea ? { ...liveScope.current, textarea } : null;
  }
  function captureFullscreenFocus(button: HTMLButtonElement): FullscreenExitFocus | undefined {
    const mountedApp = app.current;
    if (!mountedApp) return;
    const mountedPanel = panel.current;
    const captured = currentTerminal();
    focusRuntime.current = null;
    const intent: FullscreenFocusIntent = {
      button,
      original: captured?.textarea,
      keepTerminal: Boolean(captured?.textarea.isConnected) && !createDialogActive.current,
      target: null,
    };
    fullscreenFocus.current = intent;
    const terminalEligible = () => captured && canRestoreTerminalFocus(captured, currentTerminal(),
      intent.keepTerminal && !createDialogActive.current);
    const observer = new MutationObserver(() => {
      if (!terminalEligible()) {
        intent.keepTerminal = false;
        observer.disconnect();
      }
    });
    if (mountedPanel && intent.keepTerminal) {
      observer.observe(mountedPanel, { subtree: true, childList: true, attributes: true, attributeFilter: ["data-terminal-attached"] });
    }
    function currentTarget() {
      if (!mountedApp?.isConnected || app.current !== mountedApp || fullscreenFocus.current !== intent) return null;
      if (captured && terminalEligible()) return captured.textarea;
      const target = intent.target;
      if (!target?.isConnected || !mountedApp.contains(target) || target.closest(":disabled, [hidden], [inert]")) return null;
      if (createDialogActive.current && !target.closest("dialog[open]")) return null;
      return target;
    }
    function dispose() {
      observer.disconnect();
      if (fullscreenFocus.current === intent) fullscreenFocus.current = null;
    }
    return {
      dispose,
      restore: focusWebview => restoreFullscreenExitFocus(currentTarget, focusWebview, () => document.hasFocus()),
    };
  }
  useEffect(() => {
    if (!activeId || createOpen) return;
    const requested = focusRuntime.current === activeId;
    const openingFocusedPanel = Boolean(terminalKey) && document.activeElement === panel.current;
    if (!requested && !openingFocusedPanel) return;
    const frame = requestAnimationFrame(() => {
      if (focusRuntime.current !== activeId && document.activeElement !== panel.current) return;
      focusRuntime.current = null;
      focusTerminal();
    });
    return () => cancelAnimationFrame(frame);
  }, [activeId, terminalKey, createOpen, focusTerminal]);
  const showCreated = useCallback((operation: LocalCreate, source: "submit" | "recovery" | "user" | "saved", preserveActive = false) => {
    const runtime = createdRuntime(operation);
    if (!runtime) return false;
    const observed = inventoryRef.current?.runtimes.find(item => item.runtime_id === runtime.runtime_id);
    if (observed && !sameRuntime(observed, runtime)) {
      setNotice("Стан цієї сесії змінився. Оновіть запис створення.");
      return false;
    }
    const result = store.expose(operation, source, preserveActive);
    if (result.result !== "opened") {
      if (result.result === "storage") setNotice("Не вдалося зберегти вкладку. Повторіть відкриття з попередніх спроб.");
      else if (result.result === "conflict") setNotice("Не вдалося підтвердити цей термінал. Оновіть запис створення.");
      setPreferences(store.current);
      return false;
    }
    remembered.current.set(runtime.runtime_id, runtime);
    if (source === "user") {
      if (attachmentsRef.current.get(runtime.runtime_id)?.state === "closed") updateAttachment(runtime.runtime_id, null);
      setCloseFault(current => current?.runtimeId === runtime.runtime_id ? null : current);
    }
    if ((source === "submit" || source === "user") && !preserveActive) focusRuntime.current = runtime.runtime_id;
    setPreferences(store.current);
    setNotice(runtime.exited ? "Процес у терміналі завершився."
      : "Запуск термінала підтверджено. Стан входу та довіри перевірте в CLI.");
    return true;
  }, [store, updateAttachment]);
  const approveExternal = useCallback((runtime: RuntimeInfo) => {
    const operationId = runtime.operation_id ?? null;
    const cached = externalRef.current.get(runtime.runtime_id);
    if (cached?.sessionId === runtime.session_id && cached.operationId === operationId) return;
    const next = new Map(externalRef.current).set(runtime.runtime_id, { operationId, sessionId: runtime.session_id });
    externalRef.current = next;
    setExternal(next);
  }, []);
  const savedOperation = !expected && activeRuntime
    ? `${activeRuntime.runtime_id}:${activeRuntime.origin}:${activeRuntime.operation_id ?? ""}` : null;
  useEffect(() => {
    if (!canCreate || createOpen || !connection) return;
    let disposed = false;
    function restoreSaved(status: CreateStatus | null) {
      if (!store.refresh()) {
        setNotice("Не вдалося прочитати збережені вкладки. Повторіть відкриття з попередніх спроб.");
        return false;
      }
      setPreferences(store.current);
      const saved = store.current.views.open;
      for (const id of saved) {
        const exposure = store.exposure(id);
        if (exposure?.state === "presented") continue;
        const observed = inventoryRef.current?.runtimes.find(runtime => runtime.runtime_id === id);
        if (!observed) continue;
        const decision = inventoryOpen(status, observed, exposure, "saved");
        if (decision === "inventory") approveExternal(observed);
        else if (decision !== "blocked") showCreated(decision, "saved");
      }
      return true;
    }
    restoreSaved(null);
    void localClient.createStatus().catch(() => null).then(status => {
      if (disposed || createDialogActive.current || !restoreSaved(status)) return;
      for (const operation of status && !status.issue ? status.operations : []) {
        const result = operation.disposition;
        if (!operation.presented &&
          ((result.result === "launched" && result.current === "live") ||
          (result.result === "exited" && result.current === "exited" && !result.setup_issue))) {
          showCreated(operation, "recovery", selectionRevision.current !== 0);
        }
      }
      if (!status) setNotice("Не вдалося відновити записи створення.");
      else if (status.issue || status.operations.some(operation =>
        operation.disposition.result === "unknown" || operation.disposition.result === "pending")) {
        setNotice("Є незавершені спроби створення. Їхній стан доступний у вікні створення сесії.");
      }
    }).catch(() => {
      if (!disposed && !createDialogActive.current) setNotice("Не вдалося відновити записи створення.");
    });
    return () => { disposed = true; };
  }, [canCreate, connection, createOpen, showCreated, store, approveExternal, savedOperation]);
  useEffect(() => {
    if (!canCreate || !connection) return;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function reconcile() {
      for (const exposure of store.unsettled()) {
        if (!store.unsettled().includes(exposure)) continue;
        try {
          const status = await localClient.acknowledgeCreate(exposure.reference);
          if (disposed) return;
          const result = store.settle(exposure, status);
          if (result === "settled") {
            setPreferences(store.current);
            setNotice("Вкладку збережено.");
          } else if (store.unsettled().includes(exposure)) {
            setNotice("Вкладку ще не підтверджено для відновлення. Повторюємо перевірку.");
          }
        } catch {
          if (disposed) return;
          if (store.unsettled().includes(exposure)) setNotice("Вкладку ще не підтверджено для відновлення. Повторюємо перевірку.");
        }
      }
      if (!disposed) timer = setTimeout(() => void reconcile(), 1000);
    }
    void reconcile();
    return () => { disposed = true; clearTimeout(timer); };
  }, [canCreate, connection, store]);
  function changeDialog(open: boolean) {
    if (open) {
      cancelTerminalFocusRestore();
      if (fullscreenFocus.current) fullscreenFocus.current.target = null;
    }
    dialogRevision.current++;
    createDialogActive.current = open;
    setCreateOpen(open);
  }
  function captureSubmission() {
    const selection = selectionRevision.current;
    const dialog = dialogRevision.current;
    return (operation: LocalCreate) => {
      const opened = showCreated(operation, "submit", selection !== selectionRevision.current);
      if (opened && dialog === dialogRevision.current) changeDialog(false);
      return opened;
    };
  }
  async function openCreated(operation: LocalCreate) {
    const selection = ++selectionRevision.current;
    const dialog = dialogRevision.current;
    const runtime = createdRuntime(operation);
    if (!runtime) return false;
    const status = await localClient.createStatus(operation.reference);
    if (dialog !== dialogRevision.current || selection !== selectionRevision.current) return false;
    const [current] = status.operations;
    const observed = current && createdRuntime(current);
    if (status.issue || status.operations.length !== 1 || !current || current.reference !== operation.reference ||
        !observed || !sameRuntime(observed, runtime)) {
      setNotice("Не вдалося підтвердити цей термінал. Оновіть запис створення.");
      return false;
    }
    const opened = showCreated(current, "user");
    if (opened) changeDialog(false);
    return opened;
  }
  async function select(id: string) {
    cancelTerminalFocusRestore();
    const selection = ++selectionRevision.current;
    const sampled = inventoryRef.current;
    const runtime = sampled?.runtimes.find(entry => entry.runtime_id === id);
    if (!runtime) return;
    const exposure = store.exposure(id);
    const record = attachmentsRef.current.get(id);
    if (store.current.views.active === id && (exposure || externalRef.current.has(id)) &&
        record?.state !== "retired" && currentAttachment(id, record?.binding ?? null, record?.attempt ?? 0).mode !== "missing") {
      focusTerminal();
      return;
    }
    const status = canCreate && (runtime.origin !== "legacy" || exposure)
      ? await localClient.createStatus(exposure?.reference).catch(() => null) : null;
    if (selection !== selectionRevision.current) return;
    const current = inventoryRef.current;
    const latest = current?.runtimes.find(entry => entry.runtime_id === id);
    if (!latest || current?.agent_pid !== sampled?.agent_pid || !sameRuntime(latest, runtime)) return;
    if (!store.refresh()) {
      setNotice("Не вдалося прочитати збережені вкладки. Повторіть відкриття.");
      return;
    }
    const latestRecord = attachmentsRef.current.get(id);
    const { decision, retry } = explicitTerminalOpen(status, latest, store.exposure(id), latestRecord?.binding ?? null,
      latestRecord?.state === "closed" ? undefined : latestRecord?.state);
    if (decision === "blocked") {
      setNotice("Не вдалося підтвердити цей термінал. Оновіть запис створення.");
      return;
    }
    if (decision !== "inventory") {
      if (!showCreated(decision, "user")) return;
    } else {
      approveExternal(latest);
      if (latestRecord?.state === "closed") updateAttachment(id, null);
      focusRuntime.current = id;
      setViews(value => openView(value, id, [id]));
    }
    if (retry && latestRecord?.state === "retired") updateAttachment(id, { ...latestRecord, attempt: selection });
  }
  function close(id: string) {
    cancelTerminalFocusRestore();
    selectionRevision.current++;
    if (focusRuntime.current === id) focusRuntime.current = null;
    const result = store.close(id);
    updateAttachment(id, result === "pending" ? { binding: null, state: "closed", attempt: selectionRevision.current } : null);
    setPreferences(store.current);
    if (result === "closed") {
      setCloseFault(current => current?.runtimeId === id ? null : current);
      setNotice("Вкладку закрито. Доступні сесії можна відкрити знову зі списку.");
    } else {
      setCloseFault({ runtimeId: id, message: result === "pending" ? "Не вдалося зберегти закриття. Повторіть закриття вкладки."
        : "Вкладку закрито, але не вдалося зберегти закриття. Її можна відкрити з попередніх спроб." });
    }
  }
  function activateTab(id: string) {
    cancelTerminalFocusRestore();
    selectionRevision.current++;
    focusRuntime.current = null;
    if (id !== activeId) setViews(value => openView(value, id, value.open));
  }
  function title(id: string) {
    const known = remembered.current.get(id);
    return known ? runtimeTitle(known) : `Сесія · ${id.slice(0, 6)}`;
  }
  function tabTitle(id: string) {
    const status = availability(id);
    if (status === "missing") return `${title(id)} · Недоступна`;
    return status === "exited" ? `${title(id)} · Завершена` : title(id);
  }
  function focusSidebar() {
    (createButton.current ?? sidebar.current?.querySelector<HTMLButtonElement>(".session-row") ?? sidebar.current)?.focus();
  }
  function zoom(action: ZoomAction) {
    updatePreferences(value => {
      const next = zoomFont(value.fontSize, action);
      return next === value.fontSize ? value : { ...value, fontSize: next };
    });
    focusTerminal();
  }
  function handleZoomShortcut(event: KeyboardEvent<HTMLDivElement>) {
    if (!activeId || !canCreate || createDialogActive.current || event.defaultPrevented) return;
    const action = zoomShortcut(event.nativeEvent);
    if (!action) return;
    event.preventDefault();
    event.stopPropagation();
    zoom(action);
  }
  return <div ref={app} className={`app${canCreate ? " desktop" : ""}${activeId ? " has-session" : ""}${expanded && activeId ? " expanded" : ""}`}
    onPointerDownCapture={event => {
      const intent = fullscreenFocus.current;
      const target = event.nativeEvent.target;
      if (!intent || !(target instanceof Node) || intent.button.contains(target)) return;
      cancelTerminalFocusRestore();
      if (!intent.target?.contains(target)) intent.target = null;
    }} onFocusCapture={event => {
      const intent = fullscreenFocus.current;
      const target = event.nativeEvent.target;
      if (intent && target instanceof HTMLElement && target !== intent.button && target !== intent.original) {
        cancelTerminalFocusRestore();
        intent.target = target;
      }
    }}>
    <header className="titlebar"><strong>Remote Sessions</strong><span className="badge">Локальні сесії</span><span className="connection-badge">{online ? "● Підключено" : "○ Немає з’єднання"}</span></header>
    <div className="workspace">
      <aside className="sidebar" ref={sidebar} tabIndex={-1} aria-label="Сесії на Mac">
        <div className="host-label"><strong>Цей Mac</strong><small>{online ? "Локальне керування" : "Очікуємо агента"}</small></div>
        <h1>Сесії</h1>
        {canCreate && <button className="session-row" ref={createButton} onClick={() => changeDialog(true)}>
          Створити сесію…
        </button>}
        {runtimes.length === 0 && <p className="sidebar-note">{online ? "Поки немає запущених сесій." : "Список з’явиться після підключення."}</p>}
        {runtimes.map(runtime => <button className="session-row" key={runtime.runtime_id} aria-current={views.active === runtime.runtime_id ? "true" : undefined} onClick={() => void select(runtime.runtime_id)}>
          <span className="session-icon" aria-hidden="true">›_</span><span><strong>{runtimeTitle(runtime)}</strong><small>{runtime.exited ? "Завершена" : online ? "Працює" : "Стан невідомий"}</small></span>
        </button>)}
        <p className="sidebar-note">Закриття вкладки не зупиняє процес. Натисніть у терміналі, щоб вводити команди.</p>
      </aside>
      <main className="main">
        <SessionTabs views={views} title={tabTitle} onFocusTab={() => {
          cancelTerminalFocusRestore();
          focusRuntime.current = null;
        }}
          onActivate={activateTab} onMove={(id, direction) => {
            cancelTerminalFocusRestore();
            setViews(value => moveView(value, id, direction));
          }}
          onClose={close} onEmptyFocus={focusSidebar} />
        <button className="back-button" onClick={() => {
          cancelTerminalFocusRestore();
          focusRuntime.current = null;
          selectionRevision.current++;
          setViews(value => ({ ...value, active: null }));
          requestAnimationFrame(focusSidebar);
        }}>← Усі сесії</button>
        <div className="session-content" onKeyDownCapture={handleZoomShortcut}>
          <TerminalToolbar title={activeId ? title(activeId) : undefined} fontSize={fontSize} expanded={expanded}
            nativeWindow={canCreate} onZoom={zoom} onExpand={() => {
              updatePreferences(value => ({ ...value, expanded: !value.expanded }));
              focusTerminal();
            }} onReturnFocus={activeId ? focusTerminal : focusSidebar} onFullscreenExit={captureFullscreenFocus} />
          {panels.map(({ id, mounted, previousBinding, attempt }) => <section key={id} ref={id === activeId ? panel : undefined}
            id={`panel-${id}`} tabIndex={-1} hidden={id !== activeId} className={id === activeId ? "session-panel" : undefined}
            role="tabpanel" aria-labelledby={`tab-${id}`}>
            <PanelTerminal runtimeId={id} label={`Термінал ${title(id)}`}
              fontSize={fontSize} mounted={mounted} previousBinding={previousBinding} attempt={attempt}
              currentAttachment={currentAttachment} onBinding={onBinding} />
            {!mounted.key && id === activeId && <div className="empty-state">
                <h2>{mounted.mode === "offline" ? "Агент недоступний" : mounted.mode === "exited" ? "Сесія завершена"
                  : mounted.mode === "pending" ? "Перевіряємо запис сесії" : "Сесію не підтверджено"}</h2>
                <p>{mounted.mode === "offline" ? "Вкладка збережена. Оновимо стан після підключення."
                  : mounted.mode === "exited" ? "Процес завершився. Створіть нову сесію для нового запуску."
                  : "Вкладка збережена. Стан сесії оновиться, якщо вона з’явиться у списку."}</p>
            </div>}
          </section>)}
          {!activeId && <div className="empty-state"><h2>Відкрий сесію зі списку</h2><p>{isTauri() ? "Наявні сесії можна переглядати у вкладках." : "Браузерний перегляд не має доступу до локальних процесів."}</p></div>}
        </div>
        <footer><span>{message}</span></footer>
      </main>
    </div><div className="notice" role="status">{closeFault?.message ?? notice}</div>
    {createOpen && <CreateSessionDialog onDismiss={() => changeDialog(false)} onOpen={openCreated}
      captureSubmission={captureSubmission} returnFocus={createButton} connection={connection} />}
  </div>;
}
