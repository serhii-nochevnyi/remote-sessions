import type {
  Chunk, GeometryBoundaryFailure, GeometryEvent, ResizeEpoch, ResizeResult, TerminalScope, TerminalSize, TerminalStatus,
} from "./local-client.ts";

export function sameSize(left: TerminalSize, right: TerminalSize): boolean {
  return left.cols === right.cols && left.rows === right.rows;
}

function epochKey(epoch: ResizeEpoch | null): string {
  return epoch ? `${epoch.agent}/${epoch.owner}` : "";
}

export function sameTerminal(left: TerminalScope, right: TerminalScope): boolean {
  const a = left.target;
  const b = right.target;
  return left.view_id === right.view_id && left.agent_epoch === right.agent_epoch &&
    a.runtime_id === b.runtime_id && a.generation === b.generation && a.session === b.session &&
    a.window === b.window && a.pane === b.pane && a.pid === b.pid && a.process_start === b.process_start;
}

interface Proposal {
  readonly terminal: TerminalScope;
  readonly epoch: ResizeEpoch;
  readonly size: TerminalSize;
  readonly revision: number;
}

export class ResizeProposals {
  private current: TerminalStatus | undefined;
  private applied = 0;
  private owner = "";
  private queued: TerminalSize | undefined;
  private sending: Proposal | undefined;
  private waiting = 0;

  observe(status: TerminalStatus, writable: boolean, applied: number): boolean {
    const owner = writable && status.epoch?.agent === status.terminal.agent_epoch
      ? epochKey(status.epoch) : "";
    const changed = owner !== this.owner || !this.current || !sameTerminal(this.current.terminal, status.terminal);
    if (changed) {
      this.queued = undefined;
      this.waiting = 0;
    }
    this.current = status;
    this.applied = applied;
    this.owner = owner;
    return changed;
  }

  offer(size: TerminalSize) {
    if (!this.owner || !this.current) return;
    const sent = this.sending;
    if (sent && sameTerminal(sent.terminal, this.current.terminal) && epochKey(sent.epoch) === this.owner) {
      this.queued = sameSize(size, sent.size) ? undefined : size;
    } else {
      this.queued = size;
    }
  }

  take(): Proposal | undefined {
    const current = this.current;
    const size = this.queued;
    if (this.sending || !this.owner || !current?.epoch || !size ||
      this.applied < Math.max(this.waiting, current.committed.revision)) return;
    this.queued = undefined;
    if (sameSize(size, current.committed.size)) return;
    this.sending = { terminal: current.terminal, epoch: current.epoch, size, revision: current.committed.revision };
    return this.sending;
  }

  complete(proposal: Proposal, result?: ResizeResult): boolean {
    if (this.sending !== proposal) return false;
    this.sending = undefined;
    const current = this.current;
    const owner = epochKey(proposal.epoch);
    if (!current || owner !== this.owner || !sameTerminal(current.terminal, proposal.terminal)) return false;
    if (result?.result === "committed") {
      const { status } = result;
      if (!sameTerminal(status.terminal, proposal.terminal) || epochKey(status.epoch) !== owner ||
        status.committed.revision <= proposal.revision || status.committed.revision < current.committed.revision ||
        !sameSize(status.committed.size, proposal.size)) return false;
      this.waiting = status.committed.revision;
      return true;
    }
    if (result && (!sameTerminal(result.terminal, proposal.terminal) || epochKey(result.intent.epoch) !== owner)) return false;
    if (result) {
      this.queued = undefined;
      this.owner = "";
    }
    return true;
  }

  close() {
    this.owner = "";
    this.queued = undefined;
    this.sending = undefined;
  }
}

type VtState = "ground" | "escape" | "escapeIntermediate" | "osc" | "sosPmApc"
  | "csiEntry" | "csiParam" | "csiIntermediate" | "csiIgnore"
  | "dcsEntry" | "dcsParam" | "dcsIntermediate" | "dcsIgnore" | "dcsPassthrough";

