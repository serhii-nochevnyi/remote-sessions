import type { CreateStatus, Inventory, LocalCreate, RuntimeInfo, TerminalScope, TerminalViewScope, localClient } from "./local-client.ts";
import type { CreateExposure } from "./presentation-preferences.ts";
import { sameTerminal } from "./terminal-geometry.ts";

export interface DemoSession {
  readonly id: string;
  readonly name: string;
  readonly agent: "Claude Code" | "Codex" | "Shell";
  readonly project: string;
  readonly output: string;
}

export const demoSessions: readonly DemoSession[] = [
  { id: "claude-auth", name: "Авторизація", agent: "Claude Code", project: "api-service", output: "[Демонстраційні дані · CLI не запущено]\n\n❯ claude\n\n❯ Додай тести авторизації користувачів\n\nRead  src/auth/session.ts\nRead  tests/auth.test.ts\n\nТри сценарії для перевірки:\n  • чинний токен\n  • прострочений токен\n  • відсутній токен\n\nЦе приклад виведення для перевірки рендерера." },
  { id: "codex-tests", name: "Тести входу", agent: "Codex", project: "web-app", output: "[Демонстраційні дані · CLI не запущено]\n\n❯ codex\n\n❯ Додай тести форми входу\n\nRead  src/LoginForm.tsx\nWrite tests/login.test.ts\n\n✓ порожні поля\n✓ помилковий пароль\n✓ успішний вхід\n\nРезультати тестів тут ілюстративні." },
  { id: "shell-server", name: "Dev server", agent: "Shell", project: "api-service", output: "[Демонстраційні дані · shell не запущено]\n\n❯ npm run dev\n\n> api-service dev\n> tsx watch src/server.ts\n\nServer listening on localhost:3000\n\nGET /health       200\nPOST /auth/login  200\n\nЦе приклад журналу; сервер не запускався." },
];

export interface SessionViews {
  readonly open: readonly string[];
  readonly active: string | null;
}

export interface TerminalFocusTarget<T> {
  readonly runtimeId: string;
  readonly epoch: number;
  readonly textarea: T;
}

export function canRestoreTerminalFocus<T extends { readonly isConnected: boolean }>(
  captured: TerminalFocusTarget<T>,
  current: TerminalFocusTarget<T> | null,
  ownsFocusIntent: boolean,
): boolean {
  return ownsFocusIntent && captured.textarea.isConnected && current !== null &&
    captured.runtimeId === current.runtimeId && captured.epoch === current.epoch &&
    captured.textarea === current.textarea;
}

export async function restoreFullscreenExitFocus(
  currentTarget: () => Pick<HTMLElement, "focus"> | null,
  focusWebview: () => Promise<boolean>,
  windowHasFocus: () => boolean,
): Promise<void> {
  const target = currentTarget();
  if (target && await focusWebview() && currentTarget() === target && windowHasFocus()) {
    target.focus({ preventScroll: true });
  }
}

export function openView(state: SessionViews, id: string, knownIds: readonly string[] = demoSessions.map(session => session.id)): SessionViews {
  if (!knownIds.includes(id)) return state;
  return { open: state.open.includes(id) ? state.open : [...state.open, id], active: id };
}

export function viewAvailability(
  inventory: readonly { runtime_id: string; operation_id?: string | null; exited: boolean }[] | null,
  id: string,
  operationId?: string,
): "offline" | "missing" | "exited" | "live" {
  if (!inventory) return "offline";
  const runtime = inventory.find(item => item.runtime_id === id && (!operationId || item.operation_id === operationId));
  return runtime ? (runtime.exited ? "exited" : "live") : "missing";
}

export interface BoundTerminal {
  readonly identity: InventoryIdentity;
  readonly terminal: TerminalScope;
}

export interface InventoryIdentity {
  readonly agentPid: number;
  readonly sessionId: string;
  readonly operationId: string | null;
}

export function sameRuntime(current: RuntimeInfo, expected: RuntimeInfo): boolean {
  return current.origin === expected.origin && current.runtime_id === expected.runtime_id &&
    current.session_id === expected.session_id && current.pane_pid === expected.pane_pid &&
    current.pane_id === expected.pane_id && current.operation_id === expected.operation_id &&
    current.workspace_id === expected.workspace_id && current.exited === expected.exited;
}

