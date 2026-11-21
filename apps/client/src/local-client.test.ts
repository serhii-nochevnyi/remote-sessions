import assert from "node:assert/strict";
import test from "node:test";
import { parseInventory, parseAttachment, parseChunk } from "./local-client.ts";
import { closeView, openView } from "./session-state.ts";
const id = "11111111-1111-4111-8111-111111111111";
test("real view identities survive disconnection and close only removes the view", () => {
  const inventory = parseInventory({ agent_pid: 12, runtimes: [{ runtime_id: id, session_id: id, command: "codex", pane_pid: 15, exited: false, width: 100, height: 30 }] });
  const views = openView({ open: [], active: null }, id, inventory.runtimes.map(r => r.runtime_id));
  assert.deepEqual(views, { open: [id], active: id });
  assert.deepEqual(closeView(views, id), { open: [], active: null });
  assert.equal(inventory.runtimes[0]?.pane_pid, 15);
  assert.deepEqual(openView(views, "unknown", [id]), views);
});
test("native stream boundaries reject invalid IDs, dimensions and byte floods", () => {
  assert.throws(() => parseAttachment({ view_id: "../shell", cols: 100, rows: 30 }));
  assert.throws(() => parseAttachment({ view_id: id, cols: 10000, rows: 30 }));
  assert.throws(() => parseChunk({ data: new Array<number>(8193).fill(1), writable: false, closed: false }));
  assert.throws(() => parseChunk({ data: [256], writable: false, closed: false }));
  assert.throws(() => parseInventory({ agent_pid: 0, runtimes: [] }));
  assert.deepEqual(parseChunk({ data: [0xe6, 0xbc], writable: false, closed: false }).data, [0xe6, 0xbc]);
});
