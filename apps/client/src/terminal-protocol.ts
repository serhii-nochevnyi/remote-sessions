import type { Terminal } from "@xterm/xterm";

/** tmux owns the application's terminal protocol. Viewer negotiation must never
 * be forwarded through send-keys as if a person typed it into the application.
 * Negotiation replies to the viewer PTY are deferred; tmux uses its TERM defaults.
 */
export function suppressViewerQueries(terminal: Pick<Terminal, "parser">) {
  return [
    terminal.parser.registerCsiHandler({ final: "c" }, () => true),
    terminal.parser.registerCsiHandler({ prefix: ">", final: "c" }, () => true),
    terminal.parser.registerCsiHandler({ final: "n" }, () => true),
    terminal.parser.registerCsiHandler({ prefix: "?", final: "n" }, () => true),
    terminal.parser.registerCsiHandler({ intermediates: "$", final: "p" }, () => true),
    terminal.parser.registerCsiHandler({ prefix: "?", intermediates: "$", final: "p" }, () => true),
    terminal.parser.registerCsiHandler({ final: "t" }, params => [14, 16, 18, 20, 21].includes(Number(params[0]))),
    terminal.parser.registerDcsHandler({ intermediates: "$", final: "q" }, () => true),
    terminal.parser.registerDcsHandler({ intermediates: "+", final: "q" }, () => true),
    ...[4, 10, 11, 12].map(id => terminal.parser.registerOscHandler(id, () => true)),
  ];
}
