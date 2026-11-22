import type { SessionViews } from "./session-state.ts";

export const DEFAULT_TERMINAL_FONT = 14;
export const MIN_TERMINAL_FONT = 10;
export const MAX_TERMINAL_FONT = 28;
const STORAGE_KEY = "remote-sessions.presentation";
const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export interface PresentationPreferences {
  readonly views: SessionViews;
  readonly fontSize: number;
  readonly expanded: boolean;
}

export type ZoomAction = "in" | "out" | "reset";

export function zoomShortcut(event: Pick<KeyboardEvent, "key" | "metaKey" | "ctrlKey" | "altKey" | "isComposing">): ZoomAction | undefined {
  if (!event.metaKey || event.ctrlKey || event.altKey || event.isComposing) return;
  if (event.key === "+" || event.key === "=") return "in";
  if (event.key === "-" || event.key === "−") return "out";
  if (event.key === "0") return "reset";
}

export function zoomFont(size: number, action: ZoomAction): number {
  if (action === "reset") return DEFAULT_TERMINAL_FONT;
  return Math.min(MAX_TERMINAL_FONT, Math.max(MIN_TERMINAL_FONT, size + (action === "in" ? 1 : -1)));
}

export function parsePresentation(value: unknown): PresentationPreferences | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) return;
  const { version, open, active, fontSize, expanded } = value as Record<string, unknown>;
  if (Object.keys(value).length !== 5 || version !== 1 || !Array.isArray(open) ||
      typeof fontSize !== "number" || !Number.isInteger(fontSize) || fontSize < MIN_TERMINAL_FONT ||
      fontSize > MAX_TERMINAL_FONT || typeof expanded !== "boolean") return;

  const ids = new Set<string>();
  for (const id of open) {
    if (typeof id !== "string" || id.length !== 36 || !uuid.test(id)) return;
    const normalized = id.toLowerCase();
    if (ids.has(normalized)) return;
    ids.add(normalized);
  }
  if (active !== null && (typeof active !== "string" || !ids.has(active.toLowerCase()))) return;
  return { views: { open: [...ids], active: active === null ? null : active.toLowerCase() }, fontSize, expanded };
}

export function loadPresentation(storage?: Pick<Storage, "getItem">): PresentationPreferences {
  try {
    const saved = (storage ?? window.localStorage).getItem(STORAGE_KEY);
    const parsed = saved === null ? undefined : parsePresentation(JSON.parse(saved));
    if (parsed) return parsed;
  } catch {}
  return { views: { open: [], active: null }, fontSize: DEFAULT_TERMINAL_FONT, expanded: false };
}

export function savePresentation(preferences: PresentationPreferences, storage?: Pick<Storage, "setItem">): void {
  const { views, fontSize, expanded } = preferences;
  try {
    (storage ?? window.localStorage).setItem(STORAGE_KEY, JSON.stringify({
      version: 1, open: views.open, active: views.active, fontSize, expanded,
    }));
  } catch {}
}
