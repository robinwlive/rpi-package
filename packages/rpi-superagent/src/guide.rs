//! The orchestration guidance injected into the system prompt.
//!
//! This lives in its own module so the execution kernel stays a thin diff
//! against upstream `rpi-orchestrator`: rebase conflicts then land in the few
//! registration lines in `lib.rs` rather than in a whole file of prompt text.
//!
//! The guidance is **on by default whenever a goal is active** — `/goal`
//! implies orchestration, so no variable is needed. `RPI_SUPERAGENT_GUIDE`
//! only forces the decision: `0/false/no/off` disables it even with a goal,
//! `1/true/yes/on` enables it even without one.

use serde_json::Value;

/// Optional override for the orchestration guidance injected into the system
/// prompt.
pub const GUIDE_ENV: &str = "RPI_SUPERAGENT_GUIDE";

/// Project-relative path of `rpi-goal`'s state. An active goal there is what turns
/// the guidance on — a deliberate coupling to `/goal` mode.
const GOALS_FILE: &str = ".rpi/goals.json";

const GUIDE_HEADING: &str = "## Super-agent orchestration";

/// The tools this package registers, and when to reach for them.
///
/// This is a separate section from the orchestration guidance above, and it
/// is injected **whenever the model has a user message** — not only when a
/// goal is active. A tool the model was never told about is a tool it will
/// not use, and these four tools are useless unless the model knows they
/// exist as a set rather than as four unrelated names.
const TOOLS_HEADING: &str = "## Super-agent tools";

const TOOLS_SECTION: &str = r#"## Super-agent tools

These three tools work together. Use them in this order:

- `superagent_task` — create the long task and give it an id.
- `superagent_plan` — author a task DAG: nodes, dependencies, acceptance checks.
  Pass the task's id as `taskId`.
- `superagent_dispatch` — queue one ready wave of it in parallel child processes.

When to use which:

- Work with several steps, or steps that can run in parallel → `superagent_task`,
  `superagent_plan`, then `superagent_dispatch`.
- Work that should outlive one turn, or that you need to pick up after a
  restart → create it with `superagent_task`, and pass its `taskId` to
  `superagent_plan`, so dispatch records its progress without you updating it
  by hand. Check `superagent_task` with action=list at the start of a session
  to see what was already in flight.

**`superagent_dispatch` returns immediately.** It queues the wave and the wave
runs in the background; you get the report later as a message in this session.
Never wait for it, never re-dispatch the same wave to "check" on it, and go on
with other work — that is the whole point of running it in a child process. If
you would have to wait for the result to take your next step, do that step in
this session instead of dispatching it.

**Answering "how many are running, doing what, how far along":**

`superagent_plan(action=show)` answers all three from live state:

```
0/3 done, 1 running, 1 pending
[~] alpha running — Audit the routing layer (1m33s)
[!] beta failed — Rewrite the auth middleware — expected file was not produced
[ ] gamma pending deps=[alpha,beta] — Run the full test suite
```

The header counts what is moving now; each line names the node's task and, for a
running node, how long it has been going. Read the answer from this output rather
than from your memory of the `plan(set)` call that wrote it — and never guess a
progress figure the output does not contain.

There is no single-task delegation tool. One node with no dependencies *is* a
single task, so write a one-node plan — the same work, plus the report.

A node that exits 0 may still have hit its turn cap, so declare `expectFile` or
`verify` acceptance checks and let dispatch judge the node.

## When a worker asks a question

A worker runs as a one-shot `--mode rpc` child: it can ask (`UiDialog`), but
nothing answers it while it runs. Its result will then contain a block like:

    [worker asked 1 question(s), unanswered]
    - requestId=pidingo:7 which file should I edit?

That block means the node did **not** finish — it stopped to wait for an answer
that never came. Do not assume it worked it out.

**Re-dispatch that node**, and put the answer in the node's `task` text. You are
the project manager: read the question, decide from what you already know, and
tell the worker on the retry. Only ask your human when the answer is something
you cannot decide — a choice that changes the goal, spends money, or deletes
something they own.

Why not answer it live: the worker runs as a one-shot child and waits on its own
stdin, so nothing in this session is listening to it. The question arrives as a
result, not as a conversation, long after the dispatch call that started it
returned. Re-dispatching costs one extra node run; waiting for a person who may
be away for hours costs the whole task."#;

