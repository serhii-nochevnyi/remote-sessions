import type { Chunk, CreateStatus, Inventory, LocalCreate, RuntimeInfo, TerminalScope, TerminalViewScope, localClient } from "./local-client.ts";
import type { CreateExposure } from "./presentation-preferences.ts";
import type { OrderedGeometry } from "./terminal-geometry.ts";
import { sameSize, sameTerminal } from "./terminal-geometry.ts";

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
export type TerminalBindingState = "bound" | "proved" | "retired" | "changed" | "draining" | "view-closed" | "completed" | "ended";
export type AttachmentMode = "ordinary" | "observer";
export interface TerminalAttachmentIntent {
  readonly mode: "live" | "rollover";
  readonly identity: InventoryIdentity;
  readonly runtime: RuntimeInfo;
  readonly reconnect?: true;
}

export interface MountedTerminal {
  readonly mode: MountMode;
  readonly key?: string;
  readonly intent?: TerminalAttachmentIntent;
  readonly reconnectIntent?: TerminalAttachmentIntent;
  readonly observation?: BoundTerminal;
  readonly outputOnly?: BoundTerminal;
  readonly reconnectFrom?: BoundTerminal;
}

export type TerminalRecord = {
  readonly binding: BoundTerminal;
  readonly state: TerminalBindingState;
  readonly attempt: number;
  readonly reconnectFrom?: BoundTerminal;
} | { readonly binding: null; readonly state: "closed"; readonly attempt: number };

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
  if (!verified) {
    if (bound && state === "bound" && !rollover && matchesPane(runtime, bound.terminal) &&
        bound.identity.operationId !== null && bound.identity.workspaceId !== null &&
        runtime.origin === "unverified" && runtime.operation_id === null && runtime.workspace_id === null) {
      return { mode: "observe", key: bound.mountKey, observation: bound };
    }
    return { mode: "pending" };
  }
  const identity = {
    agentPid: inventory.agent_pid,
    sessionId: expected.sessionId,
    operationId: expected.operationId,
    workspaceId: runtime.workspace_id ?? bound?.identity.workspaceId ?? null,
  };
  const mode = rollover ? "rollover" : "live";
  return { mode, key: bound?.mountKey ?? `${id}:${identity.agentPid}`, intent: { mode, identity, runtime } };
}

export function terminalPanel(
  views: SessionViews,
  id: string,
  inventory: Inventory | null,
  expected: SessionIdentity | undefined,
  record: TerminalRecord | undefined,
  closed: boolean,
  selection: number,
): MountedTerminal {
  if (closed || record?.state === "closed" || !views.open.includes(id)) return { mode: "missing" };
  const reconnectFrom = record?.reconnectFrom;
  const reconnect = reconnectFrom !== undefined && record?.attempt === selection && views.active === id;
  const current = reconnect ? record : cancelTerminalReconnect(record);
  let mounted = mountedTerminal(id, inventory, expected, current?.binding ?? null, false,
    current?.state === "closed" ? undefined : current?.state, reconnect);
  if (reconnectFrom && reconnect && (record?.state === "bound" || record?.state === "proved") &&
      (mounted.mode === "rollover" || mounted.mode === "observe" || !mounted.key)) {
    mounted = { mode: "view-closed", key: reconnectFrom.mountKey };
  }
  if (reconnectFrom && reconnect && mounted.key) {
    const retained = mounted.mode === "draining" ? mounted.outputOnly : reconnectFrom;
    mounted = {
      ...mounted,
      reconnectFrom,
      ...(retained ? { outputOnly: retained } : {}),
      ...(mounted.intent ? { intent: { ...mounted.intent, reconnect: true } } : {}),
    };
  }
  if (views.active === id) return mounted;
  return mounted.outputOnly
    ? { mode: mounted.mode, key: mounted.outputOnly.mountKey, outputOnly: mounted.outputOnly } : { mode: "inactive" };
}

export function beginTerminalReconnect(mounted: MountedTerminal, binding: BoundTerminal, attempt: number): TerminalRecord | undefined {
  if (explicitClosedReconnect(mounted) === binding) return { binding, state: "view-closed", attempt, reconnectFrom: binding };
}

export function cancelTerminalReconnect(record: TerminalRecord | undefined): TerminalRecord | undefined {
  if (record?.state !== "closed" && record?.reconnectFrom) {
    return { binding: record.reconnectFrom, state: "view-closed", attempt: record.attempt };
  }
  return record;
}

