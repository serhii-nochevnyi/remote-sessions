export function observeTerminalLayout(
  host: HTMLElement,
  textarea: HTMLTextAreaElement,
  fit: { fit(): void },
  publish: (diagnostics: readonly LayoutEvent[]) => void,
) {
  const view = host.ownerDocument.defaultView!;
  const document = host.ownerDocument;
  const viewport = view.visualViewport;
  const diagnostics: LayoutEvent[] = [];
  let sequence = 0;
  let frame: number | undefined;
  let disposed = false;

  function update(event: string) {
    if (disposed) return;
    diagnostics.push({
      sequence: ++sequence, event,
      focused: document.activeElement === textarea,
      visible: document.visibilityState === "visible",
      width: viewport?.width ?? view.innerWidth,
      height: viewport?.height ?? view.innerHeight,
    });
    if (diagnostics.length > 24) diagnostics.shift();
    publish([...diagnostics]);
    frame ??= view.requestAnimationFrame(() => {
      frame = undefined;
      if (disposed) return;
      if (viewport) host.style.setProperty("--visible-height", `${viewport.height}px`);
      if (host.clientWidth > 0 && host.clientHeight > 0) fit.fit();
    });
  }

  const observer = new ResizeObserver(() => update("container resize"));
  observer.observe(host);
  const listeners: [EventTarget, string, string][] = [
    [view, "resize", "window resize"],
    [view, "orientationchange", "orientation change"],
    [textarea, "focus", "focus"],
    [textarea, "blur", "blur"],
    [document, "visibilitychange", "visibility change"],
  ];
  if (viewport) listeners.push([viewport, "resize", "viewport resize"], [viewport, "scroll", "viewport scroll"]);
  const remove = listeners.map(([target, event, name]) => {
    const listener = () => update(name);
    target.addEventListener(event, listener);
    return () => target.removeEventListener(event, listener);
  });
  update("mounted");
  return () => {
    disposed = true;
    observer.disconnect();
    for (const cleanup of remove) cleanup();
    if (frame !== undefined) view.cancelAnimationFrame(frame);
  };
}

export type LayoutEvent = {
  sequence: number;
  event: string;
  focused: boolean;
  visible: boolean;
  width: number;
  height: number;
};
