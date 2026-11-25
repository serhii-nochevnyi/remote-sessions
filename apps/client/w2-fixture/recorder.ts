export type Invoke = typeof import("@tauri-apps/api/core").invoke;

export const fixtureCaseId = "ukrainian-return";

const eventBytes = 1024;
const appendEvents = 16;
const appendBytes = 4096;
const retainedEvents = 256;
const retainedBytes = 16384;
const invokeDeadlineMs = 5000;

export type Snapshot = {
  caseId: string;
  generation: number;
  nextSequence: number;
  bytes: number[];
  eventLengths: number[];
  rejected: number;
  invalid: boolean;
};

type InputSource = {
  onData(callback: (text: string) => void): { dispose(): void };
};

type Attempt = {
  caseId: string;
  generation: number;
  nextSequence: number;
  lastSent: number;
  queue: number[][];
  pendingEvents: number;
  pendingBytes: number;
  invalid: boolean;
  drain: Promise<void> | undefined;
  snapshot?: Snapshot;
};

async function withDeadline<T>(signal: AbortSignal, operation: () => Promise<T>): Promise<T> {
  signal.throwIfAborted();
  let cancel!: () => void;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const deadline = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error("Local invoke timed out")), invokeDeadlineMs);
    cancel = () => reject(signal.reason);
  });
  signal.addEventListener("abort", cancel, { once: true });
  try {
    return await Promise.race([operation(), deadline]);
  } finally {
    clearTimeout(timer);
    signal.removeEventListener("abort", cancel);
  }
}

function isUnsigned(value: unknown, max = 0xffffffff): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= 0 && value <= max;
}

function isSnapshot(value: unknown, attempt: Attempt, minSequence: number): value is Snapshot {
  if (!value || typeof value !== "object") return false;
  const snapshot = value as Partial<Snapshot>;
  return snapshot.caseId === attempt.caseId && snapshot.generation === attempt.generation
    && isUnsigned(snapshot.nextSequence, attempt.lastSent + 1) && snapshot.nextSequence >= minSequence
    && isUnsigned(snapshot.rejected) && typeof snapshot.invalid === "boolean"
    && Array.isArray(snapshot.bytes) && snapshot.bytes.length <= retainedBytes
    && snapshot.bytes.every(byte => isUnsigned(byte, 255))
    && Array.isArray(snapshot.eventLengths) && snapshot.eventLengths.length <= retainedEvents
    && snapshot.eventLengths.every(length => isUnsigned(length, eventBytes) && length > 0)
    && snapshot.eventLengths.reduce((sum, length) => sum + length, 0) === snapshot.bytes.length;
}

export function registerRecorder(input: InputSource, invoke: Invoke, onError: (error: unknown) => void) {
  const encoder = new TextEncoder();
  let active: Attempt | undefined;
  let epoch = 0;
  const lifetime = new AbortController();
  let generation = 0;

  function invalidate(attempt: Attempt, reason: unknown) {
    if (active !== attempt || attempt.invalid) return;
    attempt.invalid = true;
    attempt.queue.length = 0;
    onError(reason);
  }

  async function send(attempt: Attempt) {
    while (active === attempt && !attempt.invalid && attempt.queue.length) {
      const events: number[][] = [];
      let bytes = 0;
      for (const event of attempt.queue) {
        if (events.length === appendEvents || bytes + event.length > appendBytes) break;
        events.push(event);
        bytes += event.length;
      }
      attempt.queue.splice(0, events.length);
      const sequence = attempt.nextSequence;
      attempt.lastSent = sequence;
      try {
        const ack = await withDeadline(lifetime.signal, () => invoke<unknown>("w2_append_input", {
          generation: attempt.generation, sequence, events,
        }));
        if (active !== attempt) return;
        if (ack !== null) throw new Error("Invalid append acknowledgment");
        attempt.nextSequence++;
      } catch (reason) {
        invalidate(attempt, reason);
      } finally {
        attempt.pendingEvents -= events.length;
        attempt.pendingBytes -= bytes;
      }
    }
  }

  function flush() {
    const attempt = active;
    if (!attempt || attempt.drain || attempt.invalid || !attempt.queue.length) return;
    attempt.drain = send(attempt).finally(() => {
      attempt.drain = undefined;
      if (active === attempt) flush();
    });
  }

  const subscription = input.onData(text => {
    const attempt = active;
    if (!attempt || attempt.invalid) return;
    if (!text.length || text.length > eventBytes) {
      invalidate(attempt, "Invalid input event size");
      return;
    }
    const encoded = encoder.encode(text);
    if (encoded.length > eventBytes) {
      invalidate(attempt, "Invalid input event size");
      return;
    }
    if (attempt.pendingEvents >= retainedEvents || attempt.pendingBytes + encoded.length > retainedBytes) {
      invalidate(attempt, "Pending input capacity exceeded");
      return;
    }
    attempt.queue.push(Array.from(encoded));
    attempt.pendingEvents++;
    attempt.pendingBytes += encoded.length;
    flush();
  });

  function retire() {
    epoch++;
    const attempt = active;
    active = undefined;
    if (attempt) attempt.queue.length = 0;
    return attempt;
  }

  async function readSnapshot(attempt: Attempt) {
    const minSequence = attempt.nextSequence;
    const snapshot = await withDeadline(lifetime.signal, () => invoke<unknown>("w2_read_attempt", { generation: attempt.generation }));
    if (!isSnapshot(snapshot, attempt, minSequence)) throw new Error("Invalid readback acknowledgment");
    return snapshot;
  }

  return {
    retire,
    async interrupt() {
      const attempt = retire();
      if (!attempt) return;
      try {
        const snapshot = await readSnapshot(attempt);
        return { snapshot, uncertain: snapshot.nextSequence <= attempt.lastSent };
      } catch {
        return {
          ...(attempt.snapshot === undefined ? {} : { snapshot: attempt.snapshot }),
          uncertain: true,
        };
      }
    },
    async start(caseId: string) {
      retire();
      const request = epoch;
      lifetime.signal.throwIfAborted();
      const next = await withDeadline(lifetime.signal, () => invoke<unknown>("w2_begin_attempt", { caseId })).catch(reason => {
        if (epoch === request) throw reason;
      });
      if (epoch !== request) return;
      if (!isUnsigned(next) || next <= generation) throw new Error("Invalid attempt generation");
      generation = next;
      active = {
        caseId, generation, nextSequence: 1, lastSent: 0, queue: [],
        pendingEvents: 0, pendingBytes: 0, invalid: false, drain: undefined,
      };
      return generation;
    },
    async read(isCurrent: () => boolean = () => true) {
      const attempt = active;
      if (!attempt) throw new Error("Start the fixture case first");
      while (attempt.drain && active === attempt) await attempt.drain;
      if (active !== attempt || !isCurrent()) throw new Error("Attempt read expired");
      const snapshot = await readSnapshot(attempt);
      if (active !== attempt || !isCurrent()) throw new Error("Attempt read expired");
      attempt.snapshot = snapshot;
      if (snapshot.invalid) invalidate(attempt, "Native recorder rejected input");
      return snapshot;
    },
    dispose() {
      lifetime.abort(new Error("Recorder is closed"));
      retire();
      subscription.dispose();
    },
  };
}
