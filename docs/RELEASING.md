# npm releases

The GitHub repository is `SnowingFox/fuck-npm`. The unscoped npm name `fuck-npm`
is occupied by an unrelated package. Do not use that registry package to try
this project. Select a package name in an npm account or organization you can
publish to; executable names remain `fnpm`, `fnpm-lock`, and `fnpmx`.

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
   Mac. Authenticate there with `npm login --registry=https://registry.npmjs.org/`.
4. Generate a new release directory:

   ```sh
   node scripts/stage-release.mjs --name @your-scope/fuck-npm \
     --artifacts /path/to/artifacts --out /path/to/new-release
   node scripts/pack-release.mjs /path/to/new-release
   node scripts/publish-release.mjs --dir /path/to/new-release
   ```

   The last command dry-runs every tarball. Inspect `packed.json` and its file
   lists. Staging refuses missing artifacts, mismatched versions, and existing
   output directories. `--target` allows a one-platform smoke fixture and marks
   it incomplete; the publication command refuses that fixture.
5. Install the tarballs in an isolated directory and test the real bins, help,
   a dependency install, warm/no-op/frozen installs, and the shim with scripts
   disabled. Use a temporary local registry for staged dependency names; plain
   file tarballs alone do not satisfy their versioned registry dependencies.
6. From the Mac, publish the exact inspected tarballs:

   ```sh
   node scripts/publish-release.mjs --dir /path/to/new-release --publish
   ```

   This checks authentication and SHA-512 integrity before publishing platforms,
   then binding, then CLI. npm publication may require browser/2FA authentication.
   Never put credentials in the release directory or paste them into chat.
7. Install the actual published CLI name/version from npm in a fresh prefix and
   repeat the smoke. Record the name, version, source commit, Actions run, and
   tarball integrity in the release notes, and update installation instructions.

The release has not happened merely because a dry run or source-checkout smoke
passed. Full Bun parity and the performance gate remain separate requirements;
see [ROADMAP.md](ROADMAP.md).
