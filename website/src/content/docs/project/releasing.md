---
title: Releases, packing and publishing
description: Current fnpm release state and project package publication commands.
---

## fnpm distribution status

**As of October 26, 2026: `fnpm` is not yet published to public npm.** The first public version will be **0.0.3**. The `0.0.2` candidate below was never published to npm; 0.0.3 replaces it with the same single-package layout plus later fixes (fnpx install progress, migration notices, `node_modules/.bun` / `.pnpm` links for scripts written for those layouts, and peer dependencies restored after yarn.lock migration). The earlier `@tiktok-fe/fnpm@0.0.1` release used separate platform packages and remains a different published layout.

The public package is `fnpm`. It includes the `fnpm`, `fnpm-lock`, `fnpx` and `fnpmx` entry points. Postinstall fetches one matching native archive from GitHub Releases and verifies its embedded SHA-512 digest. If scripts are disabled, the first command retries the download.

Eight native target archives cover macOS x64/ARM64, Linux x64/ARM64 GNU and musl, and Windows x64/ARM64. Each release is staged from a platform CI run on its source commit, which also installs the staged package on all eight targets with scripts enabled and disabled. The 0.0.2 candidate's [CI run](https://github.com/SnowingFox/fuck-npm/actions/runs/40294225393) and [GitHub prerelease](https://github.com/SnowingFox/fuck-npm/releases/tag/v0.0.2) remain as records.

Use the repository's [RELEASING.md](https://github.com/SnowingFox/fuck-npm/blob/main/docs/RELEASING.md) as the current checklist. Release artifacts are listed on [GitHub Releases](https://github.com/SnowingFox/fuck-npm/releases). This documentation marks a version as published only after a fresh install from the npm registry succeeds.

## Pack and publish projects

fnpm also provides package publishing commands for your own projects. Inspect the package contents and rewritten workspace references before sending anything to a registry:

```sh
fnpm pack --dry-run
fnpm publish --dry-run
fnpm publish --help
```

A dry run does not publish. Check the destination registry, authentication, access level, file inclusion rules and lifecycle behavior before a real publication. The registry configuration can differ between installation and `publishConfig`.

Publishing implementation and tested behaviors are recorded in the [parity inventory](https://github.com/SnowingFox/fuck-npm/blob/main/docs/PARITY.md). Full npm/Bun publishing equivalence remains an open compatibility target.
