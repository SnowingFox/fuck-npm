//! bunfig / fnpm.toml `[install]` and `[run]` parsing (bun `bunfig.rs`).
use super::*;

fn parse(text: &str) -> Result<ConfigFile> {
    parse_kind(text, FileKind::User)
}

fn parse_kind(text: &str, kind: FileKind) -> Result<ConfigFile> {
    parse_config_file(
        Path::new("/proj/bunfig.toml"),
        text,
        kind,
        Some(Path::new("/home/u")),
    )
}

fn install(text: &str) -> InstallFile {
    parse(text).unwrap().install.unwrap()
}

fn err(text: &str) -> String {
    parse(text).unwrap_err().to_string()
}

#[test]
fn registry_forms() {
    let i = install("[install]\nregistry = \"https://r.example/\"\n");
    assert_eq!(
        i.registry,
        Some(RegistrySource::with_url("https://r.example/"))
    );
    let i = install("[install]\nregistry = \"http://u:p@r/\"\n");
    let r = i.registry.unwrap();
    assert_eq!(
        (r.url.as_str(), r.username.as_str(), r.password.as_str()),
        ("http://r/", "u", "p")
    );
    let i = install("[install.registry]\nurl = \"http://:tok@r/\"\n");
    let r = i.registry.unwrap();
    assert_eq!((r.url.as_str(), r.token.as_str()), ("http://r/", "tok"));
    // Keys replace the URL's credentials as a set.
    let i = install("[install.registry]\nurl = \"http://a:b@r/\"\ntoken = \"T\"\n");
    let r = i.registry.unwrap();
    assert_eq!((r.username.as_str(), r.token.as_str()), ("", "T"));
    let i =
        install("[install.registry]\nurl = \"http://r/\"\nusername = \"u\"\npassword = \"$PW\"\n");
    assert_eq!(i.registry.unwrap().password, "$PW");
    assert!(
        err("[install]\nregistry = 1\n")
            .contains("Expected registry to be a URL string or an object")
    );
    assert!(err("[install.registry]\nurl = 1\n").contains("expected string"));
    assert!(err("[install]\nregistry = \"http://r/\\n\"\n").contains("newline"));
}

#[test]
fn scopes() {
    let i = install(
        "[install.scopes]\nmyorg = { url = \"https://m/\", token = \"T\" }\n\"@other\" = \"https://o/\"\n\"\" = \"x\"\n",
    );
    let s = i.scopes.unwrap();
    assert_eq!(s.len(), 2);
    assert_eq!(s["myorg"].token, "T");
    assert_eq!(s["other"].url, "https://o/");
    assert!(err("[install]\nscopes = 1\n").contains("expected object"));
}

#[test]
fn ca_and_cafile() {
    let i = install("[install]\nca = \"PEM\"\ncafile = \"ca/x.pem\"\n");
    assert_eq!(i.ca, Some(vec!["PEM".to_owned()]));
    assert_eq!(i.cafile, Some(PathBuf::from("/proj/ca/x.pem")));
    let i = install("[install]\nca = [\"A\", \"B\"]\ncafile = \"~/x.pem\"\n");
    assert_eq!(i.ca, Some(vec!["A".to_owned(), "B".to_owned()]));
    assert_eq!(i.cafile, Some(PathBuf::from("/home/u/x.pem")));
    assert!(err("[install]\nca = [1]\n").contains("Invalid CA"));
    assert!(err("[install]\nca = 1\n").contains("Invalid CA"));
    assert!(err("[install]\ncafile = 1\n").contains("Invalid cafile"));
}

#[test]
fn booleans_and_numbers() {
    let i = install(
        "[install]\nexact = true\ndryRun = true\nproduction = true\nfrozenLockfile = true\nsaveTextLockfile = false\nignoreScripts = true\nglobalStore = false\noptional = false\npeer = false\ndev = false\nlinkWorkspacePackages = false\nhoist = false\noffline = true\nconcurrentScripts = 3\n",
    );
    assert_eq!(
        (
            i.exact,
            i.dry_run,
            i.production,
            i.frozen_lockfile,
            i.save_text_lockfile,
            i.ignore_scripts
        ),
        (
            Some(true),
            Some(true),
            Some(true),
            Some(true),
            Some(false),
            Some(true)
        )
    );
    assert_eq!(
        (i.global_store, i.optional, i.peer, i.dev),
        (Some(false), Some(false), Some(false), Some(false))
    );
    assert_eq!(
        (i.link_workspace_packages, i.hoist, i.offline),
        (Some(false), Some(false), Some(true))
    );
    assert_eq!(i.concurrent_scripts, Some(3));
    assert_eq!(
        install("[install]\nconcurrentScripts = 0\n").concurrent_scripts,
        None
    );
    // A wrong type of a boolean key is ignored, like bun.
    assert_eq!(install("[install]\nexact = \"yes\"\n").exact, None);
}

