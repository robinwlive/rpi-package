//! Anchor-driven edits: `PUT` and `CUT` hunks resolved against line numbers.
//!
//! Syntax (mirrors the hashline vocabulary so models that learned it elsewhere
//! already know the shape):
//!
//! ```text
//! PUT 4.=6:        replace lines 4-6 with the `+` rows that follow
//! +new line
//! PUT >9:          insert the `+` rows after line 9   (`<3` = before line 3)
//! CUT 4.=6         delete lines 4-6
//! ```
//!
//! Every address is resolved against the **original** file before any splice is
//! computed, so a `CUT` and a `PUT` in one call cannot chase each other's
//! shifted line numbers.

use crate::anchors::{resolve_line, split_lines};

/// One parsed hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hunk {
    /// Replace the inclusive `start..=end` range with `lines`.
    Put {
        start: usize,
        end: usize,
        lines: Vec<String>,
    },
    /// Insert `lines` before `line` (`after: false`) or after it (`after: true`).
    Insert {
        line: usize,
        after: bool,
        lines: Vec<String>,
    },
    /// Delete the inclusive `start..=end` range.
    Cut { start: usize, end: usize },
}

/// Result of applying a hunk list.
#[derive(Debug, Clone)]
pub struct Applied {
    pub content: String,
    pub replaced: usize,
    pub inserted: usize,
    pub deleted: usize,
}

/// Parse the hunk text into [`Hunk`]s.
///
/// Blank lines outside a payload are ignored so a model can space out hunks;
/// a blank line *inside* a payload row set is kept as an empty line, which is
/// why payload rows must carry the `+` sigil.
pub fn parse(spec: &str, total: usize) -> Result<Vec<Hunk>, String> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut lines = spec.lines().peekable();
    while let Some(raw) = lines.next() {
        let line = raw.trim_end();
        if line.is_empty() {
            continue;
        }
        if let Some(payload) = line.strip_prefix('+') {
            return Err(format!(
                "error: `+{payload}` has no preceding PUT hunk; payload rows follow a `PUT` header"
            ));
        }
        let (header, rest) = line
            .split_once(':')
            .map_or((line, None), |(head, tail)| (head, Some(tail)));
        let mut words = header.split_whitespace();
        let keyword = words
            .next()
            .ok_or_else(|| format!("error: `{line}` is not a hunk"))?;
        match keyword.to_ascii_uppercase().as_str() {
            "PUT" => {
                let address = words
                    .next()
                    .ok_or_else(|| format!("error: `{line}` needs an address, e.g. `PUT 4.=6:` or `PUT >9:`"))?;
                // A `PUT` whose address is inline (`PUT 4.=6: text`) takes the
                // remainder of the header line as its only payload row.
                let mut payload: Vec<String> = Vec::new();
                if let Some(inline) = rest.filter(|text| !text.is_empty()) {
                    payload.push(inline.strip_prefix(' ').unwrap_or(inline).to_string());
                }
                while let Some(next) = lines.next() {
                    match next.strip_prefix('+') {
                        Some(row) => payload.push(row.to_string()),
                        None => {
                            // Not a payload row: re-handle it as the next hunk.
                            let replayed = next.to_string();
                            let extra = parse(&replayed, total)?;
                            hunks.extend(extra);
                            break;
                        }
                    }
                }
                hunks.push(build_put(address, payload, total)?);
            }
            "CUT" => {
                let address = words
                    .next()
                    .ok_or_else(|| format!("error: `{line}` needs an address, e.g. `CUT 4.=6`"))?;
                let (start, end) = parse_range(address, total)?;
                hunks.push(Hunk::Cut { start, end });
            }
            other => {
                return Err(format!(
                    "error: `{other}` is not a hunk keyword; use `PUT <address>:` to write or `CUT <address>` to delete"
                ))
            }
        }
    }
    if hunks.is_empty() {
        return Err("error: the patch has no hunks".into());
    }
    Ok(hunks)
}

/// Build the `PUT` hunk an address selects.
fn build_put(address: &str, payload: Vec<String>, total: usize) -> Result<Hunk, String> {
    if let Some(rest) = address.strip_prefix('>') {
        let line = if rest == "$" {
            total
        } else {
            resolve_line(total, rest).map_err(|error| error.to_string())?
        };
        return Ok(Hunk::Insert {
            line,
            after: true,
            lines: payload,
        });
    }
    if let Some(rest) = address.strip_prefix('<') {
        let line = if rest == "1" {
            1
        } else {
            resolve_line(total, rest).map_err(|error| error.to_string())?
        };
        return Ok(Hunk::Insert {
            line,
            after: false,
            lines: payload,
        });
    }
    let (start, end) = parse_range(address, total)?;
    Ok(Hunk::Put {
        start,
        end,
        lines: payload,
    })
}

/// Parse `N` or `N.=M` into an inclusive `(start, end)` span.
fn parse_range(address: &str, total: usize) -> Result<(usize, usize), String> {
    let (first, second) = match address.split_once(".=") {
        Some((first, second)) => (first, Some(second)),
        None => (address, None),
    };
    let start = resolve_line(total, first).map_err(|error| error.to_string())?;
    let end = match second {
        Some(second) => resolve_line(total, second).map_err(|error| error.to_string())?,
        None => start,
    };
    if end < start {
        return Err(format!(
            "error: range `{address}` ends before it starts; write ranges low to high"
        ));
    }
    Ok((start, end))
}

