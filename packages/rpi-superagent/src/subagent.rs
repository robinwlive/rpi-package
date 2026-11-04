//! The `subagent` tool: one bounded delegation to a child `rpi` process.
//!
//! This is the primitive the `dispatch` scheduler fans out over, and it is also
//! usable on its own — the host already parallelises tool calls, so a turn that
//! issues several `subagent` calls already runs them concurrently.

use crate::control::Control;
use crate::graph::{
    DEFAULT_MAX_TURNS, DEFAULT_TIMEOUT_SECS, MAX_MAX_TURNS, MAX_TASK_BYTES, MAX_TIMEOUT_SECS,
    MIN_TIMEOUT_SECS,
};
use crate::kit::{
    content_only, optional_string, optional_trimmed, progress, string_param, Progress,
};
use crate::spawn;
use serde_json::Value;

pub fn subagent(
    params: &Value,
    control: &Control,
    progress_tx: &Progress,
) -> Result<String, String> {
    control.check()?;
    let task = string_param(params, "task")?;
    if task.trim().is_empty() {
        return Err("task must not be empty".into());
    }
    if task.len() > MAX_TASK_BYTES {
        return Err(format!("task must be at most {MAX_TASK_BYTES} bytes"));
    }

    // `role` is recorded but does not yet change behaviour (design §7 "只记录").
    let role = optional_string(params, "role", "worker");
    let max_turns = params
        .get("maxTurns")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_TURNS)
        .clamp(1, MAX_MAX_TURNS);
    let timeout_secs = params
        .get("timeoutSeconds")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS);
    // Optional provider/model passthrough. A child `rpi` process shares this
    // session's agent dir but *not* its binding, so a call that names neither
    // would silently resolve the default provider from settings. Default to the
    // binding the host reports for this session and pass it down explicitly.
    let selection = crate::host::parent_selection(params).effective(
        optional_trimmed(params, "provider").as_deref(),
        optional_trimmed(params, "model").as_deref(),
    );

    progress(progress_tx, &format!("[subagent] running (role={role})…"));
    let output = spawn::run(
        &task,
        max_turns,
        timeout_secs,
        selection.provider(),
        selection.model(),
        control,
    )?;
    Ok(content_only(&format!(
        "[subagent role={role} turns<={max_turns} timeout={timeout_secs}s provider={} model={}]\n{output}",
        selection.provider().unwrap_or("-"),
        selection.model().unwrap_or("-"),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(params: &Value) -> Result<String, String> {
        let control = Control::default();
        let (tx, _rx) = std::sync::mpsc::channel();
        subagent(params, &control, &tx)
    }

    #[test]
    fn rejects_missing_or_empty_task() {
        assert!(run(&serde_json::json!({})).is_err());
        assert!(run(&serde_json::json!({ "task": "   " })).is_err());
    }

    #[test]
    fn rejects_oversized_task() {
        let task = "x".repeat(MAX_TASK_BYTES + 1);
        assert!(run(&serde_json::json!({ "task": task })).is_err());
    }
}
