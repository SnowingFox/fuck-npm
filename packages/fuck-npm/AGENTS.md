# AGENTS.md: `packages/fuck-npm`

The TS CLI for fnpm. Two bins from one tsup build: `fnpm` (package manager)
and `fnpm-lock` (lockfile toolkit). All real work happens in the Rust core,
loaded through `@fuck-npm/binding` (`packages/fuck-npm-binding`).

## Commands

```bash
npm run build          # tsup -> dist/fnpm.js, dist/fnpm-lock.js (+ lazy chunks)
npm test               # vitest; also run with FORCE_COLOR=0
npm run type-check     # tsc --noEmit
npm run bench:startup  # hyperfine over --version / --help
```

Verify the bundle, not only the source: `npm run build && node dist/fnpm.js --version && node dist/fnpm.js --tree`.

## Hard rules

1. **Node built-ins only**, Node >= 20.19. Source imports carry `.ts`
   extensions and use no TS-only runtime syntax (no enums), so
   `node src/bin/fnpm.ts` runs on Node 24 without a build.
2. **No shebang in source.** tsup's banner adds it; a second one is a syntax
   error on line 2. The entry check (`isEntryPoint(import.meta.url, ...)`)
   lives in each bin file and compares realpaths (npm link / global installs).
3. **Never read a flag value from cac.** mri coerces numeric-looking strings.
   Flags are parsed from argv by `cli/parse-flags.ts` against the leaf's
   FlagSpecs. Value flags use `[brackets]`, never `<angles>`.
4. **`--json` is global and every failure honours it**: `{ok: false, error:
   {code, message}}` on stdout. Codes are in `src/errors.ts`.
5. **Startup budget.** The hot path is entry -> app -> registry/plan/parse.
   Help, completions, handlers, `@clack/prompts` and the native binding are
   dynamic imports. Do not add static imports of them. `--version` alone is
   answered in `entry.ts` before the app loads.
6. **Assertions must not depend on colour.** Strip ANSI (`plain()` in the test
   harness) or match on content. picocolors' default treats `FORCE_COLOR=0`
   as on, so colours are always created from `colorEnabled(runtime, ...)`.

## Architecture

| Layer | Rule |
|---|---|
| `src/cli/trees/` | The command surface as data: `flags.ts` (bun's param tables), `fnpm.ts`, `fnpm-lock.ts`. Adding a command is one node. |
| `src/cli/plan.ts` | Resolves argv to a node, translates npm/yarn/pnpm flags (`foreign-flags.ts`), splits passthrough args, rejects unknown flags with owner hints. |
| `src/cli/runtime.ts` | Dispatch order: version, tree, help, unknown command, flags, positionals, handler. Only the matched top-level node is registered with cac. |
| `src/core/` | `types.ts` is the CLI/native contract; `options.ts` validates flags with bun's messages; `load.ts` adapts the napi module; `fake.ts` is the test core. |
| `src/commands/` | Handlers. Anything without a dedicated handler goes to the core via `generic.ts`. |
| `src/output/` | Output modes (json / rich / plain), the 50 ms single-line progress renderer, bun-style install summary. |
| `src/runtime/` | `CliRuntime` is the injectable seam. Only `processRuntime()` reads `process`. |

## Testing

Inject a runtime (`tests/unit/harness.ts`: fake core, captured streams, manual
clock); do not mock modules. The `--tree` snapshot is deliberate: a new command
must update it.
