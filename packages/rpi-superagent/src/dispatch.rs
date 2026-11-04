//! The `dispatch` tool: run one wave of the DAG.
//!
//! Scheduling is deterministic code, not a model decision: the ready set is
//! "pending nodes whose dependencies are all done", and it is executed with at
//! most `maxParallel` concurrent child processes. One call advances **one wave**,
//! so the orchestrator sees the results and can reshape the graph before the
//! next call.

use crate::control::Control;
use crate::graph::{Graph, Status};
use crate::kit::{progress, result_with_details, Progress};
use crate::spawn::{self, POLL_SLICE};
use crate::{lock_graph, lock_graph_for_wave};
use serde_json::Value;
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

const DEFAULT_MAX_PARALLEL: usize = 4;
const MAX_PARALLEL_CAP: usize = 16;
const RESULT_SUMMARY_BYTES: usize = 2_000;
/// Fallback verification budget when a node somehow has no timeout (never in
/// practice: `Node::from_value` clamps every node to at least
/// `MIN_TIMEOUT_SECS`).
const MIN_VERIFY_SECS: u64 = 10;

pub fn dispatch(
    params: &Value,
    control: &Control,
    progress_tx: &Progress,
) -> Result<String, String> {
    dispatch_with(params, control, progress_tx, run_wave)
}

pub(super) fn dispatch_with(
    params: &Value,
    control: &Control,
    progress_tx: &Progress,
    run: impl FnOnce(
        &Graph,
        &[String],
        usize,
        &Control,
        &Progress,
        &crate::host::Selection,
    ) -> Vec<(String, Result<String, String>)>,
) -> Result<String, String> {
    control.check()?;
    let max_parallel = params
        .get("maxParallel")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_PARALLEL as u64)
        .clamp(1, MAX_PARALLEL_CAP as u64) as usize;

    let mut guard = lock_graph(params);
    if guard.is_running() {
        return Err("dispatch is already running: wait for the current wave to finish".into());
    }
    let generation = guard.generation();
    let graph = guard.graph_mut();
    if graph.nodes.is_empty() {
        return Err("no plan yet: call plan(action=set, nodes=[...]) first".into());
    }

    let ready = graph.ready();
    if ready.is_empty() {
        if graph.has_pending() {
            let blocked = graph.pending_ids().join(", ");
            return Err(format!(
                "no progress: no pending node is ready (blocked by unmet dependencies): {blocked}"
            ));
        }
        return Ok(result_with_details(
            &format!("nothing left to dispatch\n\n{}", graph.render()),
            graph.to_details(),
        ));
    }

    for id in &ready {
        if let Some(node) = graph.get_mut(id) {
            node.status = Status::Running;
        }
    }

    // Snapshot before releasing the lock into the wave: the children run for a
    // long time and must not hold the state lock (a blocked wave must not block
    // `plan`, and vice versa).
    let snapshot = graph.clone();
    drop(guard);
    // Nodes that name neither a provider nor a model run on this session's
    // binding, exactly like a standalone `subagent` call.
    let parent = crate::host::parent_selection(params);
    let outcomes = run(
        &snapshot,
        &ready,
        max_parallel,
        control,
        progress_tx,
        &parent,
    );

    // Reject an already stale wave before running acceptance commands, then
    // keep verification outside the graph lock so plan/show and session
    // switches remain responsive. Validate again before applying any results.
    drop(lock_graph_for_wave(params, generation)?);
    let outcomes: Vec<_> = outcomes
        .into_iter()
        .map(|(id, outcome)| {
            let spec = snapshot.get(&id);
            let verified = match &outcome {
                Ok(_) => verify_node(
                    spec.and_then(|node| node.expect_file.as_deref()),
                    spec.and_then(|node| node.verify.as_deref()),
                    spec.map(|node| node.timeout_secs)
                        .unwrap_or(MIN_VERIFY_SECS),
                    control,
                ),
                Err(_) => Ok(()),
            };
            (id, outcome, verified)
        })
        .collect();
    let mut guard = lock_graph_for_wave(params, generation)?;
    let graph = guard.graph_mut();

    // A cancelled wave may not have reached every chunk. Those nodes never
    // started, so restore them to `pending`: leaving them `running` would strand
    // them with no child to finish, and a later dispatch could never pick them up.
    let started: std::collections::HashSet<String> =
        outcomes.iter().map(|(id, _, _)| id.clone()).collect();
    for id in &ready {
        if !started.contains(id) {
            if let Some(node) = graph.get_mut(id) {
                node.status = Status::Pending;
            }
        }
    }

    for (id, outcome, verified) in outcomes {
        if let Some(node) = graph.get_mut(&id) {
            match (outcome, verified) {
                (Ok(text), Ok(())) => {
                    node.status = Status::Done;
                    node.result = Some(summarize(&text));
                }
                (Ok(text), Err(reason)) => {
                    node.status = Status::Failed;
                    node.result = Some(summarize(&format!("{reason}\n---\n{text}")));
                    node.error = Some(reason);
                }
                (Err(error), _) => {
                    node.status = Status::Failed;
                    node.result = Some(summarize(&error));
                    node.error = Some(error);
                }
            }
        }
    }
    graph.propagate_skips();

    let text = format!("dispatched {} node(s)\n\n{}", started.len(), graph.render());
    Ok(result_with_details(&text, graph.to_details()))
}