export function refreshTerminalRecord(record: TerminalRecord, inventory: Inventory | null): TerminalRecord {
  if (!record.binding) return record;
  const { binding, reconnectFrom } = record;
  const row = inventory?.runtimes.find(runtime => runtime.runtime_id === binding.terminal.target.runtime_id);
  if (reconnectFrom && row && inventoryConflicts(row, binding.identity, binding)) {
    return { binding: reconnectFrom, state: "changed", attempt: record.attempt };
  }
  if (reconnectFrom && (!row || !inventoryMatches(row, binding.identity) ||
      (record.state === "proved" && (row.exited || inventory?.agent_pid !== binding.identity.agentPid)))) {
    return { binding: reconnectFrom, state: "view-closed", attempt: record.attempt };
  }
  if (!inventory) return record;
  const state = inventoryBindingState(binding, record.state, inventory);
  if (state === record.state) return record;
  return state === "draining" ? { ...record, state } : { binding, state, attempt: record.attempt };
}

export function updateTerminalRecord(
  record: TerminalRecord | undefined,
  mounted: MountedTerminal,
  binding: BoundTerminal,
  state: TerminalBindingState | "rendered",
  previous: BoundTerminal | null,
  attempt: number,
  selection: number,
): TerminalRecord | undefined {
  if ((record?.binding ?? null) !== previous || (record?.attempt ?? 0) !== attempt ||
      record?.state === "closed" || record?.state === "changed" || record?.state === "ended" || record?.state === "completed") return;
  const reconnectFrom = record?.reconnectFrom;
  if (reconnectFrom && attempt !== selection) return;
  if (state === "bound" || state === "proved") {
    const intent = mounted.intent;
    if (!intent || !sameInventoryIdentity(intent.identity, binding.identity) || !matchesPane(intent.runtime, binding.terminal)) return;
    return { binding, state, attempt, ...(reconnectFrom ? { reconnectFrom } : {}) };
  }
  if (state === "rendered") {
    if (record?.binding !== binding || !reconnectFrom || mounted.reconnectFrom !== reconnectFrom) return;
    return { binding, state: record.state, attempt };
  }
  if (reconnectFrom && (state === "retired" || state === "view-closed" || state === "ended")) return cancelTerminalReconnect(record);
  return { binding, state, attempt };
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

export interface ActiveTerminalEffect {
  readonly mode: AttachmentMode;
  readonly agent: number;
  binding?: BoundTerminal;
}

export function mountedTerminalEffect(
  mounted: MountedTerminal,
  active: ActiveTerminalEffect | undefined,
  mayRestore: boolean,
): { mode: AttachmentMode | undefined; agent: number | undefined } {
  if (mounted.intent) return {
    mode: desiredTerminalMode(mounted.intent, active?.mode, mayRestore),
    agent: mounted.intent.identity.agentPid,
  };
  if (mounted.outputOnly || (active?.binding &&
      mounted.observation === active.binding)) return { mode: active?.mode, agent: active?.agent };
  return { mode: undefined, agent: undefined };
}

export function mountedTerminalEntry(
  mounted: MountedTerminal,
  retained: BoundTerminal | undefined,
  mayRestore: boolean,
): AttachmentMode | undefined {
  if (retained && (mounted.reconnectFrom !== retained || !mounted.intent?.reconnect)) return;
  return desiredTerminalMode(mounted.intent, undefined, mayRestore);
}

export function terminalProofOnly(mounted: MountedTerminal, mode: AttachmentMode, mayRestore: boolean): boolean {
  return mode === "observer" && mounted.intent?.mode === "rollover" && mayRestore;
}

export function acceptMountedRenderer(
  mounted: MountedTerminal,
  binding: BoundTerminal,
  mode: AttachmentMode,
  mayRestore: boolean,
  retained: BoundTerminal | undefined,
  currentRetained: BoundTerminal | undefined,
  commit: () => boolean,
) {
  if (terminalStreamAccess(mounted, binding, mode, mayRestore) === "stale" || retained !== currentRetained) return false;
  if (retained && mounted.reconnectFrom !== retained) return false;
  return !mounted.reconnectFrom || commit();
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
  const freshRow = fresh?.runtimes.find(runtime => runtime.runtime_id === runtimeId);
  const corroborated = !needsFreshInventory || (fresh !== null && fresh.agent_pid === identity.agentPid &&
    freshRow !== undefined && sameRuntime(freshRow, intent.runtime));
  const latest = stillCurrent();
  if (!latest || !proofMatches || !corroborated) {
    if (attachment.view_id !== previous?.terminal.view_id) await client.detach(attachment.view_id).catch(() => undefined);
    if (latest && stillCurrent()) {
      if (!proofMatches) throw new TerminalAttachmentIdentityError(!sameTarget);
      if (fresh?.agent_pid === identity.agentPid && freshRow && inventoryConflicts(freshRow, identity, previous)) {
        throw new TerminalAttachmentIdentityError(true);
      }
    }
    return;
  }
  return { attachment, runtime: intent.runtime, binding: { identity, terminal, mountKey: previous?.mountKey ?? `${runtimeId}:${identity.agentPid}` }, isCurrent: () => {
    const latest = stillCurrent();
    return latest !== undefined && matchesPane(latest.runtime, terminal);
  } };
}

export async function adoptCurrentTerminal(
  runtimeId: string,
  previous: BoundTerminal | null,
  mode: AttachmentMode,
  current: (binding: BoundTerminal | null) => MountedTerminal,
  mayRestore: () => boolean,
  scope: TerminalViewScope,
  accept: (binding: BoundTerminal) => boolean,
  detached: Promise<void>,
  client: Pick<typeof localClient, "attach" | "detach" | "inventory">,
) {
  scope.setInputEligible(false);
  const candidate = await attachCurrentTerminal(runtimeId, previous, mode, () => current(previous).intent,
    mayRestore, () => scope.active, detached, client);
  if (!candidate) return;
  const { binding } = candidate;
  const { terminal } = binding;
  if (!candidate.isCurrent() || !scope.bind(terminal.view_id)) {
    await client.detach(terminal.view_id).catch(() => undefined);
    return;
  }
  if (!accept(binding)) {
    scope.retire();
    return;
  }
  return candidate;
}

const terminalDrainTimeout = 10_000;
const terminalDrainFrames = 17;

interface ConfirmationRecovery {
  readonly error: unknown;
  readonly deadline: number;
  readonly proof: Promise<{ readonly observing: boolean; readonly original: RuntimeInfo | undefined } | undefined>;
  continueEofDeadline(): void;
}

interface MountedConfirmation {
  readonly pending: boolean;
  readonly confirmed: boolean;
  readonly armed: boolean;
  readonly refused: boolean;
  readonly recovery: ConfirmationRecovery | undefined;
  readonly settled: Promise<void>;
  resumeObservation(): void;
  updateEligibility(access: StreamAccess): void;
  stop(): void;
}

function corroborateTerminalDrain(binding: BoundTerminal, inventory: Inventory | null) {
  if (!inventory) return;
  const { identity, terminal: { target } } = binding;
  const row = inventory.runtimes.find(runtime => runtime.runtime_id === target.runtime_id);
  if (inventory.agent_pid !== identity.agentPid || (row && inventoryConflicts(row, identity, binding))) return "changed";
  if (row && inventoryMatches(row, identity) && row.workspace_id === identity.workspaceId &&
      row.operation_id === identity.operationId && row.pane_id === target.pane &&
      (row.pane_pid === target.pid || (row.exited && row.pane_pid === null))) return row;
}

export function makeRefusedWriterReconnect(
  scope: TerminalViewScope,
  current: (binding: BoundTerminal) => MountedTerminal,
  mayRestore: () => boolean,
  reconnect: (binding: BoundTerminal) => boolean,
  client: Pick<typeof localClient, "inventory">,
) {
  let probing = false;
  return async (binding: BoundTerminal, confirmation: MountedConfirmation): Promise<boolean> => {
    const { terminal, identity } = binding;
    const currentIntent = () => {
      if (!scope.owns(terminal.view_id) || !confirmation.refused || confirmation.recovery || !mayRestore()) return;
      const intent = current(binding).intent;
      if (intent?.mode === "live" && !intent.reconnect && sameInventoryIdentity(intent.identity, identity) &&
          matchesPane(intent.runtime, terminal) && inventoryMatches(intent.runtime, identity) &&
          intent.runtime.workspace_id === identity.workspaceId && intent.runtime.operation_id === identity.operationId) return intent;
    };
    if (probing) return false;
    const intent = currentIntent();
    if (!intent) return false;
    probing = true;
    try {
      // @security: Retiring a refused reader requires fresh native inventory for its exact live binding.
      const fresh = await client.inventory().catch(() => null);
      const latest = currentIntent();
      if (!latest || !sameRuntime(latest.runtime, intent.runtime) || fresh?.agent_pid !== identity.agentPid) return false;
      const row = fresh.runtimes.find(runtime => runtime.runtime_id === terminal.target.runtime_id);
      return row !== undefined && sameRuntime(row, latest.runtime) && reconnect(binding);
    } finally {
      probing = false;
    }
  };
}

export function confirmMountedWriter(
  candidate: { readonly binding: BoundTerminal; readonly runtime: RuntimeInfo },
  mode: AttachmentMode,
  current: () => MountedTerminal,
  mayRestore: () => boolean,
  stream: Pick<MountedStream, "scope" | "access" | "revokeMutations" | "identityChanged">,
  recovering: () => void,
  client: Pick<typeof localClient, "confirmWriter" | "inventory" | "terminalStatus">,
): MountedConfirmation {
  const { scope } = stream;
  const { binding, runtime } = candidate;
  const { terminal, identity } = binding;
  const canConfirm = () => {
    const intent = current().intent;
    return mode === "ordinary" && scope.owns(terminal.view_id) && stream.access() === "attached" &&
      mayRestore() && intent?.mode === "live" &&
      sameInventoryIdentity(intent.identity, identity) && sameRuntime(intent.runtime, runtime) &&
      matchesPane(intent.runtime, terminal);
  };
  let pending = canConfirm();
  let confirmed = false;
  let armed = false;
  let refused = false;
  let recovery: ConfirmationRecovery | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  function updateEligibility(access: StreamAccess) {
    if (access !== "attached") armed = false;
    else if (!armed && confirmed && canConfirm()) {
      // @invariant: Rearming invalidates parked reads and requires a subsequent native writer sample.
      scope.setInputEligible(true);
      armed = true;
    }
  }
  async function confirm() {
    try {
      await client.confirmWriter(terminal, {
        agent_pid: identity.agentPid, session_id: identity.sessionId,
        operation_id: identity.operationId, workspace_id: identity.workspaceId,
      });
    } catch (error) {
      if (!scope.owns(terminal.view_id) || updateMountedStream(stream) === "stale") return;
      const deadline = performance.now() + terminalDrainTimeout;
      // @security: This candidate stays disarmed even if inventory enrichment or a writable sample returns.
      refused = true;
      scope.setInputEligible(false);
      stream.revokeMutations();
      const hasMountedObservation = () => current().observation === binding;
      const wasObserving = hasMountedObservation();
      const active = () => {
        if (!scope.owns(terminal.view_id) || updateMountedStream(stream) === "stale") return false;
        if (performance.now() < deadline) return true;
        scope.retire({ error });
        return false;
      };
      const armDeadline = () => {
        timer = setTimeout(() => { scope.retire({ error }); }, Math.max(0, Math.ceil(deadline - performance.now())));
      };
      async function classify() {
        const inventory = await client.inventory().catch(() => null);
        if (!active()) return;
        const row = inventory?.runtimes.find(row => row.runtime_id === terminal.target.runtime_id);
        const original = corroborateTerminalDrain(binding, inventory);
        if (original === "changed" || (row && !row.exited && !matchesPane(row, terminal))) {
          stream.identityChanged();
        } else {
          const degraded = row?.origin === "unverified" && identity.operationId !== null && identity.workspaceId !== null;
          const canObserve = stream.access() !== "draining" &&
            (inventory === null || Boolean(row && !row.exited && (degraded || original))) &&
            (degraded || wasObserving || hasMountedObservation());
          if (canObserve) {
            const status = await client.terminalStatus(terminal.view_id).catch(() => null);
            if (!active()) return;
            if (status && !sameTerminal(terminal, status.terminal)) {
              stream.identityChanged();
              scope.retire({ error });
              return;
            }
            if (status && stream.access() !== "draining") {
              clearTimeout(timer);
              return { observing: true, original };
            }
          }
          // @contract: A failed live status probe still permits the separately corroborated EOF path.
          if (original) return { observing: false, original };
        }
        scope.retire({ error });
      }
      armDeadline();
      recovery = { error, deadline, proof: classify(), continueEofDeadline: armDeadline };
      recovering();
      return;
    } finally {
      pending = false;
    }
    if (!scope.owns(terminal.view_id)) return;
    const access = updateMountedStream(stream);
    if (access === "stale") return;
    confirmed = true;
    updateEligibility(access);
  }
  const settled = pending ? confirm() : Promise.resolve();
  return {
    get pending() { return pending; },
    get confirmed() { return confirmed; },
    get armed() { return armed; },
    get refused() { return refused; },
    get recovery() { return recovery; },
    settled,
    resumeObservation() {
      clearTimeout(timer);
      recovery = undefined;
    },
    updateEligibility,
    stop() { clearTimeout(timer); },
  };
}

type StreamAccess = "attached" | "observing" | "draining" | "stale";

export function terminalStreamAccess(
  mounted: MountedTerminal,
  binding: BoundTerminal,
  attached: AttachmentMode,
  mayRestore: boolean,
): StreamAccess {
  if (mounted.mode === "draining" && mounted.outputOnly === binding) return "draining";
  if (mounted.observation === binding) return "observing";
  if (mounted.intent && (!mounted.outputOnly || mounted.reconnectFrom === mounted.outputOnly) &&
      sameInventoryIdentity(mounted.intent.identity, binding.identity) &&
      matchesPane(mounted.intent.runtime, binding.terminal) &&
      desiredTerminalMode(mounted.intent, attached, mayRestore) === attached) return "attached";
  return "stale";
}

interface MountedStream {
  readonly binding: BoundTerminal;
  readonly scope: TerminalViewScope;
  readonly routing: OrderedGeometry;
  readonly sink: Parameters<OrderedGeometry["consume"]>[1];
  readonly confirmation: MountedConfirmation;
  inventory(): Promise<Inventory>;
  access(): ReturnType<typeof terminalStreamAccess>;
  acceptRenderer(): boolean;
  revokeMutations(): void;
  read(view: string): Promise<Chunk>;
  consume(view: string, chunk: Chunk, render: () => Promise<void>): Promise<boolean>;
  rendered(chunk: Chunk): void;
  eof(state: "view-closed" | "completed"): void;
  closed(): void;
  identityChanged(): void;
}

export function updateMountedStream(
  stream: Pick<MountedStream, "scope" | "access" | "revokeMutations">,
  confirmation?: MountedConfirmation,
) {
  const access = stream.access();
  if (access !== "attached") {
    stream.scope.setInputEligible(false);
    stream.revokeMutations();
    if (access === "stale") stream.scope.retire();
  }
  confirmation?.updateEligibility(access);
  return access;
}

export async function runMountedTerminalStream(stream: MountedStream) {
  const { scope, routing, sink, binding, confirmation } = stream;
  const terminal = binding.terminal;
  const view = terminal.view_id;
  const active = () => scope.owns(view) && updateMountedStream(stream, confirmation) !== "stale";
  const permitted = (release?: ConfirmationRecovery) => active() && confirmation.recovery === release;
  const readOnly = (chunk: Chunk): Chunk => {
    const { paste_incarnation, paste_completion, ...observation } = chunk;
    return { ...observation, writable: false };
  };
  function finish(exited: boolean) {
    if (!stream.acceptRenderer()) {
      scope.retire();
      return;
    }
    scope.setInputEligible(false);
    stream.revokeMutations();
    stream.eof(exited ? "completed" : "view-closed");
    scope.retire();
  }
  function validateScope(chunk: Chunk) {
    if ((chunk.status && !sameTerminal(terminal, chunk.status.terminal)) ||
        (chunk.geometry && !sameTerminal(terminal, chunk.geometry.status.terminal))) {
      stream.identityChanged();
      throw Error("Стан термінала змінився");
    }
  }
  async function render(sample: Chunk, release?: ConfirmationRecovery) {
    let submitted = false;
    const rendered = await stream.consume(view, sample, async () => {
      if (!permitted(release)) return;
      if (stream.access() === "draining" && sample.geometry) throw Error("Потік перервано");
      const accept = () => {
        if (!permitted(release)) return false;
        if (stream.acceptRenderer()) return true;
        scope.retire();
        return false;
      };
      const guardGrid = () => {
        if (!permitted(release) || stream.access() === "draining") throw Error("Потік перервано");
      };
      await routing.consume(sample, {
        async write(data) {
          if (!accept()) return;
          submitted = true;
          await sink.write(data);
        },
        resize(size) {
          guardGrid();
          if (!accept()) throw Error("Потік перервано");
          submitted = true;
          return sink.resize(size);
        },
        acknowledge(event, size) {
          guardGrid();
          return sink.acknowledge(event, size);
        },
      });
      if (sample.writable && sample.paste_incarnation) accept();
    });
    if (rendered && permitted(release)) stream.rendered(sample);
    return { rendered, submitted };
  }

  async function recover(recovery: ConfirmationRecovery, head?: Chunk) {
    const held: Chunk[] = [];
    const withinBudget = () => {
      if (!permitted(recovery)) return false;
      if (performance.now() >= recovery.deadline) throw recovery.error;
      return true;
    };
    const proof = await recovery.proof;
    if (!permitted(recovery) || !proof) return;
    if (head) validateScope(head);
    if (head?.closure && head.closure.reason !== "eof") {
      stream.closed();
      scope.acceptClosure(view, head.closure);
      return;
    }
    if (proof.observing && !head?.closure && !head?.closed && stream.access() !== "draining") {
      confirmation.resumeObservation();
      return !head || (await render(readOnly(head))).rendered;
    }
    if (!withinBudget()) return;
    // @contract: An EOF racing live proof keeps the original absolute deadline.
    if (proof.observing) recovery.continueEofDeadline();
    const { original } = proof;
    if (!original) throw recovery.error;
    let chunk = head;
    while (withinBudget()) {
      chunk ??= await stream.read(view);
      if (!withinBudget()) return;
      validateScope(chunk);
      if (chunk.closure?.reason === "eof") break;
      if (chunk.closure) stream.closed();
      if (chunk.closed || chunk.writable || chunk.geometry) throw Error("Потік перервано");
      const committed = chunk.status?.committed;
      const grid = routing.status.committed;
      if (committed && (committed.revision !== routing.applied || !sameSize(committed.size, grid.size) ||
          committed.intent?.id !== grid.intent?.id || committed.intent?.epoch.agent !== grid.intent?.epoch.agent ||
          committed.intent?.epoch.owner !== grid.intent?.epoch.owner)) throw Error("Стан термінала змінився");
      if (chunk.data.length) {
        if (held.length >= terminalDrainFrames) throw Error("Потік перервано");
        held.push({ data: chunk.data, closed: false, writable: false });
      }
      chunk = undefined;
    }
    if (!withinBudget()) return;
    const latest = await stream.inventory();
    if (!withinBudget()) return;
    const final = corroborateTerminalDrain(binding, latest);
    if (final === "changed") stream.identityChanged();
    if (!final || final === "changed" || (original.exited && !final.exited)) throw recovery.error;
    confirmation.stop();
    for (const frame of held) {
      if (!permitted(recovery) || !(await render(frame, recovery)).rendered) return;
    }
    if (permitted(recovery)) finish(final.exited);
  }

  try {
    while (active()) {
      if (confirmation.recovery) {
        if (!await recover(confirmation.recovery)) return;
        continue;
      }
      const checkEligibility = scope.captureReadEligibility();
      const wasArmed = confirmation.armed;
      const chunk = await stream.read(view);
      if (!active()) return;
      if (confirmation.pending && chunk.closure?.reason === "eof") await confirmation.settled;
      if (!active()) return;
      if (confirmation.recovery) {
        if (!await recover(confirmation.recovery, chunk)) return;
        continue;
      }
      const access = stream.access();
      if (chunk.closure) {
        if (chunk.closure.reason === "eof") finish(access === "draining");
        else {
          stream.closed();
          scope.acceptClosure(view, chunk.closure);
        }
        return;
      }
      validateScope(chunk);
      const sample = access !== "attached" || !wasArmed || !confirmation.armed || checkEligibility(chunk) !== chunk
        ? readOnly(chunk) : chunk;
      const { rendered, submitted } = await render(sample);
      if (!active()) return;
      if (confirmation.recovery) {
        if (!await recover(confirmation.recovery, submitted ? undefined : chunk)) return;
        continue;
      }
      if (!rendered) return;
    }
  } finally {
    confirmation.stop();
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
