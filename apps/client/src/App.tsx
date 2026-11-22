import { useEffect, useRef, useState } from "react";
import { isTauri } from "@tauri-apps/api/core";
import { TerminalView } from "./TerminalView";
import { CreateSessionDialog } from "./CreateSessionDialog";
import { closeView, openView } from "./session-state.ts";
import type { SessionViews } from "./session-state.ts";
import { localClient, runtimeTitle } from "./local-client";
import type { RuntimeInfo } from "./local-client";
import "./App.css";

export default function App() {
  const [preflightOpen, setPreflightOpen] = useState(false);
  const preflightButton = useRef<HTMLButtonElement>(null);
  const canCheckWorkspace = isTauri() && navigator.userAgent.includes("Macintosh");
  const [views, setViews] = useState<SessionViews>({ open: [], active: null });
  const [runtimes, setRuntimes] = useState<readonly RuntimeInfo[]>([]);
  const remembered = useRef(new Map<string, RuntimeInfo>());
  const [online, setOnline] = useState(false);
  const [agentPid, setAgentPid] = useState(0);
  const [message, setMessage] = useState(isTauri() ? "Підключення до Mac…" : "Для живих сесій відкрийте десктопний застосунок.");
  const [notice, setNotice] = useState("Локальні сесії · процеси працюють незалежно від вкладок.");
  const active = views.active ? remembered.current.get(views.active) : undefined;
  const current = runtimes.find(runtime => runtime.runtime_id === views.active);
  useEffect(() => {
    if (!isTauri()) return;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function refresh() {
      try {
        const inventory = await localClient.inventory();
        if (disposed) return;
        for (const runtime of inventory.runtimes) remembered.current.set(runtime.runtime_id, runtime);
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
  function select(id: string) { setViews(value => openView(value, id, runtimes.map(r => r.runtime_id))); }
  function close(id: string) {
    setViews(value => closeView(value, id));
    setNotice("Вкладку закрито. Процес продовжує працювати — сесію можна відкрити знову.");
  }
  return <div className={`app ${active ? "has-session" : ""}`}>
    <header className="titlebar"><strong>Remote Sessions</strong><span className="badge">Локальні сесії</span><span className="connection-badge">{online ? "● Підключено" : "○ Немає з’єднання"}</span></header>
    <div className="workspace">
      <aside className="sidebar" aria-label="Сесії на Mac">
        <div className="host-label"><strong>Цей Mac</strong><small>{online ? "Локальне керування" : "Очікуємо агента"}</small></div>
        <h1>Сесії</h1>
        {canCheckWorkspace && <button className="session-row" ref={preflightButton} onClick={() => setPreflightOpen(true)}>
          Перевірити проєкт
        </button>}
        {runtimes.length === 0 && <p className="sidebar-note">{online ? "Поки немає запущених сесій." : "Список з’явиться після підключення."}</p>}
        {runtimes.map(runtime => <button className="session-row" key={runtime.runtime_id} aria-current={views.active === runtime.runtime_id ? "true" : undefined} onClick={() => select(runtime.runtime_id)}>
          <span className="session-icon" aria-hidden="true">›_</span><span><strong>{runtimeTitle(runtime)}</strong><small>{runtime.exited ? "Завершена" : online ? "Працює" : "Стан невідомий"}</small></span>
        </button>)}
        <p className="sidebar-note">Закриття вкладки не зупиняє процес. Натисніть у терміналі, щоб вводити команди.</p>
      </aside>
      <main className="main">
        <div className="tabs" role="tablist" aria-label="Відкриті сесії">
          {views.open.map(id => {
            const runtime = remembered.current.get(id);
            return <div className={`tab-wrap ${views.active === id ? "selected" : ""}`} key={id}>
              <button role="tab" id={`tab-${id}`} aria-selected={views.active === id} aria-controls="session-panel" onClick={() => setViews(value => ({ ...value, active: id }))}>{runtime ? runtimeTitle(runtime) : id.slice(0, 6)}</button>
              <button className="close-tab" aria-label={`Закрити вкладку ${runtime ? runtimeTitle(runtime) : id}`} onClick={() => close(id)}>×</button>
            </div>;
          })}
        </div>
        <button className="back-button" onClick={() => setViews(value => ({ ...value, active: null }))}>← Усі сесії</button>
        {active ? <section id="session-panel" className="session-panel" role="tabpanel" aria-labelledby={`tab-${active.runtime_id}`}>
          <div className="session-toolbar"><span>{runtimeTitle(active)}</span><span>{active.width} × {active.height}</span></div>
          {online && current && !current.exited ? <TerminalView key={`${active.runtime_id}:${agentPid}:${current.width}:${current.height}`} runtimeId={active.runtime_id} epoch={agentPid} label={`Термінал ${runtimeTitle(active)}`} /> : <div className="empty-state"><h2>{!online ? "З’єднання перерване" : "Сесія завершена або недоступна"}</h2><p>{!online ? "Вкладка збережена. Підключимося автоматично, коли агент повернеться." : "Автоматичний повторний запуск процесу не виконується."}</p></div>}
        </section> : <div className="empty-state"><h2>Відкрий сесію зі списку</h2><p>{isTauri() ? "Наявні сесії можна переглядати у вкладках." : "Браузерний перегляд не має доступу до локальних процесів."}</p></div>}
        <footer><span>{message}</span></footer>
      </main>
    </div><div className="notice" role="status">{notice}</div>
    {preflightOpen && <CreateSessionDialog onDismiss={() => setPreflightOpen(false)} returnFocus={preflightButton} />}
  </div>;
}
