import type { CreateStatus, LocalCreate, RuntimeInfo } from "./local-client.ts";
import { closeView, openView } from "./session-state.ts";
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
  readonly exposures?: readonly CreateExposure[];
}

export interface CreateExposure {
  readonly reference: string;
  readonly runtimeId: string;
  readonly operationId: string;
  readonly sessionId: string;
  readonly state: "unresolved" | "presented" | "closed";
}

type PresentationStorage = Pick<Storage, "getItem" | "setItem">;
type ExposureSource = "submit" | "recovery" | "user" | "saved";
type ExposureResult = { result: "opened"; runtime: RuntimeInfo } | { result: "ignored" | "storage" | "conflict" };

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
  const { version, open, active, fontSize, expanded, exposures } = value as Record<string, unknown>;
  if (!((version === 1 && Object.keys(value).length === 5) ||
        (version === 2 && Object.keys(value).length === 6 && Array.isArray(exposures))) || !Array.isArray(open) ||
      typeof fontSize !== "number" || !Number.isInteger(fontSize) || fontSize < MIN_TERMINAL_FONT ||
      fontSize > MAX_TERMINAL_FONT || typeof expanded !== "boolean") return;

  const ids = new Set<string>();
  for (const id of open) {
    if (!isUuid(id)) return;
    const normalized = id.toLowerCase();
    if (ids.has(normalized)) return;
    ids.add(normalized);
  }
  if (active !== null && (typeof active !== "string" || !ids.has(active.toLowerCase()))) return;
  const preferences = { views: { open: [...ids], active: active === null ? null : active.toLowerCase() }, fontSize, expanded };
  if (version === 1 || !Array.isArray(exposures)) return preferences;
  const records: CreateExposure[] = [];
  for (const entry of exposures) {
    if (!entry || typeof entry !== "object" || Array.isArray(entry) || Object.keys(entry).length !== 5) return;
    const { reference, runtimeId, operationId, sessionId, state } = entry as Record<string, unknown>;
    if (!isUuid(reference) || !isUuid(runtimeId) || !isUuid(operationId) || !isUuid(sessionId) ||
        (state !== "unresolved" && state !== "presented" && state !== "closed")) return;
    const exposure: CreateExposure = {
      reference: reference.toLowerCase(), runtimeId: runtimeId.toLowerCase(),
      operationId: operationId.toLowerCase(), sessionId: sessionId.toLowerCase(), state,
    };
    if (records.some(record => record.reference === exposure.reference || record.runtimeId === exposure.runtimeId ||
      record.operationId === exposure.operationId)) return;
    records.push(exposure);
  }
  return { ...preferences, exposures: records };
}

function isUuid(value: unknown): value is string {
  return typeof value === "string" && value.length === 36 && uuid.test(value);
}

function defaults(): PresentationPreferences {
  return { views: { open: [], active: null }, fontSize: DEFAULT_TERMINAL_FONT, expanded: false };
}

function readPresentation(storage?: Pick<Storage, "getItem">): PresentationPreferences | undefined {
  try {
    const saved = (storage ?? window.localStorage).getItem(STORAGE_KEY);
    return saved === null ? defaults() : parsePresentation(JSON.parse(saved));
  } catch {}
}

function restore(preferences: PresentationPreferences): PresentationPreferences {
  const suppressed = new Set(preferences.exposures?.filter(record => record.state !== "presented").map(record => record.runtimeId));
  const { open, active } = preferences.views;
  if (!open.some(id => suppressed.has(id))) return preferences;
  return { ...preferences, views: {
    open: open.filter(id => !suppressed.has(id)),
    active: active && !suppressed.has(active) ? active : null,
  } };
}

export function loadPresentation(storage?: Pick<Storage, "getItem">): PresentationPreferences {
  return restore(readPresentation(storage) ?? defaults());
}

export function savePresentation(preferences: PresentationPreferences, storage?: Pick<Storage, "setItem">): boolean {
  const { views, fontSize, expanded, exposures } = preferences;
  try {
    (storage ?? window.localStorage).setItem(STORAGE_KEY, JSON.stringify({
      version: exposures ? 2 : 1, open: views.open, active: views.active, fontSize, expanded,
      ...(exposures ? { exposures } : {}),
    }));
    return true;
  } catch { return false; }
}

export function createdRuntime(operation: LocalCreate): RuntimeInfo | undefined {
  const { disposition } = operation;
  if (disposition.result === "launched" || disposition.result === "exited") return disposition.runtime;
}

