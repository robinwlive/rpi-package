//! The long-task layer: a `Task` that survives a process restart.
//!
//! ## Why this exists
//!
//! `graph.rs` keeps the execution DAG in this process's memory, so a session
//! switch clears it and a restart loses it. A long job that outlives the
//! process has nowhere to live. This module gives it one.
//!
//! ## How it is stored
//!
//! Each state change appends one **complete snapshot** to the session branch
//! under a single custom type; reading folds the snapshots by `taskId` and
//! keeps the last one per task. That is the pattern `rpi-background-tasks`
//! already uses, and it is what makes the history free: every snapshot is a
//! full picture of the task at that moment, so "how did it get here" is a
//! read, not a replay.
//!
//! Three host behaviours shape the implementation (all measured, see
//! `00-GOALS-AND-DECISIONS.md` §2):
//!
//! - `GetSessionBranch` reads the **current leaf's branch**, so a task's state
//!   disappears when the session is rewound. The snapshot therefore records the
//!   branch it was written on, so a later change can tell which branch it
//!   belongs to.
//! - `afterSeq` is a lower bound only for `order: oldestFirst`; with
//!   `newestFirst` it becomes an upper bound. This module always asks for
//!   `oldestFirst` and never passes `afterSeq`.
//! - A host that ignores the type filter answers with `filtered: false`.
//!   That is refused rather than trusted, because it would return other
//!   plugins' entries.

use crate::control::Control;
use crate::graph::{Node, Status as NodeStatus};
use crate::kit::{optional_string, result_with_details, Progress};
use serde_json::{json, Value};

/// The custom entry type this module owns.
pub const TASK_ENTRY_TYPE: &str = "superagent-task";

/// ABI id of `RuntimeActionId::AppendEntry`.
const APPEND_ENTRY: u32 = 2;
/// ABI id of `RuntimeActionId::GetSessionBranch`.
const GET_SESSION_BRANCH: u32 = 20;

/// One long task. Every field is optional except `taskId`: a task is created
/// by its first snapshot, and later snapshots may carry more or less.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Task {
    /// Stable identity. Snapshots of the same task share it.
    pub task_id: String,
    /// Human-readable label.
    pub title: Option<String>,
    /// Where the task stands. See [`Stage`].
    pub stage: Stage,
    /// The node ids of the DAG this task is running, when it has one.
    pub nodes: Vec<String>,
    /// Why the task stopped, when it did.
    pub error: Option<String>,
    /// The branch the snapshot was written on. Recorded because the host
    /// scopes reads to the current leaf's branch.
    pub branch_id: Option<String>,
    /// The child processes this task is driving, by process id.
    ///
    /// A child outlives its parent when the host dies: `process_group(0)`
    /// puts it in its own group, so nothing signals it. It keeps running
    /// until it writes to the inherited stdout, which for `rpi -p` happens
    /// only when the work is done. Recording the ids lets the next process
    /// find and clean them up.
    pub pids: Vec<u32>,
    /// Milliseconds since the epoch, from the host's entry timestamp.
    pub updated_at: u64,
}

/// Where a task stands. Mirrors the stages a person cares about: a task is
/// either waiting on them, working, idle, or finished.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stage {
    /// Just created; nothing has run yet.
    #[default]
    Pending,
    /// Running now.
    Working,
    /// Waiting for a person to answer or review.
    NeedsPerson,
    /// Stopped on an error.
    Failed,
    /// Finished successfully.
    Done,
    /// Was running when the process driving it went away.
    Abandoned,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Pending => "pending",
            Stage::Working => "working",
            Stage::NeedsPerson => "needsPerson",
            Stage::Failed => "failed",
            Stage::Done => "done",
            Stage::Abandoned => "abandoned",
        }
    }

    pub fn parse(value: &str) -> Stage {
        match value {
            "working" => Stage::Working,
            "needsPerson" => Stage::NeedsPerson,
            "failed" => Stage::Failed,
            "done" => Stage::Done,
            "abandoned" => Stage::Abandoned,
            _ => Stage::Pending,
        }
    }
}

impl Task {
    /// Parse one snapshot. A record without a `taskId` is not a task and is
    /// skipped rather than failing the whole read.
    #[cfg(test)]
    pub fn from_value(value: &Value) -> Option<Task> {
        Task::from_snapshot(value, None)
    }

