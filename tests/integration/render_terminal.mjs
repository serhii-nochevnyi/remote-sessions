// Decode real attachment bytes with the same xterm 6 engine used by the client.
import fs from 'node:fs';
import xterm from '@xterm/headless';
const input = JSON.parse(fs.readFileSync(0, 'utf8'));
const terminal = new xterm.Terminal({ cols: input.cols, rows: input.rows, allowProposedApi: true, scrollback: 2000 });
await new Promise(resolve => terminal.write(Buffer.from(input.base64, 'base64'), resolve));
const buffer = terminal.buffer.active;
const lines = Array.from({ length: terminal.rows }, (_, row) => buffer.getLine(buffer.baseY + row)?.translateToString(true) ?? '');
const text = lines.join('\n');
const missing = input.expected.filter(value => !text.includes(value));
console.log(JSON.stringify({ text, missing, buffer: buffer.type, cols: terminal.cols, rows: terminal.rows }));
terminal.dispose();
if (missing.length) process.exitCode = 1;
