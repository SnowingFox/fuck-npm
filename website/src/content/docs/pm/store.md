---
title: Store and disk sharing
description: Understand fnpm's v2 package store, link strategies and physical disk measurements.
---

fnpm stores packages under `~/.fnpm/store/v2` by default. The metadata and project-state cache lives under `~/.fnpm/cache`. [Environment overrides](../configuration/#fnpm-environment-variables) let a project or benchmark isolate those directories. When a cache directory is configured explicitly, the store moves inside it.

## Sharing strategy

The v2 store uses one directory per package. Identical files inside a package can share an inode. Cross-version file deduplication was removed to reduce import overhead.

The linker uses copy-on-write clones on supported filesystems, hardlinks where appropriate, and copies as a fallback. APFS clonefile and btrfs/XFS reflink support can change physical sharing. Files on different devices cannot use a hardlink. An isolated layout also has an opt-in global virtual store; it is not required for the default install.

```sh
fnpm store --help
fnpm pm cache --help
fnpm doctor --help
```

Use the command help before pruning shared state. Store reuse can benefit multiple projects, but the recorded benchmark isolates tool state and does not quantify arbitrary multi-project savings.

## Read the disk numbers correctly

Disk tables include node_modules **and** the extra cache/store, with shared hardlinks counted once. Counting only node_modules hides the package-manager cache. Counting apparent file bytes double-counts hardlinks.

In the synthetic run, fnpm's combined physical footprint is lower than Bun's and pnpm's on all four fixtures, but fnpm's extra store/cache is larger than Bun's on all four. In the real-project run, OpenNext's pnpm combined footprint is lower than fnpm's. The target to use less disk everywhere is still open. See [measurement methodology](../../benchmarks/methodology/#physical-disk-accounting).
