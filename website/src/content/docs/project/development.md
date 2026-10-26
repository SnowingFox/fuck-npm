---
title: Develop fnpm and this site
description: Build the CLI from source and run the independent documentation site.
---

## Build fnpm

Use Node.js 20.19+ and Rust 1.90+. From a source checkout:

```sh
npm ci --ignore-scripts
npm run build -w fuck-npm
npm run build:native -w @fuck-npm/binding
node packages/fuck-npm/dist/fnpm.js --help
node packages/fuck-npm/dist/fnpm-lock.js --help
```

These commands build the Node CLI and native addon. The release launcher is a separate Rust executable. The benchmark evidence identifies the exact launcher revision it measured. Source directory names retain historical internal package names; the public distribution is `fnpm`.

Follow the [architecture](https://github.com/SnowingFox/fuck-npm/blob/main/docs/ARCHITECTURE.md), [parity checklist](https://github.com/SnowingFox/fuck-npm/blob/main/docs/PARITY.md) and [roadmap](https://github.com/SnowingFox/fuck-npm/blob/main/docs/ROADMAP.md) when changing behavior. A passing crate test does not close a CLI conformance gap by itself.

## Run the documentation site

The documentation build requires Node.js 22.23.4+ (Node 24.22.1 is also supported). fnpm itself still supports Node 20.19+.

The site has its own package manifest and npm lockfile under `website/`. Install from that directory so the repository's workspace lock remains separate.

```sh
cd website
npm ci
npm run dev
```

Open the local URL printed by Astro. Build and serve the production output with:

```sh
npm run build
npm run preview
```

The build produces static files in `website/dist/` and a Pagefind search index. It needs no hosted documentation service. The adapted Bun `website/docs.json` supplies the navigation; pages live in `website/src/content/docs`. The site uses Starlight components for callouts, code blocks, navigation and search.

No hosted deployment is configured. The default build serves from `/` without a canonical host or sitemap. For a future GitHub Pages project deployment, set the host and base explicitly:

```sh
DOCS_SITE=https://snowingfox.github.io DOCS_BASE=/fuck-npm/ npm run build
```

`DOCS_SITE` supplies the canonical host and enables the sitemap. `DOCS_BASE` prefixes routes, assets and search. Authored internal links use relative paths so they also work under `/fuck-npm/`. Both root and project-subpath builds are checked during this site change. These settings do not deploy or enable GitHub Pages.