export function inventoryOpen(
  status: CreateStatus | null,
  runtime: RuntimeInfo,
  exposure: CreateExposure | undefined,
  source: "saved" | "user",
): "blocked" | "inventory" | LocalCreate {
  if (source === "saved" && exposure && exposure.state !== "presented") return "blocked";
  if (runtime.origin !== "legacy" && runtime.origin !== "managed") return "blocked";
  for (const operation of status?.operations ?? []) {
    const result = operation.disposition;
    if (result.result !== "launched" && result.result !== "exited") continue;
    const known = result.runtime;
    if (known.runtime_id === runtime.runtime_id || (runtime.operation_id && known.operation_id === runtime.operation_id)) {
      return !status?.issue && sameRuntime(runtime, known) ? operation : "blocked";
    }
  }
  if (exposure) return "blocked";
  if (runtime.origin === "legacy") return "inventory";
  return status && !status.issue && runtime.operation_id && runtime.workspace_id &&
    !status.operations.some(operation => operation.workspace_id === runtime.workspace_id &&
      (operation.disposition.result === "unknown" || operation.disposition.result === "pending"))
    ? "inventory" : "blocked";
}

type MountMode = "live" | "observe" | "pending" | "offline" | "missing" | "exited";
export type AttachmentMode = "ordinary" | "observer";
export interface TerminalAttachmentIntent {
  readonly mode: "live" | "observe";
  readonly identity: InventoryIdentity;
  readonly runtime: RuntimeInfo;
}

function inventoryMatches(runtime: RuntimeInfo, expected: Pick<InventoryIdentity, "sessionId" | "operationId">) {
  return runtime.session_id === expected.sessionId && (expected.operationId === null
    ? runtime.origin === "legacy"
    : runtime.origin === "managed" && runtime.operation_id === expected.operationId && Boolean(runtime.workspace_id));
}

function sameInventoryIdentity(left: InventoryIdentity, right: InventoryIdentity) {
  return left.agentPid === right.agentPid && left.sessionId === right.sessionId && left.operationId === right.operationId;
}

function matchesPane(runtime: RuntimeInfo, terminal: TerminalScope) {
  return runtime.runtime_id === terminal.target.runtime_id && runtime.pane_id === terminal.target.pane &&
    runtime.pane_pid === terminal.target.pid && !runtime.exited;
}

export function mountedTerminal(
  id: string,
  inventory: Inventory | null,
  expected: Pick<InventoryIdentity, "operationId" | "sessionId"> | undefined,
  bound: BoundTerminal | null,
  closed: boolean,
): { mode: MountMode; key?: string; intent?: TerminalAttachmentIntent } {
  if (closed) return { mode: "missing" };
  const runtime = inventory?.runtimes.find(entry => entry.runtime_id === id);
  if (inventory && !runtime) return { mode: "missing" };
  if (runtime?.exited) return { mode: "exited" };
  if (!expected) return { mode: inventory ? "pending" : "offline" };
  if (!inventory || !runtime) return bound ? { mode: "pending", key: `${id}:${bound.identity.agentPid}` } : { mode: "offline" };
  if (runtime.session_id !== expected.sessionId ||
      (runtime.operation_id && runtime.operation_id !== expected.operationId) ||
      (runtime.origin === "legacy" && expected.operationId !== null) ||
      !runtime.pane_id || !runtime.pane_pid) return { mode: "missing" };
  const identity = {
    agentPid: inventory.agent_pid,
    sessionId: expected.sessionId,
    operationId: expected.operationId,
  };
  if (bound && (!sameInventoryIdentity(bound.identity, identity) || !matchesPane(runtime, bound.terminal))) return { mode: "missing" };
  const mode = inventoryMatches(runtime, expected) ? "live" : bound ? "observe" : "pending";
  if (mode === "pending") return { mode };
  return { mode, key: `${id}:${identity.agentPid}`, intent: { mode, identity, runtime } };
}

export function desiredTerminalMode(
  intent: TerminalAttachmentIntent | undefined,
  active: AttachmentMode | undefined,
  mayRestore: boolean,
): AttachmentMode | undefined {
  if (!intent) return;
  return intent.mode === "live" && (active === "ordinary" || mayRestore) ? "ordinary" : "observer";
}

export function updateTerminalEligibility(
  scope: Pick<TerminalViewScope, "setInputEligible" | "retire">,
  attached: AttachmentMode,
  desired: AttachmentMode | undefined,
) {
  if (desired === attached) return;
  scope.setInputEligible(false);
  scope.retire();
}

