---
title: fnpx, fnpmx and package execution
description: Run a package executable from your project or fnpm's execution cache.
---

`fnpx` and `fnpmx` are aliases for `fnpm x`. The `dlx` spelling is also accepted. These entry points ship in the **fnpm** npm package; there is no separate fnpx package to install.

```sh
fnpx --help
fnpmx --help
fnpm x --help
```

## Run a project tool

When you expect an installed local tool, require it without triggering a download:

```sh
fnpx --no-install tsc --noEmit
```

Without `--no-install`, fnpx can resolve and install a package into its execution cache. This example adapts Bun's bunx guide to fnpm:

```sh
fnpx cowsay 'Hello from fnpm'
fnpmx cowsay 'Hello from fnpm'
fnpm x cowsay 'Hello from fnpm'
```

A package spec can pin a version. Package installation and executable resolution use the configured registry and policy; a cached executable is still third-party code.

## Package name differs from binary name

```sh
fnpx --package typescript tsc --version
```

Place execution flags before the package or binary name. Arguments after it are passed to the program.

## Scripts, runtime and scaffolding

`--ignore-scripts` skips lifecycle scripts for a newly fetched executable package. `--trust` permits that package's scripts; review them before using it. Tools that require generated or native artifacts may not work after a script-free install.

```sh
fnpm create --help
fnpm run --help
fnpm exec --help
```

`create` resolves a create-template package. `run` executes an existing project script, while `exec` runs a shell command with local binaries on PATH. fnpm manages and launches these tools; keep the runtime required by the tool installed.
