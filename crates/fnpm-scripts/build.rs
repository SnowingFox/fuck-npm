//! Compiles `data/default-trusted-dependencies.txt` into a static perfect-hash set
//! (hash-and-displace, as in the `phf` crate) plus the sorted list for `fnpm pm default-trusted`.

use std::fmt::Write as _;
use std::path::PathBuf;

include!("src/phf_hash.rs");

/// Average keys per bucket.
const LAMBDA: usize = 4;

fn main() {
    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let data_path = manifest.join("data/default-trusted-dependencies.txt");
    println!("cargo::rerun-if-changed={}", data_path.display());
    println!("cargo::rerun-if-changed=src/phf_hash.rs");
    let data = std::fs::read_to_string(&data_path).expect("read default-trusted-dependencies.txt");

    // bun: `tokenize_any(DATA, " \r\n\t")`, then sorted.
    let mut names: Vec<&str> = data
        .split([' ', '\r', '\n', '\t'])
        .filter(|s| !s.is_empty())
        .collect();
    names.sort_unstable();
    names.dedup();
    assert!(
        !names.is_empty() && names.len() <= 512,
        "default trusted list must have 1..=512 names"
    );

    let (seed, disps, slots) = (0u64..)
        .find_map(|seed| try_build(&names, seed).map(|(d, s)| (seed, d, s)))
        .expect("a perfect hash exists for some seed");

    let mut out = String::new();
    writeln!(out, "pub(crate) const PHF_SEED: u64 = {seed};").unwrap();
    writeln!(
        out,
        "pub(crate) static PHF_DISPS: [(u32, u32); {}] = [",
        disps.len()
    )
    .unwrap();
    for (d1, d2) in &disps {
        writeln!(out, "    ({d1}, {d2}),").unwrap();
    }
    out.push_str("];\n");
    writeln!(
        out,
        "pub(crate) static PHF_KEYS: [&str; {}] = [",
        slots.len()
    )
    .unwrap();
    for &i in &slots {
        writeln!(out, "    {:?},", names[i]).unwrap();
    }
    out.push_str("];\n");
    writeln!(out, "pub(crate) static SORTED: [&str; {}] = [", names.len()).unwrap();
    for name in &names {
        writeln!(out, "    {name:?},").unwrap();
    }
    out.push_str("];\n");

    let out_path =
        PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("default_trusted.rs");
    std::fs::write(out_path, out).expect("write default_trusted.rs");
}

/// Per-bucket displacements and, per slot, the index of the name stored there.
type Table = (Vec<(u32, u32)>, Vec<usize>);

fn try_build(names: &[&str], seed: u64) -> Option<Table> {
    let len = names.len() as u32;
    let buckets_len = names.len().div_ceil(LAMBDA);
    let hashes: Vec<(u32, u32, u32)> = names
        .iter()
        .map(|n| phf_split(phf_hash(n.as_bytes(), seed)))
        .collect();
    let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); buckets_len];
    for (i, &(g, _, _)) in hashes.iter().enumerate() {
        buckets[g as usize % buckets_len].push(i);
    }
    let mut order: Vec<usize> = (0..buckets_len).collect();
    order.sort_by_key(|&b| std::cmp::Reverse(buckets[b].len()));

    let mut disps = vec![(0u32, 0u32); buckets_len];
    let mut slots: Vec<Option<usize>> = vec![None; names.len()];
    let mut taken: Vec<u32> = Vec::new();
    for b in order {
        let keys = &buckets[b];
        if keys.is_empty() {
            continue;
        }
        let fits = |d1: u32, d2: u32, taken: &mut Vec<u32>| {
            taken.clear();
            for &k in keys {
                let (_, f1, f2) = hashes[k];
                let slot = phf_slot(f1, f2, d1, d2, len);
                if slots[slot as usize].is_some() || taken.contains(&slot) {
                    return false;
                }
                taken.push(slot);
            }
            true
        };
        let found = (0..len)
            .flat_map(|d1| (0..len).map(move |d2| (d1, d2)))
            .find(|&(d1, d2)| fits(d1, d2, &mut taken));
        let (d1, d2) = found?;
        for (j, &k) in keys.iter().enumerate() {
            slots[taken[j] as usize] = Some(k);
        }
        disps[b] = (d1, d2);
    }
    Some((
        disps,
        slots
            .into_iter()
            .map(|s| s.expect("every slot is filled"))
            .collect(),
    ))
}
