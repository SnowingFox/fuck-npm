---
title: Benchmark evidence
description: Recorded fnpm comparisons with Bun, pnpm, npm and Yarn, including the cases fnpm loses.
---

fnpm's target is to beat Bun in every package-management scenario while using less disk. Current evidence falls short of that target. Two recorded suites answer different questions and must stay separate.

## Measured on five real repositories

Times are **median seconds across three completed runs**; disk is **combined node_modules plus cache/store in MiB**, with shared hardlinks counted once. Each cell is **fnpm / Bun**. Timed installs disable lifecycle scripts. These Linux x64 results measure fnpm 0.0.1 and Bun 1.4.4.

| Repository | Cold lock (s) | Warm relink (s) | No-op (s) | Combined disk (MiB) | Recorded project check |
|---|---:|---:|---:|---:|---|
| [OpenNext Cloudflare](https://github.com/opennextjs/opennextjs-cloudflare) | 7.2118 / 4.9949 | 0.5771 / 0.6028 | 0.0089 / 0.0798 | 2592.87 / 2653.39 | Cloudflare package tsc + Vitest passed |
| [OpenCode](https://github.com/anomalyco/opencode) | 9.4839 / 11.3630 | 0.6254 / 0.7154 | 0.0102 / 0.1249 | 3138.05 / 3238.86 | packages/util `bun check --no-script` passed |
| [tRPC](https://github.com/trpc/trpc) | 10.0058 / 5.6106 | 0.8811 / 1.4238 | 0.0121 / 0.1905 | 2490.91 / 2558.46 | Server package tsdown build passed |
| [Excalidraw](https://github.com/excalidraw/excalidraw) | 2.0217 / 2.3536 | 0.2277 / 0.3205 | 0.0098 / 0.0661 | 901.80 / 934.82 | tsc passed |
| [Jest](https://github.com/jestjs/jest) | 2.3337 / 2.2663 | 0.3237 / 0.3629 | 0.0103 / 0.0890 | 805.96 / 828.70 | `node ./scripts/build.mjs` passed |

OpenNext's fnpm graph contains **14 extra unique versions** relative to Bun, with none missing. The other four fnpm/Bun version sets match; matching sets do not prove identical layouts or full behavior. Timing ranges overlap in several scenarios. A passing project check covers the command listed, not every test in that repository.

A separate **script-enabled OpenNext** check passed the root build, **37 test files / 364 tests**, and a real better-sqlite3 SQL query after **building the addon from source**. Three untrusted scripts stayed blocked and the original pnpm lock was unchanged. That Linux/Node 24 validation took 240.16 seconds and is separate from the script-free timing table.

The [full real-project report](../real-projects/) keeps the pnpm/Yarn baselines, variation, revisions and exact commands visible.

## Choose a report

| Suite | Inputs | Statistic | What it tells you |
|---|---|---|---|
| [Real projects](../real-projects/) | OpenNext Cloudflare, tRPC, Excalidraw, Jest, OpenCode | Median of three successful measured runs | Install behavior on large archived projects, with package-set and direct project checks |
| [Synthetic matrix](../synthetic/) | Small, medium, large, monorepo manifests | Mean of five measured runs plus one warmup | Controlled scenarios across fnpm, Bun, pnpm, npm and Yarn classic; includes the strict speed gate |

Both suites use Linux x64 and a pre-warmed local registry mirror. Timed installs disable lifecycle scripts. Process startup is included. OS and mirror caches remain warm even when a package manager's private cache is empty. These are not public-registry or cross-platform performance claims.

## What improved, and what remains open

In the real-project suite, fnpm beats Bun in **5/5 warm**, **5/5 no-op**, and **2/5 cold-lock** medians. Combined physical disk is lower than Bun on **5/5** projects. OpenNext's fnpm graph includes **14 unique versions absent from Bun's**. Overlapping timing ranges and dependency-set differences limit conclusions.

In the synthetic suite, the strict gate still fails **9/22** comparisons with Bun. Small cold installs, several cold-lockfile scenarios and all four add scenarios remain slower. Monorepo warm is also slower. fnpm's combined disk footprint is lower than Bun and pnpm on all four synthetic fixtures; its extra cache/store alone is larger than Bun's.

Read [the methodology](../methodology/) before interpreting either result. An install finishing or a project check passing covers only the command actually recorded. Full conformance and production readiness are not established.

## What the timing excludes

The measurements do not time lock migration/preparation, project builds, test suites or lifecycle scripts. The native launcher revision is recorded for each suite. These results measure fnpm 0.0.1 source snapshots, not a fresh benchmark of the 0.0.2 release candidate.
