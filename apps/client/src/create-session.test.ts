import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import type { TestContext } from "node:test";
import { CreateSession } from "./create-session.ts";
import type { ReadinessStatus } from "./create-session.ts";
import {
  CreateReadiness, localClient, mergeCreateOperations, parseCreateStatus, parseCreateSubmission, parseWorkspaceReadiness,
  parseWorkspaceSelection, selectCreatedRuntime, workspaceIssueText,
} from "./local-client.ts";
import type { CreateIntent, ProviderProfile } from "./local-client.ts";
import { closeView, openView } from "./session-state.ts";
import type { SessionViews } from "./session-state.ts";

const workspaceId = "11111111-1111-4111-8111-111111111111";
const workspace = { workspace_id: workspaceId, label: "Проєкт з пробілами" };
const reference = "22222222-2222-4222-8222-222222222222";
const separateReference = "33333333-3333-4333-8333-333333333333";
const runtimeId = "44444444-4444-4444-8444-444444444444";
const sessionId = "55555555-5555-4555-8555-555555555555";
const runtime = { runtime_id: runtimeId, session_id: sessionId, command: "codex", pane_pid: 101, exited: false, width: 100, height: 30 };
const unknown = { result: "unknown", issue: null };
const launched = { result: "launched", runtime, current: "live", readiness: "unobserved" };
function operation(disposition: unknown, id = reference) {
  return { reference: id, workspace_id: workspaceId, profile: "codex", presented: false, disposition };
}

test("w1_readiness validates selection and excludes locator and runtime data", () => {
  assert.deepEqual(parseWorkspaceSelection({ result: "cancelled" }), { result: "cancelled" });
  assert.deepEqual(parseWorkspaceSelection({ result: "selected", workspace }), { result: "selected", workspace });
  assert.throws(() => parseWorkspaceSelection({ result: "cancelled", workspace }));
  assert.throws(() => parseWorkspaceSelection({ result: "selected", workspace: { ...workspace, locator: "/private/path" } }));
  assert.throws(() => parseWorkspaceSelection({ result: "selected", workspace: { ...workspace, workspace_id: "../project" } }));
  assert.throws(() => parseWorkspaceSelection({ result: "selected", workspace: { ...workspace, label: "\u0000project" } }));
  assert.throws(() => parseWorkspaceSelection({ result: "ready" }));
  assert.throws(() => parseWorkspaceReadiness({ result: "ready", executable: "/private/bin/codex" }));
  assert.throws(() => parseWorkspaceReadiness({ result: "blocked", issue: "toString" }));
  assert.throws(() => parseWorkspaceReadiness({ result: "started", runtime_id: workspaceId }));
});

test("create decodes distinct dispositions and keeps host keys out of native replies", () => {
  for (const disposition of [
    { result: "pending" },
    { result: "failed", issue: "workspace_changed" },
    { result: "unknown", issue: "catalog_missing" },
    unknown,
    launched,
    { result: "exited", runtime: { ...runtime, exited: true }, current: "exited", setup_issue: "workspace_changed" },
    { result: "exited", runtime: { ...runtime, exited: true }, current: "unavailable", setup_issue: null },
  ]) {
    const reply = { result: "recorded", operation: operation(disposition) };
    assert.deepEqual(parseCreateSubmission(reply), reply);
  }
  assert.deepEqual(parseCreateSubmission({ result: "blocked", issue: "pending_unavailable" }),
    { result: "blocked", issue: "pending_unavailable" });
  assert.throws(() => parseCreateStatus({ operations: [{ ...operation(unknown), key: reference }], issue: null }));
  assert.throws(() => parseCreateStatus({ operations: [operation({ ...launched, readiness: "ready" })], issue: null }));
  assert.throws(() => parseCreateStatus({ operations: [operation(unknown), operation(unknown)], issue: null }));
});

