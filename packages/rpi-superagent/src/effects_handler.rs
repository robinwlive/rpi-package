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
        } => run_launch(task_id, nodes, *max_parallel, binding, session.as_deref()),
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
) -> Result<(), Outcome> {
    // A wave already running is not a failure: wait for it.
    if crate::graph_state_is_running() {
        return Err(Outcome::Later("a wave is already running".into()));
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
    match crate::dispatch::dispatch(&params, &control, &progress) {
        Ok(text) => {
            // The wave finished. Hand the result to the session, so a person who
            // stopped watching still learns how it went.
            crate::effects().enqueue(Effect::Relay {
                task_id: task_id.to_string(),
                text: summarize(&text),
            });
            let _ = nodes;
            Ok(())
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

/// Add a node to the graph and run the wave it becomes ready in.
fn run_extend(
    task_id: &str,
    node: &Value,
    binding: &crate::effects::Binding,
    session: Option<&str>,
) -> Result<(), Outcome> {
    let control = Control::default();
    let progress = silent_progress();
    // `plan(action=update, addNodes=[...])` is the one path that appends a node
    // and validates the result, so a queued node gets the same checks a
    // hand-written one does.
    let mut params = serde_json::json!({
        "action": "update",
        "taskId": task_id,
        "addNodes": [node],
    });
    if let Some(session) = session {
        params[rpi_plugin_sdk::HOST_CONTEXT_KEY] =
            serde_json::json!({ "sessionId": session });
    }
    if let Err(message) = crate::plan::plan(&params, &control, &progress) {
        return Err(if is_deferral(&message) {
            Outcome::Later(message)
        } else {
            Outcome::Failed(message)
        });
    }
    let max_parallel = node
        .get("maxParallel")
        .and_then(Value::as_u64)
        .unwrap_or(4) as usize;
    run_launch(task_id, &[], max_parallel, binding, session)
}

/// Steer a finished wave's report into the session.
fn run_relay(task_id: &str, text: &str) -> Result<(), Outcome> {
    crate::host::relay_to_session(task_id, text)
        .map(|_| ())
        .map_err(|message| {
            if message.contains("not installed") {
                // A host without the action bridge cannot take messages; that
                // will not change on a retry, so stop retrying loudly.
                Outcome::Failed(message)
            } else {
                Outcome::Later(message)
            }
        })
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
