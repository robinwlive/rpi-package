//! Detecting an incoherent install: another extension claiming `read` / `edit`.
//!
//! **This is not automatic, and it cannot be.** The host merges registries
//! first-wins and silently: `register_tool` returns a fixed `0` and the `bool`
//! saying "a prior was kept" never crosses the ABI, so a plugin cannot observe
//! that it lost a name. What is available is this check, callable with a
//! declared list.
//!
//! The host merges plugin registries **first-wins and silently**: a competing
//! registration returns status `0` and is dropped with no diagnostic
//! (`rpi-extensions/src/registry.rs`, `absorb()`; the comment there says
//! "v1 silent first-wins"). A plugin cannot see the other registrations, and
//! the directory scan order is whatever `read_dir` returns, so which library
//! wins is not ours to control.
//!
//! That makes a reliable *automatic* pre-flight check impossible from inside a
//! plugin. What is possible is a check the user can ask for, against names the
//! user declares. The authoritative source is the host's own tool list:
//!
//! ```text
//! rpi --debug-system-prompt -p x | grep '^- read'
//! ```
//!
//! This module implements the other half: compare a declared set of extension
//! names against the tools this package owns, so the answer is a sentence rather
//! than a grep.

/// The tool names this package owns. A clone of either breaks the pair: the
/// anchor printed by one tool would be consumed by another.
pub const OWNED: [&str; 2] = ["read", "edit"];

/// A conflict between this package and a declared competitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// What the user called the other component, e.g. `rpi-edit-diagnostics`.
    pub component: String,
    /// Owned tool names it also claims.
    pub names: Vec<String>,
}

/// Compare declared component→tools claims against [`OWNED`].
///
/// `declared` is `(component, claimed tool names)`. Order is preserved so the
/// output reads like the user's own config.
pub fn detect(declared: &[(String, Vec<String>)]) -> Vec<Conflict> {
    let mut found: Vec<Conflict> = Vec::new();
    for (component, tools) in declared {
        let names: Vec<String> = OWNED
            .iter()
            .filter(|owned| tools.iter().any(|tool| tool.eq_ignore_ascii_case(owned)))
            .map(|owned| (*owned).to_string())
            .collect();
        if !names.is_empty() {
            found.push(Conflict {
                component: component.clone(),
                names,
            });
        }
    }
    found
}

/// Parse a declaration list such as
/// `rpi-edit-diagnostics=edit,rpi-foo=read+edit`.
///
/// A component claiming nothing is dropped rather than reported as clean, so a
/// typo cannot look like a pass.
pub fn parse_declared(spec: &str) -> Result<Vec<(String, Vec<String>)>, String> {
    let mut parsed: Vec<(String, Vec<String>)> = Vec::new();
    for entry in spec.split(',').map(str::trim).filter(|part| !part.is_empty()) {
        let (component, tools) = entry.split_once('=').ok_or_else(|| {
            format!("error: `{entry}` must look like `component=tool` or `component=tool|tool`")
        })?;
        let component = component.trim().trim_end_matches(".dylib");
        if component.is_empty() {
            return Err(format!("error: `{entry}` has no component name"));
        }
        let tools: Vec<String> = tools
            .split('|')
            .map(str::trim)
            .filter(|tool| !tool.is_empty())
            .map(str::to_string)
            .collect();
        if tools.is_empty() {
            return Err(format!("error: `{entry}` names no tools"));
        }
        parsed.push((component.to_string(), tools));
    }
    if parsed.is_empty() {
        return Err("error: nothing declared; pass at least one `component=tool`".into());
    }
    Ok(parsed)
}

/// Render the verdict for tool names this session's prompt no longer lists.
///
/// A missing name means the host dropped this package's registration, which
/// happens silently when another extension registered the same name first.
pub fn report_missing(missing: &[&str]) -> String {
    format!(
        "rpi-hashline: this session does not list {} in its tool set, so another \
         extension owns the name and this package's copy was dropped (the host \
         keeps the first registration silently). Anchors printed by whichever \
         copy won will not match this package's expectations. Keep exactly one \
         extension per tool name: rpi-hashline and rpi-edit-diagnostics both \
         claim `edit` and cannot be installed together.",
        missing.join(" or ")
    )
}

/// Render the human-facing verdict.
pub fn report(conflicts: &[Conflict]) -> String {
    if conflicts.is_empty() {
        return "No conflict: nothing declared claims `read` or `edit`.".to_string();
    }
    let mut text = String::from(
        "CONFLICT: the host keeps the FIRST registration of a tool name and silently \
         drops the rest, so these cannot be installed together:",
    );
    for conflict in conflicts {
        text.push_str(&format!(
            "\n  - {} also claims {}",
            conflict.component,
            conflict.names.join(", ")
        ));
    }
    text.push_str(
        "\n  Fix: keep exactly one extension per tool name. Installing both \
         rpi-hashline and rpi-edit-diagnostics leaves one of them silently inactive.",
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(pairs: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        pairs
            .iter()
            .map(|(name, tools)| {
                (
                    (*name).to_string(),
                    tools.iter().map(|tool| (*tool).to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn a_competing_edit_is_reported() {
        let found = detect(&declared(&[("rpi-edit-diagnostics", &["edit"])]));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].names, vec!["edit".to_string()]);
    }

    #[test]
    fn unrelated_tools_are_not_reported() {
        assert!(detect(&declared(&[("rpi-todo", &["todo"])])).is_empty());
    }

    #[test]
    fn a_component_claiming_both_is_named_once() {
        let found = detect(&declared(&[("rpi-hashline-clone", &["read", "edit"])]));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].names.len(), 2);
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(detect(&declared(&[("x", &["EDIT"])])).len(), 1);
    }

    #[test]
    fn a_clean_report_says_so() {
        assert!(report(&[]).contains("No conflict"));
    }

    #[test]
    fn a_conflict_report_names_the_component_and_the_fix() {
        let text = report(&detect(&declared(&[("rpi-edit-diagnostics", &["edit"])])));
        assert!(text.contains("rpi-edit-diagnostics"), "{text}");
        assert!(text.contains("silently"), "{text}");
        assert!(text.contains("Fix:"), "{text}");
    }

    #[test]
    fn declarations_parse_including_a_dylib_suffix() {
        let parsed =
            parse_declared("librpi_edit_diagnostics.dylib=edit, rpi-foo=read|edit").unwrap();
        assert_eq!(parsed[0].0, "librpi_edit_diagnostics");
        assert_eq!(parsed[1].1, vec!["read".to_string(), "edit".to_string()]);
    }

    #[test]
    fn a_malformed_declaration_is_an_error_not_a_pass() {
        assert!(parse_declared("rpi-foo").is_err());
        assert!(parse_declared("=edit").is_err());
        assert!(parse_declared("rpi-foo=").is_err());
        assert!(parse_declared("").is_err());
    }
}
