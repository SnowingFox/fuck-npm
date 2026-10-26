---
title: First install and migration
description: Try fnpm in an existing Node.js project and validate a migrated lockfile.
---

Start in a project containing `package.json`. Use a branch so you can inspect lockfile and layout changes.

## Existing npm, pnpm, Yarn or Bun project

An install without `fnpm.lock` imports a supported existing lockfile automatically, as Bun does, and says so (`migrated lockfile from bun.lock`). It writes `fnpm.lock` next to the original, which is left unchanged.

In an isolated install, a project with `bun.lock` / `bun.lockb` also gets `node_modules/.bun`, and one with `pnpm-lock.yaml` gets `node_modules/.pnpm`. Both link to `node_modules/.fnpm`, whose entries use the same `@scope+name@version` names, so postinstall scripts that look for packages there keep working.

To make the migration explicit before changing node_modules, use:

```sh
fnpm-lock migrate --dry-run
fnpm-lock migrate
fnpm-lock verify
```

Migration reads npm, pnpm, Yarn classic, Yarn Berry, Bun text and Bun binary lockfiles. fnpm writes the text-format `fnpm.lock`; it does not write `bun.lockb`. The source lockfile remains available for review. If your project contains several package-manager locks, choose a format with `--from` rather than relying on detection.

```sh
fnpm-lock migrate --from pnpm
fnpm install --ignore-scripts
fnpm-lock verify
```

Review migration warnings and the version set. Then inspect dependency [script trust](../pm/lifecycle/), run a script-enabled install where appropriate, and run your project's build and tests. A successful install alone does not prove runtime compatibility. The [real-project report](../benchmarks/real-projects/) shows why comparing installed versions matters.

Commit `fnpm.lock` with the relevant manifest changes after validation. Keep the original lock until your migration is accepted.

## Daily commands

```sh
fnpm add react
fnpm add --dev typescript
fnpm remove react
fnpm run build
fnpx --no-install tsc --noEmit
```

`fnpx` can install a package into its execution cache when needed. `--no-install` requires an available binary. It is useful when you expect a tool from your existing installation.

## Frozen CI installs

After generating and committing `fnpm.lock`, a CI install can reject lockfile changes:

```sh
fnpm ci
# Equivalent frozen mode:
fnpm install --frozen-lockfile
```

Choose script and trust policy explicitly for your project. Do not copy the benchmarks' `--ignore-scripts` setting when your build requires native addons or generated artifacts.