const GUIDE_SECTION: &str = r#"## Super-agent orchestration

For work that has several steps, or steps that can run in parallel, use the
`superagent_plan` and `superagent_dispatch` tools instead of doing everything
inline:

1. `superagent_plan` with action=set: write the task graph as nodes, each with
   an `id`, a self-contained `task`, and its `deps`.
2. `superagent_dispatch`: run the current ready wave. Nodes whose dependencies
   are done run in parallel (up to `maxParallel`).
3. Read the results, then `superagent_plan` with action=update
   (add/remove/re-dep/reset nodes) and `superagent_dispatch` again until the
   goal is met.

A subagent starts fresh and cannot see this conversation, so each node's `task`
must contain everything it needs. Prefer this over hand-rolling the whole
workflow with `bash`."#;

/// Tri-state read of [`GUIDE_ENV`]: `Some(true)` force on, `Some(false)` force
/// off, `None` means "decide from the goal state".
fn guide_override(value: Option<&str>) -> Option<bool> {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("1" | "true" | "yes" | "on") => Some(true),
        Some("0" | "false" | "no" | "off") => Some(false),
        _ => None,
    }
}

/// Whether `rpi-goal` has an active goal in this project. Parsed leniently: any
/// read or parse problem simply means "no goal".
fn active_goal_from(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    let Some(active) = value.get("active") else {
        return false;
    };
    !active.is_null() && active.get("status").and_then(Value::as_str) == Some("active")
}

fn active_goal_in_project() -> bool {
    let Ok(cwd) = std::env::current_dir() else {
        return false;
    };
    match std::fs::read_to_string(cwd.join(GOALS_FILE)) {
        Ok(text) => active_goal_from(&text),
        Err(_) => false,
    }
}

/// The tools `rpi-orchestrator` contributes. Seeing any of them means that
/// package is installed in this session alongside this one.
const NEIGHBOUR_TOOLS: &[&str] = &["plan", "dispatch", "subagent"];

/// Whether `rpi-orchestrator` is installed in the same session.
///
/// Both packages read the same `.rpi/goals.json`, so with an active goal both
/// would inject their guidance and the model would receive two conflicting
/// tool sets at once. Their tool names do not overlap, so the conflict is
/// invisible until the model starts calling the wrong ones.
///
/// The host reports the session's active tools, so a neighbour is detectable.
/// When it cannot be detected the neighbour is assumed absent: injecting two
/// sections is worse than injecting one, but silently dropping our own
/// guidance because the host would not answer is worse still.
fn neighbour_is_installed() -> bool {
    let tools = crate::host::registered_tool_names();
    NEIGHBOUR_TOOLS
        .iter()
        .any(|name| tools.iter().any(|tool| tool == name))
}

/// Whether the guidance should be injected right now.
pub fn guide_enabled() -> bool {
    // Never inject into a subagent: it has no `superagent_plan`/`superagent_dispatch`,
    // and telling it to use them would just waste a turn.
    if std::env::var(crate::spawn::SUBAGENT_ENV).is_ok() {
        return false;
    }
    // `/goal` implies orchestration: an active goal turns the guidance on unless
    // the variable explicitly overrides the decision.
    let enabled = guide_override(std::env::var(GUIDE_ENV).ok().as_deref())
        .unwrap_or_else(active_goal_in_project);
    // With the neighbour installed, its guidance names the same workflow. Two
    // sections would tell the model to use two different tool sets, so stand
    // down: this package is the superset, and its README says not to install
    // both.
    enabled && !neighbour_is_installed()
}

/// Append the tool guidance to `base`, or return `None` when it is already there.
///
/// The heading check keeps a second injection from duplicating the section.
pub fn apply_tools_to_prompt(base: &str) -> Option<String> {
    if base.contains(TOOLS_HEADING) {
        return None;
    }
    Some(format!("{base}\n\n{TOOLS_SECTION}"))
}

