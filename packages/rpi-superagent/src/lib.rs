#![deny(unsafe_code)]
// The `export_plugin!` expansion exposes the raw-pointer ABI entrypoints the host
// calls. Every other plugin crate in this workspace carries the same allow (see
// rpi-acp, rpi-btw, rpi-subagents, rpi-ui-adapter); without it
// `cargo clippy -p rpi-superagent` fails on the macro expansion alone.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
//
// `deny` rather than `forbid`: the supervisor module needs `fork`, `setsid`
// and `flock`, which have no safe equivalents. It re-enables unsafe for
// itself alone; every other module in this crate stays unsafe-free.

//! `rpi-superagent` — super-agent orchestration for the rpi Rust agent.
//!
//! A subagent is a child `rpi` process. Three tools share one session-scoped
//! task graph:
//!
//! - `plan`     — author/edit the DAG,
//! - `dispatch` — queue the ready set (one wave per call),
//! - `task`     — track a long task across restarts.
//!
//! There is no single-task delegation tool. A blocking one-shot child is what the
//! calling session could do itself, and a non-blocking one is a one-node plan —
//! so it was a second name for the same workflow with none of the reporting.
//!
//! The graph lives in this process's memory for the life of a run. Switching
//! sessions clears it — a human decides whether to resume or start over, rather
//! than the extension silently replaying an old branch. `plan`/`dispatch`
//! results still carry the graph in `details` for inspection. Design and
//! stages: `DESIGN.md`.

mod control;
mod guide;
mod dispatch;
mod effects;
mod effects_handler;
mod effects_log;
mod graph;
mod host;
mod kit;
mod passes;
pub mod passes_daemon;
mod plan;
mod prompt;
mod spawn;
pub mod supervisor;
mod task;
mod workspace;
mod tool_schema;

use serde_json::Value;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// The orchestration graph, scoped to the session the host says it is serving.
///
/// The plan is session state: switching sessions must not let session B see or
/// dispatch session A's plan. The host injects `__rpi.sessionId` into every
/// plugin tool call ([`rpi_plugin_sdk::HOST_CONTEXT_KEY`]), so the current
/// session is known without the model supplying it. A host that predates that
/// field — or an ephemeral session with no id — shares a single fallback graph
/// rather than refusing to work.
#[derive(Default)]
pub(crate) struct GraphState {
    session: Option<String>,
    graph: graph::Graph,
    generation: u64,
}

static GRAPH: OnceLock<Mutex<GraphState>> = OnceLock::new();

fn graph_state() -> &'static Mutex<GraphState> {
    GRAPH.get_or_init(|| Mutex::new(GraphState::default()))
}

/// Whether any node of the shared graph is running right now.
///
/// A background pass asks this before starting a wave: a second wave over the
/// same graph would run nodes the first wave already marked running. It is
/// treated as "try later", never as a failure.
pub(crate) fn graph_state_is_running() -> bool {
    graph_state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .is_running()
}

/// The task id the shared graph belongs to, when it names one.
pub(crate) fn graph_task_id() -> Option<String> {
    graph_state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .graph
        .task_id
        .clone()
}

/// The next wave of `graph`, if any node is ready for it.
///
/// A plan that takes more than one wave needs each wave to queue the next: the
/// queue's row is per task, so retiring it after the first wave would strand every
/// node the first wave unblocked.
pub(crate) fn next_ready_wave(
    graph: &serde_json::Value,
    max_parallel: usize,
    binding: &effects::Binding,
    session: Option<&str>,
    rpi_exe: Option<&str>,
    wave: u64,
) -> Option<effects::Effect> {
    let rebuilt = graph::Graph::from_details(graph).ok()?;
    // A node marked `confirmBeforeNext` just finished, so the next wave waits for
    // a decision instead of running on. Checked here rather than at the caller:
    // both the in-process scheduler and the daemon come through this function,
    // and a gate only one of them honoured would be a gate that leaks.
    //
    // The gate holds for **the wave after its own**, not for every wave after it.
    // `wave` counts the continuations of one task, so a finished gate whose own
    // wave was the previous one blocks wave `n+1` and nothing else. Keying it on
    // "is there a finished gate anywhere" instead would stop the plan one wave
    // short of done, permanently: the gate node stays `done` forever, so every
    // later continuation would also be refused and nobody would notice until the
    // work quietly never finished.
    if rebuilt.nodes.iter().any(|node| {
        node.confirm_before_next
            && matches!(node.status, graph::Status::Done | graph::Status::Failed)
    }) {
        return None;
    }
    let ready = rebuilt.ready();
    if ready.is_empty() {
        return None;
    }
    // The binding, session and executable are carried forward, not re-derived:
    // this wave is a continuation of the last one, and everything the daemon
    // cannot look up has to keep travelling with it.
    let task_id = rebuilt.task_id.clone()?;
    Some(effects::Effect::Launch {
        task_id,
        nodes: ready,
        max_parallel,
        binding: binding.clone(),
        session: session.map(str::to_string),
        graph: Some(graph.clone()),
        rpi_exe: rpi_exe.map(str::to_string),
        wave,
    })
}

