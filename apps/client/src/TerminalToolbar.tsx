import { useEffect, useRef, useState } from "react";
import { localClient } from "./local-client";
import { DEFAULT_TERMINAL_FONT, MAX_TERMINAL_FONT, MIN_TERMINAL_FONT } from "./presentation-preferences.ts";
import type { ZoomAction } from "./presentation-preferences.ts";

interface TerminalToolbarProps {
  readonly title: string | undefined;
  readonly fontSize: number;
  readonly expanded: boolean;
  readonly nativeWindow: boolean;
  readonly onZoom: (action: ZoomAction) => void;
  readonly onExpand: () => void;
  readonly onReturnFocus: () => void;
}

export function TerminalToolbar({ title, fontSize, expanded, nativeWindow, onZoom, onExpand, onReturnFocus }: TerminalToolbarProps) {
  const [fullscreen, setFullscreen] = useState(false);
  const [changing, setChanging] = useState(false);
  const [readFailed, setReadFailed] = useState(false);
  const [error, setError] = useState("");
  const pending = useRef(false);

  useEffect(() => {
    if (!nativeWindow || changing) return;
    let disposed = false;
    let revision = 0;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function refresh() {
      const read = ++revision;
      try {
        const actual = await localClient.windowFullscreen();
        if (!disposed && !pending.current && read === revision) {
          setFullscreen(actual);
          setReadFailed(false);
        }
      } catch {
        if (!disposed && !pending.current && read === revision) setReadFailed(true);
      }
    }
    function refreshSoon() {
      clearTimeout(timer);
      timer = setTimeout(() => void refresh(), 150);
    }
    void refresh();
    window.addEventListener("focus", refreshSoon);
    window.addEventListener("resize", refreshSoon);
    return () => {
      disposed = true;
      clearTimeout(timer);
      window.removeEventListener("focus", refreshSoon);
      window.removeEventListener("resize", refreshSoon);
    };
  }, [nativeWindow, changing]);

  async function toggleFullscreen() {
    if (pending.current) return;
    pending.current = true;
    setChanging(true);
    setError("");
    onReturnFocus();
    try {
      const target = !(await localClient.windowFullscreen());
      await localClient.windowFullscreen(target);
      for (let attempt = 0; attempt < 10; attempt++) {
        await new Promise<void>(resolve => setTimeout(resolve, attempt === 0 ? 500 : 150));
        const actual = await localClient.windowFullscreen();
        setFullscreen(actual);
        if (actual === target) return;
      }
      setError("Не вдалося підтвердити зміну повноекранного режиму.");
    } catch {
      setError("Не вдалося змінити повноекранний режим.");
    } finally {
      pending.current = false;
      setChanging(false);
    }
  }

  const fullscreenError = error || (readFailed ? "Не вдалося визначити стан повноекранного режиму." : "");
  if (!title && !fullscreen && !changing && !fullscreenError) return null;

  return <div className="session-toolbar" role="group" aria-label="Вигляд термінала">
    {title && <span>{title}</span>}
    <div className="terminal-controls">
      {title && <>
        <div className="zoom-controls" role="group" aria-label="Розмір шрифту термінала">
          <button type="button" aria-label="Зменшити шрифт" title="Зменшити шрифт (⌘−)"
            disabled={fontSize === MIN_TERMINAL_FONT} onClick={() => onZoom("out")}>−</button>
          <span role="status">{fontSize} px</span>
          <button type="button" aria-label="Збільшити шрифт" title="Збільшити шрифт (⌘+)"
            disabled={fontSize === MAX_TERMINAL_FONT} onClick={() => onZoom("in")}>+</button>
          <button type="button" aria-label={`Скинути розмір шрифту до ${DEFAULT_TERMINAL_FONT} пікселів`}
            title="Скинути розмір шрифту (⌘0)" onClick={() => onZoom("reset")}>Скинути</button>
        </div>
        <button type="button" onClick={onExpand}>{expanded ? "Відновити вигляд" : "Розгорнути термінал"}</button>
      </>}
      {nativeWindow && <button type="button" disabled={changing} onClick={() => void toggleFullscreen()}>
        {changing ? "Змінюємо режим вікна…" : fullscreen ? "Вийти з повноекранного режиму" : "Повноекранний режим"}
      </button>}
    </div>
    {fullscreenError && <span className="fullscreen-error" role="status">{fullscreenError}</span>}
  </div>;
}
