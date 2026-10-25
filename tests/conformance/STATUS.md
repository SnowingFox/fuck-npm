# Conformance status

Pass counts of the ported bun tests (tests/conformance/install) against the native release binary
(`FNPM_EXE=target/release/fnpm`). Regenerate with `node tests/conformance/tools/status.ts [file ...]`;
`FNPM_BUN_DIR` points at a bun checkout when the submodule is not initialised.
Only listed files have been measured. Each row keeps its latest per-file result; a partial run preserves other rows.
Timeouts and load failures are explicit in the note column; files without counts are excluded from the pass rate.

Total: 2081 passed, 2768 failed, 47 skipped (42.9% of the tests that ran).

| file | passed | failed | skipped | pass rate | note |
| --- | ---: | ---: | ---: | ---: | --- |
| `architecture-match.test.ts` | 30 | 0 | 0 | 100.0% |  |
| `bad-workspace.test.ts` | 15 | 32 | 0 | 31.9% |  |
| `bun-add-catalog.test.ts` | 69 | 84 | 0 | 45.1% |  |
| `bun-add-filter.test.ts` | 105 | 40 | 0 | 72.4% |  |
| `bun-add.test.ts` | 116 | 104 | 0 | 52.7% |  |
| `bun-install-cpu-os.test.ts` | 24 | 19 | 0 | 55.8% |  |
| `bun-install-lifecycle-scripts.test.ts` | 215 | 219 | 2 | 49.5% |  |
| `bun-install-native-binlink.test.ts` | 21 | 6 | 0 | 77.8% |  |
| `bun-install-offline-misses.test.ts` | 5 | 30 | 0 | 14.3% |  |
| `bun-install-offline.test.ts` | 1 | 11 | 0 | 8.3% |  |
| `bun-install-registry.test.ts` | 0 | 0 | 0 | - | timed out after 600s |
| `bun-install.test.ts` | 339 | 433 | 6 | 43.9% |  |
| `bun-lock.test.ts` | 200 | 117 | 1 | 63.1% |  |
| `bun-remove.test.ts` | 24 | 13 | 0 | 64.9% |  |
| `bun-update.test.ts` | 98 | 77 | 0 | 56.0% |  |
| `bun-workspaces-self-contained.test.ts` | 4 | 21 | 0 | 16.0% |  |
| `bun-workspaces.test.ts` | 135 | 205 | 1 | 39.7% |  |
| `catalogs.test.ts` | 71 | 84 | 0 | 45.8% |  |
| `frozen-lockfile-member-pins.test.ts` | 10 | 17 | 0 | 37.0% |  |
| `frozen-lockfile-outdated.test.ts` | 9 | 98 | 0 | 8.4% |  |
| `frozen-lockfile-pruned.test.ts` | 11 | 173 | 0 | 6.0% |  |
| `frozen-lockfile-workspace-names.test.ts` | 75 | 56 | 0 | 57.3% |  |
| `isolated-install.test.ts` | 131 | 152 | 7 | 46.3% |  |
| `isolated-relink.test.ts` | 0 | 6 | 1 | 0.0% |  |
| `lockfile-only.test.ts` | 7 | 27 | 0 | 20.6% |  |
| `migration/migrate.test.ts` | 34 | 74 | 1 | 31.5% |  |
| `nested-overrides.test.ts` | 36 | 282 | 0 | 11.3% |  |
| `npmrc.test.ts` | 208 | 375 | 5 | 35.7% |  |
| `overrides.test.ts` | 88 | 13 | 23 | 87.1% |  |
