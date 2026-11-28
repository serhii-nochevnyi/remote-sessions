import assert from "node:assert/strict";
import test from "node:test";
import { localClient, selectCreatedRuntime } from "./local-client.ts";
import type { LocalCreate } from "./local-client.ts";
import { loadPresentation, parsePresentation, PresentationStore, savePresentation, zoomFont, zoomShortcut } from "./presentation-preferences.ts";
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

test("create recovery preserves saved order and a null selection", () => {
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
  const otherTabs = { open: [c, a], active: null };
  assert.deepEqual(selectCreatedRuntime(otherTabs, operation, new Set(), true), restored.views);
  assert.deepEqual(selectCreatedRuntime(defaults.views, operation, new Set(), true), { open: [b], active: b });
  assert.deepEqual(selectCreatedRuntime(otherTabs, operation, new Set()), { open: [c, a, b], active: b });
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

test("the native fullscreen boundary correlates exit, focus and cancellation and requires boolean replies", async () => {
  const original = Object.getOwnPropertyDescriptor(globalThis, "window");
  const calls: unknown[] = [];
  let reply: unknown = false;
  Object.defineProperty(globalThis, "window", {
    configurable: true,
    value: {
      __TAURI_INTERNALS__: {
        invoke(command: string, args: unknown) {
          calls.push([command, args]);
          return reply;
        },
      },
    },
  });
  try {
    assert.equal(await localClient.windowFullscreen(), false);
    reply = true;
    assert.equal(await localClient.enterWindowFullscreen(), true);
    assert.equal(await localClient.exitWindowFullscreen(a), true);
    assert.equal(await localClient.focusWindowAfterFullscreenExit(a), true);
    reply = false;
    assert.equal(await localClient.cancelWindowFullscreenExit(a), false);
    assert.equal(await localClient.exitWindowFullscreen(b), false);
    reply = { fullscreen: true };
    await assert.rejects(localClient.windowFullscreen(), /Некоректна відповідь повноекранного режиму/);
    assert.deepEqual(calls, [
      ["local_window_fullscreen", { action: "read", intent: undefined }],
      ["local_window_fullscreen", { action: "enter", intent: undefined }],
      ["local_window_fullscreen", { action: "exit", intent: a }],
      ["local_window_fullscreen", { action: "focusAfterExit", intent: a }],
      ["local_window_fullscreen", { action: "cancelExit", intent: a }],
      ["local_window_fullscreen", { action: "exit", intent: b }],
      ["local_window_fullscreen", { action: "read", intent: undefined }],
    ]);
  } finally {
    if (original) Object.defineProperty(globalThis, "window", original);
    else Reflect.deleteProperty(globalThis, "window");
  }
});

const created = {
  reference: c, workspace_id: a, profile: "codex", presented: false,
  disposition: { result: "launched", current: "live", readiness: "unobserved",
    runtime: { runtime_id: b, session_id: b, command: "codex", pane_pid: 42, exited: false, width: 80, height: 24,
      origin: "managed", workspace_id: a, operation_id: a } },
} satisfies LocalCreate;
const presentationAck = { operations: [{ ...created, presented: true }], issue: null };

function presentationFixture() {
  let saved = JSON.stringify({ ...record, open: [c, a], active: a });
  let readable = true;
  let writable = true;
  let writes = 0;
  const storage = {
    getItem() {
      if (!readable) throw Error("unreadable");
      return saved;
    },
    setItem(_: string, value: string) {
      writes++;
      if (!writable) throw Error("write");
      saved = value;
    },
  };
  return { storage, writeCount: () => writes, saved: () => JSON.parse(saved),
    rejectWrites: (fail: boolean) => { writable = !fail; },
    rejectReads: (fail: boolean) => { readable = !fail; },
  };
}

test("first submit and first recovery write before exposure and suppress both restart paths after a failed close", async t => {
  const original = Object.getOwnPropertyDescriptor(globalThis, "window");
  const calls: unknown[] = [];
  let hostPresented = false;
  let hostAcknowledgeFails = true;
  let checkFallback = () => {};
  Object.defineProperty(globalThis, "window", {
    configurable: true,
    value: { __TAURI_INTERNALS__: {
      invoke(command: string, args: { reference?: string; presented?: boolean }) {
        calls.push([command, args]);
        assert.equal(command, "local_create_status");
        if (args.presented) {
          checkFallback();
          if (hostAcknowledgeFails) throw Error("reply lost");
          hostPresented = true;
        }
        return { operations: [{ ...created, presented: hostPresented }], issue: null };
      },
    } },
  });
  t.after(() => {
    if (original) Object.defineProperty(globalThis, "window", original);
    else Reflect.deleteProperty(globalThis, "window");
  });
  for (const source of ["submit", "recovery"] as const) {
    hostPresented = false;
    hostAcknowledgeFails = true;
    const fixture = presentationFixture();
    const store = new PresentationStore(fixture.storage);
    const [recovered] = (await localClient.createStatus()).operations;
    assert.ok(recovered);
    const operation = source === "submit" ? created : recovered;
    const originalViews = { open: [c, a], active: a };
    fixture.rejectWrites(true);
    assert.equal(store.expose(operation, source).result, "storage");
    assert.equal(fixture.writeCount(), 1);
    assert.deepEqual({ ...store.current.views }, originalViews);
    assert.deepEqual(fixture.saved().open, originalViews.open);

    fixture.rejectWrites(false);
    assert.equal(store.expose(operation, source).result, "opened");
    assert.equal(fixture.writeCount(), 2);
    assert.deepEqual({ ...store.current.views }, { open: [c, a, b], active: source === "submit" ? b : a });
    const [exposure] = store.unsettled();
    assert.ok(exposure);
    checkFallback = () => {
      assert.equal(fixture.saved().version, 2);
      assert.deepEqual(fixture.saved().exposures, [exposure]);
      assert.equal(exposure.state, "unresolved");
    };
    await assert.rejects(localClient.acknowledgeCreate(exposure.reference), /reply lost/);

    fixture.rejectWrites(true);
    assert.equal(store.close(b), "closed-unsaved");
    assert.deepEqual({ ...store.current.views }, originalViews);
    assert.deepEqual(fixture.saved().open, [c, a, b]);
    hostAcknowledgeFails = false;
    const late = await localClient.acknowledgeCreate(exposure.reference);
    assert.equal(store.settle(exposure, late), "ignored");

    const relaunched = new PresentationStore(fixture.storage);
    assert.deepEqual({ ...relaunched.current.views }, { ...originalViews, active: source === "submit" ? null : a });
    assert.equal(relaunched.expose(operation, "recovery").result, "ignored");
    const [onRestart] = (await localClient.createStatus()).operations;
    assert.ok(onRestart);
    assert.equal(relaunched.expose(onRestart, "recovery").result, "ignored");
    fixture.rejectWrites(false);
    const [forReopen] = (await localClient.createStatus(exposure.reference)).operations;
    assert.ok(forReopen);
    assert.deepEqual(relaunched.expose(forReopen, "user"), { result: "opened", runtime: created.disposition.runtime });
    assert.deepEqual(relaunched.current.views, { open: [c, a, b], active: b });
    assert.equal(relaunched.current.fontSize, 18);
    assert.equal(relaunched.current.expanded, true);
  }
  assert.deepEqual(calls.map(call => (call as [string, object])[0]), new Array(calls.length).fill("local_create_status"));
});

test("acknowledgement needs the exact presented pair and keeps the fallback when settlement cannot be saved", () => {
  const fixture = presentationFixture();
  const store = new PresentationStore(fixture.storage);
  assert.equal(store.expose(created, "submit").result, "opened");
  const [exposure] = store.unsettled();
  assert.ok(exposure);
  const { disposition } = created;
  const wrongRuntime = { ...disposition, runtime: { ...disposition.runtime, runtime_id: c } };
  for (const operation of [
    created, { ...created, reference: a, presented: true }, { ...created, disposition: wrongRuntime, presented: true },
    { ...created, disposition: { ...disposition, runtime: { ...disposition.runtime, operation_id: c } }, presented: true },
  ]) {
    assert.equal(store.settle(exposure, { operations: [operation], issue: null }), "ignored");
  }
  assert.equal(store.settle(exposure, { ...presentationAck, issue: "pending_unavailable" }), "ignored");
  fixture.rejectWrites(true);
  assert.equal(store.settle(exposure, presentationAck), "storage");
  assert.deepEqual(new PresentationStore(fixture.storage).current.views.open, [c, a]);
  fixture.rejectWrites(false);
  assert.equal(store.settle(exposure, presentationAck), "settled");
  assert.deepEqual(new PresentationStore(fixture.storage).current.views.open, [c, a, b]);
  assert.deepEqual(store.unsettled(), []);

  fixture.rejectWrites(true);
  assert.equal(store.close(b), "pending");
  assert.deepEqual(store.current.views.open, [c, a, b]);
  fixture.rejectWrites(false);
  assert.equal(store.close(b), "closed");
  store.update(value => ({ ...value, views: moveView(value.views, a, -1), fontSize: 21, expanded: false }));
  const relaunched = new PresentationStore(fixture.storage);
  assert.deepEqual(relaunched.current.views, { open: [a, c], active: a });
  assert.equal(relaunched.current.fontSize, 21);
  assert.equal(relaunched.current.expanded, false);
  assert.equal(relaunched.exposure(b)?.state, "closed");
  assert.equal(relaunched.expose(created, "recovery").result, "ignored");
  assert.equal(store.settle(exposure, presentationAck), "ignored");
});

test("unreadable preferences withhold exposure until recovery and late acknowledgement cannot settle a reopened tab", () => {
  const fixture = presentationFixture();
  fixture.rejectReads(true);
  const store = new PresentationStore(fixture.storage);
  assert.equal(store.expose(created, "recovery").result, "storage");
  assert.equal(fixture.writeCount(), 0);
  assert.deepEqual({ ...store.current }, defaults);
  fixture.rejectReads(false);
  assert.equal(store.expose(created, "recovery").result, "opened");
  assert.equal(store.current.views.active, a);
  assert.equal(store.current.fontSize, 18);
  const [initial] = store.unsettled();
  assert.ok(initial);
  assert.equal(store.close(b), "closed");
  assert.equal(store.expose(created, "recovery").result, "ignored");
  assert.equal(store.expose(created, "user").result, "opened");
  const [reopened] = store.unsettled();
  assert.ok(reopened);
  assert.equal(store.settle(initial, presentationAck), "ignored");
  assert.equal(store.settle(reopened, presentationAck), "settled");
});

test("first exposure retains a newer selection, including an explicit return to the session list", () => {
  for (const active of [c, null]) {
    const fixture = presentationFixture();
    const store = new PresentationStore(fixture.storage);
    store.update(value => ({ ...value, views: { ...value.views, active } }));
    assert.equal(store.expose(created, "submit", true).result, "opened");
    assert.deepEqual(store.current.views, { open: [c, a, b], active });
    assert.deepEqual(fixture.saved().open, [c, a, b]);
    assert.equal(fixture.saved().active, active);
  }
  const fixture = presentationFixture();
  const store = new PresentationStore(fixture.storage);
  store.close(c);
  store.close(a);
  assert.equal(store.expose(created, "recovery", true).result, "opened");
  assert.deepEqual(store.current.views, { open: [b], active: null });
});

test("inventory reopen and a late submit share the same presentation record", () => {
  const fixture = presentationFixture();
  const store = new PresentationStore(fixture.storage);
  assert.equal(store.expose(created, "user").result, "opened");
  assert.equal(store.expose(created, "submit").result, "ignored");
  assert.deepEqual(store.current.views.open, [c, a, b]);
  assert.equal(fixture.writeCount(), 1);
  assert.equal(store.close(b), "closed");
  const { runtime } = created.disposition;
  const replacement: LocalCreate = { ...created,
    disposition: { result: "launched", current: "live", readiness: "unobserved", runtime: { ...runtime, operation_id: c } } };
  assert.equal(store.expose(replacement, "user").result, "conflict");
  assert.equal(store.expose(created, "submit").result, "ignored");
  assert.deepEqual(store.current.views.open, [c, a]);
});

test("version two validates exposure identity and normalizes suppression without dropping unrelated tabs", () => {
  const exposure = { reference: c, runtimeId: b, operationId: a, sessionId: b, state: "closed" };
  const saved = { ...record, version: 2, exposures: [exposure] };
  assert.deepEqual(loadPresentation({ getItem: () => JSON.stringify(saved) }), {
    views: { open: [c, a], active: null }, fontSize: 18, expanded: true, exposures: [exposure],
  });
  for (const record of [
    { ...saved, exposures: null },
    { ...saved, exposures: [{ ...exposure, runtimeId: b.toUpperCase() }, { ...exposure, reference: a }] },
    { ...saved, exposures: [{ ...exposure, state: "pending" }] },
    { ...saved, exposures: [{ ...exposure, writer: "lease" }] },
    { ...saved, exposures: [{ ...exposure, operationId: "wrong" }] },
  ]) assert.equal(parsePresentation(record), undefined);
});
