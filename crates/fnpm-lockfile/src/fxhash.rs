//! A small non-cryptographic hasher (the rustc "Fx" hash) for the in-memory maps of the
//! lockfile algorithms. Keys are package names, paths and small integers from one lockfile;
//! HashDoS resistance is not needed and SipHash showed up in profiles.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

#[derive(Default, Clone, Copy)]
pub(crate) struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for c in chunks {
            self.add(u64::from_le_bytes(*c));
        }
        if !rest.is_empty() {
            let mut buf = [0u8; 8];
            buf[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(buf));
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i));
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

pub(crate) type FxBuild = BuildHasherDefault<FxHasher>;
pub(crate) type FxHashMap<K, V> = HashMap<K, V, FxBuild>;
pub(crate) type FxHashSet<K> = HashSet<K, FxBuild>;

#[inline]
pub(crate) fn map<K, V>(capacity: usize) -> FxHashMap<K, V> {
    FxHashMap::with_capacity_and_hasher(capacity, FxBuild::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_keys() {
        let mut m: FxHashMap<&str, u32> = map(4);
        for (i, k) in ["a", "b", "ab", "ba", "", "abcdefghi", "abcdefghj"]
            .iter()
            .enumerate()
        {
            m.insert(k, i as u32);
        }
        assert_eq!(m.len(), 7);
        assert_eq!(m["abcdefghj"], 6);
    }
}
