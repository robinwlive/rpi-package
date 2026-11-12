//! Model-facing metadata, shared by registration and contract tests.
use rpi_plugin_sdk::StableToolSchema;
use serde_json::{json, Value};

fn worker_properties() -> Value {
    json!({
        "task": {"type":"string", "pattern":"\\S", "description":"Self-contained instructions: goal, exact workspace/files allowed, constraints, deliverables and acceptance checks. No parent conversation is copied. At most 20000 UTF-8 bytes (plan nodes trim before checking; subagent checks the original string)."},
        "role": {"type":"string", "description":"Label only; does not restrict tools or change the child prompt. Standalone subagent default: worker."},
        "provider": {"type":"string", "description":"Usually omit both provider and model to inherit the parent session binding (best effort). An explicit nonblank provider disables inheritance of BOTH fields; specify a compatible model too when needed. Use only a known authenticated provider. Blank strings count as omitted."},
        "model": {"type":"string", "description":"Usually omit with provider. Any explicit nonblank model disables inheritance of BOTH fields; a model-only override is resolved by the child CLI, not paired with the parent provider. Prefer an explicit compatible provider/model pair. Missing host binding falls back to child defaults."},
        "maxTurns": {"type":"integer", "minimum":1, "maximum":32, "default":8, "description":"Hard child turn ceiling, 1..32; default 8."},
        "timeoutSeconds": {"type":"integer", "minimum":10, "maximum":1800, "default":300, "description":"Child wall-clock budget in seconds, 10..1800; default 300. Each node verification gets a separate budget of this length."}
    })
}

fn nonblank_string() -> Value {
    json!({"type":"string", "pattern":"\\S"})
}

fn string_list() -> Value {
    json!({"type":"array", "items":nonblank_string()})
}

fn node_schema() -> Value {
    let mut properties = worker_properties();
    properties["id"] = json!({"type":"string", "pattern":"\\S", "description":"Unique node id; trimmed. Dependencies must name existing ids and form an acyclic graph."});
    properties["deps"] = json!({"type":"array", "items":nonblank_string(), "default":[], "description":"Prerequisite node ids; trimmed and deduplicated. All must be done before this node runs."});
    properties["isolation"] = json!({"type":"boolean", "default": false, "description":"Give the node its own working directory, so parallel nodes cannot overwrite each other's files. Use it when nodes write files. The node's expectFile and verify then run in that directory, and paths are relative to it."});
    properties["confirmBeforeNext"] = json!({"type":"boolean", "default": false, "description":"Stop the automatic next wave after this node finishes: its report returns and you decide what runs next. Leave it off for a plan you wrote once and want to run to completion by itself."});
    properties["expectFile"] = json!({"type":"string", "description":"Optional nonblank file path (trimmed), absolute or relative to the node's working directory (its own directory when isolation is on, the shared process cwd otherwise). After successful child exit the path must exist (directories also pass); existence only, not proof that this run created or changed it."});
    properties["verify"] = json!({"type":"string", "description":"Optional nonblank shell command (trimmed) run in the node's working directory after child success and expectFile check. Must exit 0; use focused acceptance checks, not untrusted commands. Not a sandbox."});
    json!({"type":"object", "properties":properties, "required":["id","task"], "additionalProperties":true})
}

