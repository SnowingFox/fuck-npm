#!/usr/bin/env node
/**
 * Ports bun's package-manager tests (submodule/bun/test/cli/install) to the fnpm conformance suite, and writes
 * tests/conformance/INDEX.md.
 *
 *   node tests/conformance/tools/port-bun-tests.ts            # every test that is not excluded or todo
 *   node tests/conformance/tools/port-bun-tests.ts bun-add.test.ts migration/migrate.test.ts
 *
 * The port is mechanical so it can be redone when the submodule moves:
 * - imports: bun:test -> compat/bun-test.ts (vitest), bun -> compat/bun.ts, harness -> harness.ts,
 *   registry -> compat/registry.ts, ./dummy.registry -> compat/dummy-registry.ts,
 *   bun:internal-for-testing -> compat/internal-for-testing.ts; other relative imports point into the submodule
 *   (fixtures), except the helper modules that are ported next to the tests.
 * - names: bunExe -> fnpmExe, bunEnv -> fnpmEnv, runBunInstall -> runFnpmInstall, runBunUpdate -> runFnpmUpdate,
 *   normalizeBunSnapshot -> normalizeFnpmSnapshot.
 * - text: bun.lock -> fnpm.lock (bun.lockb is kept), "bun install" -> "fnpm install" (and the other pm commands),
 *   BUN_INSTALL_CACHE_DIR -> FNPM_CACHE_DIR, BUN_CONFIG_* -> FNPM_CONFIG_*, node_modules/.bun -> node_modules/.fnpm.
 * - import.meta.dir / import.meta.path -> the directory / file of the bun original, so fixtures resolve.
 * - __snapshots__/*.snap get the same text changes.
 */
