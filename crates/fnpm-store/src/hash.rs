//! Tarball integrity hashing with ring (BoringSSL assembly; `sha2` is 1.5-2x slower on
//! machines without SHA extensions). File contents are only hashed with xxh3-128 to find
//! identical files inside a package (see `staging`), and those are compared byte for byte.

use fnpm_types::{HashAlgorithm, Integrity};

fn ring_algorithm(algorithm: HashAlgorithm) -> &'static ring::digest::Algorithm {
    match algorithm {
        HashAlgorithm::Sha1 => &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        HashAlgorithm::Sha256 => &ring::digest::SHA256,
        HashAlgorithm::Sha384 => &ring::digest::SHA384,
        HashAlgorithm::Sha512 => &ring::digest::SHA512,
    }
}

/// Incremental integrity hash of a tarball body.
pub(crate) struct IntegrityHasher {
    algorithm: HashAlgorithm,
    ctx: ring::digest::Context,
}

impl IntegrityHasher {
    pub(crate) fn new(algorithm: HashAlgorithm) -> Self {
        Self {
            algorithm,
            ctx: ring::digest::Context::new(ring_algorithm(algorithm)),
        }
    }

    pub(crate) fn update(&mut self, data: &[u8]) {
        self.ctx.update(data);
    }

    pub(crate) fn finish(self) -> Integrity {
        Integrity::from_digest(self.algorithm, self.ctx.finish().as_ref())
            .expect("ring digests have the algorithm's length")
    }
}

impl std::io::Write for IntegrityHasher {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// SHA-1 of `data` (patch hashes).
pub(crate) fn sha1(data: &[u8]) -> [u8; 20] {
    let mut out = [0u8; 20];
    out.copy_from_slice(
        ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, data).as_ref(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrity_matches_fnpm_types() {
        for a in [
            HashAlgorithm::Sha1,
            HashAlgorithm::Sha256,
            HashAlgorithm::Sha384,
            HashAlgorithm::Sha512,
        ] {
            let mut h = IntegrityHasher::new(a);
            h.update(b"hel");
            h.update(b"lo");
            assert_eq!(h.finish(), Integrity::compute(a, b"hello"));
        }
    }
}
