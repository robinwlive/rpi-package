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

/// Process-lifetime host function pointers. Never retain a registration's
/// `user_data`: a candidate reload may be rolled back and freed.
#[derive(Clone, Copy)]
struct RuntimeFns {
    action: extern "C" fn(u32, StbStringRef, *mut StbString, *mut c_void) -> i32,
    free: extern "C" fn(StbString),
}

static HOST: Mutex<Option<RuntimeFns>> = Mutex::new(None);

/// Capture the host action bridge. Called once from the plugin entrypoint.
pub(crate) fn install(plugin: &rpi_plugin_sdk::PluginApi) {
    *HOST.lock().unwrap_or_else(|error| error.into_inner()) = Some(RuntimeFns {
        action: plugin.runtime_action,
        free: plugin.free_string,
    });
}

/// The provider/model a child process should run on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Selection {
    pub provider: Option<String>,
    pub model: Option<String>,
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
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
                model: None
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
        };
        // Nothing named: inherit the whole binding.
        assert_eq!(parent.effective(None, None), parent);
        // Both named: pass through untouched.
        assert_eq!(
            parent.effective(Some("openai"), Some("gpt-6-astra")),
            Selection {
                provider: Some("openai".into()),
                model: Some("gpt-6-astra".into()),
            }
        );
        // Half named: only that half is passed, so the child resolves the rest.
        assert_eq!(
            parent.effective(Some("deepseek"), None),
            Selection {
                provider: Some("deepseek".into()),
                model: None
            }
        );
        assert_eq!(
            parent.effective(None, Some("deepseek-v4-pro")),
            Selection {
                provider: None,
                model: Some("deepseek-v4-pro".into())
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