/// Run `ids` with at most `max_parallel` children at a time, preserving input
/// order in the returned pairs.
pub(super) fn run_wave(
    graph: &Graph,
    ids: &[String],
    max_parallel: usize,
    control: &Control,
    progress_tx: &Progress,
    parent: &crate::host::Selection,
) -> Vec<(String, Result<String, String>)> {
    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(max_parallel.max(1)) {
        // Cancellation can arrive after dispatch marks the ready set running
        // but before the first chunk starts. Leave these nodes without outcomes
        // so dispatch restores them to pending instead of failing a dead branch.
        if control.is_cancelled() {
            break;
        }
        progress(
            progress_tx,
            &format!(
                "[dispatch] running {} node(s): {}",
                chunk.len(),
                chunk.join(", ")
            ),
        );
        let chunk_results = std::thread::scope(|scope| {
            let handles: Vec<(String, _)> = chunk
                .iter()
                .map(|id| {
                    let node = graph.get(id).cloned().expect("ready node exists");
                    let id = id.clone();
                    let control = control.clone();
                    let selection =
                        parent.effective(node.provider.as_deref(), node.model.as_deref());
                    let handle = scope.spawn(move || {
                        spawn::run(
                            &node.task,
                            node.max_turns,
                            node.timeout_secs,
                            selection.provider(),
                            selection.model(),
                            &control,
                        )
                    });
                    (id, handle)
                })
                .collect();
            handles
                .into_iter()
                .map(|(id, handle)| {
                    let outcome = handle
                        .join()
                        .unwrap_or_else(|_| Err("subagent thread panicked".into()));
                    (id, outcome)
                })
                .collect::<Vec<_>>()
        });
        out.extend(chunk_results);
        // A cancelled wave must not start the next chunk.
        if control.is_cancelled() {
            break;
        }
    }
    out
}

/// Check a node's declared acceptance criteria after its child exits.
fn verify_node(
    expect_file: Option<&str>,
    verify: Option<&str>,
    timeout_secs: u64,
    control: &Control,
) -> Result<(), String> {
    if let Some(file) = expect_file {
        let path = std::path::Path::new(file);
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|dir| dir.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        };
        if !resolved.exists() {
            return Err(format!("expected file was not produced: {file}"));
        }
    }
    if let Some(command) = verify {
        run_verify_command(command, timeout_secs, control)?;
    }
    Ok(())
}

/// Run a node's `verify` shell command with the same timeout setting and
/// cancellation control as its child, using a fresh verification deadline.
///
/// It used to be a bare `.status()`: a hanging acceptance command blocked
/// `dispatch` (and the graph lock) forever, and an inherited stdout/stderr could
/// corrupt the host's own stdio. Output is now captured and bounded, so a
/// timeout or a failed command surfaces as a clear node result.
fn run_verify_command(command: &str, timeout_secs: u64, control: &Control) -> Result<(), String> {
    control.check()?;
    #[cfg(windows)]
    let mut command_builder = {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("cmd.exe");
        // cmd.exe needs its own quoting, not Command::arg's C-style escaping.
        // /S strips the outer pair while retaining quotes inside the script.
        cmd.args(["/D", "/S", "/C"])
            .raw_arg(format!("\"{command}\""));
        cmd
    };
    #[cfg(not(windows))]
    let mut command_builder = {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    };
    command_builder
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let (mut child, tree) = spawn::spawn_command(&mut command_builder)
        .map_err(|err| format!("could not run verification command: {err}"))?;

    let stdout = child.stdout.take().map(spawn::spawn_reader);
    let stderr = child.stderr.take().map(spawn::spawn_reader);
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if control.is_cancelled() {
            spawn::kill_and_reap(&mut child, &tree);
            return Err("verification cancelled".into());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            spawn::kill_and_reap(&mut child, &tree);
            return Err(format!("verification timed out after {timeout_secs}s"));
        }
        match child.wait_timeout(remaining.min(POLL_SLICE)) {
            Ok(Some(status)) => {
                let out = match spawn::collect_reader(stdout, deadline, control) {
                    Ok(text) => text,
                    Err(error) => {
                        spawn::kill_and_reap(&mut child, &tree);
                        return Err(error);
                    }
                };
                let err = match spawn::collect_reader(stderr, deadline, control) {
                    Ok(text) => text,
                    Err(error) => {
                        spawn::kill_and_reap(&mut child, &tree);
                        return Err(error);
                    }
                };
                if !status.success() {
                    let detail = if err.trim().is_empty() { out } else { err };
                    return Err(format!(
                        "verification command failed: {command}: {}",
                        truncate_detail(&detail)
                    ));
                }
                return Ok(());
            }
            Ok(None) => continue,
            Err(err) => {
                spawn::kill_and_reap(&mut child, &tree);
                return Err(format!("verification wait failed: {err}"));
            }
        }
    }
}