test("inventory first and create result first select the same runtime once", () => {
  const [confirmed] = parseCreateStatus({ operations: [operation(launched)], issue: null }).operations;
  assert.ok(confirmed);
  const empty: SessionViews = { open: [], active: null };
  const inventoryFirst = openView(empty, runtimeId, [runtimeId]);
  const createFirst = selectCreatedRuntime(empty, confirmed, new Set());
  assert.deepEqual(selectCreatedRuntime(inventoryFirst, confirmed, new Set()), createFirst);
  const userSelection = openView(createFirst, sessionId, [sessionId]);
  assert.equal(selectCreatedRuntime(userSelection, confirmed, new Set(), true).active, sessionId);
  const handled = new Set([reference]);
  assert.equal(selectCreatedRuntime(userSelection, confirmed, handled), userSelection);
  const closed = closeView(userSelection, runtimeId);
  assert.equal(selectCreatedRuntime(closed, confirmed, handled), closed);
});

test("a deliberate second operation and reference-scoped refresh preserve both outcomes", async () => {
  const calls: unknown[] = [];
  const windowDescriptor = Object.getOwnPropertyDescriptor(globalThis, "window");
  Object.defineProperty(globalThis, "window", {
    configurable: true,
    value: {
      __TAURI_INTERNALS__: {
        invoke(command: string, args?: Record<string, unknown>) {
          calls.push([command, args]);
          if (command === "local_create_status") {
            if (!args?.reference) return { operations: [operation(unknown)], issue: null };
            return { operations: [args.reference === reference ? operation(launched) : operation(unknown, separateReference)], issue: null };
          }
          if (command === "local_create_session") return { result: "recorded", operation: operation(unknown, separateReference) };
          throw Error("Unexpected native command");
        },
      },
    },
  });
  try {
    const recovered = await localClient.createStatus();
    const separate = await localClient.createSession(workspaceId, "codex", { separateFrom: reference });
    if (separate.result !== "recorded") throw Error("Unexpected blocked operation");
    let merged = mergeCreateOperations(recovered.operations, [separate.operation]);
    merged = mergeCreateOperations(merged, (await localClient.createStatus(reference)).operations);
    merged = mergeCreateOperations(merged, (await localClient.createStatus(separateReference)).operations);
    assert.deepEqual(merged.map(item => [item.reference, item.disposition.result]),
      [[reference, "launched"], [separateReference, "unknown"]]);
    assert.deepEqual(calls, [
      ["local_create_status", {}],
      ["local_create_session", { workspaceId, profile: "codex", separateFrom: reference }],
      ["local_create_status", { reference }],
      ["local_create_status", { reference: separateReference }],
    ]);
  } finally {
    if (windowDescriptor) Object.defineProperty(globalThis, "window", windowDescriptor);
    else Reflect.deleteProperty(globalThis, "window");
  }
});

test("w1_readiness accepts typed failures with recovery text", () => {
  for (const [issue, text] of Object.entries(workspaceIssueText)) {
    assert.ok(text.length > 0);
    const response = { result: "blocked", issue };
    assert.deepEqual(parseWorkspaceSelection(response), response);
    assert.deepEqual(parseWorkspaceReadiness(response), response);
  }
});

