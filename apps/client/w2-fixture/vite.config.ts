import { existsSync, readFileSync, realpathSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, isAbsolute, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import react from "@vitejs/plugin-react";
import * as ts from "typescript";
import { defineConfig, type Plugin } from "vite";

const fixtureRoot = fileURLToPath(new URL("./", import.meta.url));
const repoRoot = resolve(fixtureRoot, "../../..");
const resolveModule = createRequire(import.meta.url);
const lock = JSON.parse(readFileSync(resolve(repoRoot, "package-lock.json"), "utf8")) as {
  packages: Record<string, { version?: string }>;
};
const runtimeModules = ["@tauri-apps/api", "@xterm/xterm", "@xterm/addon-fit", "react", "react-dom", "scheduler"];
const assetAttributes = new Set([
  "src", "href", "xlink:href", "xlinkhref", "srcset", "imagesrcset", "poster", "data", "action",
  "formaction", "background", "style",
]);
const srcSetAttributes = new Set(["srcset", "imagesrcset"]);
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
  const decoded = reference.replace(/&(?:#(?:x([\da-f]+)|(\d+))|(colon|sol|tab|newline));?/gi,
    (entity, hex: string | undefined, decimal: string | undefined, named: string | undefined) => {
      if (hex !== undefined || decimal !== undefined) {
        const codePoint = Number.parseInt(hex ?? decimal ?? "", hex === undefined ? 10 : 16);
        return codePoint <= 0x10ffff ? String.fromCodePoint(codePoint) : "\ufffd";
      }
      switch (named?.toLowerCase()) {
        case "colon": return ":";
        case "sol": return "/";
        case "tab": return "\t";
        case "newline": return "\n";
        default: return entity;
      }
    });
  if (/^(?:[a-z][a-z\d+.-]*:|\/\/)|[\\&\u0000-\u0020]/i.test(decoded.trim())) {
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
  for (const imageSetMatch of source.matchAll(/(?:-webkit-)?image-set\([^;{}]*/gi)) {
    const imageSet = imageSetMatch[0] ?? "";
    for (const quotedMatch of imageSet.matchAll(/["']([^"']*)["']/g)) {
      const quoted = quotedMatch[1];
      if (quoted !== undefined) assertLocal(quoted);
    }
  }
}

function assertAssetAttribute(name: string, value: string) {
  if (name === "style") {
    assertCss(value);
  } else if (srcSetAttributes.has(name)) {
    for (const candidate of value.split(",")) {
      const reference = candidate.trim().split(/\s+/, 1)[0];
      if (reference) assertLocal(reference);
    }
  } else {
    assertLocal(value);
  }
}

function inlineProperties(object: ts.ObjectLiteralExpression, context: string) {
  return object.properties.map(property => {
    if ((ts.isPropertyAssignment(property) || ts.isShorthandPropertyAssignment(property))
        && (ts.isIdentifier(property.name) || ts.isStringLiteral(property.name)
          || ts.isNumericLiteral(property.name))) {
      return {
        name: property.name.text.toLowerCase(),
        value: ts.isPropertyAssignment(property) ? property.initializer : property.name,
      };
    }
    throw new Error(`Opaque ${context} props are forbidden in the offline fixture`);
  });
}

function bundledUrl(value: ts.Expression) {
  if (!ts.isPropertyAccessExpression(value) || value.name.text !== "href") return;
  const url = value.expression;
  if (!ts.isNewExpression(url) || !ts.isIdentifier(url.expression) || url.expression.text !== "URL"
      || url.arguments?.length !== 2) return;
  const [path, base] = url.arguments;
  if (path && ts.isStringLiteralLike(path) && base && ts.isPropertyAccessExpression(base)
      && base.name.text === "url" && ts.isMetaProperty(base.expression)
      && base.expression.keywordToken === ts.SyntaxKind.ImportKeyword
      && base.expression.name.text === "meta") return path.text;
}

function assertReactAssets(props: ts.Expression | undefined) {
  if (!props || props.kind === ts.SyntaxKind.NullKeyword) return;
  if (!ts.isObjectLiteralExpression(props)) {
    throw new Error("Opaque React props are forbidden in the offline fixture");
  }
  for (const { name, value } of inlineProperties(props, "React")) {
    if (!assetAttributes.has(name)) continue;
    if (name === "style") {
      if (value.kind === ts.SyntaxKind.NullKeyword) continue;
      if (!ts.isObjectLiteralExpression(value)) {
        throw new Error("Opaque React style props are forbidden in the offline fixture");
      }
      for (const { value: declaration } of inlineProperties(value, "React style")) {
        if (ts.isStringLiteralLike(declaration)) assertAssetAttribute(name, declaration.text);
        else if (!ts.isNumericLiteral(declaration) && declaration.kind !== ts.SyntaxKind.NullKeyword) {
          throw new Error("Dynamic React asset reference is forbidden in the offline fixture: style");
        }
      }
      continue;
    }
    if (ts.isStringLiteralLike(value)) {
      assertAssetAttribute(name, value.text);
      continue;
    }
    const url = bundledUrl(value);
    if (url !== undefined) {
      assertLocal(url);
      if (!srcSetAttributes.has(name) && (url.startsWith("./") || url.startsWith("../"))) continue;
    }
    throw new Error(`Dynamic React asset reference is forbidden in the offline fixture: ${name}`);
  }
}

function assertSourceAssets(source: string, id: string) {
  const scriptKind = /\.(?:m|c)?tsx$/i.test(id) ? ts.ScriptKind.TSX
    : /\.(?:m|c)?jsx$/i.test(id) ? ts.ScriptKind.JSX
      : /\.(?:m|c)?ts$/i.test(id) ? ts.ScriptKind.TS : ts.ScriptKind.JS;
  const file = ts.createSourceFile(id, source, ts.ScriptTarget.Latest, true, scriptKind);
  const reactObjects = new Set<string>();
  const reactFactories = new Set<string>();
  for (const statement of file.statements) {
    if (!ts.isImportDeclaration(statement) || !ts.isStringLiteral(statement.moduleSpecifier)
        || statement.moduleSpecifier.text !== "react") continue;
    const clause = statement.importClause;
    if (!clause || clause.isTypeOnly) continue;
    if (clause.name) reactObjects.add(clause.name.text);
    const bindings = clause.namedBindings;
    if (!bindings) continue;
    if (ts.isNamespaceImport(bindings)) reactObjects.add(bindings.name.text);
    else {
      for (const binding of bindings.elements) {
        if (binding.isTypeOnly) continue;
        const imported = (binding.propertyName ?? binding.name).text;
        if (imported === "createElement") reactFactories.add(binding.name.text);
        if (imported === "default") reactObjects.add(binding.name.text);
      }
    }
  }

  function visit(node: ts.Node) {
    if (ts.isCallExpression(node)) {
      const callee = node.expression;
      if ((ts.isIdentifier(callee) && reactFactories.has(callee.text))
          || (ts.isPropertyAccessExpression(callee) && callee.name.text === "createElement"
            && ts.isIdentifier(callee.expression) && reactObjects.has(callee.expression.text))) {
        const [tag, props] = node.arguments;
        if ((tag && ts.isSpreadElement(tag)) || (props && ts.isSpreadElement(props))) {
          throw new Error("Spread arguments for the React tag or props are forbidden in the offline fixture");
        }
        assertReactAssets(props);
      }
    }
    if (ts.isJsxOpeningElement(node) || ts.isJsxSelfClosingElement(node)) {
      for (const attribute of node.attributes.properties) {
        if (ts.isJsxSpreadAttribute(attribute)) {
          throw new Error("Opaque JSX props are forbidden in the offline fixture");
        }
        if (!ts.isJsxAttribute(attribute)) continue;
        const name = attribute.name.getText(file).toLowerCase();
        if (!assetAttributes.has(name) || attribute.initializer === undefined) continue;

        const value = attribute.initializer;
        if (ts.isStringLiteralLike(value)) {
          assertAssetAttribute(name, value.text);
        } else if (ts.isJsxExpression(value)) {
          const expression = value.expression;
          if (expression === undefined) continue;
          if (ts.isStringLiteralLike(expression)) {
            assertAssetAttribute(name, expression.text);
          } else {
            throw new Error(`Dynamic JSX asset reference is forbidden in the offline fixture: ${name}`);
          }
        } else {
          throw new Error(`Unsupported JSX asset reference is forbidden in the offline fixture: ${name}`);
        }
      }
    }
    ts.forEachChild(node, visit);
  }

  visit(file);
}

function assertHtml(html: string) {
  const source = html.replace(/<!--[\s\S]*?-->/g, "");
  for (const [tag] of source.matchAll(/<[a-z](?:"[^"]*"|'[^']*'|[^'">])*>/gi)) {
    if (/^<base\b|\s(?:http-equiv|srcdoc)\s*=/i.test(tag)) {
      throw new Error("Document redirects, base URLs and embedded HTML are forbidden in the fixture");
    }
    for (const match of tag.matchAll(
      /\s(src|href|xlink:href|srcset|imagesrcset|poster|data|action|formaction|background|style)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))/gi,
    )) {
      const name = match[1];
      if (name === undefined) continue;
      const double = match[2];
      const single = match[3];
      const bare = match[4];
      const value = double ?? single ?? bare;
      if (value !== undefined) assertAssetAttribute(name.toLowerCase(), value);
    }
  }
  for (const styleMatch of source.matchAll(/<style\b[^>]*>([\s\S]*?)<\/style\s*>/gi)) {
    const css = styleMatch[1];
    if (css !== undefined) assertCss(css);
  }
}

