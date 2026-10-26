---
title: Credits and licenses
description: Documentation provenance, original licenses and fnpm branding.
---

fnpm source and original documentation are available at [SnowingFox/fuck-npm](https://github.com/SnowingFox/fuck-npm) under the project's [MIT license](https://github.com/SnowingFox/fuck-npm/blob/main/LICENSE), copyright 2026 SnowingFox.

## Bun documentation sources

This site adapts Bun's documentation navigation and selected package-manager explanations from [oven-sh/bun at commit 449877f](https://github.com/oven-sh/bun/tree/449877f3e088feb6cc2dd2704ffb803d59be3277/docs). The source uses Mintlify's `docs.json` and MDX. This site retains the adapted navigation schema and renders rewritten Markdown/MDX through Astro so it can build independently.

Adapted source pages include installation, workspaces, lockfiles, scoped registries, lifecycle scripts, package execution, and the welcome-page card structure. fnpm-specific commands, support limits and measurements replace Bun's promises and examples. Runtime, bundler, test-runner and server pages are outside this site's package-management scope.

Bun declares its project MIT-licensed. Its original [LICENSE.md](../../licenses/bun-LICENSE.md) is preserved unmodified. The [attribution and permission notice](../../licenses/NOTICE.txt) identifies the source commit and adapted files. Copyright in upstream material remains with its authors.

The upstream license also discusses libraries linked into Bun's native binary. Those sections are preserved as source notices and do not indicate those libraries ship in this documentation site.

## Site components and brand

The page shell and stylesheet also adapt the publicly served [Bun homepage](https://bun.com/) and [docs](https://bun.com/docs), retrieved October 26, 2026. The published stylesheet is retained in `src/styles/bun-site.css`; fnpm-specific overrides, navigation, charts and search integration are local components. Bun scripts and analytics are not included.

[Astro](https://github.com/withastro/astro), [Expressive Code](https://github.com/expressive-code/expressive-code), and [Pagefind](https://github.com/Pagefind/pagefind) provide Markdown rendering, code blocks and static search under their respective MIT licenses.

The locally hosted **Archivo** and **Martian Mono** fonts use the SIL Open Font License 1.1. Their original notices are preserved in [Archivo-OFL.txt](../../licenses/Archivo-OFL.txt) and [MartianMono-OFL.txt](../../licenses/MartianMono-OFL.txt).

The **Fuck NPM / fnpm** wordmarks and f-shaped vector mark were created for this project. The site does not use Bun's mascot or logos. GitHub project links, edits and releases point to SnowingFox/fuck-npm.
