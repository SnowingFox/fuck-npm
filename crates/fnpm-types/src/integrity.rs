//! Subresource Integrity values and npm's hex `shasum` (bun's `integrity.rs`).

use std::fmt;
use std::str::FromStr;

use base64::Engine as _;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha1::Digest as _;

const B64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_encode_padding(true).with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Ordered from weakest to strongest; [`Integrity::parse`] keeps the strongest entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashAlgorithm {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl HashAlgorithm {
    pub const fn digest_len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
            Self::Sha384 => 48,
            Self::Sha512 => 64,
        }
    }

    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha384 => "sha384",
            Self::Sha512 => "sha512",
        }
    }

    fn from_prefix(s: &str) -> Option<Self> {
        Some(match s {
            "sha1" => Self::Sha1,
            "sha256" => Self::Sha256,
            "sha384" => Self::Sha384,
            "sha512" => Self::Sha512,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IntegrityError {
    #[error("no supported hash in integrity {0:?}")]
    Unsupported(String),
    #[error("invalid hex shasum {0:?}")]
    InvalidHex(String),
    #[error("digest is {got} bytes, {algorithm:?} needs {expected}")]
    WrongLength { algorithm: HashAlgorithm, expected: usize, got: usize },
}

/// A digest with its algorithm. `Copy`, 65 bytes, no heap.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Integrity {
    algorithm: HashAlgorithm,
    digest: [u8; 64],
}

impl Integrity {
    pub fn from_digest(algorithm: HashAlgorithm, digest: &[u8]) -> Result<Self, IntegrityError> {
        let expected = algorithm.digest_len();
        if digest.len() != expected {
            return Err(IntegrityError::WrongLength { algorithm, expected, got: digest.len() });
        }
        let mut buf = [0u8; 64];
        buf[..expected].copy_from_slice(digest);
        Ok(Self { algorithm, digest: buf })
    }

    /// Parses an SRI string such as `sha512-<base64>`. With several whitespace-separated
    /// entries the strongest supported one wins; options after `?` are ignored.
    pub fn parse(sri: &str) -> Result<Self, IntegrityError> {
        sri.split_ascii_whitespace()
            .filter_map(Self::parse_entry)
            .max_by_key(|i| i.algorithm)
            .ok_or_else(|| IntegrityError::Unsupported(sri.to_owned()))
    }

    fn parse_entry(entry: &str) -> Option<Self> {
        let (algo, rest) = entry.split_once('-')?;
        let algorithm = HashAlgorithm::from_prefix(algo)?;
        let b64 = rest.split_once('?').map_or(rest, |(v, _)| v);
        let b64 = b64.trim_end_matches('=');
        let bytes = B64.decode(b64).ok()?;
        Self::from_digest(algorithm, &bytes).ok()
    }

    /// npm's legacy `dist.shasum`: 40 hex digits of SHA-1.
    pub fn parse_sha1_hex(hex_str: &str) -> Result<Self, IntegrityError> {
        let bytes = hex::decode(hex_str).map_err(|_| IntegrityError::InvalidHex(hex_str.to_owned()))?;
        Self::from_digest(HashAlgorithm::Sha1, &bytes).map_err(|_| IntegrityError::InvalidHex(hex_str.to_owned()))
    }

    /// Prefers `integrity`, falls back to `shasum`, as the registry's `dist` object is read.
    pub fn from_dist(integrity: Option<&str>, shasum: Option<&str>) -> Option<Self> {
        integrity
            .and_then(|s| Self::parse(s).ok())
            .or_else(|| shasum.and_then(|s| Self::parse_sha1_hex(s).ok()))
    }

    pub fn compute(algorithm: HashAlgorithm, data: &[u8]) -> Self {
        let mut h = IntegrityHasher::new(algorithm);
        h.update(data);
        h.finish()
    }

    #[inline]
    pub fn algorithm(&self) -> HashAlgorithm {
        self.algorithm
    }

    #[inline]
    pub fn digest(&self) -> &[u8] {
        &self.digest[..self.algorithm.digest_len()]
    }

    pub fn hasher(&self) -> IntegrityHasher {
        IntegrityHasher::new(self.algorithm)
    }

    /// Hashes `data` with this value's algorithm and compares.
    pub fn verify(&self, data: &[u8]) -> bool {
        Self::compute(self.algorithm, data) == *self
    }

    pub fn to_sri(&self) -> String {
        format!("{}-{}", self.algorithm.prefix(), B64.encode(self.digest()))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.digest())
    }
}

impl fmt::Display for Integrity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_sri())
    }
}

impl fmt::Debug for Integrity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Integrity({})", self.to_sri())
    }
}