/// Apply parsed hunks to `content`.
pub fn apply(content: &str, hunks: &[Hunk]) -> Applied {
    let crlf = content.contains("\r\n");
    // `split_lines` keeps the `\r` of a CRLF file; normalizing here means the
    // join below re-adds exactly one line ending per row.
    let source: Vec<String> = split_lines(content)
        .into_iter()
        .map(|line| {
            if crlf {
                line.strip_suffix('\r').unwrap_or(line).to_string()
            } else {
                line.to_string()
            }
        })
        .collect();
    let total = source.len();
    let newline = if crlf { "\r\n" } else { "\n" };

    // (start 0-based, end 0-based exclusive, replacement rows)
    let mut splices: Vec<(usize, usize, Vec<String>)> = Vec::with_capacity(hunks.len());
    let (mut replaced, mut inserted, mut deleted) = (0usize, 0usize, 0usize);

    for hunk in hunks {
        match hunk {
            Hunk::Put { start, end, lines } => {
                let (from, to) = (start - 1, *end);
                let before = to - from;
                replaced += before;
                deleted += before.saturating_sub(lines.len());
                inserted += lines.len().saturating_sub(before);
                splices.push((from, to, lines.clone()));
            }
            Hunk::Cut { start, end } => {
                let (from, to) = (start - 1, *end);
                replaced += to - from;
                splices.push((from, to, Vec::new()));
            }
            Hunk::Insert { line, after, lines } => {
                // `after` line N and `before` line N+1 are the same gap.
                let at = if *after { *line } else { line - 1 };
                inserted += lines.len();
                splices.push((at, at, lines.clone()));
            }
        }
    }

    let _ = total;
    splices.sort_by_key(|(from, to, _)| (*from, *to));

    let mut out: Vec<String> = Vec::with_capacity(source.len() + inserted);
    let mut cursor = 0usize;
    for (from, to, rows) in &splices {
        while cursor < *from {
            out.push(source[cursor].clone());
            cursor += 1;
        }
        out.extend(rows.iter().cloned());
        cursor = cursor.max(*to);
    }
    while cursor < source.len() {
        out.push(source[cursor].clone());
        cursor += 1;
    }

    let mut text = out.join(newline);
    if content.ends_with('\n') && !text.is_empty() {
        text.push_str(newline);
    }
    Applied {
        content: text,
        replaced,
        inserted,
        deleted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(content: &str, spec: &str) -> String {
        let total = split_lines(content).len();
        let hunks = parse(spec, total).unwrap();
        apply(content, &hunks).content
    }

    #[test]
    fn put_replaces_an_inclusive_range() {
        assert_eq!(run("a\nb\nc\n", "PUT 2.=2:\n+B"), "a\nB\nc\n");
        assert_eq!(run("a\nb\nc\n", "PUT 1.=2:\n+Z"), "Z\nc\n");
    }

    #[test]
    fn put_can_grow_a_range() {
        assert_eq!(run("a\nb\n", "PUT 1.=1:\n+x\n+y"), "x\ny\nb\n");
    }

    #[test]
    fn cut_deletes_an_inclusive_range() {
        assert_eq!(run("one\ntwo\nthree\nfour\n", "CUT 2.=3"), "one\nfour\n");
        assert_eq!(run("a\nb\nc\n", "CUT 1.=1"), "b\nc\n");
    }

    #[test]
    fn insert_uses_gap_locators() {
        assert_eq!(run("a\nb\n", "PUT >1:\n+mid"), "a\nmid\nb\n");
        assert_eq!(run("a\nb\n", "PUT <2:\n+mid"), "a\nmid\nb\n");
        assert_eq!(run("a\nb\n", "PUT >$:\n+tail"), "a\nb\ntail\n");
    }

    #[test]
    fn insert_before_first_line() {
        assert_eq!(run("a\nb\n", "PUT <1:\n+top"), "top\na\nb\n");
    }

    #[test]
    fn identical_lines_land_on_the_numbered_one() {
        // Two `}` lines: only the number decides, so no hash collision can
        // redirect the edit.
        assert_eq!(
            run("if x {\n}\nif y {\n}\n", "PUT 4.=4:\n+} // y"),
            "if x {\n}\nif y {\n} // y\n"
        );
    }

    #[test]
    fn cut_and_put_in_one_call_use_original_positions() {
        assert_eq!(
            run("keep\ndrop\ntail\n", "CUT 2.=2\nPUT >1:\n+added"),
            "keep\nadded\ntail\n"
        );
    }

    #[test]
    fn empty_payload_row_writes_a_blank_line() {
        assert_eq!(run("a\nb\n", "PUT 1.=1:\n+"), "\nb\n");
    }

    #[test]
    fn out_of_range_line_is_rejected() {
        let total = split_lines("a\n").len();
        let error = parse("PUT 9.=9:\n+x", total).unwrap_err();
        assert!(error.contains("past the end"), "{error}");
    }

    #[test]
    fn reversed_range_is_rejected() {
        let total = split_lines("a\nb\nc\n").len();
        let error = parse("CUT 3.=1", total).unwrap_err();
        assert!(error.contains("low to high"), "{error}");
    }

    #[test]
    fn stray_payload_row_is_rejected() {
        let error = parse("+orphan", 1).unwrap_err();
        assert!(error.contains("no preceding PUT"), "{error}");
    }

    #[test]
    fn unknown_keyword_is_rejected() {
        let error = parse("REPLACE 1", 1).unwrap_err();
        assert!(error.contains("not a hunk keyword"), "{error}");
    }

    #[test]
    fn crlf_files_keep_their_line_endings() {
        assert_eq!(run("a\r\nb\r\n", "PUT 2.=2:\n+B"), "a\r\nB\r\n");
    }
}