#[test]
fn linker_and_backend() {
    assert_eq!(
        install("[install]\nlinker = \"isolated\"\n").linker,
        Some(LinkerSetting::Isolated)
    );
    assert_eq!(
        install("[install]\nlinker = \"hoisted\"\n").linker,
        Some(LinkerSetting::Hoisted)
    );
    assert!(
        err("[install]\nlinker = \"pnpm\"\n")
            .contains("Expected one of \"isolated\" or \"hoisted\"")
    );
    assert!(err("[install]\nlinker = 1\n").contains("expected string"));
    assert_eq!(
        install("[install]\nbackend = \"clonefile_each_dir\"\n").backend,
        Some(Backend::Clonefile)
    );
    assert_eq!(
        install("[install]\nbackend = \"symlink\"\n").backend,
        Some(Backend::Symlink)
    );
    assert!(err("[install]\nbackend = \"x\"\n").contains("backend"));
    assert_eq!(
        install("[install]\nnetworkConcurrency = 0\n").network_concurrency,
        Some(1)
    );
    assert_eq!(
        install("[install]\nnetworkConcurrency = 128\n").network_concurrency,
        Some(128)
    );
    assert!(err("[install]\nnetworkConcurrency = 70000\n").contains("networkConcurrency"));
}

#[test]
fn lockfile_table() {
    let i = install(
        "[install.lockfile]\nprint = \"yarn\"\nsave = false\npath = \"a.lock\"\nsavePath = \"b.lock\"\n",
    );
    assert_eq!(i.save_yarn_lockfile, Some(true));
    assert_eq!(i.save_lockfile, Some(false));
    assert_eq!(i.lockfile_path, Some(PathBuf::from("a.lock")));
    assert_eq!(i.save_lockfile_path, Some(PathBuf::from("b.lock")));
    assert_eq!(
        install("[install.lockfile]\nprint = \"bun\"\n").save_yarn_lockfile,
        None
    );
    assert!(err("[install.lockfile]\nprint = \"npm\"\n").contains("only 'yarn' output"));
    assert_eq!(
        install("[install]\nsaveYarnLockfile = true\n").save_yarn_lockfile,
        Some(true)
    );
}

#[test]
fn global_dirs_are_user_only() {
    let i = install("[install]\nglobalDir = \"~/g\"\nglobalBinDir = \"../b\"\n");
    assert_eq!(i.global_dir, Some(PathBuf::from("/home/u/g")));
    assert_eq!(i.global_bin_dir, Some(PathBuf::from("/b")));
    let f = parse_kind(
        "[install]\nglobalDir = \"/g\"\nglobalBinDir = \"/b\"\n",
        FileKind::Project,
    )
    .unwrap();
    let i = f.install.unwrap();
    assert_eq!((i.global_dir, i.global_bin_dir), (None, None));
    assert_eq!(f.warnings.len(), 2);
    assert!(f.warnings[0].contains("ignoring \"globalDir\" in project bunfig.toml"));
    let i = parse_kind("[install]\nglobalDir = \"/g\"\n", FileKind::Named)
        .unwrap()
        .install
        .unwrap();
    assert_eq!(i.global_dir, Some(PathBuf::from("/g")));
}

#[test]
fn cache_forms() {
    let i = install("[install]\ncache = false\n");
    assert_eq!(
        (i.disable_cache, i.disable_manifest_cache),
        (Some(true), Some(true))
    );
    assert_eq!(install("[install]\ncache = true\n").disable_cache, None);
    assert_eq!(
        install("[install]\ncache = \"~/c\"\n").cache_dir,
        Some(PathBuf::from("/home/u/c"))
    );
    assert_eq!(
        install("[install]\ncache = \"~\"\n").cache_dir,
        Some(PathBuf::from("/home/u"))
    );
    assert_eq!(
        install("[install]\ncache = \"~user/c\"\n").cache_dir,
        Some(PathBuf::from("/proj/~user/c"))
    );
    let i = install("[install.cache]\ndir = \"../c\"\ndisable = true\ndisableManifest = true\n");
    assert_eq!(i.cache_dir, Some(PathBuf::from("/c")));
    assert_eq!(
        (i.disable_cache, i.disable_manifest_cache),
        (Some(true), Some(true))
    );
}