test("w1_readiness uses the picker without inputs and sends only ID and profile for checks", async () => {
  const calls: unknown[] = [];
  let cancelled = true;
  let providerAvailable = false;
  const windowDescriptor = Object.getOwnPropertyDescriptor(globalThis, "window");
  Object.defineProperty(globalThis, "window", {
    configurable: true,
    value: {
      __TAURI_INTERNALS__: {
        invoke(command: string, args?: unknown) {
          calls.push([command, args]);
          if (command === "local_pick_workspace") return cancelled ? { result: "cancelled" } : { result: "selected", workspace };
          if (command === "local_workspace_readiness") return providerAvailable ? { result: "ready" } :
            { result: "blocked", issue: "provider_missing" };
          throw Error("Unexpected native command");
        },
      },
    },
  });
  try {
    assert.deepEqual(await localClient.pickWorkspace(), { result: "cancelled" });
    assert.deepEqual(calls, [["local_pick_workspace", {}]]);
    cancelled = false;
    assert.deepEqual(await localClient.pickWorkspace(), { result: "selected", workspace });
    assert.deepEqual(await localClient.workspaceReadiness(workspaceId, "codex"), { result: "blocked", issue: "provider_missing" });
    providerAvailable = true;
    assert.deepEqual(await localClient.workspaceReadiness(workspaceId, "claude_code"), { result: "ready" });
    assert.deepEqual(await localClient.workspaceReadiness(workspaceId, "codex"), { result: "ready" });
    assert.deepEqual(calls, [
      ["local_pick_workspace", {}],
      ["local_pick_workspace", {}],
      ["local_workspace_readiness", { workspaceId, profile: "codex" }],
      ["local_workspace_readiness", { workspaceId, profile: "claude_code" }],
      ["local_workspace_readiness", { workspaceId, profile: "codex" }],
    ]);
  } finally {
    if (windowDescriptor) Object.defineProperty(globalThis, "window", windowDescriptor);
    else Reflect.deleteProperty(globalThis, "window");
  }
});

test("draft readiness permits one current create and rejects blocked, stale and cancelled checks", () => {
  const gate = new CreateReadiness();
  const calls: string[] = [];
  const submit = (id: string, profile: "claude_code" | "codex") => {
    if (gate.take(id, profile)) calls.push(profile);
  };

  const missing = gate.begin(workspaceId, "claude_code");
  assert.equal(gate.finish(missing, { result: "blocked", issue: "provider_missing" }), true);
  submit(workspaceId, "claude_code");
  const stale = gate.begin(workspaceId, "claude_code");
  const otherFolder = gate.begin(separateReference, "claude_code");
  const current = gate.begin(workspaceId, "codex");
  assert.equal(gate.finish(stale, { result: "ready" }), false);
  assert.equal(gate.finish(otherFolder, { result: "ready" }), false);
  submit(workspaceId, "codex");
  assert.equal(gate.finish(current, { result: "ready" }), true);
  submit(separateReference, "codex");
  submit(workspaceId, "claude_code");
  submit(workspaceId, "codex");
  submit(workspaceId, "codex");
  const cancelled = gate.begin(workspaceId, "codex");
  gate.invalidate();
  assert.equal(gate.finish(cancelled, { result: "ready" }), false);
  submit(workspaceId, "codex");
  assert.deepEqual(calls, ["codex"]);
});

function deferred() {
  let resolve!: (value: unknown) => void;
  const promise = new Promise<unknown>(complete => { resolve = complete; });
  return { promise, resolve };
}

function creationFixture(t: TestContext) {
  const calls: unknown[] = [];
  const picker = deferred();
  const checks: ReturnType<typeof deferred>[] = [];
  const statuses: ReadinessStatus[] = [];
  const drafts: CreateSession["draft"][] = [];
  const creation = new CreateSession(draft => drafts.push(draft), status => statuses.push(status));
  const originalWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
  Object.defineProperty(globalThis, "window", {
    configurable: true,
    value: {
      __TAURI_INTERNALS__: {
        invoke(command: string, args?: unknown) {
          calls.push([command, args]);
          if (command === "local_pick_workspace") return picker.promise;
          if (command === "local_workspace_readiness") {
            const request = deferred();
            checks.push(request);
            return request.promise;
          }
          if (command === "local_create_session") return { result: "recorded", operation: operation(unknown) };
          throw Error(`Unexpected native command ${command}`);
        },
      },
    },
  });
  t.after(() => {
    if (originalWindow) Object.defineProperty(globalThis, "window", originalWindow);
    else Reflect.deleteProperty(globalThis, "window");
  });

  const submit = (id = workspaceId, profile: ProviderProfile = "codex", intent?: CreateIntent) =>
    creation.submit(id, profile, intent, async send => { await send(); });
  const completeCheck = (index: number) => {
    const request = checks[index];
    assert.ok(request, `Missing readiness request ${index}`);
    request.resolve({ result: "ready" });
  };

  creation.select(workspace, "codex");
  const initialCheck = creation.check(creation.draft);
  return { creation, picker, checks, statuses, drafts, calls, submit, initialCheck, completeCheck };
}

