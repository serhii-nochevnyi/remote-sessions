import type {
  Chunk, GeometryEvent, ResizeEpoch, ResizeResult, TerminalScope, TerminalSize, TerminalStatus,
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

class OutputBoundary {
  private state: "ground" | "escape" | "intermediate" | "csi" | "osc" | "string" | "string_escape" = "ground";
  private bytes = 0;
  private codePoint = 0;
  private minimum = 0;

  get complete() {
    return this.state === "ground" && this.bytes === 0;
  }

  accept(data: readonly number[]) {
    for (const byte of data) {
      if (this.bytes) {
        if ((byte & 0xc0) === 0x80) {
          this.codePoint = (this.codePoint << 6) | (byte & 0x3f);
          if (--this.bytes === 0) {
            const code = this.codePoint;
            this.control(code >= this.minimum && code <= 0x10ffff ? code : 0xfffd);
          }
          continue;
        }
        this.bytes = 0;
        this.control(0xfffd);
      }
      if (byte >= 0xc2 && byte <= 0xf4) {
        this.bytes = byte < 0xe0 ? 1 : byte < 0xf0 ? 2 : 3;
        this.minimum = this.bytes === 1 ? 0x80 : this.bytes === 2 ? 0x800 : 0x10000;
        this.codePoint = byte & (0x7f >> (this.bytes + 1));
      } else {
        this.control(byte < 0x80 ? byte : 0xfffd);
      }
    }
  }

  private control(code: number) {
    if (code === 0x18 || code === 0x1a || code === 0x9c) { this.state = "ground"; return; }
    if (this.state === "osc" || this.state === "string") {
      if (this.state === "osc" && code === 7) this.state = "ground";
      else if (code === 0x1b) this.state = "string_escape";
      return;
    }
    if (this.state === "string_escape") {
      if (code === 0x5c) { this.state = "ground"; return; }
      this.state = "escape";
    }
    if (code === 0x1b) { this.state = "escape"; return; }
    if (code === 0x9b || (this.state === "escape" && code === 0x5b)) { this.state = "csi"; return; }
    if (code === 0x9d || (this.state === "escape" && code === 0x5d)) { this.state = "osc"; return; }
    if ([0x90, 0x98, 0x9e, 0x9f].includes(code) ||
      (this.state === "escape" && [0x50, 0x58, 0x5e, 0x5f].includes(code))) { this.state = "string"; return; }
    if (this.state === "escape" && code >= 0x20 && code <= 0x2f) this.state = "intermediate";
    else if ((this.state === "escape" || this.state === "intermediate") && code >= 0x30 && code <= 0x7e) this.state = "ground";
    else if (this.state === "csi" && code >= 0x40 && code <= 0x7e) this.state = "ground";
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
      if (event.status.committed.revision <= this.applied || !this.observe(event.status) || !this.output.complete) {
        throw Error("Не вдалося узгодити межу зміни розміру");
      }
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
