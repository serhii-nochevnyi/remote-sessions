import type { Chunk, CreateStatus, Inventory, LocalCreate, RuntimeInfo, TerminalScope, TerminalViewScope, localClient } from "./local-client.ts";
import type { CreateExposure } from "./presentation-preferences.ts";
import type { OrderedGeometry } from "./terminal-geometry.ts";
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
  readonly mountKey: string;
}

interface SessionIdentity {
  readonly sessionId: string;
  readonly operationId: string | null;
}

export interface InventoryIdentity extends SessionIdentity {
  readonly agentPid: number;
  readonly workspaceId: string | null;
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

type MountMode = "live" | "observe" | "rollover" | "draining" | "view-closed" | "completed" | "pending" | "offline" | "missing" | "exited" | "inactive";
export type TerminalBindingState = "bound" | "retired" | "changed" | "draining" | "view-closed" | "completed" | "ended";
export type AttachmentMode = "ordinary" | "observer";
export interface TerminalAttachmentIntent {
  readonly mode: "live" | "observe" | "rollover";
  readonly identity: InventoryIdentity;
  readonly runtime: RuntimeInfo;
  readonly reconnect?: true;
}

export interface MountedTerminal {
  readonly mode: MountMode;
  readonly key?: string;
  readonly intent?: TerminalAttachmentIntent;
  readonly reconnectIntent?: TerminalAttachmentIntent;
  readonly outputOnly?: BoundTerminal;
}

function inventoryMatches(runtime: RuntimeInfo, expected: SessionIdentity) {
  return runtime.session_id === expected.sessionId && (expected.operationId === null
    ? runtime.origin === "legacy"
    : runtime.origin === "managed" && runtime.operation_id === expected.operationId && Boolean(runtime.workspace_id));
}

function sameInventoryIdentity(left: InventoryIdentity, right: InventoryIdentity) {
  return left.agentPid === right.agentPid && sameSessionIdentity(left, right) && left.workspaceId === right.workspaceId;
}

function sameSessionIdentity(left: SessionIdentity, right: SessionIdentity) {
  return left.sessionId === right.sessionId && left.operationId === right.operationId;
}

function inventoryConflicts(runtime: RuntimeInfo, expected: SessionIdentity, bound: BoundTerminal | null) {
  return runtime.session_id !== expected.sessionId ||
    (runtime.operation_id != null && runtime.operation_id !== expected.operationId) ||
    (runtime.origin === "legacy" && expected.operationId !== null) ||
    (runtime.origin === "managed" && expected.operationId === null) ||
    (bound !== null && (!sameSessionIdentity(bound.identity, expected) ||
      runtime.runtime_id !== bound.terminal.target.runtime_id ||
      (runtime.pane_id !== undefined && runtime.pane_id !== bound.terminal.target.pane) ||
      (runtime.pane_pid !== null && runtime.pane_pid !== bound.terminal.target.pid) ||
      (runtime.workspace_id != null && runtime.workspace_id !== bound.identity.workspaceId)));
}

function matchesPane(runtime: RuntimeInfo, terminal: TerminalScope) {
  return runtime.runtime_id === terminal.target.runtime_id && runtime.pane_id === terminal.target.pane &&
    runtime.pane_pid === terminal.target.pid && !runtime.exited;
}

export function mountedTerminal(
  id: string,
  inventory: Inventory | null,
  expected: SessionIdentity | undefined,
  bound: BoundTerminal | null,
  closed: boolean,
  state: TerminalBindingState = "bound",
  reconnect = false,
): MountedTerminal {
  if (closed || state === "changed") return { mode: "missing" };
  if (bound && expected && !sameSessionIdentity(bound.identity, expected)) return { mode: "missing" };
  const runtime = inventory?.runtimes.find(entry => entry.runtime_id === id);
  const priorIdentity = expected ?? bound?.identity;
  if (priorIdentity && runtime && inventoryConflicts(runtime, priorIdentity, bound)) return { mode: "missing" };
  if (bound && state === "view-closed") {
    const retained = { key: bound.mountKey, outputOnly: bound };
    if (inventory && runtime && inventoryMatches(runtime, bound.identity) && runtime.pane_id === bound.terminal.target.pane) {
      if (runtime.exited && inventory.agent_pid === bound.identity.agentPid) return { ...retained, mode: "completed" };
      if (expected && matchesPane(runtime, bound.terminal)) {
        const reconnectIntent: TerminalAttachmentIntent = {
          mode: inventory.agent_pid === bound.identity.agentPid ? "live" : "rollover",
          identity: { ...bound.identity, agentPid: inventory.agent_pid }, runtime, reconnect: true,
        };
        return { ...retained, mode: "view-closed", reconnectIntent, ...(reconnect ? { intent: reconnectIntent } : {}) };
      }
    }
    return { ...retained, mode: "view-closed" };
  }
  if (bound && state === "completed") return { mode: "completed", key: bound.mountKey, outputOnly: bound };
  if (state === "ended") return { mode: "exited" };
  if (bound && (state === "draining" || (state === "bound" && runtime?.exited))) {
    if (!inventory || (inventory.agent_pid === bound.identity.agentPid && (runtime
      ? inventoryMatches(runtime, bound.identity) && runtime.pane_id === bound.terminal.target.pane &&
        (runtime.pane_pid === bound.terminal.target.pid || runtime.exited)
      : state === "draining"))) {
      return { mode: "draining", key: bound.mountKey, outputOnly: bound };
    }
    return { mode: "exited" };
  }
  if (inventory && !runtime) return { mode: "missing" };
  if (runtime?.exited) return { mode: "exited" };
  if (!expected) return { mode: inventory ? "pending" : "offline" };
  if (!inventory || !runtime) return bound ? { mode: "pending", key: bound.mountKey } : { mode: "offline" };
  if (!runtime.pane_id || !runtime.pane_pid) return { mode: "missing" };
  const verified = inventoryMatches(runtime, expected);
  const rollover = bound !== null && bound.identity.agentPid !== inventory.agent_pid;
  if (rollover && !verified) return { mode: "pending", key: bound.mountKey };
  const identity = {
    agentPid: inventory.agent_pid,
    sessionId: expected.sessionId,
    operationId: expected.operationId,
    workspaceId: runtime.workspace_id ?? bound?.identity.workspaceId ?? null,
  };
  const mode = rollover ? "rollover" : verified ? "live" : bound ? "observe" : "pending";
  if (mode === "pending") return { mode };
  return { mode, key: bound?.mountKey ?? `${id}:${identity.agentPid}`, intent: { mode, identity, runtime } };
}

export function terminalPanel(
  views: SessionViews,
  id: string,
  inventory: Inventory | null,
  expected: SessionIdentity | undefined,
  bound: BoundTerminal | null,
  closed: boolean,
  state?: TerminalBindingState,
  reconnect = false,
): MountedTerminal {
  if (closed || !views.open.includes(id)) return { mode: "missing" };
  const mounted = mountedTerminal(id, inventory, expected, bound, false, state, reconnect);
  if (views.active === id) return mounted;
  return mounted.outputOnly
    ? { mode: mounted.mode, key: mounted.outputOnly.mountKey, outputOnly: mounted.outputOnly } : { mode: "inactive" };
}

export function inventoryBindingState(
  previous: BoundTerminal,
  state: TerminalBindingState,
  inventory: Inventory,
): TerminalBindingState {
  const id = previous.terminal.target.runtime_id;
  const next = mountedTerminal(id, inventory, previous.identity, previous, false, state);
  if (next.mode === "draining") return "draining";
  if (state === "view-closed") {
    if (next.mode === "completed") return "completed";
    if (next.mode === "missing") return "changed";
    return state;
  }
  if (state !== "draining" && state !== "completed") return state;
  if (next.mode === "completed") return state;
  return next.mode === "missing" && inventory.runtimes.some(entry => entry.runtime_id === id) ? "changed" : "ended";
}

export function requiresTerminalReopen(state: TerminalBindingState | "closed" | undefined) {
  return state === "retired" || state === "view-closed";
}

export function explicitClosedReconnect(mounted: MountedTerminal) {
  return mounted.reconnectIntent && !mounted.intent ? mounted.outputOnly : undefined;
}

export function explicitTerminalOpen(
  status: CreateStatus | null,
  runtime: RuntimeInfo,
  exposure: CreateExposure | undefined,
  bound: BoundTerminal | null,
  state: TerminalBindingState | undefined,
) {
  const decision = state === "changed" || (bound && (inventoryConflicts(runtime, bound.identity, bound) ||
    (!runtime.exited && !matchesPane(runtime, bound.terminal))))
    ? "blocked" : inventoryOpen(status, runtime, exposure, "user");
  return { decision, retry: decision !== "blocked" && requiresTerminalReopen(state) && !runtime.exited };
}

export function desiredTerminalMode(
  intent: TerminalAttachmentIntent | undefined,
  active: AttachmentMode | undefined,
  mayRestore: boolean,
): AttachmentMode | undefined {
  if (!intent) return;
  return intent.mode === "live" && (mayRestore || (active === "ordinary" && !intent.reconnect)) ? "ordinary" : "observer";
}

export class TerminalAttachmentIdentityError extends Error {
  readonly replacement: boolean;

