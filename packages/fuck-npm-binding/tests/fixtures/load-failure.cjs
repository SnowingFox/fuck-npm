// Usage: node load-failure.cjs <binding dir>. Loads a copy of index.js from an empty
// directory on a platform with no build, so no candidate exists, and prints the error.
const { copyFileSync, mkdtempSync, rmSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { join } = require("node:path");
const dir = mkdtempSync(join(tmpdir(), "fnpm-loader-"));
copyFileSync(join(process.argv[2], "index.js"), join(dir, "index.js"));
process.env.NAPI_RS_NATIVE_LIBRARY_PATH = join(dir, "does-not-exist.node");
process.env.CARGO_TARGET_DIR = join(dir, "no-target");
Object.defineProperty(process, "platform", { value: "aix" });
try {
  require(join(dir, "index.js"));
  process.stdout.write(JSON.stringify({ code: "LOADED" }));
} catch (error) {
  process.stdout.write(JSON.stringify({ code: error.code, message: error.message }));
} finally {
  rmSync(dir, { recursive: true, force: true });
}
