// Measures what the binding itself costs, independent of the core's work:
//   load      require() of the addon (dlopen + module init), in a fresh process
//   version   a synchronous call
//   reject    a payload error: parse in Rust, build the Error, settle the promise
//   roundtrip an async call through the runtime with no events (needs `testing` exports)
//   events    N batches through the ThreadsafeFunction, acknowledged before settling
//
// Usage: node bench/overhead.mjs [path to .node or package dir]
import { execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const target = process.argv[2] ?? join(dirname(fileURLToPath(import.meta.url)), "..");
const require = createRequire(import.meta.url);

function ms(ns) {
  return `${(Number(ns) / 1e6).toFixed(3)} ms`;
}

function us(ns) {
  return `${(Number(ns) / 1e3).toFixed(2)} us`;
}

function median(values) {
  const sorted = [...values].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
  return sorted[Math.floor(sorted.length / 2)];
}

// load: fresh processes, so the dlopen is never cached in-process.
const loadSamples = [];
for (let i = 0; i < 15; i++) {
  const out = execFileSync(process.execPath, [
    "-e",
    `const t = process.hrtime.bigint(); require(${JSON.stringify(target)}); process.stdout.write(String(process.hrtime.bigint() - t));`,
  ]);
  loadSamples.push(BigInt(out.toString()));
}
console.log(`load       median ${ms(median(loadSamples))} (15 processes)`);

const binding = require(target);

{
  const n = 200_000;
  const t = process.hrtime.bigint();
  for (let i = 0; i < n; i++) binding.version();
  console.log(`version    ${us((process.hrtime.bigint() - t) / BigInt(n))} per call`);
}

async function perCall(label, n, call) {
  for (let i = 0; i < 200; i++) await call(); // warm up the runtime
  const samples = [];
  for (let i = 0; i < n; i++) {
    const t = process.hrtime.bigint();
    await call();
    samples.push(process.hrtime.bigint() - t);
  }
  console.log(`${label.padEnd(10)} median ${us(median(samples))}, p99 ${us(samples.sort((a, b) => (a < b ? -1 : 1))[Math.floor(n * 0.99)])}`);
}

await perCall("reject", 5_000, () => binding.install({ packages: 1 }).catch(() => {}));

if (typeof binding.__testDeliver === "function") {
  await perCall("roundtrip", 5_000, () => binding.__testDeliver([]));
  for (const n of [1, 100, 1_000]) {
    const batches = Array.from({ length: n }, (_, i) => [
      { type: "download", done: i, total: n, bytes: i * 1024, name: "react-dom" },
    ]);
    await perCall(`events${n}`, 200, () => binding.__testDeliver(batches, () => {}));
  }
} else {
  console.log("roundtrip/events skipped: build with `--features testing` for the __test exports");
}
