---
title: Lockfiles and fnpm-lock
description: Migrate, verify, export and inspect fnpm.lock.
---

`fnpm install` creates **fnpm.lock**. Its text is Bun's `bun.lock` format, so the two are interchangeable. Commit it after reviewing the resolved graph.

A project that keeps a binary `bun.lockb` keeps it: fnpm reads it and writes it back in Bun 1.x's format. `fnpm install --save-text-lockfile` converts it to fnpm.lock.

## Migrate an existing lock

When a project has no fnpm.lock but has `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, `pnpm-lock.yaml`, `bun.lock` or `bun.lockb`, `fnpm install` migrates it automatically, keeps the resolved versions and prints a one-line notice on stderr naming the source file. The original file stays in place. To migrate explicitly or preview the result:

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

A frozen install fails only when the lockfile would change. Edits to package.json that leave the resolved graph the same pass. With no lockfile at all, a frozen install (and `fnpm ci`, `--production`) resolves from package.json and writes nothing, as Bun does. In a pruned checkout (`turbo prune`, a Docker layer with some workspaces), a frozen install keeps the lockfile as it is and installs the workspaces that are present. It fails only when a remaining workspace depends on one that was pruned.

For lockfile generation without linking packages, use `fnpm install --lockfile-only`.

Errors in package.json or a lockfile are printed with the file, line and a source frame. When several dependencies fail to resolve, every one is reported before the install exits.
