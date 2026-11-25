import assert from "node:assert/strict";
import test from "node:test";
import { mountFixture } from "./fixture.test.ts";
import { fixtureCaseId, type Snapshot } from "./recorder.ts";

function deferred() {
  let resolve!: (value: unknown) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<unknown>((accept, decline) => {
    resolve = accept;
    reject = decline;
  });
  return { promise, resolve, reject };
}

type Append = { generation: number; sequence: number; events: number[][] };

async function mountRecorder() {
  let generation = 0;
  let snapshot: Snapshot;
  const appends: { args: Append; reply: ReturnType<typeof deferred> }[] = [];
  let readReply: (() => unknown) | undefined;
  const fixture = await mountFixture({
    invoke(command, args) {
      switch (command) {
        case "w2_begin_attempt":
          assert.deepEqual(args, { caseId: fixtureCaseId });
          snapshot = {
            caseId: fixtureCaseId, generation: ++generation, nextSequence: 1,
            bytes: [], eventLengths: [], rejected: 0, invalid: false,
          };
          return generation;
        case "w2_append_input": {
          const reply = deferred();
          appends.push({ args: args as Append, reply });
          return reply.promise;
        }
        case "w2_read_attempt":
          assert.deepEqual(args, { generation });
          return readReply ? readReply() : structuredClone(snapshot);
        default:
          assert.fail(`unexpected command ${command}`);
      }
    },
  });
  const readout = fixture.document.getElementById("snapshot");
  assert(readout);
  return {
    ...fixture,
    appends,
    readout,
    alert: () => fixture.container.find(node => node.getAttribute("role") === "alert")?.textContent ?? "",
    setReadReply: (reply?: () => unknown) => { readReply = reply; },
    async accept(index: number) {
      const append = appends[index];
      assert(append);
      assert.equal(append.args.generation, generation);
      assert.equal(append.args.sequence, snapshot.nextSequence++);
      snapshot.bytes.push(...append.args.events.flat());
      snapshot.eventLengths.push(...append.args.events.map(event => event.length));
      await fixture.act(async () => { append.reply.resolve(null); });
    },
  };
}

type RecorderFixture = Awaited<ReturnType<typeof mountRecorder>>;
const utf8 = (text: string) => Array.from(new TextEncoder().encode(text));

async function startAndTrace(fixture: RecorderFixture) {
  await fixture.click("Start");
  await fixture.input("ї");
  assert.equal(fixture.terminal.subscriptions, 1);
  assert.deepEqual(fixture.terminal.data, ["ї"]);
  assert.equal(fixture.registrations.length, 1);
  assert.deepEqual(fixture.registrations[0]?.calls, fixture.ipc);
  assert.deepEqual(fixture.appends.map(append => append.args), [
    { generation: 1, sequence: 1, events: [[0xd1, 0x97]] },
  ]);
}

test("bounded recorder serializes input and rejects overflow", async t => {
  await t.test("one append in flight and ordered batches", async () => {
    const fixture = await mountRecorder();
    try {
      await startAndTrace(fixture);
      const small = Array.from({ length: 19 }, (_, index) => String.fromCharCode(65 + index));
      const large = Array.from({ length: 6 }, (_, index) => String(index).repeat(1024));
      await fixture.act(async () => {
        for (const text of [...small, ...large]) fixture.terminal.input(text);
      });
      assert.equal(fixture.appends.length, 1);
      const expected = [["ї"], small.slice(0, 16), [...small.slice(16), ...large.slice(0, 3)], large.slice(3)];
      for (const [index, batch] of expected.entries()) {
        assert.equal(fixture.appends.length, index + 1);
        assert.deepEqual(fixture.appends[index]?.args, {
          generation: 1, sequence: index + 1, events: batch.map(utf8),
        });
        await fixture.accept(index);
      }
      assert.equal(fixture.appends.length, expected.length);
      assert.deepEqual(fixture.registrations[0]?.calls, fixture.ipc);
    } finally {
      await fixture.dispose();
    }
  });

  for (const [name, queued, extra, message] of [
    ["event capacity", Array.from({ length: 255 }, () => "a"), "b", /capacity/i],
    ["byte capacity", [...Array.from({ length: 15 }, () => "ї".repeat(512)), "a".repeat(1022)], "b", /capacity/i],
    ["oversized event", [], "ї".repeat(512) + "b", /event/i],
    ["empty event", [], "", /event/i],
  ] as const) {
    await t.test(name, async () => {
      const fixture = await mountRecorder();
      try {
        await startAndTrace(fixture);
        await fixture.act(async () => {
          for (const text of queued) fixture.terminal.input(text);
        });
        assert.equal(fixture.appends.length, 1);
        assert.equal(fixture.alert(), "", "the exact retained capacity is valid");
        await fixture.input(extra);
        assert.match(fixture.alert(), /invalid attempt/i);
        assert.match(fixture.alert(), message);
        await fixture.accept(0);
        assert.equal(fixture.appends.length, 1, "overflow discards unsent work");
        await fixture.click("Read");
        assert.equal(fixture.readout.textContent, "d197");
      } finally {
        await fixture.dispose();
      }
    });
  }
});

