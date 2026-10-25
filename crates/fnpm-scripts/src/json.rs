//! package.json reading, tolerant of comments and trailing commas like bun's JSON parser.

use std::path::Path;

/// Parses package.json text: strict JSON first (the fast path), then with `//` and `/* */`
/// comments and trailing commas removed.
pub fn parse_package_json(text: &str) -> std::io::Result<serde_json::Value> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    match serde_json::from_str(text) {
        Ok(v) => Ok(v),
        Err(strict) => serde_json::from_str(&strip_jsonc(text))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, strict)),
    }
}

/// Reads and parses a package.json file.
pub fn read_package_json(path: &Path) -> std::io::Result<serde_json::Value> {
    let text = std::fs::read_to_string(path)?;
    parse_package_json(&text)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

/// Removes comments and trailing commas outside of strings.
fn strip_jsonc(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(bytes.len());
                out.extend_from_slice(&bytes[start..i]);
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                continue;
            }
            b']' | b'}' => {
                while let Some(&last) = out.last() {
                    if last.is_ascii_whitespace() {
                        out.pop();
                    } else {
                        break;
                    }
                }
                if out.last() == Some(&b',') {
                    out.pop();
                }
            }
            _ => {}
        }
        out.push(c);
        i += 1;
    }
    // Only ASCII bytes were removed, so the result is still UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_and_jsonc() {
        assert_eq!(parse_package_json(r#"{"a":1}"#).unwrap()["a"], 1);
        let v =
            parse_package_json("{\n // c\n \"a\": [1, 2,], /* x */ \"b\": \"//not\",\n}").unwrap();
        assert_eq!(v["a"], serde_json::json!([1, 2]));
        assert_eq!(v["b"], "//not");
        assert!(parse_package_json("{").is_err());
        assert_eq!(
            parse_package_json("\u{feff}{\"x\":\"a\\\"b\"}").unwrap()["x"],
            "a\"b"
        );
    }
}
