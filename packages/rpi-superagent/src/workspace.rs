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

/// The directory that holds workers' saved sessions.
const SESSION_ROOT: &str = ".rpi/superagent-sessions";

/// Where a worker's saved session lives.
///
/// The directory level is the **parent session id**. That is what makes a
/// worker attributable to the session that dispatched it: a worker's own file
/// records only `id`/`cwd`/`version` — the host never writes a parent field —
/// and its `--name` is `<taskId>-<nodeId>`, which does not name the parent
/// either. A task id is unique per session, so it separates two parents just as
/// well, and that was this argument's value until a reader needed to know
/// *whose* worker a file was.
///
/// Passed to the child explicitly because the host's default would be the
/// parent project's `.rpi/sessions/`, mixing worker history into the user's
/// repository.
///
/// One directory per parent also keeps two parents' workers apart: they would
/// otherwise write `<name>.jsonl` under the same folder, and the second
/// `find_session_id` would resolve the *first* parent's file and resume a
/// stranger's history — or the two would collide outright with "already open
/// in another rpi process".
///
/// Falls back to the flat directory when there is no parent id, so a caller
/// that never had one behaves exactly as before. Returns `None` when there is
/// no usable working directory; the caller then leaves the child ephemeral.
pub fn session_dir_for(parent_session_id: Option<&str>) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let dir = match parent_session_id.map(sanitize).filter(|id| !id.is_empty()) {
        Some(parent) => cwd.join(SESSION_ROOT).join(parent),
        None => cwd.join(SESSION_ROOT),
    };
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

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

/// The session name a node's worker runs under.
///
/// Always `<taskId>-<nodeId>`, never whatever the node asked for. One session per
/// node is a rule the engine enforces rather than a convention the model follows:
/// resuming one worker across nodes is what lets a node inherit the previous
/// node's `expectFile` / `verify` (they are written into the prompt), so it can
/// satisfy the *old* criteria and still pass every check.
///
/// Split out from the scheduler so the rule is testable without spawning an
/// `rpi` process.
pub fn engine_session_name(task_id: Option<&str>, node_id: &str) -> String {
    match task_id.map(str::trim).filter(|t| !t.is_empty()) {
        Some(task) => format!("{}-{}", sanitize(task), sanitize(node_id)),
        None => sanitize(node_id),
    }
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

    /// Two parent sessions in one project must not share a session directory,
    /// or one would resume the other's worker history.
    #[test]
    fn a_parent_session_gets_its_own_session_directory() {
        let dir = session_dir_for(Some("abc")).expect("a parent id has a directory");
        assert!(
            dir.ends_with("superagent-sessions/abc"),
            "the parent id must be a directory level: {dir:?}"
        );
    }

    /// No parent id: the flat directory every earlier worker used.
    #[test]
    fn no_parent_session_falls_back_to_the_flat_directory() {
        let dir = session_dir_for(None).expect("no parent id still has a directory");
        assert!(
            dir.ends_with("superagent-sessions"),
            "the old path must be unchanged: {dir:?}"
        );
        // An id that sanitizes to nothing is not a directory level either.
        assert_eq!(Some(dir), session_dir_for(Some("//")));
    }

    /// One session per node, and the name is the engine's, not the caller's.
    #[test]
    fn a_nodes_session_is_named_after_its_task_and_node() {
        assert_eq!(engine_session_name(Some("S5"), "a"), "S5-a");
        assert_eq!(engine_session_name(Some("S5"), "a"), "S5-a");
        // No task id: the node id alone still keeps nodes apart.
        assert_eq!(engine_session_name(None, "a"), "a");
        // `sanitize` **drops** unsafe characters rather than replacing them, so
        // `T/1` and `T1` produce the same name. Node ids come from the model and
        // are usually plain, but this is a real (if unlikely) collision source —
        // noted here so nobody assumes the name is injective.
        assert_eq!(engine_session_name(Some("T/1"), "n:1"), "T1-n1");
    }
}
