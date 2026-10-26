---
title: Five real projects
description: Cold-lock, warm, no-op and disk measurements on OpenNext, tRPC, Excalidraw, Jest and OpenCode.
---

These results were recorded on October 26, 2026. Each completed scenario has three measured samples. Times below are **median seconds**, including process startup. The shared Linux x64 machine has eight CPU cores and 15.4 GiB RAM, with Node 24.22.1. OS and local mirror caches remain warm.

Timed installs disable lifecycle scripts. Immutable project archives are prepared with each tool's own native lock before measurement. “Cold lock” means an existing native lock with empty private package-manager state and no node_modules. [See all scenario definitions](../methodology/).

## Read the result

fnpm's medians beat Bun on warm and no-op installs for all five projects, and on cold-lock installs for Excalidraw and OpenCode. Bun is faster on the other three cold-lock medians. Combined physical disk is lower than Bun's for all five projects. OpenNext is a package-set comparison with differences, not an equal-graph result.

The table includes the original project's pnpm or Yarn version where measured. There is no npm row: these source projects use workspace/catalog/Bun formats that npm cannot install directly. npm remains in the [synthetic comparison](../synthetic/).

## Timings and variation

| Project | Tool/version | Cold lock | Warm relink | No-op | Missing deps | Project check |
|---|---|---:|---:|---:|---:|---|
| [OpenNext Cloudflare](https://github.com/opennextjs/opennextjs-cloudflare) | bun 1.4.4 | 4.9949 | 0.6028 | 0.0798 | 0 | pass |
| [OpenNext Cloudflare](https://github.com/opennextjs/opennextjs-cloudflare) | pnpm 10.33.0 | 17.4433 | 6.8035 | 2.8804 | 0 | pass |
| [OpenNext Cloudflare](https://github.com/opennextjs/opennextjs-cloudflare) | fnpm 0.0.1 | 7.2118 | 0.5771 | 0.0089 | 0 | pass |
| [tRPC](https://github.com/trpc/trpc) | bun 1.4.4 | 5.6106 | 1.4238 | 0.1905 | 0 | pass |
| [tRPC](https://github.com/trpc/trpc) | pnpm 12.4.1 | 150.1458 | 2.9970 | 0.4394 | 0 | pass |
| [tRPC](https://github.com/trpc/trpc) | fnpm 0.0.1 | 10.0058 | 0.8811 | 0.0121 | 0 | pass |
| [Excalidraw](https://github.com/excalidraw/excalidraw) | bun 1.4.4 | 2.3536 | 0.3205 | 0.0661 | 0 | pass |
| [Excalidraw](https://github.com/excalidraw/excalidraw) | yarn 1.22.22 | 196.8902 | 18.8943 | 1.0250 | 0 | pass |
| [Excalidraw](https://github.com/excalidraw/excalidraw) | fnpm 0.0.1 | 2.0217 | 0.2277 | 0.0098 | 0 | pass |
| [Jest](https://github.com/jestjs/jest) | bun 1.4.4 | 2.2663 | 0.3629 | 0.0890 | 0 | pass |
| [Jest](https://github.com/jestjs/jest) | yarn 4.18.0 | 39.0481 | 24.3643 | 4.4330 | 0 | pass |
| [Jest](https://github.com/jestjs/jest) | fnpm 0.0.1 | 2.3337 | 0.3237 | 0.0103 | 0 | pass |
| [OpenCode](https://github.com/anomalyco/opencode) | bun 1.4.4 | 11.3630 | 0.7154 | 0.1249 | 0 | pass |
| [OpenCode](https://github.com/anomalyco/opencode) | fnpm 0.0.1 | 9.4839 | 0.6254 | 0.0102 | 0 | pass |

## Variation and package sets

Ranges are minimum–maximum seconds across successful samples. Failed scenarios can have fewer than three samples.

| Project/tool | Cold range | Warm range | No-op range | Unique versions | Instances | Bun versions missing / extra |
|---|---:|---:|---:|---:|---:|---:|
| opennextjs-cloudflare/bun | 4.4930–5.4966 | 0.5553–0.7043 | 0.0783–0.0963 | 1637 | 1717 | 0 / 0 |
| opennextjs-cloudflare/pnpm | 16.6022–17.9477 | 6.7008–7.9516 | 2.7118–2.9653 | 1651 | 1730 | 0 / 14 |
| opennextjs-cloudflare/fnpm | 6.5439–9.2931 | 0.5738–1.0458 | 0.0084–0.0093 | 1651 | 1731 | 0 / 14 |
| trpc/bun | 5.5090–5.9695 | 0.7612–1.5946 | 0.1819–0.2121 | 3401 | 3434 | 0 / 0 |
| trpc/pnpm | 145.2391–152.7533 | 2.8383–3.0355 | 0.4390–0.4819 | 3418 | 3487 | 0 / 17 |
| trpc/fnpm | 8.3016–11.8398 | 0.8740–1.4287 | 0.0113–0.0131 | 3401 | 3434 | 0 / 0 |
| excalidraw/bun | 2.1642–2.8855 | 0.2874–0.3396 | 0.0615–0.0673 | 1334 | 1453 | 0 / 0 |
| excalidraw/yarn | 144.2689–276.4668 | 18.8809–19.2327 | 1.0087–1.0258 | 1337 | 1390 | 0 / 3 |
| excalidraw/fnpm | 1.9647–2.0878 | 0.2116–0.2823 | 0.0093–0.0108 | 1334 | 1453 | 0 / 0 |
| jest/bun | 2.2419–2.3014 | 0.3599–0.3716 | 0.0848–0.0904 | 2079 | 2415 | 0 / 0 |
| jest/yarn | 38.0040–39.1674 | 24.0677–27.6749 | 4.4180–4.9762 | 2079 | 2277 | 0 / 0 |
| jest/fnpm | 2.2019–2.3918 | 0.3077–0.3424 | 0.0093–0.0116 | 2079 | 2415 | 0 / 0 |
| opencode/bun | 10.8399–20.1554 | 0.6736–0.7898 | 0.1202–0.1273 | 2150 | 2178 | 0 / 0 |
| opencode/fnpm | 8.7979–11.7698 | 0.5845–1.1048 | 0.0100–0.0103 | 2150 | 2178 | 0 / 0 |

## Physical disk usage

MiB, unique device/inode across node_modules plus cache/store, measured before project builds. Hardlinks count once; shared reflink extents are not identified. Inspect package-set differences before comparing.

| Project | Tool | node_modules | Extra cache/store | Combined |
|---|---|---:|---:|---:|
| [OpenNext Cloudflare](https://github.com/opennextjs/opennextjs-cloudflare) | bun | 2588.29 | 65.10 | 2653.39 |
| [OpenNext Cloudflare](https://github.com/opennextjs/opennextjs-cloudflare) | pnpm | 2449.44 | 130.72 | 2580.16 |
| [OpenNext Cloudflare](https://github.com/opennextjs/opennextjs-cloudflare) | fnpm | 2518.83 | 74.04 | 2592.87 |
| [tRPC](https://github.com/trpc/trpc) | bun | 2462.46 | 95.99 | 2558.46 |
| [tRPC](https://github.com/trpc/trpc) | pnpm | 2361.15 | 1180.36 | 3541.51 |
| [tRPC](https://github.com/trpc/trpc) | fnpm | 2382.83 | 108.08 | 2490.91 |
| [Excalidraw](https://github.com/excalidraw/excalidraw) | bun | 891.61 | 43.21 | 934.82 |
| [Excalidraw](https://github.com/excalidraw/excalidraw) | yarn | 1095.42 | 3199.50 | 4294.92 |
| [Excalidraw](https://github.com/excalidraw/excalidraw) | fnpm | 854.61 | 47.19 | 901.80 |
| [Jest](https://github.com/jestjs/jest) | bun | 775.04 | 53.66 | 828.70 |
| [Jest](https://github.com/jestjs/jest) | yarn | 834.14 | 173.50 | 1007.64 |
| [Jest](https://github.com/jestjs/jest) | fnpm | 748.96 | 57.00 | 805.96 |
| [OpenCode](https://github.com/anomalyco/opencode) | bun | 3144.22 | 94.64 | 3238.86 |
| [OpenCode](https://github.com/anomalyco/opencode) | fnpm | 3049.20 | 88.84 | 3138.05 |


“Missing deps” counts inspected applicable declarations; it is not proof of equivalent graphs. For OpenCode, 23 declarations target another OS/CPU and are explicitly excluded from the missing count, with metadata retained in the raw JSON. Unique versions measure name/version sets; instances include repeated placements. Matching version sets can still have different instance counts or layouts.

## Package sets and project checks

OpenNext fnpm installs 1,651 unique versions and Bun 1,637. The fnpm set has no missing Bun versions and 14 extra versions. fnpm and Bun match name/version sets for tRPC, Excalidraw, Jest and OpenCode. The other package managers' differences are shown above. Inspect the raw package identities and lock migration warnings before attributing a difference to the installer alone.

Every listed tool completed the project check recorded below. “Pass” means that command succeeded, not that the project's full build and entire test suite passed. The OpenCode check uses the project's own Bun command; it does not imply fnpm supplies a runtime.

| Project | Revision | Workspaces | Verification command |
|---|---|---:|---|
| [OpenNext Cloudflare](https://github.com/opennextjs/opennextjs-cloudflare) | `8ea9eb973b6d71743e4b37ab2fdf5d98fe4d0ddb` | 23 | `cd packages/cloudflare && PATH="$PWD/node_modules/.bin:$PATH" tsc && PATH="$PWD/node_modules/.bin:$PATH" vitest --run` |
| [tRPC](https://github.com/trpc/trpc) | `8d09833c76791b2ea7270af174fdab9f1408aeb8` | 43 | `cd packages/server && PATH="$PWD/node_modules/.bin:$PATH" tsdown` |
| [Excalidraw](https://github.com/excalidraw/excalidraw) | `b7de45e88111f5680ae4bb3c3f3541c7d3b279fe` | 10 | `tsc` |
| [Jest](https://github.com/jestjs/jest) | `61050e9323e742539dc2360236671110e37329e6` | 70 | `node ./scripts/build.mjs` |
| [OpenCode](https://github.com/anomalyco/opencode) | `7a83c69fe12f021ec5b8ad10a1171e6d01d99289` | 39 | `cd packages/util && bun check --no-script` |


## Script-enabled OpenNext check

A separate untimed install used the default trusted-dependency policy on Linux with Node 24.22.1. It completed in 240.16 seconds. Three untrusted scripts remained blocked. better-sqlite3 compiled from source, the root build ran with automatic dependency verification disabled, 37 Vitest files / 364 tests passed, and a real SQL query returned `{"value":42}`. The original pnpm lock remained unchanged.

This validates those commands on that project snapshot. It does not cover Node 26 on macOS, all native modules or arbitrary lifecycle scripts. Its duration must not be mixed into the script-free install medians.

## Provenance and limitations

The measured fnpm binary is 0.0.1 from source `048c97ddbfcf617c7f559ed70b15114ace6f5bfb`, SHA-256 `ad98fe2cbb7a3d922992f7d2c12bd20bc284e6766a8f597f95def11229f8efd2`. The final harness hash is `d89b9818e28af678e067d59b8713558bf1aa991469bc55df642d08b7fe6ea254`.

The shared machine causes variation. For example, OpenNext fnpm warm samples span 0.5738–1.0458 seconds, while Bun spans 0.5553–0.7043 seconds. The fnpm median is lower, but the ranges overlap. Three samples do not establish statistical certainty.

Installs run to completion without a time cap. Earlier interrupted Excalidraw/Yarn and OpenCode/Bun cold attempts are retained as prior capped scenarios in the JSON and were rerun without a cap. Successful earlier samples remain in the final data. Failed or interrupted attempts are not speed wins.

Physical disk is measured before project builds. Hardlinks count once; shared reflink extents are not identified. Tool state is private per run. Upstream tarball URLs pinned in original locks can bypass the local mirror.

Read the [complete Markdown report](https://github.com/SnowingFox/fuck-npm/blob/ab814fccf499e563f611f4993efcd8db00968121/bench/results/2026-10-26-linux-x64-real-projects.md) and [raw JSON samples, package sets and evidence](https://github.com/SnowingFox/fuck-npm/blob/ab814fccf499e563f611f4993efcd8db00968121/bench/results/2026-10-26-linux-x64-real-projects.json). The [methodology](../methodology/) explains isolation and disk accounting.
