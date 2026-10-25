// Usage: node load-failure.cjs <binding dir> repository|installed. Loads a copy of index.js
// as darwin-x64 with nothing built or installed, so no candidate loads, and prints the error. `repository`
// puts the copy in a plain directory; `installed` puts it in node_modules/@fuck-npm/binding
// next to planted development builds that must not be loaded.
const { copyFileSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { join } = require("node:path");
const root = mkdtempSync(join(tmpdir(), "fnpm-loader-"));
const installed = process.argv[3] === "installed";
const dir = installed ? join(root, "node_modules", "@fuck-npm", "binding") : root;
mkdirSync(dir, { recursive: true });
copyFileSync(join(process.argv[2], "index.js"), join(dir, "index.js"));
if (installed) {
  for (const planted of [
    join(root, "node_modules", "target", "release", "libfnpm_napi.dylib"),
    join(root, "node_modules", "crates", "fnpm-napi", "fnpm.darwin-x64.node"),
  ]) {
    mkdirSync(join(planted, ".."), { recursive: true });
    writeFileSync(planted, "not a library");
  }
}
process.env.NAPI_RS_NATIVE_LIBRARY_PATH = join(root, "does-not-exist.node");
process.env.CARGO_TARGET_DIR = join(root, "node_modules", "target");
Object.defineProperty(process, "platform", { value: "darwin" });
Object.defineProperty(process, "arch", { value: "x64" });
try {
  require(join(dir, "index.js"));
  process.stdout.write(JSON.stringify({ code: "LOADED" }));
} catch (error) {
  process.stdout.write(JSON.stringify({ code: error.code, message: error.message }));
} finally {
  rmSync(root, { recursive: true, force: true });
}
