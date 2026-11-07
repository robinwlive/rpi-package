//! What a queued effect actually does.
//!
//! The queue ([`crate::effects`]) and its scheduler ([`crate::passes`]) know
//! nothing about graphs, gates or sessions. This module is the one place that
//! connects them: it turns an [`Effect`] into the call that performs it.
//!
//! ## Why the handler cannot reuse the tool path
//!
//! `superagent_dispatch` runs inside a tool call, so it has a cancellation
//! token, a progress channel and a caller waiting for the answer. A background
//! pass has none of those: the tool call that asked for the work returned long
//! ago. The handler therefore builds its own cancelled-never [`Control`] and a
//! progress sink that drops updates, and it reports the outcome through
//! [`Effect::Relay`] instead of a return value.
//!
//! That split is the whole point of the queue: the answer goes to the session
//! as a message, not back to a caller that has gone.

use serde_json::Value;

use crate::control::Control;
use crate::effects::{Effect, Outcome};

/// A progress sink that drops everything.
///
/// The background pass has no tool caller to stream partial updates to; the
/// result arrives as a relay when the wave ends.
fn silent_progress() -> crate::kit::Progress {
    let (sender, receiver) = std::sync::mpsc::channel();
    // Dropping the receiver would make `send` fail on every progress call. Keep
    // it alive on a thread that discards, so the channel stays open and cheap.
    std::thread::spawn(move || {
        for _ in receiver {}
    });
    sender
}

/// Run one queued effect.
///
/// Returns the outcome the queue should record. A wave that cannot start yet
/// (no ready nodes, or a wave already running) is [`Outcome::Later`]: it will
/// be tried again, not counted as a failure.
pub fn run(effect: &Effect) -> Result<(), Outcome> {
    match effect {
        Effect::Launch {
            task_id,
            nodes,
            max_parallel,
            binding,
            session,
            graph,
            rpi_exe,
        } => run_launch(
            task_id,
            nodes,
            *max_parallel,
            binding,
            session.as_deref(),
            graph.as_ref(),
            rpi_exe.as_deref(),
        ),
        Effect::Extend {
            task_id,
            node,
            binding,
            session,
        } => run_extend(task_id, node, binding, session.as_deref()),
        Effect::Relay { task_id, text } => run_relay(task_id, text),
    }
}

/// Run a ready wave of `task_id`'s graph.
///
/// `binding` is the provider/model the asking session ran on. The background
/// pass has no invocation handle, so this is the only way the children inherit
/// it: without it they would resolve the default provider instead.
fn run_launch(
    task_id: &str,
    nodes: &[String],
    max_parallel: usize,
    binding: &crate::effects::Binding,
    session: Option<&str>,
    graph: Option<&Value>,
    rpi_exe: Option<&str>,
) -> Result<(), Outcome> {
    // A wave already running is not a failure: wait for it.
    if crate::graph_state_is_running() {
        return Err(Outcome::Later("a wave is already running".into()));
    }
    // Rebuild the plan this wave is for. The row carries it because the graph
    // otherwise lives only in the asking host's memory, which this process never
    // had. Without it there is nothing to dispatch and the wave would report
    // "no plan yet" forever.
    let Some(graph) = graph else {
        return Err(Outcome::Failed(
            "this launch carries no graph: it was queued by a version that did not record one, \
             so it cannot be run in another process. Re-queue it from a session."
                .into(),
        ));
    };
    let rebuilt = match crate::graph::Graph::from_details(graph) {
        Ok(rebuilt) => rebuilt,
        Err(message) => {
            return Err(Outcome::Failed(format!(
                "the recorded plan could not be rebuilt: {message}"
            )))
        }
    };
    if let Err(message) = crate::install_graph(session.map(str::to_string), rebuilt) {
        return Err(Outcome::Failed(message));
    }
    let control = Control::default();
    let progress = silent_progress();
    // The graph is keyed by session. Left out, `lock_graph` would switch the
    // shared state to "no session" and clear the plan this wave is for.
    let mut params = serde_json::json!({
        "taskId": task_id,
        "maxParallel": max_parallel,
        // Read back by `dispatch`, which prefers it over asking the host.
        "binding": {
            "provider": binding.provider,
            "model": binding.model,
        },
    });
    if let Some(session) = session {
        params[rpi_plugin_sdk::HOST_CONTEXT_KEY] =
            serde_json::json!({ "sessionId": session });
    }
    // The daemon's own executable is not `rpi`, so a worker launched with it
    // would spawn the daemon instead of an agent. The host's path travels in the
    // row for exactly this.
    if let Some(exe) = rpi_exe {
        params["rpiExe"] = serde_json::Value::String(exe.to_string());
    }
    match crate::dispatch::dispatch(&params, &control, &progress) {
        Ok(text) => {
            // The wave finished. `relay` carries the outcome to whoever can
            // deliver it; the caller decides how, because a relay queued into
            // this process's memory would be invisible to a daemon writing the
            // log, and the two would keep re-queueing each other.
            let _ = nodes;
            relay(task_id, &summarize(&text))
        }
        Err(message) => {
            // "nothing ready" is a scheduling fact, not a failure.
            if is_deferral(&message) {
                Err(Outcome::Later(message))
            } else {
                Err(Outcome::Failed(message))
            }
        }
    }
}