/// Install a reconstructed graph as the shared one.
///
/// Used by the background pass: the graph it is running lives in the effect log,
/// not in this process's memory, so it has to be put back before `dispatch` can
/// run it. The generation advances so a wave already in flight is treated as
/// stale rather than applied to a graph it never saw.
pub(crate) fn install_graph(session: Option<String>, graph: graph::Graph) -> Result<(), String> {
    let mut guard = graph_state()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    guard.session = session;
    guard.graph = graph;
    guard.advance_generation();
    Ok(())
}

/// The process-wide effect queue.
///
/// One queue for the whole process rather than one per session: the effects are
/// about work that must happen (launch a wave, relay a result), not about which
/// conversation asked for it. A row names its task, and a task records its own
/// session, so a relay still reaches the right place.
static EFFECTS: OnceLock<effects::Effects> = OnceLock::new();

/// The effect queue, created on first use.
///
/// Durable when a working directory is available: the rows go to a JSONL log
/// under `.rpi/superagent-effects/`, so a queued wave survives the host exiting.
/// Without a directory the queue is memory-only, which still runs work but
/// cannot outlive the process.
pub(crate) fn effects() -> &'static effects::Effects {
    EFFECTS.get_or_init(|| {
        match std::env::current_dir()
            .ok()
            .map(|cwd| crate::effects_log::effects_root(&cwd))
            .map(|root| (root.clone(), crate::effects_log::Log::open(&root, &owner())))
        {
            Some((_root, Ok(log))) => effects::Effects::durable(log),
            // A log that will not open is a reason to lose durability, not to
            // lose the work: fall back to memory and say so.
            Some((root, Err(error))) => {
                eprintln!(
                    "rpi-superagent: effect log at {} unavailable ({error}); queue is memory-only",
                    root.display()
                );
                effects::Effects::new()
            }
            None => effects::Effects::new(),
        }
    })
}

/// The name of this process's effect log.
///
/// One log per agent directory, so two independent rpi installs on one machine do
/// not share a queue. The daemon scans every log, so the name only has to be
/// stable, not meaningful.
pub(crate) fn owner() -> String {
    let dir = std::env::var("RPI_CODING_AGENT_DIR")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "default".to_string());
    // The id is a path; the log name must not be. A short hash keeps logs apart
    // without carrying a path into a file name.
    format!("agent-{:016x}", fnv1a(&dir))
}

/// A 64-bit FNV-1a hash, for turning a path into a short stable name.
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The pass scheduler, started once per process.
///
/// Held in a `OnceLock` so a second plugin registration does not start a second
/// thread over the same queue — two schedulers would race each other's rows.
static PASSES: OnceLock<Mutex<passes::Passes>> = OnceLock::new();

