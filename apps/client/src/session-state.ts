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
  readonly agentPid: number;
  readonly terminal: TerminalScope;
}

export function mountedTerminal(
  id: string,
  inventory: Inventory | null,
  expected: { operationId: string | null; sessionId: string } | undefined,
  bound: BoundTerminal | null,
  verified: TerminalScope | null | undefined,
  checking: boolean,
): { mode: "live" | "observe" | "pending" | "offline" | "missing" | "exited"; key?: string } {
  const runtime = inventory?.runtimes.find(entry => entry.runtime_id === id);
  if (inventory && !runtime) return { mode: "missing" };
  if (runtime && expected && (runtime.session_id !== expected.sessionId ||
      (runtime.operation_id && runtime.operation_id !== expected.operationId))) return { mode: "missing" };
  if (runtime?.exited) return { mode: "exited" };
  const enriched = runtime && (!expected || (runtime.operation_id ?? null) === expected.operationId);

  if (!bound || bound.terminal.target.runtime_id !== id) {
    if (!inventory) return { mode: "offline" };
    return enriched ? { mode: "live", key: `${id}:${inventory.agent_pid}` } : { mode: "missing" };
  }

  const { terminal, agentPid } = bound;
  if ((inventory && inventory.agent_pid !== agentPid) ||
      (runtime && (runtime.pane_pid !== terminal.target.pid || (runtime.pane_id && runtime.pane_id !== terminal.target.pane))) ||
      verified === null || (verified && !sameTerminal(terminal, verified))) return { mode: "missing" };

  const key = `${id}:${agentPid}`;
  if (!verified && (checking || !enriched)) return { mode: "pending", key };
  return { mode: enriched ? "live" : "observe", key };
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
import type { Inventory, TerminalScope } from "./local-client.ts";
import { sameTerminal } from "./terminal-geometry.ts";
