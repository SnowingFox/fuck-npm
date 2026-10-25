//! bun's hashes and its hash-map iteration order, needed to write bun.lock byte-for-byte.
//!
//! bun writes `trustedDependencies` and `patchedDependencies` in the slot order of a Zig
//! `std.AutoHashMap(u64)`: keys are package-name hashes (`Wyhash11`), slots are chosen by
//! the final4 `Wyhash` of the key's little-endian bytes (bun `WrittenOrderContext`).

const PRIMES: [u64; 5] = [
    0xa0761d6478bd642f,
    0xe7037ed1a0b428db,
    0x8ebc6af09c88c6e3,
    0x589965cc75374cc3,
    0x1d8e4e27c47d124f,
];

#[inline]
fn rd(data: &[u8], n: usize) -> u64 {
    let mut v = 0u64;
    for (i, &b) in data[..n].iter().enumerate() {
        v |= u64::from(b) << (8 * i);
    }
    v
}

#[inline]
fn rd8swapped(data: &[u8]) -> u64 {
    (rd(data, 4) << 32) | rd(&data[4..], 4)
}

#[inline]
fn mum(a: u64, b: u64) -> u64 {
    let r = u128::from(a) * u128::from(b);
    ((r >> 64) ^ r) as u64
}

#[inline]
fn mix0(a: u64, b: u64, seed: u64) -> u64 {
    mum(a ^ seed ^ PRIMES[0], b ^ seed ^ PRIMES[1])
}

#[inline]
fn mix1(a: u64, b: u64, seed: u64) -> u64 {
    mum(a ^ seed ^ PRIMES[2], b ^ seed ^ PRIMES[3])
}

/// The tail value of 1..=15 bytes as bun's `WyhashStateless::final_` reads it.
fn tail_word(k: &[u8]) -> (u64, u64) {
    match k.len() {
        1 => (rd(k, 1), PRIMES[4]),
        2 => (rd(k, 2), PRIMES[4]),
        3 => ((rd(k, 2) << 8) | rd(&k[2..], 1), PRIMES[4]),
        4 => (rd(k, 4), PRIMES[4]),
        5 => ((rd(k, 4) << 8) | rd(&k[4..], 1), PRIMES[4]),
        6 => ((rd(k, 4) << 16) | rd(&k[4..], 2), PRIMES[4]),
        7 => (
            (rd(k, 4) << 24) | (rd(&k[4..], 2) << 8) | rd(&k[6..], 1),
            PRIMES[4],
        ),
        8 => (rd8swapped(k), PRIMES[4]),
        9 => (rd8swapped(k), rd(&k[8..], 1)),
        10 => (rd8swapped(k), rd(&k[8..], 2)),
        11 => (rd8swapped(k), (rd(&k[8..], 2) << 8) | rd(&k[10..], 1)),
        12 => (rd8swapped(k), rd(&k[8..], 4)),
        13 => (rd8swapped(k), (rd(&k[8..], 4) << 8) | rd(&k[12..], 1)),
        14 => (rd8swapped(k), (rd(&k[8..], 4) << 16) | rd(&k[12..], 2)),
        15 => (
            rd8swapped(k),
            (rd(&k[8..], 4) << 24) | (rd(&k[12..], 2) << 8) | rd(&k[14..], 1),
        ),
        16 => (rd8swapped(k), rd8swapped(&k[8..])),
        _ => unreachable!("tail_word takes 1..=16 bytes"),
    }
}

/// bun `Wyhash11::hash` (the legacy wyhash used for package-name hashes).
pub(crate) fn wyhash11(seed: u64, input: &[u8]) -> u64 {
    let aligned = input.len() - input.len() % 32;
    let mut s = seed;
    for chunk in input[..aligned].as_chunks::<32>().0 {
        s = mix0(rd(chunk, 8), rd(&chunk[8..], 8), s)
            ^ mix1(rd(&chunk[16..], 8), rd(&chunk[24..], 8), s);
    }
    let rem = &input[aligned..];
    let s = match rem.len() {
        0 => s,
        1..=16 => {
            let (a, b) = tail_word(rem);
            mix0(a, b, s)
        }
        _ => {
            let head = mix0(rd8swapped(rem), rd8swapped(&rem[8..]), s);
            let t = &rem[16..];
            let tail = if t.len() <= 7 {
                let (a, _) = tail_word(t);
                mix1(a, PRIMES[4], s)
            } else {
                let (a, b) = tail_word(t);
                mix1(a, if t.len() == 8 { PRIMES[4] } else { b }, s)
            };
            head ^ tail
        }
    };
    mum(s ^ input.len() as u64, PRIMES[4])
}