import fs from "node:fs";
import { dirname, join, posix, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";

const here = dirname(fileURLToPath(import.meta.url));
const conformanceDir = resolve(here, "..");
const repoRoot = resolve(conformanceDir, "..", "..");
/** The submodule, where the ported files point to; FNPM_BUN_DIR reads another checkout of the same commit. */
const submoduleRoot = join(repoRoot, "submodule", "bun");
const bunRoot = process.env.FNPM_BUN_DIR ?? submoduleRoot;
const sourceRoot = join(bunRoot, "test", "cli", "install");
const outputRoot = join(conformanceDir, "install");

/** Tests that are not ported, with the reason. */
const excluded: Record<string, string> = {
  "bun-upgrade.test.ts": "bun self-upgrade (runtime binary), not a package-manager feature",
  "bun-install-android-seccomp.test.ts": "bun runtime on Android (seccomp sandbox of the bun binary)",
  "bun-install-thread-spawn-failure.test.ts": "bun-internal thread pool fault injection",
  "bun-lockb.test.ts": "binary lockfile bun.lockb; fnpm only has the text lockfile",
  "migrate-bun-lockb-v2.test.ts": "migration from binary bun.lockb; not in fnpm's migration set",
  "bun-run-dir.test.ts": "bun runtime auto-install when running a file",
  "semver.test.ts": "Bun.semver JS API; semver behaviour is covered by crates/fnpm-semver unit tests",
};

/** Tests that belong elsewhere or need fnpm APIs that do not exist yet. */
const todo: Record<string, string> = {
  "bun-run.test.ts": "mostly bun runtime (running files, tsconfig); port the package.json-script parts with `fnpm run`",
  "bun-run-bunfig.test.ts": "`bun run` + bunfig autoload; port with `fnpm run`",
  "hosted-git-info/boundary-conditions.test.ts": "unit tests of bun's hosted-git-info parser (bun:internal-for-testing); port as fnpm-resolver Rust tests",
  "hosted-git-info/from-url.test.ts": "unit tests of bun's hosted-git-info parser (bun:internal-for-testing); port as fnpm-resolver Rust tests",
  "hosted-git-info/parse-url.test.ts": "unit tests of bun's hosted-git-info parser (bun:internal-for-testing); port as fnpm-resolver Rust tests",
};

/** Non-test modules that are ported next to the tests (they import bun / harness themselves). */
const helpers = [
  "simple-dummy-registry.ts",
  "bun-security-scanner-matrix-runner.ts",
  "migration/migration-harness.ts",
  "hosted-git-info/cases.ts",
];

const renames: [RegExp, string][] = [
  [/\bbunExe\b/g, "fnpmExe"],
  [/\bbunEnv\b/g, "fnpmEnv"],
  [/\brunBunInstall\b/g, "runFnpmInstall"],
  [/\brunBunUpdate\b/g, "runFnpmUpdate"],
  [/\bnormalizeBunSnapshot\b/g, "normalizeFnpmSnapshot"],
];

const pmCommands =
  "init|add|remove|update|outdated|link|unlink|pm|patch|patch-commit|publish|audit|info|why|pack|create|prune|dedupe|x|i|a|rm|ci";

const textChanges: [RegExp, string][] = [
  [/\bbun\.lock(?![b\w])/g, "fnpm.lock"],
  [new RegExp(`\\bbun (${pmCommands})\\b`, "g"), "fnpm $1"],
  // bun's header is `bun install v1.x.y`; fnpm has its own version (`fnpm install v0.0.1`).
  [/\bfnpm (install|add|update|remove|link|unlink|outdated) v1\./g, "fnpm $1 v"],
  // A command name in a variable: `bun ${command} has no --x flag`, `bun ${command} <version> (<revision>)`
  // (not `bun ${script}.js`, which runs a file).
  [/\bbun (\$\{(?:command|header|cmd|subcommand)\})/g, "fnpm $1"],
  // Messages that name the package manager itself: fnpm prints its own name there.
  [/\b[Bb]un (?=(?:does not|doesn't|is configured|currently only supports|cannot tell|supports one|can't keep)\b)/g, "fnpm "],
  [/\bnewer version of Bun\b/g, "newer version of fnpm"],
  [/\bBUN_INSTALL_CACHE_DIR\b/g, "FNPM_CACHE_DIR"],
  [/\bBUN_CONFIG_/g, "FNPM_CONFIG_"],
  [/node_modules\/\.bun\//g, "node_modules/.fnpm/"],
  [/node_modules\/\.bun\b(?!-)/g, "node_modules/.fnpm"],
  [/"\.bun"/g, '".fnpm"'],
  [/([`"'])\.bun\//g, "$1.fnpm/"],
];

function transformText(text: string): string {
  for (const [pattern, replacement] of [...renames, ...textChanges]) text = text.replace(pattern, replacement);
  return text;
}

function rel(fromFile: string, toFile: string): string {
  // A file of the bun checkout is referenced through the submodule path, wherever it was read.
  const inBun = relative(bunRoot, toFile);
  if (!inBun.startsWith("..")) toFile = join(submoduleRoot, inBun);
  let path = relative(dirname(fromFile), toFile).split("\\").join("/");
  if (!path.startsWith(".")) path = `./${path}`;
  return path;
}

function mapSpecifier(specifier: string, sourceFile: string, outFile: string): string {
  const compat = (name: string) => rel(outFile, join(conformanceDir, name));
  switch (specifier) {
    case "bun:test":
      return compat("compat/bun-test.ts");
    case "bun":
      return compat("compat/bun.ts");
    case "harness":
      return compat("harness.ts");
    case "registry":
      return compat("compat/registry.ts");
    case "bun:internal-for-testing":
      return compat("compat/internal-for-testing.ts");
    case "mkfifo":
      return compat("compat/mkfifo.ts");
    case "path":
    case "node:path":
      return compat("compat/path.ts");
    case "fs/promises":
    case "node:fs/promises":
      return compat("compat/fs-promises.ts");
  }
  if (!specifier.startsWith(".")) return specifier;
  const target = resolve(dirname(sourceFile), specifier);
  const relToSource = relative(sourceRoot, target).split("\\").join("/").replace(/\.(ts|js)$/, "");
  if (relToSource === "dummy.registry") return compat("compat/dummy-registry.ts");
  const helper = helpers.find(h => h.replace(/\.ts$/, "") === relToSource);
  if (helper) return rel(outFile, join(outputRoot, helper));
  // A fixture (json, js) of bun's tree: import it from the submodule.
  const candidates = [target, `${target}.ts`, `${target}.js`, `${target}.json`];
  const existing = candidates.find(candidate => fs.existsSync(candidate) && fs.statSync(candidate).isFile()) ?? target;
  return rel(outFile, existing);
}

function rewriteImports(code: string, sourceFile: string, outFile: string): string {
  const replaceSpec = (_match: string, prefix: string, quote: string, spec: string) =>
    `${prefix}${quote}${mapSpecifier(spec, sourceFile, outFile)}${quote}`;
  return code
    .replace(/(\bfrom\s*)(["'])([^"'\n]+)\2/g, replaceSpec)
    .replace(/(\bimport\s*)(["'])([^"'\n]+)\2/g, replaceSpec)
    .replace(/(\bimport\(\s*)(["'])([^"'\n]+)\2/g, replaceSpec)
    .replace(/(\brequire\(\s*)(["'])([^"'\n]+)\2/g, (match, prefix, quote, spec) =>
      spec.startsWith(".") || spec.startsWith("bun") || spec === "harness" || spec === "registry"
        ? replaceSpec(match, prefix, quote, spec)
        : match,
    );
}

function header(relPath: string, kind: "test" | "helper"): string {
  const lines = [
    `// Ported from bun: test/cli/install/${relPath} (oven-sh/bun, MIT).`,
    `// Generated by tests/conformance/tools/port-bun-tests.ts; see tests/conformance/INDEX.md.`,
  ];
  if (kind === "test") {
    lines.push("// These tests define fnpm's target behaviour and are expected to fail until fnpm implements it.");
  }
  return `${lines.join("\n")}\n`;
}

/** Code that sets up names the bun original got from its runtime. Placed after the imports (which are hoisted anyway). */
function prelude(code: string, relPath: string, outFile: string): string {
  const parts: string[] = [];
  const needsDir = /\bBUN_TEST_(DIR|FILE)\b/.test(code);
  if (needsDir) {
    const dir = posix.dirname(`test/cli/install/${relPath}`);
    parts.push(`import { bunSourceDir as __bunSourceDir } from "${rel(outFile, join(conformanceDir, "harness.ts"))}";`);
    parts.push(`const BUN_TEST_DIR = __bunSourceDir(${JSON.stringify(dir)});`);
    parts.push(`const BUN_TEST_FILE = __bunSourceDir(${JSON.stringify(`test/cli/install/${relPath}`)});`);
  }
  if (/(^|[^.\w])require\(|import\.meta\.require\b/.test(code)) {
    parts.push(`import { createRequire as __createRequire } from "node:module";`);
    parts.push(`const require = __createRequire(import.meta.url);`);
  }
  return parts.length ? `${parts.join("\n")}\n` : "";
}

/** Bun's CommonJS globals refer to the original file. Walk syntax so embedded scripts keep theirs. */
function rewriteFileGlobals(code: string): string {
  const source = ts.createSourceFile("test.ts", code, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
  const edits: { start: number; end: number; text: string }[] = [];
  // Bind this file without loading libraries: a symbol means a local declaration or import.
  // The checker handles parameters, destructuring and shadowing in each lexical scope.
  const options: ts.CompilerOptions = { noLib: true, noResolve: true };
  const host = ts.createCompilerHost(options);
  host.getSourceFile = name => name === "test.ts" ? source : undefined;
  const checker = ts.createProgram(["test.ts"], options, host).getTypeChecker();
  function visit(node: ts.Node) {
    if (ts.isIdentifier(node) && (node.text === "__dirname" || node.text === "__filename")) {
      const parent = node.parent;
      const shorthand = ts.isShorthandPropertyAssignment(parent) && parent.name === node;
      const propertyName = (ts.isPropertyAccessExpression(parent) && parent.name === node)
        || ("name" in parent && parent.name === node && !shorthand);
      const symbol = shorthand ? checker.getShorthandAssignmentValueSymbol(parent) : checker.getSymbolAtLocation(node);
      if (!propertyName && symbol === undefined) {
        const value = node.text === "__dirname" ? "BUN_TEST_DIR" : "BUN_TEST_FILE";
        edits.push({ start: node.getStart(source), end: node.end, text: shorthand ? `${node.text}: ${value}` : value });
      }
    }
    ts.forEachChild(node, visit);
  }
  visit(source);
  for (const edit of edits.sort((a, b) => b.start - a.start)) {
    code = code.slice(0, edit.start) + edit.text + code.slice(edit.end);
  }
  return code;
}

/** Preserve the publishing tests' runtime and registry adaptations when regenerating them. */
function publishingAdaptations(code: string, relPath: string): string {
  if (!["bun-pack.test.ts", "bun-publish.test.ts", "bun-patch.test.ts"].includes(relPath)) return code;
  // fnpm is a package manager; fixture lifecycle scripts run with Node.
  code = code.replace(/\$\{fnpmExe\(\)\}(?= [\w-]+\.js\b)/g, "${nodeExe()}");
  code = code.replace(/^(\s*)fnpmExe,$/m, "$1fnpmExe,\n$1nodeExe,");
  if (relPath === "bun-pack.test.ts") {
    code = code.replace(/^(\s*)fnpmEnv,$/m, "$1fnpmEnv as sharedFnpmEnv,");
    code = code.replace(
      "// Runs `fnpm pm pack`",
      "const fnpmEnv: Record<string, string | undefined> = { ...sharedFnpmEnv, FNPM_REGISTRY: undefined };\n\n// Runs `fnpm pm pack`",
    );
  }
  if (relPath === "bun-publish.test.ts") {
    code = code.replace(/^(\s*)fnpmEnv as env,$/m, "$1fnpmEnv,");
    code = code.replace(
      "const registry = new TestRegistry();",
      "// Each test's bunfig.toml or .npmrc chooses its publishing registry.\n" +
        "const env: Record<string, string | undefined> = { ...fnpmEnv, FNPM_REGISTRY: undefined };\n\n" +
        "const registry = new TestRegistry();",
    );
    code = code.replaceAll("Bun/${Bun.version", "fnpm/${Bun.version");
    code = code.replaceAll("bunx npm login", "fnpm x npm login");
    code = code.replaceAll("bun run --filter", "fnpm run --filter");
    code = code.replace(String.raw`/^fnpm publish v\S+ \(\S+\)\n$/`, String.raw`/^fnpm publish v\S+(?: \(\S+\))?\n$/`);
  }
  return code;
}

function port(relPath: string, kind: "test" | "helper"): { out: string; tests: number } {
  const sourceFile = join(sourceRoot, relPath);
  const outFile = join(outputRoot, relPath);
  let code = rewriteFileGlobals(fs.readFileSync(sourceFile, "utf8"));
  code = code
    .replace(/(?:Bun\.)?fileURLToPath\(import\.meta\.url\)/g, "BUN_TEST_FILE")
    .replace(/import\.meta\.dirname\b/g, "BUN_TEST_DIR")
    .replace(/import\.meta\.dir\b/g, "BUN_TEST_DIR")
    .replace(/import\.meta\.path\b/g, "BUN_TEST_FILE")
    .replace(/import\.meta\.filename\b/g, "BUN_TEST_FILE")
    .replace(/import\.meta\.require\b/g, "require");
  const before = prelude(code, relPath, outFile);
  code = rewriteImports(code, sourceFile, outFile);
  code = publishingAdaptations(transformText(code), relPath);
  // A shebang must stay on the first line.
  let shebang = "";
  if (code.startsWith("#!")) {
    const end = code.indexOf("\n");
    shebang = code.slice(0, end + 1);
    code = code.slice(end + 1);
  }
  fs.mkdirSync(dirname(outFile), { recursive: true });
  fs.writeFileSync(outFile, `${shebang}${header(relPath, kind)}${before}${code}`);

  const snapshot = join(dirname(sourceFile), "__snapshots__", `${posix.basename(relPath)}.snap`);
  if (kind === "test" && fs.existsSync(snapshot)) {
    const outSnap = join(dirname(outFile), "__snapshots__", `${posix.basename(relPath)}.snap`);
    fs.mkdirSync(dirname(outSnap), { recursive: true });
    const text = transformText(fs.readFileSync(snapshot, "utf8")).replace(
      /^\/\/ Bun Snapshot v1[^\n]*/,
      "// Vitest Snapshot v1, https://vitest.dev/guide/snapshot.html",
    );
    fs.writeFileSync(outSnap, text);
  }
  return { out: outFile, tests: countTests(code) };
}

/** Test call sites (not expanded `.each` rows): `test(`, `it(`, `test.concurrent(`, `it.skipIf(x)(`, ... */
function countTests(code: string): number {
  const pattern = /(^|[^\w.$])(test|it)((?:\.(?:concurrent|serial|only|skip|todo|failing|each|skipIf|if|todoIf|runIf))|\([^()]*\))*\(\s*[`'"]/g;
  let count = 0;
  for (const _ of code.matchAll(pattern)) count++;
  // `test.each([...])("name", ...)` and `it.skipIf(x)("name", ...)` match on the trailing call.
  return count;
}

function listSourceTests(): string[] {
  const out: string[] = [];
  const walk = (dir: string) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      if (entry.name === "registry" || entry.name === "fixtures" || entry.name === "node_modules") continue;
      const full = join(dir, entry.name);
      if (entry.isDirectory()) {
        if (!entry.name.startsWith("__snapshots__")) walk(full);
      } else if (/\.test\.(ts|js)$/.test(entry.name)) {
        out.push(relative(sourceRoot, full).split("\\").join("/"));
      }
    }
  };
  walk(sourceRoot);
  return out.sort();
}

function main() {
  const args = process.argv.slice(2);
  const all = listSourceTests();
  const selected = args.length > 0 ? args : all.filter(file => !(file in excluded) && !(file in todo));
  const results = new Map<string, number>();
  for (const helper of helpers) port(helper, "helper");
  for (const file of selected) results.set(file, port(file, "test").tests);
  if (args.length === 0) writeIndex(all, results);
  const total = [...results.values()].reduce((a, b) => a + b, 0);
  console.log(`ported ${results.size} test files (${total} test call sites) + ${helpers.length} helpers`);
}

function describeOther(name: string, isDir: boolean): string {
  if (name === "dummy.registry.ts") return "harness: ported as `conformance/compat/dummy-registry.ts`";
  if (name === "__snapshots__") return "snapshots: copied next to each ported test with the same text changes";
  if (name === "registry") return "fixture: `registry/packages` is served by `tests/registry` (start() default storage)";
  if (name === "fixtures") return "fixture: read from the submodule by the ported tests";
  if (name === "migration") return "directory: tests listed below; fixtures read from the submodule";
  if (name === "hosted-git-info") return "directory: tests listed below";
  if (name.endsWith(".tgz")) return "fixture: served by `tests/registry` (loose tarballs) and the dummy registry";
  if (helpers.includes(name)) return `helper: ported as \`conformance/install/${name}\``;
  if (name === "bun-install-windows-rename-retry-fixture.ts") return "fixture: bun:ffi Windows helper, read from the submodule";
  if (name === "generate-scanner-tarballs.ts") return "fixture generator for the security-scanner tarballs (not needed: tarballs are checked in)";
  if (name === "wyhash11.ts" || name === "wyhash-std-collision.ts" || name === "semver-fixture.js") return "fixture: imported from the submodule";
  if (name === "bun.lockb.bin-linking") return "fixture: binary lockfile (bun.lockb), read from the submodule";
  return isDir ? "directory" : "fixture";
}

function writeIndex(all: string[], ported: Map<string, number>) {
  const rows: string[] = [];
  const total = [...ported.values()].reduce((a, b) => a + b, 0);
  for (const file of all) {
    let status: string;
    if (ported.has(file)) status = `ported (\`install/${file}\`, ${ported.get(file)} tests)`;
    else if (file in excluded) status = `excluded (${excluded[file]})`;
    else if (file in todo) status = `todo (${todo[file]})`;
    else status = "todo (not ported yet)";
    rows.push(`| \`${file}\` | ${status} |`);
  }
  const others: string[] = [];
  for (const entry of fs.readdirSync(sourceRoot, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
    if (/\.test\.(ts|js)$/.test(entry.name)) continue;
    others.push(`| \`${entry.name}${entry.isDirectory() ? "/" : ""}\` | ${describeOther(entry.name, entry.isDirectory())} |`);
  }
  for (const sub of ["migration", "hosted-git-info"]) {
    for (const entry of fs.readdirSync(join(sourceRoot, sub), { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      if (/\.test\.(ts|js)$/.test(entry.name)) continue;
      const name = `${sub}/${entry.name}`;
      const what = helpers.includes(name)
        ? `helper: ported as \`conformance/install/${name}\``
        : entry.name === "__snapshots__"
          ? "snapshots: copied next to each ported test"
          : "fixture: read from the submodule";
      others.push(`| \`${name}${entry.isDirectory() ? "/" : ""}\` | ${what} |`);
    }
  }
  const counts = {
    ported: all.filter(f => ported.has(f)).length,
    excluded: all.filter(f => f in excluded).length,
    todo: all.filter(f => !ported.has(f) && !(f in excluded)).length,
  };
  const text = `# Conformance index

Every file of \`submodule/bun/test/cli/install\` and what happened to it. Generated by
\`node tests/conformance/tools/port-bun-tests.ts\` (rerun it after the submodule moves).

Ported tests run with \`npm run test:conformance -w tests\`. They define fnpm's target behaviour and are expected to
fail until the fnpm CLI implements it. The CLI under test is \`FNPM_EXE\`, or \`packages/fuck-npm/dist/fnpm.js\`.

Summary: ${counts.ported} test files ported (${total} test call sites; \`.each\` rows count once), ${counts.todo} todo,
${counts.excluded} excluded.

## How the port works

- \`bun:test\` is vitest through \`compat/bun-test.ts\` (adds \`test.if\`, \`todoIf\`, \`failing\`, \`serial\`,
  \`setDefaultTimeout\`). \`test.concurrent\` runs sequentially unless \`FNPM_CONFORMANCE_CONCURRENT=1\`.
- \`bun\` / the \`Bun\` global is \`compat/bun.ts\` (spawn, file, write, \`$\`, serve, listen, Archive, TOML, JSONC,
  Glob, semver, hashing) on node:*.
- \`harness\` is \`harness.ts\`: \`fnpmExe()\`, \`fnpmEnv\` (FNPM_STORE_DIR / FNPM_CACHE_DIR in a temp dir per worker,
  a worker-local user npmrc supplying the shared fixture registry), \`tempDirWithFiles\`, \`tempDir\`, \`runFnpm\`, \`readLockfile\`, ...
- \`registry\` (\`TestRegistry\`) and \`./dummy.registry\` run on \`tests/registry\` (node:http).
- Text: \`bun.lock\` -> \`fnpm.lock\`, \`bun install\` -> \`fnpm install\` (and the other pm commands),
  \`BUN_INSTALL_CACHE_DIR\` -> \`FNPM_CACHE_DIR\`, \`BUN_CONFIG_*\` -> \`FNPM_CONFIG_*\`, \`node_modules/.bun\` ->
  \`node_modules/.fnpm\`. \`bunfig.toml\` is kept: fnpm reads bun's config keys.

## Tests

| bun test | status |
| --- | --- |
${rows.join("\n")}

## Other files

| bun file | role |
| --- | --- |
${others.join("\n")}
`;
  fs.writeFileSync(join(conformanceDir, "INDEX.md"), text);
}

main();