/// Append the orchestration guidance to `base`, or return `None` when there is
/// nothing to do.
///
/// The heading check keeps a second injection from duplicating the section.
pub fn apply_to_prompt(base: &str) -> Option<String> {
    if base.contains(GUIDE_HEADING) {
        return None;
    }
    Some(format!("{base}\n\n{GUIDE_SECTION}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_is_tristate() {
        assert_eq!(guide_override(Some("1")), Some(true));
        assert_eq!(guide_override(Some("TRUE")), Some(true));
        assert_eq!(guide_override(Some(" off ")), Some(false));
        assert_eq!(guide_override(Some("maybe")), None);
        assert_eq!(guide_override(None), None);
    }

    #[test]
    fn a_second_injection_is_skipped() {
        let once = apply_to_prompt("base").unwrap();
        assert!(once.contains("superagent_plan"));
        assert_eq!(apply_to_prompt(&once), None);
    }

    #[test]
    fn the_tool_guidance_names_every_registered_tool_and_when_to_use_each() {
        // Keep this list equal to the tools `lib.rs` registers: a tool the
        // prompt never names is a tool the model will not call, and a name here
        // that `lib.rs` no longer registers is one it will call and fail.
        for tool in ["superagent_plan", "superagent_dispatch", "superagent_task"] {
            assert!(TOOLS_SECTION.contains(tool), "{tool} missing from the section");
        }
        // The section must say when to reach for a tool, not only what it does.
        assert!(TOOLS_SECTION.contains("When to use which"));
        assert!(TOOLS_SECTION.contains("action=list"));
        // Dispatch is asynchronous now; the model must not be told to wait on
        // it, which is exactly what the old wording implied.
        assert!(!TOOLS_SECTION.contains("superagent_delegate"));
        assert!(TOOLS_SECTION.contains("returns immediately"));
    }

    #[test]
    fn the_tool_guidance_is_injected_independently_of_the_goal_guidance() {
        // The orchestration section needs an active goal; the tool section does
        // not, because the tools are registered either way.
        let with_tools = apply_tools_to_prompt("BASE").unwrap();
        assert!(with_tools.contains("## Super-agent tools"));
        assert!(!with_tools.contains("## Super-agent orchestration"));
        // A second injection is refused.
        assert_eq!(apply_tools_to_prompt(&with_tools), None);
    }

    /// The guidance must say what to do with an unanswered worker question.
    ///
    /// Without it the model reads "worker asked 1 question, unanswered" and
    /// assumes the node is fine — so the node is recorded as failed and nobody
    /// retries it. The answer is to re-dispatch with the answer in the task,
    /// which is a rule the model cannot infer from the result text alone.
    #[test]
    fn the_guidance_says_how_to_handle_an_unanswered_question() {
        assert!(TOOLS_SECTION.contains("unanswered"));
        assert!(TOOLS_SECTION.contains("Re-dispatch that node"));
        // It must also say who decides: the model first, the person only for
        // decisions that change the goal.
        assert!(TOOLS_SECTION.contains("You are"));
        assert!(TOOLS_SECTION.contains("Only ask your human"));
    }

    #[test]
    fn the_section_names_the_tools_this_package_registers() {
        // The orchestrator's section named `plan`/`dispatch`, which this
        // package does not register: the model would call a missing tool.
        assert!(GUIDE_SECTION.contains("superagent_plan"));
        assert!(GUIDE_SECTION.contains("superagent_dispatch"));
        assert!(!GUIDE_SECTION.contains("`plan`"));
        assert!(!GUIDE_SECTION.contains("`dispatch`"));
    }

    #[test]
    fn the_injected_section_is_appended_to_the_base_prompt() {
        let next = apply_to_prompt("BASE PROMPT").unwrap();
        assert!(next.starts_with("BASE PROMPT"));
        assert!(next.contains("## Super-agent orchestration"));
        assert!(next.contains("superagent_plan"));
    }

    #[test]
    fn the_neighbour_is_detected_from_the_sessions_tools() {
        // `plan` / `dispatch` / `subagent` are rpi-orchestrator's tools. Seeing
        // any of them means it is installed in this session.
        fn installed(tools: &[&str]) -> bool {
            tools.iter().any(|tool| NEIGHBOUR_TOOLS.contains(tool))
        }
        assert!(installed(&["read", "plan"]));
        assert!(installed(&["dispatch"]));
        assert!(installed(&["subagent"]));
        assert!(!installed(&["read", "bash", "superagent_plan"]));
        assert!(!installed(&[]));
    }

    #[test]
    fn an_active_goal_is_detected() {
        assert!(active_goal_from(
            r#"{"active":{"status":"active"}}"#
        ));
        assert!(!active_goal_from(r#"{"active":{"status":"paused"}}"#));
        assert!(!active_goal_from(r#"{"active":null}"#));
        assert!(!active_goal_from("not json"));
    }
}
