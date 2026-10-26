---
title: fnpm install
description: Install dependencies, choose a layout and keep CI installs frozen.
---

## Basic usage

Adapted from Bun's package-manager documentation, with fnpm's command names and support boundary.

```sh
fnpm install
fnpm install --ignore-scripts
fnpm install --frozen-lockfile
```

`fnpm install` resolves the project's declared dependencies, reuses the package store, links node_modules and writes `fnpm.lock` when saving is enabled. Without an fnpm lockfile it can migrate an existing supported lockfile. See [lockfiles](../../lockfile/) before switching package managers.

Dependency lifecycle scripts use fnpm's [trust policy](../../lifecycle/). An install with `--ignore-scripts` skips script execution; it may leave native modules or generated artifacts unprepared.

## Lockfile and CI modes

```sh
fnpm install --lockfile-only
fnpm ci
```

`--lockfile-only` updates the lockfile without a node_modules install. `fnpm ci` requests frozen lockfile behavior. Generate and validate the fnpm lock before using frozen mode in CI.

## Hoisted and isolated layouts

```sh
fnpm install --linker hoisted
fnpm install --linker isolated
```

The default is hoisted for a single package and isolated for workspaces. An isolated install uses `node_modules/.fnpm`; it does not reproduce pnpm's on-disk store format. Changing layout changes dependency visibility, so rerun project checks after switching.

The [store page](../../store/) explains physical sharing and the optional global virtual store.

## Select workspaces and inspect output

```sh
fnpm install --filter '@acme/*'
fnpm install --verbose
fnpm install --json
```

The CLI supports structured JSON on supported command results and plain output when piped. Use `fnpm install --help` for the complete current flag surface. Parsing a compatible npm/pnpm/Yarn flag spelling does not establish full behavioral equivalence with that package manager.
