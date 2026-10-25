//! Registry integrity values, read exactly as bun reads them (bun `install/integrity.rs`,
//! `install/npm.rs`), so the same manifests verify the same way:
//!
//! - an SRI string may hold several whitespace-separated entries; the strongest supported
//!   one wins (sha512 > sha384 > sha256 > sha1), the first of equal strength;
//! - `?options` and trailing `=` padding are ignored; a digest shorter than the algorithm's
//!   is zero-padded (as bun's decoder does), a longer one is unusable;
//! - `dist.shasum` is hex SHA-1: at most the first 40 digits are read and an odd count is
//!   malformed;
//! - an unusable `integrity` falls back to `shasum` silently; only when neither yields a hash
//!   and one of them was malformed is there a warning.

use base64::Engine as _;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use fnpm_types::{HashAlgorithm, Integrity};

const B64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::RequireNone)
        // bun's decoder rejects a final symbol with bits set past the last whole byte.
        .with_decode_allow_trailing_bits(false),
);

fn parse_entry(entry: &str) -> Option<Integrity> {
    if entry.len() < "sha256-".len() {
        return None;
    }
    let (algorithm, rest) = if let Some(r) = entry.strip_prefix("sha1-") {
        (HashAlgorithm::Sha1, r)
    } else if let Some(r) = entry.strip_prefix("sha256-") {
        (HashAlgorithm::Sha256, r)
    } else if let Some(r) = entry.strip_prefix("sha384-") {
        (HashAlgorithm::Sha384, r)
    } else {
        (HashAlgorithm::Sha512, entry.strip_prefix("sha512-")?)
    };
    // bun's `Tag::parse`: an algorithm with nothing after its `-` is unknown.
    if rest.is_empty() {
        return None;
    }
    let rest = rest.split_once('?').map_or(rest, |(v, _)| v);
    let rest = rest.trim_end_matches('=');
    let decoded = B64.decode(rest).ok()?;
    let expected = algorithm.digest_len();
    if decoded.len() > expected {
        return None;
    }
    let mut digest = vec![0u8; expected];
    digest[..decoded.len()].copy_from_slice(&decoded);
    Integrity::from_digest(algorithm, &digest).ok()
}

/// The strongest usable entry of an SRI string.
pub fn parse_sri(sri: &str) -> Option<Integrity> {
    let mut best: Option<Integrity> = None;
    for entry in sri.split_ascii_whitespace() {
        if let Some(parsed) = parse_entry(entry)
            && best.is_none_or(|b| parsed.algorithm() > b.algorithm())
        {
            best = Some(parsed);
        }
    }
    best
}

/// `dist.shasum`: `Ok(None)` when empty, `Err(())` when malformed.
#[allow(clippy::result_unit_err)]
pub fn parse_shasum(hex: &str) -> Result<Option<Integrity>, ()> {
    if hex.is_empty() {
        return Ok(None);
    }
    let bytes = hex.as_bytes();
    let end = bytes.len().min(40);
    if !end.is_multiple_of(2) {
        return Err(());
    }
    let mut digest = [0u8; 20];
    for (i, pair) in bytes[..end].chunks(2).enumerate() {
        let s = std::str::from_utf8(pair).map_err(|_| ())?;
        digest[i] = u8::from_str_radix(s, 16).map_err(|_| ())?;
    }
    Integrity::from_digest(HashAlgorithm::Sha1, &digest)
        .map(Some)
        .map_err(|_| ())
}

/// The integrity a registry `dist` object yields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DistIntegrity {
    /// The value to verify against, if any.
    pub integrity: Option<Integrity>,
    /// Neither field gave a hash and one of them was malformed: warn with
    /// [`malformed_warning`].
    pub malformed: bool,
}

/// Reads `dist.integrity` / `dist.shasum` as bun does.
pub fn dist_integrity(integrity: Option<&str>, shasum: Option<&str>) -> DistIntegrity {
    let mut malformed = false;
    if let Some(s) = integrity {
        if let Some(i) = parse_sri(s) {
            return DistIntegrity {
                integrity: Some(i),
                malformed: false,
            };
        }
        malformed = !s.is_empty();
    }
    if let Some(s) = shasum {
        match parse_shasum(s) {
            Ok(Some(i)) => {
                return DistIntegrity {
                    integrity: Some(i),
                    malformed: false,
                };
            }
            Ok(None) => {}
            Err(()) => malformed = true,
        }
    }
    DistIntegrity {
        integrity: None,
        malformed,
    }
}

/// bun's warning for [`DistIntegrity::malformed`].
pub fn malformed_warning(name: &str, version: &str) -> String {
    format!(
        "Unsupported or malformed integrity hash in registry metadata for {name}@{}, tarball will not be verified",
        crate::tar::escape_control(version)
    )
}

