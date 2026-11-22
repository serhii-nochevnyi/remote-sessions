import { invoke } from "@tauri-apps/api/core";
import { openView } from "./session-state.ts";
import type { SessionViews } from "./session-state.ts";

export interface RuntimeInfo {
  readonly runtime_id: string;
  readonly session_id: string;
  readonly command: string;
  readonly pane_pid: number | null;
  readonly exited: boolean;
  readonly width: number;
  readonly height: number;
  readonly origin?: "legacy" | "managed";
  readonly workspace_id?: string | null;
  readonly operation_id?: string | null;
}
export interface Inventory { readonly agent_pid: number; readonly runtimes: readonly RuntimeInfo[] }
export interface Attachment { readonly view_id: string; readonly cols: number; readonly rows: number }
export interface Chunk { readonly writable: boolean; readonly data: readonly number[]; readonly closed: boolean }
export type ProviderProfile = "claude_code" | "codex";
export interface WorkspaceDisplay { readonly workspace_id: string; readonly label: string }
type PaneLiveness = "live" | "exited" | "unavailable";
export type CreateDisposition =
  | { readonly result: "pending" }
  | { readonly result: "failed"; readonly issue: WorkspaceIssue }
  | { readonly result: "unknown"; readonly issue: WorkspaceIssue | null }
  | { readonly result: "launched"; readonly runtime: RuntimeInfo; readonly current: PaneLiveness; readonly readiness: "unobserved" }
  | { readonly result: "exited"; readonly runtime: RuntimeInfo; readonly current: PaneLiveness; readonly setup_issue: WorkspaceIssue | null };
export interface LocalCreate {
  readonly reference: string;
  readonly workspace_id: string;
  readonly profile: ProviderProfile;
  readonly presented: boolean;
  readonly disposition: CreateDisposition;
}
export type CreateSubmission = Blocked | { readonly result: "recorded"; readonly operation: LocalCreate };
export interface CreateStatus { readonly operations: readonly LocalCreate[]; readonly issue: WorkspaceIssue | null }
export type CreateIntent = { readonly separateFrom: string; readonly continueReference?: never }
  | { readonly continueReference: string; readonly separateFrom?: never };

export const workspaceIssueText = {
  invalid_request: "Оберіть папку та один із доступних профілів і повторіть перевірку.",
  invalid_folder: "Виберіть наявну папку проєкту.",
  unsupported_path: "Виберіть папку зі шляхом у UTF-8 без керівних символів.",
  folder_unavailable: "Папка недоступна. Під’єднайте диск або виберіть іншу папку.",
  folder_access_denied: "Агент Mac не має доступу до папки. Перевірте дозволи в параметрах macOS або виберіть іншу папку.",
  workspace_unknown: "Папки немає в каталозі агента. Виберіть її знову.",
  workspace_changed: "Папку переміщено або замінено. Виберіть її знову.",
  workspace_check_timed_out: "Агент Mac не встиг завершити перевірку. Дайте відповідь на запит macOS про доступ, якщо він з’явився, і повторіть спробу.",
  provider_missing: "Агент Mac не знайшов цей CLI. Встановіть його й додайте каталог із програмою до PATH агента.",
  provider_not_executable: "Агент Mac не може виконати цей CLI. Перевірте дозволи або перевстановіть CLI.",
  interpreter_unavailable: "Агент Mac не може виконати інтерпретатор цього CLI. Перевірте його встановлення та PATH агента.",
  catalog_unconfigured: "Каталог проєктів не налаштовано. Налаштуйте сховище в локальному агенті.",
  catalog_missing: "Бракує файлів каталогу. Відновіть сховище агента з резервної копії та перезапустіть агент.",
  catalog_unsafe: "Перевірте розташування, власника та дозволи каталогу проєктів і перезапустіть агент.",
  catalog_busy: "Каталог проєктів зайнятий. Зачекайте й повторіть спробу. Якщо помилка повторюється, перевірте, чи тільки один агент використовує каталог.",
  catalog_unsupported: "Агент не підтримує формат каталогу. Перевірте версію агента та каталогу.",
  catalog_unavailable: "Каталог проєктів недоступний. Збережіть його файли та перевірте стан локального сховища.",
  create_conflict: "Цю спробу вже пов’язано з іншою папкою або профілем.",
  runtime_limit: "Досягнуто ліміту локальних сесій.",
  tmux_unavailable: "Служба терміналів недоступна. Оновіть стан після відновлення підключення.",
  launch_setup_failed: "Не вдалося підготувати запуск CLI. Перевірте папку й запуск вибраного профілю.",
  pending_unavailable: "Не вдалося прочитати або зберегти локальні записи створення. Перевірте сховище застосунку.",
} as const;
type WorkspaceIssue = keyof typeof workspaceIssueText;
type Blocked = { readonly result: "blocked"; readonly issue: WorkspaceIssue };
export type WorkspaceSelection = Blocked | { readonly result: "cancelled" }
  | { readonly result: "selected"; readonly workspace: WorkspaceDisplay };