// @contract: Follow the xterm 6.0.0 VT500 transition table and string end handlers.
function nextVtState(state: VtState, code: number): VtState {
  switch (code) {
    case 0x1b: return "escape";
    case 0x90: return "dcsEntry";
    case 0x98: case 0x9e: case 0x9f: return "sosPmApc";
    case 0x9b: return "csiEntry";
    case 0x9d: return "osc";
  }
  if (code === 0x18 || code === 0x1a || (code >= 0x80 && code <= 0x9f)) return "ground";
  if (code === 0x07 && state === "osc") return "ground";
  if (code < 0x20 || code === 0x7f) return state;

  const intermediate = code <= 0x2f;
  const parameter = code >= 0x30 && code <= 0x3b;
  const privateMarker = code >= 0x3c && code <= 0x3f;
  const final = code >= 0x40 && code <= 0x7e;
  switch (state) {
    case "escape":
      switch (code) {
        case 0x50: return "dcsEntry";
        case 0x58: case 0x5e: case 0x5f: return "sosPmApc";
        case 0x5b: return "csiEntry";
        case 0x5d: return "osc";
      }
      return intermediate ? "escapeIntermediate" : "ground";
    case "escapeIntermediate":
      return intermediate ? state : "ground";
    case "csiEntry":
      if (intermediate) return "csiIntermediate";
      return parameter || privateMarker ? "csiParam" : "ground";
    case "csiParam":
      if (intermediate) return "csiIntermediate";
      if (privateMarker) return "csiIgnore";
      return parameter ? state : "ground";
    case "csiIntermediate":
      if (parameter || privateMarker) return "csiIgnore";
      return intermediate ? state : "ground";
    case "csiIgnore":
      return final ? "ground" : state;
    case "dcsEntry":
      if (intermediate) return "dcsIntermediate";
      if (parameter || privateMarker) return "dcsParam";
      return final ? "dcsPassthrough" : "ground";
    case "dcsParam":
      if (intermediate) return "dcsIntermediate";
      if (privateMarker) return "dcsIgnore";
      if (parameter) return state;
      return final ? "dcsPassthrough" : "ground";
    case "dcsIntermediate":
      if (intermediate) return state;
      if (parameter || privateMarker) return "dcsIgnore";
      return final ? "dcsPassthrough" : "ground";
    case "osc": case "dcsIgnore": case "dcsPassthrough":
      return state;
    case "sosPmApc":
      return code <= 0x7e ? state : "ground";
    case "ground":
      return state;
  }
}

class OutputBoundary {
  private state: VtState = "ground";
  private readonly interim = new Uint8Array(3);

  get complete() {
    return this.state === "ground" && this.interim[0] === 0;
  }

  accept(data: readonly number[]) {
    const start = this.interim[0] ? this.resumeScalar(data) : 0;
    if (start === undefined) return;
    let offset = start;
    decode: while (offset < data.length) {
      const first = offset;
      const lead = data[offset++]!;
      if (lead < 0x80) {
        this.state = nextVtState(this.state, lead);
        continue;
      }
      if (lead < 0xc0 || lead > 0xf7) continue;
      const length = lead < 0xe0 ? 2 : lead < 0xf0 ? 3 : 4;
      let code = lead & (0x7f >> length);
      for (let index = 1; index < length; index++) {
        const byte = data[offset];
        if (byte === undefined) {
          for (let tail = first; tail < offset; tail++) this.interim[tail - first] = data[tail]!;
          return;
        }
        if ((byte & 0xc0) !== 0x80) continue decode;
        code = (code << 6) | (byte & 0x3f);
        offset++;
      }
      this.scalar(code, length);
    }
  }

