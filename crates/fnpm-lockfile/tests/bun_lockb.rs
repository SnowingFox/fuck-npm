//! bun.lockb written by fnpm: read back to the same graph, and byte for byte what bun wrote for
//! the same install (fixtures from bun `test/cli/install/fixtures`).

use fnpm_lockfile::formats::bun_binary;

fn fixture(name: &str) -> Vec<u8> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bun-lockb");
    std::fs::read(dir.join(name)).unwrap()
}

#[test]
fn rewrites_bun_lockb_as_bun_does() {
    let bytes = fixture("unnamed-folder-dependency.lockb");
    let graph = bun_binary::parse(&bytes).unwrap();
    assert_eq!(bun_binary::serialize(&graph).unwrap(), bytes);
}

#[test]
fn reads_back_what_it_writes() {
    for name in [
        "unnamed-folder-dependency.lockb",
        "workspace-name-dist-tag.lockb",
    ] {
        let graph = bun_binary::parse(&fixture(name)).unwrap();
        let written = bun_binary::serialize(&graph).unwrap();
        assert_eq!(bun_binary::parse(&written).unwrap(), graph, "{name}");
        assert_eq!(
            bun_binary::serialize(&graph).unwrap(),
            written,
            "{name}: deterministic"
        );
    }
}
