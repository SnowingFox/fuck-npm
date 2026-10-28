---
title: Manage and inspect dependencies
description: Add, remove, update and explain installed packages.
---

## Change dependencies

```sh
fnpm add react@19
fnpm add --dev typescript
fnpm remove react
fnpm update
fnpm outdated
```

Inspect the manifest and lock diff after changes. `install <package>` is accepted as an add form; `add` makes the intent explicit.

## Explain a dependency graph

```sh
fnpm why typescript
fnpm ls
fnpm info typescript
fnpm-lock why typescript
```

The `fnpm pm` subcommands follow Bun's: `pm pkg`, `pm version`, `pm ls`, `pm why`, `pm view`, `pm licenses`, `pm fetch`, `pm diff`, `pm trust` / `pm untrusted`, `pm cache` and `pm audit` (with `audit fix`).

`fnpm why` explains dependency paths in the project. `fnpm-lock why` reads the lockfile graph. `info` queries package information from the configured registry.

## Overrides, extensions and patches

fnpm includes overrides/resolutions, package extensions, `patch` and `patch-commit`. These affect resolution or installed files and should be reviewed with the lockfile. Use the current command's help before creating a patch; preserve generated patch files in version control with their manifest reference.

```sh
fnpm patch --help
fnpm patch-commit --help
fnpm dedupe --help
fnpm prune --help
```

Compatibility work for complex peer graphs and migration edge cases continues in the [parity inventory](https://github.com/SnowingFox/fuck-npm/blob/main/docs/PARITY.md). Test the actual consumers of changed packages.