export async function attachCurrentTerminal(
  runtimeId: string,
  previous: BoundTerminal | null,
  mode: AttachmentMode,
  current: () => TerminalAttachmentIntent | undefined,
  mayRestore: () => boolean,
  active: () => boolean,
  detached: Promise<void>,
  client: Pick<typeof localClient, "attach" | "detach">,
) {
  const admissible = () => {
    const intent = current();
    if (!active() || !intent || desiredTerminalMode(intent, undefined, mayRestore()) !== mode ||
        intent.runtime.runtime_id !== runtimeId || (mode === "observer" && !previous) ||
        (previous && (!sameInventoryIdentity(previous.identity, intent.identity) || !matchesPane(intent.runtime, previous.terminal)))) return;
    return intent;
  };
  await detached;
  const intent = admissible();
  if (!intent) return;
  const { identity } = intent;
  const attachment = await client.attach(runtimeId, mode === "observer" ? { observer: true }
    : previous ? { restoration: { previous: previous.terminal, agent_pid: identity.agentPid,
      session_id: identity.sessionId, operation_id: identity.operationId } } : undefined);
  const terminal = attachment.status.terminal;
  const sameTarget = matchesPane(intent.runtime, terminal) && (!previous ||
    (terminal.view_id !== previous.terminal.view_id &&
      sameTerminal({ ...previous.terminal, view_id: terminal.view_id }, terminal)));
  const stillCurrent = () => {
    const latest = admissible();
    return latest && sameInventoryIdentity(identity, latest.identity) ? latest : undefined;
  };
  const latest = stillCurrent();
  if (!latest || !matchesPane(latest.runtime, terminal) || !sameTarget) {
    await client.detach(attachment.view_id).catch(() => undefined);
    if (latest && stillCurrent()) throw Error("Terminal attachment identity changed");
    return;
  }
  return { attachment, binding: { identity, terminal }, isCurrent: () => {
    const latest = stillCurrent();
    return latest !== undefined && matchesPane(latest.runtime, terminal);
  } };
}

export function tabStop(state: SessionViews, focused: string | null): string | null {
  return focused !== null && state.open.includes(focused) ? focused : state.active ?? state.open[0] ?? null;
}

export function tabKey(state: SessionViews, id: string, key: string): { action: "focus" | "activate"; id: string } | undefined {
  const index = state.open.indexOf(id);
  if (index < 0) return;
  if (key === "Enter" || key === " ") return { action: "activate", id };

  const count = state.open.length;
  const next = key === "ArrowLeft" ? state.open[(index + count - 1) % count]
    : key === "ArrowRight" ? state.open[(index + 1) % count]
    : key === "Home" ? state.open[0]
    : key === "End" ? state.open[count - 1] : undefined;
  if (next !== undefined) return { action: "focus", id: next };
}

export function adjacentView(state: SessionViews, id: string, direction: -1 | 1): string | undefined {
  const index = state.open.indexOf(id);
  return index < 0 ? undefined : state.open[index + direction];
}

export function moveView(state: SessionViews, id: string, direction: -1 | 1): SessionViews {
  const adjacent = adjacentView(state, id, direction);
  if (!adjacent) return state;
  const open = [...state.open];
  const index = open.indexOf(id);
  open[index] = adjacent;
  open[index + direction] = id;
  return { ...state, open };
}

export function focusAfterClose(state: SessionViews, id: string, focused: string | null): string | null {
  if (focused !== id) return focused;
  return adjacentView(state, id, 1) ?? adjacentView(state, id, -1) ?? null;
}

// Views contain no runtime records, so closing one cannot delete a session.
export function closeView(state: SessionViews, id: string): SessionViews {
  if (!state.open.includes(id)) return state;
  const open = state.open.filter(item => item !== id);
  return { open, active: focusAfterClose(state, id, state.active) };
}

export interface ScaffoldStatus {
  readonly protocolVersion: 1;
  readonly appVersion: string;
  readonly stage: "scaffold";
  readonly capabilities: readonly [];
}

export function parseScaffoldStatus(value: unknown): ScaffoldStatus {
  if (typeof value !== "object" || value === null ||
      !("protocolVersion" in value) || value.protocolVersion !== 1 ||
      !("appVersion" in value) || typeof value.appVersion !== "string" || value.appVersion.length === 0 ||
      !("stage" in value) || value.stage !== "scaffold" ||
      !("capabilities" in value) || !Array.isArray(value.capabilities) || value.capabilities.length !== 0 ||
      Object.keys(value).length !== 4) {
    throw new Error("Unsupported scaffold status");
  }
  return { protocolVersion: 1, appVersion: value.appVersion, stage: "scaffold", capabilities: [] };
}