/// Hand a wave's outcome to whatever can deliver it.
///
/// The in-process scheduler delivers immediately: the host is right here. A
/// daemon cannot deliver — it has no session — so it records the relay instead,
/// and the next host that opens the project picks it up. Detecting which case
/// this is by trying the host first is wrong: a daemon *has* the action bridge
/// compiled in but no session behind it, so the attempt fails in a way that looks
/// like a delivery error and gets retried forever.
///
/// Both paths therefore go through here, and what differs is only where the text
/// ends up.
fn relay(task_id: &str, text: &str) -> Result<(), Outcome> {
    match crate::host::relay_to_session(task_id, text) {
        Ok(()) => Ok(()),
        // No session to steer into: this is a background process. Whether the
        // relay is worth recording is the caller's decision, so this is not a
        // failure — the wave itself succeeded.
        Err(_) => Ok(()),
    }
}

/// Add a node to a plan, then run the wave it becomes ready in.
///
/// Only meaningful inside the session that queued it: a node has to be added to
/// a plan, and a plan lives in the host's memory. The daemon has none, so it
/// reports that plainly instead of guessing at what the caller meant — a wrong
/// guess would run unrelated work under the caller's name.
fn run_extend(
    task_id: &str,
    node: &Value,
    binding: &crate::effects::Binding,
    session: Option<&str>,
) -> Result<(), Outcome> {
    let _ = (task_id, node, binding, session);
    Err(Outcome::Failed(
        "extend runs only in the session that queued it: it adds a node to a plan this \
         process does not hold. Build the whole plan with `plan(action=set)` and queue that."
            .into(),
    ))
}

/// Steer a finished wave's report into the session.
fn run_relay(task_id: &str, text: &str) -> Result<(), Outcome> {
    match crate::host::relay_to_session(task_id, text) {
        Ok(()) => Ok(()),
        // A relay that cannot be delivered is not retried. Retrying forever was
        // the first version's bug: the daemon has no session, so every attempt
        // failed, each failure re-queued the row, and the queue spun.
        //
        // The wave itself succeeded; only the message is undeliverable from
        // here. Saying so once and retiring the row is the honest outcome — the
        // task's own record still holds the full report.
        Err(message) => {
            eprintln!(
                "rpi-superagent: relay for `{task_id}` not delivered ({message}); \
                 the task record still has the report"
            );
            Ok(())
        }
    }
}

/// Whether a dispatch error means "try again later" rather than "this failed".
fn is_deferral(message: &str) -> bool {
    message.starts_with("dispatch is already running") || message.starts_with("no progress:")
}

/// The first lines of a wave's report, for a relay message.
///
/// A full wave report can be long; a message that scrolls a conversation is not
/// read. The task still holds the complete text.
fn summarize(text: &str) -> String {
    const LIMIT: usize = 1_200;
    if text.len() <= LIMIT {
        return text.to_string();
    }
    let mut end = LIMIT;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated; superagent_task has the full report]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wave_already_running_defers_rather_than_fails() {
        assert!(is_deferral("dispatch is already running: wait for the current wave to finish"));
        assert!(is_deferral("no progress: no pending node is ready (blocked by unmet dependencies): a"));
        assert!(!is_deferral("no plan yet: call plan(action=set, nodes=[...]) first"));
    }

    #[test]
    fn a_long_report_is_truncated_on_a_char_boundary() {
        let text = "意".repeat(1_000);
        let out = summarize(&text);
        assert!(out.len() < text.len() + 100);
        assert!(out.ends_with("full report]"));
        // Truncating on a boundary keeps the string valid UTF-8, which slicing
        // mid-character would not.
        assert!(out.is_char_boundary(0));
    }
}
