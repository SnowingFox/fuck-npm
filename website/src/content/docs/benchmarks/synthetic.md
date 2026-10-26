---
title: Synthetic install matrix
description: All 22 recorded scenario comparisons across fnpm, Bun, pnpm, npm and Yarn.
---

This run was measured on October 25 and recorded in the October 26 report. The machine is an eight-core Xeon with 15.4 GiB RAM, Linux x64 and ext4, using Node 24.22.1 and hyperfine 1.15.0. All tools use the same pre-warmed local Verdaccio mirror and receive `--ignore-scripts`. npm also disables audit and funding requests.

Tools: **fnpm 0.0.1**, **Bun 1.4.4**, **pnpm 12.15.0**, **npm 11.19.1**, **Yarn 1.22.22**. Each row shows the **mean of five measured runs after one warmup**, with the fastest mean in bold. Do not compare these means directly with the separate real-project medians.

## All recorded scenarios

| fixture | scenario | fnpm | bun | pnpm | npm | yarn |
|---|---|---:|---:|---:|---:|---:|
| small | cold | 696 ms | **554 ms** | 800 ms | 2.95 s | 3.28 s |
| small | cold-lockfile | 141 ms | **123 ms** | 883 ms | 1.61 s | 2.76 s |
| small | warm | **21.1 ms** | 35.3 ms | 212 ms | 1.24 s | 996 ms |
| small | noop | **2.7 ms** | 7.2 ms | 135 ms | 581 ms | 399 ms |
| small | add | 10.8 ms | **9.7 ms** | 221 ms | 662 ms | 824 ms |
| small | ci | **21.1 ms** | 25.0 ms | 271 ms | 1.24 s | 1.07 s |
| medium | cold | **12.6 s** | 12.8 s | 14.4 s | 47.8 s | 47.5 s |
| medium | cold-lockfile | **1.00 s** | 1.16 s | 27.4 s | 11.7 s | 33.6 s |
| medium | warm | **71.6 ms** | 141 ms | 446 ms | 9.97 s | 5.40 s |
| medium | noop | **3.0 ms** | 10.5 ms | 148 ms | 701 ms | 420 ms |
| medium | add | 17.3 ms | **14.3 ms** | 371 ms | 900 ms | 2.02 s |
| medium | ci | **60.0 ms** | 141 ms | 442 ms | 9.85 s | 5.32 s |
| large | cold-lockfile | 1.35 s | **1.10 s** | 42.2 s | 20.3 s | 38.9 s |
| large | warm | **185 ms** | 230 ms | 1.00 s | 15.1 s | 15.4 s |
| large | noop | **2.2 ms** | 62.4 ms | 148 ms | 2.83 s | 770 ms |
| large | add | 76.9 ms | **71.3 ms** | 1.07 s | 3.22 s | 7.14 s |
| large | ci | **172 ms** | 215 ms | 1.01 s | 15.2 s | 15.5 s |
| monorepo | cold-lockfile | 960 ms | **759 ms** | 28.5 s | 12.5 s | 30.5 s |
| monorepo | warm | 176 ms | **157 ms** | 685 ms | 9.65 s | 8.03 s |
| monorepo | noop | **3.2 ms** | 27.2 ms | 143 ms | 1.45 s | 629 ms |
| monorepo | add | 49.1 ms | **30.3 ms** | 4.42 s | 1.49 s | 3.98 s |
| monorepo | ci | **147 ms** | 152 ms | 650 ms | 9.83 s | 7.56 s |


The report contains **22** comparisons with Bun. It has no `cold` row for large or monorepo; those scenarios were not recorded in this matrix. Missing rows are not wins.

`cold` has no lockfile. `cold-lockfile` has a prepared native lock. `warm` deletes node_modules but retains the cache. `noop` retains both. `add` starts from an installed project with warm cache; `ci` uses a frozen lock and warm cache. [Full definitions](../methodology/).

## Strict speed gate

The gate compares means from the same run and fails if fnpm is slower than Bun anywhere. It fails **9 of 22** comparisons:

- Small: cold, cold-lockfile and add.
- Medium: add.
- Large: cold-lockfile and add.
- Monorepo: cold-lockfile, warm and add.

The comparison uses a strict lower-mean target. It is not a statistical significance test. Small differences, such as medium cold, should be interpreted with their sample ranges and standard deviations in the full report.

## Combined physical disk

These values include node_modules plus the extra tool cache/store with shared inodes counted once.

| fixture | fnpm | bun | pnpm | npm | yarn |
|---|---:|---:|---:|---:|---:|
| small | 16.7 MiB | 17.1 MiB | 21.7 MiB | 19.3 MiB | 34.7 MiB |
| medium | 502.7 MiB | 506.6 MiB | 777.7 MiB | 627.2 MiB | 3.7 GiB |
| large | 423.0 MiB | 446.4 MiB | 836.7 MiB | 510.0 MiB | 1.0 GiB |
| monorepo | 407.6 MiB | 425.6 MiB | 553.9 MiB | 493.4 MiB | 2.5 GiB |


fnpm is lower than Bun and pnpm on all four combined totals. fnpm's **extra store/cache alone is larger than Bun's** on each fixture. The underlying report separates node_modules, extra store/cache, unique inodes, directory entries and cold filesystem deltas.

The cold filesystem delta is also lower than Bun and pnpm in this run. It can include unrelated writes on the shared filesystem. Directory-walk measurements do not identify shared reflink extents, so another filesystem may change the relationship between apparent, allocated and shared bytes.

## Provenance and limits

Source revision: `23cc2210361a8f4b5b53221aa48b76a4dff55163`. Native launcher SHA-256: `cd278a4690db221dca51b63a76b9ce929a651bdec9b49b10905b7191f1436aac`.

Synthetic fixtures contain manifests, with each tool's native lock generated outside the timed section. Every tool uses its default linker. Workspace fixtures use `*` for cross-workspace references because npm does not accept `workspace:`. The medium fixture derives from Bun's Next.js/T3 install benchmark.

Cold resolution and pnpm cold-lockfile timings include Verdaccio's metadata serialization overhead. The mirror is the same for all tools, but its latency changes ratios compared with a public registry. Script work, project builds and migration are outside the timing. Linux results do not establish macOS or Windows performance.

The [full report with deviations, ranges and run counts](https://github.com/SnowingFox/fuck-npm/blob/ab814fccf499e563f611f4993efcd8db00968121/bench/results/2026-10-26-linux-x64-land.md) and [harness protocol](https://github.com/SnowingFox/fuck-npm/blob/ab814fccf499e563f611f4993efcd8db00968121/bench/README.md) contain the underlying evidence.
