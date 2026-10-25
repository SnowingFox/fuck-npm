// Shared by `build.rs` (which `include!`s this file) and the runtime lookup in `trust.rs`.

/// 64-bit FNV-1a over `key` with a seed, finished with the splitmix64 mixer.
#[inline]
pub(crate) const fn phf_hash(key: &[u8], seed: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ seed;
    let mut i = 0;
    while i < key.len() {
        h ^= key[i] as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// Splits a hash into `(bucket selector, f1, f2)` for the hash-and-displace scheme.
#[inline]
pub(crate) const fn phf_split(h: u64) -> (u32, u32, u32) {
    (
        (h >> 40) as u32,
        ((h >> 20) & 0xf_ffff) as u32,
        (h & 0xf_ffff) as u32,
    )
}

/// The slot of a key: `(f1 + d1 * f2 + d2) % len`.
#[inline]
pub(crate) const fn phf_slot(f1: u32, f2: u32, d1: u32, d2: u32, len: u32) -> u32 {
    ((f1 as u64 + d1 as u64 * f2 as u64 + d2 as u64) % len as u64) as u32
}
