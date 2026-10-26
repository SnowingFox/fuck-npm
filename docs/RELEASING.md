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

## 2026-10-26 candidate status

Candidate `0.0.1` uses source `048c97ddbfcf617c7f559ed70b15114ace6f5bfb`
and [Actions run 40283402372](https://github.com/SnowingFox/fuck-npm/actions/runs/40283402372),
which passed all 21 jobs. All eight native targets, the binding loader, and CLI
were packed as ten leaf tarballs. Normal and `--ignore-scripts` tarball installs
passed on macOS arm64, including the addon, four executable names, local dependency
execution through `fnpx`/`fnpmx`, and offline/no-op/frozen reinstalls.

The bnpm dry run passed. Actual publication of the first platform package returned
`E403`: the candidate `@snowingfox` scope was unavailable to the publishing account.
No candidate packages were published. A permitted registry scope is still required;
these checks do not make the candidate installable from a registry.

The release helper accepts bnpm's missing-version diagnostics on stderr while
still stopping on authorization errors. With npm versions that enforce lifecycle
approval, the local smoke explicitly allows this CLI's inspected postinstall in
the fixture's `allowScripts` policy. Global installs on those npm versions need
the package approved through npm's `--allow-scripts=<published-name>` setting to
swap the shim for the native executable automatically.
