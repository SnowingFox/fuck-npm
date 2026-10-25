# fnpm feature parity with bun's package manager

The checklist for porting every package-manager feature of bun (reference: `submodule/bun`) to fnpm. Each row means "implemented and tested in fnpm". Source of truth: `docs/inventory/bun-pm.json`.

A ticked row is implemented and tested in its owning crate (unit and integration tests against bun's cases). Rows owned by `fnpm-core` and `cli` stay open until the command is wired end to end; end-to-end gates are tracked in `docs/ROADMAP.md`. Crate rows left open are partial; their gaps are listed in `docs/ROADMAP.md`.

Conventions:

- Names: `bun` becomes `fnpm`, `bunx` becomes `fnpmx` / `fnpm x`, `bun.lock` becomes `fnpm.lock`, `bunfig.toml` becomes `fnpm.toml`. fnpm still reads `bunfig.toml` and reads `bun.lock`/`bun.lockb` for migration. `BUN_*` env vars become `FNPM_*`; the `BUN_*` spelling is read as a fallback. The `node_modules/.bun` store becomes `node_modules/.fnpm`.
- Paths after `bun:` are relative to `submodule/bun`.
- Owners: `fnpm-types`, `fnpm-semver`, `fnpm-config`, `fnpm-registry`, `fnpm-store`, `fnpm-resolver`, `fnpm-lockfile`, `fnpm-linker`, `fnpm-scripts`, `fnpm-workspace`, `fnpm-core`, `cli` (TS CLI in `packages/fuck-npm`: argv parsing, flag validation, help, prompts/TUI, progress and install-summary rendering). Every other command behaviour, including the pre-rendered `lines` a command prints, belongs to `fnpm-core` (see the command table in `docs/ARCHITECTURE.md`).

## Commands

### Dispatch, help and flag parsing

- [ ] **Command names and aliases** — `install`/`i` (with `-g` routes to add), `ci` (forces frozen lockfile), `add`/`a`, `remove`/`rm`/`r`/`uninstall`, `update`/`up`, `outdated`; `install <pkgs>` acts as add · bun: `src/runtime/cli/mod.rs` · owner: `cli`
- [ ] **Top-level pm aliases** — `fnpm whoami`, `fnpm list` (= `pm ls`), `fnpm info` (= `pm view`), `fnpm why` (= `pm why`); leading `pm` positional dropped · bun: `src/runtime/cli/mod.rs` · owner: `cli`
- [ ] **Command header line** — `fnpm <cmd> v<version>` (one newline; the summary opens with the blank line, as bun's tree printer does) unless silent/no-summary; outdated always prints it; `update -i` prints `fnpm update --interactive vX` · bun: `src/install/PackageManager/updatePackageJSONAndInstall.rs` · owner: `cli`
- [ ] **--help / -h per subcommand** — Usage/Alias/description, flag table from the param table, examples + docs URL; exit 0 · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `cli`
- [ ] **Unknown long flag is an error** — `fnpm <cmd> has no --<flag> flag.` + note `Run 'fnpm <cmd> --help'`, exit 1; other parse errors print help + diagnostic · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `cli`
- [ ] **Foreign npm/yarn/pnpm flag translation** — install/add/remove/update only: `--save-dev`→`--dev`, `--save-exact`, `--immutable`→frozen, `--shamefully-hoist`/`--node-linker`/`--install-strategy`→`--linker`, `--loglevel`→log level, etc. · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `cli`
- [ ] **Foreign flag gating rules** — alias skipped if command owns that flag or lacks target flag; update ignores dev/prod aliases; `--name value` == `--name=value` · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `cli`
- [ ] **Ignored foreign flags** — boolean (`--no-audit`, `--no-fund`, `--legacy-peer-deps`, `--pure-lockfile`, …) and valued (`--cache-folder`, `--store-dir`, `--fetch-retries`, …) plus `--config.*` silently skipped · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `cli`
- [ ] **Hidden runtime flags accepted** — `-b/--bun`, `--watch`, `--hot`, `--smol`, `--if-present`, `--no-install`, … accepted and ignored by PM commands · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `cli`
- [ ] **Flag conflicts** — `--production`+`--trust`, `--frozen-lockfile`+`--trust`, `--catalog`+`-g`, `--filter`+`-g`, `prune -g` error; `-g` silently disables `--recursive` · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `cli`
- [ ] **--filter / --json gating** — `--filter` only for outdated/install/update/add/remove/prune/pm/publish; `--json` only for audit/pm/info · bun: `src/install/PackageManager.rs` · owner: `cli`
- [ ] **Missing package.json handling** — add/install create `{"dependencies": {}}`; update/remove crash `No package.json, so nothing to update|remove`; pm commands error unless no-project-ok · bun: `src/install/PackageManager/PackageManagerDirectories.rs` · owner: `fnpm-core`

### Shared install flags (install/add/remove/update/outdated and pm commands)

- [ ] **-c, --config <path>** — fnpm.toml path (absolute or relative to cwd); replaces ./fnpm.toml · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `cli`
- [x] **-y, --yarn** — also write `yarn.lock` v1 · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `fnpm-lockfile`
- [ ] **-p, --production / -P, --prod** — no local devDependencies, implies frozen lockfile + fail-early, lockfile not saved; for update means prod selector only · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-core`
- [ ] **--no-save / --save** — no lockfile and no package.json write (still installs); `--save` default true (false for link/unlink) · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-core`
- [x] **--ca <PEM>... / --cafile <path>** — replace config ca+cafile wholesale · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-registry`
- [ ] **--dry-run** — resolve and list; no node_modules, no package.json/lockfile/yarn.lock write, no scripts · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-core`
- [ ] **--frozen-lockfile** — lockfile never saved; error if package.json disagrees with lockfile · bun: `src/install/PackageManager/install_with_manager.rs` · owner: `fnpm-core`
- [ ] **-f, --force** — ignore manifest cache-control, reinstall all packages, force-save lockfile · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-core`
- [x] **--cache-dir <path>** — override cache directory (resolved once vs start dir) · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-config`
- [x] **--no-cache** — skip manifest cache for this run · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-registry`
- [ ] **--silent / --quiet / --verbose** — precedence silent > verbose > quiet; silent = errors only, no summary; quiet hides progress · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `cli`
- [ ] **--no-progress / --no-summary** — disable progress bar / disable summary only · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `cli`
- [x] **--no-verify** — skip tarball integrity verification (local tarballs still verified) · bun: `src/install/extract_tarball.rs` · owner: `fnpm-store`
- [x] **--offline / --prefer-offline** — no network / cached manifests of any age; offline beats prefer-offline · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-registry`
- [x] **--ignore-scripts** — run no lifecycle scripts, including root/trusted · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-scripts`
- [ ] **--trust** — add named packages to trustedDependencies and run their scripts · bun: `src/install/PackageManager/package_json_write_back.rs` · owner: `fnpm-core`
- [ ] **-g, --global** — operate on global dir package.json/lockfile; link bins to global bin dir · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-core`
- [ ] **--cwd <dir>** — chdir once on first parse; `.`-relative resolved vs cwd; failure `failed to change directory to "p"` · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `cli`
- [x] **--env-file <f>... / --no-env-file** — load only these env files / disable default .env loading · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `fnpm-config`
- [x] **--backend <method>** — clonefile (macOS default), clonefile_each_dir, hardlink (Linux/Windows default), copyfile, symlink; unknown silently ignored · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `fnpm-linker`
- [x] **--registry <url>** — must start with http(s):// (error redacts creds); beats env/config and publishConfig.registry · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `fnpm-config`
- [x] **--concurrent-scripts <N>** — max concurrent lifecycle scripts; 0/invalid keeps default 2×cores · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-scripts`
- [x] **--network-concurrency <N>** — max simultaneous requests (default 64); u16 else crash `Expected --network-concurrency to be a number between 0 and 65535`; clamped ≥1 · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `fnpm-registry`
- [x] **--save-text-lockfile** — save text lockfile (fnpm.lock) · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-lockfile`
- [ ] **--omit dev|optional|peer** — repeatable; dev off locally, optional/peer off local+remote; bad value crashes `invalid \`omit\` value` · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-core`
- [ ] **--lockfile-only** — resolve + write lockfile, no node_modules, no tarball prefetch · bun: `src/install/PackageManager/install_with_manager.rs` · owner: `fnpm-core`
- [x] **--linker isolated|hoisted** — override linker; invalid → `Invalid value for --linker`, exit 1 · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `fnpm-linker`
- [x] **--minimum-release-age <N|duration>** — seconds or `90m`/`3d`/`2 weeks`; invalid fatal; 0 clears gate · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `fnpm-resolver`
- [x] **--minimum-release-age-excludes <s>...** — repeatable, comma-split, trimmed; replaces config list · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-resolver`
- [x] **--cpu / --os / --libc <s>...** — override platform filter; `*`, `any`, `none`, `!name`; unknown name fatal with valid list · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `fnpm-resolver`
- [x] **--use-system-ca / --use-openssl-ca / --use-bundled-ca** — pick CA store; more than one → `choose exactly one`, exit 1 · bun: `src/bun_core/Global.rs` · owner: `fnpm-registry`

### fnpm install / fnpm ci

- [x] **-F, --filter <pattern>...** — install only matching workspaces' deps (name/path globs, `!`); unmatched patterns reported; root implicit · bun: `src/install/PackageManager/workspace_selection.rs` · owner: `fnpm-workspace`
- [ ] **fnpm ci** — install with frozen lockfile forced · bun: `src/runtime/cli/install_command.rs` · owner: `cli`
- [ ] **-a, --analyze <files>** — scan imports of given files, add missing bare packages (implies --only-missing); fnpm uses a JS import scanner, not a bundler · bun: `src/runtime/cli/pm_update_package_json.rs` · owner: `fnpm-core`
- [ ] **No-lockfile frozen install** — frozen with no lockfile installs without writing one; migrated lockfiles kept in memory · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-core`
- [x] **Leftover cleanup after skipped group** — `add -d x --production`: remove unplaced top-level folder, prune bins · bun: `src/install/PackageManager/updatePackageJSONAndInstall.rs` · owner: `fnpm-linker`

### fnpm add

- [ ] **-d, --dev / -D, --development** — write to devDependencies · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `cli`
- [ ] **--optional / --peer** — write to optionalDependencies / peerDependencies; precedence dev > optional > peer > dependencies · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `cli`
- [ ] **--save-prod** — force dependencies even when another group lists the name (moves entry) · bun: `src/install/PackageManager/PackageJSONEditor.rs` · owner: `fnpm-core`
- [ ] **-E, --exact** — write resolved version without `^` · bun: `src/install/PackageManager/PackageJSONEditor.rs` · owner: `fnpm-core`
- [ ] **--only-missing** — skip names already present in any group; existing ranges untouched · bun: `src/install/PackageManager/PackageJSONEditor.rs` · owner: `fnpm-core`
- [ ] **--catalog[=NAME]** — write version into root catalog and `catalog:`/`catalog:<name>` into target; requires root `workspaces` · bun: `src/install/PackageManager/add_catalog.rs` · owner: `fnpm-core`
- [ ] **--catalog edge cases** — reuse entry unless version given; change notes; rejects local paths, `workspace:`, member names; auto-uses existing default catalog entry · bun: `src/install/PackageManager/add_catalog.rs` · owner: `fnpm-core`
- [ ] **-F, --filter** — edit each selected workspace; root only when named explicitly; `*` excludes root · bun: `src/install/PackageManager/add_remove_with_filter.rs` · owner: `fnpm-workspace`
- [ ] **Positional spec parsing** — trim, posix separators; `name`, `name@ver`, `alias@npm:real@ver`, git/github/URL/path; dedupe; `unrecognised dependency format` crash · bun: `src/install/PackageManager/UpdateRequest.rs` · owner: `fnpm-core`
- [ ] **No package given** — `error: no package specified to add`, prints help, exit 0 · bun: `src/install/PackageManager/updatePackageJSONAndInstall.rs` · owner: `cli`
- [ ] **Target group selection** — without flag update existing entry in place (deps, dev, optional, peer order); all non-peer duplicates rewritten unless group skipped · bun: `src/install/PackageManager/PackageJSONEditor.rs` · owner: `fnpm-core`
- [ ] **Group move semantics** — group flag moves entry; peer entries stay; dev entry kept beside new peer and follows its range; empty groups deleted · bun: `src/install/PackageManager/PackageJSONEditor.rs` · owner: `fnpm-core`
- [ ] **Written version literal** — dist-tag/bare → `^<resolved>` (exact with -E); explicit range written as typed; `workspace:*`; git/tarball/path verbatim · bun: `src/install/PackageManager/PackageJSONEditor.rs` · owner: `fnpm-core`
- [ ] **Name collisions for nameless specs** — key = resolved name, replaces older literal; `"a" and "b" both resolve to "name"` error · bun: `src/install/PackageManager/PackageJSONEditor.rs` · owner: `fnpm-core`
- [ ] **Local path respelling** — paths rewritten relative to target package.json (`./`, keep `file:`/`link:`, trailing `/`); absolute with -g; differing spellings across filters error · bun: `src/install/PackageManager/add_remove_with_filter.rs` · owner: `fnpm-core`
- [ ] **Global add** — only bins of named packages linked globally; PATH warning per shell · bun: `src/install/PackageManager/updatePackageJSONAndInstall.rs` · owner: `fnpm-core`

### fnpm remove

- [ ] **Remove from all groups** — deletes name from dependencies/dev/optional/peer; empty group objects deleted · bun: `src/install/PackageManager/PackageJSONEditor.rs` · owner: `fnpm-core`
- [ ] **Remove errors** — non-object / empty package.json crash; no dependency groups exit 0; no package `no package specified to remove` · bun: `src/install/PackageManager/updatePackageJSONAndInstall.rs` · owner: `fnpm-core`
- [ ] **Post-remove cleanup** — delete leftover top-level `node_modules/<name>`, prune dangling `.bin`; with -g prune global bins and relink freed names · bun: `src/install/PackageManager/install_with_manager.rs` · owner: `fnpm-linker`
- [ ] **-F, --filter** — only workspaces listing the name change; none changed → `<names> is not a dependency of <ws>`, exit 0 · bun: `src/install/PackageManager/add_remove_with_filter.rs` · owner: `fnpm-workspace`
- [ ] **Remove summary** — `- <name>` rows, `<N> package(s) removed [time]` counting requests that left the lockfile · bun: `src/install/PackageManager/install_with_manager.rs` · owner: `cli`

### fnpm update

- [ ] **Bare update** — newest within declared ranges (direct+transitive); rewrites only `^`/`~`/exact forms keeping operator; never reads manifest cache · bun: `src/install/PackageManager/PackageJSONEditor.rs` · owner: `fnpm-core`
- [ ] **Range rewrite rules** — keep complex/short ranges (`~1`, `^0`), dist-tags, `catalog:` (root entry updated), npm: prefix, workspace ranges; `up to date, package.json synced` note · bun: `src/install/PackageManager/PackageJSONEditor.rs` · owner: `fnpm-core`
- [ ] **update <names|patterns>** — updates only those (incl. transitive); `name@range`; `*` globs and `!` negation over lockfile names; detailed no-match errors · bun: `src/install/update_scope.rs` · owner: `fnpm-core`
- [ ] **-L, --latest** — direct deps to `latest` tag keeping range style; rewrites pins/tags; no downgrade of ahead prereleases; error with explicit version · bun: `src/install/PackageManager/PackageJSONEditor.rs` · owner: `fnpm-core`
- [ ] **-d/-D, --dev; -p/-P, --prod; --no-optional** — select groups to update only (install still installs all); combine with names/latest/-i · bun: `src/install/update_scope.rs` · owner: `fnpm-core`
- [ ] **--depth 0** — only direct entries re-resolved; other values error `--depth only accepts 0` · bun: `src/install/update_scope.rs` · owner: `fnpm-core`
- [ ] **-r, --recursive / -F, --filter** — rewrite every / selected workspace package.json; root catalogs edited when targeted · bun: `src/install/PackageManager/package_json_write_back.rs` · owner: `fnpm-workspace`
- [ ] **-E, --exact** — write bare versions except peers and catalog entries used by peers · bun: `src/install/PackageManager/PackageJSONEditor.rs` · owner: `fnpm-core`
- [ ] **-g, --global** — update packages in the global dir (all or named) · bun: `src/runtime/cli/update_command.rs` · owner: `fnpm-core`
- [x] **Held-back rules** — never widen ranges; overrides and patched versions held (`kept x (patched, vY available)`); unfetchable git/tarball warn; failed direct fetch errors · bun: `src/install/update_transitive.rs` · owner: `fnpm-resolver`

### fnpm update --interactive

- [ ] **TTY requirement** — non-TTY stdin errors with note to use update/outdated, exit 1; env override for tests · bun: `src/runtime/cli/update_interactive_command.rs` · owner: `cli`
- [ ] **Package list** — outdated direct deps grouped by type with Current/Target/Latest(/Workspace) columns, catalog rows grouped, colors by semver diff · bun: `src/runtime/cli/update_interactive_command.rs` · owner: `cli`
- [ ] **Keys and viewport** — Space, a, n, i, l, j/k/arrows, ←/→ target/latest, PgUp/PgDn, mouse wheel, Enter confirm, Ctrl+C cancel; width-based column caps · bun: `src/runtime/cli/update_interactive_command.rs` · owner: `cli`
- [ ] **Apply selection** — `--dry-run` prints `↑ name from → to`; else rewrites package.json/catalog literals then runs `update <selected>` · bun: `src/runtime/cli/update_interactive_command.rs` · owner: `fnpm-core`

### fnpm outdated

- [ ] **Outdated table** — Package/Current/Update/Latest[/Workspace]; boxed when ANSI, `|` table otherwise; dev/peer/optional suffix; colored versions · bun: `src/runtime/cli/outdated_command.rs` · owner: `fnpm-core`
- [ ] **-F, --filter / -r, --recursive** — select workspaces and show Workspace column; else cwd workspace · bun: `src/runtime/cli/outdated_command.rs` · owner: `fnpm-workspace`
- [ ] **Positional patterns** — glob per name and alias; `!` negation; `*` all · bun: `src/runtime/cli/outdated_command.rs` · owner: `fnpm-core`
- [ ] **Min-release-age marker** — ` *` when latest filtered by minimumReleaseAge + footer note · bun: `src/runtime/cli/outdated_command.rs` · owner: `fnpm-core`
- [ ] **Catalog rows** — one row per (name, catalog, group) with `catalog (ws1, ws2)` · bun: `src/runtime/cli/outdated_command.rs` · owner: `fnpm-core`
- [ ] **No --json; forces dry run** — `--json` unsupported; missing lockfile `nothing outdated`; fetch failures crash after table · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `cli`

### fnpm link / fnpm unlink

- [ ] **fnpm link (register)** — validate name, symlink project into `<global>/node_modules/<name>`, link bins globally, print `Success! Registered "name"` + usage hints · bun: `src/runtime/cli/link_command.rs` · owner: `fnpm-core`
- [ ] **fnpm link <pkg>** — add `link:<name>` deps like add; `-g` ignored · bun: `src/runtime/cli/link_command.rs` · owner: `fnpm-core`
- [ ] **fnpm unlink** — current dir only; not linked → exit 0 message; removes global entry and bins · bun: `src/runtime/cli/unlink_command.rs` · owner: `fnpm-core`
- [ ] **--save opt-in for link/unlink** — `--no-save` default; `--save` writes package.json + lockfile · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `cli`
- [ ] **--dry-run** — `dry run: would link "name"` / unlink message, no writes · bun: `src/runtime/cli/link_command.rs` · owner: `fnpm-core`

### fnpm patch / fnpm patch-commit

- [ ] **fnpm patch <pkg|path>** — name, name@version, or node_modules path; replace folder with editable real copy; print edit + commit instructions · bun: `src/install/PackageManager/patchPackage.rs` · owner: `fnpm-core`
- [ ] **patch refusals** — file:/link:/workspace/bundled refused; multiple versions need version; missing arg error · bun: `src/install/PackageManager/patchPackage.rs` · owner: `fnpm-core`
- [ ] **--commit <dir> / patch-commit** — `git diff --no-index` pristine vs edited; verify by applying; set patchedDependencies; install; write patch after checks · bun: `src/install/PackageManager/patchPackage.rs` · owner: `fnpm-core`
- [ ] **--patches-dir <dir>** — default `patches`; written relative, posix · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `cli`
- [ ] **Empty / binary diffs** — empty removes existing patch or `No changes detected`; binary changes error · bun: `src/install/PackageManager/patchPackage.rs` · owner: `fnpm-core`
- [ ] **Patch file naming** — `<name>@<version>.patch`, creds stripped, unsafe chars percent-escaped · bun: `src/install/PackageManager/patchPackage.rs` · owner: `fnpm-core`
- [ ] **--dry-run** — `dry run: would write <path> and add <key> to patchedDependencies` · bun: `src/install/PackageManager/patchPackage.rs` · owner: `fnpm-core`

### fnpm prune

- [x] **Prune behavior** — remove packages not in lockfile tree, stale store entries, staging leftovers, dangling bins; never touches workspaces/dotdirs/registry/scripts · bun: `src/install/prune.rs` · owner: `fnpm-linker`
- [x] **-p/--production, --omit** — also remove dev-only / omitted groups · bun: `src/install/prune.rs` · owner: `fnpm-linker`
- [ ] **--check / --dry-run** — list removable; `--check` exits 1 if any · bun: `src/runtime/cli/prune_command.rs` · owner: `fnpm-core`
- [x] **--os/--cpu/--libc, --linker** — platform filter; linker tie-break for layout detection · bun: `src/install/prune.rs` · owner: `fnpm-linker`
- [ ] **-F, --filter** — limit to selected workspaces, keep what others need in shared locations · bun: `src/install/prune.rs` · owner: `fnpm-workspace`
- [ ] **Preconditions** — no args; `-g` rejected; lockfile must match package.json; no node_modules → `nothing to prune`; symlinked node_modules error · bun: `src/install/prune.rs` · owner: `fnpm-core`
- [x] **Layout detection** — vote hoisted (dirs) vs isolated (store symlinks), tie → configured linker · bun: `src/install/prune.rs` · owner: `fnpm-linker`

### fnpm dedupe

- [x] **Dedupe algorithm** — re-point edges to fewest locked versions satisfying effective ranges (prefer newer); patched versions pinned; peer variants not duplicates · bun: `src/install/dedupe.rs` · owner: `fnpm-resolver`
- [ ] **--check / --dry-run** — report `N duplicate versions can be removed`; check exits 1 · bun: `src/runtime/cli/dedupe_command.rs` · owner: `fnpm-core`
- [ ] **--lockfile-only, --linker** — write lockfile only / pick linker · bun: `src/runtime/cli/dedupe_command.rs` · owner: `fnpm-core`
- [ ] **Frozen/no-save refusal** — fails with reason + note when lockfile can't be written · bun: `src/runtime/cli/dedupe_command.rs` · owner: `fnpm-core`
- [ ] **Dedupe output** — `↳ name from → to`, `(removed)`, `(downgrade)`, summary / no-duplicates line · bun: `src/runtime/cli/dedupe_command.rs` · owner: `fnpm-core`
- [x] **collapse_appended on install/update** — newly appended versions collapsed, existing untouched · bun: `src/install/dedupe.rs` · owner: `fnpm-resolver`

### fnpm audit

- [ ] **Positionals** — none or `fix`; others `unknown subcommand "X"` + note; `fix <args>` error · bun: `src/runtime/cli/audit_command.rs` · owner: `cli`
- [ ] **--json** — raw merged registry JSON (unfiltered); exit code still filtered · bun: `src/runtime/cli/audit_command.rs` · owner: `fnpm-core`
- [ ] **--audit-level low|moderate|high|critical** — keep severity ≥ level; invalid errors with valid list · bun: `src/runtime/cli/audit_command.rs` · owner: `fnpm-core`
- [ ] **--ignore <id>...** — match advisory id exactly or substring of URL (GHSA works) · bun: `src/runtime/cli/audit_command.rs` · owner: `fnpm-core`
- [ ] **-p/--prod/--production, --omit** — audit only reachable packages · bun: `src/runtime/cli/audit_command.rs` · owner: `fnpm-core`
- [ ] **Audit report** — per package versions, severity-colored vulns, dependency paths, summary + fix hints; clean line with below-level/ignored/skipped counts · bun: `src/runtime/cli/audit_command.rs` · owner: `fnpm-core`
- [ ] **Audit exit code** — 0 when no remaining vulns after filters, else 1; bad response 1 · bun: `src/runtime/cli/audit_command.rs` · owner: `fnpm-core`
- [ ] **fnpm audit fix** — plan lowest safe version all dependents allow; exact own pins treated as `^`; install, re-audit, print fixed/remaining · bun: `src/install/audit_fix.rs` · owner: `fnpm-core`
- [ ] **audit fix --latest / --dry-run / --json** — rewrite own ranges keeping style / plan only / single JSON object · bun: `src/install/audit_fix/json.rs` · owner: `fnpm-core`
- [ ] **audit fix preconditions** — lockfile must match; refuses frozen/production/no-save with notes · bun: `src/runtime/cli/audit_command.rs` · owner: `fnpm-core`
- [ ] **audit fix sections** — fixing / blocked by dependent range / no published fix / unmatched / downgrade and min-release-age markers · bun: `src/install/audit_fix.rs` · owner: `fnpm-core`

### fnpm publish

- [ ] **Directory publish flow** — pack for publish, run prepublishOnly/prepack/prepare/postpack, PUT, then publish/postpublish · bun: `src/runtime/cli/publish_command.rs` · owner: `fnpm-core`
- [ ] **[tarball] positional** — publish existing tarball: strip first component, find package.json/README, no scripts · bun: `src/runtime/cli/publish_command.rs` · owner: `fnpm-core`
- [ ] **--access public|restricted** — invalid crashes; restricted unscoped refused · bun: `src/runtime/cli/publish_command.rs` · owner: `fnpm-core`
- [ ] **--tag <tag>** — default latest; version-like tag invalid; prerelease without tag error · bun: `src/runtime/cli/publish_command.rs` · owner: `fnpm-core`
- [x] **--otp <code> / --auth-type web|legacy** — OTP header; auth type default web · bun: `src/runtime/cli/publish_command.rs` · owner: `fnpm-registry`
- [ ] **--gzip-level, --ignore-scripts, --dry-run, --registry** — as pack; dry run prints ` + name@version (dry-run)` · bun: `src/runtime/cli/publish_command.rs` · owner: `fnpm-core`
- [x] **--tolerate-republish** — existing version → warn and exit 0 · bun: `src/runtime/cli/publish_command.rs` · owner: `fnpm-core`
- [x] **--provenance / --no-provenance / --provenance-file** — provenance requested from any source is refused `not supported yet` (fnpm may implement later) · bun: `src/runtime/cli/publish_command.rs` · owner: `cli`
- [ ] **-r, --recursive / -F, --filter** — dependency order, skip private and already-published, pack all then publish; flags must follow `publish` · bun: `src/runtime/cli/publish_command.rs` · owner: `fnpm-workspace`
- [x] **Publish validation** — name/version required, valid semver, private refused · bun: `src/runtime/cli/publish_command.rs` · owner: `fnpm-core`
- [x] **Request body** — `_id`, dist-tags, normalized manifest (`_integrity`, dist, bin object, readme), base64 `_attachments` · bun: `src/runtime/cli/publish_command.rs` · owner: `fnpm-registry`
- [ ] **publishConfig** — tag/access unless CLI; registry and `@scope:registry` replace defaults unless `--registry`; no env expansion · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-config`

### fnpm pm pack

- [x] **--destination <dir> / --filename <name>** — mutually exclusive; destination created · bun: `src/runtime/cli/pack_command.rs` · owner: `cli`
- [x] **--gzip-level <0-9>** — default 9; invalid errors · bun: `src/runtime/cli/pack_command.rs` · owner: `cli`
- [x] **--quiet / --silent / --dry-run / --ignore-scripts** — quiet prints only tarball path; dry run computes sizes without tarball · bun: `src/runtime/cli/pack_command.rs` · owner: `fnpm-core`
- [x] **Pack scripts** — prepack, prepare, postpack with npm_* env; re-read package.json after · bun: `src/runtime/cli/pack_command.rs` · owner: `fnpm-scripts`
- [ ] **Tarball name and output** — `<scope>-<name>-<version>.tgz`; file list, shasum, integrity, sizes, bundled deps · bun: `src/runtime/cli/pack_command.rs` · owner: `fnpm-core`
- [ ] **File selection** — `files` field, `.npmignore` else `.gitignore`, default ignores, always-included README/LICENSE, lockfiles never packed · bun: `src/runtime/cli/pack_command.rs` · owner: `fnpm-core`
- [ ] **Bins in tarball** — `bin`/`directories.bin` added with +x; must stay inside package · bun: `src/runtime/cli/pack_command.rs` · owner: `fnpm-core`
- [ ] **bundledDependencies** — bool or array; resolve via node_modules ancestry, nest transitive deps, cycle error · bun: `src/runtime/cli/pack_command.rs` · owner: `fnpm-core`
- [ ] **workspace:/catalog: substitution** — rewrite specs to concrete versions in packed package.json; failure crashes · bun: `src/runtime/cli/pack_command.rs` · owner: `fnpm-core`
- [x] **Deterministic tarball** — pax + gzip, fixed mtime 1985-10-26, `package/` prefix, package.json first · bun: `src/runtime/cli/pack_command.rs` · owner: `fnpm-store`
- [x] **Atomic tarball write** — temp file + rename; symlink/FIFO handling · bun: `src/runtime/cli/pack_command.rs` · owner: `fnpm-core`

### fnpm pm (dispatcher and subcommands)

- [ ] **pm dispatcher and help** — lists all subcommands; unknown → help + `"X" unknown command` exit 1; none → help exit 0 · bun: `src/runtime/cli/package_manager_command.rs` · owner: `cli`
- [ ] **pm per-subcommand flag gating** — `--max-age` cache, diff flags, licenses flags, ls flags, pack flags, version flags, why flags; misuse error · bun: `src/install/PackageManager/CommandLineArguments.rs` · owner: `cli`
- [ ] **Lockfile load errors** — `missing lockfile` + note run install; `failed to <verb> lockfile` · bun: `src/runtime/cli/package_manager_command.rs` · owner: `fnpm-core`
- [ ] **pm bin [-g]** — print bin dir path; `-g` warns when not in $PATH · bun: `src/runtime/cli/package_manager_command.rs` · owner: `fnpm-core`
- [ ] **pm ls / fnpm list** — flat direct deps of installed packages; filtered by on-disk presence · bun: `src/runtime/cli/package_manager_command.rs` · owner: `fnpm-core`
- [ ] **pm ls --all / --trusted / --depth 0 / names** — full tree, trusted only, depth 0, search by name (no match exit 1) · bun: `src/runtime/cli/package_manager_command.rs` · owner: `fnpm-core`
- [x] **pm hash / hash-string / hash-print** — recompute meta hash / print hash input / print stored hash · bun: `src/runtime/cli/package_manager_command.rs` · owner: `fnpm-lockfile`
- [ ] **pm cache** — print cache dir path; bad positional usage error · bun: `src/runtime/cli/package_manager_command.rs` · owner: `fnpm-core`
- [x] **pm cache rm [--dry-run]** — clear contents from trusted sources only; refuses $HOME/project/install dirs; also clears fnpmx temp dirs · bun: `src/runtime/cli/package_manager_command.rs` · owner: `fnpm-store`
- [x] **pm cache prune [--max-age days] [--dry-run]** — delete entries unused for N days (default 30), keep global store and manifests · bun: `src/runtime/cli/pm_cache_prune_command.rs` · owner: `fnpm-store`
- [x] **pm cache pack / unpack <file>** — portable cache archive with checksums; validate before writing, never overwrite · bun: `src/install/cache_pack.rs` · owner: `fnpm-store`
- [ ] **pm fetch** — download all lockfile deps into cache without node_modules/scripts/writes; `--dry-run` counts · bun: `src/runtime/cli/pm_fetch_command.rs` · owner: `fnpm-core`
- [ ] **pm migrate [--dry-run] [--force]** — migrate foreign lockfile; existing lockfile requires --force · bun: `src/runtime/cli/package_manager_command.rs` · owner: `fnpm-lockfile`
- [x] **pm untrusted** — list installed deps with blocked scripts and their scripts; non-registry undeclared note · bun: `src/runtime/cli/pm_trusted_command.rs` · owner: `fnpm-scripts`
- [x] **pm trust <names...> | --all [--dry-run]** — run blocked scripts deepest-first, add to trustedDependencies, save lockfile · bun: `src/runtime/cli/pm_trusted_command.rs` · owner: `fnpm-scripts`
- [x] **pm default-trusted** — print default trusted list with count · bun: `src/runtime/cli/pm_trusted_command.rs` · owner: `fnpm-scripts`
- [ ] **pm version <increment>** — patch/minor/major/pre*/prerelease/from-git/explicit; node-semver inc semantics · bun: `src/runtime/cli/pm_version_command.rs` · owner: `fnpm-core`
- [ ] **pm version flags** — `--preid`, `--allow-same-version`, `-m/--message %s`, `--git-tag-version`/`--no-git-tag-version`, `--force` dirty tree, `--dry-run` · bun: `src/runtime/cli/pm_version_command.rs` · owner: `fnpm-core`
- [ ] **pm version git and scripts** — preversion/version/postversion with env; commit and annotated tag `v<ver>` · bun: `src/runtime/cli/pm_version_command.rs` · owner: `fnpm-core`
- [ ] **pm pkg get/set/delete/fix** — key path grammar (dots, `[i]`, `[]` append); `--json` strict values; `--dry-run`; preserves formatting · bun: `src/runtime/cli/pm_pkg_command.rs` · owner: `fnpm-core`
- [ ] **pm view / fnpm info <pkg>[@ver] [prop...] [--json]** — fetch full manifest, select version, print summary or properties · bun: `src/runtime/cli/pm_view_command.rs` · owner: `fnpm-core`
- [x] **pm whoami** — username from config creds or `GET /-/whoami`; missing auth error · bun: `src/install/npm.rs` · owner: `fnpm-registry`
- [ ] **pm licenses [ls]** — group installed packages by license; `--json`, `--filter`, `--dev`, `--long`, `--prod`/`--omit` · bun: `src/runtime/cli/pm_licenses_command.rs` · owner: `fnpm-core`
- [ ] **pm diff sides and filters** — registry/local/tarball sides, `name@a..b`, path/glob filters · bun: `src/runtime/cli/pm_diff_command.rs` · owner: `fnpm-core`
- [ ] **pm diff output modes** — `--raw`/`--unformatted`, `--minify`, `--unminify`, `-w`, `--stat`, `--name-only`, `-U <n>`, `--json` · bun: `src/runtime/cli/pm_diff_command.rs` · owner: `fnpm-core`
- [ ] **pm diff normalization** — JS/JSON/CSS canonical re-print, formatting-only detection, git-applyable patch when piped · bun: `src/runtime/cli/pm_diff_normalize.rs` · owner: `fnpm-core`
- [ ] **pm diff security notes** — install scripts, new builtins, eval/network growth, executables, bidi/escapes, manifest disagreements · bun: `src/runtime/cli/pm_diff_normalize.rs` · owner: `fnpm-core`
- [x] **pm scan** — run security scanner over all lockfile packages; exit 1 on any advisory · bun: `src/runtime/cli/scan_command.rs` · owner: `fnpm-core`
- [ ] **fnpm ./bun.lockb [--hash]** — print binary lockfile as yarn.lock v1, or its hash · bun: `src/runtime/cli/mod.rs` · owner: `fnpm-lockfile`

### fnpm why

- [ ] **why <pattern>** — name globs (`*`), optional version query; matches aliases; no match exit 1 · bun: `src/runtime/cli/why_command.rs` · owner: `fnpm-core`
- [ ] **--top / --depth <N>** — depth 1 / max depth (default 100) · bun: `src/runtime/cli/why_command.rs` · owner: `fnpm-core`
- [ ] **why output** — dependents tree sorted by specificity/type/name, `*circular`, `*deduped`, workspace labels, peers via served copies · bun: `src/runtime/cli/why_command.rs` · owner: `fnpm-core`

### fnpm run

- [x] **run <script>** — look up nearest package.json `scripts`, else `node_modules/.bin` binary; `Script not found`, exit 1 · bun: `src/runtime/cli/run_command.rs` · owner: `fnpm-scripts`
- [x] **Passthrough args** — args after script name go to it, shell-escaped; never to pre/post · bun: `src/runtime/cli/run_command.rs` · owner: `fnpm-scripts`
- [x] **Script cwd** — directory of the defining package.json; PATH binaries run in process cwd · bun: `src/runtime/cli/run_command.rs` · owner: `fnpm-scripts`
- [x] **pre/post hooks** — `pre<name>`, main, `post<name>`; non-zero stops chain · bun: `src/runtime/cli/run_command.rs` · owner: `fnpm-scripts`
- [x] **--shell system** — POSIX `sh -c` via bash/sh/zsh search on original PATH; Windows cmd.exe · bun: `src/runtime/cli/run_command.rs` · owner: `fnpm-scripts`
- [x] **PATH with node_modules/.bin chain** — every ancestor's `.bin` prepended; original PATH kept for shell lookup · bun: `src/runtime/cli/run_command.rs` · owner: `fnpm-scripts`
- [x] **Run env vars** — npm_command=run-script, npm_package_name/version/json/config_*, npm_lifecycle_event/script per script, user agent; also set per package in --filter mode · bun: `src/runtime/cli/run_command.rs` · owner: `fnpm-scripts`
- [ ] **--silent** — no `$ cmd` echo and no fnpm error messages · bun: `src/runtime/cli/run_command.rs` · owner: `fnpm-core`
- [ ] **--if-present** — missing script/bin exits 0 silently · bun: `src/runtime/cli/run_command.rs` · owner: `fnpm-core`
- [x] **Exit code/signal propagation** — `script "x" exited with code N`; signals re-raised; SIGINT silent · bun: `src/runtime/cli/run_command.rs` · owner: `fnpm-scripts`
- [ ] **No-arg script listing** — usage + `package.json scripts (N found)` list · bun: `src/runtime/cli/run_command.rs` · owner: `fnpm-core`
- [ ] **--cwd, --env-file, --no-env-file, -c/--config** — chdir, explicit env files, config path · bun: `src/runtime/cli/Arguments.rs` · owner: `cli`
- [ ] **--no-orphans** — parent-death watchdog, kill descendants on exit; propagated via env · bun: `src/runtime/cli/Arguments.rs` · owner: `fnpm-scripts`
- [x] **--filter / --workspaces run** — run script across selected packages in topological order, unlimited concurrency, cycle fallback · bun: `src/runtime/cli/filter_run.rs` · owner: `fnpm-workspace`
- [ ] **--filter TUI / plain output, --elide-lines <N>** — framed per-script panes on TTY, prefixed lines otherwise · bun: `src/runtime/cli/filter_run.rs` · owner: `cli`
- [x] **--parallel / --sequential / --no-exit-on-error** — Foreman-style multi-script with `label |` prefixes, globs over script names · bun: `src/runtime/cli/multi_run.rs` · owner: `fnpm-scripts`
- [x] **NODE_CHANNEL_FD IPC passthrough** — keep IPC fd for node-style parent/child IPC (POSIX) · bun: `src/runtime/cli/run_command.rs` · owner: `fnpm-scripts`
- [ ] **Shell completions via getcompletes** — script names list (skip pre/post), descriptions · bun: `src/runtime/cli/run_command.rs` · owner: `cli`

### fnpm x / fnpmx

- [ ] **Invocation and flags** — `fnpmx` argv0 or `fnpm x`; `--package/-p`, `--no-install`, `--trust`, `--ignore-scripts`, `--registry`, `--ca/--cafile`, `--cwd`, `--env-file`, `--minimum-release-age`, `--verbose/--silent` · bun: `src/runtime/cli/bunx_command.rs` · owner: `cli`
- [ ] **Flag boundary and foreign flags** — flags must precede spec; npx/pnpm/yarn flags skipped; unknown flag error · bun: `src/runtime/cli/bunx_command.rs` · owner: `cli`
- [ ] **Spec parsing and bin inference** — aliases `tsc`→typescript, `claude`; scoped name bin is a guess; GitHub repo name · bun: `src/runtime/cli/bunx_command.rs` · owner: `fnpm-core`
- [ ] **Lookup order** — PATH, project node_modules bin, cache tree, then install; versioned specs skip PATH · bun: `src/runtime/cli/bunx_command.rs` · owner: `fnpm-core`
- [x] **Private cache dir** — `<cache>/.fnpmx-<uid>` 0700 owned; temp fallback warns; tree naming with option suffixes · bun: `src/runtime/cli/bunx_command.rs` · owner: `fnpm-store`
- [ ] **Freshness and offline fallback** — refresh daily; dist-tag rechecks; `--no-install` uses stale; registry unreachable uses cached copy · bun: `src/runtime/cli/bunx_command.rs` · owner: `fnpm-core`
- [ ] **Install into cache with flock** — exclusive `<tree>.lock`; spawn `fnpm add` with forwarded flags; concurrent-safe · bun: `src/runtime/cli/bunx_command.rs` · owner: `fnpm-core`
- [ ] **Run resolved binary** — argv `[bin, ...args]`, no shell, cwd = process cwd, cache `.bin` prepended · bun: `src/runtime/cli/run_command.rs` · owner: `fnpm-core`

### fnpm create

- [ ] **create <template>** — `foo`→`create-foo`, `@org/x`→`@org/create-x`, `@org`→`@org/create`; then `fnpm x` · bun: `src/runtime/cli/bunx_command.rs` · owner: `fnpm-core`

### fnpm completions

- [ ] **completions [dir]** — install bash/zsh/fish script to detected dir, enable in .zshrc, print to stdout when piped · bun: `src/runtime/cli/install_completions_command.rs` · owner: `cli`
- [ ] **getcompletes [filter] [prefix]** — dynamic backend: scripts, bins, command list, npm name prefix list (`a`) · bun: `src/runtime/cli/shell_completions.rs` · owner: `cli`

## Configuration

### fnpm.toml discovery

- [x] **Global then project file** — `$XDG_CONFIG_HOME/.fnpm.toml` or `~/.fnpm.toml`, then `--config` or `./fnpm.toml` (bunfig.toml also read); later wins per key · bun: `src/bunfig/arguments.rs` · owner: `fnpm-config`
- [x] **TOML vs JSONC** — `.toml` parsed as TOML, else JSONC; non-object root error · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **Global-only keys** — project `globalDir`/`globalBinDir` ignored with warning unless passed via `--config` · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **Global install config** — with `-g`, only `--config` or the user global file is loaded · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-config`

### [install] keys

- [x] **install.registry** — URL string (embedded `user:pass@`, `user@`, `:token@`) or `{url, username, password, token}`; `$VAR` expansion; CR/LF/NUL fatal · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.scopes** — `{scope: registry}` with or without `@`; merges over .npmrc, config wins per scope · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.ca / install.cafile** — PEM string/array or file path relative to config; one combined setting · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.exact** — save versions without `^` · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.dryRun** — same as `--dry-run` · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.production** — no dev deps, frozen, fail-early · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.frozenLockfile** — frozen lockfile · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.saveTextLockfile** — true text, false binary lockfile · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.ignoreScripts** — no lifecycle scripts · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.optional / install.peer / install.dev** — enable optional, peer, local dev deps (all default true) · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.linkWorkspacePackages** — default true; npm range satisfied by workspace links it · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.hoist** — default true; false skips isolated hidden hoist dir · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.offline** — offline mode · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.globalStore** — enable global virtual store for isolated linker · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.force / disableManifest / disableCache** — force install; disable manifest cache / all cache · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-config`
- [x] **install.concurrentScripts** — number; 0 keeps default 2×threads · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.linker** — `"isolated"` or `"hoisted"` else error; default auto · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.lockfile {print, save, path, savePath}** — `print="yarn"` writes yarn.lock; `save` bool forces save; custom paths · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.globalDir / install.globalBinDir** — global dirs, `~` expanded, relative to config · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.cache** — bool, dir string, or `{disable, disableManifest, dir}` · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.security.scanner** — scanner package name or path; must be string in a table object · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.minimumReleaseAge** — seconds or duration string; stored ms; 0 = off · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.minimumReleaseAgeExcludes** — `name` or `name@1.2.3 || 1.2.4`; malformed entries warned and dropped · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.publicHoistPattern / install.hoistPattern** — string or array of pnpm globs with `!` · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.prefer** — `offline`/`latest`/`online`; offline makes install prefer-offline · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.logLevel** — `debug`/`error`/`warn`/`info` · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **install.saveYarnLockfile / provenance** — write yarn.lock; publish provenance request · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-config`

### [run] keys

- [x] **run.shell** — `system` (fnpm has no embedded shell; `bun` value warned) · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **run.silent** — suppress command echo and errors · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **run.elide-lines** — default lines shown in --filter TUI · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`
- [x] **run.noOrphans** — enable parent-death watchdog · bun: `src/bunfig/bunfig.rs` · owner: `fnpm-config`

### .npmrc

- [x] **File locations** — `$NPM_CONFIG_USERCONFIG` else XDG `.npmrc` else `~/.npmrc`, then project `.npmrc`; later wins; global npmrc not read · bun: `src/install/PackageManager.rs` · owner: `fnpm-config`
- [x] **INI parser** — npm/ini compatible: sections, `key[]`, comments, quotes, `${VAR}` and `${VAR?}` expansion, `__proto__` skipped · bun: `src/ini/lib.rs` · owner: `fnpm-config`
- [x] **registry / @scope:registry** — default and scoped registries · bun: `src/ini/lib.rs` · owner: `fnpm-config`
- [x] **//host/path/:_authToken|_auth|username|_password|email** — nerf-dart keys, npm hasAuth order, regFromURI walk · bun: `src/ini/lib.rs` · owner: `fnpm-config`
- [x] **certfile / keyfile** — warned `not supported` (no mTLS); misspelled credential keys warned · bun: `src/ini/lib.rs` · owner: `fnpm-config`
- [x] **cache, dry-run, ca, cafile** — cache dir (`false` disables), dry run, CA settings · bun: `src/ini/lib.rs` · owner: `fnpm-config`
- [x] **omit / include** — dev|peer|optional toggles · bun: `src/ini/lib.rs` · owner: `fnpm-config`
- [x] **ignore-scripts, link-workspace-packages, save-exact, provenance, hoist** — npm boolean coercion · bun: `src/ini/lib.rs` · owner: `fnpm-config`
- [x] **install-strategy / node-linker** — hoisted/linked; pnpm|isolated / node-modules|hoisted · bun: `src/ini/lib.rs` · owner: `fnpm-config`
- [x] **public-hoist-pattern / hoist-pattern** — string or array; invalid dropped · bun: `src/ini/lib.rs` · owner: `fnpm-config`

### Environment variables

- [x] **Registry env** — `FNPM_REGISTRY` > `FNPM_CONFIG_REGISTRY` > `BUN_CONFIG_REGISTRY` > `NPM_CONFIG_REGISTRY` > `npm_config_registry`; first non-empty wins · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-config`
- [x] **Token env** — `FNPM_CONFIG_TOKEN` > `NPM_CONFIG_TOKEN` > `npm_config_token` as Bearer on default scope · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-config`
- [x] **NPM_CONFIG_USERCONFIG / NPM_CONFIG_IGNORE_SCRIPTS / NPM_CONFIG_PROVENANCE** — user npmrc path; force scripts off; publish provenance · bun: `src/install/PackageManager.rs` · owner: `fnpm-config`
- [x] **FNPM_HOME, FNPM_STORE_DIR, FNPM_CACHE_DIR, FNPM_GLOBAL_DIR, FNPM_GLOBAL_BIN_DIR, FNPM_GLOBAL_STORE** — home root (bun `BUN_INSTALL`), store, cache, global dir, bin dir, global store toggle; `BUN_INSTALL*` spellings read as fallback · bun: `src/bun_core/env_var.rs` · owner: `fnpm-config`
- [x] **FNPM_PROGRESS / FNPM_VERBOSE** — `FNPM_PROGRESS=0` (or `BUN_INSTALL_PROGRESS=0`) disables progress; verbose logging · bun: `src/bun_core/env_var.rs` · owner: `fnpm-config`
- [x] **FNPM_MANIFEST_CACHE** — 1 cache without cache-control, 2 with, other off · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-config`
- [x] **FNPM_CONFIG_YARN_LOCKFILE** — any value writes yarn.lock · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-config`
- [x] **FNPM_CONFIG_SKIP_SAVE_LOCKFILE / SKIP_LOAD_LOCKFILE / SKIP_INSTALL_PACKAGES** — any value other than `0` skips · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-config`
- [x] **FNPM_CONFIG_NO_VERIFY** — `1`/`true` skips integrity · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-config`
- [ ] **HTTP tuning env** — `_HTTP_RETRY_COUNT` (5), `_HTTP_RETRY_BACKOFF` (50ms), `_MAX_HTTP_REQUESTS` (64), `_HTTP_IDLE_TIMEOUT` (300s), `_DNS_TIME_TO_LIVE_SECONDS` (30) · bun: `src/bun_core/env_var.rs` · owner: `fnpm-registry`
- [x] **Install tuning env** — streaming min size 2MiB, drain threshold 256KiB, Windows rename retry 5000ms, `FNPM_TMPDIR` · bun: `src/bun_core/env_var.rs` · owner: `fnpm-store`
- [x] **Feature-flag env** — disable install index, streaming install, copy_file_range, ficlonerange, native binlink, ignore-scripts optimizer; force junctions · bun: `src/bun_core/env_var.rs` · owner: `fnpm-config`
- [x] **XDG_CONFIG_HOME / XDG_CACHE_HOME / HOME / USERPROFILE** — config and cache roots · bun: `src/bun_core/env_var.rs` · owner: `fnpm-config`
- [x] **GITHUB_API_URL** — base for GitHub tarballs (default api.github.com) · bun: `src/install/PackageManager/runTasks.rs` · owner: `fnpm-registry`
- [x] **TLS env** — `NODE_TLS_REJECT_UNAUTHORIZED=0`, `NODE_USE_SYSTEM_CA`, `SSL_CERT_FILE`, `SSL_CERT_DIR`, `NODE_EXTRA_CA_CERTS` · bun: `src/bun_core/env_var.rs` · owner: `fnpm-registry`
- [x] **CI detection** — `CI`, `TDDIUM`, `GITHUB_ACTIONS`, `JENKINS_URL`, `bamboo.buildKey` disable progress; CI name in user agent · bun: `src/bun_core/env_var.rs` · owner: `fnpm-config`

### Precedence

- [x] **npmrc < fnpm.toml < env < CLI** — overlay rules: registry replaces, scopes merge, ca+cafile together; npmrc url auth fills remaining scopes last · bun: `src/install/PackageManager.rs` · owner: `fnpm-config`
- [x] **Credential carry-over** — env/CLI registry keeps creds only if same host + path prefix without https downgrade · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-config`
- [x] **Scope with empty URL** — entry removed; creds attached to default registry · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-config`
- [x] **Option defaults** — retries 5, network 64, scripts 2×threads, bin `node_modules/.bin`, auto linker, current cpu/os/libc, manifest cache on · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-config`
- [x] **Derived option rules** — frozen disables save; dry run strips writes; offline forces manifest cache; scanner/lockfile-only disable prefetch; update disables manifest cache · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-config`

## Dependency protocols

- [x] **Tag inference** — classify specifier into npm/dist-tag/tarball/folder/git/github/workspace/catalog/link with bun's exact rules incl. leading-digit tags and scp-like · bun: `src/install/dependency.rs` · owner: `fnpm-types`
- [x] **npm semver ranges** — `=`, `<`, `>`, `^`, `~`, x-ranges, hyphen, `||`, prerelease; leading `v` stripped · bun: `src/install/dependency.rs` · owner: `fnpm-semver`
- [x] **dist-tags and `*`** — empty = latest; `*` picks latest unless deprecated; implicit latest skips deprecated · bun: `src/install/npm.rs` · owner: `fnpm-resolver`
- [x] **npm: alias** — `npm:name@range`, scoped; `npm:^1` is own-name range; nameless aliases error/warn · bun: `src/install/dependency.rs` · owner: `fnpm-types`
- [x] **git dependencies** — git+ssh/https, git://, ssh://, scp-like, gitlab:/bitbucket:; committish after last `#` · bun: `src/install/repository.rs` · owner: `fnpm-registry`
- [x] **git process** — bare clone cache, fetch, `git log` commit resolve, `restore` checkout, hooks disabled, ssh accept-new, askpass echo · bun: `src/install/git_runner.rs` · owner: `fnpm-registry`
- [x] **git checkout cleanup** — remove .git and tag file, strip node_modules unless bundled, safe resolved tag check · bun: `src/install/git_runner.rs` · owner: `fnpm-store`
- [x] **GitHub dependencies** — `github:`, `user/repo`, github URLs → API tarball; tag `<owner>-<repo>-<sha7>` · bun: `src/install/hosted_git_info.rs` · owner: `fnpm-registry`
- [x] **hosted-git-info port** — bug-for-bug npm hosted-git-info for github/gitlab/bitbucket/gist/sourcehut · bun: `src/install/hosted_git_info.rs` · owner: `fnpm-types`
- [x] **Tarball URLs and local tarballs** — http(s) remote, `file:` local; suffix detection; unsupported `://` error; name fallback · bun: `src/install/dependency.rs` · owner: `fnpm-types`
- [x] **file: folders** — `file:` paths, npm-compat `file://../x`; unsafe paths rejected; workspace wins over same-name folder · bun: `src/install/dependency.rs` · owner: `fnpm-resolver`
- [x] **link:** — package name = `fnpm link` registration, else directory symlink; stored with `./` · bun: `src/install/dependency.rs` · owner: `fnpm-resolver`
- [x] **workspace:** — `*`/`^`/`~`, ranges must match, `name@range` alias, path form, root self-reference · bun: `src/install/lockfile/Package.rs` · owner: `fnpm-workspace`
- [x] **catalog:** — `catalog:`/`catalog:default`/`catalog:<name>` from root workspaces or top-level catalogs; root/workspaces only · bun: `src/install/lockfile/CatalogMap.rs` · owner: `fnpm-resolver`
- [x] **patch: protocol** — unsupported; treated as npm with warning · bun: `src/install/dependency.rs` · owner: `fnpm-types`

## Resolution policy

- [x] **overrides / resolutions** — root only; npm `overrides` beats yarn `resolutions`; `//` comments; non-object error · bun: `src/install/lockfile/OverrideMap.rs` · owner: `fnpm-resolver`
- [x] **Override values** — any spec, `$name` reference, `catalog:`; `-`/`patch:`/empty warned · bun: `src/install/lockfile/OverrideMap.rs` · owner: `fnpm-resolver`
- [x] **Override selectors** — `name@range`, one-level nested, pnpm `a>b`, yarn `a/b`, `**/b`; too deep errors · bun: `src/install/lockfile/override_selector.rs` · owner: `fnpm-resolver`
- [x] **Override matching precedence** — ranged parent > parent > flat; target range by declared spec; not applied to npm: aliases · bun: `src/install/lockfile/OverrideMap.rs` · owner: `fnpm-resolver`
- [x] **catalogs** — default + named; duplicate definition errors; non-root catalog: unresolved; catalog peer → optional `*` · bun: `src/install/lockfile/CatalogMap.rs` · owner: `fnpm-resolver`
- [x] **patchedDependencies** — root `{"name@ver": path}`; patch hash in cache name; missing file fails before linking · bun: `src/install/patch_install.rs` · owner: `fnpm-resolver`
- [x] **Patch apply at install** — apply patch to cache entry `_patch_hash=`; skipped sections warned · bun: `src/install/patch_install.rs` · owner: `fnpm-store`
- [x] **packageExtensions** — `name[@range]` → add deps/optional/peers/meta; allowed spec kinds; recorded in lockfile so removal undoes · bun: `src/install/lockfile/package_extensions.rs` · owner: `fnpm-resolver`
- [x] **trustedDependencies** — root+workspace union; declared list replaces default; `[]` none; non-npm trust requires declaration · bun: `src/install/lockfile.rs` · owner: `fnpm-scripts`
- [x] **Default trusted list** — built-in list (~367 names) for npm packages with canonical tarball URL only · bun: `src/install/default-trusted-dependencies.txt` · owner: `fnpm-scripts`
- [x] **minimumReleaseAge gate** — skip versions newer than age; excludes; stability window; needs full manifest `time` · bun: `src/install/npm.rs` · owner: `fnpm-resolver`
- [x] **minimumReleaseAge errors** — TooRecent / AllVersionsTooRecent messages incl. peers · bun: `src/install/npm.rs` · owner: `fnpm-resolver`
- [x] **cpu / os / libc filtering** — Negatable arrays with `!`, `any`, `none`; disabled packages kept in lockfile but not downloaded/placed · bun: `src/install_types/resolver_hooks.rs` · owner: `fnpm-resolver`
- [x] **Peer dependencies** — installed by default; meta-only names synthesize optional `*` peer; mismatch and missing warnings · bun: `src/install/lockfile/Package.rs` · owner: `fnpm-resolver`
- [x] **optionalDependencies** — failures skipped silently; failed script deletes package; duplicates with deps merge · bun: `src/install/lifecycle_script_runner.rs` · owner: `fnpm-resolver`
- [x] **bundledDependencies (install side)** — both spellings; bundled deps not fetched separately · bun: `src/install/lockfile/Package.rs` · owner: `fnpm-resolver`
- [x] **linkWorkspacePackages** — npm range matching a workspace version links it; same-name dist-tag links · bun: `src/install/lockfile/Package.rs` · owner: `fnpm-resolver`
- [ ] **Reuse cached versions** — any cached version satisfying range used before download · bun: `src/install/PackageManager/install_with_manager.rs` · owner: `fnpm-resolver`

## Linking & layouts

- [x] **Linker selection** — CLI > fnpm.toml > .npmrc `node-linker`; values hoisted/isolated · bun: `src/install_types/NodeLinker.rs` · owner: `fnpm-linker`
- [x] **Auto linker** — isolated if configVersion 1 + workspaces (not npm-migrated) or `node_modules/.fnpm` exists, else hoisted · bun: `src/install/lockfile.rs` · owner: `fnpm-linker`
- [x] **Isolated store layout** — `node_modules/.fnpm/<key>[+peerhash]/node_modules/<name>`; store key escaping, 80-byte cap + hash · bun: `src/install/isolated_install/Store.rs` · owner: `fnpm-linker`
- [x] **Isolated dedupe and relink** — same pkg + peer set share entry; existing entries reused; `--force` rebuilds · bun: `src/install/isolated_install.rs` · owner: `fnpm-linker`
- [x] **Hidden hoist dir** — `.fnpm/node_modules/<dep>` first claimant wins, filtered by hoistPattern; `hoist=false` disables · bun: `src/install/isolated_install.rs` · owner: `fnpm-linker`
- [x] **publicHoistPattern** — matching transitive deps symlinked into root node_modules; direct deps never replaced · bun: `src/install/isolated_install.rs` · owner: `fnpm-linker`
- [x] **pnpm matcher semantics** — `*`, globs, `!` exclude; all-include / all-exclude / mixed last-match rules · bun: `src/install_types/NodeLinker.rs` · owner: `fnpm-linker`
- [x] **Global virtual store** — `<cache>/links/<key>-<hash>`; eligibility rules (no trusted/patched/folder); SCC hashing; atomic rename · bun: `src/install/isolated_install.rs` · owner: `fnpm-linker`
- [x] **Materialization backends** — clonefile, hardlink, copyfile fallback, symlink · bun: `src/install/isolated_install/FileCloner.rs` · owner: `fnpm-linker`
- [ ] **Isolated task pipeline** — link → symlink deps → wait deps → dep bins → preinstall → own bins → remaining scripts · bun: `src/install/isolated_install/Installer.rs` · owner: `fnpm-linker`
- [x] **Hoisted install** — trees in order, parent trees first, batched tasks, bins/scripts after · bun: `src/install/hoisted_install.rs` · owner: `fnpm-linker`
- [x] **Switching linkers** — move old entries to `.old_modules-<hex>` and delete in background; keep other dot dirs · bun: `src/install/isolated_install.rs` · owner: `fnpm-linker`
- [ ] **Self-contained workspaces** — `installConfig.hoistingLimits: "workspaces"` or `workspaces.selfContained`: hoisting barrier, real copies, not in lockfile · bun: `src/install/lockfile/Package/WorkspaceMap.rs` · owner: `fnpm-linker`
- [ ] **Up-to-date fast path** — node_modules matches expected packages → no work · bun: `src/install/PackageManager/install_with_manager.rs` · owner: `fnpm-core`
- [x] **bin field parsing** — string, object, `directories.bin`; non-string entries skipped · bun: `src/install/bin.rs` · owner: `fnpm-linker`
- [x] **Bin name normalization** — basename after `/ \ :`; reject empty, `.`, `..`, control chars · bun: `src/install/bin.rs` · owner: `fnpm-linker`
- [x] **.bin symlinks** — relative links, mkdir on ENOENT, replace foreign links, chmod 0777&~umask, CRLF shebang fix · bun: `src/install/bin.rs` · owner: `fnpm-linker`
- [x] **Bin conflicts** — first wins; direct before transitive, registry before local, then alphabetical; global names pre-seeded · bun: `src/install/bin.rs` · owner: `fnpm-linker`
- [ ] **Windows bin shims** — `.exe` + metadata shim per bin, shebang/extension interpreter selection, foreign exe protection · bun: `src/install/windows-shim/BinLinkingShim.rs` · owner: `fnpm-linker`
- [x] **Isolated .bin linking** — each entry gets deps' bins and own bins; linked node_modules real paths · bun: `src/install/isolated_install/Installer.rs` · owner: `fnpm-linker`
- [x] **Global dirs** — global dir and bin dir precedence (env > config > FNPM_INSTALL > ~/.fnpm); own package.json · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `fnpm-config`
- [ ] **Global bin PATH warning** — shell-specific `export PATH` / `fish_add_path` / pwsh hint · bun: `src/install/PackageManager/updatePackageJSONAndInstall.rs` · owner: `cli`

## Lifecycle scripts

- [x] **Script names and order** — deps: preinstall/install/postinstall; git+root add pre/prepare/post; workspaces add prepare; root last in foreground · bun: `src/install/lockfile/Package/Scripts.rs` · owner: `fnpm-scripts`
- [x] **node-gyp default** — `binding.gyp`, no install/preinstall, gypfile not false → `node-gyp rebuild` shim; JOBS env · bun: `src/install/lockfile/Package/Scripts.rs` · owner: `fnpm-scripts`
- [x] **Script env** — npm_config_local_prefix, user agent, npm_execpath, INIT_CWD, npm_package_*, npm_lifecycle_event/script, node path · bun: `src/install/lib.rs` · owner: `fnpm-scripts`
- [x] **Script PATH** — `.bin` chain per package; untrusted ancestor bins searched last · bun: `src/install/PackageManager/PackageManagerLifecycle.rs` · owner: `fnpm-scripts`
- [x] **Script rewrite** — `npm run`/`yarn <cmd>`/`pnpm run` → `fnpm run`; `npx`/`pnpm dlx`/`pnpx` → `fnpm x`; workspace flags translated · bun: `src/install/lifecycle_script_runner.rs` · owner: `fnpm-scripts`
- [x] **Script execution** — `sh -c`; output buffered unless verbose; PDEATHSIG; failure prints output + error and exits with code · bun: `src/install/lifecycle_script_runner.rs` · owner: `fnpm-scripts`
- [x] **Slow script warning** — `<pkg>'s postinstall cost you <dur>` after 30s (5s verbose); disable env · bun: `src/install/lifecycle_script_runner.rs` · owner: `fnpm-scripts`
- [x] **Script concurrency** — max concurrent default 2×threads; trees/entries wait for deps · bun: `src/install/PackageInstaller.rs` · owner: `fnpm-scripts`
- [x] **Ignore scripts sources** — CLI, fnpm.toml, .npmrc, NPM_CONFIG_IGNORE_SCRIPTS (only turns off) · bun: `src/install/PackageManager.rs` · owner: `fnpm-scripts`
- [x] **Trust rules for non-npm deps** — folder inside declarer untrusted; trusted transitive from `--trust` requests · bun: `src/install/lockfile.rs` · owner: `fnpm-scripts`
- [x] **Blocked script reporting** — count blocked scripts; `Blocked N postinstall(s). Run \`fnpm pm untrusted\`` · bun: `src/install/PackageInstaller.rs` · owner: `fnpm-scripts`
- [x] **Postinstall optimizer** — `nativeDependencies`/`ignoreScripts` arrays; defaults esbuild native binlink, sharp ignore · bun: `src/install/postinstall_optimizer.rs` · owner: `fnpm-scripts`
- [x] **run_lifecycle_script_foreground** — run named hook for version/pack/publish with `.bin` PATH · bun: `src/runtime/cli/run_command.rs` · owner: `fnpm-scripts`

## Lockfiles & migration

- [x] **fnpm.lock text format** — JSONC, 2-space; keys lockfileVersion, configVersion, workspaces, trusted, patched, overrides, catalog(s), packageExtensions, packages tuples · bun: `src/install/lockfile/bun.lock.rs` · owner: `fnpm-lockfile`
- [x] **lockfileVersion 0–3** — strictness by version; write version selection; unsupported version error · bun: `src/install/lockfile/bun.lock.rs` · owner: `fnpm-lockfile`
- [x] **configVersion** — V0/V1, clamp, write on line 2; changed forces save · bun: `src/install/ConfigVersion.rs` · owner: `fnpm-lockfile`
- [x] **Read bun.lock / bun.lockb** — parse bun text and binary lockfiles for migration · bun: `src/install/lockfile/bun.lockb.rs` · owner: `fnpm-lockfile`
- [x] **Load priority and save format** — fnpm.lock, then bun.lock/bun.lockb, then foreign; save text by default; load errors `Ignoring lockfile` · bun: `src/install/lockfile.rs` · owner: `fnpm-lockfile`
- [ ] **When lockfile is saved** — on diff/meta hash change, requests, migration, force; empty lockfile deleted · bun: `src/install/PackageManager/install_with_manager.rs` · owner: `fnpm-core`
- [ ] **Frozen lockfile check** — compare overrides/catalog/extensions then packages; `lockfile had changes, but lockfile is frozen` + notes · bun: `src/install/PackageManager/install_with_manager.rs` · owner: `fnpm-core`
- [ ] **Lockfile-only save** — `Saved fnpm.lock (N packages)` · bun: `src/install/PackageManager/install_with_manager.rs` · owner: `fnpm-core`
- [x] **yarn.lock v1 output** — `-y`, `lockfile.print=yarn`, env · bun: `src/install/lockfile/printer/Yarn.rs` · owner: `fnpm-lockfile`
- [x] **Meta hash** — deterministic hash of resolved packages · bun: `src/install/lockfile.rs` · owner: `fnpm-lockfile`
- [x] **Migration order** — npm-shrinkwrap.json, package-lock.json, yarn.lock (v1/berry), pnpm-lock.yaml; warn and try next · bun: `src/install/migration.rs` · owner: `fnpm-lockfile`
- [x] **npm lockfile v2–4** — unsupported version falls back to package.json resolution · bun: `src/install/migration/npm_lock.rs` · owner: `fnpm-lockfile`
- [x] **yarn v1 and berry** — incl. builtin package extensions · bun: `src/install/yarn_berry.rs` · owner: `fnpm-lockfile`
- [x] **pnpm lockfile** — pnpm-workspace.yaml workspaces/catalogs/overrides moved into package.json; onlyBuiltDependencies, patched, packageExtensions migrated · bun: `src/install/pnpm.rs` · owner: `fnpm-lockfile`
- [ ] **Post-migration** — read trusted from manifests, copy patches, `migrated lockfile from <name>`; original kept · bun: `src/install/migration.rs` · owner: `fnpm-lockfile`
- [ ] **Package.json write ordering** — lockfile saved before package.json; failures exit before writes · bun: `src/install/PackageManager/package_json_write_back.rs` · owner: `fnpm-core`
- [ ] **package.json formatting preservation** — indentation, trailing newline, alphabetized dep lists, write only on change · bun: `src/install/PackageManager/WorkspacePackageJSONCache.rs` · owner: `fnpm-core`

## Workspaces & filter

- [x] **workspaces field** — array or `{packages}`; globs with negation; nameless skipped with warning; external workspaces · bun: `src/install/lockfile/Package/WorkspaceMap.rs` · owner: `fnpm-workspace`
- [x] **Duplicate workspace names** — `Workspace name "x" already exists` error · bun: `src/install/lockfile/Package.rs` · owner: `fnpm-workspace`
- [x] **Run-side discovery** — walk up to workspaces root, else `**/package.json`; skip node_modules/.git · bun: `src/runtime/cli/filter_arg.rs` · owner: `fnpm-workspace`
- [x] **--filter grammar** — `!`, `...` dependents/dependencies with `^`, `{dir}` subtree, `./path`, name globs · bun: `src/install/PackageManager/workspace_selection.rs` · owner: `fnpm-workspace`
- [x] **No-match handling** — warn per pattern; error when nothing selected unless `--if-present` · bun: `src/install/PackageManager/workspace_selection.rs` · owner: `fnpm-workspace`
- [x] **Root selection** — implicit for install/update/outdated, explicit only for add/remove · bun: `src/install/PackageManager/workspace_selection.rs` · owner: `fnpm-workspace`
- [x] **--workspaces** — all packages except root; missing script error unless `--if-present` · bun: `src/runtime/cli/filter_arg.rs` · owner: `fnpm-workspace`
- [x] **Topological order** — dependents wait on workspace deps; cycles drop ordering · bun: `src/runtime/cli/filter_run.rs` · owner: `fnpm-workspace`
- [x] **Dependency-ordered publish** — workspaces published deps first · bun: `src/install/PackageManager/workspace_manifests.rs` · owner: `fnpm-workspace`

## Security

- [ ] **Security scanner trigger** — after resolution, before linking, for all installing commands; skipped on dry run / lockfile-only · bun: `src/install/PackageManager/security_scanner.rs` · owner: `fnpm-core`
- [ ] **Scan scope** — full scan or partial (requests, moved packages, BFS) · bun: `src/install/PackageManager/security_scanner.rs` · owner: `fnpm-core`
- [ ] **Scanner payload** — `{name, version, requestedRange, tarball}` array, credentials stripped, via fd 4 · bun: `src/install/PackageManager/security_scanner.rs` · owner: `fnpm-core`
- [ ] **Scanner subprocess** — run scanner module with node; version "1" API; result/error messages on fd 3 · bun: `src/install/PackageManager/scanner-entry.ts` · owner: `fnpm-core`
- [ ] **Scanner result validation** — exact error messages for bad JSON/fields/levels/exit · bun: `src/install/PackageManager/security_scanner.rs` · owner: `fnpm-core`
- [ ] **Scanner auto-install** — install root scanner dep if missing then retry; workspace scanner dep error · bun: `src/install/PackageManager/security_scanner.rs` · owner: `fnpm-core`
- [ ] **Advisory output and prompt** — FATAL/WARNING lines with paths; fatal aborts; warnings prompt `[y/N]` on TTY else cancel · bun: `src/install/PackageManager/security_scanner.rs` · owner: `cli`
- [ ] **Scanner timing logs** — verbose `[SecurityProvider]` lines; slow scan note ≥1s · bun: `src/install/PackageManager/security_scanner.rs` · owner: `fnpm-core`
- [x] **Audit bulk request** — npm packages grouped by registry, gzip POST `/-/npm/v1/security/advisories/bulk`, merge responses · bun: `src/runtime/cli/audit_command.rs` · owner: `fnpm-registry`
- [x] **Audit registry failures** — non-default registry skipped with warning; default registry failure fatal · bun: `src/runtime/cli/audit_command.rs` · owner: `fnpm-registry`
- [x] **Integrity parsing** — strongest SRI (sha512>384>256>1), hex shasum fallback, malformed warning · bun: `src/install/integrity.rs` · owner: `fnpm-store`
- [x] **Tarball integrity verification** — hash while streaming; `Integrity check failed for tarball`; compute sha512 for unpinned tarballs · bun: `src/install/extract_tarball.rs` · owner: `fnpm-store`
- [x] **Lockfile integrity validation** — malformed hash error (v≥2) or ignored (v<2) · bun: `src/install/lockfile/bun.lock.rs` · owner: `fnpm-lockfile`
- [x] **Tarball path traversal** — npm mode files only, strip first component, skip `..`, mask modes, long paths skipped · bun: `src/install/TarballStream.rs` · owner: `fnpm-store`
- [x] **Symlink target safety** — non-npm archives: reject escaping/absolute targets, deferred creation, parent swap check · bun: `src/libarchive/lib.rs` · owner: `fnpm-store`
- [x] **No write-through symlinks** — refuse writing/deleting through symlinked scope/.bin/package/store dirs · bun: `src/install/PackageInstaller.rs` · owner: `fnpm-linker`
- [x] **Bin target escape checks** — absolute, `:`, NUL, `..` climbing, realpath/openat2 containment · bun: `src/install/bin.rs` · owner: `fnpm-linker`
- [x] **Name / folder validation** — safe install folder names, lockfile names, manifest URL must stay on registry · bun: `src/install/dependency.rs` · owner: `fnpm-types`
- [ ] **Secret redaction** — userinfo, UUIDs, npm tokens masked in URLs and config echoes; control chars escaped · bun: `src/bun_core/fmt.rs` · owner: `fnpm-types`
- [x] **pm cache rm safety** — cache dir only from trusted sources; refuse dangerous paths · bun: `src/runtime/cli/package_manager_command.rs` · owner: `fnpm-store`

## Output/UX

- [ ] **Log levels** — Default/Verbose/Silent/Quiet/NoProgress variants; verbose logs manifests, retries, tarballs · bun: `src/install/PackageManager/PackageManagerOptions.rs` · owner: `cli`
- [ ] **Progress bar** — stderr, 500ms delay, 50ms refresh; Resolving/Installing/Saving lockfile/Running script nodes; plain fallback lines · bun: `src/install/PackageManager/ProgressStrings.rs` · owner: `cli`
- [ ] **Install summary** — `installed x@v with binaries`, `+ x@v`, `↑ x a → b`, `(vX available)`, counts, no-op `Checked N installs`, failures · bun: `src/install/lockfile/printer/tree_printer.rs` · owner: `cli`
- [ ] **Time format** — `[12.34ms]` ≤1500ms else `[1.23s]` · bun: `src/bun_core/output.rs` · owner: `cli`
- [ ] **Message prefixes** — `error:` red, `warn:` yellow, `note:` blue on stderr · bun: `src/bun_core/output.rs` · owner: `cli`
- [ ] **Network and resolution messages** — `GET <url> - <status>`, download/extract/clone errors, `No version matching`, peer warnings · bun: `src/install/PackageManager/runTasks.rs` · owner: `cli`
- [ ] **Exit codes** — crash = 1; install fails on failed packages, scanner, integrity, frozen, bad flags · bun: `src/bun_core/Global.rs` · owner: `cli`
- [ ] **Control-char escaping** — all untrusted names/strings escaped in output · bun: `src/install/dependency.rs` · owner: `cli`
- [ ] **No-color fallbacks** — `^ name a -> b`, `|` tables, no emoji when no ANSI · bun: `src/install/lockfile/printer/tree_printer.rs` · owner: `cli`

## Network

- [x] **HTTP proxy** — `http(s)_proxy`/`HTTP(S)_PROXY`, `all_proxy` (http only), `DOMAIN\user:pass@`; proxy errors reported · bun: `src/dotenv/env_loader.rs` · owner: `fnpm-registry`
- [x] **NO_PROXY** — `*`, domains, subdomains, IPs, CIDR, ports · bun: `src/dotenv/no_proxy.rs` · owner: `fnpm-registry`
- [x] **TLS / CA store** — bundled/system/openssl stores, custom ca/cafile, reject-unauthorized off via env; cert errors not retried · bun: `src/bun_core/Global.rs` · owner: `fnpm-registry`
- [x] **Retries and backoff** — 5 retries on no response/5xx/429/408; 50ms base exponential capped ×40 ±20%; Retry-After ≤60s · bun: `src/install/retry_timer.rs` · owner: `fnpm-registry`
- [x] **Timeouts** — idle 300s; first timeout warns once · bun: `src/install/retry_timer.rs` · owner: `fnpm-registry`
- [x] **Concurrency** — 64 simultaneous requests; adaptive limit after repeated failures · bun: `src/install/PackageManager.rs` · owner: `fnpm-registry`
- [x] **Manifest requests** — corgi Accept; full doc for minimumReleaseAge/libc; ETag/If-Modified-Since; 304 reuse; status mapping · bun: `src/install/NetworkTask.rs` · owner: `fnpm-registry`
- [x] **Manifest cache** — on-disk per-name binary cache, max-age ≤300s, clock-skew expiry; offline/prefer-offline semantics · bun: `src/install/npm.rs` · owner: `fnpm-registry`
- [x] **Auth headers** — Bearer token or Basic `_auth`, only over http(s); `npm-auth-type: legacy` · bun: `src/install/npm.rs` · owner: `fnpm-registry`
- [x] **Scoped registry selection** — `@scope/x` uses scope registry (exact name match) · bun: `src/install/npm.rs` · owner: `fnpm-registry`
- [x] **Tarball credential scoping** — per-URL npmrc creds (https) else same-origin registry creds; URL userinfo as Basic · bun: `src/install/NetworkTask.rs` · owner: `fnpm-registry`
- [x] **Embedded URL credentials** — yarn `/:_authToken=`, JFrog `/_auth=`, `username=`/`_password=` parsed and stripped · bun: `src/install/npm.rs` · owner: `fnpm-registry`
- [ ] **User agent and npm headers** — `fnpm/<ver> npm/? node/v<ver> <os> <arch>`, npm-command, ci name, npm-notice printed · bun: `src/install/npm.rs` · owner: `fnpm-registry`
- [x] **Publish OTP and web login** — 401 handling, authUrl/doneUrl polling, OTP prompt, IP restriction message · bun: `src/runtime/cli/publish_command.rs` · owner: `fnpm-registry`
- [ ] **Cache folder naming** — `name@ver[-prehash][+BUILDHASH]@@@1`, non-default registry host suffix, `@G@`/`@GH@`, patch hash · bun: `src/install/PackageManager/PackageManagerDirectories.rs` · owner: `fnpm-store`
- [x] **Cache directory resolution** — env > config > npmrc > FNPM_INSTALL > XDG/HOME > `node_modules/.fnpm-cache` · bun: `src/install/PackageManager/PackageManagerDirectories.rs` · owner: `fnpm-store`

## Excluded (bun-runtime-only)

- Running JS/TS/HTML/Markdown files directly with `run`, `run -`, module-resolution fallback, `Cannot run <loader> files` · bun: `src/runtime/cli/run_command.rs`
- Bare `bun <script>` auto command and script-vs-builtin disambiguation (`script_or_command`, `--no-script`) · bun: `src/runtime/cli/mod.rs`
- `--bun` / `run.bun` node shim aliasing node to the runtime (fnpm still sets `NODE`/`npm_node_execpath` to real node) · bun: `src/runtime/cli/run_command.rs`
- Bun shell interpreter (`--shell bun`, `bun exec`, Windows `bun exec` for scripts); fnpm uses the system shell · bun: `src/runtime/cli/exec_command.rs`
- `--eval`/`--print`/REPL, `--watch`/`--hot`, `--check`, `--smol`, `--preload`, inspector/profiler, transpiler/loader flags · bun: `src/runtime/cli/Arguments.rs`
- Runtime auto-install (`install.auto`, `-i`/`--install`) and its scanner-based disable · bun: `src/install/auto_installer.rs`
- Standalone compiled executables and `DISABLE_AUTOLOAD_BUNFIG` · bun: `src/bunfig/arguments.rs`
- `getcompletes` JS-file filters (`g`, `j`), bun-repl env scrub, `bunx --bun`, preconnect flags · bun: `src/runtime/cli/shell_completions.rs`
- JS test binding `read_tarball` for pack tests · bun: `src/runtime/cli/pack_command.bind.ts`
