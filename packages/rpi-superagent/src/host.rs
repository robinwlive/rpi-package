//! The calling session's execution binding, read from the host at tool-call time.
//!
//! A subagent is a child `rpi` process. It inherits this session's agent dir, but
//! **not** its provider/model: without an explicit `--provider`/`--model` the
//! child resolves the *default* provider from settings, which is neither the
//! provider this session runs on nor necessarily authenticated at all. That
//! silent switch is how a delegation lands on a different model, or fails with
//! `No API key` for a provider the parent never used.
//!
//! The host already reports the binding: `RuntimeActionId::GetExecutionContext`
//! returns `{"model":{"id","provider","api"},"thinkingLevel":…}`. This module
//! reads it once per tool call so `subagent`/`dispatch` can hand the parent
//! binding to the child. A host that cannot answer degrades to the previous
//! behaviour (no `--provider`/`--model`) instead of failing the tool.

use rpi_plugin_sdk::{StbString, StbStringRef};
use serde_json::Value;
use std::ffi::c_void;
use std::sync::Mutex;

/// ABI id of `RuntimeActionId::GetExecutionContext`.
///
/// Kept as a number (like `rpi-subagents`) so this package keeps building against
/// the published SDK, whose action enum predates the variant. The host routes by
/// id, not by name.
const GET_EXECUTION_CONTEXT: u32 = 23;

/// ABI id of `RuntimeActionId::GetSessionBranch`.
const GET_SESSION_BRANCH: u32 = 20;

/// ABI id of `RuntimeActionId::SendMessage`.
///
/// Not in the published SDK's enum, so it is a number here for the same reason
/// `GET_EXECUTION_CONTEXT` is: the host routes by id, and the id predates the
/// variant's name in this crate's SDK version.
///
/// The host's default behaviour is what makes this usable as a relay: an idle
/// session appends the message, and a running turn **steers** it — the text
/// joins the turn in progress instead of waiting for the next one. That is how a
/// worker's report reaches the model while it is still working.
const SEND_MESSAGE: u32 = 0;

/// Process-lifetime host function pointers. Never retain a registration's
/// `user_data`: a candidate reload may be rolled back and freed.
#[derive(Clone, Copy)]
struct RuntimeFns {
    action: extern "C" fn(u32, StbStringRef, *mut StbString, *mut c_void) -> i32,
    free: extern "C" fn(StbString),
    /// The registration's opaque handle. The host recovers the session bridge
    /// from it, so an action can be driven without a per-call invocation handle.
    user_data: usize,
}

static HOST: Mutex<Option<RuntimeFns>> = Mutex::new(None);

/// Capture the host action bridge. Called once from the plugin entrypoint.
pub(crate) fn install(plugin: &rpi_plugin_sdk::PluginApi) {
    *HOST.lock().unwrap_or_else(|error| error.into_inner()) = Some(RuntimeFns {
        action: plugin.runtime_action,
        free: plugin.free_string,
        user_data: plugin.user_data as usize,
    });
}

/// The provider/model a child process should run on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Selection {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// The parent session's thinking level, passed down as `--thinking`.
    ///
    /// Carried for the same reason as the model: a child shares the agent dir
    /// but **not** the binding, so without it a worker resolves the default
    /// level from settings — the worker would then think at a different depth
    /// than the session that asked for it, and the user has no way to tell.
    /// `"off"` is a real value here (thinking turned off), not "unknown":
    /// unknown is `None`.
    pub thinking: Option<String>,
    /// The `rpi` executable a worker should be launched with.
    ///
    /// Carried alongside the binding because it is the same kind of fact: the
    /// host knows it, a background pass does not. Inside the daemon,
    /// `current_exe` names the daemon, so a worker launched from it would spawn
    /// a second scheduler instead of an agent.
    pub exe: Option<String>,
}

impl Selection {
    /// Resolve what to pass as `--provider`/`--model`.
    ///
    /// Explicit parameters always win, verbatim and independently: a caller that
    /// names only a model (or only a provider) means it, and silently attaching
    /// the other half of the parent binding could pair a model with a provider
    /// that does not serve it. The parent binding is inherited only when the call
    /// names neither.
    pub(crate) fn effective(
        &self,
        explicit_provider: Option<&str>,
        explicit_model: Option<&str>,
    ) -> Selection {
        if explicit_provider.is_some() || explicit_model.is_some() {
            return Selection {
                provider: explicit_provider.map(str::to_string),
                model: explicit_model.map(str::to_string),
                // Naming a model does not name a thinking depth: the level is
                // still this session's own, so it is inherited.
                thinking: self.thinking.clone(),
                // The executable is not part of the binding choice; it travels
                // with it so every worker of a wave uses the same one.
                exe: self.exe.clone(),
            };
        }
        self.clone()
    }

