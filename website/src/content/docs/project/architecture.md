---
title: Architecture
description: Native hot paths, a TypeScript CLI and the Rust package-management pipeline.
---

fnpm has a native Rust launcher for install-heavy paths and a TypeScript CLI for help, prompts and the broader command surface. Both reach the Rust core. Commands and flags are defined in TypeScript; a build step emits the cli-spec consumed by the native parser.

## Install path

The core coordinates configuration, workspace discovery, registry access, resolution, fetching, extraction, the v2 store, linking and scripts. Resolution, downloads and linking can overlap. Tarballs stream through decompression, extraction and hashing.

The native launcher handles hot commands without Node startup. Other commands delegate to the bundled TypeScript entry point through Node and the native addon. A no-op project-state cache can avoid redoing work when the tracked inputs and installed state remain valid. Cache validation and complex conformance cases remain part of ongoing correctness work.

## Package layout

| Layer | Repository location |
|---|---|
| CLI, flags, output and prompts | `packages/fuck-npm` |
| Native addon package | `packages/fuck-npm-binding` |
| Rust launcher | `crates/fnpm-cli` |
| Shared package-manager implementation | `crates/fnpm-core` and component crates |
| Behavioral inventory | `docs/PARITY.md` and `docs/inventory` |
| Measurement harnesses | `bench` |

Read the [architecture document](https://github.com/SnowingFox/fuck-npm/blob/main/docs/ARCHITECTURE.md) for command ownership and the [decision log](https://github.com/SnowingFox/fuck-npm/blob/main/docs/DECISIONS.md) for store and launcher tradeoffs.