const readyResult = { result: "ready" };
const codexCheck = ["local_workspace_readiness", { workspaceId, profile: "codex" }];

test("ready folder cancellation preserves the draft and one create permission with only a picker call", async t => {
  const { creation, picker, statuses, drafts, calls, submit, initialCheck, completeCheck } = creationFixture(t);
  completeCheck(0);
  await initialCheck;
  const draft = creation.draft;
  const before = [...calls];
  const visible = [...statuses];
  assert.deepEqual(before, [codexCheck]);
  assert.deepEqual(visible, ["pending", readyResult]);

  const choice = creation.chooseWorkspace(() => true);
  assert.equal(creation.allows(workspaceId, "codex"), true);
  picker.resolve({ result: "cancelled" });
  assert.equal(await choice, undefined);
  assert.equal(creation.draft, draft);
  assert.equal(drafts.length, 1);
  assert.deepEqual(statuses, visible);
  assert.deepEqual(calls, [...before, ["local_pick_workspace", {}]]);

  await submit();
  assert.equal(submit(), undefined);
  assert.deepEqual(calls.slice(before.length), [
    ["local_pick_workspace", {}],
    ["local_create_session", { workspaceId, profile: "codex" }],
  ]);
});

test("in-flight folder cancellation accepts the original readiness completion without another request", async t => {
  const { creation, picker, statuses, drafts, calls, submit, initialCheck, completeCheck } = creationFixture(t);
  const draft = creation.draft;
  const before = [...calls];
  const choice = creation.chooseWorkspace(() => true);
  assert.deepEqual(statuses, ["pending"]);
  picker.resolve({ result: "cancelled" });
  await choice;
  assert.equal(creation.draft, draft);
  assert.equal(drafts.length, 1);
  assert.equal(submit(), undefined);
  assert.deepEqual(statuses, ["pending"]);
  completeCheck(0);
  await initialCheck;
  assert.deepEqual(statuses, ["pending", readyResult]);
  assert.equal(creation.allows(workspaceId, "codex"), true);
  assert.deepEqual(before, [codexCheck]);
  assert.deepEqual(calls, [...before, ["local_pick_workspace", {}]]);
});

test("same-folder selection, Retry, profile change and reconnect fence create by current readiness generation", async t => {
  const { creation, picker, checks, statuses, drafts, calls, submit, initialCheck, completeCheck } = creationFixture(t);
  const choice = creation.chooseWorkspace(() => true);
  picker.resolve({ result: "selected", workspace });
  assert.deepEqual(await choice, { result: "selected", workspace });
  completeCheck(0);
  await initialCheck;
  assert.deepEqual(statuses, ["pending"]);
  assert.equal(submit(), undefined);
  const selectedCheck = creation.check(creation.draft);

  creation.retry();
  const retryCheck = creation.check(creation.draft);
  creation.select(workspace, "claude_code");
  const profileCheck = creation.check(creation.draft);
  creation.invalidate();
  const reconnectCheck = creation.check(creation.draft);
  assert.equal(new Set(drafts).size, 4);
  const claudeCheck = ["local_workspace_readiness", { workspaceId, profile: "claude_code" }];
  const expected = [codexCheck, ["local_pick_workspace", {}], codexCheck, codexCheck, claudeCheck, claudeCheck];
  assert.deepEqual(calls, expected);
  assert.equal(checks.length, 5);

  completeCheck(3);
  completeCheck(1);
  completeCheck(2);
  await Promise.all([profileCheck, selectedCheck, retryCheck]);
  assert.deepEqual(statuses, Array(5).fill("pending"));
  assert.equal(submit(workspaceId, "claude_code"), undefined);
  assert.equal(submit(), undefined);
  assert.deepEqual(calls, expected);

  completeCheck(4);
  await reconnectCheck;
  assert.equal(submit(separateReference, "claude_code"), undefined);
  assert.equal(submit(), undefined);
  await submit(workspaceId, "claude_code", { separateFrom: reference });
  assert.equal(submit(workspaceId, "claude_code"), undefined);
  assert.deepEqual(calls, [...expected, ["local_create_session", { workspaceId, profile: "claude_code", separateFrom: reference }]]);
});

