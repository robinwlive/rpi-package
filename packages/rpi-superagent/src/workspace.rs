//! Per-node working directories.
//!
//! ## Why
//!
//! Nodes that share one working directory have to be given non-overlapping
//! write ownership by hand. That is a rule the model has to remember, and a
//! rule that is forgotten shows up as two nodes overwriting each other's
//! files in the middle of a run — with no error anywhere.
//!
//! A node that asks for isolation gets a directory of its own instead. The
//! child process runs there, and the acceptance checks run there too, so
//! `expectFile` names a path relative to that node's own work.
//!
//! ## Where the directories live
//!
//! Under `<cwd>/.rpi/superagent-work/<run id>/<node id>`. A directory under
//! the working directory rather than in a temporary place, because the work is
//! the point: a node's files have to still be there when the run is over and
//! somebody goes to look at them.
//!
//! The `run id` is what keeps two runs apart. Two dispatches in the same
//! project must not land in each other's directories.

use std::path::{Path, PathBuf};

/// The directory that holds every run's working directories.
const ROOT: &str = ".rpi/superagent-work";

/// The directory a node works in, if it asked for isolation.
///
/// `None` means the node shares the process's working directory.
pub fn node_dir(cwd: &Path, run_id: &str, node_id: &str) -> Option<PathBuf> {
    let run = sanitize(run_id);
    let node = sanitize(node_id);
    if run.is_empty() || node.is_empty() {
        return None;
    }
    Some(cwd.join(ROOT).join(run).join(node))
}

/// Create the directory a node works in.
///
/// Returns the directory, or `None` when it could not be created — the caller
/// then runs the node in the shared directory rather than failing the wave.
pub fn prepare(cwd: &Path, run_id: &str, node_id: &str) -> Option<PathBuf> {
    let dir = node_dir(cwd, run_id, node_id)?;
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    Some(dir)
}

/// Remove a run's working directories.
///
/// Called when a wave finishes: the work is done, and leaving the directories
/// behind would grow without bound. The files themselves are the run's
/// output, so this only removes the scaffolding, not what was produced.
pub fn cleanup_run(cwd: &Path, run_id: &str) {
    let run = sanitize(run_id);
    if run.is_empty() {
        return;
    }
    let dir = cwd.join(ROOT).join(run);
    let _ = std::fs::remove_dir_all(dir);
}

/// Reduce a name to characters that are safe in a path.
///
/// A node id comes from the model, so it can contain anything. A run id is
/// ours, but the same rule is cheap and keeps one code path.
///
/// A dot is allowed so names like `report.md` survive, but a name that is
/// *only* dots is not: `..` would step up a level and escape the directory it
/// is supposed to stay in.
fn sanitize(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(64)
        .collect();
    // A name made only of dots names a directory level rather than a name.
    if cleaned.chars().all(|c| c == '.') {
        return String::new();
    }
    // A name that starts with dots could still step up: `..foo` is a name, but
    // `..` is not, and neither is a name ending in one.
    let trimmed = cleaned.trim_start_matches('.').trim_end_matches('.');
    if trimmed.is_empty() {
        return String::new();
    }
    trimmed.to_string()
}

/// A run id that is stable for one dispatch and different for the next.
pub fn run_id(seed: &str) -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("{}-{:x}", sanitize(seed), stamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_directory_is_under_the_run() {
        let dir = node_dir(Path::new("/w"), "run1", "n1").unwrap();
        assert_eq!(dir, Path::new("/w/.rpi/superagent-work/run1/n1"));
    }

    #[test]
    fn two_runs_do_not_share_a_directory() {
        let a = node_dir(Path::new("/w"), "run1", "n1").unwrap();
        let b = node_dir(Path::new("/w"), "run2", "n1").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn two_nodes_in_one_run_do_not_share_a_directory() {
        let a = node_dir(Path::new("/w"), "run1", "a").unwrap();
        let b = node_dir(Path::new("/w"), "run1", "b").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn a_node_id_cannot_escape_its_run() {
        // The id comes from the model, so it can contain a path.
        let dir = node_dir(Path::new("/w"), "run1", "../../etc").unwrap();
        assert!(
            dir.starts_with("/w/.rpi/superagent-work/run1"),
            "node id must not escape its run: {dir:?}"
        );
        assert_eq!(dir.components().count(), Path::new("/w/.rpi/superagent-work/run1/x").components().count());
    }

    #[test]
    fn a_run_id_cannot_escape_the_root() {
        let dir = node_dir(Path::new("/w"), "../../etc", "n1").unwrap();
        assert!(
            dir.starts_with("/w/.rpi/superagent-work"),
            "run id must not escape the root: {dir:?}"
        );
    }

    /// `..` must not step up a level.
    ///
    /// The earlier test used `../../etc`, which the character filter already
    /// caught because of the slashes — so the dot-only case was never covered.
    #[test]
    fn a_name_of_dots_cannot_step_up() {
        assert!(node_dir(Path::new("/w"), "run1", "..").is_none());
        assert!(node_dir(Path::new("/w"), "..", "n1").is_none());
        assert!(node_dir(Path::new("/w"), "run1", "...").is_none());
        // A name that merely starts with dots is still a name.
        let dir = node_dir(Path::new("/w"), "run1", "..report").unwrap();
        assert!(
            dir.starts_with("/w/.rpi/superagent-work/run1"),
            "got: {dir:?}"
        );
    }

    #[test]
    fn an_empty_name_has_no_directory() {
        assert!(node_dir(Path::new("/w"), "", "n1").is_none());
        assert!(node_dir(Path::new("/w"), "run1", "").is_none());
    }

    #[test]
    fn run_ids_differ_between_calls() {
        assert_ne!(run_id("r"), run_id("r"));
    }
}
