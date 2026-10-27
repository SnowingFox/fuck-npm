//! bun's source frame for an error at a place in a JSON file (bun `logger.Msg` printing):
//!
//! ```text
//! 1 | {"name":"mono","workspaces":["packages/a"]}
//!                                 ^
//! error: Workspace not found "packages/a"
//!     at /work/package.json:1:30
//! ```
//!
//! [`Frame::framed`] puts the frame into an error message; [`split`] (and `splitFrame` in
//! `packages/fuck-npm/src/errors.ts`) takes it out again, so each renderer (the native
//! launcher's and the TS CLI's) prints it above its own `error:`.

use std::path::Path;

/// A position in a source text: the 1-based line and column, and the text of that line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub line: usize,
    pub column: usize,
    pub text: String,
}

impl Frame {
    /// The frame of the byte `offset` of `source` (clamped to its end). Columns count
    /// characters, as bun's do.
    pub fn at_offset(source: &str, offset: usize) -> Self {
        let mut offset = offset.min(source.len());
        while !source.is_char_boundary(offset) {
            offset -= 1;
        }
        let start = source[..offset].rfind('\n').map_or(0, |i| i + 1);
        let end = source[offset..]
            .find('\n')
            .map_or(source.len(), |i| offset + i);
        Self {
            line: source[..start].matches('\n').count() + 1,
            column: source[start..offset].chars().count() + 1,
            text: source[start..end].trim_end_matches('\r').to_owned(),
        }
    }

    /// The frame of the 1-based `line` and `column` (characters) of `source`, clamped to the
    /// line's end.
    pub fn at_line_col(source: &str, line: usize, column: usize) -> Self {
        let start = source
            .split_inclusive('\n')
            .take(line.saturating_sub(1))
            .map(str::len)
            .sum::<usize>();
        let rest = &source[start.min(source.len())..];
        let end = rest.find('\n').unwrap_or(rest.len());
        let offset = rest[..end]
            .char_indices()
            .nth(column.saturating_sub(1))
            .map_or(end, |(i, _)| i);
        Self::at_offset(source, start + offset)
    }

    /// The frame of the first `"<value>"` string after `"<key>"` in `source`: where a JSON
    /// file names an entry (`workspaces` → `"packages/a"`). `None` when it is not there.
    pub fn of_string_after(source: &str, key: &str, value: &str) -> Option<Self> {
        let key_at = source.find(&format!("\"{key}\""))?;
        let quoted = serde_json::to_string(value).ok()?;
        let at = key_at + source[key_at..].find(&quoted)?;
        Some(Self::at_offset(source, at))
    }

    /// `message` with this frame: the source line and the caret under the column first, then
    /// `message` and `    at <path>:<line>:<column>`. A renderer prints it through
    /// [`split`], with its `error: ` between the two parts.
    pub fn framed(&self, path: &Path, message: &str) -> String {
        let gutter = format!("{} | ", self.line);
        let caret = " ".repeat(gutter.len() + self.column - 1);
        format!(
            "{gutter}{}\n{caret}^\n{message}\n    at {}:{}:{}",
            self.text,
            path.display(),
            self.line,
            self.column
        )
    }
}

/// Splits an error message made by [`Frame::framed`] into its frame (the source line and the
/// caret line) and the message to print after `error: `. A message without a frame is
/// `(None, message)`.
pub fn split(message: &str) -> (Option<&str>, &str) {
    let mut lines = message.splitn(3, '\n');
    let (Some(source), Some(caret), Some(rest)) = (lines.next(), lines.next(), lines.next()) else {
        return (None, message);
    };
    let gutter = source
        .split_once(" | ")
        .is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if !gutter || caret.trim_start() != "^" {
        return (None, message);
    }
    (Some(&message[..source.len() + 1 + caret.len()]), rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_a_workspace_entry_like_bun() {
        let source = r#"{"name":"mono","workspaces":["packages/app","packages/other"]}"#;
        let frame = Frame::of_string_after(source, "workspaces", "packages/other").unwrap();
        assert_eq!((frame.line, frame.column), (1, 45));
        let text = frame.framed(Path::new("/w/package.json"), "Workspace not found");
        let (lines, message) = split(&text);
        assert_eq!(
            lines,
            Some(format!("1 | {source}\n{}^", " ".repeat(4 + 44)).as_str())
        );
        assert_eq!(message, "Workspace not found\n    at /w/package.json:1:45");
        assert_eq!(split("a | b\n^\nc"), (None, "a | b\n^\nc"));
        assert_eq!(split("plain"), (None, "plain"));
    }

    #[test]
    fn counts_lines_and_characters() {
        let source = "{\n  \"é\": \"x\"\n}";
        let frame = Frame::at_offset(source, source.find('x').unwrap());
        assert_eq!(
            (frame.line, frame.column, frame.text.as_str()),
            (2, 9, "  \"é\": \"x\"")
        );
        assert_eq!(Frame::at_offset("", 5).column, 1);
        assert_eq!(Frame::at_line_col(source, 2, 9), frame);
    }
}
