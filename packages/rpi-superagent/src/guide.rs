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

These four tools work together. Use them in this order:

- `superagent_plan` — author a task DAG: nodes, dependencies, acceptance checks.
- `superagent_dispatch` — run one ready wave of it in parallel child processes.
- `superagent_delegate` — run one bounded task in a child process, when the work
  has no dependencies and needs no scheduling.
- `superagent_task` — track a long task across restarts: create it, record its
  state, list them.

When to use which:

- Work with several steps, or steps that can run in parallel → `superagent_plan`
  then `superagent_dispatch`.
- One self-contained task → `superagent_delegate`.
- Work that should outlive one turn, or that you need to pick up after a
  restart → also use `superagent_task`: create it, and pass its `taskId` to
  `superagent_plan`, so dispatch records its progress without you updating it
  by hand. Check `superagent_task` with action=list at the start of a session
  to see what was already in flight.

A node that exits 0 may still have hit its turn cap, so declare `expectFile` or
`verify` acceptance checks and let dispatch judge the node."#;

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
    fn the_tool_guidance_names_all_four_tools_and_says_when_to_use_each() {
        for tool in [
            "superagent_plan",
            "superagent_dispatch",
            "superagent_delegate",
            "superagent_task",
        ] {
            assert!(TOOLS_SECTION.contains(tool), "{tool} missing from the section");
        }
        // The section must say when to reach for a tool, not only what it does.
        assert!(TOOLS_SECTION.contains("When to use which"));
        assert!(TOOLS_SECTION.contains("action=list"));
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
