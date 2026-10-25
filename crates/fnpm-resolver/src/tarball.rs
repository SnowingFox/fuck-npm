//! Reads `package.json` out of a `.tgz` without extracting it, for tarball and GitHub
//! dependencies whose dependencies are only known after the download.

use std::io::Read;

/// The largest `package.json` read from a tarball.
const MAX_PACKAGE_JSON: u64 = 16 << 20;

/// Returns the text of the tarball's top-level `package.json` (the first path component, usually
/// `package/`, is stripped like npm does). Pax `path` records and GNU long names are honoured.
pub(crate) fn package_json_from_tgz(tgz: &[u8]) -> Result<String, String> {
    let mut reader = flate2::read::GzDecoder::new(tgz);
    let mut tar = Vec::new();
    reader
        .read_to_end(&mut tar)
        .map_err(|e| format!("invalid gzip data: {e}"))?;
    package_json_from_tar(&tar)
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
        if len == 0 || len > rest.len() {
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

pub(crate) fn package_json_from_tar(tar: &[u8]) -> Result<String, String> {
    let mut offset = 0usize;
    let mut next_name: Option<String> = None;
    while offset + 512 <= tar.len() {
        let header = &tar[offset..offset + 512];
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let size = octal(&header[124..136]).ok_or("invalid tar header size")?;
        let typeflag = header[156];
        let data_start = offset + 512;
        let data_end = data_start
            .checked_add(size as usize)
            .ok_or("invalid tar entry size")?;
        if data_end > tar.len() {
            return Err("truncated tar archive".into());
        }
        let data = &tar[data_start..data_end];
        match typeflag {
            b'x' => next_name = pax_path(data),
            b'L' => next_name = Some(String::from_utf8_lossy(cstr(data)).into_owned()),
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
                    return String::from_utf8(data.to_vec())
                        .map_err(|_| "package.json is not UTF-8".to_owned());
                }
            }
            _ => next_name = None,
        }
        offset = data_start + (size as usize).div_ceil(512) * 512;
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
