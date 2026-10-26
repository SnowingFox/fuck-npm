# npm and bnpm releases

The GitHub repository is `SnowingFox/fuck-npm`. The unscoped npm name `fuck-npm`
is occupied by an unrelated package. Do not use that registry package to try
this project. Select a package name in an npm account or organization you can
publish to. For bnpm, select a package name and namespace allowed by that registry; executable names remain `fnpm`, `fnpm-lock`, `fnpx`, and `fnpmx`.

Release tooling keeps workspace names unchanged and generates the published
names and exact dependencies in a separate staging directory. For example,
`--name @your-scope/fuck-npm` produces that CLI, `@your-scope/fnpm-binding`, and
eight `@your-scope/fnpm-binding-<abi>` packages. A missing package on the registry
does not establish permission to publish to its scope.

1. Use a reviewed commit and a successful `napi` Actions run for that commit.
   Download its eight `binding-<rust-target>` artifacts into an empty directory.
   Each must contain `fnpm.<abi>.node`, `fnpm`, and `fnpm-lock` (`.exe` on Windows).
2. Build and verify the CLI on the development machine:
   `npm ci --ignore-scripts`, `npm run build -w fuck-npm`, `npm test -w fuck-npm`,
   `FORCE_COLOR=0 npm test -w fuck-npm`, and `npm run type-check -w fuck-npm`.
3. Bring the source, built `packages/fuck-npm/dist`, and artifacts to the release
   Mac. Check the selected registry identity there with `npm whoami --registry=https://registry.npmjs.org/`
   or `bnpm whoami --registry=https://registry.example.com/`. Log in with the selected CLI if needed.
4. Generate a new release directory:

   ```sh
   node scripts/stage-release.mjs --name @your-scope/fuck-npm \
     --artifacts /path/to/artifacts --out /path/to/new-release
   node scripts/pack-release.mjs /path/to/new-release
   node scripts/publish-release.mjs --dir /path/to/new-release
   ```

   For bnpm, stage the leaf packages with the bnpm registry, then dry-run using bnpm:

   ```sh
   node scripts/stage-release.mjs --name @snowingfox/fuck-npm \
     --registry https://registry.example.com/ \
     --artifacts /path/to/artifacts --out /path/to/new-bnpm-release
   node scripts/pack-release.mjs /path/to/new-bnpm-release
   node scripts/publish-release.mjs --dir /path/to/new-bnpm-release \
     --cli bnpm --registry https://registry.example.com/
   ```

   `@snowingfox` is a candidate namespace; verify its registry permissions before a release.
   `--registry` is recorded in every staged leaf manifest's `publishConfig` and
   in `release.json`/`packed.json`. Publication defaults to that registry and refuses
   a different registry, so re-stage and re-pack to change destinations. The root
   monorepo stays private; only these generated leaf packages are published.
   Packing still uses `npm pack` for either registry.

   The last command dry-runs every tarball. Inspect `packed.json` and its file
   lists. Staging refuses missing artifacts, mismatched versions, and existing
   output directories. `--target` allows a one-platform smoke fixture and marks
   it incomplete; the publication command refuses that fixture.
5. Run `node scripts/smoke-release.mjs /path/to/new-release`. It installs the
   actual tarballs through a temporary localhost registry and checks the real bins, help,
   a dependency install, warm/no-op/frozen installs, and the shim with scripts
   disabled. The temporary registry serves only the staged packages; the dependency
   fixture is a local tarball. File tarballs alone do not satisfy their versioned
   registry dependencies, so retain this registry-based smoke.
6. From the Mac, publish the exact inspected tarballs:

   ```sh
   node scripts/publish-release.mjs --dir /path/to/new-release --publish
   ```

   For the bnpm release, use:

   ```sh
   node scripts/publish-release.mjs --dir /path/to/new-bnpm-release \
     --cli bnpm --registry https://registry.example.com/ --publish
   ```

   This checks authentication and SHA-512 integrity before publishing platforms,
   then binding, then CLI. If a run stops, rerun the same command: an existing
   exact version is skipped only when its registry integrity matches the inspected
   tarball. A conflicting existing version stops the release. Publication may require browser/2FA authentication.
   Never put credentials in the release directory or paste them into chat.
7. Install the actual published CLI name/version from the selected registry in a fresh prefix and
   repeat the smoke. Record the name, version, source commit, Actions run, and
   tarball integrity in the release notes, and update installation instructions.

The release has not happened merely because a dry run or source-checkout smoke
passed. Full Bun parity and the performance gate remain separate requirements;
see [ROADMAP.md](ROADMAP.md).
