import { useLayoutEffect, useRef, useState } from "react";
import type { KeyboardEvent, MouseEvent } from "react";
import { adjacentView, focusAfterClose, tabKey, tabStop } from "./session-state.ts";
import type { SessionViews } from "./session-state.ts";

interface SessionTabsProps {
  readonly views: SessionViews;
  readonly title: (id: string) => string;
  readonly onFocusTab: () => void;
  readonly onActivate: (id: string) => void;
  readonly onMove: (id: string, direction: -1 | 1) => void;
  readonly onClose: (id: string) => void;
  readonly onEmptyFocus: () => void;
}

function focusTab(id: string) {
  document.getElementById(`tab-${id}`)?.focus();
}

function keepPointerFocus(event: MouseEvent<HTMLButtonElement>) {
  if (event.button === 0) event.preventDefault();
}

export function SessionTabs({ views, title, onFocusTab, onActivate, onMove, onClose, onEmptyFocus }: SessionTabsProps) {
  const bar = useRef<HTMLDivElement>(null);
  const [focused, setFocused] = useState<string | null>(null);
  const restoreFocus = useRef<HTMLElement | "fallback" | null>(null);
  const stop = tabStop(views, focused);

  useLayoutEffect(() => {
    const target = restoreFocus.current;
    if (!target) return;
    restoreFocus.current = null;
    if (target !== "fallback" && target.isConnected && !target.matches(":disabled")) target.focus();
    else if (stop) focusTab(stop);
    else onEmptyFocus();
  }, [views.open, stop, onEmptyFocus]);

  function activate(id: string) {
    focusTab(id);
    onActivate(id);
  }

  function navigate(event: KeyboardEvent<HTMLDivElement>, id: string) {
    if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey || event.nativeEvent.isComposing) return;
    const target = tabKey(views, id, event.key);
    if (!target || (target.action === "activate" && event.target !== document.getElementById(`tab-${id}`))) return;
    event.preventDefault();
    if (target.action === "activate") activate(target.id);
    else focusTab(target.id);
  }

  function move(id: string, direction: -1 | 1) {
    if (!adjacentView(views, id, direction)) return;
    const element = document.activeElement;
    if (element instanceof HTMLElement && bar.current?.contains(element)) restoreFocus.current = element;
    setFocused(stop);
    onMove(id, direction);
  }

  function close(id: string) {
    const element = document.activeElement;
    if (element instanceof HTMLElement && bar.current?.contains(element)) restoreFocus.current = element;
    else if (views.open.length === 1 || document.getElementById(`panel-${id}`)?.contains(element)) restoreFocus.current = "fallback";
    setFocused(focusAfterClose(views, id, stop));
    onClose(id);
  }

  if (views.open.length === 0) return null;

  return <div className="tabs" ref={bar} role="tablist" aria-label="Відкриті сесії">
    {views.open.map(id => {
      const name = title(id);
      const selected = views.active === id;
      const tabIndex = stop === id ? 0 : -1;
      return <div className={`tab-wrap ${selected ? "selected" : ""}`} role="presentation" key={id}
        onKeyDown={event => navigate(event, id)}>
        <button type="button" role="tab" id={`tab-${id}`} aria-selected={selected} aria-controls={`panel-${id}`}
          tabIndex={tabIndex} onFocus={() => { setFocused(id); onFocusTab(); }} onClick={() => activate(id)}>{name}</button>
        <button type="button" className="tab-action" tabIndex={tabIndex} disabled={!adjacentView(views, id, -1)}
          aria-label={`Перемістити вкладку ${name} ліворуч`} title="Перемістити ліворуч"
          onMouseDown={keepPointerFocus} onClick={() => move(id, -1)}>←</button>
        <button type="button" className="tab-action" tabIndex={tabIndex} disabled={!adjacentView(views, id, 1)}
          aria-label={`Перемістити вкладку ${name} праворуч`} title="Перемістити праворуч"
          onMouseDown={keepPointerFocus} onClick={() => move(id, 1)}>→</button>
        <button type="button" className="tab-action" tabIndex={tabIndex} aria-label={`Закрити вкладку ${name}`}
          title="Закрити вкладку" onMouseDown={keepPointerFocus} onClick={() => close(id)}>×</button>
      </div>;
    })}
  </div>;
}