export type WorkspaceReadiness = Blocked | { readonly result: "ready" };

const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
function record(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw Error("Некоректна відповідь агента");
  return value as Record<string, unknown>;
}
function id(value: unknown): string {
  if (typeof value !== "string" || !uuid.test(value)) throw Error("Некоректний ідентифікатор");
  return value;
}
function integer(value: unknown, min: number, max: number): number {
  if (typeof value !== "number" || !Number.isInteger(value) || value < min || value > max) throw Error("Некоректне числове значення");
  return value;
}
export function parseInventory(value: unknown): Inventory {
  const item = record(value);
  if (!Array.isArray(item.runtimes) || item.runtimes.length > 16) throw Error("Некоректний список сесій");
  const runtimes = item.runtimes.map(parseRuntime);
  if (new Set(runtimes.map(r => r.runtime_id)).size !== runtimes.length) throw Error("Повторений runtime ID");
  return { agent_pid: integer(item.agent_pid, 1, 2 ** 31), runtimes };
}
function parseRuntime(value: unknown): RuntimeInfo {
  const runtime = record(value);
  if (typeof runtime.command !== "string" || runtime.command.length > 256 || typeof runtime.exited !== "boolean") throw Error("Некоректний опис сесії");
  const base = { runtime_id: id(runtime.runtime_id), session_id: id(runtime.session_id), command: runtime.command,
    pane_pid: runtime.pane_pid === null ? null : integer(runtime.pane_pid, 1, 2 ** 31), exited: runtime.exited,
    width: integer(runtime.width, 1, 10000), height: integer(runtime.height, 1, 10000) };
  if (runtime.origin === undefined) return base;
  if (runtime.origin === "managed") {
    return { ...base, origin: "managed", workspace_id: id(runtime.workspace_id), operation_id: id(runtime.operation_id) };
  }
  if (runtime.origin === "legacy" && runtime.workspace_id === null && runtime.operation_id === null) {
    return { ...base, origin: "legacy", workspace_id: null, operation_id: null };
  }
  throw Error("Некоректне походження сесії");
}
export function parseAttachment(value: unknown): Attachment {
  const item = record(value);
  return { view_id: id(item.view_id), cols: integer(item.cols, 2, 500), rows: integer(item.rows, 2, 200) };
}
export function parseChunk(value: unknown): Chunk {
  const item = record(value);
  if (typeof item.writable !== "boolean" || typeof item.closed !== "boolean" || !Array.isArray(item.data) || item.data.length > 8192) throw Error("Некоректний потік термінала");
  return { writable: item.writable, closed: item.closed, data: item.data.map((byte: unknown) => integer(byte, 0, 255)) };
}
function expectKeys(item: Record<string, unknown>, ...keys: string[]) {
  if (Object.keys(item).length !== keys.length || keys.some(key => !Object.prototype.hasOwnProperty.call(item, key))) {
    throw Error("Некоректна відповідь агента");
  }
}
function parseWorkspaceIssue(item: Record<string, unknown>): Blocked {
  expectKeys(item, "result", "issue");
  return { result: "blocked", issue: parseIssue(item.issue) };
}
function parseIssue(value: unknown): WorkspaceIssue {
  if (typeof value !== "string" || !Object.prototype.hasOwnProperty.call(workspaceIssueText, value)) {
    throw Error("Некоректний стан перевірки");
  }
  return value as WorkspaceIssue;
}
function optionalIssue(value: unknown): WorkspaceIssue | null {
  return value === null ? null : parseIssue(value);
}
export function parseWorkspaceSelection(value: unknown): WorkspaceSelection {
  const item = record(value);
  if (item.result === "blocked") return parseWorkspaceIssue(item);
  if (item.result === "cancelled") {
    expectKeys(item, "result");
    return { result: "cancelled" };
  }
  if (item.result !== "selected") throw Error("Некоректний результат вибору папки");
  expectKeys(item, "result", "workspace");
  const workspace = record(item.workspace);
  expectKeys(workspace, "workspace_id", "label");
  if (typeof workspace.label !== "string" || workspace.label.length === 0 || workspace.label.length > 4096
    || /[\u0000-\u001f\u007f-\u009f]/u.test(workspace.label)) {
    throw Error("Некоректна назва папки");
  }
  return { result: "selected", workspace: { workspace_id: id(workspace.workspace_id), label: workspace.label } };
}
export function parseWorkspaceReadiness(value: unknown): WorkspaceReadiness {
  const item = record(value);
  if (item.result === "blocked") return parseWorkspaceIssue(item);
  if (item.result !== "ready") throw Error("Некоректний результат перевірки");
  expectKeys(item, "result");
  return { result: "ready" };
}
function parseDisposition(value: unknown): CreateDisposition {
  const item = record(value);
  switch (item.result) {
    case "pending":
      expectKeys(item, "result");
      return { result: "pending" };
    case "failed":
      expectKeys(item, "result", "issue");
      return { result: "failed", issue: parseIssue(item.issue) };
    case "unknown":
      expectKeys(item, "result", "issue");
      return { result: "unknown", issue: optionalIssue(item.issue) };
    case "launched":
    case "exited": {
      const current = item.current;
      if (current !== "live" && current !== "exited" && current !== "unavailable") throw Error("Некоректний стан термінала");
      const runtime = parseRuntime(item.runtime);
      if (item.result === "launched") {
        expectKeys(item, "result", "runtime", "current", "readiness");
        if (item.readiness !== "unobserved" || runtime.exited || current === "exited") throw Error("Некоректний стан запуску");
        return { result: "launched", runtime, current, readiness: item.readiness };
      }
      expectKeys(item, "result", "runtime", "current", "setup_issue");
      if (!runtime.exited || current === "live") throw Error("Некоректний стан завершення");
      return { result: "exited", runtime, current, setup_issue: optionalIssue(item.setup_issue) };
    }
    default: throw Error("Некоректний стан створення");
  }
}
function parseOperation(value: unknown): LocalCreate {
  const item = record(value);
  expectKeys(item, "reference", "workspace_id", "profile", "presented", "disposition");
  if (item.profile !== "claude_code" && item.profile !== "codex") throw Error("Некоректний профіль");
  if (typeof item.presented !== "boolean") throw Error("Некоректний стан вкладки");
  return { reference: id(item.reference), workspace_id: id(item.workspace_id), profile: item.profile,
    presented: item.presented, disposition: parseDisposition(item.disposition) };
}
export function parseCreateSubmission(value: unknown): CreateSubmission {
  const item = record(value);
  if (item.result === "blocked") return parseWorkspaceIssue(item);
  expectKeys(item, "result", "operation");
  if (item.result !== "recorded") throw Error("Некоректний результат створення");
  return { result: "recorded", operation: parseOperation(item.operation) };
}
export function parseCreateStatus(value: unknown): CreateStatus {
  const item = record(value);
  expectKeys(item, "operations", "issue");
  if (!Array.isArray(item.operations)) throw Error("Некоректний список операцій");
  const operations = item.operations.map(parseOperation);
  if (new Set(operations.map(operation => operation.reference)).size !== operations.length) throw Error("Повторена операція");
  return { operations, issue: optionalIssue(item.issue) };
}
export function mergeCreateOperations(current: readonly LocalCreate[], incoming: readonly LocalCreate[]): LocalCreate[] {
  const operations = new Map(current.map(operation => [operation.reference, operation]));
  for (const operation of incoming) operations.set(operation.reference, operation);
  return [...operations.values()];
}
export function selectCreatedRuntime(views: SessionViews, operation: LocalCreate, handled: ReadonlySet<string>, preserveActive = false): SessionViews {
  const result = operation.disposition;
  if (handled.has(operation.reference) || (result.result !== "launched" && result.result !== "exited")) return views;
  const next = openView(views, result.runtime.runtime_id, [result.runtime.runtime_id]);
  return preserveActive && views.active ? { ...next, active: views.active } : next;
}
export const localClient = {
  inventory: async () => parseInventory(await invoke<unknown>("local_inventory")),
  pickWorkspace: async () => parseWorkspaceSelection(await invoke<unknown>("local_pick_workspace")),
  workspaceReadiness: async (workspaceId: string, profile: ProviderProfile) =>
    parseWorkspaceReadiness(await invoke<unknown>("local_workspace_readiness", { workspaceId, profile })),
  createSession: async (workspaceId: string, profile: ProviderProfile, intent?: CreateIntent) =>
    parseCreateSubmission(await invoke<unknown>("local_create_session", { workspaceId, profile, ...intent })),
  createStatus: async (reference?: string) =>
    parseCreateStatus(await invoke<unknown>("local_create_status", reference ? { reference } : {})),
  acknowledgeCreate: async (reference: string) =>
    parseCreateStatus(await invoke<unknown>("local_create_status", { reference, presented: true })),
  attach: async (runtimeId: string) => parseAttachment(await invoke<unknown>("terminal_attach", { runtimeId })),
  read: async (viewId: string) => parseChunk(await invoke<unknown>("terminal_read", { viewId })),
  input: async (viewId: string, data: readonly number[], paste: boolean) => { await invoke("terminal_input", { viewId, data, paste }); },
  detach: async (viewId: string) => { await invoke("terminal_detach", { viewId }); },
};
export function runtimeTitle(runtime: RuntimeInfo): string {
  const command = runtime.command.toLowerCase();
  const name = command.includes("claude") ? "Claude Code" : command.includes("codex") ? "Codex" : runtime.command || "Завершена сесія";
  return `${name} · ${runtime.runtime_id.slice(0, 6)}`;
}
