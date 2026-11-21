import assert from "node:assert/strict";
import test from "node:test";
import headless from "@xterm/headless";
import { suppressViewerQueries } from "./terminal-protocol.ts";

test("tmux negotiation cannot turn into application input; user input remains intact", async () => {
  const terminal = new headless.Terminal({ allowProposedApi: true });
  const handlers = suppressViewerQueries(terminal);
  const input: string[] = [];
  terminal.onData(data => input.push(data));
  await new Promise<void>(resolve => terminal.write("\x1b[c\x1b[>c\x1b[6n\x1b[?2026$p\x1b]10;?\x07\x1b]11;?\x07\x1bP$qm\x1b\\", resolve));
  assert.deepEqual(input, []);
  terminal.input("Привіт\x1b[A\r", true);
  assert.deepEqual(input, ["Привіт\x1b[A\r"]);
  handlers.forEach(handler => handler.dispose());
  terminal.dispose();
});
