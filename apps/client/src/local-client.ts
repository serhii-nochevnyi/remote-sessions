import { invoke } from "@tauri-apps/api/core";
import { openView } from "./session-state.ts";
import type { SessionViews } from "./session-state.ts";
import { TERMINAL_INPUT_BUDGET, terminalInputChunks } from "./terminal-protocol.ts";

export interface RuntimeInfo {
  readonly runtime_id: string;
  readonly session_id: string;
  readonly command: string;
  readonly pane_pid: number | null;
  readonly pane_id?: string;
  readonly exited: boolean;
  readonly width: number;
  readonly height: number;
  readonly origin?: "legacy" | "managed" | "unverified";
  readonly workspace_id?: string | null;
  readonly operation_id?: string | null;
}
export interface Inventory { readonly agent_pid: number; readonly runtimes: readonly RuntimeInfo[] }
export interface Attachment { readonly view_id: string; readonly cols: number; readonly rows: number }
export interface TerminalSize { readonly cols: number; readonly rows: number }
export interface ResizeEpoch { readonly agent: string; readonly owner: string }
export interface ResizeIntent { readonly epoch: ResizeEpoch; readonly id: string }
export interface TerminalScope {
  readonly view_id: string;
  readonly agent_epoch: string;
  readonly target: {
    readonly runtime_id: string;
    readonly generation: string;
    readonly session: string;
    readonly window: string;
    readonly pane: string;
    readonly pid: number;
    readonly process_start: number;
  };
}
export interface TerminalRestoration {
  readonly previous: TerminalScope;
  readonly agent_pid: number;
  readonly session_id: string;
  readonly operation_id: string | null;
}
export type TerminalAttachOptions = { readonly observer: true } | { readonly restoration: TerminalRestoration };
export interface CommittedGeometry {
  readonly size: TerminalSize;
  readonly revision: number;
  readonly intent: ResizeIntent | null;
}
export interface GridSample { readonly size: TerminalSize; readonly sampled_at_us: number }
export interface TerminalStatus {
  readonly terminal: TerminalScope;
  readonly epoch: ResizeEpoch | null;
  readonly committed: CommittedGeometry;
  readonly provider: GridSample | null;
  readonly observation: "pending" | "observed" | "unavailable";
}
export interface GeometryEvent {
  readonly status: TerminalStatus;
  readonly bridge_pty: GridSample;
  readonly client: GridSample;
}
export interface GridAcknowledgement {
  readonly renderer_id: string;
  readonly epoch: ResizeEpoch | null;
  readonly revision: number;
  readonly size: TerminalSize;
  readonly focused: boolean;
}
export type ResizeResult = { readonly result: "committed"; readonly admission: ResizeIntent; readonly status: TerminalStatus }
  | { readonly result: "uncertain" | "rejected"; readonly terminal: TerminalScope; readonly intent: ResizeIntent };
export type TerminalClosure = { readonly reason: "overflow"; readonly high_water: 16 }
  | { readonly reason: "eof" | "detached" | "geometry_timeout" };
export interface Chunk {
  readonly writable: boolean;
  readonly data: readonly number[];
  readonly closed: boolean;
  readonly status?: TerminalStatus | null;
  readonly geometry?: GeometryEvent | null;
  readonly closure?: TerminalClosure;
  readonly paste_incarnation?: string;
  readonly paste_completion?: PasteCompletion;
  readonly paste_admission?: PasteAdmission;
}
type PasteAdmission = "prior_send_unknown" | "unavailable";
interface PasteCompletion { readonly incarnation: string; readonly operation: number }
interface PasteGesture extends PasteCompletion { readonly viewId: string }
function samePaste(left: PasteCompletion | undefined, right: PasteCompletion | undefined) {
  return left !== undefined && right !== undefined
    && left.incarnation === right.incarnation && left.operation === right.operation;
}
type PasteResult = { readonly result: "staged" | "stale" | "invalid" | "unavailable" | "unknown" }
  | { readonly result: "committed"; readonly receipt: string }
  | { readonly result: "acknowledged"; readonly writable: boolean };