/// The session id the host attached to this tool call, if any.
pub(crate) fn session_of(params: &Value) -> Option<String> {
    params
        .get(rpi_plugin_sdk::HOST_CONTEXT_KEY)
        .and_then(|ctx| ctx.get("sessionId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// Lock the graph belonging to `params`' session.
///
/// A session change **resets** the graph: whether to resume an old plan or start
/// over is a human decision, not something the extension should silently replay.
/// A poisoned lock is recovered rather than panicking, mirroring the SDK's
/// tool-handle convention.
pub(crate) fn lock_graph(params: &Value) -> MutexGuard<'static, GraphState> {
    let session = session_of(params);
    let mut guard = graph_state().lock().unwrap_or_else(|p| p.into_inner());
    if guard.session != session {
        guard.session = session;
        guard.graph = graph::Graph::default();
        guard.advance_generation();
    }
    guard
}

/// Rejoin a wave without changing the active session or touching a newer plan.
/// The generation also detects A -> B -> A switches with reused node ids.
pub(crate) fn lock_graph_for_wave(
    params: &Value,
    generation: u64,
) -> Result<MutexGuard<'static, GraphState>, String> {
    let guard = graph_state().lock().unwrap_or_else(|p| p.into_inner());
    if guard.session != session_of(params) || guard.generation != generation {
        return Err(
            "plan or session changed while dispatch was running; stale results discarded".into(),
        );
    }
    Ok(guard)
}

impl GraphState {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn advance_generation(&mut self) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("graph generation exhausted");
    }

    /// Whether any node is running.
    pub(crate) fn is_running(&self) -> bool {
        self.graph
            .nodes
            .iter()
            .any(|node| node.status == graph::Status::Running)
    }

    /// The graph, for reading. A queued wave reads the ready set through this
    /// without taking the mutable borrow the tool path uses.
    pub(crate) fn graph(&self) -> &graph::Graph {
        &self.graph
    }

    pub(crate) fn graph_mut(&mut self) -> &mut graph::Graph {
        &mut self.graph
    }
}

crate::tool_entry!(plan_tool, crate::plan::plan);
crate::tool_entry!(dispatch_tool, crate::dispatch::dispatch);
crate::tool_entry!(task_tool, crate::task::task);

pub use crate::guide::GUIDE_ENV;


fn write_out(out: &mut rpi_plugin_sdk::callbacks::Output<'_>, value: Value) {
    if out.is_available() {
        out.write(value.to_string());
    }
}

rpi_plugin_sdk::string_handler! {
/// Append the orchestration guidance to the system prompt, only when opted in.
///
/// Replacement contract: the handler reads the base prompt from the event's
/// `systemPrompt` and writes `{"systemPrompt": "..."}` when it changed it, or
/// `{}` (no change) otherwise. A malformed envelope declines rather than
/// replacing the host's prompt with our section alone.
fn on_before_agent_start(event_json, out, _user_data) {
    let parsed: Value = match serde_json::from_str(event_json) {
        Ok(value) => value,
        Err(_) => {
            write_out(out, serde_json::json!({}));
            return 0;
        }
    };
    let base = parsed
        .get("systemPrompt")
        .and_then(Value::as_str)
        .unwrap_or_default();

    // The tool guidance goes in whenever the model has a prompt to answer,
    // whether or not a goal is active: the tools are registered either way,
    // and a registered tool the model was never told about is one it will not
    // use. The orchestration guidance stays behind its goal condition.
    let mut next = base.to_string();
    let mut changed = false;
    if let Some(tools) = crate::guide::apply_tools_to_prompt(&next) {
        next = tools;
        changed = true;
    }
    if crate::guide::guide_enabled() {
        if let Some(guide) = crate::guide::apply_to_prompt(&next) {
            next = guide;
            changed = true;
        }
    }
    if !changed {
        write_out(out, serde_json::json!({}));
        return 0;
    }
    write_out(out, serde_json::json!({"systemPrompt": next}));
    0
}
}

