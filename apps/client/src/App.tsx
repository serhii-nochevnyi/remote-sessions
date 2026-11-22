import { useCallback, useEffect, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import { isTauri } from "@tauri-apps/api/core";
import { TerminalView } from "./TerminalView";
import { CreateSessionDialog } from "./CreateSessionDialog";
import { closeView, openView } from "./session-state.ts";
import type { SessionViews } from "./session-state.ts";
import { localClient, runtimeTitle, selectCreatedRuntime } from "./local-client";
import type { LocalCreate, RuntimeInfo } from "./local-client";
import "./App.css";

export default function App() {
  const [createOpen, setCreateOpen] = useState(false);
  const createButton = useRef<HTMLButtonElement>(null);
  const createDialogActive = useRef(false);
  const canCreate = isTauri() && navigator.userAgent.includes("Macintosh");
  const [views, setViews] = useState<SessionViews>({ open: [], active: null });
  const [runtimes, setRuntimes] = useState<readonly RuntimeInfo[]>([]);
  const remembered = useRef(new Map<string, RuntimeInfo>());
  const openedOperations = useRef(new Set<string>());
  const runtimeOperations = useRef(new Map<string, string>());
  const presentation = useRef(new Map<string, string>());
  const focusRuntime = useRef<string | null>(null);
  const panel = useRef<HTMLElement>(null);
  const [online, setOnline] = useState(false);
  const [agentPid, setAgentPid] = useState(0);
  const [message, setMessage] = useState(isTauri() ? "Підключення до Mac…" : "Для живих сесій відкрийте десктопний застосунок.");
  const [notice, setNotice] = useState("Локальні сесії · процеси працюють незалежно від вкладок.");
  const active = views.active ? remembered.current.get(views.active) : undefined;
  const operationId = views.active ? runtimeOperations.current.get(views.active) : undefined;
  const current = runtimes.find(runtime => runtime.runtime_id === views.active
    && (!operationId || runtime.operation_id === operationId));
  const connection = online ? agentPid : 0;
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
  useEffect(() => {
    if (!active || focusRuntime.current !== active.runtime_id || createOpen) return;
    if (!active.exited && (!online || !current)) return;
    const frame = requestAnimationFrame(() => {
      if (focusRuntime.current !== active.runtime_id) return;
      focusRuntime.current = null;
      (panel.current?.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea") ?? panel.current)?.focus();
    });
    return () => cancelAnimationFrame(frame);
  }, [active, createOpen, current, online]);
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
      if (source !== "recovery") focusRuntime.current = result.current === "unavailable" ? null : runtime.runtime_id;
      if (!operation.presented) presentation.current.set(runtime.runtime_id, operation.reference);
      setViews(value => source === "user"
        ? openView(value, runtime.runtime_id, [runtime.runtime_id])
        : selectCreatedRuntime(value, operation, prior, source === "recovery"));
      setNotice(result.result === "launched"
        ? "Запуск термінала підтверджено. Стан входу та довіри перевірте в CLI."
        : "Процес у терміналі завершився.");
    }
  }, []);
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
    createDialogActive.current = open;
    setCreateOpen(open);
  }
  function openCreated(operation: LocalCreate, explicit: boolean) {
    showCreated(operation, explicit ? "user" : "submit");
    changeDialog(false);
  }
  function select(id: string) {
    focusRuntime.current = null;
    setViews(value => openView(value, id, runtimes.map(r => r.runtime_id)));
  }
  function close(id: string) {
    if (focusRuntime.current === id) focusRuntime.current = null;
    setViews(value => closeView(value, id));
    setNotice("Вкладку закрито. Процес продовжує працювати — сесію можна відкрити знову.");
  }
  function activateTab(id: string) {
    focusRuntime.current = null;
    setViews(value => ({ ...value, active: id }));
  }
  function navigateTabs(event: KeyboardEvent<HTMLButtonElement>, index: number) {
    const length = views.open.length;
    const target = event.key === "ArrowRight" ? (index + 1) % length :
      event.key === "ArrowLeft" ? (index + length - 1) % length :
      event.key === "Home" ? 0 : event.key === "End" ? length - 1 : -1;
    if (target < 0) return;
    const id = views.open[target];
    if (!id) return;
    event.preventDefault();
    activateTab(id);
    document.getElementById(`tab-${id}`)?.focus();
  }
  return <div className={`app ${active ? "has-session" : ""}`}>
    <header className="titlebar"><strong>Remote Sessions</strong><span className="badge">Локальні сесії</span><span className="connection-badge">{online ? "● Підключено" : "○ Немає з’єднання"}</span></header>
    <div className="workspace">
      <aside className="sidebar" aria-label="Сесії на Mac">
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
        <div className="tabs" role="tablist" aria-label="Відкриті сесії">
          {views.open.map((id, index) => {
            const runtime = remembered.current.get(id);
            return <div className={`tab-wrap ${views.active === id ? "selected" : ""}`} key={id}>
              <button role="tab" id={`tab-${id}`} aria-selected={views.active === id} aria-controls={views.active === id ? "session-panel" : undefined}
                tabIndex={views.active === id || (!views.active && index === 0) ? 0 : -1}
                onClick={() => activateTab(id)} onKeyDown={event => navigateTabs(event, index)}>{runtime ? runtimeTitle(runtime) : id.slice(0, 6)}</button>
              <button className="close-tab" aria-label={`Закрити вкладку ${runtime ? runtimeTitle(runtime) : id}`} onClick={() => close(id)}>×</button>
            </div>;
          })}
        </div>
        <button className="back-button" onClick={() => { focusRuntime.current = null; setViews(value => ({ ...value, active: null })); }}>← Усі сесії</button>
        {active ? <section ref={panel} id="session-panel" tabIndex={-1} className="session-panel" role="tabpanel" aria-labelledby={`tab-${active.runtime_id}`}>
          <div className="session-toolbar"><span>{runtimeTitle(active)}</span><span>{active.width} × {active.height}</span></div>
          {online && current && !current.exited ? <TerminalView key={`${active.runtime_id}:${agentPid}`} runtimeId={active.runtime_id} epoch={agentPid} label={`Термінал ${runtimeTitle(active)}`} /> : <div className="empty-state"><h2>{!online ? "З’єднання перерване" : "Сесія завершена або недоступна"}</h2><p>{!online ? "Вкладка збережена. Підключимося автоматично, коли агент повернеться." : "Автоматичний повторний запуск процесу не виконується."}</p></div>}
        </section> : <div className="empty-state"><h2>Відкрий сесію зі списку</h2><p>{isTauri() ? "Наявні сесії можна переглядати у вкладках." : "Браузерний перегляд не має доступу до локальних процесів."}</p></div>}
        <footer><span>{message}</span></footer>
      </main>
    </div><div className="notice" role="status">{notice}</div>
    {createOpen && <CreateSessionDialog onDismiss={() => changeDialog(false)} onOpen={openCreated}
      returnFocus={createButton} connection={connection} />}
  </div>;
}