test("blocked selection clears readiness while deliberate continuation uses its saved reference", async t => {
  const { creation, picker, statuses, calls, submit, initialCheck, completeCheck } = creationFixture(t);
  const choice = creation.chooseWorkspace(() => true);
  const blocked = { result: "blocked", issue: "workspace_changed" };
  picker.resolve(blocked);
  assert.deepEqual(await choice, blocked);
  assert.equal(creation.draft.workspace, null);
  await creation.check(creation.draft);
  completeCheck(0);
  await initialCheck;
  assert.deepEqual(statuses, ["pending", null]);
  assert.equal(submit(), undefined);
  await submit(workspaceId, "codex", { continueReference: reference });
  assert.deepEqual(calls, [
    codexCheck, ["local_pick_workspace", {}],
    ["local_create_session", { workspaceId, profile: "codex", continueReference: reference }],
  ]);
});

test("dismissal ignores late picker and readiness results", async t => {
  const { creation, picker, statuses, calls, submit, initialCheck, completeCheck } = creationFixture(t);
  let mounted = true;
  const choice = creation.chooseWorkspace(() => mounted);
  const draft = creation.draft;
  creation.invalidate();
  mounted = false;
  picker.resolve({ result: "selected", workspace });
  completeCheck(0);
  assert.equal(await choice, undefined);
  await initialCheck;
  assert.equal(creation.draft, draft);
  assert.deepEqual(statuses, ["pending"]);
  assert.equal(submit(), undefined);
  assert.deepEqual(calls, [codexCheck, ["local_pick_workspace", {}]]);
});

test("dialog wires picker, readiness effect and submit to the tested controller", () => {
  const source = readFileSync(new URL("./CreateSessionDialog.tsx", import.meta.url), "utf8");
  assert.match(source, /new CreateSession\(setDraft, setReadinessStatus\)/);
  assert.match(source, /useLayoutEffect\(\(\) => \{\s*void creation\.check\(draft\);\s*return \(\) => creation\.invalidate\(\);\s*\}, \[creation, draft, connection\]\);/);
  assert.match(source, /return act\("picker", async \(\) => \{\s*const choice = await creation\.chooseWorkspace\(\(\) => mounted\.current && dialogRef\.current !== null\);/);
  assert.match(source, /onClick=\{\(\) => void chooseWorkspace\(\)\}/);
  assert.match(source, /return creation\.submit\(id, selectedProfile, intent, send => \{\s*const present = captureSubmission\(\);\s*return act\("submit", async \(\) => \{\s*const reply = await send\(\);/);
  assert.match(source, /onClick=\{\(\) => creation\.retry\(\)\}/);
  assert.match(source, /function selectProfile\(next: ProviderProfile\) \{\s*creation\.select\(workspace, next\);/);
  assert.match(source, /function dismiss\(\) \{\s*creation\.invalidate\(\);\s*mounted\.current = false;/);
  assert.doesNotMatch(source, /localClient\.(?:pickWorkspace|workspaceReadiness|createSession)\(/);
});
