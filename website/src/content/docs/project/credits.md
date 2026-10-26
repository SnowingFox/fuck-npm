---
title: Credits and licenses
description: Documentation provenance, original licenses and fnpm branding.
---

fnpm source and original documentation are available at [SnowingFox/fuck-npm](https://github.com/SnowingFox/fuck-npm) under the project's [MIT license](https://github.com/SnowingFox/fuck-npm/blob/main/LICENSE), copyright 2026 SnowingFox.

## Bun documentation sources

This site adapts Bun's documentation navigation and selected package-manager explanations from [oven-sh/bun at commit 449877f](https://github.com/oven-sh/bun/tree/449877f3e088feb6cc2dd2704ffb803d59be3277/docs). The source uses Mintlify's `docs.json` and MDX. This site retains the adapted navigation schema and renders rewritten Markdown/MDX through Astro Starlight so it can build independently.

Adapted source pages include installation, workspaces, lockfiles, scoped registries, lifecycle scripts and package execution. fnpm-specific commands, support limits and measurements replace Bun's promises and examples. Runtime, bundler, test-runner and server pages are outside this site's package-management scope.

Bun declares its project MIT-licensed. Its original [LICENSE.md](../../licenses/bun-LICENSE.md) is preserved unmodified. The [attribution and permission notice](../../licenses/NOTICE.txt) identifies the source commit and adapted files. Copyright in upstream material remains with its authors.

The upstream license also discusses libraries linked into Bun's native binary. Those sections are preserved as source notices and do not indicate those libraries ship in this documentation site.

## Site components and brand

[Astro](https://github.com/withastro/astro) and [Starlight](https://github.com/withastro/starlight) provide the static site and accessible documentation components under their respective MIT licenses. Their package license files remain in the dependency distribution.

The **Fuck NPM / fnpm** wordmarks and f-shaped vector mark were created for this project. The site does not use Bun's mascot or logos. GitHub project links, edits and releases point to SnowingFox/fuck-npm.