fn parameters(name: &str) -> Value {
    match name {
        "superagent_plan" => json!({
            "type":"object", "additionalProperties":true,
            "properties":{
                "action":{"type":"string", "enum":["set","update","show"], "default":"show"},
                "nodes":{"type":"array", "minItems":1, "items":node_schema(), "description":"action=set: complete replacement graph; at least one node. There is no runtime node-count cap."},
                "addNodes":{"type":"array", "items":node_schema(), "description":"action=update: append new ids, before removals and dependency edits. Empty array is allowed."},
                "removeIds":{"type":"array", "items":nonblank_string(), "description":"action=update: remove ids (trimmed); unknown ids are ignored. Repair remaining dependencies in the same update."},
                "setDeps":{"type":"array", "items":{"type":"object", "properties":{"id":{"type":"string", "description":"Exact existing node id; unlike creation this field is NOT trimmed."}, "deps":string_list()}, "required":["id","deps"], "additionalProperties":true}, "description":"action=update: replace dependencies for existing ids; empty deps clears them. Dependency ids are trimmed and deduplicated."},
                "reset":{"type":"array", "items":nonblank_string(), "description":"action=update: re-arm existing ids to pending and clear their results. Also reset skipped dependents when retrying; this is not automatic."},
                "cancel":{"type":"array", "items":nonblank_string(), "description":"action=update: drop work that is no longer wanted — mark these ids skipped and skip their dependents. Use this, not `reset`, for a node that is running or that a newer task has replaced: `reset` would re-arm it and leave the live child working with nobody waiting for its result."},
                "taskId":{"type":"string", "description":"The long task this plan belongs to. When set, dispatch records the plan's progress on that task automatically, so you do not have to update it by hand. A plan without one is still fine."}
            },
            "allOf":[{"if":{"properties":{"action":{"const":"set"}},"required":["action"]},"then":{"required":["nodes"]}}]
        }),
        "superagent_dispatch" => {
            json!({"type":"object", "additionalProperties":true, "properties":{"maxParallel":{"type":"integer", "minimum":1, "maximum":16, "default":4, "description":"Concurrent children within this wave, 1..16; default 4. All nodes ready at wave start are attempted, in batches if needed."}}})
        }
        "superagent_task" => {
            json!({
                "type":"object",
                "additionalProperties":true,
                "properties":{
                    "action":{"type":"string","enum":["create","update","list"],"default":"list","description":"create writes a task's first snapshot; update writes a new snapshot for an existing task, which is how its state changes; list reads every task back, folded to its latest snapshot."},
                    "taskId":{"type":"string","description":"Stable task identity. Required for create and update; the same id across snapshots is what makes them one task."},
                    "title":{"type":"string","description":"Short human-readable label for the task."},
                    "stage":{"type":"string","enum":["pending","working","needsPerson","failed","done"],"description":"Where the task stands. needsPerson means it is waiting on a person to answer or review."},
                    "nodes":{"type":"array","items":nonblank_string(),"description":"Node ids of the task DAG this task is running, when it has one."},
                    "error":{"type":"string","description":"Why the task stopped, when it did."}
                }
            })
        }
        _ => unreachable!("unknown superagent tool"),
    }
}

