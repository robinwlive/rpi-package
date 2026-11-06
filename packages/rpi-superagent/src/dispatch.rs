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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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

/// Queue a wave and return immediately.
///
/// The tool reports which nodes it queued; the wave itself runs on the pass
/// scheduler ([`crate::passes`]) and its outcome reaches the session as a relay
/// message. Nothing is left waiting on the tool call.
///
/// A wave with no ready node is refused rather than queued: an empty launch would
/// spin on the queue's retry budget for no reason.
fn queue_wave(params: &Value, max_parallel: usize) -> Result<String, String> {
    // A queued wave runs on a background thread, and a thread dies with its
    // host. A one-shot `rpi -p` run therefore cannot honour `async`: accepting it
    // would tell the caller the work was queued and then silently drop it, which
    // is worse than refusing. Refuse, and say what to do instead.
    if crate::host::host_exits_after_this_turn() {
        return Err(
            "async dispatch needs a host that outlives the turn: this run is one-shot \
             (`rpi -p` or a bare prompt), so a queued wave would be dropped when it exits. \
             Either drop `async` to run the wave inline, or run under `--mode rpc` / an \
             interactive session, which stays up to drive the queue."
                .into(),
        );
    }

    let task_id = params
        .get("taskId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .or_else(crate::graph_task_id)
        .ok_or(
            "action=dispatch with async requires a taskId: pass plan(taskId=...) so the result \
             has a subject to report against",
        )?;

    let ready = {
        let guard = lock_graph(params);
        let graph = guard.graph();
        if graph.nodes.is_empty() {
            return Err("no plan yet: call plan(action=set, nodes=[...]) first".into());
        }
        let ready = graph.ready();
        if ready.is_empty() {
            if graph.has_pending() {
                return Err(format!(
                    "no progress: no pending node is ready (blocked by unmet dependencies): {}",
                    graph.pending_ids().join(", ")
                ));
            }
            return Ok(crate::kit::result_with_details(
                &format!("nothing left to dispatch\n\n{}", graph.render()),
                graph.to_details(),
            ));
        }
        ready
    };

    crate::effects().enqueue(crate::effects::Effect::Launch {
        task_id: task_id.clone(),
        nodes: ready.clone(),
        max_parallel,
        // Captured here because this is the last moment the host's invocation
        // handle exists; the background pass cannot ask which model to use.
        binding: crate::effects::Binding::from_params(params),
        // Same reason: the graph is keyed by session, so the pass must run as
        // this session or it would clear the plan it is meant to execute.
        session: crate::session_of(params),
    });
    // Do not wait for the scheduler's tick: the caller just asked for this.
    if let Some(passes) = crate::PASSES.get() {
        passes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .request();
    }

    Ok(crate::kit::result_with_details(
        &format!(
            "queued {n} node(s) for task `{task_id}`: {ids}\n\nThe wave runs in the background; \
             its result arrives as a message. Check it with superagent_task(action=list).",
            n = ready.len(),
            ids = ready.join(", ")
        ),
        serde_json::json!({"async": true, "taskId": task_id, "queued": ready}),
    ))
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
        &Arc<Mutex<Vec<u32>>>,
        &str,
    ) -> Vec<(String, Result<String, String>)>,
) -> Result<String, String> {
    control.check()?;
    let max_parallel = params
        .get("maxParallel")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_PARALLEL as u64)
        .clamp(1, MAX_PARALLEL_CAP as u64) as usize;

    // Queue the wave and return, when the caller asked for that. The work then
    // runs on the pass scheduler and reports back as a relay message, which is
    // what lets a session keep taking requests while a wave is in flight.
    if params
        .get("async")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return queue_wave(params, max_parallel);
    }

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

    // Record the start of the wave on the long task, not just its end. The
    // task's heartbeat is the last time the host wrote it, so a wave that
    // takes a long time needs a write when it begins as well as when it
    // finishes — otherwise the task looks abandoned while it is running.
    let pids = Arc::new(Mutex::new(Vec::new()));
    let heartbeat = graph
        .task_id
        .clone()
        .and_then(|task_id| start_heartbeat(task_id, Arc::clone(&pids)));
    if let Some(task_id) = graph.task_id.clone() {
        if let Err(error) = record_progress(&task_id, graph, &[]) {
            eprintln!("could not record the start of task `{task_id}`: {error}");
        }
    }

    // Snapshot before releasing the lock into the wave: the children run for a
    // long time and must not hold the state lock (a blocked wave must not block
    // `plan`, and vice versa).
    let snapshot = graph.clone();
    drop(guard);
    // Nodes that name neither a provider nor a model run on this session's
    // binding, exactly like a standalone `subagent` call.
    //
    // A background pass has no invocation handle, so it cannot ask the host; it
    // carries the binding captured at enqueue time in `params.binding` instead.
    // Without that a queued wave's children would resolve the *default* provider
    // rather than the one this session runs on.
    let parent = match params.get("binding") {
        Some(binding) => crate::host::Selection {
            provider: binding
                .get("provider")
                .and_then(Value::as_str)
                .map(str::to_string),
            model: binding
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        None => crate::host::parent_selection(params),
    };
    // One id for the wave, so every isolated node lands under the same run
    // directory and two waves never share one.
    let run_id = crate::workspace::run_id("wave");
    let outcomes = run(
        &snapshot,
        &ready,
        max_parallel,
        control,
        progress_tx,
        &parent,
        &pids,
        &run_id,
    );

    // Reject an already stale wave before running acceptance commands, then
    // keep verification outside the graph lock so plan/show and session
    // switches remain responsive. Validate again before applying any results.
    //
    // A failure here returns early, so the heartbeat must already be stopped
    // by then — otherwise a session switch would leave it writing to a
    // session it no longer serves.
    let wave = lock_graph_for_wave(params, generation);
    if let Some((handle, stop)) = heartbeat {
        stop.store(true, Ordering::Release);
        let _ = handle.join();
    }
    drop(wave?);
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
                    spec.and_then(|node| {
                        if !node.isolation {
                            return None;
                        }
                        let dir = std::env::current_dir().ok()?;
                        let dir = crate::workspace::node_dir(&dir, &run_id, &node.id)?;
                        Some(dir)
                    })
                    .as_deref(),
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

    // Record the wave's outcome on the long task this graph belongs to, if it
    // names one. A failure to record must not fail the dispatch: the work ran,
    // and the wave's own result is the authoritative account of it. The task
    // record is a convenience for the next reader, not the source of truth.
    let child_pids = pids.lock().map(|pids| pids.clone()).unwrap_or_default();
    // The wave is over: these children are finished, so they need no guard
    // against the reaper.
    for pid in &child_pids {
        release_child(*pid);
    }
    let task_note = match graph.task_id.clone() {
        Some(task_id) => match record_progress(&task_id, graph, &child_pids) {
            Ok(note) => note,
            Err(error) => format!("could not record task `{task_id}`: {error}"),
        },
        None => String::new(),
    };

    // The wave's working directories are scaffolding, not output: the files a
    // node produced are what matter, and those are inside them. So the
    // directories go, and the run's result stays in the transcript.
    if let Ok(cwd) = std::env::current_dir() {
        crate::workspace::cleanup_run(&cwd, &run_id);
    }

    let mut text = format!("dispatched {} node(s)\n\n{}", started.len(), graph.render());
    if !task_note.is_empty() {
        text.push_str("\n\n");
        text.push_str(&task_note);
    }
    Ok(result_with_details(&text, graph.to_details()))
}

