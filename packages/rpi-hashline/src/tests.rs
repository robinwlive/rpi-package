//! Integration tests: the tool entry points against a real file on disk.

use super::*;
use serde_json::json;

/// A scoped temporary directory, removed on drop.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        // A per-test counter makes the name unique even when two tests start in
        // the same nanosecond.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "{prefix}-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(&base).unwrap();
        Self(base)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Write `content` to `f.txt` in a fresh temp dir.
fn fixture(content: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new("rpi-hashline");
    let path = dir.path().join("f.txt");
    fs::write(&path, content).unwrap();
    (dir, path)
}

/// The snapshot tag from a `read` header, e.g. `#1A2B` -> `1A2B`.
fn tag_of(printed: &str) -> String {
    printed
        .split('#')
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

#[test]
fn read_prints_a_tag_and_numbered_lines() {
    let (_dir, path) = fixture("alpha\nbeta\n");
    let out = run_read(&json!({ "path": path.to_str().unwrap() })).unwrap();
    assert!(out.contains("2 lines"), "{out}");
    assert!(out.contains("1:alpha"), "{out}");
    assert!(out.contains("2:beta"), "{out}");
    assert_eq!(tag_of(&out).len(), anchors::TAG_LEN);
}

#[test]
fn read_window_reports_where_to_continue() {
    let (_dir, path) = fixture("l1\nl2\nl3\nl4\nl5\n");
    let out = run_read(&json!({
        "path": path.to_str().unwrap(), "offset": 2, "limit": 2
    }))
    .unwrap();
    assert!(out.contains("2:l2"), "{out}");
    assert!(out.contains("3:l3"), "{out}");
    assert!(!out.contains("1:l1"), "{out}");
    assert!(out.contains("continue with offset: 4"), "{out}");
}

#[test]
fn read_rejects_offset_past_the_end() {
    let (_dir, path) = fixture("a\n");
    let error = run_read(&json!({ "path": path.to_str().unwrap(), "offset": 9 })).unwrap_err();
    assert!(error.contains("past the end"), "{error}");
}

#[test]
fn read_handles_empty_files() {
    let (_dir, path) = fixture("");
    let out = run_read(&json!({ "path": path.to_str().unwrap() })).unwrap();
    assert!(out.contains("empty file"), "{out}");
}

#[test]
fn edit_replaces_by_line_number_and_writes_the_file() {
    let (_dir, path) = fixture("one\ntwo\nthree\n");
    let printed = run_read(&json!({ "path": path.to_str().unwrap() })).unwrap();
    let tag = tag_of(&printed);

    let result = run_edit(&json!({
        "path": path.to_str().unwrap(),
        "tag": tag,
        "patch": "PUT 2.=2:\n+TWO"
    }))
    .unwrap();
    assert!(result.contains("1 line(s) replaced"), "{result}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "one\nTWO\nthree\n");
}

#[test]
fn a_stale_tag_leaves_the_file_untouched() {
    let (_dir, path) = fixture("a\nb\n");
    let error = run_edit(&json!({
        "path": path.to_str().unwrap(),
        "tag": "0000",
        "patch": "PUT 1.=1:\n+X"
    }))
    .unwrap_err();
    assert!(error.contains("changed since it was read"), "{error}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "a\nb\n");
}

#[test]
fn edit_requires_a_patch() {
    let (_dir, path) = fixture("a\n");
    let error = run_edit(&json!({ "path": path.to_str().unwrap(), "patch": "" })).unwrap_err();
    assert!(error.contains("non-empty string"), "{error}");
}

#[test]
fn cut_and_insert_compose_in_one_call() {
    let (_dir, path) = fixture("keep\ndrop\ntail\n");
    run_edit(&json!({
        "path": path.to_str().unwrap(),
        "patch": "CUT 2.=2\nPUT >1:\n+added"
    }))
    .unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "keep\nadded\ntail\n");
}

#[test]
fn an_out_of_range_line_reports_the_file_length() {
    let (_dir, path) = fixture("a\n");
    let error = run_edit(&json!({
        "path": path.to_str().unwrap(),
        "patch": "PUT 9.=9:\n+X"
    }))
    .unwrap_err();
    assert!(error.contains("past the end of the file (1 lines)"), "{error}");
}

#[test]
fn identical_lines_are_addressed_by_number_not_by_text() {
    let (_dir, path) = fixture("if x {\n}\nif y {\n}\n");
    run_edit(&json!({
        "path": path.to_str().unwrap(),
        "patch": "PUT 4.=4:\n+} // y"
    }))
    .unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "if x {\n}\nif y {\n} // y\n"
    );
}

#[test]
fn both_tool_schemas_are_valid_json() {
    // A malformed schema is rejected by the host at registration, silently
    // leaving the built-in tool in place, so this is checked here instead.
    for (label, source) in [
        ("read", READ_PARAMETERS),
        ("edit", EDIT_PARAMETERS),
    ] {
        let parsed: Value = serde_json::from_str(source)
            .unwrap_or_else(|error| panic!("{label} schema is not valid JSON: {error}"));
        assert_eq!(parsed["type"], "object", "{label}");
    }
}

#[test]
fn schemas_advertise_the_fields_the_handlers_read() {
    let read: Value = serde_json::from_str(READ_PARAMETERS).unwrap();
    assert_eq!(read["required"][0], "path");
    let edit: Value = serde_json::from_str(EDIT_PARAMETERS).unwrap();
    let required: Vec<&str> = edit["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert!(required.contains(&"path"));
    assert!(required.contains(&"patch"));
    assert!(edit["properties"]["tag"].is_object());
}