/// Bound a verification command's captured output before it reaches the model.
fn truncate_detail(text: &str) -> String {
    const LIMIT: usize = 2_000;
    if text.len() <= LIMIT {
        return text.trim().to_string();
    }
    let mut end = LIMIT;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", text[..end].trim_end())
}

fn summarize(text: &str) -> String {
    if text.len() <= RESULT_SUMMARY_BYTES {
        return text.to_string();
    }
    let mut end = RESULT_SUMMARY_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn verify_command_supports_quoted_file_paths() {
        let path =
            std::env::temp_dir().join(format!("rpi verify quoted {}.txt", std::process::id()));
        let command = format!("echo verified>\"{}\"", path.display());
        let result = run_verify_command(&command, 5, &Control::default());
        let contents = std::fs::read_to_string(&path);
        let _ = std::fs::remove_file(path);
        assert!(result.is_ok(), "quoted verification failed: {result:?}");
        assert_eq!(contents.unwrap().trim(), "verified");
    }

    #[test]
    fn summarize_bounds_and_stays_char_safe() {
        assert_eq!(summarize("ok"), "ok");
        let long = "あ".repeat(RESULT_SUMMARY_BYTES);
        let text = summarize(&long);
        assert!(text.ends_with('…'));
        assert!(text.len() <= RESULT_SUMMARY_BYTES + '…'.len_utf8());
    }

    #[test]
    fn verify_node_checks_declared_files() {
        assert!(verify_node(None, None, 5, &Control::default()).is_ok());
        assert!(verify_node(Some("Cargo.toml"), None, 5, &Control::default()).is_ok());
        assert!(verify_node(
            Some("no-such-file-orchestrator.txt"),
            None,
            5,
            &Control::default()
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn verify_node_runs_declared_commands() {
        assert!(verify_node(None, Some("true"), 5, &Control::default()).is_ok());
        assert!(verify_node(None, Some("false"), 5, &Control::default()).is_err());
    }

    /// Regression: a hanging acceptance command used to block `dispatch` — and
    /// the graph lock — forever. It must give up at the node's budget.
    #[cfg(unix)]
    #[test]
    fn verify_command_times_out_instead_of_blocking_forever() {
        let started = Instant::now();
        let error = verify_node(None, Some("sleep 30"), 1, &Control::default()).unwrap_err();
        assert!(error.contains("timed out"), "got: {error}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "verify ignored its timeout: {:?}",
            started.elapsed()
        );
    }

    /// Regression: a cancelled call must abandon a running acceptance command.
    #[cfg(unix)]
    #[test]
    fn verify_command_observes_cancellation() {
        let control = Control::default();
        let canceller = control.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            canceller.cancel();
        });
        let started = Instant::now();
        let error = verify_node(None, Some("sleep 30"), 30, &control).unwrap_err();
        let _ = handle.join();
        assert!(error.contains("cancelled"), "got: {error}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "cancel took too long: {:?}",
            started.elapsed()
        );
    }

    /// Regression: a failed acceptance command must report its captured output
    /// rather than inheriting the host's stdio and losing the diagnostic.
    #[cfg(unix)]
    #[test]
    fn verify_command_reports_captured_output() {
        let error =
            verify_node(None, Some("echo boom >&2; exit 3"), 5, &Control::default()).unwrap_err();
        assert!(error.contains("boom"), "stderr not captured: {error}");
    }

    /// Regression: nodes whose chunk was never reached during a cancelled wave
    /// used to stay `running` forever and could never be dispatched again.
    #[test]
    fn cancelled_wave_restores_unstarted_nodes_to_pending() {
        let mut graph = Graph::from_nodes_value(&serde_json::json!([
            {"id": "a", "task": "a"},
            {"id": "b", "task": "b"}
        ]))
        .unwrap();
        for id in ["a", "b"] {
            graph.get_mut(id).unwrap().status = Status::Running;
        }
        // Only `a` produced an outcome; `b`'s chunk never started.
        let ready = vec!["a".to_string(), "b".to_string()];
        let started: std::collections::HashSet<&String> = [&ready[0]].into_iter().collect();
        for id in &ready {
            if !started.contains(id) {
                if let Some(node) = graph.get_mut(id) {
                    node.status = Status::Pending;
                }
            }
        }
        assert_eq!(graph.get("a").unwrap().status, Status::Running);
        assert_eq!(graph.get("b").unwrap().status, Status::Pending);
        // The restored node is schedulable again, so the wave can resume.
        assert_eq!(graph.ready(), vec!["b".to_string()]);
    }
}
