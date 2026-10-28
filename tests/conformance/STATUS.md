# Conformance status

Pass counts of the ported bun tests (tests/conformance/install) against the native release binary
(`FNPM_EXE=target/release/fnpm`). Regenerate with `node tests/conformance/tools/status.ts [file ...]`;
`FNPM_BUN_DIR` points at a bun checkout when the submodule is not initialised.
Only listed files have been measured. Each row keeps its latest per-file result; a partial run preserves other rows.
Timeouts and load failures are explicit in the note column; files without counts are excluded from the pass rate.

Total: 8787 passed, 2273 failed, 157 skipped (79.4% of the tests that ran).

| file | passed | failed | skipped | pass rate | note |
| --- | ---: | ---: | ---: | ---: | --- |
| `architecture-match.test.ts` | 30 | 0 | 0 | 100.0% |  |
| `bad-workspace.test.ts` | 22 | 25 | 0 | 46.8% |  |
| `bun-add-catalog.test.ts` | 134 | 19 | 0 | 87.6% |  |
| `bun-add-filter.test.ts` | 112 | 33 | 0 | 77.2% |  |
| `bun-add.test.ts` | 175 | 45 | 0 | 79.5% |  |
| `bun-audit.test.ts` | 260 | 0 | 0 | 100.0% |  |
| `bun-create.test.ts` | 138 | 3 | 1 | 97.9% |  |
| `bun-dedupe.test.ts` | 79 | 3 | 0 | 96.3% |  |
| `bun-info.test.ts` | 74 | 0 | 1 | 100.0% |  |
| `bun-install-backends.test.ts` | 9 | 19 | 4 | 32.1% |  |
| `bun-install-cache-publish.test.ts` | 5 | 1 | 0 | 83.3% |  |
| `bun-install-cpu-os.test.ts` | 27 | 16 | 0 | 62.8% |  |
| `bun-install-env-file.test.ts` | 22 | 18 | 0 | 55.0% |  |
| `bun-install-git-deps.test.ts` | 69 | 131 | 1 | 34.5% |  |
| `bun-install-hardlink-fallback.test.ts` | 6 | 4 | 0 | 60.0% |  |
| `bun-install-lifecycle-scripts.test.ts` | 338 | 96 | 2 | 77.9% |  |
| `bun-install-link-path.test.ts` | 29 | 20 | 0 | 59.2% |  |
| `bun-install-native-binlink.test.ts` | 25 | 2 | 0 | 92.6% |  |
| `bun-install-non-ntfs-windows.test.ts` | 0 | 0 | 1 | - |  |
| `bun-install-offline-misses.test.ts` | 28 | 7 | 0 | 80.0% |  |
| `bun-install-offline.test.ts` | 8 | 4 | 0 | 66.7% |  |
| `bun-install-package-extensions.test.ts` | 27 | 16 | 0 | 62.8% |  |
| `bun-install-patch.test.ts` | 26 | 27 | 0 | 49.1% |  |
| `bun-install-pathname-trailing-slash.test.ts` | 4 | 2 | 0 | 66.7% |  |
| `bun-install-proxy.test.ts` | 13 | 1 | 0 | 92.9% |  |
| `bun-install-registry.test.ts` | 334 | 156 | 13 | 68.2% |  |
| `bun-install-retry.test.ts` | 103 | 105 | 0 | 49.5% |  |
| `bun-install-security-provider.test.ts` | 92 | 6 | 0 | 93.9% |  |
| `bun-install-staging.test.ts` | 25 | 26 | 0 | 49.0% |  |
| `bun-install-stalled-tls.test.ts` | 3 | 2 | 0 | 60.0% |  |
| `bun-install-streaming-extract.test.ts` | 0 | 0 | 0 | - | timed out after 900s |
| `bun-install-tarball-integrity.test.ts` | 67 | 98 | 42 | 40.6% |  |
| `bun-install-tmpdir-exdev.test.ts` | 0 | 1 | 0 | 0.0% |  |
| `bun-install-windows-rename-retry.test.ts` | 0 | 0 | 8 | - |  |
| `bun-install.test.ts` | 604 | 168 | 6 | 78.2% |  |
| `bun-link.test.ts` | 39 | 20 | 2 | 66.1% |  |
| `bun-lock.test.ts` | 295 | 22 | 1 | 93.1% |  |
| `bun-pack.test.ts` | 373 | 8 | 1 | 97.9% |  |
| `bun-patch.test.ts` | 151 | 81 | 5 | 65.1% |  |
| `bun-pm-cache-pack.test.ts` | 6 | 200 | 0 | 2.9% |  |
| `bun-pm-diff.test.ts` | 58 | 18 | 0 | 76.3% |  |
| `bun-pm-fetch.test.ts` | 34 | 0 | 0 | 100.0% |  |
| `bun-pm-licenses.test.ts` | 85 | 0 | 0 | 100.0% |  |
| `bun-pm-pkg.test.ts` | 134 | 0 | 3 | 100.0% |  |
| `bun-pm-scan.test.ts` | 20 | 0 | 0 | 100.0% |  |
| `bun-pm-version.test.ts` | 114 | 0 | 0 | 100.0% |  |
| `bun-pm-why.test.ts` | 116 | 0 | 0 | 100.0% |  |
| `bun-pm.test.ts` | 136 | 18 | 0 | 88.3% |  |
| `bun-prune.test.ts` | 158 | 19 | 0 | 89.3% |  |
| `bun-publish.test.ts` | 279 | 0 | 1 | 100.0% |  |
| `bun-remove.test.ts` | 31 | 6 | 0 | 83.8% |  |
| `bun-security-scanner-matrix-with-node-modules.test.ts` | 0 | 0 | 0 | - | timed out after 900s |
| `bun-security-scanner-matrix-without-node-modules.test.ts` | 0 | 0 | 0 | - | timed out after 900s |
| `bun-security-scanner-workspaces.test.ts` | 11 | 0 | 0 | 100.0% |  |
| `bun-update-lockfile-sync.test.ts` | 127 | 45 | 0 | 73.8% |  |
| `bun-update-security-edge-cases.test.ts` | 6 | 0 | 0 | 100.0% |  |
| `bun-update-security-provider.test.ts` | 1 | 1 | 0 | 50.0% |  |
| `bun-update-security-scan-all.test.ts` | 5 | 0 | 1 | 100.0% |  |
| `bun-update-security-simple.test.ts` | 2 | 0 | 0 | 100.0% |  |
| `bun-update-transitive.test.ts` | 271 | 55 | 0 | 83.1% |  |
| `bun-update.test.ts` | 135 | 40 | 0 | 77.1% |  |
| `bun-workspaces-self-contained.test.ts` | 23 | 2 | 0 | 92.0% |  |
| `bun-workspaces.test.ts` | 267 | 73 | 1 | 78.5% |  |
| `bunx-concurrent.test.ts` | 1 | 0 | 0 | 100.0% |  |
| `bunx-directories-bin.test.ts` | 4 | 0 | 0 | 100.0% |  |
| `bunx.test.ts` | 227 | 3 | 2 | 98.7% |  |
| `catalogs.test.ts` | 127 | 28 | 0 | 81.9% |  |
| `config-precedence.test.ts` | 137 | 4 | 0 | 97.2% |  |
| `config-version.test.ts` | 8 | 0 | 0 | 100.0% |  |
| `dt-unknown-readdir.test.ts` | 0 | 0 | 6 | - |  |
| `escape-control-characters.test.ts` | 41 | 25 | 0 | 62.1% |  |
| `frozen-lockfile-member-pins.test.ts` | 24 | 3 | 0 | 88.9% |  |
| `frozen-lockfile-outdated.test.ts` | 104 | 3 | 0 | 97.2% |  |
| `frozen-lockfile-pruned.test.ts` | 171 | 13 | 0 | 92.9% |  |
| `frozen-lockfile-workspace-names.test.ts` | 118 | 13 | 0 | 90.1% |  |
| `GHSA-pfwx-36v6-832x.test.ts` | 7 | 3 | 0 | 70.0% |  |
| `hoist.test.ts` | 129 | 46 | 0 | 73.7% |  |
| `install-fd-leak.test.ts` | 2 | 2 | 0 | 50.0% |  |
| `isolated-install.test.ts` | 191 | 92 | 7 | 67.5% |  |
| `isolated-relink.test.ts` | 3 | 3 | 1 | 50.0% |  |
| `lockfile-only.test.ts` | 34 | 0 | 0 | 100.0% |  |
| `lockfile-version-2.test.ts` | 15 | 0 | 0 | 100.0% |  |
| `migration/complex-workspace.test.ts` | 38 | 0 | 0 | 100.0% |  |
| `migration/migrate.test.ts` | 86 | 22 | 1 | 79.6% |  |
| `migration/npm-arborist.test.ts` | 54 | 3 | 0 | 94.7% |  |
| `migration/pnpm-comprehensive.test.ts` | 1 | 4 | 0 | 20.0% |  |
| `migration/pnpm-lock-migration.test.ts` | 5 | 15 | 0 | 25.0% |  |
| `migration/pnpm-lock-v9.test.ts` | 150 | 18 | 0 | 89.3% |  |
| `migration/pnpm-migration-complete.test.ts` | 0 | 1 | 0 | 0.0% |  |
| `migration/pnpm-migration.test.ts` | 8 | 6 | 11 | 57.1% |  |
| `migration/yarn-berry.test.ts` | 107 | 9 | 0 | 92.2% |  |
| `migration/yarn-lock-migration.test.ts` | 60 | 9 | 0 | 87.0% |  |
| `minimum-release-age.test.ts` | 231 | 13 | 0 | 94.7% |  |
| `nested-overrides.test.ts` | 284 | 34 | 0 | 89.3% |  |
| `npmrc.test.ts` | 425 | 158 | 5 | 72.9% |  |
| `overrides.test.ts` | 99 | 2 | 23 | 98.0% |  |
| `public-hoist-pattern.test.ts` | 12 | 2 | 0 | 85.7% |  |
| `redacted-config-logs.test.ts` | 47 | 51 | 0 | 48.0% |  |
| `shebang-normalize.test.ts` | 0 | 1 | 5 | 0.0% |  |
| `symlink-path-traversal.test.ts` | 68 | 25 | 2 | 73.1% |  |
| `test-dev-peer-dependency-priority.test.ts` | 4 | 0 | 0 | 100.0% |  |