  private resumeScalar(data: readonly number[]): number | undefined {
    const lead = this.interim[0]!;
    const length = lead < 0xe0 ? 2 : lead < 0xf0 ? 3 : 4;
    let code = lead & (0x7f >> length);
    let carried = 1;
    // @contract: xterm 6.0.0 stops counting saved continuations when the low six bits are zero.
    for (; carried < this.interim.length; carried++) {
      const payload = this.interim[carried]! & 0x3f;
      if (!payload) break;
      code = (code << 6) | payload;
    }
    let offset = 0;
    const missing = length - carried;
    for (; offset < missing; offset++) {
      const byte = data[offset];
      if (byte === undefined) return;
      if ((byte & 0xc0) !== 0x80) break;
      this.interim[carried + offset] = byte;
      code = (code << 6) | (byte & 0x3f);
    }
    if (offset === missing) this.scalar(code, length);
    this.interim.fill(0);
    return offset;
  }

  private scalar(code: number, length: number) {
    const minimum = length === 2 ? 0x80 : length === 3 ? 0x800 : 0x10000;
    if (code >= minimum && code <= 0x10ffff && code !== 0xfeff &&
      (code < 0xd800 || code > 0xdfff)) this.state = nextVtState(this.state, code);
  }
}

export class GeometryBoundaryError extends Error {
  readonly reason: GeometryBoundaryFailure;

  constructor(reason: GeometryBoundaryFailure) {
    super("Не вдалося узгодити межу зміни розміру");
    this.reason = reason;
  }
}
interface GeometrySink {
  write(data: readonly number[]): Promise<void>;
  resize(size: TerminalSize): TerminalSize;
  acknowledge(event: GeometryEvent, size: TerminalSize): Promise<GeometryEvent>;
}

export class OrderedGeometry {
  private readonly output = new OutputBoundary();
  private readonly retiredOwners = new Set<string>();
  private closed = false;
  private busy = false;
  status: TerminalStatus;
  applied: number;

  constructor(initial: TerminalStatus) {
    this.status = initial;
    this.applied = initial.committed.revision;
  }

  private observe(status: TerminalStatus): boolean {
    const { committed: next } = status;
    const { committed: current } = this.status;
    const previousOwner = epochKey(this.status.epoch);
    const owner = epochKey(status.epoch);
    if (!sameTerminal(this.status.terminal, status.terminal) || next.revision < current.revision ||
      (next.revision === current.revision &&
        (!sameSize(next.size, current.size) || next.intent?.id !== current.intent?.id ||
          epochKey(next.intent?.epoch ?? null) !== epochKey(current.intent?.epoch ?? null))) ||
      (owner !== previousOwner && this.retiredOwners.has(owner))) return false;
    if (owner !== previousOwner) this.retiredOwners.add(previousOwner);
    this.status = status;
    return true;
  }

  async consume(chunk: Chunk, sink: GeometrySink) {
    if (this.closed) return;
    if (this.busy) throw Error("Попередні дані ще обробляються");
    this.busy = true;
    try {
      if (chunk.closed) throw Error("Потік перервано");
      if (chunk.status && !this.observe(chunk.status)) throw Error("Стан термінала змінився");
      if (chunk.data.length) {
        await sink.write(chunk.data);
        if (this.closed) return;
        this.output.accept(chunk.data);
      }
      const event = chunk.geometry;
      if (!event) return;
      if (event.status.committed.revision <= this.applied) throw new GeometryBoundaryError("geometry_revision_not_new");
      if (!this.observe(event.status)) throw new GeometryBoundaryError("geometry_status_rejected");
      if (!this.output.complete) throw new GeometryBoundaryError("geometry_output_incomplete");
      const actual = sink.resize(event.status.committed.size);
      if (this.closed || !sameSize(actual, event.status.committed.size)) throw Error("Розміри відображення не збігаються");
      const acknowledged = await sink.acknowledge(event, actual);
      if (this.closed) return;
      if (acknowledged.status.committed.revision !== event.status.committed.revision ||
        epochKey(acknowledged.status.epoch) !== epochKey(event.status.epoch) ||
        !this.observe(acknowledged.status)) throw Error("Підтвердження розміру застаріло");
      this.applied = acknowledged.status.committed.revision;
    } catch (error) {
      this.close();
      throw error;
    } finally {
      this.busy = false;
    }
  }

  close() {
    this.closed = true;
  }
}