  constructor(replacement: boolean) {
    super("Terminal attachment identity changed");
    this.replacement = replacement;
  }
}

export async function attachCurrentTerminal(
  runtimeId: string,
  previous: BoundTerminal | null,
  mode: AttachmentMode,
  current: () => TerminalAttachmentIntent | undefined,
  mayRestore: () => boolean,
  active: () => boolean,
  detached: Promise<void>,
  client: Pick<typeof localClient, "attach" | "detach" | "inventory">,
) {
  const admissible = () => {
    const intent = current();
    if (!active() || !intent || desiredTerminalMode(intent, undefined, mayRestore()) !== mode ||
        intent.runtime.runtime_id !== runtimeId || (mode === "observer" && !previous) ||
        (previous && (!sameSessionIdentity(previous.identity, intent.identity) ||
          previous.identity.workspaceId !== intent.identity.workspaceId ||
          !matchesPane(intent.runtime, previous.terminal) ||
          (intent.mode === "rollover"
            ? mode !== "observer" || previous.identity.agentPid === intent.identity.agentPid ||
              !inventoryMatches(intent.runtime, previous.identity)
            : !sameInventoryIdentity(previous.identity, intent.identity))))) return;
    return intent;
  };
  await detached;
  const intent = admissible();
  if (!intent) return;
  const { identity } = intent;
  const stillCurrent = () => {
    const latest = admissible();
    return latest && latest.mode === intent.mode && latest.reconnect === intent.reconnect && sameInventoryIdentity(identity, latest.identity) &&
      sameRuntime(latest.runtime, intent.runtime) ? latest : undefined;
  };
  const options = mode === "observer" ? { observer: true as const }
    : previous ? { restoration: { previous: previous.terminal, agent_pid: identity.agentPid,
      session_id: identity.sessionId, operation_id: identity.operationId } } : undefined;
  const attachment = await client.attach(runtimeId, options).catch((error: unknown) => {
    if (stillCurrent()) throw error;
  });
  if (!attachment) return;
  const terminal = attachment.status.terminal;
  const rollover = intent.mode === "rollover";
  const sameTarget = !previous || sameTerminal({ ...previous.terminal, view_id: terminal.view_id,
    agent_epoch: terminal.agent_epoch }, terminal);
  const matchesAttempt = attachment.view_id === terminal.view_id && (!previous ||
    (terminal.view_id !== previous.terminal.view_id && rollover === (terminal.agent_epoch !== previous.terminal.agent_epoch)));
  const proofMatches = matchesPane(intent.runtime, terminal) && sameTarget && matchesAttempt;
  const needsFreshInventory = rollover || intent.reconnect;
  const fresh = needsFreshInventory && proofMatches && stillCurrent() ? await client.inventory().catch(() => null) : null;
  const corroborated = !needsFreshInventory || (fresh !== null && fresh.agent_pid === identity.agentPid &&
    fresh.runtimes.some(runtime => sameRuntime(runtime, intent.runtime)));
  const latest = stillCurrent();
  if (!latest || !proofMatches || !corroborated) {
    if (attachment.view_id !== previous?.terminal.view_id) await client.detach(attachment.view_id).catch(() => undefined);
    if (latest && !proofMatches && stillCurrent()) throw new TerminalAttachmentIdentityError(!sameTarget);
    return;
  }
  return { attachment, binding: { identity, terminal, mountKey: previous?.mountKey ?? `${runtimeId}:${identity.agentPid}` }, isCurrent: () => {
    const latest = stillCurrent();
    return latest !== undefined && matchesPane(latest.runtime, terminal);
  } };
}

export function terminalStreamAccess(
  mounted: MountedTerminal,
  binding: BoundTerminal,
  attached: AttachmentMode,
  mayRestore: boolean,
): "attached" | "draining" | "stale" {
  if (mounted.mode === "draining" && mounted.outputOnly === binding) return "draining";
  if (!mounted.outputOnly && mounted.intent && sameInventoryIdentity(mounted.intent.identity, binding.identity) &&
      matchesPane(mounted.intent.runtime, binding.terminal) &&
      desiredTerminalMode(mounted.intent, attached, mayRestore) === attached) return "attached";
  return "stale";
}

interface MountedStream {
  readonly binding: BoundTerminal;
  readonly scope: TerminalViewScope;
  readonly routing: OrderedGeometry;
  readonly sink: Parameters<OrderedGeometry["consume"]>[1];
  access(): ReturnType<typeof terminalStreamAccess>;
  revokeMutations(): void;
  read(view: string): Promise<Chunk>;
  consume(view: string, chunk: Chunk, render: () => Promise<void>): Promise<boolean>;
  rendered(chunk: Chunk): void;
  eof(state: "view-closed" | "completed"): void;
  closed(): void;
  identityChanged(): void;
}

export function updateMountedStream(stream: Pick<MountedStream, "scope" | "access" | "revokeMutations">) {
  const access = stream.access();
  if (access !== "attached") {
    stream.scope.setInputEligible(false);
    stream.revokeMutations();
    if (access === "stale") stream.scope.retire();
  }
  return access;
}

export async function runMountedTerminalStream(stream: MountedStream) {
  const { scope, routing, sink, binding } = stream;
  const terminal = binding.terminal;
  const view = terminal.view_id;
  while (scope.owns(view) && updateMountedStream(stream) !== "stale") {
    const checkEligibility = scope.captureReadEligibility();
    const chunk = await stream.read(view);
    if (!scope.owns(view)) return;
    const access = updateMountedStream(stream);
    if (access === "stale") return;
    if (chunk.closure) {
      if (chunk.closure.reason === "eof") {
        scope.setInputEligible(false);
        stream.revokeMutations();
        stream.eof(access === "draining" ? "completed" : "view-closed");
        scope.retire();
      } else {
        stream.closed();
        scope.acceptClosure(view, chunk.closure);
      }
      return;
    }
    if ((chunk.status && !sameTerminal(terminal, chunk.status.terminal)) ||
        (chunk.geometry && !sameTerminal(terminal, chunk.geometry.status.terminal))) {
      stream.identityChanged();
      throw Error("Стан термінала змінився");
    }
    const { paste_incarnation, paste_completion, ...readOnly } = chunk;
    const sample = access === "draining" ? { ...readOnly, writable: false } : checkEligibility(chunk);
    const rendered = await stream.consume(view, sample, async () => {
      const current = updateMountedStream(stream);
      if (current === "stale") return;
      if (current === "draining" && chunk.geometry) throw Error("Потік перервано");
      await routing.consume(sample, sink);
    });
    if (!rendered || updateMountedStream(stream) === "stale") return;
    stream.rendered(sample);
  }
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
