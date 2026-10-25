use fnpm_types::spec::{
    Committish, DependencySpec, GitSpec, GithubSpec, SpecError, SpecTag, TarballUri, WorkspaceSpec,
    infer_tag, is_scp_like_path, link_path_for_lockfile, nameless_npm_alias, without_npm_protocol,
};
use fnpm_types::{Behavior, Dependency, HostProvider};

fn parse(alias: &str, lit: &str) -> DependencySpec {
    DependencySpec::parse(alias, lit).unwrap_or_else(|e| panic!("{lit}: {e}"))
}

fn npm(name: &str, range: &str, is_alias: bool) -> DependencySpec {
    DependencySpec::Npm {
        name: name.into(),
        range: range.into(),
        is_alias,
    }
}

#[test]
fn infer_npm_ranges() {
    for lit in [
        "^1.2.3",
        "~1.2.3",
        ">=1",
        "<2",
        "=1.0.0",
        "*",
        "1.2.3",
        "1",
        "1.x",
        "x",
        "X",
        "x.x",
        "1.2.3 - 2",
        "1 || 2",
        "|| 1.x",
        "v1.2.3",
        "~1",
        "1.0.0-beta.1",
        "patch:foo@1",
        "npm:foo@^1",
        "npm:foo",
    ] {
        assert_eq!(infer_tag(lit), SpecTag::Npm, "{lit}");
    }
}

#[test]
fn infer_dist_tags() {
    for lit in [
        "",
        "latest",
        "beta",
        "next",
        "2x",
        "1beta",
        "v",
        "vx",
        "v2x",
        "sshlatest",
        "httplatest",
        "foo/bar/baz",
        "npm:foo@latest",
    ] {
        assert_eq!(infer_tag(lit), SpecTag::DistTag, "{lit:?}");
    }
}

#[test]
fn infer_tarballs_and_folders() {
    for lit in [
        "https://registry.npmjs.org/a/-/a-1.0.0.tgz",
        "http://example.com/foo",
        "./foo.tgz",
        "file:foo.tgz",
        "foo.tgz",
        "/abs/foo.tar.gz",
        "file:///abs/x.tgz",
        "https://github.com/user/repo/tarball/main",
        "https://github.com/user/repo.tar.gz",
        "123.tar.gz",
        "verilog.tar.gz",
        "~/x.tgz",
    ] {
        assert_eq!(infer_tag(lit), SpecTag::Tarball, "{lit}");
    }
    for lit in [
        "./foo",
        "../bar",
        "/abs",
        "~/x",
        "file:../x",
        "file:x",
        "file://../x",
        ".",
        "..",
    ] {
        assert_eq!(infer_tag(lit), SpecTag::Folder, "{lit}");
    }
    assert_eq!(infer_tag("link:../x"), SpecTag::Symlink);
    assert_eq!(infer_tag("workspace:*"), SpecTag::Workspace);
    assert_eq!(infer_tag("catalog:"), SpecTag::Catalog);
    assert_eq!(infer_tag("catalog:react18"), SpecTag::Catalog);
}

#[test]
fn infer_git_and_github() {
    for lit in [
        "git+ssh://git@github.com:user/repo.git#v1",
        "git+ssh://git@example.com/repo.git",
        "git+file:///path/to/repo",
        "git+git://example.com/repo",
        "ssh://git@host/repo.git",
        "git@github.com:user/repo.git",
        "git@example.com:path/to/repo.git",
        "git+https://gitlab.com/a/b.git",
        "gitlab:user/repo",
        "bitbucket:user/repo",
        "git://example.com/repo.git",
        "virt@example.com:repo.git",
    ] {
        assert_eq!(infer_tag(lit), SpecTag::Git, "{lit}");
    }
    for lit in [
        "github:user/repo",
        "user/repo",
        "user/repo#main",
        "https://github.com/user/repo",
        "git://github.com/user/repo",
        "git+https://github.com/user/repo.git",
        "git+http://github.com/user/repo#v1",
        "github:user/repo#semver:^1.0",
        "verilog/repo",
    ] {
        assert_eq!(infer_tag(lit), SpecTag::Github, "{lit}");
    }
}