#[test]
fn security_scanner() {
    assert_eq!(
        install("[install.security]\nscanner = \"@acme/scan\"\n")
            .security_scanner
            .as_deref(),
        Some("@acme/scan")
    );
    assert_eq!(
        install("[install.security]\nscanner = \"./s.js\"\n")
            .security_scanner
            .as_deref(),
        Some("/proj/s.js")
    );
    let i = parse_kind(
        "[install.security]\nscanner = \"./s.js\"\n",
        FileKind::Project,
    )
    .unwrap()
    .install
    .unwrap();
    assert_eq!(i.security_scanner.as_deref(), Some("./s.js"));
    assert!(err("[install]\nsecurity = \"x\"\n").contains("Invalid security config"));
    assert!(err("[install.security]\nscanner = 1\n").contains("expected string"));
}

#[test]
fn minimum_release_age() {
    assert_eq!(
        install("[install]\nminimumReleaseAge = 259200\n").minimum_release_age_ms,
        Some(259_200_000)
    );
    assert_eq!(
        install("[install]\nminimumReleaseAge = 1.5\n").minimum_release_age_ms,
        Some(1500)
    );
    assert_eq!(
        install("[install]\nminimumReleaseAge = \"3d\"\n").minimum_release_age_ms,
        Some(259_200_000)
    );
    assert_eq!(
        install("[install]\nminimumReleaseAge = \"2 weeks\"\n").minimum_release_age_ms,
        Some(1_209_600_000)
    );
    assert_eq!(
        install("[install]\nminimumReleaseAge = \"90m\"\n").minimum_release_age_ms,
        Some(5_400_000)
    );
    assert!(err("[install]\nminimumReleaseAge = -1\n").contains("positive number"));
    assert!(err("[install]\nminimumReleaseAge = \"soon\"\n").contains("minimumReleaseAge"));
    assert!(err("[install]\nminimumReleaseAge = true\n").contains("minimumReleaseAge"));
    let i = install("[install]\nminimumReleaseAgeExcludes = [\"@my-org/*\", \"a@1.2.3\"]\n");
    assert_eq!(
        i.minimum_release_age_excludes,
        Some(vec!["@my-org/*".to_owned(), "a@1.2.3".to_owned()])
    );
    assert_eq!(
        install("[install]\nminimumReleaseAgeExcludes = []\n").minimum_release_age_excludes,
        None
    );
    assert!(err("[install]\nminimumReleaseAgeExcludes = \"a\"\n").contains("Expected array"));
    assert!(err("[install]\nminimumReleaseAgeExcludes = [1]\n").contains("expected string"));
}

#[test]
fn parse_seconds_units() {
    assert_eq!(parse_seconds("10"), Some(10.0));
    assert_eq!(parse_seconds("1.5h"), Some(5400.0));
    assert_eq!(parse_seconds("1 day"), Some(86400.0));
    assert_eq!(parse_seconds("1w"), Some(604_800.0));
    assert_eq!(parse_seconds("-5"), None);
    assert_eq!(parse_seconds("inf"), None);
    assert_eq!(parse_seconds("5y"), None);
}

#[test]
fn hoist_patterns_prefer_log_level() {
    assert_eq!(
        install("[install]\npublicHoistPattern = \"*types*\"\n").public_hoist_pattern,
        Some(vec!["*types*".to_owned()])
    );
    assert_eq!(
        install("[install]\nhoistPattern = [\"a\", \"!b\"]\n").hoist_pattern,
        Some(vec!["a".to_owned(), "!b".to_owned()])
    );
    assert!(
        err("[install]\nhoistPattern = [1]\n").contains("Expected a string or an array of strings")
    );
    assert_eq!(
        install("[install]\nprefer = \"offline\"\n").prefer,
        Some(OfflineMode::PreferOffline)
    );
    assert_eq!(
        install("[install]\nprefer = \"latest\"\n").prefer,
        Some(OfflineMode::Online)
    );
    assert!(err("[install]\nprefer = \"never\"\n").contains("Invalid prefer setting"));
    assert_eq!(
        install("[install]\nlogLevel = \"debug\"\n").log_level,
        Some(LogLevel::Verbose)
    );
    assert_eq!(
        install("[install]\nlogLevel = \"error\"\n").log_level,
        Some(LogLevel::Quiet)
    );
    assert!(err("[install]\nlogLevel = \"loud\"\n").contains("Invalid log level"));
}

#[test]
fn run_section() {
    let f = parse("[run]\nsilent = true\n\"elide-lines\" = 5\nshell = \"system\"\nbun = true\nnoOrphans = true\n").unwrap();
    assert!(f.install.is_none());
    assert_eq!(
        f.run,
        RunFile {
            use_system_shell: Some(true),
            silent: Some(true),
            elide_lines: Some(5),
            bun: Some(true),
            no_orphans: Some(true)
        }
    );
    let f = parse("[run]\nshell = \"bun\"\n").unwrap();
    assert_eq!(f.run.use_system_shell, Some(true));
    assert_eq!(f.warnings.len(), 1);
    assert!(err("[run]\nshell = \"zsh\"\n").contains("Invalid shell"));
    assert!(err("[run]\nshell = 1\n").contains("Expected string"));
    assert!(err("[run]\nsilent = 1\n").contains("Expected boolean"));
    assert!(err("[run]\n\"elide-lines\" = \"x\"\n").contains("Expected number"));
}

