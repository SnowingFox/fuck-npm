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
| Bun conformance snapshot | 3,667 passed; 2,973 failed; 58 skipped; 55.2% pass rate | Historical integrated result, not full parity |
| Synthetic speed gate | 9 of 22 comparisons slower than Bun | The all-scenario speed requirement is unmet |
| Real-project warm / no-op medians | Faster than Bun on 5/5 projects in each scenario | Linux samples with scripts disabled; version-set caveats apply |
| Real-project cold-lock medians | Faster than Bun on 2/5 projects | No universal cold-install advantage |
| Real-project combined physical disk | Lower than Bun on 5/5 projects | Hardlink-aware node_modules plus cache/store accounting |

The [roadmap](https://github.com/SnowingFox/fuck-npm/blob/main/docs/ROADMAP.md) records platform CI, integration fixes and remaining work. These figures refer to different checks and source revisions. Do not combine them into one compatibility percentage.

## Adoption boundary

fnpm remains in development. Test your dependency graph, lifecycle scripts, platform-specific packages, registry authentication, workspace behavior and lockfile changes in a disposable checkout. Neither the benchmark results nor native artifact smoke tests establish production readiness.

The documentation covers Node.js package management. Your existing Node.js and project tools execute applications, build code and run tests.