pub(super) fn tool_schema(name: &str) -> StableToolSchema {
    let description = match name {
        "superagent_task" => "Track a long task across restarts. create writes a task, update records a new state for it, and list shows every task with its latest state. State is stored in the session branch, so it survives a process restart but is scoped to the current branch: rewinding the session hides the task list. Use this for work that outlives one turn; use plan/dispatch for the execution graph itself.",
        "superagent_plan" => "Author the session-scoped in-memory execution DAG, NOT plan_mode's read-only design/approval workflow. set replaces nodes; update applies addNodes, removeIds, setDeps, reset, cancel in that order and validates atomically; show (default) inspects. The plan is editable while a wave runs: an edit invalidates that wave's results rather than being refused, so work that has become pointless can be dropped without waiting for unrelated workers. Nodes need self-contained tasks; give a node isolation:true when it writes files, so parallel nodes cannot overwrite each other. Declare expectFile/verify acceptance checks: only dispatch can mark completion, and exit 0 alone can include a turn-limited incomplete answer. Session switches clear this graph; it is not a durable queue. Pass taskId to bind the plan to a long task: dispatch then records the plan's progress on that task by itself. A node's `session` names the worker's saved conversation: reuse one name across the nodes of a task that continues (the worker keeps its history, its context, and its prefix cache, and will not repeat side effects it already performed), and leave it unset for independent one-off nodes (sharing one would show each node the others' unrelated work).",
        "superagent_dispatch" => "Queue one ready wave of the current plan and return at once; it runs on the background scheduler in bounded rpi child processes. Call superagent_plan(action=set, taskId=...) first — the taskId is required, because the wave's report is delivered against it. All pending nodes whose dependencies are done at wave start run in batches up to maxParallel, and a wave that unblocks more nodes queues the next wave by itself. The report arrives later as a message in this session; superagent_task(action=list) shows progress meanwhile. Overlapping dispatch/edit calls are rejected. Inspect results and acceptance checks before editing the plan; failed dependencies cause skipped nodes, and retries require explicit plan(update, reset). Shared files are not isolated: parallelize only independent work.",
        _ => unreachable!("unknown superagent tool"),
    };
    crate::kit::schema(name, description, &parameters(name).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Graph, Node, MAX_TASK_BYTES};

    fn registered(name: &str) -> Value {
        let schema = tool_schema(name);
        assert_eq!(schema.name.to_string_lossy(), name);
        assert!(!schema.description.to_string_lossy().is_empty());
        serde_json::from_str(&schema.parameters.to_string_lossy()).unwrap()
    }

    #[test]
    fn registered_schemas_are_json_and_allow_host_context() {
        for name in ["superagent_plan", "superagent_dispatch", "superagent_task"] {
            let schema = registered(name);
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["additionalProperties"], true); // includes __rpi
            assert!(schema["properties"].is_object());
        }
        assert_eq!(registered("superagent_plan")["properties"]["action"]["default"], "show");
        // Dispatch is always asynchronous now: an `async` switch would only
        // offer back the blocking mode this package no longer has.
        assert!(registered("superagent_dispatch")["properties"]["async"].is_null());
        // The node schema has to offer `isolation`, or the model has no way to
        // ask for an isolated working directory.
        let node = &registered("superagent_plan")["properties"]["nodes"]["items"];
        assert!(
            node["properties"]["isolation"].is_object(),
            "the node schema must offer isolation: {node}"
        );
        let parallel = &registered("superagent_dispatch")["properties"]["maxParallel"];
        assert_eq!(
            (
                parallel["minimum"].as_u64(),
                parallel["maximum"].as_u64(),
                parallel["default"].as_u64()
            ),
            (Some(1), Some(16), Some(4))
        );
    }

    #[test]
    fn plan_nested_items_and_conditional_requirement_match_parser() {
        let schema = registered("superagent_plan");
        let props = &schema["properties"];
        assert_eq!(props["nodes"]["minItems"], 1);
        assert_eq!(props["nodes"]["items"], props["addNodes"]["items"]);
        assert_eq!(props["nodes"]["items"]["required"], json!(["id", "task"]));
        let edit = &props["setDeps"]["items"];
        assert_eq!(edit["required"], json!(["id", "deps"]));
        assert_eq!(edit["properties"]["deps"]["items"]["type"], "string");
        assert_eq!(schema["allOf"][0]["then"]["required"], json!(["nodes"]));
        assert_eq!(props["action"]["default"], "show");
        assert!(Graph::from_nodes_value(&json!([])).is_err());
        assert!(Node::from_value(&json!({"id":"a"})).is_err());
        assert!(
            Graph::from_nodes_value(&json!([{"id":"a","task":"x","deps":["missing"]}])).is_err()
        );
    }

    #[test]
    fn node_defaults_bounds_and_utf8_rules_match_registered_metadata() {
        let schema = registered("superagent_plan");
        let fields = &schema["properties"]["nodes"]["items"]["properties"];
        let node = Node::from_value(&json!({"id":" a ","task":" x ","deps":["b"," b "]})).unwrap();
        assert_eq!(node.id, "a");
        assert_eq!(node.task, "x");
        assert_eq!(node.deps, vec!["b"]);
        assert_eq!(Some(node.max_turns), fields["maxTurns"]["default"].as_u64());
        assert_eq!(
            Some(node.timeout_secs),
            fields["timeoutSeconds"]["default"].as_u64()
        );
        let clamped =
            Node::from_value(&json!({"id":"a","task":"x","maxTurns":99,"timeoutSeconds":0}))
                .unwrap();
        assert_eq!(
            Some(clamped.max_turns),
            fields["maxTurns"]["maximum"].as_u64()
        );
        assert_eq!(
            Some(clamped.timeout_secs),
            fields["timeoutSeconds"]["minimum"].as_u64()
        );
        assert!(
            Node::from_value(&json!({"id":"a","task":"界".repeat(MAX_TASK_BYTES / 3 + 1)}))
                .is_err()
        );
        // Bounds apply to trimmed node tasks, not the raw JSON string length.
        assert!(Node::from_value(
            &json!({"id":"a","task":format!("{}x", " ".repeat(MAX_TASK_BYTES))})
        )
        .is_ok());
    }
}
