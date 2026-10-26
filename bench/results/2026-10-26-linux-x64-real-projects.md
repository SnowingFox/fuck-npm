# Real-project package manager measurements

Three measured runs per completed scenario. Times are median seconds; failed scenarios are not speed wins. A passing check covers only the command listed below.

8 CPU cores, 15.4 GiB RAM, Node v24.22.1, linux x64. Immutable git archives; shared development machine; OS and mirror caches remain warm. Timed installs disable lifecycle scripts.

Cold installs use each tool's native lock after untimed preparation, with empty private PM state and no node_modules. Warm installs retain PM state and remove node_modules; no-op retains both. Each tool has private HOME/cache/store. Migration warnings and package-set differences remain in the JSON. npm is covered by the synthetic matrix; these projects use workspace/catalog/Bun formats it cannot install directly.

Installs run to completion without a time cap. Earlier interrupted Excalidraw/Yarn and OpenCode/Bun cold attempts are preserved under priorCappedScenarios and rerun without a cap. Other successful samples are retained. Ranges show the shared-machine variation.

| Project | Tool/version | Cold lock | Warm relink | No-op | Missing deps | Project check |
|---|---|---:|---:|---:|---:|---|
| opennextjs-cloudflare | bun 1.4.4 | 4.9949 | 0.6028 | 0.0798 | 0 | pass |
| opennextjs-cloudflare | pnpm 10.33.0 | 17.4433 | 6.8035 | 2.8804 | 0 | pass |
| opennextjs-cloudflare | fnpm 0.0.1 | 7.2118 | 0.5771 | 0.0089 | 0 | pass |
| trpc | bun 1.4.4 | 5.6106 | 1.4238 | 0.1905 | 0 | pass |
| trpc | pnpm 12.4.1 | 150.1458 | 2.9970 | 0.4394 | 0 | pass |
| trpc | fnpm 0.0.1 | 10.0058 | 0.8811 | 0.0121 | 0 | pass |
| excalidraw | bun 1.4.4 | 2.3536 | 0.3205 | 0.0661 | 0 | pass |
| excalidraw | yarn 1.22.22 | 196.8902 | 18.8943 | 1.0250 | 0 | pass |
| excalidraw | fnpm 0.0.1 | 2.0217 | 0.2277 | 0.0098 | 0 | pass |
| jest | bun 1.4.4 | 2.2663 | 0.3629 | 0.0890 | 0 | pass |
| jest | yarn 4.18.0 | 39.0481 | 24.3643 | 4.4330 | 0 | pass |
| jest | fnpm 0.0.1 | 2.3337 | 0.3237 | 0.0103 | 0 | pass |
| opencode | bun 1.4.4 | 11.3630 | 0.7154 | 0.1249 | 0 | pass |
| opencode | fnpm 0.0.1 | 9.4839 | 0.6254 | 0.0102 | 0 | pass |

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
| opennextjs-cloudflare | bun | 2588.29 | 65.10 | 2653.39 |
| opennextjs-cloudflare | pnpm | 2449.44 | 130.72 | 2580.16 |
| opennextjs-cloudflare | fnpm | 2518.83 | 74.04 | 2592.87 |
| trpc | bun | 2462.46 | 95.99 | 2558.46 |
| trpc | pnpm | 2361.15 | 1180.36 | 3541.51 |
| trpc | fnpm | 2382.83 | 108.08 | 2490.91 |
| excalidraw | bun | 891.61 | 43.21 | 934.82 |
| excalidraw | yarn | 1095.42 | 3199.50 | 4294.92 |
| excalidraw | fnpm | 854.61 | 47.19 | 901.80 |
| jest | bun | 775.04 | 53.66 | 828.70 |
| jest | yarn | 834.14 | 173.50 | 1007.64 |
| jest | fnpm | 748.96 | 57.00 | 805.96 |
| opencode | bun | 3144.22 | 94.64 | 3238.86 |
| opencode | fnpm | 3049.20 | 88.84 | 3138.05 |

## Inputs and verification

| Project | Revision | Workspaces | Verification command |
|---|---|---:|---|
| opennextjs-cloudflare | `8ea9eb973b6d71743e4b37ab2fdf5d98fe4d0ddb` | 23 | `cd packages/cloudflare && PATH="$PWD/node_modules/.bin:$PATH" tsc && PATH="$PWD/node_modules/.bin:$PATH" vitest --run` |
| trpc | `8d09833c76791b2ea7270af174fdab9f1408aeb8` | 43 | `cd packages/server && PATH="$PWD/node_modules/.bin:$PATH" tsdown` |
| excalidraw | `b7de45e88111f5680ae4bb3c3f3541c7d3b279fe` | 10 | `tsc` |
| jest | `61050e9323e742539dc2360236671110e37329e6` | 70 | `node ./scripts/build.mjs` |
| opencode | `7a83c69fe12f021ec5b8ad10a1171e6d01d99289` | 39 | `cd packages/util && bun check --no-script` |

## fnpm binary provenance

| Source revision | SHA-256 |
|---|---|
| `048c97ddbfcf617c7f559ed70b15114ace6f5bfb` | `ad98fe2cbb7a3d922992f7d2c12bd20bc284e6766a8f597f95def11229f8efd2` |

Final harness SHA-256: `d89b9818e28af678e067d59b8713558bf1aa991469bc55df642d08b7fe6ea254`. Final harness adds explicit OS/CPU declaration accounting, portable source/provenance inputs and uncapped execution. Earlier successful samples used GNU timeout wrappers; uncapped reruns launch tools directly. Package-manager install arguments are unchanged.


## Failures and limits

- opennextjs-cloudflare/pnpm: installed version set differs from Bun; inspect JSON before comparing.
- opennextjs-cloudflare/fnpm: installed version set differs from Bun; inspect JSON before comparing.
- trpc/bun: Pristine source/native-lock install with empty private state and scripts disabled; package-version set unchanged. Replaces earlier disk evidence polluted by a verification command.
- trpc/pnpm: installed version set differs from Bun; inspect JSON before comparing.
- excalidraw/yarn: installed version set differs from Bun; inspect JSON before comparing; earlier capped cold attempt interrupted; preserved and rerun without a cap.
- opencode/bun: 23 declarations target another OS/CPU; explicit lock metadata is retained in JSON and these are excluded from the missing count; earlier capped cold attempt interrupted; preserved and rerun without a cap.
- opencode/fnpm: 23 declarations target another OS/CPU; explicit lock metadata is retained in JSON and these are excluded from the missing count.

## Script-enabled OpenNext validation

Node v24.22.1; source `048c97ddbfcf617c7f559ed70b15114ace6f5bfb`. Install: exit 0, 240.16s. Original pnpm lock unchanged: true. Project tsc + Vitest exit: 0. better-sqlite3 SQL smoke exit: 0. This is a Linux check; Node 26 on macOS is not covered.

Default trusted-dependency policy; three untrusted scripts were blocked. better-sqlite3 compiled from source; project root pnpm build ran with automatic dependency verification disabled.

Vitest: 37 test files / 364 tests passed; SQL result `{"value":42}`.

Full Bun parity and the all-scenario speed target remain open. Raw samples, package sets, differences, binary hashes, source revisions and evidence hashes are in the companion JSON.