/// The child processes this process started.
///
/// The reaper must not kill these: they are live work, and the host may run
/// `list` while a wave is still running.
static OWNED_CHILDREN: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();

/// The children this process started.
fn owned_children() -> &'static Mutex<Vec<u32>> {
    OWNED_CHILDREN.get_or_init(|| Mutex::new(Vec::new()))
}

/// Whether this process started the given child.
pub(crate) fn owns_child(pid: u32) -> bool {
    owned_children()
        .lock()
        .map(|children| children.contains(&pid))
        .unwrap_or(false)
}

/// Remember a child this process started.
pub(crate) fn remember_child(pid: u32) {
    if let Ok(mut children) = owned_children().lock() {
        children.push(pid);
    }
}

/// Forget a child this process started.
///
/// The list is a guard against the reaper killing live work; a child that has
/// finished needs no guard, and keeping every pid forever would grow it
/// without bound.
pub(crate) fn release_child(pid: u32) {
    if let Ok(mut children) = owned_children().lock() {
        children.retain(|owned| *owned != pid);
    }
}

/// How often a running wave refreshes its task's heartbeat.
const HEARTBEAT_INTERVAL_SECS: u64 = 60;

/// Keep the task's heartbeat alive while a wave runs.
///
/// The heartbeat is the last time the host wrote the task, so a wave that
/// runs longer than the abandonment threshold would look abandoned while it
/// is still working. A thread that writes a heartbeat every minute keeps the
/// task's age below the threshold for as long as the wave lives, and stops
/// writing when the wave ends.
///
/// The thread writes the task's own snapshot rather than the graph's derived
/// state: the graph is owned by the dispatching thread, and a heartbeat only
/// has to say "this is still alive", not what the state is.
fn start_heartbeat(
    task_id: String,
    pids: Arc<Mutex<Vec<u32>>>,
) -> Option<(std::thread::JoinHandle<()>, Arc<AtomicBool>)> {
    if task_id.is_empty() {
        return None;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let handle = std::thread::spawn(move || loop {
        // Wake often enough that a stop is noticed promptly, but write only
        // every interval.
        let mut waited = 0;
        while waited < HEARTBEAT_INTERVAL_SECS {
            if flag.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
            waited += 1;
        }
        if flag.load(Ordering::Acquire) {
            return;
        }
        let Ok(Some(mut task)) = crate::task::read_task(&task_id) else {
            // The task is gone, or the host cannot answer. Stop trying.
            return;
        };
        if task.stage != crate::task::Stage::Working {
            return;
        }
        task.branch_id = crate::host::current_branch_id();
        // The heartbeat carries the live children's ids, so a process that
        // starts later can find and kill children this one left behind.
        task.pids = pids.lock().map(|pids| pids.clone()).unwrap_or_default();
        // Who started those children. A sweeper in another process has to be
        // able to tell them from orphans: while this host is alive they are
        // its work, not leftovers.
        task.owner_pid = Some(std::process::id());
        if crate::task::append_task(&task).is_err() {
            return;
        }
    });
    Some((handle, stop))
}

/// Write the graph's implied state onto its long task.
fn record_progress(
    task_id: &str,
    graph: &Graph,
    child_pids: &[u32],
) -> Result<String, String> {
    let stage = crate::task::stage_for_graph(&graph.nodes);
    let error = crate::task::error_for_graph(&graph.nodes);
    let nodes: Vec<String> = graph.nodes.iter().map(|node| node.id.clone()).collect();

    let mut task = crate::task::read_task(task_id)?.unwrap_or_else(|| crate::task::Task {
        task_id: task_id.to_string(),
        ..crate::task::Task::default()
    });
    task.stage = stage;
    task.nodes = nodes;
    task.error = error;
    task.branch_id = crate::host::current_branch_id();
    task.pids = child_pids.to_vec();
    task.owner_pid = Some(std::process::id());
    crate::task::append_task(&task)?;
    Ok(format!("task `{task_id}` → {}", stage.as_str()))
}

/// Run `ids` with at most `max_parallel` children at a time, preserving input
/// order in the returned pairs.
// The argument list mirrors what a wave needs; grouping them into a struct
// would mean threading one more thing through every caller for no gain.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_wave(
    graph: &Graph,
    ids: &[String],
    max_parallel: usize,
    control: &Control,
    progress_tx: &Progress,
    parent: &crate::host::Selection,
    pids: &Arc<Mutex<Vec<u32>>>,
    run_id: &str,
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
                    // A node that asked for isolation gets a directory of its
                    // own, so two parallel nodes cannot overwrite each other.
                    let node_dir = if node.isolation {
                        std::env::current_dir()
                            .ok()
                            .and_then(|cwd| crate::workspace::prepare(&cwd, run_id, &node.id))
                    } else {
                        None
                    };
                    let handle = scope.spawn(move || {
                        let exe = match std::env::current_exe() {
                            Ok(exe) => exe,
                            Err(error) => {
                                return Err(format!("cannot resolve the rpi executable: {error}"))
                            }
                        };
                        let mut on_started = |pid: u32| {
                            if let Ok(mut pids) = pids.lock() {
                                pids.push(pid);
                            }
                            remember_child(pid);
                        };
                        // A named session resumes that worker; its directory is
                        // passed explicitly so worker history never lands in the
                        // user's repository.
                        let session_dir = crate::workspace::session_dir();
                        spawn::run_child_reporting(
                            &exe,
                            &node.task,
                            node.max_turns,
                            node.timeout_secs,
                            selection.provider(),
                            selection.model(),
                            &control,
                            node_dir.as_deref(),
                            node.session.as_deref(),
                            session_dir.as_deref(),
                            &mut on_started,
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
    cwd: Option<&std::path::Path>,
) -> Result<(), String> {
    if let Some(file) = expect_file {
        let path = std::path::Path::new(file);
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            // An isolated node's acceptance check looks in the node's own
            // directory, which is where its child wrote the file.
            cwd.map(|dir| dir.join(path))
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .map(|dir| dir.join(path))
                        .unwrap_or_else(|_| path.to_path_buf())
                })
        };
        if !resolved.exists() {
            return Err(format!("expected file was not produced: {file}"));
        }
    }
    if let Some(command) = verify {
        run_verify_command(command, timeout_secs, control, cwd)?;
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
fn run_verify_command(
    command: &str,
    timeout_secs: u64,
    control: &Control,
    cwd: Option<&std::path::Path>,
) -> Result<(), String> {
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
        // The check runs where the work happened: an isolated node's files are
        // in its own directory.
        .current_dir(cwd.unwrap_or_else(|| std::path::Path::new(".")))
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
        let result = run_verify_command(&command, 5, &Control::default(), None);
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
        assert!(verify_node(None, None, 5, &Control::default(), None).is_ok());
        assert!(verify_node(Some("Cargo.toml"), None, 5, &Control::default(), None).is_ok());
        assert!(verify_node(
            Some("no-such-file-superagent.txt"),
            None,
            5,
            &Control::default(),
            None
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn verify_node_runs_declared_commands() {
        assert!(verify_node(None, Some("true"), 5, &Control::default(), None).is_ok());
        assert!(verify_node(None, Some("false"), 5, &Control::default(), None).is_err());
    }

    /// Regression: a hanging acceptance command used to block `dispatch` — and
    /// the graph lock — forever. It must give up at the node's budget.
    #[cfg(unix)]
    #[test]
    fn verify_command_times_out_instead_of_blocking_forever() {
        let started = Instant::now();
        let error = verify_node(None, Some("sleep 30"), 1, &Control::default(), None).unwrap_err();
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
        let error = verify_node(None, Some("sleep 30"), 30, &control, None).unwrap_err();
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
            verify_node(None, Some("echo boom >&2; exit 3"), 5, &Control::default(), None).unwrap_err();
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
