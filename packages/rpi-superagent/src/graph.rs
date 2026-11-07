//! The orchestration DAG: nodes, dependencies, statuses, plus the ready-set /
//! no-progress logic the scheduler is built on.
//!
//! Pure data and algorithms (no session, no child process), so every rule that
//! decides *what runs next* is unit-testable on its own.

use serde_json::{json, Value};

pub const DEFAULT_MAX_TURNS: u64 = 8;
pub const MAX_MAX_TURNS: u64 = 32;
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
pub const MIN_TIMEOUT_SECS: u64 = 10;
pub const MAX_TIMEOUT_SECS: u64 = 1800;
pub const MAX_TASK_BYTES: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Running => "running",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Skipped => "skipped",
        }
    }

    /// Read a status back from its name.
    ///
    /// Unknown names read as `pending`: a status this version does not know is
    /// work it cannot claim was done, and re-running is recoverable while
    /// skipping silently is not.
    pub fn parse(value: &str) -> Status {
        match value {
            "done" => Status::Done,
            "failed" => Status::Failed,
            "skipped" => Status::Skipped,
            // `running` becomes `pending`: a wave that was running when its host
            // died did not finish, so it has to be retried, not resumed.
            _ => Status::Pending,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Node {
    pub id: String,
    pub task: String,
    pub deps: Vec<String>,
    pub role: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Whether the node runs in its own working directory.
    ///
    /// Without it, parallel nodes share one directory and have to be given
    /// non-overlapping write ownership by hand — a rule the model can forget.
    /// With it, each node gets a directory of its own under
    /// `<cwd>/.rpi/superagent-work/<run id>/<node id>`, and the acceptance
    /// checks run there too, so `expectFile` names a path relative to that
    /// node's own work.
    pub isolation: bool,
    /// After the child exits successfully, this file must exist or the node is
    /// marked failed. The engine trusts exit codes otherwise, which lets a child
    /// that hit its turn cap "succeed" without producing anything.
    pub expect_file: Option<String>,
    /// After the child exits successfully, this shell command must exit 0.
    pub verify: Option<String>,
    /// The saved session the node's worker works in.
    ///
    /// Naming one makes a later node with the same name **resume** that worker:
    /// its history, and what it already learned about the repository, survive an
    /// earlier turn. Without it every node starts from nothing.
    ///
    /// The host allows one `rpi` process per session file, so a name also keeps
    /// two nodes from working the same session at once.
    pub session: Option<String>,
    pub max_turns: u64,
    pub timeout_secs: u64,
    pub status: Status,
    pub result: Option<String>,
    /// Why a node failed. `render` shows it so a blocked Ticket is explainable
    /// to a human, not just marked `!`.
    pub error: Option<String>,
}

impl Node {
    pub fn from_value(value: &Value) -> Result<Node, String> {
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or("every node needs a non-empty `id`")?
            .to_string();
        let task = value
            .get("task")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|task| !task.is_empty())
            .ok_or_else(|| format!("node `{id}` needs a non-empty `task`"))?
            .to_string();
        if task.len() > MAX_TASK_BYTES {
            return Err(format!("node `{id}` task exceeds {MAX_TASK_BYTES} bytes"));
        }
        let mut deps = parse_string_list(value.get("deps"), &format!("node `{id}` deps"))?;
        // Duplicate dependency edges are meaningless and would break the
        // indegree count in `check_acyclic`.
        let mut seen_deps: Vec<String> = Vec::with_capacity(deps.len());
        deps.retain(|dep| {
            if seen_deps.contains(dep) {
                false
            } else {
                seen_deps.push(dep.clone());
                true
            }
        });
        let role = string_field(value, "role");
        let provider = string_field(value, "provider");
        let model = string_field(value, "model");
        let expect_file = string_field(value, "expectFile");
        let verify = string_field(value, "verify");
        let session = string_field(value, "session");
        let isolation = value
            .get("isolation")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let max_turns = clamp(
            value.get("maxTurns").and_then(Value::as_u64),
            DEFAULT_MAX_TURNS,
            1,
            MAX_MAX_TURNS,
        );
        let timeout_secs = clamp(
            value.get("timeoutSeconds").and_then(Value::as_u64),
            DEFAULT_TIMEOUT_SECS,
            MIN_TIMEOUT_SECS,
            MAX_TIMEOUT_SECS,
        );
        Ok(Node {
            id,
            task,
            deps,
            role,
            provider,
            model,
            isolation,
            expect_file,
            verify,
            session,
            max_turns,
            timeout_secs,
            status: Status::Pending,
            result: None,
            error: None,
        })
    }

    fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "task": self.task,
            "deps": self.deps,
            "role": self.role,
            "provider": self.provider,
            "model": self.model,
            "isolation": self.isolation,
            "expectFile": self.expect_file,
            "verify": self.verify,
            "session": self.session,
            "maxTurns": self.max_turns,
            "timeoutSeconds": self.timeout_secs,
            "status": self.status.as_str(),
            "result": self.result,
            "error": self.error,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    /// The long task this graph belongs to, when it belongs to one.
    ///
    /// The task is the durable record; the graph is the in-memory execution
    /// plan. Linking them lets `dispatch` record the task's progress without
    /// the model having to remember to do it, which is the whole point: a
    /// caller that has to sync two things by hand will eventually forget.
    pub task_id: Option<String>,
}

impl Graph {
    pub fn from_nodes_value(value: &Value) -> Result<Graph, String> {
        let array = value.as_array().ok_or("`nodes` must be an array")?;
        if array.is_empty() {
            return Err("`nodes` must contain at least one node".into());
        }
        let mut nodes = Vec::with_capacity(array.len());
        for item in array {
            nodes.push(Node::from_value(item)?);
        }
        let graph = Graph {
            nodes,
            task_id: None,
        };
        graph.validate()?;
        Ok(graph)
    }

    pub fn to_details(&self) -> Value {
        json!({
            "nodes": self.nodes.iter().map(Node::to_value).collect::<Vec<_>>(),
            "taskId": self.task_id,
        })
    }

    /// Rebuild a graph from [`Graph::to_details`].
    ///
    /// This is what lets a process that never saw the plan run it: the effect
    /// log stores the details, and a scheduler in another process — or the same
    /// one after a restart — reconstructs the graph from them.
    ///
    /// Node status is preserved, with two deliberate exceptions:
    ///
    /// - `running` becomes `pending`. A node that was running when its host
    ///   disappeared did not finish, and its worker's output is gone with it, so
    ///   it has to run again — resuming it would claim work that never completed.
    /// - An unknown status becomes `pending` for the same reason: re-running is
    ///   recoverable, silently skipping is not.
    ///
    /// A node's `result` is kept, so a wave that is half done keeps the output of
    /// the half that is finished.
    pub fn from_details(value: &Value) -> Result<Graph, String> {
        let nodes = value
            .get("nodes")
            .ok_or("details need a `nodes` array")?;
        let array = nodes.as_array().ok_or("`nodes` must be an array")?;
        if array.is_empty() {
            return Err("`nodes` must contain at least one node".into());
        }
        let mut built = Vec::with_capacity(array.len());
        for item in array {
            let mut node = Node::from_value(item)?;
            if let Some(name) = item.get("status").and_then(Value::as_str) {
                node.status = Status::parse(name);
            }
            node.result = item
                .get("result")
                .and_then(Value::as_str)
                .map(str::to_string);
            node.error = item
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string);
            built.push(node);
        }
        let graph = Graph {
            nodes: built,
            task_id: value
                .get("taskId")
                .and_then(Value::as_str)
                .map(str::to_string),
        };
        graph.validate()?;
        Ok(graph)
    }

    pub fn get(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|node| node.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Node> {
        self.nodes.iter_mut().find(|node| node.id == id)
    }

    pub fn has_pending(&self) -> bool {
        self.nodes.iter().any(|node| node.status == Status::Pending)
    }

    /// Ids of `pending` nodes whose dependencies are all `done`.
    pub fn ready(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|node| node.status == Status::Pending)
            .filter(|node| {
                node.deps
                    .iter()
                    .all(|dep| self.get(dep).map(Node::is_done).unwrap_or(false))
            })
            .map(|node| node.id.clone())
            .collect()
    }

    /// Every remaining `pending` node — the caller reports these when nothing is
    /// ready, which is the "no progress" condition.
    pub fn pending_ids(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|node| node.status == Status::Pending)
            .map(|node| node.id.clone())
            .collect()
    }

    /// Mark every pending node that depends (transitively) on a failed/skipped
    /// node as skipped, so a dead branch cannot look dispatchable forever.
    pub fn propagate_skips(&mut self) {
        loop {
            let mut changed = false;
            let dead: Vec<String> = self
                .nodes
                .iter()
                .filter(|node| matches!(node.status, Status::Failed | Status::Skipped))
                .map(|node| node.id.clone())
                .collect();
            for node in &mut self.nodes {
                if node.status != Status::Pending {
                    continue;
                }
                if node.deps.iter().any(|dep| dead.contains(dep)) {
                    node.status = Status::Skipped;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// Unique ids, deps that exist, and no cycles.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen: Vec<&str> = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            if seen.contains(&node.id.as_str()) {
                return Err(format!("duplicate node id `{}`", node.id));
            }
            seen.push(&node.id);
        }
        for node in &self.nodes {
            for dep in &node.deps {
                if !seen.contains(&dep.as_str()) {
                    return Err(format!("node `{}` depends on unknown `{dep}`", node.id));
                }
            }
        }
        self.check_acyclic()
    }

    fn check_acyclic(&self) -> Result<(), String> {
        // Kahn's algorithm over the dependency edges.
        let mut indegree: Vec<usize> = self.nodes.iter().map(|node| node.deps.len()).collect();
        let mut queue: Vec<usize> = Vec::new();
        for (index, degree) in indegree.iter().enumerate() {
            if *degree == 0 {
                queue.push(index);
            }
        }
        let mut visited = 0usize;
        while let Some(index) = queue.pop() {
            visited += 1;
            let id = self.nodes[index].id.clone();
            for (other, node) in self.nodes.iter().enumerate() {
                if node.deps.contains(&id) {
                    indegree[other] -= 1;
                    if indegree[other] == 0 {
                        queue.push(other);
                    }
                }
            }
        }
        if visited == self.nodes.len() {
            Ok(())
        } else {
            let cyclic: Vec<String> = self
                .nodes
                .iter()
                .enumerate()
                .filter(|(index, _)| indegree[*index] > 0)
                .map(|(_, node)| node.id.clone())
                .collect();
            Err(format!("cycle detected among: {}", cyclic.join(", ")))
        }
    }

    pub fn render(&self) -> String {
        if self.nodes.is_empty() {
            return "plan is empty".into();
        }
        let done = self
            .nodes
            .iter()
            .filter(|node| node.status == Status::Done)
            .count();
        let mut lines = vec![format!("{done}/{} done", self.nodes.len())];
        for node in &self.nodes {
            let mark = match node.status {
                Status::Done => "x",
                Status::Failed => "!",
                Status::Skipped => "-",
                Status::Running => "~",
                Status::Pending => " ",
            };
            let deps = if node.deps.is_empty() {
                String::new()
            } else {
                format!(" deps=[{}]", node.deps.join(","))
            };
            // Show the isolation setting: without it a caller cannot tell
            // whether the field it passed was accepted or silently dropped.
            let isolation = if node.isolation { " isolated" } else { "" };
            let mut line = format!(
                "[{}] {} {}{deps}{isolation}",
                mark,
                node.id,
                node.status.as_str()
            );
            if let Some(error) = &node.error {
                let one_line = error.replace('\n', " ");
                let one_line = one_line.trim();
                if !one_line.is_empty() {
                    line.push_str(&format!(" — {one_line}"));
                }
            }
            lines.push(line);
        }
        lines.join("\n")
    }
}