type PasteIssue = "invalid" | "unavailable" | "stale" | "stageStopped" | "unknown" | "oversize";
export const pasteIssueText: Record<PasteIssue, string> = {
  invalid: "Вставлення заблоковано до надсилання. Текст містить неприпустимі символи.",
  unavailable: "Вставлення з буфера обміну заблоковано до надсилання й недоступне на цьому бекенді.",
  stale: "Вставлення скасовано до надсилання через зміну керування.",
  stageStopped: "Це вставлення не було надіслано. Ввід у терміналі заблоковано через непідтверджену доставку попереднього вводу.",
  unknown: "Доставку вставлення не підтверджено. Перевірте чернетку в цій сесії. Для продовження створіть окрему сесію й введіть текст вручну.",
  oversize: "Вставлення заблоковано до надсилання. Завеликий обсяг тексту в черзі вводу.",
};
export type GeometryBoundaryFailure = "geometry_revision_not_new" | "geometry_status_rejected" | "geometry_output_incomplete";
export type TerminalStreamFailure = "attachment_setup" | "read_response" | "grid_ack_response"
  | "geometry_routing" | "stream_closed" | "terminal_status_changed" | GeometryBoundaryFailure
  | "renderer_grid_mismatch" | "stale_grid_ack" | "renderer_write_timeout";
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

interface ReadinessCheck {
  readonly workspaceId: string;
  readonly profile: ProviderProfile;
}

export class CreateReadiness {
  private current: ReadinessCheck | undefined;
  private ready = false;

  begin(workspaceId: string, profile: ProviderProfile): ReadinessCheck {
    this.ready = false;
    return this.current = { workspaceId, profile };
  }

  invalidate() {
    this.current = undefined;
    this.ready = false;
  }

  finish(check: ReadinessCheck, result: WorkspaceReadiness | undefined): boolean {
    if (this.current !== check) return false;
    this.ready = result?.result === "ready";
    return true;
  }

  allows(workspaceId: string, profile: ProviderProfile): boolean {
    return this.ready && this.current?.workspaceId === workspaceId && this.current.profile === profile;
  }