impl FromStr for Integrity {
    type Err = IntegrityError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for Integrity {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_sri())
    }
}

impl<'de> Deserialize<'de> for Integrity {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Streaming hasher for one algorithm (tarball bytes are hashed while they download).
#[derive(Clone)]
pub enum IntegrityHasher {
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha384(sha2::Sha384),
    Sha512(sha2::Sha512),
}

impl IntegrityHasher {
    pub fn new(algorithm: HashAlgorithm) -> Self {
        match algorithm {
            HashAlgorithm::Sha1 => Self::Sha1(sha1::Sha1::new()),
            HashAlgorithm::Sha256 => Self::Sha256(sha2::Sha256::new()),
            HashAlgorithm::Sha384 => Self::Sha384(sha2::Sha384::new()),
            HashAlgorithm::Sha512 => Self::Sha512(sha2::Sha512::new()),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            Self::Sha1(h) => h.update(data),
            Self::Sha256(h) => h.update(data),
            Self::Sha384(h) => h.update(data),
            Self::Sha512(h) => h.update(data),
        }
    }

    pub fn finish(self) -> Integrity {
        let (algorithm, out): (HashAlgorithm, Vec<u8>) = match self {
            Self::Sha1(h) => (HashAlgorithm::Sha1, h.finalize().to_vec()),
            Self::Sha256(h) => (HashAlgorithm::Sha256, h.finalize().to_vec()),
            Self::Sha384(h) => (HashAlgorithm::Sha384, h.finalize().to_vec()),
            Self::Sha512(h) => (HashAlgorithm::Sha512, h.finalize().to_vec()),
        };
        Integrity::from_digest(algorithm, &out).expect("digest length matches algorithm")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `echo -n hello | openssl dgst -sha512 -binary | base64`
    const HELLO_SHA512: &str =
        "sha512-m3HSJL1i83hdltRq0+o9czGb+8KJDKra4t/3JRlnPKcjI8PZm6XBHXx6zG4UuMXaDEZjR1wuXDre9G9zvN7AQw==";
    const HELLO_SHA1_HEX: &str = "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d";

    #[test]
    fn sri_roundtrip() {
        let i = Integrity::parse(HELLO_SHA512).unwrap();
        assert_eq!(i.algorithm(), HashAlgorithm::Sha512);
        assert_eq!(i.to_sri(), HELLO_SHA512);
        assert!(i.verify(b"hello"));
        assert!(!i.verify(b"hellO"));
        assert_eq!(Integrity::compute(HashAlgorithm::Sha512, b"hello"), i);
    }

    #[test]
    fn strongest_wins_and_options_ignored() {
        let sha1 = Integrity::compute(HashAlgorithm::Sha1, b"hello").to_sri();
        let s = format!("{sha1} {HELLO_SHA512}?foo bogus-abc");
        assert_eq!(Integrity::parse(&s).unwrap().algorithm(), HashAlgorithm::Sha512);
        let no_pad = HELLO_SHA512.trim_end_matches('=');
        assert_eq!(Integrity::parse(no_pad).unwrap().to_sri(), HELLO_SHA512);
    }

    #[test]
    fn invalid() {
        assert!(Integrity::parse("").is_err());
        assert!(Integrity::parse("md5-abc").is_err());
        assert!(Integrity::parse("sha512-short").is_err());
        assert!(Integrity::parse_sha1_hex("zz").is_err());
        assert!(Integrity::parse_sha1_hex("abcd").is_err());
    }

    #[test]
    fn shasum_and_dist() {
        let i = Integrity::parse_sha1_hex(HELLO_SHA1_HEX).unwrap();
        assert_eq!(i.to_hex(), HELLO_SHA1_HEX);
        assert!(i.verify(b"hello"));
        let d = Integrity::from_dist(Some(HELLO_SHA512), Some(HELLO_SHA1_HEX)).unwrap();
        assert_eq!(d.algorithm(), HashAlgorithm::Sha512);
        let d = Integrity::from_dist(Some("garbage"), Some(HELLO_SHA1_HEX)).unwrap();
        assert_eq!(d.algorithm(), HashAlgorithm::Sha1);
    }

    #[test]
    fn streaming_and_serde() {
        let mut h = IntegrityHasher::new(HashAlgorithm::Sha256);
        h.update(b"hel");
        h.update(b"lo");
        assert_eq!(h.finish(), Integrity::compute(HashAlgorithm::Sha256, b"hello"));
        let i = Integrity::parse(HELLO_SHA512).unwrap();
        let j = serde_json::to_string(&i).unwrap();
        assert_eq!(j, format!("\"{HELLO_SHA512}\""));
        assert_eq!(serde_json::from_str::<Integrity>(&j).unwrap(), i);
    }
}