function guardFixtureBundle(): Plugin {
  let appRoot: string;
  let packageRoots: string[];

  function assertModule(id: string) {
    if (virtualModules.has(id)) return;
    let file = id.split("?", 1)[0] ?? id;
    if (!isAbsolute(file)) {
      try {
        file = resolveModule.resolve(file);
      } catch {
        throw new Error(`Module is outside the fixture and its locked packages: ${id}`);
      }
    }
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
        if (!source.startsWith("\0")) assertLocal(source.split("?", 1)[0] ?? source);
      },
    },
    transform: {
      order: "pre",
      handler(source, id) {
        if (/\.css(?:$|\?)/i.test(id)) {
          assertCss(source);
          for (const match of source.matchAll(
            /@import\s+(?:url\(\s*)?(?:"([^"]*)"|'([^']*)'|([^\s);]+))/gi,
          )) {
            const double = match[1];
            const single = match[2];
            const bare = match[3];
            const path = double ?? single ?? bare;
            if (path === undefined) continue;
            if (path.startsWith(".")) assertModule(resolve(dirname(id.split("?", 1)[0] ?? id), path));
            else if (path.startsWith("/")) assertModule(resolve(appRoot, `.${path}`));
          }
        } else if (/\.[cm]?[jt]sx?(?:$|\?)/i.test(id) && isAbsolute(id)) {
          const file = id.split("?", 1)[0];
          if (file === undefined || !existsSync(file) || !inside(appRoot, realpathSync(file))) return;
          assertSourceAssets(source, file);
          for (const match of source.matchAll(/\bnew\s+URL\s*\(\s*(["'`])([^"'`]*)\1/g)) {
            const url = match[2];
            if (url !== undefined) assertLocal(url);
          }
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
  plugins: [guardFixtureBundle(), react()],
  build: { assetsInlineLimit: 0 },
});