/// bun's package-name hash (`String.Builder.stringHash`).
#[inline]
pub(crate) fn string_hash(s: &str) -> u64 {
    wyhash11(0, s.as_bytes())
}

const SECRET: [u64; 4] = [
    0xa0761d6478bd642f,
    0xe7037ed1a0b428db,
    0x8ebc6af09c88c6e3,
    0x589965cc75374cc3,
];

#[inline]
fn mix(a: u64, b: u64) -> u64 {
    let x = u128::from(a) * u128::from(b);
    (x as u64) ^ ((x >> 64) as u64)
}

/// bun `Wyhash::hash` (final4), one-shot.
pub(crate) fn wyhash(seed: u64, input: &[u8]) -> u64 {
    let s0 = seed ^ mix(seed ^ SECRET[0], SECRET[1]);
    let mut state = [s0, s0, s0];
    let (a, b);
    if input.len() <= 16 {
        if input.len() >= 4 {
            let end = input.len() - 4;
            let quarter = (input.len() >> 3) << 2;
            a = (rd(input, 4) << 32) | rd(&input[quarter..], 4);
            b = (rd(&input[end..], 4) << 32) | rd(&input[end - quarter..], 4);
        } else if !input.is_empty() {
            a = (u64::from(input[0]) << 16)
                | (u64::from(input[input.len() >> 1]) << 8)
                | u64::from(input[input.len() - 1]);
            b = 0;
        } else {
            a = 0;
            b = 0;
        }
    } else {
        let mut i = 0usize;
        if input.len() >= 48 {
            while i < input.len() - 48 {
                let r = |o: usize| rd(&input[i + o..], 8);
                state[0] = mix(r(0) ^ SECRET[1], r(8) ^ state[0]);
                state[1] = mix(r(16) ^ SECRET[2], r(24) ^ state[1]);
                state[2] = mix(r(32) ^ SECRET[3], r(40) ^ state[2]);
                i += 48;
            }
            state[0] ^= state[1] ^ state[2];
        }
        let rest = &input[i..];
        let mut j = 0usize;
        while j + 16 < rest.len() {
            state[0] = mix(
                rd(&rest[j..], 8) ^ SECRET[1],
                rd(&rest[j + 8..], 8) ^ state[0],
            );
            j += 16;
        }
        a = rd(&input[input.len() - 16..], 8);
        b = rd(&input[input.len() - 8..], 8);
    }
    let (mut a, mut b) = (a ^ SECRET[1], b ^ state[0]);
    let x = u128::from(a) * u128::from(b);
    a = x as u64;
    b = (x >> 64) as u64;
    mix(a ^ SECRET[0] ^ input.len() as u64, b ^ SECRET[1])
}