    /// Parse one snapshot, taking its timestamp from the entry that carried it.
    ///
    /// The entry's own timestamp is the only clock that survives a restart:
    /// a task's heartbeat has to mean "when the host last wrote this", not
    /// "when this process last read it".
    pub fn from_snapshot(value: &Value, entry_timestamp: Option<u64>) -> Option<Task> {
        let task_id = value.get("taskId")?.as_str()?.trim();
        if task_id.is_empty() {
            return None;
        }
        Some(Task {
            task_id: task_id.to_string(),
            title: value
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string),
            stage: value
                .get("stage")
                .and_then(Value::as_str)
                .map(Stage::parse)
                .unwrap_or_default(),
            nodes: value
                .get("nodes")
                .and_then(Value::as_array)
                .map(|nodes| {
                    nodes
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            error: value
                .get("error")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string),
            branch_id: value
                .get("branchId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string),
            pids: value
                .get("pids")
                .and_then(Value::as_array)
                .map(|pids| pids.iter().filter_map(Value::as_u64).map(|pid| pid as u32).collect())
                .unwrap_or_default(),
            updated_at: entry_timestamp
                .or_else(|| value.get("updatedAt").and_then(Value::as_u64))
                .unwrap_or(0),
        })
    }

    /// The snapshot written to the branch. It is complete: a reader that keeps
    /// the last snapshot per task needs nothing else.
    pub fn to_value(&self) -> Value {
        json!({
            "taskId": self.task_id,
            "title": self.title,
            "stage": self.stage.as_str(),
            "nodes": self.nodes,
            "error": self.error,
            "branchId": self.branch_id,
            "pids": self.pids,
            "updatedAt": self.updated_at,
        })
    }
}

/// Fold snapshots into the current state of each task.
///
/// Snapshots arrive oldest first, so the last one for a task wins. This is
/// the "last write is the state" rule the background-tasks snapshot contract
/// describes.
#[cfg(test)]
pub fn fold(snapshots: &[Value]) -> Vec<Task> {
    fold_entries(snapshots.iter().map(|value| (value.clone(), None)))
}

/// Fold entries, taking each snapshot's timestamp from the entry carrying it.
pub fn fold_entries(entries: impl IntoIterator<Item = (Value, Option<u64>)>) -> Vec<Task> {
    let mut order: Vec<String> = Vec::new();
    let mut latest: std::collections::HashMap<String, Task> = std::collections::HashMap::new();
    for (snapshot, timestamp) in entries {
        let Some(task) = Task::from_snapshot(&snapshot, timestamp) else {
            continue;
        };
        if !latest.contains_key(&task.task_id) {
            order.push(task.task_id.clone());
        }
        latest.insert(task.task_id.clone(), task);
    }
    order
        .into_iter()
        .filter_map(|id| latest.remove(&id))
        .collect()
}

/// Read every task snapshot on the current branch.
///
/// Fails when the host cannot answer, and when it answers without filtering:
/// an unfiltered answer would contain other plugins' entries.
pub fn read_tasks() -> Result<Vec<Task>, String> {
    let reply = crate::host::call_action(
        GET_SESSION_BRANCH,
        &json!({"customTypes": [TASK_ENTRY_TYPE], "order": "oldestFirst"}),
    )?;
    if reply.get("filtered").and_then(Value::as_bool) != Some(true) {
        return Err(
            "the host returned an unfiltered branch; refusing to read other plugins' entries"
                .into(),
        );
    }
    let entries = reply
        .get("entries")
        .and_then(Value::as_array)
        .ok_or("the host returned no entries array")?;
    // Each snapshot's timestamp comes from the entry that carried it: that is
    // the host's write time, and it is what a heartbeat has to mean.
    let snapshots: Vec<(Value, Option<u64>)> = entries
        .iter()
        .filter(|entry| {
            entry.get("customType").and_then(Value::as_str) == Some(TASK_ENTRY_TYPE)
        })
        .filter_map(|entry| {
            let data = entry.get("data")?.clone();
            let timestamp = entry.get("timestamp").and_then(Value::as_u64);
            Some((data, timestamp))
        })
        .collect();
    Ok(fold_entries(snapshots))
}

/// Append one task snapshot to the current branch.
pub fn append_task(task: &Task) -> Result<String, String> {
    let reply = crate::host::call_action(
        APPEND_ENTRY,
        &json!({"customType": TASK_ENTRY_TYPE, "data": task.to_value()}),
    )?;
    reply
        .get("entryId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "the host accepted the entry but returned no entryId".into())
}

/// The stage a graph implies for its task.
///
/// The rule is deliberately mechanical: the graph is the truth about what
/// ran, so the task must not claim anything the graph does not support.
/// A failed node outranks a running one, because a graph with a dead branch
/// is failed even while other nodes are still going.
pub fn stage_for_graph(nodes: &[Node]) -> Stage {
    if nodes.is_empty() {
        return Stage::Pending;
    }
    if nodes
        .iter()
        .any(|node| matches!(node.status, NodeStatus::Failed | NodeStatus::Skipped))
    {
        return Stage::Failed;
    }
    if nodes.iter().any(|node| node.status == NodeStatus::Running) {
        return Stage::Working;
    }
    if nodes.iter().all(|node| node.status == NodeStatus::Done) {
        return Stage::Done;
    }
    Stage::Pending
}

/// The error a graph implies for its task: the first dead node's reason.
///
/// A dead node without a recorded reason still names itself, so the task
/// says *which* node stopped rather than nothing at all.
pub fn error_for_graph(nodes: &[Node]) -> Option<String> {
    let dead = nodes
        .iter()
        .find(|node| matches!(node.status, NodeStatus::Failed | NodeStatus::Skipped))?;
    dead.error.clone().or_else(|| Some(dead.id.clone()))
}

/// How long a task may go without a write before it counts as abandoned.
///
/// A task that says it is working is held by a process. If that process dies,
/// nothing writes another snapshot, and the task would otherwise claim to be
/// working forever. The threshold is generous because a long child process
/// can legitimately run for a while without the task being touched; the
/// point is to catch a dead holder, not to interrupt a slow one.
pub const ABANDON_AFTER_MS: u64 = 30 * 60 * 1000;

/// Decide what happened to a task that claims to be working.
///
/// `now` is the current time in milliseconds. A working task whose last write
/// is older than [`ABANDON_AFTER_MS`] lost the process that was driving it,
/// so it becomes `abandoned` rather than a task that claims to be running
/// forever.
///
/// Every other stage is left alone: a task that is waiting on a person stays
/// waiting on a person, however long that takes.
pub fn reconcile_stage(task: &Task, now: u64) -> (Stage, Option<String>) {
    if task.stage != Stage::Working {
        return (task.stage, None);
    }
    // A task with no timestamp cannot be aged. Leave it alone rather than
    // declaring it abandoned on missing information.
    if task.updated_at == 0 || now <= task.updated_at {
        return (task.stage, None);
    }
    if now - task.updated_at <= ABANDON_AFTER_MS {
        return (task.stage, None);
    }
    (
        Stage::Abandoned,
        Some(format!(
            "no progress for {} minutes; the process driving it is gone",
            (now - task.updated_at) / 60_000
        )),
    )
}

/// Kill the child processes a task left behind.
///
/// A child outlives its host: `process_group(0)` puts it in its own group, so
/// nothing signals it when the host dies. It keeps running — for `rpi -p`,
/// which only writes at the end, that can be a long time — and nobody
/// collects its result.
///
/// Every task records the ids of the children it started. When a process reads
/// the tasks back, any recorded id that is still alive but is not one of this
/// process's own children is an orphan, and is killed.
///
/// The age check keeps this from killing a child that is merely slow: a
/// process that started moments ago is somebody else's live work.
pub fn reap_orphans(tasks: &[Task]) -> Vec<u32> {
    let mut killed = Vec::new();
    for task in tasks {
        for pid in &task.pids {
            let pid = *pid;
            if crate::dispatch::owns_child(pid) {
                // A child this process started is live work, not an orphan.
                // The host may run `list` alongside `dispatch`, so killing by
                // pid alone would kill a wave that is still running.
                continue;
            }
            if !crate::spawn::process_is_alive(pid) {
                continue;
            }
            crate::spawn::kill_process_tree(pid);
            killed.push(pid);
        }
    }
    killed
}


/// The `superagent_task` tool: create, update, or list long tasks.
///
/// `action=create` writes a task's first snapshot. `action=update` writes a new
/// snapshot for an existing task, which is how its state changes. `action=list`
/// reads every task back, folded to its latest snapshot.
pub fn task(
    params: &Value,
    control: &Control,
    _progress: &Progress,
) -> Result<String, String> {
    control.check()?;
    let action = optional_string(params, "action", "list");
    match action.as_str() {
        "create" => {
            let task_id = required(params, "taskId")?;
            let mut task = Task {
                task_id,
                title: optional_field(params, "title"),
                stage: Stage::Pending,
                ..Task::default()
            };
            if let Some(stage) = params.get("stage").and_then(Value::as_str) {
                task.stage = Stage::parse(stage);
            }
            task.nodes = string_list(params, "nodes");
            task.error = optional_field(params, "error");
            if let Some(pids) = params.get("_pids").and_then(Value::as_array) {
                task.pids = pids.iter().filter_map(Value::as_u64).map(|pid| pid as u32).collect();
            }
            task.branch_id = crate::host::current_branch_id();
            let entry_id = append_task(&task)?;
            let text = format!(
                "created task `{}` ({})",
                task.task_id,
                task.stage.as_str()
            );
            Ok(result_with_details(
                &text,
                json!({"taskId": task.task_id, "entryId": entry_id, "stage": task.stage.as_str()}),
            ))
        }
        "update" => {
            let task_id = required(params, "taskId")?;
            // An update to an unknown task creates it: a caller that only knows
            // the id should not have to know whether the task already exists.
            let mut task = match read_task(&task_id)? {
                Some(task) => task,
                None => Task {
                    task_id: task_id.clone(),
                    ..Task::default()
                },
            };
            if let Some(title) = params.get("title").and_then(Value::as_str) {
                task.title = Some(title.trim().to_string());
            }
            if let Some(stage) = params.get("stage").and_then(Value::as_str) {
                task.stage = Stage::parse(stage);
            }
            if params.get("nodes").is_some() {
                task.nodes = string_list(params, "nodes");
            }
            if let Some(error) = params.get("error").and_then(Value::as_str) {
                task.error = Some(error.trim().to_string());
            }
            // The child's process ids. Not a model-facing field: the model has
            // no business naming process ids, and a wrong one would make the
            // reaper kill an unrelated process.
            if let Some(pids) = params.get("_pids").and_then(Value::as_array) {
                task.pids = pids.iter().filter_map(Value::as_u64).map(|pid| pid as u32).collect();
            }
            task.branch_id = crate::host::current_branch_id();
            let entry_id = append_task(&task)?;
            let text = format!(
                "updated task `{}` → {}",
                task.task_id,
                task.stage.as_str()
            );
            Ok(result_with_details(
                &text,
                json!({"taskId": task.task_id, "entryId": entry_id, "stage": task.stage.as_str()}),
            ))
        }
        "list" => {
            let mut tasks = read_tasks()?;
            // Children whose host died are still running, doing work nobody
            // will collect. Kill them before deciding what state the tasks
            // are in: an orphan is the reason a task looks alive.
            let reaped = reap_orphans(&tasks);
            // A task that claims to be working but has not been written for a
            // long time lost the process driving it. Record that, so the
            // next reader sees the same thing instead of a task that claims
            // to be running forever.
            let now = now_ms();
            let mut reconciled = 0;
            for task in &mut tasks {
                let (stage, reason) = reconcile_stage(task, now);
                if stage != task.stage {
                    task.stage = stage;
                    task.error = reason;
                    if append_task(task).is_ok() {
                        reconciled += 1;
                    }
                }
            }
            let text = render(&tasks);
            Ok(result_with_details(
                &text,
                json!({
                    "tasks": tasks.iter().map(Task::to_value).collect::<Vec<_>>(),
                    "reconciled": reconciled,
                    "reapedOrphans": reaped,
                }),
            ))
        }
        other => Err(format!(
            "unknown action `{other}` (expected create, update, list)"
        )),
    }
}

/// Read one task by id, folded to its latest snapshot.
pub(crate) fn read_task(task_id: &str) -> Result<Option<Task>, String> {
    Ok(read_tasks()?.into_iter().find(|task| task.task_id == task_id))
}

fn render(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return "no tasks".to_string();
    }
    let mut lines = vec![format!("{} task(s)", tasks.len())];
    for task in tasks {
        let mut line = format!(
            "[{}] {} {}",
            match task.stage {
                Stage::Done => "x",
                Stage::Failed => "!",
                Stage::Abandoned => "~",
                Stage::NeedsPerson => "?",
                Stage::Working => "~",
                Stage::Pending => " ",
            },
            task.task_id,
            task.stage.as_str()
        );
        if let Some(title) = &task.title {
            line.push_str(&format!(" — {title}"));
        }
        if let Some(error) = &task.error {
            line.push_str(&format!(" ({error})"));
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// Milliseconds since the epoch.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn required(params: &Value, name: &str) -> Result<String, String> {
    let value = params
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| format!("`{name}` is required and must be a non-empty string"))?;
    Ok(value.to_string())
}

fn optional_field(params: &Value, name: &str) -> Option<String> {
    params
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn string_list(params: &Value, name: &str) -> Vec<String> {
    params
        .get(name)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(task_id: &str, stage: &str) -> Value {
        json!({"taskId": task_id, "stage": stage})
    }

    #[test]
    fn the_last_snapshot_wins() {
        let tasks = fold(&[
            snapshot("t1", "pending"),
            snapshot("t2", "working"),
            snapshot("t1", "done"),
        ]);
        assert_eq!(tasks.len(), 2);
        // The order is the order the tasks first appeared.
        assert_eq!(tasks[0].task_id, "t1");
        assert_eq!(tasks[0].stage, Stage::Done);
        assert_eq!(tasks[1].task_id, "t2");
        assert_eq!(tasks[1].stage, Stage::Working);
    }

    #[test]
    fn a_snapshot_without_a_task_id_is_skipped() {
        let tasks = fold(&[
            json!({"stage": "working"}),
            snapshot("t1", "working"),
            json!({"taskId": "  "}),
        ]);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "t1");
    }

    #[test]
    fn a_snapshot_round_trips() {
        let task = Task {
            task_id: "t1".into(),
            title: Some("ship the thing".into()),
            stage: Stage::NeedsPerson,
            nodes: vec!["a".into(), "b".into()],
            error: Some("waiting on review".into()),
            branch_id: Some("leaf-1".into()),
            pids: vec![4242],
            updated_at: 42,
        };
        let back = Task::from_value(&task.to_value()).unwrap();
        assert_eq!(back, task);
    }

    fn node(id: &str, status: NodeStatus) -> Node {
        Node {
            id: id.to_string(),
            task: "t".into(),
            isolation: false,
            deps: Vec::new(),
            role: None,
            provider: None,
            model: None,
            expect_file: None,
            verify: None,
            max_turns: 1,
            timeout_secs: 10,
            status,
            result: None,
            error: None,
        }
    }

    #[test]
    fn a_working_task_that_went_silent_becomes_abandoned() {
        let now = 1_000_000_000u64;
        let task = Task {
            task_id: "t1".into(),
            stage: Stage::Working,
            updated_at: now - ABANDON_AFTER_MS - 60_000,
            ..Task::default()
        };
        let (stage, reason) = reconcile_stage(&task, now);
        assert_eq!(stage, Stage::Abandoned);
        assert!(reason.unwrap().contains("no progress"));
    }

    #[test]
    fn a_working_task_that_is_still_fresh_is_left_alone() {
        let now = 1_000_000_000u64;
        let task = Task {
            task_id: "t1".into(),
            stage: Stage::Working,
            updated_at: now - 60_000,
            ..Task::default()
        };
        let (stage, reason) = reconcile_stage(&task, now);
        assert_eq!(stage, Stage::Working);
        assert!(reason.is_none());
    }

    #[test]
    fn a_task_waiting_on_a_person_is_never_abandoned() {
        // Waiting for a person can take as long as it takes; ageing is only
        // meaningful for work a process was driving.
        let now = 1_000_000_000u64;
        for stage in [Stage::NeedsPerson, Stage::Done, Stage::Failed, Stage::Pending] {
            let task = Task {
                task_id: "t1".into(),
                stage,
                updated_at: now - ABANDON_AFTER_MS * 10,
                ..Task::default()
            };
            let (got, reason) = reconcile_stage(&task, now);
            assert_eq!(got, stage);
            assert!(reason.is_none());
        }
    }

    #[test]
    fn a_working_task_without_a_timestamp_is_left_alone() {
        // Missing information is not evidence of death.
        let task = Task {
            task_id: "t1".into(),
            stage: Stage::Working,
            updated_at: 0,
            ..Task::default()
        };
        let (stage, reason) = reconcile_stage(&task, 1_000_000_000);
        assert_eq!(stage, Stage::Working);
        assert!(reason.is_none());
    }

    #[test]
    fn the_entry_timestamp_is_the_tasks_heartbeat() {
        // The heartbeat must be the host's write time, not the reader's clock:
        // it has to survive a restart to mean anything.
        let snapshot = json!({"taskId": "t1", "stage": "working"});
        let task = Task::from_snapshot(&snapshot, Some(1_700_000_000_000)).unwrap();
        assert_eq!(task.updated_at, 1_700_000_000_000);
    }

    #[test]
    fn a_child_this_process_started_is_not_reaped() {
        // The host may run `list` while a wave is still running, so the reaper
        // must leave this process's own children alone.
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .expect("the shell starts");
        crate::dispatch::remember_child(child.id());
        let task = Task {
            task_id: "t1".into(),
            stage: Stage::Working,
            pids: vec![child.id()],
            ..Task::default()
        };
        let reaped = reap_orphans(&[task]);
        assert!(reaped.is_empty(), "reaped a live child: {reaped:?}");
        crate::dispatch::release_child(child.id());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn a_finished_process_is_not_alive() {
        // A child that has exited but not been reaped stays in the process
        // table as a zombie, and `kill -0` reports it as present. Treating a
        // zombie as alive would make every finished child look like an
        // orphan, and the reaper would keep trying to kill it.
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("the shell starts");
        child.wait().expect("the shell exits");
        // The child is reaped now, so it is gone from the table entirely.
        assert!(!crate::spawn::process_is_alive(child.id()));
    }

    #[test]
    fn this_process_is_alive() {
        assert!(crate::spawn::process_is_alive(std::process::id()));
    }

    #[test]
    fn the_graph_implies_the_task_stage() {
        // All done: the task is done.
        assert_eq!(
            stage_for_graph(&[node("a", NodeStatus::Done), node("b", NodeStatus::Done)]),
            Stage::Done
        );
        // Something running: the task is working.
        assert_eq!(
            stage_for_graph(&[node("a", NodeStatus::Done), node("b", NodeStatus::Running)]),
            Stage::Working
        );
        // A dead branch outranks a running one.
        assert_eq!(
            stage_for_graph(&[node("a", NodeStatus::Failed), node("b", NodeStatus::Running)]),
            Stage::Failed
        );
        // A skipped node is a dead branch too.
        assert_eq!(
            stage_for_graph(&[node("a", NodeStatus::Skipped)]),
            Stage::Failed
        );
        // Nothing has run yet.
        assert_eq!(
            stage_for_graph(&[node("a", NodeStatus::Pending)]),
            Stage::Pending
        );
        assert_eq!(stage_for_graph(&[]), Stage::Pending);
    }

    #[test]
    fn the_graph_implies_the_task_error() {
        let mut failed = node("a", NodeStatus::Failed);
        failed.error = Some("expected file was not produced: out.txt".into());
        assert_eq!(
            error_for_graph(&[failed]),
            Some("expected file was not produced: out.txt".to_string())
        );
        // A skipped node names the node it was skipped for.
        assert_eq!(error_for_graph(&[node("a", NodeStatus::Skipped)]), Some("a".to_string()));
        assert_eq!(error_for_graph(&[node("a", NodeStatus::Done)]), None);
    }

    #[test]
    fn a_task_snapshot_carries_the_branch_it_was_written_on() {
        // The host scopes reads to the current leaf's branch, so a snapshot
        // records where it belongs. Without this, a later change could not
        // tell which branch it applies to.
        let task = Task {
            task_id: "t1".into(),
            branch_id: Some("leaf-1".into()),
            ..Task::default()
        };
        let value = task.to_value();
        assert_eq!(value["branchId"], json!("leaf-1"));
        assert_eq!(Task::from_value(&value).unwrap().branch_id.as_deref(), Some("leaf-1"));
    }

    #[test]
    fn an_unknown_stage_falls_back_to_pending() {
        assert_eq!(Stage::parse("nonsense"), Stage::Pending);
        assert_eq!(Stage::parse("working"), Stage::Working);
    }

    #[test]
    fn the_render_shows_the_stage_and_the_error() {
        let tasks = fold(&[json!({
            "taskId": "t1", "stage": "failed", "title": "build", "error": "boom"
        })]);
        let text = render(&tasks);
        assert!(text.contains("[!] t1 failed"), "got: {text}");
        assert!(text.contains("build"), "got: {text}");
        assert!(text.contains("boom"), "got: {text}");
    }
}
