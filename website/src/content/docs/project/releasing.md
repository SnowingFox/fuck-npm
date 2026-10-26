---
title: Releases, packing and publishing
description: Current fnpm release state and project package publication commands.
---

## fnpm distribution status

**As of October 26, 2026: `fnpm@0.0.2` is a release candidate; public npm publication and fresh-registry verification are pending.** The previous `@tiktok-fe/fnpm@0.0.1` release used separate platform packages. It remains a different published layout.

0.0.2's intended public package is `fnpm`. It includes the `fnpm`, `fnpm-lock`, `fnpx` and `fnpmx` entry points. Postinstall fetches one matching native archive from GitHub Releases and verifies its embedded SHA-512 digest. If scripts are disabled, the first command retries the download.

Eight native target archives cover macOS x64/ARM64, Linux x64/ARM64 GNU and musl, and Windows x64/ARM64. The [candidate CI run](https://github.com/SnowingFox/fuck-npm/actions/runs/40294225393) passed all 21 jobs, including eight archive smoke tests and script-disabled first-command coverage. CI success is separate from npm publication and Bun conformance.

The [v0.0.2 GitHub prerelease](https://github.com/SnowingFox/fuck-npm/releases/tag/v0.0.2) contains the candidate archives, SHA512SUMS and provenance. This is separate from the pending public npm publication.

Use the repository's [RELEASING.md](https://github.com/SnowingFox/fuck-npm/blob/main/docs/RELEASING.md) as the current checklist. Release artifacts are listed on [GitHub Releases](https://github.com/SnowingFox/fuck-npm/releases). This documentation does not mark a candidate as published based on a version number or passing CI.

## Pack and publish projects

fnpm also provides package publishing commands for your own projects. Inspect the package contents and rewritten workspace references before sending anything to a registry:

```sh
fnpm pack --dry-run
fnpm publish --dry-run
fnpm publish --help
```

A dry run does not publish. Check the destination registry, authentication, access level, file inclusion rules and lifecycle behavior before a real publication. The registry configuration can differ between installation and `publishConfig`.

Publishing implementation and tested behaviors are recorded in the [parity inventory](https://github.com/SnowingFox/fuck-npm/blob/main/docs/PARITY.md). Full npm/Bun publishing equivalence remains an open compatibility target.