/// bun's error line for [`crate::Error::Integrity`]: `Integrity check failed for tarball: <name>`.
pub fn integrity_failure_message(name: &str) -> String {
    format!(
        "Integrity check failed for tarball: {}",
        crate::tar::escape_control(name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sri(algorithm: HashAlgorithm, data: &[u8]) -> String {
        Integrity::compute(algorithm, data).to_sri()
    }

    #[test]
    fn strongest_entry_wins() {
        let s1 = sri(HashAlgorithm::Sha1, b"x");
        let s512 = sri(HashAlgorithm::Sha512, b"x");
        let s256 = sri(HashAlgorithm::Sha256, b"x");
        let multi = format!("{s1} {s512}\t{s256}");
        assert_eq!(
            parse_sri(&multi),
            Some(Integrity::compute(HashAlgorithm::Sha512, b"x"))
        );
        // Unknown algorithms and junk are ignored.
        assert_eq!(
            parse_sri(&format!("md5-abc sha512-!!! {s1}")),
            Some(Integrity::compute(HashAlgorithm::Sha1, b"x"))
        );
        assert_eq!(parse_sri("md5-xyz"), None);
        assert_eq!(parse_sri(""), None);
    }

    #[test]
    fn option_suffix_and_padding() {
        let s = sri(HashAlgorithm::Sha512, b"tgz");
        assert_eq!(
            parse_sri(&format!("{s}?foo=bar")),
            Some(Integrity::compute(HashAlgorithm::Sha512, b"tgz"))
        );
        assert_eq!(
            parse_sri(s.trim_end_matches('=')),
            Some(Integrity::compute(HashAlgorithm::Sha512, b"tgz"))
        );
    }

    #[test]
    fn short_digest_is_zero_padded_long_is_rejected() {
        let short = parse_sri("sha256-AAEC").unwrap();
        assert_eq!(&short.digest()[..3], &[0, 1, 2]);
        assert!(short.digest()[3..].iter().all(|&b| b == 0));
        let long = format!(
            "sha1-{}",
            base64::engine::general_purpose::STANDARD.encode([7u8; 32])
        );
        assert_eq!(parse_sri(&long), None);
    }

    #[test]
    fn empty_values_and_trailing_bits_follow_bun() {
        // Nothing after the dash: unknown, so the shasum fallback applies.
        assert_eq!(parse_sri("sha256-"), None);
        assert_eq!(parse_sri("sha512-"), None);
        let shasum = "3cd0599b099384b815c10f7fa7df0092b62d534f";
        assert_eq!(
            dist_integrity(Some("sha512-"), Some(shasum)).integrity,
            parse_shasum(shasum).unwrap()
        );
        // Only padding: bun decodes an empty digest, zero-padded.
        let zero = parse_sri("sha256-=").unwrap();
        assert!(zero.digest().iter().all(|&b| b == 0));
        // A final symbol with bits past the last byte is malformed, as is a lone symbol.
        assert_eq!(parse_sri("sha1-AB"), None);
        assert_eq!(parse_sri("sha1-AAAAA"), None);
        assert!(parse_sri("sha1-AA").is_some());
    }

    #[test]
    fn shasum_rules() {
        let full = "3cd0599b099384b815c10f7fa7df0092b62d534f";
        assert_eq!(parse_shasum(full).unwrap().unwrap().to_hex(), full);
        assert_eq!(parse_shasum("").unwrap(), None);
        assert!(parse_shasum("abc").is_err());
        assert!(parse_shasum("zz").is_err());
        // Longer values: only the first 40 digits are read.
        let longer = format!("{full}0011");
        assert_eq!(parse_shasum(&longer).unwrap().unwrap().to_hex(), full);
    }

    #[test]
    fn dist_fallbacks() {
        let shasum = "3cd0599b099384b815c10f7fa7df0092b62d534f";
        let s = sri(HashAlgorithm::Sha512, b"a");
        assert_eq!(
            dist_integrity(Some(&s), Some(shasum)).integrity,
            parse_sri(&s)
        );
        let d = dist_integrity(Some("sha999-nope"), Some(shasum));
        assert_eq!(
            d,
            DistIntegrity {
                integrity: parse_shasum(shasum).unwrap(),
                malformed: false
            }
        );
        assert_eq!(
            dist_integrity(Some("sha999-nope"), None),
            DistIntegrity {
                integrity: None,
                malformed: true
            }
        );
        assert_eq!(
            dist_integrity(None, Some("xyz")),
            DistIntegrity {
                integrity: None,
                malformed: true
            }
        );
        assert_eq!(
            dist_integrity(Some(""), Some("")),
            DistIntegrity {
                integrity: None,
                malformed: false
            }
        );
        assert_eq!(
            dist_integrity(None, None),
            DistIntegrity {
                integrity: None,
                malformed: false
            }
        );
        assert_eq!(
            malformed_warning("pkg", "1.0.0"),
            "Unsupported or malformed integrity hash in registry metadata for pkg@1.0.0, tarball will not be verified"
        );
    }
}
