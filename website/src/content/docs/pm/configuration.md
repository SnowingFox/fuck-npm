---
title: Configuration and registries
description: Configure fnpm.toml, .npmrc, scopes and environment overrides.
---

fnpm reads `.npmrc`, Bun's `bunfig.toml`, its own `fnpm.toml`, and the `fnpm` section in package.json. `fnpm.toml` uses the same `[install]` shape as Bun's install configuration.

```toml title="fnpm.toml"
[install]
registry = "https://registry.npmjs.org/"
linker = "isolated"
minimumReleaseAge = 86400
```

`minimumReleaseAge` is in seconds. It restricts selection of newly published versions; it does not inspect their code. Existing locks, exclusions and registry timestamp metadata matter. Review resolution output when adopting the policy.

## Precedence

From lowest to highest:

1. User .npmrc
2. Project .npmrc
3. Global and then project bunfig.toml
4. Global and then project fnpm.toml
5. package.json `fnpm` configuration
6. Environment variables
7. CLI flags

Command-specific settings such as publishConfig also apply in publishing. Use `--config` to choose an explicit file where supported.

## Scoped registries

The following .npmrc structure adapts the registry configuration in Bun's docs. Supply the registry and token for your own organization:

```ini title=".npmrc"
registry=https://registry.npmjs.org/
@acme:registry=https://registry.example.com/
//registry.example.com/:_authToken=${NPM_TOKEN}
```

Keep tokens in the environment or your secret manager. Commit the variable reference, never its resolved value. Tokens are scoped to the registry host and path.

fnpm masks credentials in every URL it prints, in warnings, errors and verbose logs, and escapes control characters in names and paths it echoes. A token stays attached when a registry redirects from http to https on the same host and port. Tarballs from registries other than npmjs keep their full URL in the lockfile.

## fnpm environment variables

| Variable | Purpose |
|---|---|
| `FNPM_HOME` | Base directory for fnpm state |
| `FNPM_STORE_DIR` | Store parent; v2 packages live under its `v2` subdirectory |
| `FNPM_CACHE_DIR` | Metadata and project-state cache |
| `FNPM_REGISTRY` | Default registry override |
| `FNPM_INSTALL` | Root for `-g` installs, like Bun's `BUN_INSTALL`: packages in `<root>/install/global`, executables in `<root>/bin` |
| `FNPM_GLOBAL_DIR` / `FNPM_GLOBAL_BIN_DIR` | Override the global package and executable locations |

Without `FNPM_INSTALL`, the global root is `$XDG_CACHE_HOME/.fnpm`, then `~/.fnpm`. A `~/.fnpm/global` left by an older fnpm stays in use until it is moved.

A cache directory set by flag, environment (`FNPM_CACHE_DIR`, `BUN_INSTALL_CACHE_DIR`) or config also holds the package store, as Bun's cache does, so a CI cache of that directory restores the packages. `FNPM_STORE_DIR` still wins. With the cache disabled, downloads go to the project's `node_modules/.cache`.

Some Bun/npm-compatible environment spellings are fallbacks. Prefer fnpm's names for new configuration. Find the complete implementation and precedence in [fnpm-config](https://github.com/SnowingFox/fuck-npm/tree/main/crates/fnpm-config).
