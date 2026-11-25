import { existsSync, readFileSync, realpathSync } from "node:fs";
import { dirname, isAbsolute, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import react from "@vitejs/plugin-react";
import { defineConfig, type Plugin } from "vite";

const fixtureRoot = fileURLToPath(new URL("./", import.meta.url));
const repoRoot = resolve(fixtureRoot, "../../..");
const lock = JSON.parse(readFileSync(resolve(repoRoot, "package-lock.json"), "utf8")) as {
  packages: Record<string, { version?: string }>;
};
const runtimeModules = ["@tauri-apps/api", "@xterm/xterm", "react", "react-dom", "scheduler"];
const virtualModules = new Set([
  "\0rolldown/runtime.js",
  "\0vite/modulepreload-polyfill.js",
  "\0vite/preload-helper.js",
]);

function lockedRoots() {
  return runtimeModules.map(name => {
    const path = `node_modules/${name}`;
    const directory = realpathSync(resolve(repoRoot, path));
    const installed = JSON.parse(readFileSync(resolve(directory, "package.json"), "utf8"));
    if (installed.name !== name || !lock.packages[path]?.version
        || installed.version !== lock.packages[path]?.version) {
      throw new Error(`Fixture dependency is outside the root lock: ${name}`);
    }
    return directory;
  });
}

function inside(root: string, path: string) {
  const fromRoot = relative(root, path);
  return fromRoot !== ".." && !fromRoot.startsWith(`..${sep}`) && !isAbsolute(fromRoot);
}

function assertLocal(reference: string) {
  if (/^(?:[a-z][a-z\d+.-]*:|\/\/)|[\\&\u0000-\u0020]/i.test(reference.trim())) {
    throw new Error(`External asset is forbidden in the fixture: ${reference}`);
  }
}

function assertCss(css: string) {
  const source = css.replace(/\x2f\x2a[\s\S]*?\x2a\x2f/g, "").replace(
    /\\(?:([\da-f]{1,6})(?:\r\n|[\t\n\f\r ])?|(\r\n|[\s\S]))/gi,
    (_escape, hex: string | undefined, literal: string | undefined) => {
      if (!hex) return literal?.replace(/[\r\n\f]/g, "") ?? "";
      const codePoint = Number.parseInt(hex, 16);
      return codePoint <= 0x10ffff ? String.fromCodePoint(codePoint) : "\ufffd";
    },
  );
  for (const match of source.matchAll(/url\(\s*(?:"([^"]*)"|'([^']*)'|([^)]*))\s*\)|@import\s+(?:"([^"]*)"|'([^']*)')/gi)) {
    assertLocal(match.slice(1).find(value => value !== undefined) ?? "");
  }
  for (const [imageSet] of source.matchAll(/(?:-webkit-)?image-set\([^;{}]*/gi)) {
    for (const [, quoted] of imageSet.matchAll(/["']([^"']*)["']/g)) assertLocal(quoted);
  }
}

function assertHtml(html: string) {
  const source = html.replace(/<!--[\s\S]*?-->/g, "");
  for (const [tag] of source.matchAll(/<[a-z](?:"[^"]*"|'[^']*'|[^'">])*>/gi)) {
    if (/^<base\b|\s(?:http-equiv|srcdoc)\s*=/i.test(tag)) {
      throw new Error("Document redirects, base URLs and embedded HTML are forbidden in the fixture");
    }
    for (const [, name, double, single, bare] of tag.matchAll(
      /\s(src|href|xlink:href|srcset|imagesrcset|poster|data|action|formaction|background|style)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))/gi,
    )) {
      const value = double ?? single ?? bare;
      if (name.toLowerCase() === "style") assertCss(value);
      else if (name.toLowerCase().endsWith("srcset")) {
        for (const candidate of value.split(",")) assertLocal(candidate.trim().split(/\s+/, 1)[0]);
      } else assertLocal(value);
    }
  }
  for (const [, css] of source.matchAll(/<style\b[^>]*>([\s\S]*?)<\/style\s*>/gi)) assertCss(css);
}

function guardFixtureBundle(): Plugin {
  let appRoot: string;
  let packageRoots: string[];

  function assertModule(id: string) {
    if (virtualModules.has(id)) return;
    const file = id.split("?", 1)[0];
    if (isAbsolute(file) && existsSync(file)) {
      const actual = realpathSync(file);
      if (inside(appRoot, actual) || packageRoots.some(root => inside(root, actual))) return;
    }
    throw new Error(`Module is outside the fixture and its locked packages: ${id}`);
  }

  return {
    name: "guard-offline-fixture",
    apply: "build",
    configResolved(config) {
      appRoot = realpathSync(config.root);
      packageRoots = lockedRoots();
    },
    resolveId: {
      order: "pre",
      handler(source) {
        if (!source.startsWith("\0")) assertLocal(source.split("?", 1)[0]);
      },
    },
    transform: {
      order: "pre",
      handler(source, id) {
        if (/\.css(?:$|\?)/i.test(id)) {
          assertCss(source);
          for (const [, double, single, bare] of source.matchAll(
            /@import\s+(?:url\(\s*)?(?:"([^"]*)"|'([^']*)'|([^\s);]+))/gi,
          )) {
            const path = double ?? single ?? bare;
            if (path.startsWith(".")) assertModule(resolve(dirname(id.split("?", 1)[0]), path));
            else if (path.startsWith("/")) assertModule(resolve(appRoot, `.${path}`));
          }
        } else if (/\.[cm]?[jt]sx?(?:$|\?)/i.test(id) && isAbsolute(id)) {
          const file = id.split("?", 1)[0];
          if (!existsSync(file) || !inside(appRoot, realpathSync(file))) return;
          for (const [, , url] of source.matchAll(/\bnew\s+URL\s*\(\s*(["'`])([^"'`]*)\1/g)) assertLocal(url);
        }
      },
    },
    transformIndexHtml: { order: "pre", handler: assertHtml },
    generateBundle: {
      order: "post",
      handler(_options, bundle) {
        for (const id of this.getModuleIds()) assertModule(id);
        for (const entry of Object.values(bundle)) {
          if (entry.type === "chunk") {
            for (const id of entry.moduleIds) assertModule(id);
            for (const imported of [...entry.imports, ...entry.dynamicImports]) {
              if (!Object.hasOwn(bundle, imported)) this.error(`External bundle import: ${imported}`);
            }
          } else {
            for (const file of entry.originalFileNames) assertModule(resolve(appRoot, file));
            if (/\.(?:css|html|svg)$/i.test(entry.fileName)) {
              const source = typeof entry.source === "string" ? entry.source : Buffer.from(entry.source).toString();
              if (entry.fileName.endsWith(".css")) assertCss(source);
              else assertHtml(source);
            }
          }
        }
      },
    },
  };
}

export default defineConfig({
  root: fixtureRoot,
  base: "./",
  publicDir: false,
  plugins: [react(), guardFixtureBundle()],
  build: { assetsInlineLimit: 0 },
});
