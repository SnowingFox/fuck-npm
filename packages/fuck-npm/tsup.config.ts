import { defineConfig } from "tsup";

/**
 * CLI bins from one build. `splitting` lets them share chunks (registry
 * helpers, cac, picocolors) and keeps everything reached only through a
 * dynamic import (help, completions, @clack/prompts, command handlers) out of
 * the startup path.
 *
 * `target: node20` because `engines.node` is >=20.19: the entry must still
 * parse on the oldest supported runtime for the version guard to report it.
 *
 * Dependencies are bundled (`noExternal`) so startup does no node_modules
 * resolution. The native binding stays external: it is resolved at runtime,
 * per platform.
 */
export default defineConfig({
  entry: {
    fnpm: "./src/bin/fnpm.ts",
    "fnpm-lock": "./src/bin/fnpm-lock.ts",
    fnpmx: "./src/bin/fnpmx.ts",
    fnpx: "./src/bin/fnpx.ts",
  },
  outDir: "dist",
  target: "node20",
  platform: "node",
  format: "esm",
  banner: { js: "#!/usr/bin/env node" },
  clean: true,
  dts: false,
  minify: true,
  sourcemap: false,
  splitting: true,
  noExternal: ["cac", "picocolors", "@clack/prompts"],
  external: ["@fuck-npm/binding"],
});