    pub(crate) fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    pub(crate) fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The `--thinking` value, if the host reported one.
    pub(crate) fn thinking(&self) -> Option<&str> {
        self.thinking.as_deref()
    }

    /// `provider/model` (or whichever half is known, or `-`) for result banners
    /// and failure messages, so a delegation is traceable from the transcript.
    pub(crate) fn label(&self) -> String {
        match (self.provider(), self.model()) {
            (Some(provider), Some(model)) => format!("{provider}/{model}"),
            (Some(provider), None) => provider.to_string(),
            (None, Some(model)) => model.to_string(),
            (None, None) => "-".to_string(),
        }
    }
}

/// The host's opaque invocation handle attached to this tool call.
///
/// The host injects `__rpi.runtimeHandle` for every plugin tool invocation. A
/// host that predates it — or an invocation with no handle — yields `None`, which
/// keeps the child on its default provider.
fn runtime_handle(params: &Value) -> Option<usize> {
    params
        .get(rpi_plugin_sdk::HOST_CONTEXT_KEY)
        .and_then(|context| context.get("runtimeHandle"))
        .and_then(Value::as_u64)
        .and_then(|id| usize::try_from(id).ok())
        .filter(|id| *id != 0)
}

/// The names of every tool registered in the calling session, including tools
/// other plugins contributed.
///
/// `GetExecutionContext` answers from the host's active tool set, so this sees
/// `plan` / `dispatch` when `rpi-orchestrator` is installed alongside this
/// package. It is the only way to detect that neighbour: the plugin ABI has no
/// "list other plugins" call, and `before_agent_start` receives no invocation
/// handle — but the host still recovers the session bridge from the
/// registration's `user_data`, so the action works from there.
///
/// Empty when the host cannot answer; the caller then treats the neighbour as
/// absent rather than guessing.
pub(crate) fn registered_tool_names() -> Vec<String> {
    let Some(host) = *HOST.lock().unwrap_or_else(|error| error.into_inner()) else {
        return Vec::new();
    };
    let mut out = StbString::empty();
    let rc = (host.action)(
        GET_EXECUTION_CONTEXT,
        StbStringRef::from_str("{}"),
        &mut out as *mut StbString,
        host.user_data as *mut c_void,
    );
    let text = out.to_string_lossy();
    (host.free)(out);
    if rc != 0 {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    value
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Read the calling session's binding from the host. Best-effort by design: a
/// missing bridge, a missing handle, a failing action or a malformed reply all
/// yield an empty selection.
pub(crate) fn parent_selection(params: &Value) -> Selection {
    let Some(host) = *HOST.lock().unwrap_or_else(|error| error.into_inner()) else {
        return Selection::default();
    };
    select_with(
        |handle| call_host(&host, handle, GET_EXECUTION_CONTEXT, &serde_json::json!({})),
        params,
    )
}

/// Look the binding up through `lookup`, which receives the invocation handle.
/// Split out from [`parent_selection`] so the parsing rules are testable without
/// an FFI host.
fn select_with(lookup: impl FnOnce(usize) -> Result<Value, String>, params: &Value) -> Selection {
    let Some(handle) = runtime_handle(params) else {
        return Selection::default();
    };
    match lookup(handle) {
        Ok(context) => selection_from_context(&context),
        Err(_) => Selection::default(),
    }
}

/// Extract `model.provider` / `model.id` from an execution-context reply.
pub(crate) fn selection_from_context(context: &Value) -> Selection {
    let field = |name: &str| {
        context
            .get("model")
            .and_then(|model| model.get(name))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    Selection {
        provider: field("provider"),
        model: field("id"),
        // `thinkingLevel` sits next to `model` in the reply and is a plain
        // lowercase level name (`off` … `max`), the same spelling the host's
        // `--thinking` flag accepts.
        thinking: context
            .get("thinkingLevel")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        // The host reports the binding, not the executable: this process *is*
        // an `rpi`, so `current_exe` is the right answer here.
        exe: None,
    }
}

/// Invoke a host runtime action using the registration's own handle.
///
/// The host recovers the session bridge from `PluginApi::user_data`, so an
/// action can be driven without a per-call invocation handle. `before_agent_start`
/// has no such handle, but the registration's is always valid, which is what
/// makes this usable from a prompt transform as well as from a tool call.
pub(crate) fn call_action(action: u32, args: &Value) -> Result<Value, String> {
    let Some(host) = *HOST.lock().unwrap_or_else(|error| error.into_inner()) else {
        return Err("the host action bridge is not installed".into());
    };
    let mut out = StbString::empty();
    let rc = (host.action)(
        action,
        StbStringRef::from_str(&args.to_string()),
        &mut out as *mut StbString,
        host.user_data as *mut c_void,
    );
    let text = out.to_string_lossy();
    (host.free)(out);
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("invalid action reply (status {rc}): {error}"))?;
    if rc != 0 {
        return Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the host action failed")
            .to_string());
    }
    Ok(value)
}

/// The leaf the main lane currently points at, when the host reports it.
///
/// A task's state is scoped to a branch, so a snapshot records the branch it
/// was written on. The host exposes this through `GetSessionBranch`'s
/// `leafId`.
pub(crate) fn current_branch_id() -> Option<String> {
    let reply = call_action(
        GET_SESSION_BRANCH,
        &serde_json::json!({"customTypes": [crate::task::TASK_ENTRY_TYPE], "order": "oldestFirst"}),
    )
    .ok()?;
    reply
        .get("leafId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// Invoke a host runtime action and parse its JSON reply.
fn call_host(host: &RuntimeFns, handle: usize, action: u32, args: &Value) -> Result<Value, String> {
    let encoded = args.to_string();
    let mut out = StbString::empty();
    let rc = (host.action)(
        action,
        StbStringRef::from_str(&encoded),
        &mut out as *mut StbString,
        handle as *mut c_void,
    );
    let text = out.to_string_lossy();
    (host.free)(out);
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("invalid execution-context reply (status {rc}): {error}"))?;
    if rc != 0 {
        return Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("execution-context lookup failed")
            .to_string());
    }
    Ok(value)
}

/// Whether this host exits as soon as the current turn ends.
///
/// A one-shot `rpi -p` run is a process that ends the moment the model stops.
/// A background thread dies with it, so a queued wave would be accepted and
/// then never run — the worst outcome, because the caller was told the work was
/// queued. Long-lived hosts (`--mode rpc`, the Feishu/Dash sessions) stay up and
/// can drive the queue.
///
/// The plugin shares the host's process, so its own argv is the host's argv.
/// `GetCliFlag` would be the tidier answer but a host need not implement it, and
/// a wrong "unknown" would be read as "long-lived" — the dangerous direction.
/// Reading argv fails safe: an unrecognised command line is treated as one-shot,
/// so a queued wave is refused rather than silently lost.
pub(crate) fn host_exits_after_this_turn() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    // `--print` / `-p`: process one prompt and exit.
    if has("--print") || has("-p") {
        return true;
    }
    // `--mode rpc`, `--connect` and `--server` are long-lived by construction.
    if has("--mode") || has("--connect") || has("--server") {
        return false;
    }
    // A bare `rpi` opens the TUI, which stays up.
    if args.is_empty() {
        return false;
    }
    // Any other positional argument is a prompt, so this is a one-shot run.
    args.iter().any(|arg| !arg.starts_with('-'))
}