  take(workspaceId: string, profile: ProviderProfile): boolean {
    if (!this.allows(workspaceId, profile)) return false;
    this.invalidate();
    return true;
  }
}

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
  const runtimes = item.runtimes.map(runtime => parseRuntime(runtime, true));
  if (new Set(runtimes.map(r => r.runtime_id)).size !== runtimes.length) throw Error("Повторений runtime ID");
  return { agent_pid: integer(item.agent_pid, 1, 2 ** 31), runtimes };
}
function parseRuntime(value: unknown, inventory = false): RuntimeInfo {
  const runtime = record(value);
  if (typeof runtime.command !== "string" || runtime.command.length > 256 || typeof runtime.exited !== "boolean") throw Error("Некоректний опис сесії");
  const base = { runtime_id: id(runtime.runtime_id), session_id: id(runtime.session_id), command: runtime.command,
    pane_pid: runtime.pane_pid === null ? null : integer(runtime.pane_pid, 1, 2 ** 31), exited: runtime.exited,
    width: integer(runtime.width, 1, 10000), height: integer(runtime.height, 1, 10000),
    ...(runtime.pane_id === undefined ? {} : { pane_id: tmuxId(runtime.pane_id, "%") }) };
  if (runtime.origin === undefined) return base;
  if (runtime.origin === "managed") {
    if (inventory && runtime.workspace_id === null && runtime.operation_id === null) {
      return { ...base, origin: "unverified", workspace_id: null, operation_id: null };
    }
    return { ...base, origin: "managed", workspace_id: scopeId(runtime.workspace_id), operation_id: scopeId(runtime.operation_id) };
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
  const chunk = { writable: item.writable, closed: item.closed, data: item.data.map((byte: unknown) => integer(byte, 0, 255)) };
  if ("closure" in item) {
    expectKeys(item, "writable", "closed", "data", "status", "geometry", "closure");
    if (!chunk.closed || chunk.writable || chunk.data.length || item.status !== null || item.geometry !== null) {
      throw Error("Некоректне закриття потоку термінала");
    }
    const metadata = record(item.closure);
    const reason = metadata.reason;
    let closure: TerminalClosure;
    switch (reason) {
      case "overflow":
        expectKeys(metadata, "reason", "high_water");
        if (metadata.high_water !== 16) throw Error("Некоректна глибина черги термінала");
        closure = { reason, high_water: 16 };
        break;
      case "eof": case "detached": case "geometry_timeout":
        expectKeys(metadata, "reason");
        closure = { reason };
        break;
      default:
        throw Error("Некоректна причина закриття потоку термінала");
    }
    return { ...chunk, status: null, geometry: null, closure };
  }
  if (!("status" in item) && !("geometry" in item)) return chunk;
  const optional = ["paste_incarnation", "paste_completion", "paste_admission"].filter(key => key in item);
  expectKeys(item, "writable", "closed", "data", "status", "geometry", ...optional);
  const status = item.status === null ? null : parseTerminalStatus(item.status);
  const geometry = item.geometry === null ? null : parseGeometryEvent(item.geometry);
  if (geometry && (chunk.closed || chunk.data.length || status)) throw Error("Некоректна межа зміни розміру");
  const incarnation = "paste_incarnation" in item ? scopeId(item.paste_incarnation) : undefined;
  const completion = "paste_completion" in item ? parsePasteCompletion(item.paste_completion) : undefined;
  const admission = item.paste_admission;
  if ((admission !== undefined && admission !== "prior_send_unknown" && admission !== "unavailable")
    || (incarnation && !chunk.writable) || ((completion || admission) && chunk.writable)
    || (chunk.closed && optional.length) || (completion && admission)) throw Error("Некоректний стан вставлення");
  return { ...chunk, status, geometry,
    ...(incarnation ? { paste_incarnation: incarnation } : {}),
    ...(completion ? { paste_completion: completion } : {}),
    ...(admission ? { paste_admission: admission } : {}),
  };
}

function parsePasteCompletion(value: unknown): PasteCompletion {
  const item = record(value);
  expectKeys(item, "incarnation", "operation");
  return { incarnation: scopeId(item.incarnation), operation: integer(item.operation, 1, Number.MAX_SAFE_INTEGER) };
}

function parsePasteResult(value: unknown): PasteResult {
  const item = record(value);
  switch (item.result) {
    case "staged": case "stale": case "invalid": case "unavailable": case "unknown":
      expectKeys(item, "result");
      return { result: item.result };
    case "committed":
      expectKeys(item, "result", "receipt");
      return { result: "committed", receipt: scopeId(item.receipt) };
    case "acknowledged":
      expectKeys(item, "result", "writable");
      if (typeof item.writable !== "boolean") break;
      return { result: "acknowledged", writable: item.writable };
  }
  throw Error("Некоректна відповідь вставлення");
}

interface TerminalPasteCallbacks {
  stage(gesture: PasteGesture, offset: number, data: number[]): Promise<PasteResult>;
  commit(gesture: PasteGesture, bytes: number): Promise<PasteResult>;
  ack(gesture: PasteGesture, receipt: string): Promise<PasteResult>;
  changed(): void;
}
interface TerminalViewCallbacks {
  input(view: string, data: number[], incarnation: string): Promise<void>;
  inputEligible?(): boolean;
  paste?: TerminalPasteCallbacks;
  inputFailure(error?: unknown): void;
  grantRejected?(): void;
  retire(view: string | null): void;
  reconnect(error: unknown): void;
}

class StaleNativeInput extends Error {
  #viewId: string;
  #incarnation: string;

  constructor(viewId: string, incarnation: string) {
    super("Керування змінилося");
    this.#viewId = viewId;
    this.#incarnation = incarnation;
  }

  static matches(error: unknown, viewId: string, incarnation: string) {
    return typeof error === "object" && error !== null && #viewId in error &&
      error.#viewId === viewId && error.#incarnation === incarnation;
  }
}

function isStaleNativeInput(error: unknown, viewId: string): boolean {
  try {
    const value = record(error);
    if (Object.getPrototypeOf(value) !== Object.prototype ||
      Reflect.ownKeys(value).length !== 4 ||
      Object.values(Object.getOwnPropertyDescriptors(value)).some(field => !("value" in field))) return false;
    expectKeys(value, "reason", "view_id", "rejection_id", "event_time_us");
    const rejection = scopeId(value.rejection_id);
    return value.reason === "stale_input" && scopeId(value.view_id) === viewId &&
      rejection !== viewId && rejection === rejection.toLowerCase() &&
      rejection[14] === "4" && /^[89ab]$/.test(rejection[19] ?? "") &&
      integer(value.event_time_us, 1, Number.MAX_SAFE_INTEGER) > 0;
  } catch {
    return false;
  }
}

export class TerminalDelivery {
  private pending = 0;
  private ordinaryIssue: string | undefined;
  private pasteHold: "stageStopped" | "unknown" | undefined;
  onChange: (() => void) | undefined;

  get inputIssue() { return this.ordinaryIssue; }
  get pasteIssue() { return this.pasteHold; }
  get held() { return this.ordinaryIssue !== undefined || this.pasteHold !== undefined; }
  get mayRestore() { return this.pending === 0 && !this.held; }

  begin() {
    this.pending++;
    return () => {
      this.pending--;
      if (this.pending === 0) this.onChange?.();
    };
  }

  holdInput(error: unknown) {
    if (this.ordinaryIssue === undefined) {
      this.ordinaryIssue = "";
      try {
        if (typeof error === "string" || error instanceof Error) this.ordinaryIssue = String(error);
      } catch {}
    }
    this.onChange?.();
  }

  holdPaste(issue: "stageStopped" | "unknown") {
    if (this.pasteHold === issue) return;
    this.pasteHold = issue;
    this.onChange?.();
  }
}

export class TerminalViewScope {
  private attached: string | null = null;
  private retired = false;
  private writable = false;
  private inputEligible = true;
  private eligibilityVersion = 0;
  private inputFailed = false;
  private incarnation: string | undefined;
  private readonly rejectedIncarnations = new Set<string>();
  private completion: PasteCompletion | undefined;
  private admission: PasteAdmission | undefined;
  private pasteIssue: PasteIssue | undefined;
  private pasteHeld = false;
  private pendingPaste: PasteGesture | undefined;
  private acknowledgedPaste: PasteGesture | undefined;
  private endedPasteIncarnation: string | undefined;
  private snapshot = 0;
  private writeGeneration = 0;
  private operation = 0;
  private pendingBytes = 0;
  private inputChain = Promise.resolve();
  private readonly pendingInput = new Set<{ bytes: Uint8Array | null; incarnation: string }>();

  private readonly callbacks: TerminalViewCallbacks;
  private readonly delivery: TerminalDelivery;

  constructor(callbacks: TerminalViewCallbacks, delivery = new TerminalDelivery()) {
    this.callbacks = callbacks;
    this.delivery = delivery;
  }

  get viewId() { return this.attached; }
  get active() { return !this.retired; }
  get canWrite() {
    return this.active && this.inputEligible && this.writable && this.incarnation !== undefined &&
      !this.rejectedIncarnations.has(this.incarnation) && !this.inputFailed && !this.delivery.held &&
      (this.callbacks.inputEligible?.() ?? true);
  }
  get pasteMessage() {
    return this.pasteIssue ? pasteIssueText[this.pasteIssue]
      : this.delivery.pasteIssue ? pasteIssueText[this.delivery.pasteIssue]
      : this.admission === "unavailable" ? pasteIssueText.unavailable : undefined;
  }

  bind(view: string): boolean {
    if (this.retired || this.attached !== null) return false;
    this.attached = view;
    return true;
  }

  owns(view: string) { return this.active && this.attached === view; }

  setInputEligible(eligible: boolean) {
    if (this.inputEligible === eligible) return;
    this.inputEligible = eligible;
    this.eligibilityVersion++;
    this.writeGeneration++;
    this.writable = false;
    for (const frame of this.pendingInput) frame.bytes = null;
  }

  captureReadEligibility(): (chunk: Chunk) => Chunk {
    const version = this.eligibilityVersion;
    return chunk => {
      if (version === this.eligibilityVersion || !chunk.writable) return chunk;
      const { paste_incarnation: _, ...readOnly } = chunk;
      return { ...readOnly, writable: false };
    };
  }

  retire(recovery?: { error: unknown }) {
    if (this.retired) return;
    this.retired = true;
    const view = this.attached;
    this.attached = null;
    this.writable = false;
    for (const frame of this.pendingInput) frame.bytes = null;
    this.pendingInput.clear();
    this.callbacks.retire(view);
    if (recovery) this.callbacks.reconnect(recovery.error);
  }

  acceptClosure(view: string, closure: TerminalClosure) {
    if (this.owns(view)) {
      const reason = closure.reason === "overflow" ? "Спостерігач не встиг обробити вивід" : "Потік перервано";
      this.retire({ error: Error(reason) });
    }
  }

  async consume(view: string, chunk: Chunk, render: () => Promise<void>): Promise<boolean> {
    if (!this.owns(view)) return false;
    if (chunk.paste_admission === "prior_send_unknown" && this.delivery.pasteIssue === undefined) this.holdPaste("unknown");
    if (samePaste(chunk.paste_completion, this.acknowledgedPaste) ||
      (chunk.writable && chunk.paste_incarnation !== undefined && this.rejectedIncarnations.has(chunk.paste_incarnation))) {
      await render();
      return this.owns(view);
    }
    const snapshot = ++this.snapshot;
    const eligibility = this.eligibilityVersion;
    const writable = chunk.writable && (chunk.paste_incarnation === undefined ||
      chunk.paste_incarnation !== this.endedPasteIncarnation);
    const incarnation = writable ? chunk.paste_incarnation : undefined;
    if (this.pendingPaste && (
      (incarnation && incarnation !== this.pendingPaste.incarnation)
      || (chunk.paste_completion && !samePaste(chunk.paste_completion, this.pendingPaste))
    )) {
      this.pendingPaste = undefined;
    }
    if (this.incarnation !== incarnation || (this.writable && !writable)) {
      const ownCompletion = samePaste(chunk.paste_completion, this.pendingPaste)
        || (samePaste(this.completion, this.pendingPaste) && incarnation === this.pendingPaste?.incarnation);
      if (!ownCompletion) this.writeGeneration++;
      this.writable = false;
    }
    this.incarnation = incarnation;
    this.completion = chunk.paste_completion;
    this.admission = chunk.paste_admission;
    await render();
    if (!this.owns(view)) return false;
    if (snapshot === this.snapshot && eligibility === this.eligibilityVersion && this.inputEligible) this.writable = writable;
    return true;
  }

  private failInput(error?: unknown) {
    this.inputFailed = true;
    this.writeGeneration++;
    for (const frame of this.pendingInput) frame.bytes = null;
    this.callbacks.inputFailure(error);
  }

  private holdPaste(issue: "stageStopped" | "unknown") {
    this.delivery.holdPaste(issue);
    if (this.pasteHeld) return;
    this.pasteHeld = true;
    this.writeGeneration++;
    this.pasteIssue = undefined;
    for (const frame of this.pendingInput) frame.bytes = null;
  }

  private showPaste(issue?: PasteIssue) {
    if (issue === "stageStopped" || issue === "unknown") this.holdPaste(issue);
    else if (this.delivery.pasteIssue) return;
    this.pasteIssue = issue;
    this.callbacks.paste?.changed();
  }

  private enqueue(data: Uint8Array, incarnation: string, work: (frame: { bytes: Uint8Array | null }) => Promise<void>) {
    const length = data.length;
    const frame = { bytes: data, incarnation };
    this.pendingBytes += length;
    this.pendingInput.add(frame);
    this.inputChain = this.inputChain.then(() => work(frame)).finally(() => {
      this.pendingInput.delete(frame);
      this.pendingBytes -= length;
    });
    return this.inputChain;
  }

  send(data: Uint8Array): Promise<void> {
    const view = this.attached;
    const incarnation = this.incarnation;
    if (!view || !incarnation || !this.canWrite) return Promise.resolve();
    if (this.pendingBytes + data.length > TERMINAL_INPUT_BUDGET) {
      this.failInput();
      return Promise.resolve();
    }
    const generation = this.writeGeneration;
    const live = () => this.owns(view) && this.canWrite && this.incarnation === incarnation
      && this.writeGeneration === generation;
    return this.enqueue(data, incarnation, async frame => {
      if (!frame.bytes) return;
      for (const { bytes } of terminalInputChunks(frame.bytes)) {
        if (!live()) return;
        const finish = this.delivery.begin();
        try {
          await this.callbacks.input(view, Array.from(bytes), incarnation);
        } catch (error) {
          if (StaleNativeInput.matches(error, view, incarnation)) {
            this.rejectedIncarnations.add(incarnation);
            for (const queued of this.pendingInput) {
              if (queued.incarnation === incarnation) queued.bytes = null;
            }
            if (this.owns(view)) this.callbacks.grantRejected?.();
          } else {
            this.delivery.holdInput(error);
            if (this.owns(view)) this.failInput(error);
          }
          return;
        } finally {
          finish();
        }
      }
    });
  }

  paste(data: Uint8Array | null): Promise<void> {
    const viewId = this.attached;
    const incarnation = this.incarnation;
    const paste = this.callbacks.paste;
    if (!viewId || !this.canWrite || data?.length === 0) return Promise.resolve();
    if (!incarnation || !paste) {
      this.showPaste("unavailable");
      return Promise.resolve();
    }
    if (!data || this.pendingBytes + data.length > TERMINAL_INPUT_BUDGET) {
      this.showPaste("oversize");
      return Promise.resolve();
    }
    const gesture = { viewId, incarnation, operation: ++this.operation };
    const generation = this.writeGeneration;
    const live = () => this.owns(viewId) && this.canWrite && this.incarnation === incarnation
      && this.writeGeneration === generation;
    return this.enqueue(data, incarnation, async frame => {
      let submitted = false;
      if (!frame.bytes) return;
      if (!live()) { this.showPaste("stale"); return; }
      const finish = this.delivery.begin();
      const issue = (result: PasteIssue) => {
        if (this.owns(viewId)) this.showPaste(result);
        else if (result === "stageStopped" || result === "unknown") this.delivery.holdPaste(result);
      };
      try {
        this.showPaste();
        for (const { offset, bytes } of terminalInputChunks(frame.bytes)) {
          if (!live()) { issue("stale"); return; }
          const staged = await paste.stage(gesture, offset, Array.from(bytes));
          if (staged.result === "unknown") { issue("stageStopped"); return; }
          if (!live()) { issue("stale"); return; }
          if (staged.result !== "staged") {
            issue(staged.result === "stale" || staged.result === "invalid" ? staged.result : "unavailable");
            return;
          }
        }
        if (!live()) { issue("stale"); return; }
        submitted = true;
        this.pendingPaste = gesture;
        const result = await paste.commit(gesture, frame.bytes.length);
        if (result.result !== "committed") {
          issue(result.result === "stale" || result.result === "invalid" || result.result === "unavailable"
            ? result.result : "unknown");
          return;
        }
        if (!this.owns(viewId) || this.callbacks.inputEligible?.() === false) { issue("unknown"); return; }
        // @invariant: The exact receipt permits Ack after a read-only sample, even when completion polling lags.
        if (this.pendingPaste !== gesture) { issue("unknown"); return; }
        const ack = await paste.ack(gesture, result.receipt);
        if (!this.owns(viewId)) {
          if (ack.result !== "acknowledged") issue("unknown");
          return;
        }
        if (ack.result !== "acknowledged") { issue("unknown"); return; }
        this.acknowledgedPaste = gesture;
        if (ack.writable && this.pendingPaste === gesture
          && this.writeGeneration === generation && samePaste(this.completion, gesture)) {
          this.snapshot++;
          this.incarnation = incarnation;
          this.completion = undefined;
          this.writable = true;
        }
        if (!ack.writable) {
          this.endedPasteIncarnation = incarnation;
          if (this.incarnation === incarnation) {
            this.snapshot++;
            this.incarnation = undefined;
            this.writable = false;
            this.writeGeneration++;
          }
        }
      } catch {
        issue(submitted ? "unknown" : "unavailable");
      } finally {
        if (this.pendingPaste === gesture) this.pendingPaste = undefined;
        finish();
      }
    });
  }
}
function expectKeys(item: Record<string, unknown>, ...keys: string[]) {
  if (Object.keys(item).length !== keys.length || keys.some(key => !Object.prototype.hasOwnProperty.call(item, key))) {
    throw Error("Некоректна відповідь агента");
  }
}
function scopeId(value: unknown): string {
  const parsed = id(value);
  if (parsed === "00000000-0000-0000-0000-000000000000") throw Error("Некоректна прив’язка термінала");
  return parsed;
}
function tmuxId(value: unknown, prefix: string): string {
  if (typeof value !== "string" || value[0] !== prefix || !/^(0|[1-9][0-9]{0,9})$/.test(value.slice(1))) {
    throw Error("Некоректна прив’язка термінала");
  }
  integer(Number(value.slice(1)), 0, 2 ** 32 - 1);
  return value;
}
function parseTerminalScope(value: unknown): TerminalScope {
  const item = record(value);
  expectKeys(item, "view_id", "agent_epoch", "target");
  const target = record(item.target);
  expectKeys(target, "runtime_id", "generation", "session", "window", "pane", "pid", "process_start");
  return { view_id: scopeId(item.view_id), agent_epoch: scopeId(item.agent_epoch), target: {
    runtime_id: scopeId(target.runtime_id), generation: scopeId(target.generation),
    session: tmuxId(target.session, "$"), window: tmuxId(target.window, "@"), pane: tmuxId(target.pane, "%"),
    pid: integer(target.pid, 1, 2 ** 31 - 1), process_start: integer(target.process_start, 1, Number.MAX_SAFE_INTEGER),
  } };
}
function parseEpoch(value: unknown): ResizeEpoch {
  const item = record(value);
  expectKeys(item, "agent", "owner");
  return { agent: scopeId(item.agent), owner: scopeId(item.owner) };
}
function parseIntent(value: unknown): ResizeIntent {
  const item = record(value);
  expectKeys(item, "epoch", "id");
  return { epoch: parseEpoch(item.epoch), id: scopeId(item.id) };
}
function parseSize(value: unknown): TerminalSize {
  const item = record(value);
  expectKeys(item, "cols", "rows");
  return { cols: integer(item.cols, 2, 500), rows: integer(item.rows, 2, 200) };
}
function parseGridSample(value: unknown): GridSample {
  const item = record(value);
  expectKeys(item, "size", "sampled_at_us");
  return { size: parseSize(item.size), sampled_at_us: integer(item.sampled_at_us, 1, Number.MAX_SAFE_INTEGER) };
}
function sameSize(left: TerminalSize, right: TerminalSize): boolean {
  return left.cols === right.cols && left.rows === right.rows;
}
export function parseTerminalStatus(value: unknown): TerminalStatus {
  const item = record(value);
  expectKeys(item, "terminal", "epoch", "committed", "provider", "observation");
  const terminal = parseTerminalScope(item.terminal);
  const epoch = item.epoch === null ? null : parseEpoch(item.epoch);
  const committed = record(item.committed);
  expectKeys(committed, "size", "revision", "intent");
  const size = parseSize(committed.size);
  const revision = integer(committed.revision, 0, Number.MAX_SAFE_INTEGER);
  const intent = committed.intent === null ? null : parseIntent(committed.intent);
  if ((revision === 0) !== (intent === null) || (intent && !epoch)) throw Error("Некоректний намір зміни розміру");
  const provider = item.provider === null ? null : parseGridSample(item.provider);
  const observation = item.observation;
  if (observation !== "pending" && observation !== "observed" && observation !== "unavailable") throw Error("Некоректний стан розміру");
  if ((observation === "observed" && (!provider || !sameSize(size, provider.size)))
    || (observation === "unavailable" && provider)
    || (observation === "pending" && provider && sameSize(size, provider.size))) throw Error("Некоректний вимір розміру");
  return { terminal, epoch, committed: { size, revision, intent }, provider, observation };
}
export function parseResizeResult(value: unknown): ResizeResult {
  const item = record(value);
  if (item.result === "committed") {
    expectKeys(item, "result", "admission", "status");
    const status = parseTerminalStatus(item.status);
    const admission = parseIntent(item.admission);
    const intent = status.committed.intent;
    const epoch = status.epoch;
    if (!intent || !epoch || epoch.agent !== status.terminal.agent_epoch ||
      epoch.agent !== intent.epoch.agent || epoch.owner !== intent.epoch.owner ||
      admission.id !== intent.id || admission.epoch.agent !== epoch.agent || admission.epoch.owner !== epoch.owner) {
      throw Error("Некоректна відповідь на зміну розміру");
    }
    return { result: "committed", admission, status };
  }
  if (item.result === "uncertain" || item.result === "rejected") {
    expectKeys(item, "result", "terminal", "intent");
    const terminal = parseTerminalScope(item.terminal);
    const intent = parseIntent(item.intent);
    if (intent.epoch.agent !== terminal.agent_epoch) throw Error("Некоректна відповідь на зміну розміру");
    return { result: item.result, terminal, intent };
  }
  throw Error("Некоректний результат зміни розміру");
}
export function parseGeometryEvent(value: unknown): GeometryEvent {
  const item = record(value);
  expectKeys(item, "status", "bridge_pty", "client");
  const status = parseTerminalStatus(item.status);
  const bridge_pty = parseGridSample(item.bridge_pty);
  const client = parseGridSample(item.client);
  if (status.committed.revision < 1 || !sameSize(status.committed.size, bridge_pty.size)
    || !sameSize(status.committed.size, client.size)) throw Error("Розміри потоку термінала не збігаються");
  return { status, bridge_pty, client };
}
function parseLiveAttachment(value: unknown): Attachment & { status: TerminalStatus } {
  const attachment = parseAttachment(value);
  const item = record(value);
  expectKeys(item, "view_id", "cols", "rows", "status");
  const status = parseTerminalStatus(item.status);
  if (attachment.view_id !== status.terminal.view_id || !sameSize(attachment, status.committed.size)) {
    throw Error("Некоректна прив’язка термінала");
  }
  return { ...attachment, status };
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
  const id = result.runtime.runtime_id;
  const next = openView(views, id, [id]);
  return preserveActive && views.open.length > 0 ? { ...next, active: views.active } : next;
}

async function invokeFullscreen(action: "read" | "enter" | "exit" | "cancelExit" | "focusAfterExit", intent?: string): Promise<boolean> {
  const result = await invoke<unknown>("local_window_fullscreen", { action, intent });
  if (typeof result !== "boolean") throw Error("Некоректна відповідь повноекранного режиму");
  return result;
}

export const localClient = {
  inventory: async () => parseInventory(await invoke<unknown>("local_inventory")),
  windowFullscreen: () => invokeFullscreen("read"),
  enterWindowFullscreen: () => invokeFullscreen("enter"),
  exitWindowFullscreen: (intent: string) => invokeFullscreen("exit", intent),
  cancelWindowFullscreenExit: (intent: string) => invokeFullscreen("cancelExit", intent),
  focusWindowAfterFullscreenExit: (intent: string) => invokeFullscreen("focusAfterExit", intent),
  pickWorkspace: async () => parseWorkspaceSelection(await invoke<unknown>("local_pick_workspace")),
  workspaceReadiness: async (workspaceId: string, profile: ProviderProfile) =>
    parseWorkspaceReadiness(await invoke<unknown>("local_workspace_readiness", { workspaceId, profile })),
  createSession: async (workspaceId: string, profile: ProviderProfile, intent?: CreateIntent) =>
    parseCreateSubmission(await invoke<unknown>("local_create_session", { workspaceId, profile, ...intent })),
  createStatus: async (reference?: string) =>
    parseCreateStatus(await invoke<unknown>("local_create_status", reference ? { reference } : {})),
  acknowledgeCreate: async (reference: string) =>
    parseCreateStatus(await invoke<unknown>("local_create_status", { reference, presented: true })),
  attach: async (runtimeId: string, options?: TerminalAttachOptions) =>
    parseLiveAttachment(await invoke<unknown>("terminal_attach", { runtimeId, ...options })),
  read: async (viewId: string) => parseChunk(await invoke<unknown>("terminal_read", { viewId })),
  terminalStatus: async (viewId: string): Promise<TerminalStatus> =>
    parseTerminalStatus(await invoke<unknown>("terminal_status", { viewId })),
  resize: async (viewId: string, epoch: ResizeEpoch, size: TerminalSize) =>
    parseResizeResult(await invoke<unknown>("terminal_resize", { viewId, epoch, size })),
  acknowledgeGrid: async (viewId: string, acknowledgement: GridAcknowledgement) =>
    parseGeometryEvent(await invoke<unknown>("terminal_acknowledge_grid", { viewId, acknowledgement })),
  input: async ({ viewId, data, incarnation }: { viewId: string; data: readonly number[]; incarnation: string }) => {
    try {
      await invoke("terminal_input", { viewId, data, incarnation });
    } catch (error) {
      if (isStaleNativeInput(error, viewId)) throw new StaleNativeInput(viewId, incarnation);
      throw error;
    }
  },
  pasteStage: async (gesture: PasteGesture, offset: number, data: number[]) =>
    parsePasteResult(await invoke("terminal_paste_stage", { ...gesture, offset, data })),
  pasteCommit: async (gesture: PasteGesture, bytes: number) =>
    parsePasteResult(await invoke("terminal_paste_commit", { ...gesture, bytes })),
  pasteAck: async (gesture: PasteGesture, receipt: string) =>
    parsePasteResult(await invoke("terminal_paste_ack", { ...gesture, receipt })),
  detach: async (viewId: string, failure?: TerminalStreamFailure) => {
    await invoke("terminal_detach", { viewId, failure });
  },
};
export function runtimeTitle(runtime: RuntimeInfo): string {
  const command = runtime.command.toLowerCase();
  const name = command.includes("claude") ? "Claude Code" : command.includes("codex") ? "Codex" : runtime.command || "Завершена сесія";
  return `${name} · ${runtime.runtime_id.slice(0, 6)}`;
}
