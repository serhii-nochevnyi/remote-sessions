import assert from "node:assert/strict";
import test from "node:test";
import { localClient, selectCreatedRuntime } from "./local-client.ts";
import type { LocalCreate } from "./local-client.ts";
import { loadPresentation, parsePresentation, savePresentation, zoomFont, zoomShortcut } from "./presentation-preferences.ts";
import { moveView, viewAvailability } from "./session-state.ts";

const a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const c = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const defaults = { views: { open: [], active: null }, fontSize: 14, expanded: false };
const record = { version: 1, open: [c, a, b], active: b, fontSize: 18, expanded: true };

test("the actual reordered tabs and selection survive relaunch and delayed inventory", () => {
  const saved = new Map<string, string>();
  const storage = {
    setItem: (key: string, value: string) => { saved.set(key, value); },
    getItem: (key: string) => saved.get(key) ?? null,
  };
  const starting = { open: [a, b, c], active: b };
  const userViews = moveView(moveView(starting, c, -1), c, -1);
  const presentation = {
    views: { ...userViews, focused: c }, fontSize: 18, expanded: true,
    fullscreen: true, writer: "lease", stream: [1, 2, 3], pending: c,
  };
  savePresentation(presentation, storage);
  assert.deepEqual([...saved.values()].map(value => JSON.parse(value)), [record]);

  const restored = loadPresentation(storage);
  assert.deepEqual(restored, { views: { open: [c, a, b], active: b }, fontSize: 18, expanded: true });
  assert.deepEqual(restored.views.open.map(id => viewAvailability(null, id)), ["offline", "offline", "offline"]);
  const inventory = [
    { runtime_id: a, exited: false, operation_id: null },
    { runtime_id: b, exited: false, operation_id: c },
    { runtime_id: c, exited: false, operation_id: null },
  ];
  assert.deepEqual(restored.views.open.map(id => viewAvailability(inventory, id)), ["live", "live", "live"]);
  assert.equal(viewAvailability(inventory, restored.views.active ?? "", c), "live");
  assert.equal(viewAvailability(inventory, b, a), "missing");
  const laterInventory = [{ runtime_id: a, exited: true }, { runtime_id: b, exited: false }];
  assert.deepEqual(restored.views.open.map(id => viewAvailability(laterInventory, id)), ["missing", "exited", "live"]);
  assert.deepEqual(loadPresentation(storage), restored);
});

test("a recovered create already present in saved order preserves a null selection", () => {
  const restored = parsePresentation({ ...record, active: null });
  assert.ok(restored);
  const operation: LocalCreate = {
    reference: c, workspace_id: a, profile: "codex", presented: false,
    disposition: {
      result: "launched", current: "live", readiness: "unobserved",
      runtime: { runtime_id: b, session_id: b, command: "codex", pane_pid: 42, exited: false, width: 80, height: 24 },
    },
  };
  assert.deepEqual(selectCreatedRuntime(restored.views, operation, new Set(), true), restored.views);
});

test("version, shape, UUID membership, duplicate identity and font bounds are validated", () => {
  for (const invalid of [
    null, [], {}, { ...record, version: 2 }, { ...record, open: a }, { ...record, open: ["A"] },
    { ...record, open: [a, a], active: a }, { ...record, open: [a, a.toUpperCase()], active: a },
    { ...record, active: "dddddddd-dddd-4ddd-8ddd-dddddddddddd" }, { ...record, active: false },
    { ...record, fontSize: 9 }, { ...record, fontSize: 29 }, { ...record, fontSize: 14.5 }, { ...record, fontSize: "14" },
    { ...record, expanded: 1 }, { ...record, fullscreen: false }, { ...record, pending: [] },
  ]) {
    assert.equal(parsePresentation(invalid), undefined);
  }
  assert.deepEqual(parsePresentation({ ...record, active: null, fontSize: 10 })?.views, { open: [c, a, b], active: null });
  assert.equal(parsePresentation({ ...record, active: b.toUpperCase(), fontSize: 28 })?.views.active, b);
});

test("malformed or unavailable storage uses defaults and failed writes leave live preferences intact", () => {
  assert.deepEqual(loadPresentation({ getItem: () => null }), defaults);
  assert.deepEqual(loadPresentation({ getItem: () => "{" }), defaults);
  assert.deepEqual(loadPresentation({ getItem: () => { throw Error("storage unavailable"); } }), defaults);
  const current = loadPresentation({ getItem: () => JSON.stringify(record) });
  assert.doesNotThrow(() => savePresentation(current, { setItem: () => { throw Error("quota exceeded"); } }));
  assert.deepEqual(current, { views: { open: [c, a, b], active: b }, fontSize: 18, expanded: true });
});

test("zoom clamps and resets, and only the Cmd terminal shortcuts are intercepted", () => {
  assert.equal(zoomFont(14, "in"), 15);
  assert.equal(zoomFont(14, "out"), 13);
  assert.equal(zoomFont(10, "out"), 10);
  assert.equal(zoomFont(28, "in"), 28);
  assert.equal(zoomFont(28, "reset"), 14);
  const cmd = { metaKey: true, altKey: false, ctrlKey: false, isComposing: false };
  assert.equal(zoomShortcut({ ...cmd, key: "+" }), "in");
  assert.equal(zoomShortcut({ ...cmd, key: "=" }), "in");
  assert.equal(zoomShortcut({ ...cmd, key: "-" }), "out");
  assert.equal(zoomShortcut({ ...cmd, key: "−" }), "out");
  assert.equal(zoomShortcut({ ...cmd, key: "0" }), "reset");
  assert.equal(zoomShortcut({ ...cmd, key: "0", metaKey: false, ctrlKey: true }), undefined);
  assert.equal(zoomShortcut({ ...cmd, key: "+", ctrlKey: true }), undefined);
  assert.equal(zoomShortcut({ ...cmd, key: "-", altKey: true }), undefined);
  assert.equal(zoomShortcut({ ...cmd, key: "0", isComposing: true }), undefined);
  assert.equal(zoomShortcut({ ...cmd, key: "c" }), undefined);
});

test("the native fullscreen boundary sends only the desired state and requires boolean readback", async () => {
  const original = Object.getOwnPropertyDescriptor(globalThis, "window");
  const calls: unknown[] = [];
  let actual: unknown = false;
  Object.defineProperty(globalThis, "window", {
    configurable: true,
    value: {
      __TAURI_INTERNALS__: {
        invoke(command: string, args: { fullscreen?: boolean }) {
          calls.push([command, args]);
          if (args.fullscreen !== undefined) actual = args.fullscreen;
          return actual;
        },
      },
    },
  });
  try {
    assert.equal(await localClient.windowFullscreen(), false);
    assert.equal(await localClient.windowFullscreen(true), true);
    assert.equal(await localClient.windowFullscreen(false), false);
    actual = { fullscreen: true };
    await assert.rejects(localClient.windowFullscreen());
    assert.deepEqual(calls, [
      ["local_window_fullscreen", {}], ["local_window_fullscreen", { fullscreen: true }],
      ["local_window_fullscreen", { fullscreen: false }], ["local_window_fullscreen", {}],
    ]);
  } finally {
    if (original) Object.defineProperty(globalThis, "window", original);
    else Reflect.deleteProperty(globalThis, "window");
  }
});
