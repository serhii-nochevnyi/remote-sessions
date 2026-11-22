import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import { isTauri } from "@tauri-apps/api/core";
import { TerminalView } from "./TerminalView";
import { TerminalToolbar } from "./TerminalToolbar";
import type { FullscreenExitFocus } from "./TerminalToolbar";
import { CreateSessionDialog } from "./CreateSessionDialog";
import { SessionTabs } from "./SessionTabs";
import { canRestoreTerminalFocus, closeView, moveView, openView, restoreFullscreenExitFocus, viewAvailability } from "./session-state.ts";
import type { SessionViews, TerminalFocusTarget } from "./session-state.ts";
import { loadPresentation, savePresentation, zoomFont, zoomShortcut } from "./presentation-preferences.ts";
import type { ZoomAction } from "./presentation-preferences.ts";
import { localClient, runtimeTitle, selectCreatedRuntime } from "./local-client";
import type { LocalCreate, RuntimeInfo } from "./local-client";
import "./App.css";

interface FullscreenFocusIntent {
  readonly button: HTMLButtonElement;
  readonly original: HTMLTextAreaElement | undefined;
  keepTerminal: boolean;
  target: HTMLElement | null;
}

export default function App() {
  const [createOpen, setCreateOpen] = useState(false);
  const createButton = useRef<HTMLButtonElement>(null);
  const sidebar = useRef<HTMLElement>(null);
  const createDialogActive = useRef(false);
  const canCreate = isTauri() && navigator.userAgent.includes("Macintosh");
  const [preferences, setPreferences] = useState(loadPresentation);
  const { views, fontSize, expanded } = preferences;
  const lastPresentation = useRef(preferences);
  const [runtimes, setRuntimes] = useState<readonly RuntimeInfo[]>([]);
  const remembered = useRef(new Map<string, RuntimeInfo>());
  const openedOperations = useRef(new Set<string>());
  const runtimeOperations = useRef(new Map<string, string>());
  const presentation = useRef(new Map<string, string>());
  const focusRuntime = useRef<string | null>(null);
  const app = useRef<HTMLDivElement>(null);
  const panel = useRef<HTMLElement>(null);
  const liveScope = useRef<{ runtimeId: string; epoch: number } | null>(null);
  const fullscreenFocus = useRef<FullscreenFocusIntent | null>(null);
  const [online, setOnline] = useState(false);
  const [agentPid, setAgentPid] = useState(0);
  const [message, setMessage] = useState(isTauri() ? "Підключення до Mac…" : "Для живих сесій відкрийте десктопний застосунок.");
  const [notice, setNotice] = useState("Локальні сесії · процеси працюють незалежно від вкладок.");
  const activeId = views.active;
  function availability(id: string) {
    return viewAvailability(online ? runtimes : null, id, runtimeOperations.current.get(id));
  }
  const activeStatus = activeId ? availability(activeId) : undefined;
  const connection = online ? agentPid : 0;
  // @invariant: A newer DOM focus choice revokes terminal focus while AppKit still settles the exit.
  const cancelTerminalFocusRestore = useCallback(() => {
    if (fullscreenFocus.current) fullscreenFocus.current.keepTerminal = false;
  }, []);
  useLayoutEffect(() => {
    liveScope.current = activeId && activeStatus === "live" ? { runtimeId: activeId, epoch: agentPid } : null;
    return cancelTerminalFocusRestore;
  }, [activeId, activeStatus, agentPid, cancelTerminalFocusRestore]);
  const setViews = useCallback((update: (views: SessionViews) => SessionViews) => {
    setPreferences(value => {
      const next = update(value.views);
      return next === value.views ? value : { ...value, views: next };
    });
  }, []);
  useEffect(() => {
    if (lastPresentation.current === preferences) return;
    lastPresentation.current = preferences;
    savePresentation(preferences);
  }, [preferences]);
  useEffect(() => {
    if (!isTauri()) return;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function refresh() {
      try {
        const inventory = await localClient.inventory();
        if (disposed) return;
        for (const runtime of inventory.runtimes) {
          const expected = runtimeOperations.current.get(runtime.runtime_id);
          if (!expected || expected === runtime.operation_id) remembered.current.set(runtime.runtime_id, runtime);
        }
        setRuntimes(inventory.runtimes); setAgentPid(inventory.agent_pid); setOnline(true); setMessage("Mac підключено");
      } catch (error) {
        if (disposed) return;
        setOnline(false); setMessage(`Агент недоступний · повторюємо підключення. ${String(error)}`);
      }
      if (!disposed) timer = setTimeout(() => void refresh(), 1000);
    }
    void refresh();
    return () => { disposed = true; clearTimeout(timer); };
  }, []);
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
    const openingFocusedPanel = activeStatus === "live" && document.activeElement === panel.current;
    if (!requested && !openingFocusedPanel) return;
    const frame = requestAnimationFrame(() => {
      if (focusRuntime.current !== activeId && document.activeElement !== panel.current) return;
      focusRuntime.current = null;
      focusTerminal();
    });
    return () => cancelAnimationFrame(frame);
  }, [activeId, activeStatus, createOpen, focusTerminal]);
  const showCreated = useCallback((operation: LocalCreate, source: "submit" | "recovery" | "user") => {
    const result = operation.disposition;
    if (result.result !== "launched" && result.result !== "exited") return;
    const prior = openedOperations.current;
    if (source === "user" || !prior.has(operation.reference)) {
      const runtime = result.runtime;
      const observed = remembered.current.get(runtime.runtime_id);
      if (!observed || observed.operation_id !== runtime.operation_id) remembered.current.set(runtime.runtime_id, runtime);
      if (runtime.operation_id) runtimeOperations.current.set(runtime.runtime_id, runtime.operation_id);
      openedOperations.current = new Set([...prior, operation.reference]);
      if (source !== "recovery") focusRuntime.current = runtime.runtime_id;
      if (!operation.presented) presentation.current.set(runtime.runtime_id, operation.reference);
      setViews(value => source === "user"
        ? openView(value, runtime.runtime_id, [runtime.runtime_id])
        : selectCreatedRuntime(value, operation, prior, source === "recovery"));
      setNotice(result.result === "launched"
        ? "Запуск термінала підтверджено. Стан входу та довіри перевірте в CLI."
        : "Процес у терміналі завершився.");
    }
  }, [setViews]);
  useEffect(() => {
    if (!canCreate) return;
    let disposed = false;
    void localClient.createStatus().then(status => {
      if (disposed || createDialogActive.current) return;
      for (const operation of status.operations) {
        const result = operation.disposition;
        if (!operation.presented &&
          ((result.result === "launched" && result.current === "live") ||
          (result.result === "exited" && result.current === "exited" && !result.setup_issue))) {
          showCreated(operation, "recovery");
        }
      }
      if (status.issue || status.operations.some(operation =>
        operation.disposition.result === "unknown" || operation.disposition.result === "pending")) {
        setNotice("Є незавершені спроби створення. Їхній стан доступний у вікні створення сесії.");
      }
    }).catch(() => {
      if (!disposed && !createDialogActive.current) setNotice("Не вдалося відновити записи створення.");
    });
    return () => { disposed = true; };
  }, [canCreate, connection, showCreated]);
  useEffect(() => {
    for (const runtimeId of views.open) {
      const reference = presentation.current.get(runtimeId);
      if (!reference) continue;
      presentation.current.delete(runtimeId);
      void localClient.acknowledgeCreate(reference).catch(() => {});
    }
  }, [views]);
  function changeDialog(open: boolean) {
    if (open) {
      cancelTerminalFocusRestore();
      if (fullscreenFocus.current) fullscreenFocus.current.target = null;
    }
    createDialogActive.current = open;
    setCreateOpen(open);
  }
  function openCreated(operation: LocalCreate, explicit: boolean) {
    showCreated(operation, explicit ? "user" : "submit");
    changeDialog(false);
  }
  function select(id: string) {
    cancelTerminalFocusRestore();
    if (activeId === id) {
      focusTerminal();
      return;
    }
    focusRuntime.current = id;
    setViews(value => openView(value, id, runtimes.map(r => r.runtime_id)));
  }
  function close(id: string) {
    cancelTerminalFocusRestore();
    if (focusRuntime.current === id) focusRuntime.current = null;
    setViews(value => closeView(value, id));
    setNotice("Вкладку закрито. Доступні сесії можна відкрити знову зі списку.");
  }
  function activateTab(id: string) {
    cancelTerminalFocusRestore();
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
    setPreferences(value => {
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
        {runtimes.map(runtime => <button className="session-row" key={runtime.runtime_id} aria-current={views.active === runtime.runtime_id ? "true" : undefined} onClick={() => select(runtime.runtime_id)}>
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
          setViews(value => ({ ...value, active: null }));
          requestAnimationFrame(focusSidebar);
        }}>← Усі сесії</button>
        <div className="session-content" onKeyDownCapture={handleZoomShortcut}>
          <TerminalToolbar title={activeId ? title(activeId) : undefined} fontSize={fontSize} expanded={expanded}
            nativeWindow={canCreate} onZoom={zoom} onExpand={() => {
              setPreferences(value => ({ ...value, expanded: !value.expanded }));
              focusTerminal();
            }} onReturnFocus={activeId ? focusTerminal : focusSidebar} onFullscreenExit={captureFullscreenFocus} />
          {views.open.filter(id => id !== activeId).map(id =>
            <section key={id} id={`panel-${id}`} hidden role="tabpanel" aria-labelledby={`tab-${id}`} />)}
          {activeId ? <section key={activeId} ref={panel} id={`panel-${activeId}`} tabIndex={-1} className="session-panel"
            role="tabpanel" aria-labelledby={`tab-${activeId}`}>
            {activeStatus === "live"
              ? <TerminalView key={`${activeId}:${agentPid}`} runtimeId={activeId} epoch={agentPid} label={`Термінал ${title(activeId)}`} fontSize={fontSize} />
              : <div className="empty-state">
                <h2>{activeStatus === "offline" ? "Агент недоступний" : activeStatus === "exited" ? "Сесія завершена" : "Сесії немає у списку агента"}</h2>
                <p>{activeStatus === "offline" ? "Вкладка збережена. Оновимо стан після підключення."
                  : activeStatus === "exited" ? "Процес завершився. Створіть нову сесію для нового запуску."
                  : "Вкладка збережена. Стан сесії оновиться, якщо вона з’явиться у списку."}</p>
              </div>}
          </section> : <div className="empty-state"><h2>Відкрий сесію зі списку</h2><p>{isTauri() ? "Наявні сесії можна переглядати у вкладках." : "Браузерний перегляд не має доступу до локальних процесів."}</p></div>}
        </div>
        <footer><span>{message}</span></footer>
      </main>
    </div><div className="notice" role="status">{notice}</div>
    {createOpen && <CreateSessionDialog onDismiss={() => changeDialog(false)} onOpen={openCreated}
      returnFocus={createButton} connection={connection} />}
  </div>;
}
