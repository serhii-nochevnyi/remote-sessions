import assert from "node:assert/strict";
import { mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { build } from "vite";

const configFile = fileURLToPath(new URL("./vite.config.ts", import.meta.url));
const desktopModule = fileURLToPath(new URL("../src/session-state.ts", import.meta.url));
const repoRoot = fileURLToPath(new URL("../../../", import.meta.url));

test("guarded bundle rejects desktop modules and external assets", async () => {
  const root = mkdtempSync(join(tmpdir(), "w2-bundle-"));
  const write = (name: string, source: string) => writeFileSync(join(root, name), source);
  symlinkSync(join(repoRoot, "node_modules"), join(root, "node_modules"), "dir");
  const html = '<!doctype html><style>body { color: black; }</style>'
    + '<img src="./marker.svg"><script type="module" src="./entry.tsx"></script>';
  const entry = 'import "./local.css"; document.body.dataset.fixture = "local-control";';
  const bundle = () => build({
    configFile, root, logLevel: "silent", build: { write: false },
  });

  try {
    write("index.html", html);
    write("entry.tsx", entry);
    write("local.css", "body { background-image: url(./marker.svg); }");
    write("marker.svg", '<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>');
    const result = await bundle();
    assert("output" in result, "Vite emits a bundled local control");
    assert(result.output.some(file => file.fileName === "index.html"));
    assert(result.output.some(file => file.fileName.endsWith(".svg")));
    assert(result.output.some(file => file.fileName.endsWith(".css")));
    assert(result.output.some(file => file.type === "chunk" && file.code.includes("local-control")));

    write("entry.tsx", `import { demoSessions } from ${JSON.stringify(desktopModule)};
      document.body.dataset.fixture = demoSessions[0].id;`);
    await assert.rejects(bundle, /Module is outside the fixture and its locked packages: .*session-state\.ts/);
    write("entry.tsx", entry);

    write("index.html", html.replace("./marker.svg", "//example.invalid/remote.svg"));
    await assert.rejects(bundle, /External asset is forbidden in the fixture: .*remote\.svg/);
    write("index.html", html.replace("./marker.svg", "https&#58;//example.invalid/remote.svg"));
    await assert.rejects(bundle, /External asset is forbidden in the fixture: .*remote\.svg/);
    write("index.html", html);

    write("local.css", 'body { background-image: url("https://example.invalid/remote.png"); }');
    await assert.rejects(bundle, /External asset is forbidden in the fixture: .*remote\.png/);
    write("local.css", String.raw`body { background-image: u\72l(\68ttps://example.invalid/escaped.png); }`);
    await assert.rejects(bundle, /External asset is forbidden in the fixture: .*escaped\.png/);
    write("local.css", "body { color: black; }");

    write("index.html", html + '<iframe srcdoc="&lt;img src=//example.invalid/remote.svg&gt;"></iframe>');
    await assert.rejects(bundle, /embedded HTML are forbidden in the fixture/);
    write("index.html", html);

    write("entry.tsx", 'document.body.dataset.fixture = new URL("https://example.invalid/remote.woff2", import.meta.url).href;');
    await assert.rejects(bundle, /External asset is forbidden in the fixture: .*remote\.woff2/);

    write("entry.tsx", '<img src="https://example.invalid/remote.svg" alt="remote" />;');
    await assert.rejects(bundle, /External asset is forbidden in the fixture: .*remote\.svg/);
    write("entry.tsx", '<img src="https&#58;//example.invalid/remote.svg" alt="remote" />;');
    await assert.rejects(bundle, /External asset is forbidden in the fixture: .*remote\.svg/);
    write("entry.tsx", '<img src={"https://example.invalid/remote.svg"} alt="remote" />;');
    await assert.rejects(bundle, /External asset is forbidden in the fixture: .*remote\.svg/);
    write("entry.tsx", 'const remote = "https://example.invalid/remote.svg"; <img src={remote} alt="remote" />;');
    await assert.rejects(bundle, /Dynamic JSX asset reference is forbidden in the offline fixture: src/);
    write("entry.tsx", 'const props = { src: "https://example.invalid/remote.svg" }; <img {...props} alt="remote" />;');
    await assert.rejects(bundle, /Opaque JSX props are forbidden in the offline fixture/);

    write("entry.tsx", '<img src="./marker.svg" alt="local" />;');
    const localJsx = await bundle();
    assert("output" in localJsx, "Vite emits a local JSX asset control");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
