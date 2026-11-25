import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { build } from "vite";

const configFile = fileURLToPath(new URL("./vite.config.ts", import.meta.url));
const desktopModule = fileURLToPath(new URL("../src/session-state.ts", import.meta.url));

test("guarded bundle rejects desktop modules and external assets", async () => {
  const root = mkdtempSync(join(tmpdir(), "w2-bundle-"));
  const write = (name: string, source: string) => writeFileSync(join(root, name), source);
  const html = '<!doctype html><style>body { color: black; }</style>'
    + '<img src="./marker.svg"><script type="module" src="./entry.ts"></script>';
  const entry = 'import "./local.css"; document.body.dataset.fixture = "local-control";';
  const bundle = () => build({
    configFile, root, logLevel: "silent", build: { write: false },
  });

  try {
    write("index.html", html);
    write("entry.ts", entry);
    write("local.css", "body { background-image: url(./marker.svg); }");
    write("marker.svg", '<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>');
    const result = await bundle();
    assert("output" in result, "Vite emits a bundled local control");
    assert(result.output.some(file => file.fileName === "index.html"));
    assert(result.output.some(file => file.fileName.endsWith(".svg")));
    assert(result.output.some(file => file.fileName.endsWith(".css")));
    assert(result.output.some(file => file.type === "chunk" && file.code.includes("local-control")));

    write("entry.ts", `import { demoSessions } from ${JSON.stringify(desktopModule)};
      document.body.dataset.fixture = demoSessions[0].id;`);
    await assert.rejects(bundle, /Module is outside the fixture and its locked packages: .*session-state\.ts/);
    write("entry.ts", entry);

    write("index.html", html.replace("./marker.svg", "//example.invalid/remote.svg"));
    await assert.rejects(bundle, /External asset is forbidden in the fixture: .*remote\.svg/);
    write("index.html", html);

    write("local.css", 'body { background-image: url("https://example.invalid/remote.png"); }');
    await assert.rejects(bundle, /External asset is forbidden in the fixture: .*remote\.png/);
    write("local.css", "body { color: black; }");

    write("entry.ts", 'document.body.dataset.fixture = new URL("https://example.invalid/remote.woff2", import.meta.url).href;');
    await assert.rejects(bundle, /External asset is forbidden in the fixture: .*remote\.woff2/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
