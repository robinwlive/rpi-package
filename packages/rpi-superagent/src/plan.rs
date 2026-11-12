//! The `plan` tool: author and edit the task DAG.
//!
//! Scheduling lives in `dispatch`; this tool only changes the graph's *shape*
//! (and can re-arm node state). Every result carries the graph in `details` so
//! it can be replayed from the session branch.
//!
//! There is deliberately **no** way for the model to mark a node `done`/`failed`:
//! completion is only ever written by `dispatch`, so the plan cannot be
//! "completed" by hand. `reset` is the one state edit the model keeps.

use crate::control::Control;
use crate::graph::{Graph, Node, Status};
use crate::kit::{result_with_details, Progress};
use crate::lock_graph;
use serde_json::Value;

/// The agent-spawn generation this process was started at.
///
/// Read from the environment rather than from the tool arguments: the model is
/// the party being limited, so it must not be able to say which generation it is
/// in. A person's own session has no such variable, so their plan starts at zero.
fn tier_from_env() -> u32 {
    std::env::var(crate::spawn::TIER_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(0)
        .min(crate::graph::MAX_TIER)
}

pub fn plan(params: &Value, control: &Control, _progress: &Progress) -> Result<String, String> {
    control.check()?;
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("show");
    let mut guard = lock_graph(params);
    // The plan is editable while a wave runs.
    //
    // It used to be refused ("wait for the wave to finish"), which quietly
    // assumed a job is written once and then executed. Real work is not like
    // that: a step becomes pointless, or a better version of it arrives, while
    // the other workers are still busy. Forbidding the edit at exactly that
    // moment forced the caller to wait for unrelated work to finish before it
    // could cancel something already known to be wasted.
    //
    // What the wave is doing is unaffected: `dispatch` snapshots the graph and
    // hands the snapshot to the children, and results are applied only if the
    // generation still matches. An edit bumps the generation, so the running
    // wave's results are discarded rather than written onto the new plan.
    let graph = guard.graph_mut();
    match action {
        "set" => {
            let nodes = params
                .get("nodes")
                .ok_or("`nodes` is required for action=set")?;
            *graph = Graph::from_nodes_value(nodes)?;
            // A plan may name the long task it belongs to. `dispatch` then
            // records that task's progress without the caller having to
            // update it by hand.
            graph.task_id = task_id_of(params);
            // The generation comes from the environment the worker was started
            // with, not from the arguments: a limit the limited party can pass is
            // not a limit. A person starting a plan in their own session has no
            // such variable, which is exactly right — that is generation zero.
            graph.tier = tier_from_env();
        }
        "update" => {
            let mut next = graph.clone();
            apply_update(&mut next, params)?;
            next.validate()?;
            if params.get("taskId").is_some() {
                next.task_id = task_id_of(params);
            }
            *graph = next;
        }
        "show" => {}
        other => {
            return Err(format!(
                "unknown plan action `{other}` (expected set, update, show)"
            ))
        }
    }
    // Sessions named by this plan, measured so a caller can see when a resumed
    // worker has grown past what re-sending it every turn is worth. Reported at
    // `set`/`update` because that is when the choice is still cheap to change.
    let render = graph.render();
    let result = result_with_details(&render, graph.to_details());
    if matches!(action, "set" | "update") {
        guard.advance_generation();
    }
    Ok(result)
}

/// The task id a plan is bound to, when it names one.
fn task_id_of(params: &Value) -> Option<String> {
    params
        .get("taskId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn apply_update(graph: &mut Graph, params: &Value) -> Result<(), String> {
    if let Some(add) = params.get("addNodes") {
        let array = add.as_array().ok_or("`addNodes` must be an array")?;
        for item in array {
            let node = Node::from_value(item)?;
            if graph.get(&node.id).is_some() {
                return Err(format!("node `{}` already exists", node.id));
            }
            graph.nodes.push(node);
        }
    }
    if let Some(remove) = params.get("removeIds") {
        let array = remove.as_array().ok_or("`removeIds` must be an array")?;
        let mut removed: Vec<String> = Vec::with_capacity(array.len());
        for item in array {
            let id = item
                .as_str()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .ok_or("`removeIds` must contain non-empty strings")?;
            removed.push(id.to_string());
        }
        graph.nodes.retain(|node| !removed.contains(&node.id));
        // Drop the edges pointing at them.
        //
        // Without this, `validate()` rejects the whole update for "unknown dep",
        // so the *good* half of the same update — the replacement node — is
        // discarded too. Replacing a task is one update ("drop the old, here is
        // the new"), and it failing because a dependent still named the old node
        // would force the caller to remove the edges by hand first: exactly the
        // cleanup-before-you-can-act cost this tool exists to remove.
        //
        // The dependent keeps running; it simply no longer waits for a node that
        // does not exist.
        if !removed.is_empty() {
            for node in &mut graph.nodes {
                node.deps.retain(|dep| !removed.contains(dep));
            }
        }
    }
    if let Some(set_deps) = params.get("setDeps") {
        let array = set_deps.as_array().ok_or("`setDeps` must be an array")?;
        for item in array {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .ok_or("each `setDeps` entry needs an `id`")?;
            let deps = item
                .get("deps")
                .and_then(Value::as_array)
                .ok_or("each `setDeps` entry needs a `deps` array")?;
            let mut parsed = Vec::with_capacity(deps.len());
            for dep in deps {
                let dep = dep
                    .as_str()
                    .map(str::trim)
                    .filter(|dep| !dep.is_empty())
                    .ok_or("`deps` must contain non-empty strings")?
                    .to_string();
                // Match Node::from_value: duplicate edges are one dependency,
                // otherwise the cycle detector's indegree can never reach zero.
                if !parsed.contains(&dep) {
                    parsed.push(dep);
                }
            }
            let node = graph
                .get_mut(id)
                .ok_or_else(|| format!("node `{id}` not found"))?;
            node.deps = parsed;
        }
    }
    if let Some(reset) = params.get("reset") {
        let array = reset.as_array().ok_or("`reset` must be an array")?;
        for item in array {
            let id = item
                .as_str()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .ok_or("`reset` must contain non-empty strings")?;
            let node = graph
                .get_mut(id)
                .ok_or_else(|| format!("node `{id}` not found"))?;
            node.status = Status::Pending;
            node.result = None;
            // A re-armed node has not failed yet: a stale reason would make a
            // pending node look like it already failed.
            node.error = None;
            // It is about to be run again (or has just been cancelled), so the
            // old start time no longer describes it.
            node.started_at = 0;
        }
    }
    if params.get("cancel").is_some() {
        // Drop work that is no longer wanted.
        //
        // This is not `reset`. A node that is `running` has a live child; marking
        // it `pending` would leave that child working with nobody waiting for it,
        // and the next wave would start a second copy of the same job. `cancel`
        // says `skipped` instead: nobody wants this result any more.
        //
        // Its dependents become skipped too — `propagate_skips` does that below,
        // because a node that needed the cancelled result cannot proceed either.
        for id in node_ids(params, "cancel")? {
            let node = graph
                .get_mut(&id)
                .ok_or_else(|| format!("node `{id}` not found"))?;
            node.status = Status::Skipped;
            node.result = None;
            node.error = None;
            node.started_at = 0;
        }
        graph.propagate_skips();
    }
    Ok(())
}

/// A list of node ids from one array parameter, trimmed and non-empty.
fn node_ids(params: &Value, name: &str) -> Result<Vec<String>, String> {
    let array = params
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("`{name}` must be an array"))?;
    array
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .ok_or_else(|| format!("`{name}` must contain non-empty strings"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seeded() -> Graph {
        Graph::from_nodes_value(&json!([
            {"id": "a", "task": "x"},
            {"id": "b", "task": "y", "deps": ["a"]}
        ]))
        .unwrap()
    }

    #[test]
    fn update_can_add_remove_and_reset() {
        let mut g = seeded();
        apply_update(
            &mut g,
            &json!({"addNodes": [{"id": "c", "task": "z", "deps": ["b"]}]}),
        )
        .unwrap();
        assert_eq!(g.nodes.len(), 3);

        apply_update(&mut g, &json!({"setDeps": [{"id": "b", "deps": []}]})).unwrap();
        assert!(g.get("b").unwrap().deps.is_empty());

        apply_update(&mut g, &json!({"reset": ["a"]})).unwrap();
        assert_eq!(g.get("a").unwrap().status, Status::Pending);

        apply_update(&mut g, &json!({"removeIds": ["c"]})).unwrap();
        assert!(g.get("c").is_none());
    }

    #[test]
    fn reset_clears_a_previous_failure_reason() {
        let mut g = seeded();
        let node = g.get_mut("a").unwrap();
        node.status = Status::Failed;
        node.error = Some("boom".to_string());
        apply_update(&mut g, &json!({"reset": ["a"]})).unwrap();
        let node = g.get("a").unwrap();
        assert_eq!(node.status, Status::Pending);
        // A re-armed node must not still show why it failed last time.
        assert_eq!(node.error, None);
        assert!(!g.render().contains("boom"), "got: {}", g.render());
    }

    #[test]
    fn dependency_updates_accept_duplicate_edges_like_plan_creation() {
        let mut graph = seeded();
        apply_update(
            &mut graph,
            &json!({"setDeps":[{"id":"b","deps":["a", " a "]}]}),
        )
        .unwrap();
        graph
            .validate()
            .expect("duplicate dependency edges are not a cycle");
        assert_eq!(graph.get("b").unwrap().deps, vec!["a"]);
        assert_eq!(graph.ready(), vec!["a"]);
        graph.get_mut("a").unwrap().status = Status::Done;
        assert_eq!(graph.ready(), vec!["b"]);
    }

    #[test]
    fn dependency_edits_require_nested_id_and_deps_array() {
        for entry in [
            json!({"deps": []}),
            json!({"id": "b"}),
            json!({"id": "b", "deps": "a"}),
            json!({"id": "b", "deps": [null]}),
            json!({"id": "b", "deps": ["  "]}),
            json!({"id": " b ", "deps": []}), // exact id lookup, no trim
        ] {
            assert!(apply_update(&mut seeded(), &json!({"setDeps": [entry]})).is_err());
        }
        let mut graph = seeded();
        apply_update(
            &mut graph,
            &json!({"addNodes": [], "setDeps": [{"id": "b", "deps": []}], "__rpi": {"trace": [1]}}),
        )
        .unwrap();
        graph.validate().unwrap();
        assert!(graph.get("b").unwrap().deps.is_empty());
    }

    #[test]
    fn update_cannot_mark_nodes_done() {
        // Completion is dispatch-only: there is no `status` action, so a stray
        // `status` field is ignored and cannot hand-mark a node done.
        let mut g = seeded();
        apply_update(&mut g, &json!({"status": [{"id": "a", "status": "done"}]})).unwrap();
        assert_eq!(g.get("a").unwrap().status, Status::Pending);
    }

    /// A node whose work became pointless can be dropped while other workers
    /// keep going. This is the case the package exists for: the plan is not
    /// written once and executed, it is steered.
    #[test]
    fn cancel_skips_a_running_node_and_its_dependents() {
        let mut g = seeded();
        g.get_mut("a").unwrap().status = Status::Running;
        g.get_mut("a").unwrap().started_at = 12345;
        // `b` depends on the cancelled node, so it cannot proceed either.
        apply_update(&mut g, &json!({"cancel": ["a"]})).unwrap();
        assert_eq!(g.get("a").unwrap().status, Status::Skipped);
        assert_eq!(g.get("a").unwrap().started_at, 0);
        assert_eq!(g.get("b").unwrap().status, Status::Skipped);
    }

    /// `cancel` must not re-arm: marking a live node pending would leave its
    /// child running for nobody and start a second copy on the next wave.
    #[test]
    fn cancel_does_not_rearm_a_node_the_way_reset_does() {
        let mut g = seeded();
        g.get_mut("a").unwrap().status = Status::Running;
        apply_update(&mut g, &json!({"cancel": ["a"]})).unwrap();
        assert_ne!(
            g.get("a").unwrap().status,
            Status::Pending,
            "cancel re-armed the node instead of dropping it"
        );
    }

    /// Cancelling one node leaves its siblings alone.
    #[test]
    fn cancel_leaves_unrelated_nodes_running() {
        let mut g = seeded();
        g.get_mut("a").unwrap().status = Status::Running;
        g.get_mut("b").unwrap().status = Status::Running;
        apply_update(&mut g, &json!({"cancel": ["b"]})).unwrap();
        assert_eq!(g.get("a").unwrap().status, Status::Running);
        assert_eq!(g.get("b").unwrap().status, Status::Skipped);
    }

    /// Cancelling a node must not touch work that already finished.
    ///
    /// `propagate_skips` walks dependents, and a dependent that is `done` has a
    /// result the person may already have read. Overwriting it with `skipped`
    /// would erase finished work because unrelated work was cancelled.
    #[test]
    fn cancel_does_not_erase_an_already_finished_dependent() {
        let mut g = seeded();
        g.get_mut("a").unwrap().status = Status::Running;
        g.get_mut("b").unwrap().status = Status::Done;
        g.get_mut("b").unwrap().result = Some("finished work".into());
        apply_update(&mut g, &json!({"cancel": ["a"]})).unwrap();
        let b = g.get("b").unwrap();
        assert_eq!(b.status, Status::Done, "finished work was skipped");
        assert_eq!(b.result.as_deref(), Some("finished work"));
    }

    /// A replacement task must be able to take over from a cancelled one.
    ///
    /// The live scenario: `a` is obsolete, so the person cancels it and adds
    /// `a2` with the updated instructions, while `b` — still waiting — should now
    /// depend on `a2`. If the new node cannot be wired up in the same update, the
    /// person is back to cleaning up by hand before they can act.
    /// Dropping the dangling edge must not eat edges written in the same update.
    ///
    /// `removeIds` runs before `setDeps`, so the cleanup only clears edges that
    /// still point at a removed id. An edge the same update deliberately sets to a
    /// surviving node must be untouched — otherwise the fix for one problem
    /// silently breaks the other half of the same edit.
    #[test]
    fn removing_a_node_keeps_deps_set_to_surviving_nodes() {
        let mut g = seeded();
        apply_update(
            &mut g,
            &json!({
                "removeIds": ["a"],
                "addNodes": [{"id": "a2", "task": "replacement"}],
                "setDeps": [{"id": "b", "deps": ["a2"]}]
            }),
        )
        .unwrap();
        assert_eq!(
            g.get("b").unwrap().deps,
            vec!["a2".to_string()],
            "the update's own setDeps was cleared by the removeIds cleanup"
        );
    }

    #[test]
    fn a_replacement_node_can_take_over_from_a_cancelled_one() {
        let mut g = seeded();
        g.get_mut("a").unwrap().status = Status::Running;
        apply_update(
            &mut g,
            &json!({
                "cancel": ["a"],
                "addNodes": [{"id": "a2", "task": "the updated version"}],
                "setDeps": [{"id": "b", "deps": ["a2"]}]
            }),
        )
        .unwrap();
        assert_eq!(g.get("a").unwrap().status, Status::Skipped);
        // `b` was skipped when `a` was cancelled, so it has to be re-armed to run
        // against the replacement.
        apply_update(&mut g, &json!({"reset": ["b"]})).unwrap();
        assert_eq!(g.get("b").unwrap().status, Status::Pending);
        assert_eq!(g.get("b").unwrap().deps, vec!["a2".to_string()]);
        // The replacement is what runs next.
        assert_eq!(g.ready(), vec!["a2".to_string()]);
    }

    #[test]
    fn cancel_rejects_an_unknown_node() {
        let mut g = seeded();
        assert!(apply_update(&mut g, &json!({"cancel": ["ghost"]}))
            .unwrap_err()
            .contains("not found"));
    }

    /// Removing a node that others depend on must not dead-end the edit.
    ///
    /// A person replacing a task says "drop the old one, here is the new one" in a
    /// single update. The dependents' edges to the removed node would fail
    /// `validate()` ("unknown dep"), and because validation runs on the whole
    /// graph the *good* half of the edit — the new node — is thrown away too. The
    /// person then has to discover and name every dependent edge to get anything
    /// done, which is exactly the "wait, let me clean up first" cost this feature
    /// exists to remove.
    #[test]
    fn removing_a_node_that_others_depend_on_does_not_fail_the_edit() {
        let mut g = seeded();
        // `b` depends on `a`; drop `a` and add its replacement.
        assert!(
            apply_update(
                &mut g,
                &json!({"removeIds": ["a"], "addNodes": [{"id": "a2", "task": "replacement"}]})
            )
            .is_ok(),
            "replacing a node that has dependents was rejected"
        );
        assert!(g.validate().is_ok(), "the edited graph is not valid");
        // The dependent survives, without a dangling edge to the removed node.
        assert!(g.get("b").is_some());
        assert!(
            !g.get("b").unwrap().deps.iter().any(|d| d == "a"),
            "b still points at the removed node"
        );
    }

    #[test]
    fn update_rejects_unknown_dep_and_cycle() {
        let mut g = seeded();
        // A missing dep is caught by validate(), not apply_update().
        apply_update(
            &mut g,
            &json!({"setDeps": [{"id": "b", "deps": ["ghost"]}]}),
        )
        .unwrap();
        assert!(g.validate().unwrap_err().contains("unknown"));

        let mut g = seeded();
        apply_update(&mut g, &json!({"setDeps": [{"id": "a", "deps": ["b"]}]})).unwrap();
        assert!(g.validate().unwrap_err().contains("cycle"));
    }
}
