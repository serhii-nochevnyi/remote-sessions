import { readdir, readFile, access } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';

const ignored = new Set(['.git', '.cache', 'node_modules', 'target', 'dist', 'gen']);
async function walk(path) {
  const files = [];
  for (const entry of await readdir(path, { withFileTypes: true })) {
    if (ignored.has(entry.name)) continue;
    const child = join(path, entry.name);
    if (entry.isDirectory()) files.push(...await walk(child));
    else if (entry.name.endsWith('.md')) files.push(child);
  }
  return files;
}

const files = await walk(process.cwd());
for (const file of files) {
  const text = await readFile(file, 'utf8');
  for (const match of text.matchAll(/\]\(([^)]+)\)/g)) {
    const link = match[1];
    if (/^(https?:|mailto:|#)/.test(link)) continue;
    await access(resolve(dirname(file), link.split('#')[0]));
  }
}
console.log(`Validated local links in ${files.length} Markdown files.`);
