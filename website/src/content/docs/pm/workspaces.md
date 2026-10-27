---
title: Workspaces, filters and catalogs
description: Install and run scripts across a monorepo.
---

A workspace groups independent packages inside one repository. Each package has its own package.json. This structure and the core examples adapt Bun's workspace guide.

## Declare packages

```json title="package.json"
{
  "name": "acme-repo",
  "private": true,
  "workspaces": ["apps/*", "packages/*"]
}
```

Use `workspace:` to refer to another workspace package:

```json title="apps/web/package.json"
{
  "name": "@acme/web",
  "version": "1.0.0",
  "dependencies": { "@acme/shared": "workspace:*" }
}
```

Give `packages/shared/package.json` the name `@acme/shared` and a version. From the repository root, `fnpm install` discovers packages and links the graph. Workspace installs default to the isolated layout. Hoisted linking is available explicitly; validate dependency visibility after changing it.

A `workspace:<path>` dependency (for example `workspace:../tools/lint`) makes that folder a member even when the `workspaces` globs miss it.

## Self-contained workspaces

A member that declares `"installConfig": { "hoistingLimits": "workspaces" }`, or that the root lists by path or name in `workspaces.selfContained`, gets a complete node_modules of its own, as in Bun. Nothing it depends on is hoisted above it, and its packages are real copies (clonefile where available) rather than hardlinks into the store, so the folder can be packaged or copied into a container on its own. The lockfile does not record the setting; every install reads it from the manifests.

```json title="package.json"
{
  "workspaces": {
    "packages": ["apps/*", "packages/*"],
    "selfContained": ["apps/lambda"]
  }
}
```

## Filter installs and scripts

```sh
fnpm install --filter '@acme/*'
fnpm install --filter '@acme/*' --filter '!@acme/legacy'
fnpm run --filter '@acme/*' build
fnpm run --workspaces --if-present test
```

Quote selectors so your shell does not expand them. The selector implementation supports names, directory selectors, exclusions and dependency/dependent forms. Use `fnpm install --help` and validate selection in your repository before broad changes. Script flags go before the script name; arguments after it go to the script.

Workspace script execution includes dependency ordering. Cycles and missing scripts require project-specific validation; explicit `--parallel` and `--sequential` modes are available in `fnpm run --help`.

## Catalogs

fnpm implements default and named catalogs and the `catalog:` dependency protocol. These centralize shared versions and are relevant when migrating pnpm workspaces. `fnpm add <name> --catalog` writes `catalog:` into the selected package.json and adds or reuses the catalog entry, following Bun's (and pnpm's) rules: it refuses workspace packages and relative local paths, seeds a new entry with the range you asked for, and prints a note when a target's existing range follows or keeps its own value. `fnpm update` moves the catalog entries that `catalog:` references read, keeping the reference. Review migrated catalog definitions, generated locks and installed versions together. The [real-project results](../../benchmarks/real-projects/) include catalog-using repositories; they do not establish every catalog edge case.

Publishing rewrites supported workspace references for distribution. Inspect a pack before publishing and check the resulting manifest. See [packing and publishing](../../project/releasing/#pack-and-publish-projects).
