import assert from "node:assert/strict";
import test from "node:test";
import { controlledClock, mountFixture } from "./fixture.test.ts";
import { fixtureCaseId, registerRecorder, type AppendOutcome, type Invoke, type Snapshot } from "./recorder.ts";

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
  const terminal = fixture.terminal;
  assert(terminal, "production entry creates an xterm");
  return {
    ...fixture,
    terminal,
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

test("recorder settles the current append once before a waiting Read", async t => {
  for (const mode of ["on time", "exact deadline", "elapsed first", "timeout first", "malformed", "rejected"] as const) {
    await t.test(mode, { timeout: 5000 }, async () => {
      let input!: (text: string) => void;
      const raw = deferred();
      const readReply = deferred();
      const reading = deferred();
      const appends: Append[] = [];
      const errors: string[] = [];
      const log: (AppendOutcome | "read")[] = [];
      const bytes = utf8("ї");
      const snapshot: Snapshot = {
        caseId: fixtureCaseId, generation: 1, nextSequence: 2,
        bytes, eventLengths: [bytes.length], rejected: 0, invalid: false,
      };
      const invoke: Invoke = async <T,>(command: string, args?: Parameters<Invoke>[1]) => {
        if (command === "w2_begin_attempt") return 1 as T;
        if (command === "w2_append_input") {
          appends.push(args as Append);
          return await raw.promise as T;
        }
        assert.equal(command, "w2_read_attempt");
        assert.deepEqual(args, { generation: 1 });
        log.push("read");
        reading.resolve(undefined);
        return await readReply.promise as T;
      };
      const clock = controlledClock();
      const recorder = registerRecorder({
        onData(receive) {
          input = receive;
          return { dispose() {} };
        },
      }, invoke, reason => errors.push(String(reason)), outcome => log.push(outcome));
      try {
        assert.equal(await recorder.start(fixtureCaseId), 1);
        input("ї");
        if (mode !== "on time") input("discard pending input");
        const read = recorder.read();
        assert.deepEqual(log, []);
        assert.deepEqual(appends, [{ generation: 1, sequence: 1, events: [bytes] }]);
        switch (mode) {
          case "on time": clock.elapse(4999); raw.resolve(null); break;
          case "exact deadline": clock.elapse(5000); raw.resolve(null); break;
          case "elapsed first": clock.elapse(5001); raw.resolve(null); break;
          case "timeout first": clock.advance(5000); break;
          case "malformed": raw.resolve({ generation: 1, sequence: 1 }); break;
          case "rejected": raw.reject(new Error("Rejected append")); break;
        }
        await reading.promise;
        const expected: AppendOutcome = {
          generation: 1, sequence: 1, bytes: bytes.length, status: mode === "on time" ? "acknowledged" : "unconfirmed",
        };
        assert.deepEqual(log, [expected, "read"]);
        assert.equal(clock.invokeTimeouts, Number(mode === "timeout first"));
        const expectedErrors = mode === "on time" ? [] : [mode === "malformed" ? "Error: Invalid append acknowledgment"
          : mode === "rejected" ? "Error: Rejected append" : "Error: Local invoke timed out"];
        assert.deepEqual(errors, expectedErrors);
        readReply.resolve(snapshot);
        assert.deepEqual(await read, snapshot);
        raw.resolve(null);
        clock.advance(5000);
        await Promise.resolve();
        assert.equal(clock.invokeTimeouts, Number(mode === "timeout first"));
        assert.deepEqual(log, [expected, "read"]);
        assert.deepEqual(errors, expectedErrors);
        assert.deepEqual(appends, [{ generation: 1, sequence: 1, events: [bytes] }]);
        assert.deepEqual(await recorder.interrupt(), { snapshot, uncertain: mode !== "on time" });
        assert.deepEqual(log, [expected, "read", "read"]);
      } finally {
        recorder.dispose();
        clock.dispose();
      }
    });
  }
});

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
    ["event capacity", Array.from({ length: 255 }, () => "a"), "b", "Pending input capacity exceeded"],
    ["byte capacity", [...Array.from({ length: 15 }, () => "ї".repeat(512)), "a".repeat(1022)], "b",
      "Pending input capacity exceeded"],
    ["oversized event", [], "ї".repeat(512) + "b", "Invalid input event size"],
    ["empty event", [], "", "Invalid input event size"],
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
        assert.equal(fixture.alert(), `Held: ${message}`);
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

test("retired append counts against fresh pending capacity", async t => {
  for (const [name, inFlight, queued] of [
    ["events", ["a", "b"], Array.from({ length: 254 }, () => "c")],
    ["UTF-8 bytes", ["ї".repeat(512), "ї"],
      [...Array.from({ length: 14 }, () => "ї".repeat(512)), "a".repeat(1022)]],
  ] as const) {
    for (const overflow of [false, true]) {
      await t.test(`${name} ${overflow ? "overflow" : "at the limit"}`, async () => {
        const fixture = await mountRecorder();
        try {
          await startAndTrace(fixture);
          await fixture.act(async () => {
            for (const text of inFlight) fixture.terminal.input(text);
          });
          await fixture.accept(0);
          const old = fixture.appends[1];
          assert(old);
          assert.deepEqual(old.args, { generation: 1, sequence: 2, events: inFlight.map(utf8) });
          await fixture.input("discard retired queue");
          await fixture.click("Restart");
          await fixture.act(async () => {
            for (const text of queued) fixture.terminal.input(text);
          });
          assert.equal(fixture.appends.length, 2, "fresh input waits behind the entire retired batch");
          assert.equal(fixture.alert(), "", "the combined capacity is allowed");
          if (overflow) {
            await fixture.input("+");
            assert.equal(fixture.alert(), "Held: Pending input capacity exceeded");
          }
          await fixture.click("Read");
          if (!overflow) {
            assert.equal(fixture.readout.textContent, "—");
            assert(!fixture.ipc.some(call => call.command === "w2_read_attempt"));
          }
          await fixture.act(async () => { old.reply.reject("retired append rejected"); });
          if (overflow) {
            assert.equal(fixture.appends.length, 2, "overflow discards all fresh queued input");
            assert.equal(fixture.readout.textContent, "(empty)");
            assert.equal(fixture.alert(), "Held: Pending input capacity exceeded");
            await fixture.click("Restart");
            await fixture.input("new");
            assert.deepEqual(fixture.appends[2]?.args, { generation: 3, sequence: 1, events: [utf8("new")] });
            await fixture.accept(2);
            await fixture.click("Read");
            assert.equal(fixture.readout.textContent, "6e6577");
            assert.equal(fixture.appends.length, 3);
          } else {
            const dispatched: number[][] = [];
            for (let index = 2; dispatched.length < queued.length; index++) {
              const append = fixture.appends[index];
              assert(append, "retired rejection releases the fresh queue");
              dispatched.push(...append.args.events);
              await fixture.accept(index);
            }
            assert.deepEqual(dispatched, queued.map(utf8));
            assert.equal(fixture.readout.textContent, Buffer.from(queued.join("")).toString("hex"));
            assert.equal(fixture.alert(), "", "retired rejection leaves the fresh attempt valid");
          }
        } finally {
          await fixture.dispose();
        }
      });
    }
  }
});

test("late retired rejection leaves the fresh append slot occupied", async t => {
  const fixture = await mountRecorder();
  try {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    await startAndTrace(fixture);
    const old = fixture.appends[0];
    assert(old);
    await fixture.click("Restart");
    await fixture.input("a");
    assert.equal(fixture.appends.length, 1);
    await fixture.act(async () => { t.mock.timers.tick(5000); });
    assert.deepEqual(fixture.appends[1]?.args, { generation: 2, sequence: 1, events: [utf8("a")] });
    await fixture.input("b");
    await fixture.click("Read");
    await fixture.act(async () => { old.reply.reject("retired append rejected after the deadline"); });
    assert.equal(fixture.appends.length, 2);
    assert.deepEqual(fixture.registrations[0]?.outcomes, [], "retired batch publishes no outcome");
    assert.equal(fixture.readout.textContent, "—");
    assert.equal(fixture.alert(), "");
    await fixture.accept(1);
    assert.deepEqual(fixture.appends[2]?.args, { generation: 2, sequence: 2, events: [utf8("b")] });
    await fixture.accept(2);
    assert.equal(fixture.readout.textContent, "6162");
    assert.equal(fixture.appends.length, 3);
    assert.deepEqual(fixture.registrations[0]?.outcomes, [
      { generation: 2, sequence: 1, bytes: 1, status: "acknowledged" },
      { generation: 2, sequence: 2, bytes: 1, status: "acknowledged" },
    ]);
    assert.equal(fixture.alert(), "");
  } finally {
    await fixture.dispose();
  }
});

test("invoke failure retires pending input without replay", async t => {
  for (const [mode, message] of [
    ["rejection", "native rejected append"],
    ["missing", "Error: Invalid append acknowledgment"],
    ["malformed", "Error: Invalid append acknowledgment"],
    ["sequence", "Error: Invalid append acknowledgment"],
    ["generation", "Error: Invalid append acknowledgment"],
    ["timeout", "Error: Local invoke timed out"],
  ] as const) {
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
        assert.equal(fixture.alert(), `Held: ${message}`);
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
