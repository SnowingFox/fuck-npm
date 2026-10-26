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

On a terminal, the install into the execution cache shows the same live progress as `fnpm install`: resolution, downloads, extraction, linking, registry traffic and workers. The progress region is erased before the executable starts and leaves one line:

```text
✓ cowsay@1.6.0 installed · 33 packages · 1.2 MiB downloaded · 2.69s
✓ cowsay@1.6.0 (cached)
```

When another fnpx process is installing the same package, fnpx says it is waiting for that install. With output piped or in CI, fnpx prints Bun's status lines instead (`Resolving dependencies`, `Resolved, downloaded and extracted [N]`, `Saved lockfile`); a cached run prints nothing. `--silent` keeps all install output off the terminal. `fnpm create` shows the same progress while it downloads a GitHub template and installs the template's dependencies.

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
