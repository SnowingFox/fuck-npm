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
| Bun conformance, whole suite | 8,787 passed; 2,273 failed; 157 skipped (79.4%) | 101 files on the current source (`fa9876a`, 2026-10-28); 3 files time out and are not counted |
| Synthetic speed gate | 9 of 22 comparisons slower than Bun | The all-scenario speed requirement is unmet |
| Real repositories | fnpm, Bun, pnpm, Yarn and npm on pinned repositories | Per-repository charts, counts and checks in [the real-project results](../benchmarks/real-projects/) |

### Reading the conformance numbers

The conformance suite is Bun's own package-manager test suite ported to fnpm. A failing test does not mean a package fails to install. The previous whole-suite snapshot passed 55.2%; the fixes since then cover frozen lockfiles, offline installs, `.npmrc` and config precedence, overrides, catalogs, workspaces, lifecycle scripts, the `pm` subcommands and migration.

Files that now pass completely include `bun-audit`, `bun-pm-why`, `bun-pm-pkg`, `bun-pm-version`, `bun-pm-licenses`, `bun-pm-fetch`, `bun-info`, `bun-publish`, `lockfile-only`, `lockfile-version-2` and `config-version`. The remaining failures fall into these groups:

- **Exact output**: lockfile text or stderr wording that differs from Bun's recorded snapshot.
- **Bun internals**: tests that call Bun-only APIs (`bun:internal-for-testing`, lockfile and ini parsers, terminal callbacks). They cannot pass against another binary.
- **Deliberate differences**: fnpm's own cache-pack format, its content store layout, and a relink that skips a deleted-link check on the no-op path for speed.
- **Behavior still open**: git dependencies, tarball integrity edge cases, request retries, isolated-layout details and a deadlock when an install script waits on a package linked after it.

The per-file counts are in [the status report](https://github.com/SnowingFox/fuck-npm/blob/main/tests/conformance/STATUS.md).

The real-repository benchmarks are a different kind of evidence: each tool's installed tree must pass the repository's own build, type check or tests. They found and fixed a real hoisting problem (peer dependencies lost in yarn.lock migration, which broke webpack's option validation and affects Bun 1.4.4 too).

The [roadmap](https://github.com/SnowingFox/fuck-npm/blob/main/docs/ROADMAP.md) records platform CI, integration fixes and remaining work. These figures refer to different checks and source revisions. Do not combine them into one compatibility percentage.

## Adoption boundary

fnpm remains in development. Test your dependency graph, lifecycle scripts, platform-specific packages, registry authentication, workspace behavior and lockfile changes in a disposable checkout. Neither the benchmark results nor native artifact smoke tests establish production readiness.

The documentation covers Node.js package management. Your existing Node.js and project tools execute applications, build code and run tests.
