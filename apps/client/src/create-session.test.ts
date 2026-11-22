import assert from "node:assert/strict";
import test from "node:test";
import { localClient, parseWorkspaceReadiness, parseWorkspaceSelection, workspaceIssueText } from "./local-client.ts";

const workspaceId = "11111111-1111-4111-8111-111111111111";
const workspace = { workspace_id: workspaceId, label: "Проєкт з пробілами" };

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
  const windowDescriptor = Object.getOwnPropertyDescriptor(globalThis, "window");
  Object.defineProperty(globalThis, "window", {
    configurable: true,
    value: {
      __TAURI_INTERNALS__: {
        invoke(command: string, args?: unknown) {
          calls.push([command, args]);
          if (command === "local_pick_workspace") return cancelled ? { result: "cancelled" } : { result: "selected", workspace };
          if (command === "local_workspace_readiness") return { result: "ready" };
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
    assert.deepEqual(await localClient.workspaceReadiness(workspaceId, "claude_code"), { result: "ready" });
    assert.deepEqual(await localClient.workspaceReadiness(workspaceId, "codex"), { result: "ready" });
    assert.deepEqual(calls, [
      ["local_pick_workspace", {}],
      ["local_pick_workspace", {}],
      ["local_workspace_readiness", { workspaceId, profile: "claude_code" }],
      ["local_workspace_readiness", { workspaceId, profile: "codex" }],
    ]);
  } finally {
    if (windowDescriptor) Object.defineProperty(globalThis, "window", windowDescriptor);
    else Reflect.deleteProperty(globalThis, "window");
  }
});
