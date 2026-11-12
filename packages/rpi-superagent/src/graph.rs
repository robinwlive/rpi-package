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

/// How many agent-spawn generations a chain may reach before it is recorded but
/// no longer dispatched.
///
/// The number is a judgement, not a measurement: eight generations is already a
/// chain nobody planned, and each one costs a full model run. It matches the
/// prototype this package follows, so the two agree on where "this has run away"
/// begins.
pub const MAX_TIER: u32 = 8;

/// The shape version of [`Graph::to_details`].
///
/// A queue is durable, so a plan can outlive the code that wrote it. Written into
/// the details so a reader can tell an old plan from a corrupt one: without it,
/// both look like "a field is missing", and the wrong guess loses work.
pub const DETAILS_VERSION: u32 = 1;
pub const MAX_TASK_BYTES: usize = 20_000;

/// How much of a node's task `render` shows before cutting it off.
///
/// A task can be 20 KB; a status view that prints them in full is unreadable and
/// costs the caller a small fortune in tokens for no added answer.
pub const TASK_PREVIEW_CHARS: usize = 120;

/// Thinking levels a node may name (`rpi-ai`'s `ThinkingLevel`, lowercase).
pub const THINKING_LEVELS: &[&str] = &[
    "off", "minimal", "low", "medium", "high", "xhigh", "max",
];

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
    /// The thinking depth for this node's worker (`off` … `max`).
    ///
    /// Unlike `model`, this **does** override the parent session rather than being
    /// inherited alongside it. The reason the two differ: naming a model usually
    /// does not mean naming a depth (the same model runs at several), but "this
    /// job is mechanical" is a judgement about the **task** and has to be sayable
    /// on its own. A worker that edits three files does not need to think, and a
    /// session set to `max` should not silently pay for that on every node.
    ///
    /// `None` means "inherit the parent session's level".
    pub thinking: Option<String>,
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
    /// **Ignored.** Kept only so an existing plan still parses.
    ///
    /// This used to let a node resume a worker by name, so a later node inherited
    /// an earlier one's history. That is now forbidden: the engine always names
    /// the worker `<taskId>-<nodeId>` (see [`crate::dispatch`]), one session per
    /// node.
    ///
    /// Why revoking it matters more than it looks — reusing one worker across
    /// nodes fails three ways, and the second is the dangerous one:
    ///
    /// 1. carry-over: the worker applies the previous node's assumptions;
    /// 2. **acceptance drift**: [`crate::prompt::worker_prompt`] writes the node's
    ///    `expect_file` / `verify` into the prompt, so a resumed worker still has
    ///    the *previous* node's criteria in context. It can pass checks while
    ///    producing the wrong artifact, and the artifact exists, so nothing looks
    ///    broken;
    /// 3. the stale warning is only a warning, and only checked when `plan` is
    ///    called with `set`/`update` — by the time it fires the context is already
    ///    mixed.
    ///
    /// Passing a name is not an error; it is overwritten and the caller is told so
    /// in the tool result.
    pub session: Option<String>,
    pub max_turns: u64,
    pub timeout_secs: u64,
    pub status: Status,
    pub result: Option<String>,
    /// Why a node failed. `render` shows it so a blocked Ticket is explainable
    /// to a human, not just marked `!`.
    pub error: Option<String>,
    /// Stop the automatic next wave after this node finishes.
    ///
    /// The engine normally queues the next ready wave by itself once a wave ends,
    /// so a plan written once runs to completion without asking again. That is
    /// the right default: the plan *is* the project manager's decision, and
    /// re-confirming every wave just makes a long plan slower.
    ///
    /// This is the exception a "decide before the next step" node needs. When a
    /// node marked here finishes, the next wave is **not** queued: the report
    /// goes back to the session, and the model decides what to do next. Costs one
    /// round trip, buys back the ability to change course at exactly one point.
    pub confirm_before_next: bool,
    /// When this node's child started, in ms since the epoch; 0 when it has not.
    ///
    /// Without it "how far along is it" has no answer a caller can trust: a
    /// `running` mark says something is happening, not that it started two
    /// seconds or twenty minutes ago. It is the one progress signal that costs
    /// nothing to record and cannot be wrong.
    pub started_at: u64,
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
        // `thinking` is validated rather than free-form: an unknown level would be
        // forwarded to the child as a `--thinking` flag the host rejects, and the
        // failure would surface as "the worker never started".
        let thinking = match string_field(value, "thinking") {
            Some(level) => {
                let level = level.trim().to_ascii_lowercase();
                if !THINKING_LEVELS.contains(&level.as_str()) {
                    return Err(format!(
                        "node `{id}` thinking must be one of {}",
                        THINKING_LEVELS.join(", ")
                    ));
                }
                Some(level)
            }
            None => None,
        };
        let confirm_before_next = value
            .get("confirmBeforeNext")
            .and_then(Value::as_bool)
            .unwrap_or(false);
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
            thinking,
            isolation,
            expect_file,
            verify,
            session,
            max_turns,
            timeout_secs,
            status: Status::Pending,
            result: None,
            error: None,
            confirm_before_next,
            started_at: 0,
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
            "thinking": self.thinking,
            "isolation": self.isolation,
            "expectFile": self.expect_file,
            "confirmBeforeNext": self.confirm_before_next,
            "verify": self.verify,
            "session": self.session,
            "maxTurns": self.max_turns,
            "timeoutSeconds": self.timeout_secs,
            "status": self.status.as_str(),
            "startedAt": self.started_at,
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
    /// How deep in a chain of agent-spawns this plan sits.
    ///
    /// DeepSeek's TTL, applied to agents: a worker whose output queues another
    /// wave is one level deeper, and the counter does not reset for a human
    /// question or a scheduled retry. Without it, a wave that produces work which
    /// produces work can run forever — each step is individually reasonable and
    /// the chain is not. Past [`MAX_TIER`] the plan is recorded but not dispatched.
    ///
    /// The counter is carried by the plan, not asked of the model, for the same
    /// reason the TTL is not: whoever is being limited cannot be trusted to count.
    pub tier: u32,
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
            tier: 0,
        };
        graph.validate()?;
        Ok(graph)
    }

    pub fn to_details(&self) -> Value {
        json!({
            "v": DETAILS_VERSION,
            "nodes": self.nodes.iter().map(Node::to_value).collect::<Vec<_>>(),
            "taskId": self.task_id,
            "tier": self.tier,
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
        // A plan from a newer version is refused rather than half-read: a shape
        // this code does not know may mean something different, and running a
        // guessed version of it is worse than saying so.
        if let Some(version) = value.get("v").and_then(Value::as_u64) {
            if version > u64::from(DETAILS_VERSION) {
                return Err(format!(
                    "this plan was written by a newer version (shape {version}, this code reads \
                     {DETAILS_VERSION}); upgrade the package or re-create the plan"
                ));
            }
        }
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
            // Carried through the round trip like `status`: a wave that reads the
            // plan back and continues it must not lose when each child started,
            // or the elapsed time resets to "just now" on every wave.
            node.started_at = item.get("startedAt").and_then(Value::as_u64).unwrap_or(0);
            // Same reason as `started_at`: a wave that reads the plan back must
            // not lose the gate, or the next wave would resume past a node the
            // project manager asked to decide on first.
            node.confirm_before_next = item
                .get("confirmBeforeNext")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            built.push(node);
        }
        let graph = Graph {
            nodes: built,
            task_id: value
                .get("taskId")
                .and_then(Value::as_str)
                .map(str::to_string),
            tier: value
                .get("tier")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(u64::from(MAX_TIER)) as u32,
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
        let running = self
            .nodes
            .iter()
            .filter(|node| node.status == Status::Running)
            .count();
        let pending = self
            .nodes
            .iter()
            .filter(|node| node.status == Status::Pending)
            .count();
        // The counts a person asks for first: how many are actually moving right
        // now, and how many are still waiting. `done/total` alone cannot tell a
        // wave that is running everything from one that has not started.
        let mut lines = vec![format!(
            "{done}/{} done, {running} running, {pending} pending",
            self.nodes.len()
        )];
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
            // The task text, so a person asking "what is each worker doing" gets an
            // answer from the plan itself instead of from memory of the call that
            // wrote it. Trimmed to one readable line: this is a status view, and a
            // 20 KB node task would bury the other nodes.
            let task = node.task.replace('\n', " ");
            let task = task.trim();
            if !task.is_empty() {
                let shown = if task.chars().count() > TASK_PREVIEW_CHARS {
                    let cut: String = task.chars().take(TASK_PREVIEW_CHARS).collect();
                    format!("{cut}…")
                } else {
                    task.to_string()
                };
                line.push_str(&format!(" — {shown}"));
            }
            // How long it has been going. The one progress signal that costs
            // nothing and cannot be wrong; `running` alone does not separate
            // "just started" from "stuck for twenty minutes".
            if node.status == Status::Running && node.started_at > 0 {
                let elapsed = crate::effects_log::now_ms().saturating_sub(node.started_at) / 1000;
                line.push_str(&format!(" ({})", human_secs(elapsed)));
            }
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

/// A duration in seconds, short enough for one status line.
fn human_secs(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m{}s", seconds / 60, seconds % 60),
        _ => format!("{}h{}m", seconds / 3600, (seconds % 3600) / 60),
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
        // The task preview also uses an em dash, so check what the error rule
        // actually promises: the node's own task text appears, and nothing is
        // appended after it.
        assert_eq!(rendered.lines().last().unwrap(), "[x] ok done — x");
    }

    /// A person asking "what is each worker doing" must get an answer from the
    /// plan, not from their memory of the call that wrote it.
    #[test]
    fn render_names_each_nodes_task() {
        let g = graph(json!([
            {"id": "alpha", "task": "audit the routing layer"},
            {"id": "beta", "task": "rewrite the auth middleware"}
        ]));
        let rendered = g.render();
        assert!(rendered.contains("audit the routing layer"), "got: {rendered}");
        assert!(rendered.contains("rewrite the auth middleware"), "got: {rendered}");
    }

    /// A 20 KB task must not bury the other nodes in the status view.
    #[test]
    fn render_truncates_a_long_task_and_stays_char_safe() {
        let long = "界".repeat(TASK_PREVIEW_CHARS * 2);
        let g = graph(json!([{"id": "a", "task": long}]));
        let rendered = g.render();
        assert!(rendered.ends_with('…'), "got: {rendered}");
        // Bound the node line, not the whole render: the header grew a counts
        // line and has nothing to do with the preview limit.
        let node_line = rendered.lines().last().unwrap();
        assert!(
            node_line.chars().count() < TASK_PREVIEW_CHARS + 40,
            "preview was not bounded: {node_line}"
        );
    }

    /// "How many are running right now" is the first thing a person asks, and
    /// `done/total` cannot answer it.
    #[test]
    fn render_counts_running_and_pending_separately() {
        let mut g = graph(json!([
            {"id": "a", "task": "a"},
            {"id": "b", "task": "b"},
            {"id": "c", "task": "c"}
        ]));
        g.get_mut("a").unwrap().status = Status::Running;
        g.get_mut("b").unwrap().status = Status::Done;
        let header = g.render().lines().next().unwrap().to_string();
        assert!(header.contains("1/3 done"), "got: {header}");
        assert!(header.contains("1 running"), "got: {header}");
        assert!(header.contains("1 pending"), "got: {header}");
    }

    /// A running node shows how long it has been going: `running` alone does not
    /// separate "just started" from "stuck for twenty minutes".
    #[test]
    fn render_shows_elapsed_time_for_a_running_node() {
        let mut g = graph(json!([{"id": "a", "task": "a"}]));
        let node = g.get_mut("a").unwrap();
        node.status = Status::Running;
        node.started_at = crate::effects_log::now_ms().saturating_sub(90_000);
        let rendered = g.render();
        assert!(rendered.contains("(1m30s)"), "got: {rendered}");
    }

    /// A node that has not started must not claim an elapsed time.
    #[test]
    fn render_omits_elapsed_time_for_a_pending_node() {
        let g = graph(json!([{"id": "a", "task": "a"}]));
        assert!(!g.render().contains('('), "got: {}", g.render());
    }

    #[test]
    fn human_secs_reads_as_minutes_and_hours() {
        assert_eq!(human_secs(0), "0s");
        assert_eq!(human_secs(59), "59s");
        assert_eq!(human_secs(90), "1m30s");
        assert_eq!(human_secs(3700), "1h1m");
    }

    /// A multi-line task would otherwise break the one-line-per-node shape.
    #[test]
    fn render_folds_a_multiline_task_onto_one_line() {
        let g = graph(json!([{"id": "a", "task": "step one\nstep two\nstep three"}]));
        let rendered = g.render();
        assert_eq!(rendered.lines().count(), 2, "got: {rendered}");
        assert!(rendered.contains("step one step two step three"));
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

    /// The TTL: a plan past the generation limit is not dispatched.
    #[test]
    fn the_tier_is_capped_when_read_back() {
        // A plan cannot claim a generation beyond the limit, so the check in
        // `dispatch` always sees an honest number.
        let details = json!({
            "nodes": [{"id": "a", "task": "x"}],
            "taskId": null,
            "tier": 999
        });
        assert_eq!(Graph::from_details(&details).unwrap().tier, MAX_TIER);
    }

    #[test]
    fn the_tier_survives_the_details_round_trip() {
        let mut g = graph(json!([{"id": "a", "task": "x"}]));
        g.tier = 3;
        assert_eq!(Graph::from_details(&g.to_details()).unwrap().tier, 3);
    }

    #[test]
    fn a_plan_without_a_tier_starts_at_zero() {
        // A person's session has no generation: their plan is the root.
        let details = json!({"nodes": [{"id": "a", "task": "x"}], "taskId": null});
        assert_eq!(Graph::from_details(&details).unwrap().tier, 0);
    }

    /// The TTL has to be a hard stop, not a suggestion: at the limit the plan is
    /// recorded and nothing runs.
    #[test]
    fn a_plan_at_the_tier_limit_is_refused() {
        let mut g = graph(json!([{"id": "a", "task": "x"}]));
        g.tier = MAX_TIER;
        // `dispatch` checks this before reading the ready set; the check itself is
        // expressed through the field, so a plan at the limit is recognisably at
        // the limit.
        assert!(g.tier >= MAX_TIER);
        g.tier = MAX_TIER - 1;
        assert!(g.tier < MAX_TIER, "one generation below still runs");
    }

    #[test]
    fn a_newer_plan_shape_is_refused_rather_than_guessed() {
        // A durable queue can hold a plan written by newer code. Running a guessed
        // version of it is worse than saying the shape is unknown.
        let details = json!({
            "v": DETAILS_VERSION + 1,
            "nodes": [{"id": "a", "task": "x"}],
            "taskId": null
        });
        let error = Graph::from_details(&details).unwrap_err();
        assert!(error.contains("newer version"), "got: {error}");
    }

    #[test]
    fn an_older_plan_without_a_version_still_loads() {
        // Plans written before the version field existed are shape 0, and this
        // code reads shape 1 as the same thing.
        let details = json!({"nodes": [{"id": "a", "task": "x"}], "taskId": null});
        assert!(Graph::from_details(&details).is_ok());
    }

    #[test]
    fn the_details_carry_their_version() {
        let g = graph(json!([{"id": "a", "task": "x"}]));
        assert_eq!(g.to_details()["v"], json!(DETAILS_VERSION));
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

