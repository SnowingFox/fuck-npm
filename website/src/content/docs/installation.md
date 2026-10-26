---
title: Installation
description: Install the fnpm npm package and understand its native binary download.
---

## Release status

As of October 26, 2026, `fnpm@0.0.2` is a candidate for public npm. Publication and fresh-registry verification are pending. The command below is the intended installation path once that publication completes. Check [release status](../project/releasing/) and the [repository release checklist](https://github.com/SnowingFox/fuck-npm/blob/main/docs/RELEASING.md).

```sh
npm install -g fnpm
fnpm --version
fnpm --help
```

The package is named **fnpm**. The unrelated unscoped `fuck-npm` package on npm belongs to another maintainer. The earlier `@tiktok-fe/fnpm@0.0.1` distribution used separate platform packages; it is not the single-package layout described here.

## Requirements and download

Use Node.js **20.19 or newer**. The npm package contains the JavaScript CLI, four bin entries and the installer. Its postinstall downloads the matching native archive from [fnpm GitHub Releases](https://github.com/SnowingFox/fuck-npm/releases), verifies the embedded SHA-512 digest and executable versions, then installs the native launchers and addon. There are no separate platform npm dependencies in the 0.0.2 layout.

Supported release targets are macOS x64/ARM64, Linux x64/ARM64 with GNU or musl, and Windows x64/ARM64. Artifact smoke checks establish loading and a small install on those targets. They do not establish platform performance or complete compatibility.

On Unix, npm, Bun and Yarn classic can replace the bin shim with the native launcher. pnpm, Yarn Berry and Windows keep a Node shim that starts it. The native launcher avoids Node startup for hot package-manager commands.

## Installations with scripts disabled

When postinstall does not run or its download fails, the first invocation of `fnpm`, `fnpm-lock`, `fnpx` or `fnpmx` retries the native asset download. That invocation needs access to GitHub release assets. Subsequent commands reuse the installed files. When your npm version requires lifecycle approval, its supported `--allow-scripts=fnpm` option permits the native-bin swap.

For an offline environment, prepare and verify the native installation before cutting network access. Installing only the npm tarball with scripts disabled does not prepare the native archive.

## Build from source

Use the [development instructions](../project/development/) while publication is pending. After installation, follow the [first-project guide](../quickstart/).
