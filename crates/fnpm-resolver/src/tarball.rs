//! Reads `package.json` out of a `.tgz` without extracting it, for tarball and GitHub
//! dependencies whose dependencies are only known after the download.
//!
//! The archive is decompressed as a stream and the scan stops at `package.json` (npm puts it
//! near the front), so a large tarball, or a gzip bomb, never has to fit in memory.

use std::io::Read;

/// The largest `package.json` read from a tarball.
const MAX_PACKAGE_JSON: u64 = 16 << 20;

/// The largest pax / GNU long-name record honoured.
const MAX_NAME_RECORD: u64 = 1 << 20;

/// How much uncompressed data is scanned for `package.json` before giving up.
const MAX_SCANNED: u64 = 1 << 30;

/// Returns the text of the tarball's top-level `package.json` (the first path component, usually
/// `package/`, is stripped like npm does). Pax `path` records and GNU long names are honoured.
pub(crate) fn package_json_from_tgz(tgz: &[u8]) -> Result<String, String> {
    scan(flate2::read::GzDecoder::new(tgz), true)
}

fn octal(field: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(field).ok()?;
    let text = text.trim_matches(|c: char| c == '\0' || c == ' ');
    if text.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(text, 8).ok()
}

fn cstr(field: &[u8]) -> &[u8] {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    &field[..end]
}

/// Parses pax extended header records (`<len> key=value\n`) for `path`.
fn pax_path(data: &[u8]) -> Option<String> {
    let mut rest = data;
    while !rest.is_empty() {
        let space = rest.iter().position(|&b| b == b' ')?;
        let len: usize = std::str::from_utf8(&rest[..space]).ok()?.parse().ok()?;
        if len <= space || len > rest.len() {
            return None;
        }
        let record = &rest[space + 1..len];
        let record = record.strip_suffix(b"\n").unwrap_or(record);
        if let Some(value) = record.strip_prefix(b"path=") {
            return String::from_utf8(value.to_vec()).ok();
        }
        rest = &rest[len..];
    }
    None
}

fn strip_first_component(path: &str) -> &str {
    let path = path.trim_start_matches("./");
    match path.split_once('/') {
        Some((_, rest)) => rest,
        None => "",
    }
}

#[cfg(test)]
pub(crate) fn package_json_from_tar(tar: &[u8]) -> Result<String, String> {
    scan(tar, false)
}

/// Maps a read error: running out of data is a truncated archive, anything else comes from
/// the decompressor.
fn read_error(e: std::io::Error, gzip: bool) -> String {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        "truncated tar archive".into()
    } else if gzip {
        format!("invalid gzip data: {e}")
    } else {
        format!("cannot read tar archive: {e}")
    }
}

