---
title: Scripts and dependency trust
description: Run project scripts and decide which dependency lifecycle scripts can execute.
---

## Run a project script

```sh
fnpm run build
fnpm run test --run
fnpm exec 'node --version'
```

`run` resolves a package.json script or a node_modules/.bin binary. `exec` runs a shell command with local binaries on PATH. Project scripts use your project's runtime and tools. Put fnpm flags before the script name; everything after it is a script argument.

```sh
fnpm run --filter '@acme/*' --if-present build
```

## Dependency lifecycle scripts

fnpm includes a trusted-dependency policy and Bun's default trusted package list. Packages in the default list can run scripts; this is not an empty allowlist. Project lifecycle scripts and explicitly trusted dependency scripts also need review.

This package.json example adapts Bun's lifecycle documentation:

```json title="package.json"
{
  "name": "my-app",
  "trustedDependencies": ["my-reviewed-package"]
}
```

Only add a package after reviewing what its scripts do. Reinstall after changing trust. Inspect the blocked list and trust commands before applying a change:

```sh
fnpm pm untrusted
fnpm pm trust --help
fnpm install
```

## Skip scripts explicitly

```sh
fnpm install --ignore-scripts
```

This is useful for dependency graph inspection, but a successful script-free install may leave native modules unusable. All timed benchmark installs use this flag, so their times exclude script work.

A separate [OpenNext lifecycle check](../../benchmarks/real-projects/#script-enabled-opennext-check) installed with the default policy, built better-sqlite3 from source, ran 364 tests and executed a SQL query. Three untrusted scripts remained blocked. That check covers one Linux project and Node version.
