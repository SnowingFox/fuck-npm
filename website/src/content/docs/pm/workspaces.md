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

fnpm implements default and named catalogs and the `catalog:` dependency protocol. These centralize shared versions and are relevant when migrating pnpm workspaces. Review migrated catalog definitions, generated locks and installed versions together. The [real-project results](../../benchmarks/real-projects/) include catalog-using repositories; they do not establish every catalog edge case.

Publishing rewrites supported workspace references for distribution. Inspect a pack before publishing and check the resulting manifest. See [packing and publishing](../../project/releasing/#pack-and-publish-projects).
