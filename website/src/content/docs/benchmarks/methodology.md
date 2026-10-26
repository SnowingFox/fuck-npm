---
title: Benchmark methodology
description: Scenario definitions, isolation, mirror behavior, physical disk accounting and reproduction.
---

Measurements are useful when they identify what changed and what was held constant. fnpm records two suites with different statistics and source revisions. The [overview](../overview/) links both; never treat them as one run.

## Cold, warm and no-op

| Scenario | Lockfile | Private PM cache/store | node_modules | What is timed |
|---|---|---|---|---|
| Synthetic `cold` | Absent | Empty | Absent | Resolve, fetch, link and process startup |
| `cold-lockfile` / real-project “cold lock” | Prepared native lock | Empty | Absent | Install with a lock, including fetch/link and startup |
| `warm` / “warm relink” | Present | Retained | Deleted | Relink from warm state and startup |
| `noop` | Present | Retained | Retained | Check an already installed, unchanged project and startup |
| Synthetic `add` | Restored, then installed outside timing | Warm | Fully installed before timing | Add the designated package |
| Synthetic `ci` | Present | Warm | Deleted | Frozen install and startup |

A package-manager-cold run is **not** an OS-cold or registry-cold run. The OS page cache and pre-warmed mirror remain warm. Real-project “cold” uses a native lock, so it corresponds to `cold-lockfile`, not the synthetic no-lock `cold` scenario.

All timed installs disable lifecycle scripts. Setup, lockfile generation, deleting node_modules, project builds and verification are outside the timed section. Startup remains inside it. A native fnpm launcher is used for the recorded results; a fallback through Node would include Node startup.

## Isolation and comparison boundary

Each tool/project pair receives private HOME, cache and store directories. Project and state directories share a filesystem so hardlinks/clones can work. The harness forces the registry setting and isolates user configuration. Corepack's own distribution cache is retained to avoid downloading package-manager executables inside the experiment. CI detection variables are cleared where they change tool behavior.

Each tool uses its default linker and its own prepared native lock. The project inputs come from manifests or immutable git archives. This compares the tools as configured by the harness, not one byte-identical layout. For real projects, the report inspects installed name/version sets, package instances, applicable missing declarations and explicit OS/CPU exclusions. OpenNext's graph difference must remain visible when discussing its times or disk.

The synthetic suite includes npm and Yarn classic directly. Real projects retain their relevant pnpm or Yarn version and omit npm when its supported input formats cannot represent that source graph. A missing tool result is not a failed install or a win for fnpm.

## Registry and sampling

The synthetic suite uses hyperfine with one warmup and five measured runs. It reports means, standard deviations, ranges and run counts. The strict gate compares means without a statistical margin. The real-project runner times processes directly and reports medians and ranges for three completed samples. Both include startup and use a pre-warmed local Verdaccio mirror.

Verdaccio's metadata serialization and single-process throughput are part of cold timings. Equal mirror access does not recreate a public registry's network latency or metadata behavior. Some original project locks pin upstream tarball URLs that bypass the mirror. The machine is shared; scheduling and other work affect sample ranges.

Uncapped real-project installs run to completion. Prior interrupted capped attempts stay in the raw data and are replaced by completed uncapped scenarios. Failed scenarios never count as speed wins. Medians with overlapping ranges should not be described as a universal advantage.

## Physical disk accounting

The combined metric walks all project node_modules directories and the tool's cache/store using `lstat`, without following symlinks. Allocated bytes are `st_blocks * 512`. Files are deduplicated by **(device, inode)** across both walks, so a hardlink between store and node_modules counts once.

The node_modules walk runs first. “Extra cache/store” means allocated bytes not already counted through node_modules. Therefore:

```text
combined physical = node_modules physical + extra cache/store physical
```

This differs from summing apparent file sizes or measuring node_modules alone. It does not detect extents shared by separate reflink inodes. The reported real-project totals are taken before builds; build outputs are not included.

The synthetic suite also reads the change in filesystem used space around a separate cold install after syncing. This can observe copy-on-write sharing, but includes any unrelated writes on that filesystem. It is a noisy supplemental measurement. Directory entries and unique inode counts in the report answer separate filesystem-work questions.

These Linux/ext4 figures do not establish APFS, btrfs, XFS or Windows disk behavior. Private per-tool state also does not model arbitrary cache reuse across many real developer projects.

## Project checks and scripts

The real-project report names an exact build, type-check or test command per archived revision. A pass covers that command only. Zero missing inspected declarations does not prove complete runtime equivalence. OpenNext's script-enabled check is a separate validation with its own install duration, trust policy, test count and native-addon SQL smoke result.

Full Bun parity and the strict all-scenario speed target remain open. Read [support status](../../status/) alongside performance claims.

## Reproduce the synthetic run

Use Node 22.18+ for the benchmark scripts' type stripping, hyperfine 1.13+, the native fnpm build and the other package managers on PATH. From the repository root:

```sh
node bench/mirror.ts prewarm
node bench/run.ts --fixtures small,medium --runs 5
node bench/gate.ts
node bench/mirror.ts stop
```

The gate can exit nonzero when fnpm loses a comparison; that is a benchmark result. Use a fresh output path and inspect the run's tool versions, source revision and isolation settings. The commands illustrate the existing harness, not new measurements made for this site.

For real-project reproduction and all harness flags, use [bench/README.md](https://github.com/SnowingFox/fuck-npm/blob/main/bench/README.md). Recorded results include source revisions, binary/harness hashes, raw samples, package identities and evidence hashes. Do not substitute an unverified binary and label its output as the published run.