/// Reads one 512-byte block; `Ok(false)` at a clean end of the stream.
fn read_block<R: Read>(r: &mut R, block: &mut [u8; 512], gzip: bool) -> Result<bool, String> {
    let mut filled = 0;
    while filled < block.len() {
        match r.read(&mut block[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err("truncated tar archive".into()),
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(read_error(e, gzip)),
        }
    }
    Ok(true)
}

fn read_data<R: Read>(r: &mut R, size: u64, gzip: bool) -> Result<Vec<u8>, String> {
    let mut data = Vec::with_capacity(size as usize);
    let n = r
        .take(size)
        .read_to_end(&mut data)
        .map_err(|e| read_error(e, gzip))?;
    if (n as u64) < size {
        return Err("truncated tar archive".into());
    }
    Ok(data)
}

fn skip<R: Read>(r: &mut R, size: u64, gzip: bool) -> Result<(), String> {
    let n =
        std::io::copy(&mut r.take(size), &mut std::io::sink()).map_err(|e| read_error(e, gzip))?;
    if n < size {
        return Err("truncated tar archive".into());
    }
    Ok(())
}

fn padding(size: u64) -> u64 {
    size.div_ceil(512) * 512 - size
}

fn scan<R: Read>(mut r: R, gzip: bool) -> Result<String, String> {
    let mut header = [0u8; 512];
    let mut next_name: Option<String> = None;
    let mut scanned: u64 = 0;
    while read_block(&mut r, &mut header, gzip)? {
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let size = octal(&header[124..136]).ok_or("invalid tar header size")?;
        scanned = scanned.saturating_add(512).saturating_add(size);
        if scanned > MAX_SCANNED {
            return Err("no package.json in the first 1 GiB of the tarball".into());
        }
        let typeflag = header[156];
        match typeflag {
            b'x' | b'L' => {
                if size > MAX_NAME_RECORD {
                    return Err("tar name record is too large".into());
                }
                let data = read_data(&mut r, size, gzip)?;
                next_name = if typeflag == b'x' {
                    pax_path(&data)
                } else {
                    Some(String::from_utf8_lossy(cstr(&data)).into_owned())
                };
            }
            b'0' | 0 | b'7' => {
                let name = match next_name.take() {
                    Some(n) => n,
                    None => {
                        let mut name = String::from_utf8_lossy(cstr(&header[0..100])).into_owned();
                        let prefix = cstr(&header[345..500]);
                        if &header[257..262] == b"ustar" && !prefix.is_empty() {
                            name = format!("{}/{}", String::from_utf8_lossy(prefix), name);
                        }
                        name
                    }
                };
                if strip_first_component(&name) == "package.json" {
                    if size > MAX_PACKAGE_JSON {
                        return Err("package.json is too large".into());
                    }
                    let data = read_data(&mut r, size, gzip)?;
                    return String::from_utf8(data)
                        .map_err(|_| "package.json is not UTF-8".to_owned());
                }
                skip(&mut r, size, gzip)?;
            }
            _ => {
                next_name = None;
                skip(&mut r, size, gzip)?;
            }
        }
        skip(&mut r, padding(size), gzip)?;
    }
    Err("no package.json in tarball".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str, size: usize, typeflag: u8) -> Vec<u8> {
        let mut h = vec![0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        let s = format!("{size:011o}\0");
        h[124..136].copy_from_slice(s.as_bytes());
        h[156] = typeflag;
        h[257..263].copy_from_slice(b"ustar\0");
        h
    }

    fn entry(name: &str, data: &[u8], typeflag: u8) -> Vec<u8> {
        let mut out = header(name, data.len(), typeflag);
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
        out
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn finds_package_json_in_any_root_folder() {
        let mut tar = entry("pkg/README.md", b"hi", b'0');
        tar.extend(entry("pkg/lib/package.json", b"{}", b'0'));
        tar.extend(entry("pkg/package.json", br#"{"name":"x"}"#, b'0'));
        tar.extend(vec![0u8; 1024]);
        assert_eq!(
            package_json_from_tgz(&gzip(&tar)).unwrap(),
            r#"{"name":"x"}"#
        );
    }

    #[test]
    fn pax_and_gnu_long_names() {
        let pax = b"29 path=package/package.json\n";
        let mut tar = entry("PaxHeader", pax, b'x');
        tar.extend(entry("ignored", b"{\"a\":1}", b'0'));
        assert_eq!(package_json_from_tar(&tar).unwrap(), "{\"a\":1}");
        let mut tar = entry("././@LongLink", b"package/package.json\0", b'L');
        tar.extend(entry("trunc", b"{\"b\":2}", b'0'));
        assert_eq!(package_json_from_tar(&tar).unwrap(), "{\"b\":2}");
    }

    #[test]
    fn malformed_pax_records_do_not_panic() {
        // A record length shorter than its own length prefix.
        let mut tar = entry("PaxHeader", b"1 path=package/package.json\n", b'x');
        tar.extend(entry("package/package.json", b"{}", b'0'));
        assert_eq!(package_json_from_tar(&tar).unwrap(), "{}");
    }

    #[test]
    fn streams_past_large_entries() {
        let big = vec![b'a'; 3 << 20];
        let mut tar = entry("package/dist/big.js", &big, b'0');
        tar.extend(entry("package/package.json", br#"{"name":"big"}"#, b'0'));
        tar.extend(vec![0u8; 1024]);
        assert_eq!(
            package_json_from_tgz(&gzip(&tar)).unwrap(),
            r#"{"name":"big"}"#
        );
    }

    #[test]
    fn oversized_records_are_rejected_without_allocating() {
        let tar = header("PaxHeader", 1 << 26, b'x');
        assert_eq!(
            package_json_from_tar(&tar).unwrap_err(),
            "tar name record is too large"
        );
        let tar = header("package/package.json", 1 << 26, b'0');
        assert_eq!(
            package_json_from_tar(&tar).unwrap_err(),
            "package.json is too large"
        );
        let tar = header("package/huge.bin", 1 << 32, b'0');
        assert!(package_json_from_tar(&tar).is_err());
    }

    #[test]
    fn errors() {
        assert!(package_json_from_tgz(b"not gzip").is_err());
        let tar = entry("package/index.js", b"x", b'0');
        assert_eq!(
            package_json_from_tar(&tar).unwrap_err(),
            "no package.json in tarball"
        );
        let mut tar = header("package/package.json", 100, b'0');
        tar.extend_from_slice(b"short");
        assert_eq!(
            package_json_from_tar(&tar).unwrap_err(),
            "truncated tar archive"
        );
    }
}
