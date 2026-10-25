// Usage: node tests/smoke.cjs (from packages/fuck-npm-binding). Loads whatever build
// index.js finds and makes real calls: a USAGE rejection, an empty-project install with
// events, then the no-op fast path. CI runs it on every release artifact on its own
// platform (musl ones in Alpine); it needs no dev dependencies and no network.
"use strict";
const assert = require("node:assert/strict");
const { mkdtempSync, rmSync, writeFileSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { join } = require("node:path");

const binding = require("..");

async function main() {
  console.log(`loaded ${binding.__platformArchAbi()} core ${binding.version()}`);
  const usage = await binding.install({ cwd: tmpdir(), packages: 1 }).then(
    () => null,
    (error) => error,
  );
  assert.equal(usage?.code, "USAGE", "a malformed payload rejects with USAGE");

  const dir = mkdtempSync(join(tmpdir(), "fnpm-smoke-"));
  try {
    process.env.FNPM_HOME = join(dir, ".fnpm");
    writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "smoke", version: "1.0.0" }));
    let batches = 0;
    const first = await binding.install({ cwd: dir, offline: true }, () => {
      batches += 1;
    });
    assert.equal(first.failedCount, 0);
    assert.ok(batches > 0, "events arrive before the summary");
    const second = await binding.install({ cwd: dir, offline: true });
    assert.equal(second.noop, true, "the second install is a no-op");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  console.log("smoke ok");
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
