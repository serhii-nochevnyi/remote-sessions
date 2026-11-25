export type Invoke = typeof import("@tauri-apps/api/core").invoke;

export const fixtureCaseId = "ukrainian-return";

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

export function registerRecorder(input: InputSource, invoke: Invoke, onError: (error: unknown) => void) {
  const encoder = new TextEncoder();
  let pending = Promise.resolve();
  let active: { generation: number; nextSequence: number; failed: boolean } | undefined;
  let disposed = false;

  function serialize<T>(operation: () => Promise<T>): Promise<T> {
    const result = pending.then(() => {
      if (disposed) throw new Error("Recorder is closed");
      return operation();
    });
    pending = result.then(() => {}, () => {});
    return result;
  }

  const subscription = input.onData(text => {
    const attempt = active;
    if (!attempt || attempt.failed || disposed || !text.length) return;
    const events = [Array.from(encoder.encode(text))];
    void serialize(async () => {
      if (attempt.failed) return;
      try {
        await invoke("w2_append_input", {
          generation: attempt.generation,
          sequence: attempt.nextSequence,
          events,
        });
        attempt.nextSequence += 1;
      } catch (error) {
        attempt.failed = true;
        if (!disposed && active === attempt) onError(error);
      }
    }).catch(() => {});
  });

  return {
    start(caseId: string) {
      active = undefined;
      return serialize(async () => {
        const generation = await invoke<number>("w2_begin_attempt", { caseId });
        if (!disposed) active = { generation, nextSequence: 1, failed: false };
        return generation;
      });
    },
    read() {
      const attempt = active;
      if (!attempt) return Promise.reject(new Error("Start the fixture case first"));
      return serialize(() => invoke<Snapshot>("w2_read_attempt", { generation: attempt.generation }));
    },
    dispose() {
      disposed = true;
      active = undefined;
      subscription.dispose();
    },
  };
}