rpi_plugin_sdk::export_plugin!(|api| {
    // Capture the host action bridge first: `subagent`/`dispatch` read this
    // session's provider/model through it so a child inherits the parent binding
    // instead of quietly resolving the default provider.
    crate::host::install(api);

    // Two things must run after this host is gone: the effect queue, and the
    // sweep that kills workers whose host died. One daemon does both — it was two
    // programs at first, which bought nothing but two locks and two launches.
    //
    // The in-process scheduler below still starts, and is not redundant: while
    // this host is up it runs queued work without a 2s tick of latency, and the
    // daemon covers the case where the host dies. The daemon's lock keeps two
    // daemons apart; the thread and the daemon are kept apart by the graph's
    // running-wave check.
    // Only a host that stays up runs the in-process scheduler. A one-shot run
    // would start a wave, exit mid-flight, and leave the daemon to start the same
    // wave again — the two collide on the worker's session file. A long-lived host
    // and the daemon do not collide: the daemon waits for a row to look abandoned
    // before it adopts it.
    if !crate::host::host_exits_after_this_turn() {
        let _ = PASSES.set(Mutex::new(passes::Passes::start(
            effects().clone(),
            effects_handler::run,
        )));
    }
    if let Ok(cwd) = std::env::current_dir() {
        let _ = passes_daemon::create_root(&passes_daemon::effects_dir(&cwd));
        passes_daemon::start_daemon(&cwd);
    }


    if let Some(register_prompt) = api.register_before_agent_start {
        let _ = register_prompt(
            on_before_agent_start,
            rpi_plugin_sdk::callbacks::free_string,
            api.user_data,
        );
    }

    let Some(register) = api.register_tool else {
        return 1;
    };

    {
        let schema = Box::new(crate::tool_schema::tool_schema("superagent_plan"));
        let rc = register(
            &*schema,
            plan_tool::execute,
            plan_tool::poll,
            plan_tool::cancel,
            plan_tool::destroy,
            crate::kit::plugin_free_string,
        );
        drop(schema);
        if rc != 0 {
            return 1;
        }
    }

    {
        let schema = Box::new(crate::tool_schema::tool_schema("superagent_dispatch"));
        let rc = register(
            &*schema,
            dispatch_tool::execute,
            dispatch_tool::poll,
            dispatch_tool::cancel,
            dispatch_tool::destroy,
            crate::kit::plugin_free_string,
        );
        drop(schema);
        if rc != 0 {
            return 1;
        }
    }

    {
        let schema = Box::new(crate::tool_schema::tool_schema("superagent_task"));
        let rc = register(
            &*schema,
            task_tool::execute,
            task_tool::poll,
            task_tool::cancel,
            task_tool::destroy,
            crate::kit::plugin_free_string,
        );
        drop(schema);
        if rc != 0 {
            return 1;
        }
    }

    0
});

#[cfg(test)]
mod tests {
    use super::*;



    /// The graph is process-global by design, so graph-mutating tests must not
    /// interleave. A poisoned lock is recovered (a failed test still releases it).
    static GRAPH_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn graph_test_guard() -> MutexGuard<'static, ()> {
        GRAPH_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn params_for(session: Option<&str>) -> Value {
        match session {
            Some(id) => serde_json::json!({
                rpi_plugin_sdk::HOST_CONTEXT_KEY: { "cwd": "/tmp/p", "sessionId": id }
            }),
            None => serde_json::json!({}),
        }
    }

    #[test]
    fn cancellation_before_the_first_chunk_keeps_the_whole_plan_retryable() {
        let _serial = graph_test_guard();
        let mut params = params_for(Some("cancel-before-first-chunk"));
        params["action"] = serde_json::json!("set");
        params["maxParallel"] = serde_json::json!(1);
        params["nodes"] = serde_json::json!([
            {"id":"a", "task":"a"},
            {"id":"b", "task":"b"},
            {"id":"dependent", "task":"dependent", "deps":["a"]}
        ]);
        let control = crate::control::Control::default();
        let progress = std::sync::mpsc::channel().0;
        crate::plan::plan(&params, &control, &progress).unwrap();
        crate::dispatch::dispatch_with(
            &params,
            &control,
            &progress,
            |graph, ready, parallel, control, progress, parent, pids, run_id, parent_session| {
                // Cancel after dispatch marks the ready set running, but
                // before run_wave starts its first chunk. No child may run.
                control.cancel();
                crate::dispatch::run_wave(
                    graph,
                    ready,
                    parallel,
                    control,
                    progress,
                    parent,
                    pids,
                    run_id,
                    parent_session,
                )
            },
        )
        .unwrap();
        {
            let state = lock_graph(&params);
            for node in &state.graph().nodes {
                assert_eq!(
                    node.status,
                    graph::Status::Pending,
                    "unstarted node {} became {:?}",
                    node.id,
                    node.status
                );
            }
        }
        let control = crate::control::Control::default();
        let fixture = |_: &graph::Graph,
                       ready: &[String],
                       _: usize,
                       _: &crate::control::Control,
                       _: &crate::kit::Progress,
                       _: &crate::host::Selection,
                       _: &std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
                       _: &str,
                       _: Option<&str>| {
            ready
                .iter()
                .map(|id| (id.clone(), Ok("fixture success".into())))
                .collect()
        };
        // Retrying without resetting the graph completes the independent nodes,
        // then their dependent on the next wave. The fixture launches no child.
        crate::dispatch::dispatch_with(&params, &control, &progress, fixture).unwrap();
        assert_eq!(lock_graph(&params).graph().ready(), vec!["dependent"]);
        crate::dispatch::dispatch_with(&params, &control, &progress, fixture).unwrap();
        assert!(lock_graph(&params)
            .graph()
            .nodes
            .iter()
            .all(|node| node.status == graph::Status::Done));
    }