function matchesExposure(record: CreateExposure, operation: LocalCreate, runtime: RuntimeInfo) {
  return record.reference === operation.reference && record.runtimeId === runtime.runtime_id &&
    record.operationId === runtime.operation_id && record.sessionId === runtime.session_id;
}

export class PresentationStore {
  private readable: boolean;
  private value: PresentationPreferences;
  private readonly storage: PresentationStorage | undefined;

  constructor(storage?: PresentationStorage) {
    this.storage = storage;
    const saved = readPresentation(storage);
    this.readable = saved !== undefined;
    this.value = restore(saved ?? defaults());
  }

  get current() { return this.value; }

  refresh(): boolean {
    const saved = readPresentation(this.storage);
    if (!saved) return false;
    if (!this.readable) {
      this.value = restore(saved);
      this.readable = true;
    }
    return true;
  }

  private commit(next: PresentationPreferences): boolean {
    if (!savePresentation(next, this.storage)) return false;
    this.value = next;
    return true;
  }

  update(update: (current: PresentationPreferences) => PresentationPreferences) {
    const readable = this.refresh();
    const next = update(this.value);
    if (readable && next !== this.value) savePresentation(next, this.storage);
    this.value = next;
  }

  exposure(runtimeId: string) {
    return this.value.exposures?.find(record => record.runtimeId === runtimeId);
  }

  expose(operation: LocalCreate, source: ExposureSource, preserveActive = false): ExposureResult {
    const { disposition } = operation;
    const runtime = createdRuntime(operation);
    const restoreExited = source === "user" || source === "saved";
    if (!runtime?.operation_id ||
        !((disposition.result === "launched" && disposition.current === "live") ||
          (disposition.result === "exited" && disposition.current === "exited" && (restoreExited || !disposition.setup_issue)))) {
      return { result: "conflict" };
    }
    if (!this.refresh()) return { result: "storage" };
    const { views, exposures = [] } = this.value;
    const id = runtime.runtime_id;
    const previous = exposures.find(record => record.runtimeId === id || record.reference === operation.reference ||
      record.operationId === runtime.operation_id);
    if (previous && !matchesExposure(previous, operation, runtime)) return { result: "conflict" };
    if (((source === "recovery" || source === "submit") && (previous || operation.presented)) ||
        (source === "saved" && (!views.open.includes(id) || previous))) return { result: "ignored" };

    const opened = openView(views, id, [id]);
    const nextViews = preserveActive || source === "saved" || (source === "recovery" && views.open.length > 0)
      ? { ...opened, active: views.active } : opened;
    if (previous && previous.state !== "closed" && views.open.includes(id)) {
      this.update(value => ({ ...value, views: nextViews }));
      return { result: "opened", runtime };
    }
    const exposure: CreateExposure = { reference: operation.reference, runtimeId: id,
      operationId: runtime.operation_id, sessionId: runtime.session_id, state: "unresolved" };
    const next = { ...this.value, views: nextViews, exposures: [...exposures.filter(record => record !== previous), exposure] };
    return this.commit(next) ? { result: "opened", runtime } : { result: "storage" };
  }

  unsettled() {
    return this.value.exposures?.filter(record => record.state === "unresolved" && this.value.views.open.includes(record.runtimeId)) ?? [];
  }

  settle(expected: CreateExposure, status: CreateStatus): "settled" | "ignored" | "storage" {
    if (this.exposure(expected.runtimeId) !== expected || !this.unsettled().includes(expected)) return "ignored";
    const operation = status.operations[0];
    const runtime = operation && createdRuntime(operation);
    if (status.issue || status.operations.length !== 1 || !operation?.presented || !runtime ||
        !matchesExposure(expected, operation, runtime)) return "ignored";
    if (!this.refresh()) return "storage";
    const exposures = this.value.exposures?.map(record => record === expected ? { ...record, state: "presented" as const } : record);
    if (!exposures) return "ignored";
    return this.commit({ ...this.value, exposures }) ? "settled" : "storage";
  }

  close(id: string): "closed" | "closed-unsaved" | "pending" {
    const readable = this.refresh();
    const { views, exposures } = this.value;
    const nextViews = closeView(views, id);
    if (nextViews === views) return "closed";
    const exposure = this.exposure(id);
    const next = { ...this.value, views: nextViews, ...(exposures && exposure
      ? { exposures: exposures.map(record => record === exposure ? { ...record, state: "closed" as const } : record) } : {}) };
    if (readable && this.commit(next)) return "closed";
    if (exposure?.state !== "unresolved") return "pending";
    this.value = { ...this.value, views: nextViews };
    return "closed-unsaved";
  }
}
