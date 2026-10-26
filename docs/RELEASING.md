# npm releases

The public npm CLI name is `fnpm`; executable names are `fnpm`,
`fnpm-lock`, `fnpx` and `fnpmx`. The unscoped npm name `fuck-npm` belongs to an
unrelated maintainer. The source monorepo stays private in npm and retains its
workspace package names.

## Single-package distribution

Starting with `0.0.2`, publish **only `fnpm`** to `https://registry.npmjs.org/`. Its manifest has no npm dependencies or optional dependencies. Eight
native archives are hosted on the matching GitHub Release, covering macOS arm64
and x64, Linux arm64 and x64 with glibc or musl, and Windows arm64 and x64.
Each archive contains `fnpm`, `fnpm-lock` (`.exe` on Windows), and `fnpm.<abi>.node`.

The npm tarball embeds `lib/native-release.json` with each archive's HTTPS URL,
SHA-512 integrity, file list and byte size. Postinstall downloads just the current
ABI, checks integrity and both executable versions, then installs into
`native/<abi>`. Only a fully validated archive replaces an incomplete installation.
The internal addon loader resolves that directory without platform npm packages.

Eligible Unix installs replace the four bin shims atomically using a hardlink or
copy followed by rename. Windows, pnpm and Yarn Berry retain Node shims because
their generated bin wrappers explicitly invoke Node. These installs still download
the native files. If scripts are disabled or postinstall cannot download, the shim
retries on first execution and reports an error if the native files remain
unavailable. Node 20.19+ and access to GitHub release assets are required for that
first download. Afterward, normal execution needs no archive download.

`FNPM_NATIVE_MIRROR` may point to a mirror directory containing the same archive
filenames. SHA-512 verification still uses the embedded manifest. HTTPS is
required except for localhost HTTP used by release smoke tests. It does not
change the registry used to install project dependencies.

## Build, stage and publish

1. Bump the CLI and development binding workspace versions together, update the
   workspace lockfile, regenerate `cli-spec.json` and version fixtures, and commit.
   Build and test on the development machine: `npm ci --ignore-scripts`,
   `npm test -w fuck-npm`, `FORCE_COLOR=0 npm test -w fuck-npm`,
   `npm run type-check -w fuck-npm`, and `npm run build -w fuck-npm`.
   Check the built `--version` and `--tree`, and run `node --test scripts/release.test.mjs`.
2. Use a successful `napi` Actions run for that source commit. Download all eight
   `binding-<rust-target>` artifacts into a new directory. Every artifact must
   contain the addon and both launchers. CI stages and installs the single-package
   tarball on all eight targets, including Alpine for musl.
3. Bring the matching source, built CLI `dist` and artifacts to the release Mac.
   Confirm `npm whoami --registry=https://registry.npmjs.org/` without exposing auth files.
   Stage into a new directory, specifying that registry and the final asset URL:

   ```sh
   node scripts/stage-single-release.mjs --name fnpm \
     --registry https://registry.npmjs.org/ \
     --asset-base https://github.com/SnowingFox/fuck-npm/releases/download/v0.0.2/ \
     --artifacts /path/to/artifacts --out /path/to/new-release
   node scripts/pack-release.mjs /path/to/new-release
   node scripts/smoke-release.mjs /path/to/new-release
   node scripts/publish-release.mjs --dir /path/to/new-release --cli npm
   ```

   The registry is public npm. Packing uses
   `npm pack --ignore-scripts`. Inspect `packed.json`: exactly one npm tarball,
   eight native assets, no platform dependencies. `--target` creates an incomplete
   one-platform CI fixture; publication refuses it. The localhost smoke exercises
   normal and disabled-script installs, first-use download, addon loading, all
   four bins, dependency execution, and offline/no-op/frozen installs.
4. Create the GitHub Release at the source commit and upload the exact staged
   `assets/*.tar.gz` before publishing the CLI, so its URLs are already reachable.
   Include checksums and source/CI provenance. Do not recreate archives after
   packing: their digests are embedded in the npm tarball.
5. From the Mac, publish the inspected single tarball:

   ```sh
   node scripts/publish-release.mjs --dir /path/to/new-release --cli npm --publish
   ```

   The helper checks authentication, target registry and SHA-512. It resumes an
   already published version only when registry integrity matches the local
   tarball, and refuses conflicting content. Never overwrite an existing version.
   Never put credentials in release artifacts or paste them into chat.
6. Install the exact name and version through the actual npm registry into fresh
   prefixes, with scripts enabled and disabled. Use the real GitHub URLs, invoke
   all four bins, load the addon, and install a local dependency. Record the source
   commit, Actions run, npm integrity and native asset integrity in release notes.

With npm versions that enforce lifecycle approval, approve the package with
`--allow-scripts=fnpm` or a project `allowScripts` entry. Disabled scripts
still work through first-use installation, but retain the Node startup overhead.

## Release history

`@tiktok-fe/fnpm@0.0.1` and its nine binding/platform packages were published via
bnpm on 2026-10-26. Its native artifacts came from source
`048c97ddbfcf617c7f559ed70b15114ace6f5bfb` and
[Actions run 40283402372](https://github.com/SnowingFox/fuck-npm/actions/runs/40283402372).
The earlier `@snowingfox` candidate returned a scope-permission error; the later
`@tiktok-fe` publication succeeded. Existing `0.0.1` packages remain unchanged.

`fnpm@0.0.2` targets public npm with the single-package layout above. The four
commands, including `fnpx`, belong to that one package.
[CI run 40294225393](https://github.com/SnowingFox/fuck-npm/actions/runs/40294225393)
passed all 21 jobs at native artifact source `e64f2878de3a4ddbe80a4169a46736b930c9b343`.
The release-name and documentation commit `ab814fccf499e563f611f4993efcd8db00968121`
has identical CLI and Rust sources. Local CLI verification passed 677 tests in
normal and no-color modes, type checking, bundle build and four Rust CLI fixture
tests. Windows archive creation streams tar bytes to avoid drive-letter parsing.
The public npm identity check currently requires renewed local authentication;
`fnpm` metadata reports a historical unpublish, which does not establish name
ownership. Publication and
fresh-registry verification must complete before treating a candidate as released.
Full Bun conformance and the strict speed gate remain separate open requirements;
see [ROADMAP.md](ROADMAP.md).
