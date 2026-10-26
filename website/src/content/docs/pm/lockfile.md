---
title: Lockfiles and fnpm-lock
description: Migrate, verify, export and inspect fnpm.lock.
---

`fnpm install` creates **fnpm.lock** in Bun's text lockfile format. Commit it after reviewing the resolved graph. fnpm can read `bun.lockb` for migration, but never writes the binary format.

## Migrate an existing lock

```sh
fnpm-lock migrate --dry-run
fnpm-lock migrate --from pnpm
fnpm-lock verify
```

Supported input formats are npm (`package-lock.json`), Yarn classic, Yarn Berry, pnpm, Bun text and Bun binary. The original file is preserved. Format support does not mean every upstream protocol and migration edge case has complete parity.

`--from` accepts `npm`, `yarn`, `berry`, `pnpm` or `bun`. Without it, migration detects the source. `--force` permits overwriting an existing `fnpm.lock`; inspect the dry run before using it. Verify package versions, peers and workspace links, then run project checks. See the [migration workflow](../../quickstart/).

## Inspect and verify

```sh
fnpm-lock print
fnpm-lock verify
fnpm-lock verify --integrity
fnpm-lock diff HEAD~1
fnpm-lock why typescript
fnpm-lock dedupe --check
```

`verify` checks lockfile integrity fields and agreement with package.json. `--integrity` also rehashes stored packages. This checks package content against recorded hashes; it does not establish package trust or absence of vulnerabilities.

## Export

```sh
fnpm-lock export --to npm --dry-run
fnpm-lock export --to yarn --output yarn.lock
```

Targets are `npm`, `yarn`, `berry`, `pnpm` and `bun`. The dry run prints the output instead of writing it. Treat an export as an interchange step: review warnings and validate it using the destination package manager. Each tool represents peers, protocols and layouts differently.

## Frozen installs

```sh
fnpm install --frozen-lockfile
```

A frozen install rejects required lockfile changes. For lockfile generation without linking packages, use `fnpm install --lockfile-only`.