#[test]
fn parse_npm_and_aliases() {
    assert_eq!(parse("foo", "^1.2.3"), npm("foo", "^1.2.3", false));
    assert_eq!(parse("foo", "v1.2.3"), npm("foo", "1.2.3", false));
    assert_eq!(parse("foo", "  ^1"), npm("foo", "^1", false));
    assert_eq!(parse("bar", "npm:foo@^1"), npm("foo", "^1", true));
    assert_eq!(
        parse("bar", "npm:@scope/pkg@1.0.0"),
        npm("@scope/pkg", "1.0.0", true)
    );
    assert_eq!(parse("bar", "npm:foo"), npm("foo", "", true));
    assert_eq!(parse("foo", "npm:^1.0.0"), npm("foo", "^1.0.0", false));
    assert_eq!(parse("foo", "npm:1.0.0"), npm("foo", "1.0.0", false));
    assert_eq!(parse("foo", "npm:1"), npm("1", "", true));
    assert_eq!(
        parse("bar", "npm:foo@latest"),
        DependencySpec::DistTag {
            name: "foo".into(),
            tag: "latest".into(),
            is_alias: true
        }
    );
    assert_eq!(
        parse("foo", ""),
        DependencySpec::DistTag {
            name: "foo".into(),
            tag: "latest".into(),
            is_alias: false
        }
    );
    assert_eq!(
        parse("foo", "beta"),
        DependencySpec::DistTag {
            name: "foo".into(),
            tag: "beta".into(),
            is_alias: false
        }
    );
    assert_eq!(parse("bar", "npm:foo@^1").registry_name("bar"), "foo");
    assert!(parse("bar", "npm:foo@^1").is_alias());
    assert_eq!(without_npm_protocol("npm:^1.0.0"), "^1.0.0");
    assert_eq!(without_npm_protocol("npm:foo@^1"), "npm:foo@^1");
    assert_eq!(nameless_npm_alias("npm:@1.0.0"), Some("@1.0.0"));
    assert_eq!(nameless_npm_alias("npm:foo@1"), None);
}

#[test]
fn parse_tarballs() {
    let url = "https://registry.npmjs.org/a/-/a-1.0.0.tgz";
    assert_eq!(
        parse("a", url),
        DependencySpec::Tarball {
            uri: TarballUri::Remote(url.into())
        }
    );
    assert_eq!(
        parse("a", "file:x.tgz"),
        DependencySpec::Tarball {
            uri: TarballUri::Local("x.tgz".into())
        }
    );
    assert_eq!(
        parse("a", "file:///abs/x.tgz"),
        DependencySpec::Tarball {
            uri: TarballUri::Local("/abs/x.tgz".into())
        }
    );
    assert_eq!(
        parse("a", "./x.tgz"),
        DependencySpec::Tarball {
            uri: TarballUri::Local("./x.tgz".into())
        }
    );
    assert!(matches!(
        DependencySpec::parse_with_tag("a", "ftp://host/x.tgz", SpecTag::Tarball),
        Err(SpecError::InvalidTarball(_))
    ));
}

#[test]
fn parse_folders_links_workspaces_catalogs() {
    let folder = |p: &str| DependencySpec::Folder { path: p.into() };
    assert_eq!(parse("a", "./foo"), folder("./foo"));
    assert_eq!(parse("a", "file:../x"), folder("../x"));
    assert_eq!(parse("a", "file:x"), folder("x"));
    assert_eq!(parse("a", "file://../x"), folder("../x"));
    assert_eq!(parse("a", "file:///../x"), folder("../x"));
    assert_eq!(parse("a", "file:/abs"), folder("/abs"));
    assert!(matches!(
        DependencySpec::parse_with_tag("a", "foo:bar", SpecTag::Folder),
        Err(SpecError::UnsupportedProtocol(_))
    ));
    assert_eq!(
        parse("a", "link:../x"),
        DependencySpec::Symlink {
            path: "../x".into()
        }
    );
    let ws = |s: WorkspaceSpec| DependencySpec::Workspace { spec: s };
    assert_eq!(parse("a", "workspace:*"), ws(WorkspaceSpec::Any));
    assert_eq!(parse("a", "workspace:"), ws(WorkspaceSpec::Any));
    assert_eq!(parse("a", "workspace:^"), ws(WorkspaceSpec::Caret));
    assert_eq!(parse("a", "workspace:~"), ws(WorkspaceSpec::Tilde));
    assert_eq!(
        parse("a", "workspace:1.2.3"),
        ws(WorkspaceSpec::Range("1.2.3".into()))
    );
    assert_eq!(
        parse("a", "workspace:../x"),
        ws(WorkspaceSpec::Path("../x".into()))
    );
    assert_eq!(WorkspaceSpec::Caret.publish_spec("1.2.3"), "^1.2.3");
    assert_eq!(WorkspaceSpec::Any.publish_spec("1.2.3"), "1.2.3");
    assert_eq!(
        WorkspaceSpec::Range("^2".into()).publish_spec("2.1.0"),
        "^2"
    );
    assert_eq!(
        parse("a", "catalog:"),
        DependencySpec::Catalog { catalog: "".into() }
    );
    assert_eq!(
        parse("a", "catalog: react18 "),
        DependencySpec::Catalog {
            catalog: "react18".into()
        }
    );
}