/// Steer a message into the calling session.
///
/// This is the relay: a background pass has no caller waiting for a return
/// value, so its result travels as a message. The host appends to an idle
/// session and **steers** a running turn, so a report reaches the model mid-work
/// rather than waiting for a boundary.
///
/// `task_id` names the work in the message text only; the host routes to the
/// session that registered this plugin, which is the one that asked for the
/// work.
pub(crate) fn relay_to_session(task_id: &str, text: &str) -> Result<(), String> {
    let body = format!("super-agent task `{task_id}`:\n{text}");
    call_action(SEND_MESSAGE, &serde_json::json!({ "text": body }))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The decision `host_exits_after_this_turn` makes, against a given argv.
    /// Split out so every shape can be tested without spawning an `rpi`.
    fn exits_after_turn(args: &[&str]) -> bool {
        let has = |flag: &str| args.iter().any(|arg| *arg == flag);
        if has("--print") || has("-p") {
            return true;
        }
        if has("--mode") || has("--connect") || has("--server") {
            return false;
        }
        if args.is_empty() {
            return false;
        }
        args.iter().any(|arg| !arg.starts_with('-'))
    }

    #[test]
    fn a_print_run_is_one_shot() {
        // The case that matters: a queued wave would be dropped here.
        assert!(exits_after_turn(&["-p", "do the thing"]));
        assert!(exits_after_turn(&["--print", "do the thing"]));
        assert!(exits_after_turn(&["-p", "--local-only", "-e", "x.dylib"]));
    }

    #[test]
    fn a_long_lived_host_is_recognised() {
        assert!(!exits_after_turn(&["--mode", "rpc"]));
        assert!(!exits_after_turn(&["--mode", "json"]));
        assert!(!exits_after_turn(&["--connect", "ws://host"]));
        assert!(!exits_after_turn(&["--server"]));
        // A bare `rpi` opens the TUI and stays up.
        assert!(!exits_after_turn(&[]));
        // Options with no prompt also stay open.
        assert!(!exits_after_turn(&["--approve"]));
    }

    #[test]
    fn a_positional_prompt_is_one_shot() {
        // A prompt with no -p is still a single run.
        assert!(exits_after_turn(&["do the thing"]));
    }

    #[test]
    fn reads_the_binding_the_host_reports() {
        let context = json!({
            "model": {"id": "gpt-6.1-sol", "provider": "routeryo-copy", "api": "openai-responses"},
            "thinkingLevel": "high",
            "tools": ["read"],
        });
        let selection = selection_from_context(&context);
        assert_eq!(selection.provider(), Some("routeryo-copy"));
        assert_eq!(selection.model(), Some("gpt-6.1-sol"));
        assert_eq!(selection.label(), "routeryo-copy/gpt-6.1-sol");
    }

    #[test]
    fn a_context_without_a_model_or_with_blank_fields_selects_nothing() {
        assert_eq!(selection_from_context(&json!({})), Selection::default());
        assert_eq!(
            selection_from_context(&json!({"model": {"id": " ", "provider": ""}})),
            Selection::default()
        );
        assert_eq!(
            Selection {
                provider: Some("deepseek".into()),
                model: None,
                thinking: None,
                exe: None,
            }
            .label(),
            "deepseek"
        );
        assert_eq!(Selection::default().label(), "-");
    }

    #[test]
    fn explicit_parameters_win_and_are_never_completed_from_the_parent() {
        let parent = Selection {
            provider: Some("routeryo-copy".into()),
            model: Some("gpt-6.1-sol".into()),
            thinking: Some("high".into()),
            exe: None,
        };
        // Nothing named: inherit the whole binding.
        assert_eq!(parent.effective(None, None), parent);
        // Both named: pass through untouched.
        assert_eq!(
            parent.effective(Some("openai"), Some("gpt-6-astra")),
            Selection {
                provider: Some("openai".into()),
                model: Some("gpt-6-astra".into()),
                thinking: Some("high".into()),
                exe: None,
            }
        );
        // Half named: only that half is passed, so the child resolves the rest.
        assert_eq!(
            parent.effective(Some("deepseek"), None),
            Selection {
                provider: Some("deepseek".into()),
                model: None,
                thinking: Some("high".into()),
                exe: None,
            }
        );
        assert_eq!(
            parent.effective(None, Some("deepseek-v4-pro")),
            Selection {
                provider: None,
                model: Some("deepseek-v4-pro".into()),
                thinking: Some("high".into()),
                exe: None,
            }
        );
    }

    #[test]
    fn looks_the_binding_up_with_the_invocation_handle() {
        let params = json!({rpi_plugin_sdk::HOST_CONTEXT_KEY: {"runtimeHandle": 7}});
        let mut seen = None;
        let selection = select_with(
            |handle| {
                seen = Some(handle);
                Ok::<_, String>(json!({"model": {"id": "deepseek-v4-pro", "provider": "deepseek"}}))
            },
            &params,
        );
        assert_eq!(seen, Some(7));
        assert_eq!(selection.provider(), Some("deepseek"));
        assert_eq!(selection.model(), Some("deepseek-v4-pro"));
    }

    #[test]
    fn a_missing_runtime_handle_never_calls_the_host() {
        let mut calls = 0;
        for params in [
            json!({"task": "x"}),
            json!({rpi_plugin_sdk::HOST_CONTEXT_KEY: {}}),
            json!({rpi_plugin_sdk::HOST_CONTEXT_KEY: {"runtimeHandle": 0}}),
            json!({rpi_plugin_sdk::HOST_CONTEXT_KEY: {"runtimeHandle": "7"}}),
            json!({rpi_plugin_sdk::HOST_CONTEXT_KEY: {"runtimeHandle": -1}}),
        ] {
            let selection = select_with(
                |_| {
                    calls += 1;
                    Ok::<_, String>(json!({"model": {"id": "m", "provider": "p"}}))
                },
                &params,
            );
            assert_eq!(selection, Selection::default());
        }
        assert_eq!(calls, 0);
    }

    #[test]
    fn a_failing_or_malformed_lookup_degrades_to_the_default_binding() {
        let params = json!({rpi_plugin_sdk::HOST_CONTEXT_KEY: {"runtimeHandle": 3}});
        assert_eq!(
            select_with(|_| Err("host missing".to_string()), &params),
            Selection::default()
        );
        assert_eq!(
            select_with(|_| Ok(json!({"unexpected": true})), &params),
            Selection::default()
        );
    }
}