    /// `started_at` is what makes "how far along is it" answerable, and it is
    /// written on the real dispatch path — the same one a wave takes, with the
    /// children replaced by a fixture so no network is involved.
    #[test]
    fn a_started_node_records_when_it_started_and_keeps_it_across_waves() {
        let _serial = graph_test_guard();
        let params = params_for(Some("started-at"));
        let control = crate::control::Control::default();
        let progress = std::sync::mpsc::channel().0;
        let mut set = params.clone();
        set["action"] = serde_json::json!("set");
        set["nodes"] = serde_json::json!([{"id": "a", "task": "a"}, {"id": "b", "task": "b", "deps": ["a"]}]);
        crate::plan::plan(&set, &control, &progress).unwrap();

        let fixture = |_: &graph::Graph,
                       ready: &[String],
                       _: usize,
                       _: &crate::control::Control,
                       _: &crate::kit::Progress,
                       _: &crate::host::Selection,
                       _: &std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
                       _: &str,
                       _: Option<&str>| {
            ready
                .iter()
                .map(|id| (id.clone(), Ok("ok".into())))
                .collect()
        };
        crate::dispatch::dispatch_with(&params, &control, &progress, fixture).unwrap();
        let first = {
            let state = lock_graph(&params);
            let node = state.graph().get("a").unwrap();
            assert_eq!(node.status, graph::Status::Done);
            assert!(node.started_at > 0, "a started node recorded no start time");
            node.started_at
        };

        // The second wave must not move the first wave's start time.
        crate::dispatch::dispatch_with(&params, &control, &progress, fixture).unwrap();
        let state = lock_graph(&params);
        assert_eq!(
            state.graph().get("a").unwrap().started_at,
            first,
            "a later wave rewrote an earlier start time"
        );
        assert!(state.graph().get("b").unwrap().started_at > 0);
    }