#[test]
fn parse_git() {
    let DependencySpec::Git(GitSpec {
        url,
        committish,
        hosted,
    }) = parse("a", "git+ssh://git@github.com:user/repo.git#v1")
    else {
        panic!()
    };
    assert_eq!(url, "ssh://git@github.com:user/repo.git");
    assert_eq!(committish.reference.as_deref(), Some("v1"));
    assert_eq!(hosted.unwrap().provider, HostProvider::Github);

    let DependencySpec::Git(g) = parse(
        "a",
        "git+https://gitlab.com/a/b.git#semver:^1.2::path:packages/x",
    ) else {
        panic!()
    };
    assert_eq!(g.url, "https://gitlab.com/a/b.git");
    assert_eq!(g.committish.semver.as_deref(), Some("^1.2"));
    assert_eq!(g.committish.path.as_deref(), Some("packages/x"));
    assert_eq!(g.committish.reference, None);
    assert_eq!(g.hosted.unwrap().provider, HostProvider::Gitlab);

    let DependencySpec::Git(g) = parse("a", "git+file:///path/to/repo") else {
        panic!()
    };
    assert_eq!(g.url, "file:///path/to/repo");
    assert!(g.committish.is_empty());
    assert!(g.hosted.is_none());

    let DependencySpec::Git(g) = parse("a", "git@example.com:path/repo.git#feature/x") else {
        panic!()
    };
    assert_eq!(g.url, "git@example.com:path/repo.git");
    assert_eq!(g.committish.reference.as_deref(), Some("feature/x"));
}

#[test]
fn parse_github() {
    let gh = |owner: &str, repo: &str, c: &str| {
        DependencySpec::Github(GithubSpec {
            owner: owner.into(),
            repo: repo.into(),
            committish: Committish::parse(c),
        })
    };
    assert_eq!(parse("a", "user/repo"), gh("user", "repo", ""));
    assert_eq!(parse("a", "user/repo#main"), gh("user", "repo", "main"));
    assert_eq!(
        parse("a", "github:user/repo#v1.0.0"),
        gh("user", "repo", "v1.0.0")
    );
    assert_eq!(
        parse("a", "https://github.com/user/repo"),
        gh("user", "repo", "")
    );
    // A deep github URL is not a shortcut representation, so bun classifies it as git.
    assert_eq!(
        infer_tag("https://github.com/user/repo/tree/dev"),
        SpecTag::Git
    );
    assert_eq!(
        DependencySpec::parse_with_tag(
            "a",
            "https://github.com/user/repo/tree/dev",
            SpecTag::Github
        )
        .unwrap(),
        gh("user", "repo", "dev")
    );
    assert_eq!(
        parse("a", "git+https://github.com/user/repo.git#abc123"),
        gh("user", "repo", "abc123")
    );
    let DependencySpec::Github(g) = parse("a", "github:user/repo#semver:^1.0") else {
        panic!()
    };
    assert_eq!(g.committish.semver.as_deref(), Some("^1.0"));
    assert!(
        DependencySpec::parse_with_tag("a", "github:user/repo#a\u{1b}b", SpecTag::Github).is_err()
    );
    assert!(DependencySpec::parse_with_tag("a", "not a url", SpecTag::Github).is_err());
}

#[test]
fn committish_roundtrip() {
    for raw in [
        "",
        "main",
        "semver:^1",
        "main::path:sub",
        "::path:sub",
        "semver:^1::path:a/b",
    ] {
        assert_eq!(Committish::parse(raw).to_raw(), raw, "{raw}");
    }
}

#[test]
fn helpers() {
    assert!(is_scp_like_path("git@host:path"));
    assert!(is_scp_like_path("host:path"));
    assert!(!is_scp_like_path("https://host/path"));
    // bun treats `user@host/path` as scp-like too.
    assert!(is_scp_like_path("a@b/c"));
    assert!(!is_scp_like_path("a/b:c"));
    assert_eq!(link_path_for_lockfile("pkgs/a"), "./pkgs/a");
    assert_eq!(link_path_for_lockfile("../a"), "../a");
    assert_eq!(link_path_for_lockfile(""), ".");
    let d = Dependency::parse("bar", " npm:foo@^2", Behavior::DEV).unwrap();
    assert_eq!(d.literal, "npm:foo@^2");
    assert_eq!(d.registry_name(), "foo");
}

#[test]
fn serde_shapes() {
    let s = parse("bar", "npm:foo@^1");
    let j = serde_json::to_string(&s).unwrap();
    assert_eq!(
        j,
        r#"{"type":"npm","name":"foo","range":"^1","is_alias":true}"#
    );
    assert_eq!(serde_json::from_str::<DependencySpec>(&j).unwrap(), s);
    let w = parse("a", "workspace:^");
    assert_eq!(
        serde_json::from_str::<DependencySpec>(&serde_json::to_string(&w).unwrap()).unwrap(),
        w
    );
}
