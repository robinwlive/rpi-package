//! File snapshot tags and numbered-line addressability.
//!
//! The address of a line is its **line number**; the tag is a 4-hex fingerprint
//! of the whole file, printed once in the `read` header. That split is what
//! makes two identical lines unambiguous (their numbers differ) while still
//! detecting "the file moved under me" (the tag no longer matches).
//!
//! A per-line hash cannot do this: two short tags collide, and a collision
//! silently edits the wrong line. One tag per file cannot collide with itself.

/// Number of hexadecimal characters in a snapshot tag.
pub const TAG_LEN: usize = 4;

/// Split `content` into addressable lines.
///
/// A trailing newline does not produce a final empty line, so line numbers match
/// what an editor shows.
pub fn split_lines(content: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    if content.is_empty() {
        return Vec::new();
    }
    lines
}

/// Render one numbered line, `12:<text>`.
pub fn numbered_line(number: usize, text: &str) -> String {
    format!("{number}:{text}")
}

/// Four-hex fingerprint of a whole file.
///
/// Ignores trailing spaces, tabs and CR at the end of each line, so a formatter
/// that only reindents or a CRLF checkout does not invalidate a tag the model
/// just read.
pub fn snapshot_tag(content: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for segment in content.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        let kept = line
            .trim_end_matches([' ', '\t', '\r'])
            .as_bytes();
        for &byte in kept {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // Keep the line boundary significant.
        hash ^= b'\n' as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:04X}", hash & 0xffff)
}

/// Whether `spec` is a snapshot tag: exactly [`TAG_LEN`] hex characters.
pub fn is_tag(spec: &str) -> bool {
    spec.len() == TAG_LEN && spec.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Why a header or address was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorError {
    /// The tag is not four hex characters.
    MalformedTag(String),
    /// The tag is well formed but the file no longer hashes to it.
    StaleTag { given: String, actual: String },
    /// The line number is not a positive integer.
    BadLine(String),
    /// The line number is not in the file.
    NoSuchLine { line: usize, total: usize },
}

impl std::fmt::Display for AnchorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnchorError::MalformedTag(tag) => write!(
                f,
                "error: `#{tag}` is not a snapshot tag ({TAG_LEN} hex characters, e.g. `#1A2B`); copy the tag from the read header"
            ),
            AnchorError::StaleTag { given, actual } => write!(
                f,
                "error: this file changed since it was read (tag #{given}, now #{actual}); read it again and use the current line numbers"
            ),
            AnchorError::BadLine(value) => {
                write!(f, "error: `{value}` is not a line number")
            }
            AnchorError::NoSuchLine { line, total } => write!(
                f,
                "error: line {line} is past the end of the file ({total} lines)"
            ),
        }
    }
}

impl std::error::Error for AnchorError {}

/// Check a model-supplied tag against the current content.
pub fn verify_tag(content: &str, given: &str) -> Result<(), AnchorError> {
    if !is_tag(given) {
        return Err(AnchorError::MalformedTag(given.to_string()));
    }
    let actual = snapshot_tag(content);
    if !actual.eq_ignore_ascii_case(given) {
        return Err(AnchorError::StaleTag {
            given: given.to_uppercase(),
            actual,
        });
    }
    Ok(())
}

/// Resolve a 1-based line number against `total` lines.
pub fn resolve_line(total: usize, spec: &str) -> Result<usize, AnchorError> {
    let line: usize = spec
        .trim()
        .parse()
        .map_err(|_| AnchorError::BadLine(spec.to_string()))?;
    if line == 0 {
        return Err(AnchorError::BadLine(spec.to_string()));
    }
    if line > total {
        return Err(AnchorError::NoSuchLine { line, total });
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_four_hex_and_stable() {
        let tag = snapshot_tag("fn main() {}\n");
        assert_eq!(tag.len(), TAG_LEN);
        assert!(is_tag(&tag), "{tag}");
        assert_eq!(tag, snapshot_tag("fn main() {}\n"));
    }

    #[test]
    fn a_real_change_changes_the_tag() {
        assert_ne!(snapshot_tag("a\n"), snapshot_tag("b\n"));
        assert_ne!(snapshot_tag("a\n"), snapshot_tag("a\nb\n"));
    }

    #[test]
    fn trailing_whitespace_and_crlf_do_not_invalidate_a_tag() {
        // A formatter or checkout flipping line endings must not force a re-read.
        assert_eq!(snapshot_tag("a\nb\n"), snapshot_tag("a  \nb\t\n"));
        assert_eq!(snapshot_tag("a\nb\n"), snapshot_tag("a\r\nb\r\n"));
    }

    #[test]
    fn identical_lines_are_distinguished_by_number() {
        let lines = split_lines("if x {\n}\nif y {\n}\n");
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[1], "}");
        assert_eq!(lines[3], "}");
        assert_eq!(resolve_line(4, "1").unwrap(), 1);
        assert_eq!(resolve_line(4, "4").unwrap(), 4);
    }

    #[test]
    fn stale_tag_is_reported_with_both_values() {
        let error = verify_tag("new content\n", "0000").unwrap_err();
        match error {
            AnchorError::StaleTag { given, actual } => {
                assert_eq!(given, "0000");
                assert_eq!(actual, snapshot_tag("new content\n"));
            }
            other => panic!("expected stale tag, got {other:?}"),
        }
    }

    #[test]
    fn malformed_tag_names_the_shape() {
        let error = verify_tag("a\n", "zz").unwrap_err();
        assert!(error.to_string().contains("not a snapshot tag"), "{error}");
    }

    #[test]
    fn line_bounds_are_checked() {
        assert!(matches!(
            resolve_line(2, "3"),
            Err(AnchorError::NoSuchLine { line: 3, total: 2 })
        ));
        assert!(matches!(resolve_line(2, "0"), Err(AnchorError::BadLine(_))));
        assert!(matches!(resolve_line(2, "x"), Err(AnchorError::BadLine(_))));
    }

    #[test]
    fn trailing_newline_does_not_add_a_line() {
        assert_eq!(split_lines("a\nb\n").len(), 2);
        assert_eq!(split_lines("a\nb").len(), 2);
        assert_eq!(split_lines("").len(), 0);
    }
}