    #[test]
    fn verification_keeps_plan_readable_and_does_not_commit_into_a_new_session() {
        use std::time::{Duration, Instant};
        let _serial = graph_test_guard();
        let marker_name = format!(
            "rpi-verify-review-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let marker = std::env::current_dir().unwrap().join(&marker_name);
        #[cfg(windows)]
        let verify = format!("echo started>{marker_name} & ping -n 31 127.0.0.1 >nul");
        #[cfg(unix)]
        let verify = format!(
            "printf started > '{}'; sleep 30",
            marker.to_string_lossy().replace('\'', "'\\''")
        );
        let mut params = params_for(Some("verification-a"));
        params["action"] = serde_json::json!("set");
        params["nodes"] = serde_json::json!([{"id":"x", "task":"original", "verify":verify}]);
        let control = crate::control::Control::default();
        crate::plan::plan(&params, &control, &std::sync::mpsc::channel().0).unwrap();
        let wave_params = params.clone();
        let wave_control = control.clone();
        let wave = std::thread::spawn(move || {
            crate::dispatch::dispatch_with(
                &wave_params,
                &wave_control,
                &std::sync::mpsc::channel().0,
                |_, ready, _, _, _, _, _, _, _| {
                    ready
                        .iter()
                        .map(|id| (id.clone(), Ok("fixture result".into())))
                        .collect()
                },
            )
        });
        let setup_deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() && Instant::now() < setup_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let verification_started = marker.exists();
        let (tx, rx) = std::sync::mpsc::channel();
        let show_params = params_for(Some("verification-a"));
        let show = std::thread::spawn(move || {
            let _ = tx.send(crate::plan::plan(
                &show_params,
                &crate::control::Control::default(),
                &std::sync::mpsc::channel().0,
            ));
        });
        let readable = rx.recv_timeout(Duration::from_millis(500));
        let replacement = if matches!(&readable, Ok(Ok(_))) {
            let editing = crate::plan::plan(&params, &control, &std::sync::mpsc::channel().0);
            let mut replacement = params_for(Some("verification-b"));
            replacement["action"] = serde_json::json!("set");
            replacement["nodes"] = serde_json::json!([{"id":"x", "task":"replacement"}]);
            let result = crate::plan::plan(&replacement, &control, &std::sync::mpsc::channel().0);
            Some((editing, result, replacement))
        } else {
            None
        };
        // Clean up both workers before any assertion, including on old code
        // where the show worker is blocked behind the verification command.
        control.cancel();
        let wave_result = wave.join().unwrap();
        show.join().unwrap();
        let _ = std::fs::remove_file(marker);
        assert!(
            verification_started,
            "verification fixture did not start: {wave_result:?}"
        );
        assert!(
            matches!(&readable, Ok(Ok(_))),
            "plan show blocked during verification: {readable:?}"
        );
        let (editing, result, replacement) = replacement.unwrap();
        // Editing during a wave is allowed now: a step can become pointless while
        // other workers are still busy, and making the caller wait for unrelated
        // work before it could cancel that step was the bug.
        assert!(
            editing.is_ok(),
            "an edit during a running wave must be accepted: {editing:?}"
        );
        result.unwrap();
        // The running wave's results are still refused — the edit bumped the
        // generation, so what the old wave produced no longer describes this plan.
        assert!(wave_result.unwrap_err().contains("stale results discarded"));
        let state = lock_graph(&replacement);
        assert_eq!(state.graph().get("x").unwrap().task, "replacement");
        assert_eq!(
            state.graph().get("x").unwrap().status,
            graph::Status::Pending
        );
    }

    /// An edit during a running wave used to leave its nodes `running` forever.
    ///
    /// `dispatch` marks the ready set `running`, then edits the plan, and the
    /// generation check refuses the wave's results ("stale results discarded").
    /// The refusal returns **before** the nodes are written back, so they keep the
    /// `running` mark with no child behind them. `queue_wave` refuses to dispatch
    /// while any node is `running`, so that session could never dispatch again.
    ///
    /// Editing during a wave is the whole point of steering live work, so this is
    /// the common path, not an edge case.
    #[test]
    fn editing_during_a_wave_leaves_nothing_stuck_running() {
        let _serial = graph_test_guard();
        let params = params_for(Some("stuck-running"));
        let control = crate::control::Control::default();
        let progress = std::sync::mpsc::channel().0;
        let mut set = params.clone();
        set["action"] = serde_json::json!("set");
        set["nodes"] = serde_json::json!([{"id": "a", "task": "slow"}]);
        crate::plan::plan(&set, &control, &progress).unwrap();

        let edit = params.clone();
        let fixture = move |_: &graph::Graph,
                            ready: &[String],
                            _: usize,
                            _: &crate::control::Control,
                            _: &crate::kit::Progress,
                            _: &crate::host::Selection,
                            _: &std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
                            _: &str,
                            _: Option<&str>| {
            // The person changes their mind while this wave is still running.
            let mut update = edit.clone();
            update["action"] = serde_json::json!("update");
            update["addNodes"] = serde_json::json!([{"id": "z", "task": "new work"}]);
            crate::plan::plan(&update, &crate::control::Control::default(), &std::sync::mpsc::channel().0)
                .expect("the edit must be accepted during a wave");
            ready
                .iter()
                .map(|id| (id.clone(), Ok("too late".into())))
                .collect::<Vec<(String, Result<String, String>)>>()
        };
        // The stale-results error is expected; what matters is the graph after it.
        let _ = crate::dispatch::dispatch_with(&params, &control, &progress, fixture);

        let state = lock_graph(&params);
        let node = state.graph().get("a").unwrap();
        assert_ne!(
            node.status,
            graph::Status::Running,
            "the stale wave left its node running with no child behind it"
        );
    }

    /// The gate flag is cleared by the wave that finishes the node.
    ///
    /// This is what keeps a gated plan from stopping one wave short of done: the
    /// flag is a brake, not a record, and a brake left on is a stuck plan.
    #[test]
    fn a_finished_gate_is_cleared_so_the_plan_can_continue() {
        let _serial = graph_test_guard();
        let params = params_for(Some("gate-consumed"));
        let control = crate::control::Control::default();
        let progress = std::sync::mpsc::channel().0;
        let mut set = params.clone();
        set["action"] = serde_json::json!("set");
        set["nodes"] = serde_json::json!([
            {"id": "gate", "task": "decide", "confirmBeforeNext": true},
            {"id": "b", "task": "then this", "deps": ["gate"]}
        ]);
        crate::plan::plan(&set, &control, &progress).unwrap();

        let fixture = |_: &graph::Graph,
                       ready: &[String],
                       _: usize,
                       _: &crate::control::Control,
                       _: &crate::kit::Progress,
                       _: &crate::host::Selection,
                       _: &std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
                       _: &str,
                       _: Option<&str>| {
            ready
                .iter()
                .map(|id| (id.clone(), Ok("ok".into())))
                .collect::<Vec<(String, Result<String, String>)>>()
        };
        crate::dispatch::dispatch_with(&params, &control, &progress, fixture).unwrap();

        let state = lock_graph(&params);
        let gate = state.graph().get("gate").unwrap();
        assert_eq!(gate.status, graph::Status::Done);
        assert!(
            !gate.confirm_before_next,
            "the gate stayed on, so every later wave will be refused"
        );
    }

    /// Dispatch never runs a wave itself: it queues one and returns. A blocking
    /// dispatch is the mode this package dropped — it made the caller wait inside
    /// a tool call for work the calling session could have done directly — so the
    /// tool is now only allowed to hand the work to the scheduler.
    ///
    /// The wave itself is left to the scheduler here, which is what makes this a
    /// test of the tool rather than of `run_wave`: no child is ever spawned.
    #[test]
    fn dispatch_queues_and_returns_without_running_the_wave() {
        let _serial = graph_test_guard();
        let params = params_for(Some("always-async"));
        let control = crate::control::Control::default();
        let progress = std::sync::mpsc::channel().0;
        let mut set = params.clone();
        set["action"] = serde_json::json!("set");
        set["taskId"] = serde_json::json!("always-async-task");
        set["nodes"] = serde_json::json!([{"id":"x", "task":"never runs here"}]);
        crate::plan::plan(&set, &control, &progress).unwrap();
        crate::effects().cancel(crate::effects::Kind::Launch, "always-async-task");

        let reply = crate::dispatch::dispatch(&params, &control, &progress).unwrap();
        let queued = crate::effects().pending(crate::effects::Kind::Launch, "always-async-task");
        // Relase the row before asserting, so a failure does not leave the
        // process-global queue holding work for a later test.
        crate::effects().cancel(crate::effects::Kind::Launch, "always-async-task");

        assert!(reply.contains("queued"), "dispatch did not queue: {reply}");
        assert!(queued, "dispatch returned without queueing a wave");
        // The node is still pending: nothing ran inline, so no result was written.
        let state = lock_graph(&params);
        assert_eq!(state.graph().get("x").unwrap().status, graph::Status::Pending);
    }

    #[test]
    fn a_running_wave_cannot_be_queued_again() {
        let _serial = graph_test_guard();
        let params = params_for(Some("busy-wave"));
        let control = crate::control::Control::default();
        let progress = std::sync::mpsc::channel().0;
        let mut set = params.clone();
        set["action"] = serde_json::json!("set");
        set["nodes"] = serde_json::json!([{"id":"x", "task":"original"}]);
        crate::plan::plan(&set, &control, &progress).unwrap();
        let generation = {
            let mut guard = lock_graph(&params);
            guard.graph_mut().get_mut("x").unwrap().status = graph::Status::Running;
            guard.generation()
        };

        // Queueing a second wave while one runs is still refused: that would run
        // nodes the first wave already marked running.
        assert!(crate::dispatch::dispatch(&params, &control, &progress)
            .unwrap_err()
            .contains("already running"));

        // Editing the plan, however, is allowed — that is how a node gets
        // cancelled or replaced while the other workers are still busy.
        let mut update = params.clone();
        update["action"] = serde_json::json!("update");
        update["reset"] = serde_json::json!(["x"]);
        for edit in [&set, &update] {
            assert!(
                crate::plan::plan(edit, &control, &progress).is_ok(),
                "an edit during a running wave must be accepted"
            );
        }
        // The edit landed, and it is what makes the running wave stale.
        //
        // That is the contract: the wave keeps working on the snapshot it was
        // given, but its results may not be written onto a plan a person has
        // since changed. Refusing the results (not the edit) is the correct trade
        // — the alternative was making the person wait for unrelated workers
        // before they could cancel work they already knew was wasted.
        assert!(
            lock_graph_for_wave(&params, generation).is_err(),
            "the edit did not invalidate the running wave"
        );
        let state = lock_graph(&params);
        assert_eq!(state.graph().get("x").unwrap().status, graph::Status::Pending);
        assert_eq!(state.graph().get("x").unwrap().task, "original");
    }

    #[test]
    fn stale_wave_cannot_reset_new_session_or_touch_reused_node_ids() {
        let _serial = graph_test_guard();
        let a = params_for(Some("wave-a"));
        let b = params_for(Some("wave-b"));
        let old_generation = lock_graph(&a).generation();
        let control = crate::control::Control::default();
        let progress = std::sync::mpsc::channel().0;
        let mut replacement = b.clone();
        replacement["action"] = serde_json::json!("set");
        replacement["nodes"] = serde_json::json!([{"id":"x", "task":"replacement"}]);
        crate::plan::plan(&replacement, &control, &progress).unwrap();
        assert!(lock_graph_for_wave(&a, old_generation).is_err());
        {
            // Inspect directly: using lock_graph(&b) would hide an accidental
            // reset by a stale completion by switching the session back again.
            let guard = graph_state().lock().unwrap();
            assert_eq!(guard.session, session_of(&b));
            assert_eq!(guard.graph().get("x").unwrap().task, "replacement");
            assert_eq!(
                guard.graph().get("x").unwrap().status,
                graph::Status::Pending
            );
        }
        // Returning to A still must not accept an old A wave's results.
        replacement[rpi_plugin_sdk::HOST_CONTEXT_KEY] = a[rpi_plugin_sdk::HOST_CONTEXT_KEY].clone();
        crate::plan::plan(&replacement, &control, &progress).unwrap();
        assert!(lock_graph_for_wave(&a, old_generation).is_err());
        assert_eq!(
            lock_graph(&a).graph().get("x").unwrap().status,
            graph::Status::Pending
        );
    }

    /// Regression: the graph used to be one process-global value, so session B
    /// saw (and could dispatch) session A's plan. Each session now owns its own
    /// graph, and a switch resets rather than replays.
    #[test]
    fn graphs_are_isolated_per_session() {
        let _serial = graph_test_guard();
        let a = params_for(Some("session-a"));
        let b = params_for(Some("session-b"));

        let result = crate::plan::plan(
            &serde_json::json!({
                "action": "set",
                "nodes": [{"id": "only-a", "task": "a-task"}],
                rpi_plugin_sdk::HOST_CONTEXT_KEY: { "sessionId": "session-a" }
            }),
            &crate::control::Control::default(),
            &std::sync::mpsc::channel().0,
        )
        .unwrap();
        assert!(result.contains("only-a"));

        {
            let guard = lock_graph(&a);
            assert_eq!(guard.graph().nodes.len(), 1);
            assert!(guard.graph().get("only-a").is_some());
        }
        // A different session must not inherit that plan.
        {
            let guard = lock_graph(&b);
            assert!(
                guard.graph().nodes.is_empty(),
                "session B saw session A's plan"
            );
        }
    }

    /// Switching back to the first session starts clean rather than replaying:
    /// resuming an old plan is the human's decision.
    #[test]
    fn returning_to_a_session_does_not_resurrect_the_old_plan() {
        let _serial = graph_test_guard();
        let a = params_for(Some("session-a"));
        let b = params_for(Some("session-b"));
        for params in [&a, &b] {
            let mut guard = lock_graph(params);
            *guard.graph_mut() = graph::Graph::default();
            guard.graph_mut().nodes.push(
                graph::Node::from_value(&serde_json::json!({"id": "x", "task": "t"})).unwrap(),
            );
        }
        // Touch B, then come back to A: A's graph is reset, not restored.
        drop(lock_graph(&b));
        let guard = lock_graph(&a);
        assert!(
            guard.graph().nodes.is_empty(),
            "a stale plan survived a session round-trip"
        );
    }

    /// A host without `__rpi` (or an ephemeral session with no id) still works:
    /// it shares the single `None` bucket instead of refusing to serve.
    #[test]
    fn missing_session_id_falls_back_to_one_shared_graph() {
        let _serial = graph_test_guard();
        let none = params_for(None);
        assert!(session_of(&none).is_none());
        {
            let mut guard = lock_graph(&none);
            *guard.graph_mut() = graph::Graph::from_nodes_value(&serde_json::json!([
                {"id": "fallback", "task": "t"}
            ]))
            .unwrap();
        }
        let guard = lock_graph(&params_for(None));
        assert!(guard.graph().get("fallback").is_some());
    }
}
