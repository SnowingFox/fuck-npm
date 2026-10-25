#!/usr/bin/env node
/**
 * Runs the test registry until it is interrupted.
 *
 *   node tests/registry/serve.ts --port 4873
 *   node tests/registry/serve.ts --port 4873 --uplink https://registry.npmjs.org --cache-dir .cache/registry
 *
 * With --uplink and no --packages-dir, only upstream packages are served (a caching mirror for benchmarks).
 * Upstream requests go through HTTPS_PROXY / HTTP_PROXY when set; NO_PROXY is respected.
 */
import { resolve } from "node:path";
import { parseArgs } from "node:util";
import { start } from "./index.ts";

const { values } = parseArgs({
  options: {
    port: { type: "string", default: "4873" },
    host: { type: "string", default: "127.0.0.1" },
    uplink: { type: "string" },
    "cache-dir": { type: "string" },
    "packages-dir": { type: "string" },
    fixtures: { type: "boolean", default: false },
    revalidate: { type: "boolean", default: false },
    "latency-ms": { type: "string" },
    quiet: { type: "boolean", short: "q", default: false },
    help: { type: "boolean", short: "h", default: false },
  },
});

if (values.help) {
  console.log(`Usage: node tests/registry/serve.ts [options]
  --port <n>            port (default 4873, 0 for a free one)
  --host <addr>         address (default 127.0.0.1)
  --uplink <url>        proxy and cache an upstream registry
  --cache-dir <dir>     disk cache for the uplink (default: memory only)
  --packages-dir <dir>  verdaccio-layout packages to serve (default: bun fixtures, or none with --uplink)
  --fixtures            serve the bun fixtures next to the uplink
  --revalidate          revalidate cached packuments with upstream
  --latency-ms <n>      delay every answer
  -q, --quiet           do not log requests`);
  process.exit(0);
}

const uplink = values.uplink;
const packagesDir =
  values["packages-dir"] !== undefined
    ? resolve(values["packages-dir"])
    : uplink !== undefined && !values.fixtures
      ? null
      : undefined;

const registry = await start({
  port: Number(values.port),
  hostname: values.host,
  uplink,
  cacheDir: values["cache-dir"] ? resolve(values["cache-dir"]) : undefined,
  packagesDir,
  tarballDirs: packagesDir === null ? [] : undefined,
  revalidate: values.revalidate,
  latencyMs: values["latency-ms"] ? Number(values["latency-ms"]) : undefined,
  recordRequests: false,
  intercept: values.quiet
    ? undefined
    : request => {
        const url = new URL(request.url);
        process.stderr.write(`${request.method} ${url.pathname}${url.search}\n`);
      },
});

console.log(`fnpm test registry listening on ${registry.url}${uplink ? ` (uplink ${uplink})` : ""}`);

let stopping = false;
const shutdown = async () => {
  if (stopping) return;
  stopping = true;
  const stats = registry.registry.uplink?.stats;
  if (stats && !values.quiet) process.stderr.write(`uplink: ${JSON.stringify(stats)}\n`);
  await registry.stop();
  process.exit(0);
};
process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);