test("invoke failure retires pending input without replay", async t => {
  for (const mode of ["rejection", "missing", "malformed", "sequence", "generation", "timeout"] as const) {
    await t.test(mode, async subtest => {
      const fixture = await mountRecorder();
      try {
        subtest.mock.timers.enable({ apis: ["setTimeout"] });
        await startAndTrace(fixture);
        await fixture.accept(0);
        await fixture.input("?");
        await fixture.input("queued");
        const old = fixture.appends[1];
        assert(old);
        assert.equal(fixture.appends.length, 2);
        await fixture.act(async () => {
          switch (mode) {
            case "rejection": old.reply.reject("native rejected append"); break;
            case "missing": old.reply.resolve(undefined); break;
            case "malformed": old.reply.resolve(false); break;
            case "sequence": old.reply.resolve({ generation: 1, nextSequence: 99 }); break;
            case "generation": old.reply.resolve({ generation: 2, nextSequence: 3 }); break;
            case "timeout":
              subtest.mock.timers.tick(4999);
              assert.equal(fixture.alert(), "");
              subtest.mock.timers.tick(1);
              break;
          }
        });
        assert.match(fixture.alert(), /invalid attempt/i);
        assert.equal(fixture.appends.length, 2);
        const lateRead = deferred();
        if (mode === "missing") {
          fixture.setReadReply(() => lateRead.promise);
        } else if (mode === "generation") {
          fixture.setReadReply(() => ({
            caseId: fixtureCaseId, generation: 2, nextSequence: 2,
            bytes: [255], eventLengths: [1], rejected: 0, invalid: false,
          }));
        }
        await fixture.click("Read");
        if (mode === "missing") {
          await fixture.act(async () => { subtest.mock.timers.tick(5000); });
        }
        assert.equal(fixture.readout.textContent,
          mode === "missing" || mode === "generation" ? "Unavailable" : "d197");
        fixture.setReadReply();

        const restart = fixture.ipc.length;
        await fixture.click("Restart");
        assert.deepEqual(fixture.ipc.slice(restart), [
          { command: "w2_begin_attempt", args: { caseId: fixtureCaseId } },
        ]);
        assert.equal(fixture.alert(), "");
        await fixture.click("Read");
        assert.equal(fixture.readout.textContent, "(empty)");
        await fixture.input("\r");
        await fixture.accept(2);
        await fixture.click("Read");
        assert.equal(fixture.readout.textContent, "0d");
        const freshTrace = [
          { command: "w2_begin_attempt", args: { caseId: fixtureCaseId } },
          { command: "w2_read_attempt", args: { generation: 2 } },
          { command: "w2_append_input", args: { generation: 2, sequence: 1, events: [[13]] } },
          { command: "w2_read_attempt", args: { generation: 2 } },
        ];
        await fixture.act(async () => {
          old.reply.resolve(null);
          lateRead.resolve({
            caseId: fixtureCaseId, generation: 1, nextSequence: 2,
            bytes: [0xd1, 0x97], eventLengths: [2], rejected: 0, invalid: false,
          });
          subtest.mock.timers.tick(5000);
        });
        assert.deepEqual(fixture.ipc.slice(restart), freshTrace);
        assert.deepEqual(fixture.registrations[0]?.calls, fixture.ipc);
        assert.equal(fixture.readout.textContent, "0d");
        assert.equal(fixture.alert(), "");
      } finally {
        await fixture.dispose();
      }
    });
  }
});
