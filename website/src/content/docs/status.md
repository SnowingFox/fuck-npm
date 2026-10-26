---
title: Support and current limits
description: What fnpm implements, what the evidence covers, and what remains open.
---

fnpm targets Bun's package-management behavior. The goal is broad compatibility with faster installs and less disk use. The target is still open.

## Implemented surfaces

The integrated CLI includes installs and frozen installs; add/remove/update; dependency inspection; lockfile migration and export; workspaces and catalogs; hoisted and isolated layouts; patches and overrides; script trust; package execution; audit; packing and publishing. Read each page's constraints and validate your project before adopting it.

The [414-row parity inventory](https://github.com/SnowingFox/fuck-npm/blob/main/docs/PARITY.md) is more precise than a feature list. **231 rows are checked and 183 remain open** in the recorded snapshot. A checked row means implementation and tests in its owning crate. It does not imply every CLI path or upstream case passes.

## Validation snapshot

| Evidence | Recorded result | Meaning |
|---|---|---|
| Bun conformance, whole suite | 3,667 passed; 2,973 failed; 58 skipped (55.2%) | An older integrated snapshot; not re-run on the current source |
| Bun conformance, lockfile migration files | 170 passed; 426 failed (current source) vs 115 / 481 before today's fixes | No new failures; most remaining ones compare exact output, see below |
| Synthetic speed gate | 9 of 22 comparisons slower than Bun | The all-scenario speed requirement is unmet |
| Real repositories | fnpm, Bun, pnpm, Yarn and npm on pinned repositories | Per-repository charts, counts and checks in [the real-project results](../benchmarks/real-projects/) |

### Reading the conformance numbers

The conformance suite is Bun's own package-manager test suite ported to fnpm. A failing test does not mean a package fails to install. In the current lockfile-migration files, the largest groups of failures are:

- **Snapshot differences (76)**: the generated `fnpm.lock` text differs from Bun's recorded `bun.lock` snapshot (key order, fields Bun writes and fnpm does not, or the reverse).
- **Expected output lines (81)**: tests that expect Bun's exact stderr text in situations where fnpm prints something else or nothing.
- **Behavior**: the rest, including frozen-lockfile and offline cases, which need fixes in fnpm itself.

The breakdown applies to the migration files that were re-run. Frozen-lockfile, offline and `.npmrc` areas report low pass rates in the older whole-suite snapshot and are the next ones to re-run and fix. Until the whole suite is re-run on current source, treat 55.2% as a historical number.

The real-repository benchmarks are a different kind of evidence: each tool's installed tree must pass the repository's own build, type check or tests. They found and fixed a real hoisting problem (peer dependencies lost in yarn.lock migration, which broke webpack's option validation and affects Bun 1.4.4 too).

The [roadmap](https://github.com/SnowingFox/fuck-npm/blob/main/docs/ROADMAP.md) records platform CI, integration fixes and remaining work. These figures refer to different checks and source revisions. Do not combine them into one compatibility percentage.

## Adoption boundary

fnpm remains in development. Test your dependency graph, lifecycle scripts, platform-specific packages, registry authentication, workspace behavior and lockfile changes in a disposable checkout. Neither the benchmark results nor native artifact smoke tests establish production readiness.

The documentation covers Node.js package management. Your existing Node.js and project tools execute applications, build code and run tests.
