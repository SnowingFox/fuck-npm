# Conformance status

Pass counts of the ported bun tests (tests/conformance/install) against the native release binary
(`FNPM_EXE=target/release/fnpm`). Regenerate with `node tests/conformance/tools/status.ts [file ...]`;
`FNPM_BUN_DIR` points at a bun checkout when the submodule is not initialised.
Only listed files have been measured. Each row keeps its latest per-file result; a partial run preserves other rows.
Timeouts and load failures are explicit in the note column; files without counts are excluded from the pass rate.

Integration run on 2026-10-25 from `23cc2210361a8f4b5b53221aa48b76a4dff55163`. Native CLI SHA-256:
`cd278a4690db221dca51b63a76b9ce929a651bdec9b49b10905b7191f1436aac`. Commands routed through TypeScript use the debug NAPI addon built from the same revision;
addon SHA-256 `c63d237756ec86f8a2f140b06b68175ccc646efd67cf80f453ec8afc401ef7a8`. The TypeScript bundle is from that checkout.
Adapter SHA-256 `b7ea7fd4bb39cc3346cc7dcb21c561784aa3211b4eb0826624401a900dcb2d5c` fixes pipes read after process exit. Completed suites were rerun with
that fix; incomplete suite notes preserve the prior attempt. Per-suite hashes are in
[the machine-readable result](results/2026-10-26-land.json). Execution sandboxes use their home npmrc
so the inherited test configuration cannot override it.
The 42 completed files were rerun against this fixed build. The registry timeout used this build;
the two incomplete scanner-matrix attempts retain their earlier source revision.
This is a measured subset of the 101 ported files; incomplete files do not contribute to the percentage.
The exact broad-track comparison is 2,250/4,849 passing assertions
versus 2,081/4,849 reviewed; execution/scanner is 488/501 versus 486/501.
The two scanner matrices use the ported CI sampling rule (about 10% chosen randomly); their skipped cases
are unmeasured, and their counts should not be compared across runs as a fixed test set.
Their terminal cases wait for `Bun.spawn({ terminal })` callbacks that the Node adapter does not implement;
matrix file timeouts therefore do not establish scanner behavior. Focused scanner files are reported separately.
Retry notes: The first create run had one spawn exit 127; isolated smoke checks passed and the complete file was rerun before acceptance. The first frozen workspace-names run had 76 passes and 55 failures, losing one reviewed passing assertion at a frozen reinstall. A complete-file retry had 77 passes and 54 failures with every reviewed passing assertion restored; no source change was made between attempts.
The lockfile suite also includes request-order/gate assertions that reach per-test deadlines; those failures
are included in its counts, separately from file-level timeout rows.

Total: 3667 passed, 2973 failed, 58 skipped (55.2% of the tests that ran).

| file | passed | failed | skipped | pass rate | note |
| --- | ---: | ---: | ---: | ---: | --- |
| `architecture-match.test.ts` | 30 | 0 | 0 | 100.0% |  |
| `bad-workspace.test.ts` | 15 | 32 | 0 | 31.9% |  |
| `bun-add-catalog.test.ts` | 71 | 82 | 0 | 46.4% |  |
| `bun-add-filter.test.ts` | 105 | 40 | 0 | 72.4% |  |
| `bun-add.test.ts` | 126 | 94 | 0 | 57.3% |  |
| `bun-audit.test.ts` | 133 | 127 | 0 | 51.2% |  |
| `bun-create.test.ts` | 138 | 3 | 1 | 97.9% |  |
| `bun-install-cpu-os.test.ts` | 26 | 17 | 0 | 60.5% |  |
| `bun-install-lifecycle-scripts.test.ts` | 251 | 183 | 2 | 57.8% |  |
| `bun-install-native-binlink.test.ts` | 25 | 2 | 0 | 92.6% |  |
| `bun-install-offline-misses.test.ts` | 5 | 30 | 0 | 14.3% |  |
| `bun-install-offline.test.ts` | 1 | 11 | 0 | 8.3% |  |
| `bun-install-patch.test.ts` | 14 | 39 | 0 | 26.4% |  |
| `bun-install-registry.test.ts` | 0 | 0 | 0 | - | timed out after 600s |
| `bun-install-security-provider.test.ts` | 96 | 2 | 0 | 98.0% |  |
| `bun-install.test.ts` | 346 | 426 | 6 | 44.8% |  |
| `bun-lock.test.ts` | 207 | 110 | 1 | 65.3% |  |
| `bun-pack.test.ts` | 370 | 11 | 1 | 97.1% |  |
| `bun-patch.test.ts` | 98 | 134 | 5 | 42.2% |  |
| `bun-pm-diff.test.ts` | 26 | 50 | 0 | 34.2% |  |
| `bun-pm-scan.test.ts` | 20 | 0 | 0 | 100.0% |  |
| `bun-publish.test.ts` | 279 | 0 | 1 | 100.0% |  |
| `bun-remove.test.ts` | 24 | 13 | 0 | 64.9% |  |
| `bun-security-scanner-matrix-with-node-modules.test.ts` | 0 | 0 | 0 | - | timed out after 600s |
| `bun-security-scanner-matrix-without-node-modules.test.ts` | 0 | 0 | 0 | - | stopped: Node adapter lacks terminal callbacks; TTY cases cannot finish |
| `bun-security-scanner-workspaces.test.ts` | 11 | 0 | 0 | 100.0% |  |
| `bun-update-security-scan-all.test.ts` | 5 | 0 | 1 | 100.0% |  |
| `bun-update.test.ts` | 100 | 75 | 0 | 57.1% |  |
| `bun-workspaces-self-contained.test.ts` | 4 | 21 | 0 | 16.0% |  |
| `bun-workspaces.test.ts` | 155 | 185 | 1 | 45.6% |  |
| `bunx-concurrent.test.ts` | 1 | 0 | 0 | 100.0% |  |
| `bunx-directories-bin.test.ts` | 4 | 0 | 0 | 100.0% |  |
| `bunx.test.ts` | 222 | 8 | 2 | 96.5% |  |
| `catalogs.test.ts` | 84 | 71 | 0 | 54.2% |  |
| `frozen-lockfile-member-pins.test.ts` | 10 | 17 | 0 | 37.0% |  |
| `frozen-lockfile-outdated.test.ts` | 28 | 79 | 0 | 26.2% |  |
| `frozen-lockfile-pruned.test.ts` | 11 | 173 | 0 | 6.0% |  |
| `frozen-lockfile-workspace-names.test.ts` | 77 | 54 | 0 | 58.8% |  |
| `isolated-install.test.ts` | 156 | 127 | 7 | 55.1% |  |
| `isolated-relink.test.ts` | 1 | 5 | 1 | 16.7% |  |
| `lockfile-only.test.ts` | 7 | 27 | 0 | 20.6% |  |
| `migration/migrate.test.ts` | 35 | 73 | 1 | 32.4% |  |
| `nested-overrides.test.ts` | 36 | 282 | 0 | 11.3% |  |
| `npmrc.test.ts` | 226 | 357 | 5 | 38.8% |  |
| `overrides.test.ts` | 88 | 13 | 23 | 87.1% |  |
