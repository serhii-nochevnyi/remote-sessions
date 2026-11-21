import { invoke } from "@tauri-apps/api/core";

export interface RuntimeInfo {
  readonly runtime_id: string;
  readonly session_id: string;
  readonly command: string;
  readonly pane_pid: number | null;
  readonly exited: boolean;
  readonly width: number;
  readonly height: number;
}
export interface Inventory { readonly agent_pid: number; readonly runtimes: readonly RuntimeInfo[] }
export interface Attachment { readonly view_id: string; readonly cols: number; readonly rows: number }
export interface Chunk { readonly writable: boolean; readonly data: readonly number[]; readonly closed: boolean }
const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
function record(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw Error("Некоректна відповідь агента");
  return value as Record<string, unknown>;
}
function id(value: unknown): string {
  if (typeof value !== "string" || !uuid.test(value)) throw Error("Некоректний ідентифікатор");
  return value;
}
function integer(value: unknown, min: number, max: number): number {
  if (typeof value !== "number" || !Number.isInteger(value) || value < min || value > max) throw Error("Некоректне числове значення");
  return value;
}
export function parseInventory(value: unknown): Inventory {
  const item = record(value);
  if (!Array.isArray(item.runtimes) || item.runtimes.length > 16) throw Error("Некоректний список сесій");
  const runtimes = item.runtimes.map((value: unknown): RuntimeInfo => {
    const runtime = record(value);
    if (typeof runtime.command !== "string" || runtime.command.length > 256 || typeof runtime.exited !== "boolean") throw Error("Некоректний опис сесії");
    return { runtime_id: id(runtime.runtime_id), session_id: id(runtime.session_id), command: runtime.command,
      pane_pid: runtime.pane_pid === null ? null : integer(runtime.pane_pid, 1, 2 ** 31), exited: runtime.exited,
      width: integer(runtime.width, 1, 10000), height: integer(runtime.height, 1, 10000) };
  });
  if (new Set(runtimes.map(r => r.runtime_id)).size !== runtimes.length) throw Error("Повторений runtime ID");
  return { agent_pid: integer(item.agent_pid, 1, 2 ** 31), runtimes };
}
export function parseAttachment(value: unknown): Attachment {
  const item = record(value);
  return { view_id: id(item.view_id), cols: integer(item.cols, 2, 500), rows: integer(item.rows, 2, 200) };
}
export function parseChunk(value: unknown): Chunk {
  const item = record(value);
  if (typeof item.writable !== "boolean" || typeof item.closed !== "boolean" || !Array.isArray(item.data) || item.data.length > 8192) throw Error("Некоректний потік термінала");
  return { writable: item.writable, closed: item.closed, data: item.data.map((byte: unknown) => integer(byte, 0, 255)) };
}
export const localClient = {
  inventory: async () => parseInventory(await invoke<unknown>("local_inventory")),
  attach: async (runtimeId: string) => parseAttachment(await invoke<unknown>("terminal_attach", { runtimeId })),
  read: async (viewId: string) => parseChunk(await invoke<unknown>("terminal_read", { viewId })),
  input: async (viewId: string, data: readonly number[], paste: boolean) => { await invoke("terminal_input", { viewId, data, paste }); },
  detach: async (viewId: string) => { await invoke("terminal_detach", { viewId }); },
};
export function runtimeTitle(runtime: RuntimeInfo): string {
  const command = runtime.command.toLowerCase();
  const name = command.includes("claude") ? "Claude Code" : command.includes("codex") ? "Codex" : runtime.command || "Завершена сесія";
  return `${name} · ${runtime.runtime_id.slice(0, 6)}`;
}