impl Node {
    pub fn is_done(&self) -> bool {
        self.status == Status::Done
    }
}

fn clamp(value: Option<u64>, default: u64, min: u64, max: u64) -> u64 {
    value.unwrap_or(default).clamp(min, max)
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn parse_string_list(value: Option<&Value>, what: &str) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| format!("{what} must be an array of strings"))?;
    let mut out = Vec::with_capacity(array.len());
    for item in array {
        let text = item
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| format!("{what} must contain non-empty strings"))?;
        out.push(text.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(nodes: Value) -> Graph {
        Graph::from_nodes_value(&nodes).unwrap()
    }

    #[test]
    fn render_shows_the_failure_reason() {
        let mut g = graph(json!([{"id": "boom", "task": "x"}]));
        let node = g.get_mut("boom").unwrap();
        node.status = Status::Failed;
        node.error = Some("expected file was not produced: out.txt".to_string());
        let rendered = g.render();
        assert!(
            rendered.contains("[!] boom failed") && rendered.contains("out.txt"),
            "got: {rendered}"
        );
    }

    #[test]
    fn render_omits_a_reason_for_a_node_that_did_not_fail() {
        let mut g = graph(json!([{"id": "ok", "task": "x"}]));
        g.get_mut("ok").unwrap().status = Status::Done;
        let rendered = g.render();
        assert!(!rendered.contains("—"), "got: {rendered}");
    }

    #[test]
    fn rejects_duplicate_ids_and_unknown_deps() {
        let dup = Graph::from_nodes_value(&json!([
            {"id": "a", "task": "x"},
            {"id": "a", "task": "y"}
        ]));
        assert!(dup.unwrap_err().contains("duplicate"));

        let unknown = Graph::from_nodes_value(&json!([
            {"id": "a", "task": "x", "deps": ["ghost"]}
        ]));
        assert!(unknown.unwrap_err().contains("unknown"));
    }

    #[test]
    fn rejects_cycles() {
        let err = Graph::from_nodes_value(&json!([
            {"id": "a", "task": "x", "deps": ["b"]},
            {"id": "b", "task": "y", "deps": ["a"]}
        ]))
        .unwrap_err();
        assert!(err.contains("cycle"), "got: {err}");
    }

    /// A node that asks for isolation keeps the setting, and it is visible in
    /// the rendering so a caller can tell the field was accepted.
    #[test]
    fn a_node_keeps_its_isolation_setting() {
        let g = graph(json!([
            {"id": "a", "task": "x", "isolation": true},
            {"id": "b", "task": "y"}
        ]));
        assert!(g.get("a").unwrap().isolation, "isolation must survive parsing");
        assert!(!g.get("b").unwrap().isolation, "isolation defaults to off");
        let rendered = g.render();
        assert!(rendered.contains("isolated"), "got: {rendered}");
        // The setting is carried through to the details, so a plan can be
        // replayed with it intact.
        let details = g.to_details();
        assert_eq!(details["nodes"][0]["isolation"], json!(true));
    }

    /// A node that names a session keeps the name, and it survives the round
    /// trip to `to_details`, so a later node with the same name resumes that
    /// worker instead of starting from nothing.
    /// A graph written to details and read back is the same graph, so a
    /// scheduler in another process can run the plan it never saw.
    #[test]
    fn details_round_trip() {
        let g = graph(json!([
            {"id": "a", "task": "first", "session": "w1"},
            {"id": "b", "task": "second", "deps": ["a"], "expectFile": "out.txt", "isolation": true}
        ]));
        let details = g.to_details();
        let back = Graph::from_details(&details).unwrap();
        assert_eq!(back.nodes.len(), 2);
        assert_eq!(back.get("a").unwrap().task, "first");
        assert_eq!(back.get("a").unwrap().session.as_deref(), Some("w1"));
        let b = back.get("b").unwrap();
        assert_eq!(b.deps, vec!["a".to_string()]);
        assert_eq!(b.expect_file.as_deref(), Some("out.txt"));
        assert!(b.isolation);
    }

    #[test]
    fn details_keep_a_finished_node_finished() {
        // Re-running work that already finished would repeat its side effects.
        let mut g = graph(json!([
            {"id": "a", "task": "first"},
            {"id": "b", "task": "second", "deps": ["a"]}
        ]));
        g.get_mut("a").unwrap().status = Status::Done;
        g.get_mut("a").unwrap().result = Some("output".into());
        let back = Graph::from_details(&g.to_details()).unwrap();
        assert_eq!(back.get("a").unwrap().status, Status::Done);
        assert_eq!(back.get("a").unwrap().result.as_deref(), Some("output"));
        assert_eq!(back.get("b").unwrap().status, Status::Pending);
    }

    #[test]
    fn a_running_node_comes_back_pending() {
        // A wave that was running when its host died did not finish; its worker's
        // output is gone, so the node has to run again.
        let mut g = graph(json!([{"id": "a", "task": "x"}]));
        g.get_mut("a").unwrap().status = Status::Running;
        let back = Graph::from_details(&g.to_details()).unwrap();
        assert_eq!(back.get("a").unwrap().status, Status::Pending);
    }

    #[test]
    fn an_unknown_status_comes_back_pending() {
        // Skipping silently would be worse than re-running.
        let details = json!({
            "nodes": [{"id": "a", "task": "x", "status": "something-new"}],
            "taskId": null
        });
        let g = Graph::from_details(&details).unwrap();
        assert_eq!(g.get("a").unwrap().status, Status::Pending);
    }

    #[test]
    fn details_carry_the_task_id() {
        let details = json!({
            "nodes": [{"id": "a", "task": "x"}],
            "taskId": "T9"
        });
        assert_eq!(Graph::from_details(&details).unwrap().task_id.as_deref(), Some("T9"));
    }

    #[test]
    fn a_node_keeps_its_session_name() {
        let g = graph(json!([
            {"id": "a", "task": "x", "session": "worker-a"},
            {"id": "b", "task": "y"}
        ]));
        assert_eq!(
            g.get("a").unwrap().session.as_deref(),
            Some("worker-a"),
            "session must survive parsing"
        );
        assert_eq!(
            g.get("b").unwrap().session, None,
            "session defaults to unset"
        );
        let details = g.to_details();
        assert_eq!(details["nodes"][0]["session"], json!("worker-a"));
        assert_eq!(details["nodes"][1]["session"], json!(null));
    }

    #[test]
    fn ready_set_respects_dependencies() {        let mut g = graph(json!([
            {"id": "a", "task": "x"},
            {"id": "b", "task": "y"},
            {"id": "c", "task": "z", "deps": ["a", "b"]}
        ]));
        assert_eq!(g.ready(), vec!["a".to_string(), "b".to_string()]);
        g.get_mut("a").unwrap().status = Status::Done;
        assert_eq!(g.ready(), vec!["b".to_string()]);
        g.get_mut("b").unwrap().status = Status::Done;
        assert_eq!(g.ready(), vec!["c".to_string()]);
    }

    #[test]
    fn failed_dependency_skips_downstream_transitively() {
        let mut g = graph(json!([
            {"id": "a", "task": "x"},
            {"id": "b", "task": "y", "deps": ["a"]},
            {"id": "c", "task": "z", "deps": ["b"]}
        ]));
        g.get_mut("a").unwrap().status = Status::Failed;
        g.propagate_skips();
        assert_eq!(g.get("b").unwrap().status, Status::Skipped);
        assert_eq!(g.get("c").unwrap().status, Status::Skipped);
        assert!(!g.has_pending());
    }
}
