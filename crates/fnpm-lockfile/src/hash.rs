//! The lockfile meta hash (bun `Lockfile::generate_meta_hash`, `fnpm pm hash`,
//! `pm hash-string`, `pm hash-print`): SHA-512/256 over every package's `name@resolution`,
//! alphabetised.

use sha2::{Digest, Sha512_256};

use fnpm_types::{LockGraph, Resolution};

use crate::flat::resolution_order;

/// A meta hash. All zeros for a lockfile with no packages besides the root.
pub type MetaHash = [u8; 32];

const HASH_PREFIX: &str = "\n-- BEGIN SHA512/256(`${alphabetize(name)}@${order(version)}`) --\n";
const HASH_SUFFIX: &str = "-- END HASH--\n";

/// bun's resolution text (`Resolution::fmt`): versions, paths, `workspace:` and `link:` paths,
/// git labels and URLs.
pub(crate) fn resolution_text(r: &Resolution) -> String {
    match r {
        Resolution::Root => String::new(),
        Resolution::Npm { version, .. } => version.clone(),
        Resolution::LocalTarball { path, .. } | Resolution::Folder { path } => path.clone(),
        Resolution::RemoteTarball { url, .. } => url.clone(),
        Resolution::Git { .. } | Resolution::Github { .. } => {
            crate::text::write::repository_label(r)
        }
        Resolution::Workspace { path } => format!("workspace:{path}"),
        Resolution::Symlink { path } => format!("link:{path}"),
    }
}

/// The text that is hashed (`pm hash-string`).
pub fn meta_hash_input(graph: &LockGraph) -> String {
    if graph.packages.len() <= 1 {
        return String::new();
    }
    let mut ids: Vec<usize> = (1..graph.packages.len()).collect();
    ids.sort_unstable_by(|&a, &b| {
        let (pa, pb) = (&graph.packages[a], &graph.packages[b]);
        pa.name
            .as_bytes()
            .cmp(pb.name.as_bytes())
            .then_with(|| resolution_order(&pa.resolution, &pb.resolution))
    });
    let mut s = String::with_capacity(HASH_PREFIX.len() + HASH_SUFFIX.len() + ids.len() * 32);
    s.push_str(HASH_PREFIX);
    for i in ids {
        let p = &graph.packages[i];
        s.push_str(&p.name);
        s.push('@');
        s.push_str(&resolution_text(&p.resolution));
        s.push('\n');
    }
    s.push_str(HASH_SUFFIX);
    s
}

/// The meta hash of a graph.
pub fn meta_hash(graph: &LockGraph) -> MetaHash {
    if graph.packages.len() <= 1 {
        return [0; 32];
    }
    let mut h = Sha512_256::new();
    h.update(meta_hash_input(graph).as_bytes());
    h.finalize().into()
}

/// bun's display form: four 8-byte groups, alternating upper and lower case hex.
pub fn format_meta_hash(hash: &MetaHash) -> String {
    let mut s = String::with_capacity(67);
    for (i, chunk) in hash.chunks(8).enumerate() {
        if i != 0 {
            s.push('-');
        }
        for b in chunk {
            if i % 2 == 0 {
                s.push_str(&format!("{b:02X}"));
            } else {
                s.push_str(&format!("{b:02x}"));
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use fnpm_types::Package;

    #[test]
    fn hash_is_order_independent_and_zero_for_empty() {
        let mut g = LockGraph::new(Package::new("app", "", Resolution::Root));
        assert_eq!(meta_hash(&g), [0; 32]);
        g.push(Package::new("b", "1.0.0", Resolution::npm("1.0.0", None)));
        g.push(Package::new("a", "2.0.0", Resolution::npm("2.0.0", None)));
        let mut h = LockGraph::new(Package::new("app", "", Resolution::Root));
        h.push(Package::new("a", "2.0.0", Resolution::npm("2.0.0", None)));
        h.push(Package::new("b", "1.0.0", Resolution::npm("1.0.0", None)));
        assert_eq!(meta_hash(&g), meta_hash(&h));
        assert_eq!(
            meta_hash_input(&g),
            "\n-- BEGIN SHA512/256(`${alphabetize(name)}@${order(version)}`) --\na@2.0.0\nb@1.0.0\n-- END HASH--\n"
        );
        let f = format_meta_hash(&meta_hash(&g));
        assert_eq!(f.len(), 67);
        assert_eq!(&f[16..17], "-");
    }
}