#[test]
fn formats_and_errors() {
    assert!(err("[install\n").contains("Failed to parse"));
    let f = parse_config_file(
        Path::new("/p/fnpm.json"),
        "// comment\n{ \"install\": { /* c */ \"exact\": true, \"linker\": \"isolated\", }, }\n",
        FileKind::User,
        None,
    )
    .unwrap();
    assert_eq!(f.install.unwrap().exact, Some(true));
    let e = parse_config_file(Path::new("/p/fnpm.json"), "[1]", FileKind::User, None).unwrap_err();
    assert!(e.to_string().contains("expects an object"));
    assert_eq!(parse("install = 1\n").unwrap().install, None);
    assert_eq!(
        strip_jsonc("{\"a\": \"//not a comment\", \"b\": [1,2,],}"),
        "{\"a\": \"//not a comment\", \"b\": [1,2]}"
    );
}

#[test]
fn overlay_rules() {
    let mut base =
        install("[install]\nexact = true\nca = \"A\"\n[install.scopes]\na = \"https://a/\"\n");
    base.overlay(install(
        "[install]\ncafile = \"/x.pem\"\n[install.scopes]\nb = \"https://b/\"\n",
    ));
    assert_eq!(base.exact, Some(true));
    assert_eq!(base.ca, None);
    assert_eq!(base.cafile, Some(PathBuf::from("/x.pem")));
    assert_eq!(base.scopes.as_ref().unwrap().len(), 2);
    base.overlay(install(
        "[install]\nexact = false\n[install.scopes]\na = \"https://a2/\"\n",
    ));
    assert_eq!(base.exact, Some(false));
    assert_eq!(base.scopes.unwrap()["a"].url, "https://a2/");
}

#[test]
fn load_files() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        load_config_file(&dir.path().join("missing.toml"), FileKind::User, None)
            .unwrap()
            .is_none()
    );
    assert!(load_config_file(&dir.path().join("missing.toml"), FileKind::Named, None).is_err());
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"name":"x","fnpm":{"exact":true,"globalDir":"/g"}}"#,
    )
    .unwrap();
    let f = load_package_json_config(dir.path(), None).unwrap().unwrap();
    assert_eq!(f.install.as_ref().unwrap().exact, Some(true));
    assert_eq!(f.install.unwrap().global_dir, None);
    std::fs::write(dir.path().join("package.json"), "not json").unwrap();
    assert!(
        load_package_json_config(dir.path(), None)
            .unwrap()
            .is_none()
    );
    std::fs::write(dir.path().join("bunfig.toml"), "[install]\nexact = true\n").unwrap();
    assert_eq!(
        load_install_table(&dir.path().join("bunfig.toml"))
            .unwrap()
            .unwrap()
            .exact,
        Some(true)
    );
}

#[test]
fn candidates() {
    let c = config_file_candidates(Path::new("/p"), Path::new("/h"), Some(Path::new("/x")));
    assert_eq!(
        c,
        [
            PathBuf::from("/x/.bunfig.toml"),
            PathBuf::from("/p/bunfig.toml"),
            PathBuf::from("/x/.fnpm.toml"),
            PathBuf::from("/h/.fnpm/fnpm.toml"),
            PathBuf::from("/p/fnpm.toml")
        ]
    );
    assert_eq!(
        config_file_candidates(Path::new("/p"), Path::new("/h"), Some(Path::new("rel")))[0],
        PathBuf::from("/h/.bunfig.toml")
    );
    let g = global_config_candidates(Some(Path::new("/h")), None, Path::new("/f"));
    assert_eq!(
        g,
        [
            PathBuf::from("/h/.bunfig.toml"),
            PathBuf::from("/h/.fnpm.toml"),
            PathBuf::from("/f/fnpm.toml")
        ]
    );
}

#[test]
fn resolve_paths() {
    let home = Some(Path::new("/h"));
    assert_eq!(resolve_path(Path::new("/d"), "", home), PathBuf::from(""));
    assert_eq!(
        resolve_path(Path::new("/d"), "/abs", home),
        PathBuf::from("/abs")
    );
    assert_eq!(
        resolve_path(Path::new("rel"), "x", home),
        PathBuf::from("x")
    );
    assert_eq!(
        resolve_path(Path::new("/d/e"), "../x/./y", home),
        PathBuf::from("/d/x/y")
    );
    assert_eq!(
        resolve_path(Path::new("/d"), "~/x", None),
        PathBuf::from("/d/~/x")
    );
}