/// The iteration order of a bun/Zig `HashMap<u64, _, WrittenOrderContext>` that was given
/// `reserve` capacity and then had `keys` inserted in order (a repeated key keeps its slot).
/// Returns indices into `keys` of the first occurrence of each distinct key.
pub(crate) fn written_order(keys: &[u64], reserve: usize) -> Vec<usize> {
    fn cap_for(size: u64) -> usize {
        ((size * 100 / 80 + 1) as usize).next_power_of_two().max(8)
    }
    let mut slots: Vec<Option<(u64, usize)>> = Vec::new();
    let mut available = 0usize;
    let mut size = 0usize;
    let grow = |slots: &mut Vec<Option<(u64, usize)>>,
                available: &mut usize,
                size: usize,
                new_cap: usize| {
        let old = std::mem::replace(slots, vec![None; new_cap]);
        *available = new_cap * 80 / 100 - size;
        for (k, i) in old.into_iter().flatten() {
            let mut idx = (wyhash(0, &k.to_le_bytes()) as usize) & (new_cap - 1);
            while slots[idx].is_some() {
                idx = (idx + 1) & (new_cap - 1);
            }
            slots[idx] = Some((k, i));
        }
    };
    if reserve > 0 {
        grow(&mut slots, &mut available, 0, cap_for(reserve as u64));
    }
    for (i, &k) in keys.iter().enumerate() {
        if 1 > available {
            let load = slots.len() * 80 / 100 - available;
            grow(&mut slots, &mut available, size, cap_for((load + 1) as u64));
        }
        let mask = slots.len() - 1;
        let mut idx = (wyhash(0, &k.to_le_bytes()) as usize) & mask;
        let mut found = false;
        while let Some((existing, _)) = slots[idx] {
            if existing == k {
                found = true;
                break;
            }
            idx = (idx + 1) & mask;
        }
        if !found {
            slots[idx] = Some((k, i));
            available -= 1;
            size += 1;
        }
    }
    slots.into_iter().flatten().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_stable_and_length_sensitive() {
        let mut seen = std::collections::HashSet::new();
        for len in 0..100 {
            let s: Vec<u8> = (0..len as u8).collect();
            assert!(seen.insert(wyhash11(0, &s)), "wyhash11 collision at {len}");
            assert!(seen.insert(wyhash(0, &s)), "wyhash collision at {len}");
        }
        assert_eq!(wyhash11(0, b"abc"), wyhash11(0, b"abc"));
        assert_ne!(wyhash11(1, b"abc"), wyhash11(0, b"abc"));
    }

    fn order(names: &[&str], suffix: &str) -> Vec<String> {
        let mut sorted: Vec<&str> = names.to_vec();
        sorted.sort_unstable();
        let keys: Vec<u64> = sorted
            .iter()
            .map(|n| string_hash(&format!("{n}{suffix}")))
            .collect();
        written_order(&keys, names.len())
            .into_iter()
            .map(|i| sorted[i].to_owned())
            .collect()
    }

    #[test]
    fn written_order_matches_bun() {
        // bun test/cli/install/bun-lock.test.ts "writes trustedDependencies and
        // patchedDependencies in the order earlier versions wrote them".
        let trusted = [
            "esbuild",
            "sharp",
            "@prisma/client",
            "prisma",
            "bcrypt",
            "core-js",
            "@prisma/engines",
        ];
        assert_eq!(
            order(&trusted, ""),
            [
                "bcrypt",
                "esbuild",
                "sharp",
                "@prisma/engines",
                "@prisma/client",
                "core-js",
                "prisma"
            ]
        );
        let patched = ["esbuild", "sharp", "prisma", "bcrypt", "core-js"];
        assert_eq!(
            order(&patched, "@1.0.0"),
            ["prisma", "bcrypt", "core-js", "esbuild", "sharp"]
        );
        let trusted = [
            "esbuild",
            "sharp",
            "@prisma/client",
            "prisma",
            "bcrypt",
            "core-js",
            "@prisma/engines",
            "puppeteer",
            "playwright",
            "electron",
            "better-sqlite3",
            "fsevents",
            "@swc/core",
        ];
        assert_eq!(
            order(&trusted, ""),
            [
                "bcrypt",
                "@swc/core",
                "core-js",
                "playwright",
                "esbuild",
                "sharp",
                "@prisma/engines",
                "fsevents",
                "@prisma/client",
                "electron",
                "better-sqlite3",
                "prisma",
                "puppeteer",
            ]
        );
        let patched = [
            "esbuild",
            "sharp",
            "prisma",
            "bcrypt",
            "core-js",
            "puppeteer",
        ];
        assert_eq!(
            order(&patched, "@1.0.0"),
            [
                "prisma",
                "bcrypt",
                "core-js",
                "esbuild",
                "puppeteer",
                "sharp"
            ]
        );
    }

    #[test]
    fn written_order_dedupes_and_grows() {
        let keys: Vec<u64> = (0..40).chain(0..5).collect();
        let order = written_order(&keys, 2);
        assert_eq!(order.len(), 40);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..40).collect::<Vec<_>>());
    }
}
