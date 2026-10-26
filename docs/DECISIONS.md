# Decisions

Short records of the choices that shape fnpm. Each says what was decided, why, and what it
costs. A decision that is replaced stays here, marked as superseded. Details live in
[ARCHITECTURE.md](ARCHITECTURE.md).

## 1. TypeScript CLI on a Rust core via napi

2026-10-25. Partly superseded by [2](#2-native-launcher-for-the-hot-commands).

- **Decision:** the CLI (argv parsing, help, prompts, output) is TypeScript in
  `packages/fuck-npm` (cac, @clack/prompts, picocolors, tsup, vitest). All real work
  (resolution, network, store, linking, scripts) is Rust in `crates/*`, loaded as a napi-rs addon
  (`fnpm.node`). Heavy work runs on Rust threads; progress comes back as batched events.
- **Why:** fast iteration on a large CLI surface (414 parity rows, many of them messages and
  flags), easy testing with injected fakes, and npm-native distribution through napi-rs prebuilt
  platform packages.
- **Cost:** every run pays Node's startup. That was expected to matter only for the no-op
  install; decision 2 shows it matters for every fast scenario.

## 2. Native launcher for the hot commands

2026-10-25.

- **Numbers** (Linux baseline, `bench/results/2026-10-25-linux-x64.md`): bare `node -e 0` takes
  about 47 ms. bun on the small fixture: no-op 7.2 ms, `add` 9.5 ms, warm 23.3 ms, `ci` 23.5 ms;
  on medium: no-op 12.5 ms, `add` 15.4 ms. Under Node, fnpm loses all of these before doing any
  work, and the project's goal is to be faster than bun in every scenario.
- **Decision:** `fnpm` and `fnpm-lock` are Rust executables (`crates/fnpm-cli`). `install`, `i`,
  `ci`, `add`, `remove` and `update` run natively, in process, with no Node. The integrated
  execution track also routes `x` and `create` natively. Other commands,
  `--help` and interactive commands exec `node dist/fnpm.js` with the same argv. The command and
  flag registry stays single-sourced in TypeScript; a build step emits `cli-spec.json`, which the
  Rust parser compiles in. Platform packages ship the native binary and `fnpm.node`; postinstall
  swaps the native binary in behind `.bin`, as bun does, with a JS shim fallback when scripts
  are disabled.
- **Alternatives rejected:** keep Node and optimise startup (compile cache, snapshot): still
  well above 7.2 ms. Rewrite the whole CLI in Rust: duplicates hundreds of help texts, prompts
  and long-tail commands for no benchmark gain.
- **Cost:** the native renderer duplicates the TS progress and summary formatting (kept in sync
  by shared golden fixtures); the core is linked twice per platform package (binary and
  `.node`); the Windows `.bin` setup needs care (see ARCHITECTURE "Packaging and distribution").

## 3. fnpm.lock uses bun.lock's schema

2026-10-25.

- **Decision:** `fnpm.lock` is bun.lock's JSONC text format (same keys, tuples, ordering), with
  one additive field (`deprecated`) that bun ignores.
- **Why:** bun's lockfile tests apply directly, `migrate --from bun` and `export --to bun` are
  near-identity, and the format already holds everything an install needs (integrity, tarball
  location, bin, os/cpu/libc, install-script flag), so a frozen install makes zero manifest
  requests. It is text, sorted and one line per package, so diffs review well.
- **Cost:** fnpm inherits bun's format decisions (hoisted-path keys); anything fnpm-specific has
  to stay additive. A binary sidecar (`<cache>/projects/<project-path-hash>/lock.bin`) covers fast reloads.

## 4. Global virtual store is opt-in

2026-10-26 (revises the 2026-10-25 default).

- **Decision:** isolated installs keep package entries in the project by default, like bun.
  `FNPM_GLOBAL_STORE=1` / `globalStore = true` enables shared entries in
  `~/.fnpm/store/v2/links/`.
- **Why:** Node resolves symlinks to their real location. A shared entry cannot reach the
  project's hidden hoist, breaking packages that import undeclared dependencies. The trpc
  corpus reproduces this when `@nuxt/schema` imports `scule` during postinstall.
- **Tradeoff:** opting in makes warm isolated installs one symlink per package instead of a
  clone or hardlink per file, but requires packages that resolve their dependencies without
  the project hidden hoist. Packages that write into a shared entry affect every project
  sharing it; entries are keyed by package and dependency closure.

## 5. Per-file content-addressed store

2026-10-25.

- **Decision:** the store keeps file blobs addressed by BLAKE3 with a 64-byte output
  (`store/v1/files/<2 hex>/<128 hex>[-x]`); package directories in `store/v1/pkgs/` are hardlinks to
  those blobs plus a precomputed file index (`index.bin`). Projects get files by clonefile,
  FICLONE or hardlink, copy only as a last resort.
- **Why:** bun's cache is per package, so two versions of a package store every file twice; a
  per-file store shares unchanged files across versions (pnpm's insight), which is how fnpm
  targets less disk than both. The file index means linking needs no `readdir`. Because package
  directories still exist, macOS can clone a whole package directory in one call.
- **Why BLAKE3:** every file of every imported package is hashed, which is the one cost fnpm
  has over an extractor that only writes files. BLAKE3 runs at several GB/s per core (SHA-512
  is under 1 GB/s) and is cryptographically strong, so a crafted file cannot take another
  file's blob. The registry's SHA-512/SHA-1 tarball integrity is still verified (with ring).
- **Cost:** extraction hashes every file and writes two levels (blob, then package directory);
  `fnpm store prune` needs reference counting across projects (`projects.json`).
- **Superseded by 9** (2026-10-26): the blob directory is gone.

## 6. HTTP/2 to the registry

2026-10-25.

- **Decision:** the registry client uses HTTP/2 (reqwest with rustls), multiplexing all manifest
  and tarball requests over one connection per host, with manifests prioritised over tarballs.
  HTTP/1.1 remains the fallback for registries that don't offer h2.
- **Why:** `bun install` runs up to 64 concurrent HTTP/1.1 requests
  (`DEFAULT_MAX_SIMULTANEOUS_REQUESTS_FOR_BUN_INSTALL` in `install/PackageManager.rs`), each
  needing its own connection with its own TCP and TLS handshake; bun's HTTP client has h2 code,
  but the installer doesn't use it. One h2 connection with hundreds of streams removes that setup
  cost on cold installs. The `feat/registry` branch offers ALPN `h2` then `http/1.1`.
- **Cost:** a single connection is exposed to head-of-line blocking at the TCP level on lossy
  networks; concurrency limits (`networkConcurrency`, `FNPM_CONFIG_MAX_HTTP_REQUESTS`) still apply
  per stream.

## 7. FNPM_* environment with BUN_* and npm_config_* fallbacks

2026-10-25.

- **Decision:** fnpm's own variables are `FNPM_*` (`FNPM_HOME`, `FNPM_STORE_DIR`,
  `FNPM_CACHE_DIR`, `FNPM_REGISTRY`, `FNPM_CONFIG_<KEY>`, ...). The `BUN_*` and
  `npm_config_*` / `NPM_CONFIG_*` spellings of the same settings are honoured as fallbacks; the
  `FNPM_*` name wins when several are set. The full table is in ARCHITECTURE "Environment
  variables".
- **Why:** existing CI setups and bun's tests keep working unchanged, and fnpm still has an
  unambiguous namespace of its own.
- **Cost:** a larger precedence table to document and test (`config-precedence.test.ts`).

## 8. Benchmark against a local registry mirror

2026-10-25.

- **Decision:** every tool is timed with hyperfine against the same pre-warmed local verdaccio
  mirror (`bench/mirror.ts`), with isolated per-tool state and the registry forced by flag and
  env. Disk is measured as physical bytes (statfs delta and an `lstat` walk that counts
  hardlinks once).
- **Why:** numbers against the public registry measure the network more than the package
  manager and vary run to run. A local mirror makes runs comparable and lets the gate
  (`bench/gate.ts`) fail on small regressions.
- **Cost:** verdaccio is itself slow at serving large packuments (0.2 to 2 s each), so `cold`
  scenarios are dominated by it and understate the differences between tools. A static,
  pre-serialised registry (`BENCH_REGISTRY`) is the planned fix for cold numbers. The page cache
  isn't dropped between runs, so "cold" means a cold package-manager cache, not a cold disk.

## 9. Store without a global blob directory (layout v2)

2026-10-26. Supersedes 5.

- **Decision:** `store/v2/pkgs/<dir>/package/` holds each package's files directly. An import
  writes every file once, into the staging package, and publishes it with one `rename`;
  nothing outside the package is touched per file. Identical files inside one package (same
  mode, size, xxh3-128 and bytes, compared byte for byte) share an inode. Files of different
  packages are never shared. `index.bin` (format 2) lists dirs, files, sizes and modes; it has
  no per-file digests. The layout version moved to `v2` (`fnpm_config::STORE_LAYOUT`), so a v1
  store is left alone and a new one is built next to it.
- **Why:** measured on the bench fixtures (`bench/results`, `strace -f -c`): the v1 import did
  three `linkat` per file (probe the blob, write the file, link it into `files/`), 52,791
  calls with 16,718 failures on medium, plus 2,611 `ENOENT` retries for missing shard dirs, and
  every file was hashed with BLAKE3. Cross-package sharing saved 3.7 MiB of 492 MiB on medium
  (0.75 %); sharing inside a package saved 5.2 MiB, and v2 keeps that. The blob directory
  itself cost 4.2 MiB of directory blocks for 257 shard dirs. bun's cache is per package as
  well; the combined size still depends on directory, index and metadata-cache overhead and
  must be measured for each fixture. Per install the linker still does one `linkat` (or clone)
  per file from `pkgs/`.
- **Cost:** two versions of a package with mostly unchanged files store them twice (pnpm
  shares those). On the fixtures that is under 1 % of the store; a monorepo with many
  versions of one large package would see more. Files above 8 MiB stream directly to disk
  and currently remain separate from other large streamed copies. `store prune` frees a
  package's files with its `pkgs/` entry (files still hard linked into some `node_modules`
  stay there). Recovery of an incomplete entry pins it before checking and deleting children,
  so cleanup cannot recursively delete another importer's completed replacement.
