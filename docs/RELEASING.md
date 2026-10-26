# npm and bnpm releases

The published CLI name is `@tiktok-fe/fnpm`; executable names are `fnpm`,
`fnpm-lock`, `fnpx` and `fnpmx`. The unscoped npm name `fuck-npm` belongs to an
unrelated maintainer. The source monorepo stays private in npm and retains its
workspace package names.

## Single-package distribution

Starting with `0.0.2`, publish **only `@tiktok-fe/fnpm`** to the configured bnpm
registry. Its manifest has no npm dependencies or optional dependencies. Eight
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
   Confirm `bnpm whoami` against the intended registry without exposing auth files.
   Stage into a new directory, specifying that registry and the final asset URL:

   ```sh
   node scripts/stage-single-release.mjs --name @tiktok-fe/fnpm \
     --registry https://registry.example.com/ \
     --asset-base https://github.com/SnowingFox/fuck-npm/releases/download/v0.0.2/ \
     --artifacts /path/to/artifacts --out /path/to/new-release
   node scripts/pack-release.mjs /path/to/new-release
   node scripts/smoke-release.mjs /path/to/new-release
   node scripts/publish-release.mjs --dir /path/to/new-release --cli bnpm
   ```

   Replace the example registry with the configured bnpm registry. Packing uses
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
   node scripts/publish-release.mjs --dir /path/to/new-release --cli bnpm --publish
   ```

   The helper checks authentication, target registry and SHA-512. It resumes an
   already published version only when registry integrity matches the local
   tarball, and refuses conflicting content. Never overwrite an existing version.
   Never put credentials in release artifacts or paste them into chat.
6. Install the exact name and version through the actual bnpm registry into fresh
   prefixes, with scripts enabled and disabled. Use the real GitHub URLs, invoke
   all four bins, load the addon, and install a local dependency. Record the source
   commit, Actions run, npm integrity and native asset integrity in release notes.

With npm versions that enforce lifecycle approval, approve the package with
`--allow-scripts=@tiktok-fe/fnpm` or a project `allowScripts` entry. Disabled scripts
still work through first-use installation, but retain the Node startup overhead.

## Release history

`@tiktok-fe/fnpm@0.0.1` and its nine binding/platform packages were published via
bnpm on 2026-10-26. Its native artifacts came from source
`048c97ddbfcf617c7f559ed70b15114ace6f5bfb` and
[Actions run 40283402372](https://github.com/SnowingFox/fuck-npm/actions/runs/40283402372).
The earlier `@snowingfox` candidate returned a scope-permission error; the later
`@tiktok-fe` publication succeeded. Existing `0.0.1` packages remain unchanged.

`0.0.2` changes delivery to the single-package layout above. Publication and
fresh-registry verification must complete before treating a candidate as released.
Full Bun conformance and the strict speed gate remain separate open requirements;
see [ROADMAP.md](ROADMAP.md).
