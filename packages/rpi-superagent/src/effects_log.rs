//! Durable storage for the effect queue, as an append-only JSONL log.
//!
//! ## Why a log rather than a database
//!
//! DeepSeek's prototype keeps its effects in SQLite (`src/app/effects.ts`) because
//! it has a dozen tables sharing transactions. This package has **one** table —
//! the rows that still need to run — and no other state to be consistent with, so
//! a database would add a C dependency, a link step and a schema migration for no
//! gain.
//!
//! The log costs nothing that is already here: `serde_json` is a dependency,
//! `supervisor.rs` already reads the session directory, and `task.rs` already
//! folds a list of snapshots into the current state. This module reuses that last
//! idea: **every line is a state change, and the newest line for a key wins.**
//!
//! ## The format
//!
//! One JSON object per line:
//!
//! ```json
//! {"v":1,"kind":"launch","key":"T1","state":"queued","seq":3,"attempts":0,
//!  "due_at":1730000000000,"payload":{...}}
//! {"v":1,"kind":"launch","key":"T1","state":"done","seq":3}
//! ```
//!
//! `state` is `queued`, `done` or `cancelled`. A `done`/`cancelled` line needs no
//! payload — it only retires the row, and folding keeps the last state per key.
//!
//! Lines are appended and flushed, never rewritten in place: an interrupted write
//! can lose the newest line but cannot corrupt the ones before it, and a torn last
//! line is skipped on read rather than failing the load.
//!
//! ## Compaction
//!
//! Appending forever would grow without bound, so the file is rewritten once it
//! passes [`COMPACT_AFTER`] lines: the live rows are written to a temporary file
//! and renamed over the original. The rename is atomic, so a reader either sees
//! the old file or the new one, never a half-written mix.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::effects::{Binding, Effect, Outcome, Row};
use crate::effects::Kind;

/// The format version. A line from a newer version is ignored rather than
/// misread.
pub const VERSION: u32 = 1;

/// Rewrite the log once it holds this many lines.
pub const COMPACT_AFTER: usize = 1_000;

/// The directory holding effect logs, beside the other super-agent state.
const ROOT: &str = ".rpi/superagent-effects";

/// The state of a row as recorded on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Still to run.
    Queued,
    /// Ran to completion.
    Done,
    /// Cancelled by its owner.
    Cancelled,
}

/// One line of the log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Line {
    /// Format version.
    pub v: u32,
    /// Effect kind: `launch`, `extend` or `relay`.
    pub kind: String,
    /// What the row is about (a task id).
    pub key: String,
    /// The row's state after this line.
    pub state: State,
    /// Queue position. Preserved across a payload replacement.
    pub seq: u64,
    /// Consecutive failures so far.
    #[serde(default)]
    pub attempts: u32,
    /// When it may next run, in milliseconds since the epoch.
    #[serde(default)]
    pub due_at: u64,
    /// The effect itself. Absent on a `done`/`cancelled` line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Stored>,
    /// The last failure, so a row can explain itself after a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The session the row belongs to; the graph is keyed by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// The serialisable form of an [`Effect`].
///
/// Kept separate from [`Effect`] so the on-disk shape is explicit and can be
/// versioned, rather than following whatever the enum happens to look like.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Stored {
    Launch {
        task_id: String,
        nodes: Vec<String>,
        max_parallel: usize,
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        model: Option<String>,
        /// The session's thinking level. Defaulted so rows written by an older
        /// version still load (they simply carry no level).
        #[serde(default)]
        thinking: Option<String>,
        /// The graph, so a process that never saw the plan can rebuild it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph: Option<serde_json::Value>,
        /// The `rpi` executable to run workers with, captured from the host.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rpi_exe: Option<String>,
    },
    Extend {
        task_id: String,
        node: serde_json::Value,
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        thinking: Option<String>,
    },
    Relay {
        task_id: String,
        text: String,
    },
}

impl Stored {
    /// The effect this line carries.
    pub fn to_effect(&self) -> Effect {
        match self {
            Stored::Launch {
                task_id,
                nodes,
                max_parallel,
                provider,
                model,
                thinking,
                graph,
                rpi_exe,
            } => Effect::Launch {
                task_id: task_id.clone(),
                nodes: nodes.clone(),
                max_parallel: *max_parallel,
                binding: Binding {
                    provider: provider.clone(),
                    model: model.clone(),
                    thinking: thinking.clone(),
                },
                session: None,
                graph: graph.clone(),
                rpi_exe: rpi_exe.clone(),
            },
            Stored::Extend {
                task_id,
                node,
                provider,
                model,
                thinking,
            } => Effect::Extend {
                task_id: task_id.clone(),
                node: node.clone(),
                binding: Binding {
                    provider: provider.clone(),
                    model: model.clone(),
                    thinking: thinking.clone(),
                },
                session: None,
            },
            Stored::Relay { task_id, text } => Effect::Relay {
                task_id: task_id.clone(),
                text: text.clone(),
            },
        }
    }

    /// The on-disk form of `effect`.
    pub fn from_effect(effect: &Effect) -> Self {
        match effect {
            Effect::Launch {
                task_id,
                nodes,
                max_parallel,
                binding,
                graph,
                rpi_exe,
                ..
            } => Stored::Launch {
                task_id: task_id.clone(),
                nodes: nodes.clone(),
                max_parallel: *max_parallel,
                provider: binding.provider.clone(),
                model: binding.model.clone(),
                thinking: binding.thinking.clone(),
                graph: graph.clone(),
                rpi_exe: rpi_exe.clone(),
            },
            Effect::Extend {
                task_id,
                node,
                binding,
                ..
            } => Stored::Extend {
                task_id: task_id.clone(),
                node: node.clone(),
                provider: binding.provider.clone(),
                model: binding.model.clone(),
                thinking: binding.thinking.clone(),
            },
            Effect::Relay { task_id, text } => Stored::Relay {
                task_id: task_id.clone(),
                text: text.clone(),
            },
        }
    }
}

/// Milliseconds since the epoch, the unit `due_at` uses.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The kind name used on disk.
fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Launch => "launch",
        Kind::Extend => "extend",
        Kind::Relay => "relay",
    }
}

/// Read a kind name back.
///
/// Not called yet: a reader that needs the kind as a value rather than a name
/// will use it, and keeping the inverse next to the mapping is what stops the
/// two drifting apart.
#[allow(dead_code)]
fn parse_kind(name: &str) -> Option<Kind> {
    match name {
        "launch" => Some(Kind::Launch),
        "extend" => Some(Kind::Extend),
        "relay" => Some(Kind::Relay),
        _ => None,
    }
}

/// Where the log for `owner` lives.
///
/// `owner` names the log — the session id when there is one, so two sessions do
/// not share a queue. A name is sanitised to something safe in a path.
pub fn log_path(effects_root: &Path, owner: &str) -> PathBuf {
    effects_root.join(format!("{}.jsonl", sanitize(owner)))
}

/// Where the effects root lives under a project working directory.
pub fn effects_root(cwd: &Path) -> PathBuf {
    cwd.join(ROOT)
}

/// Reduce a name to characters safe in a file name.
///
/// A session id is a UUID in practice, but it comes from the host, so it is not
/// trusted to be one. Everything outside `[A-Za-z0-9_-]` becomes `_`.
///
/// A dot is **not** allowed: `..` would step up a directory and a name of only
/// dots would escape the effects directory entirely. Mapped to `_`, `..` becomes
/// `__`, which is a file name and nothing more.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "default".to_string()
    } else {
        cleaned
    }
}

/// Fold a list of lines into the rows still outstanding.
///
/// Later lines win, which is what makes a log work as state: a `done` line
/// retires the payload queued before it. A row whose payload was never seen
/// (because compaction ran between the two, or the file was truncated) is
/// dropped rather than guessed at.
pub fn fold(lines: &[Line]) -> Vec<Row> {
    let mut order: Vec<String> = Vec::new();
    let mut latest: std::collections::HashMap<String, Line> = std::collections::HashMap::new();
    for line in lines {
        // One row per kind and key, exactly like the in-memory queue.
        let id = format!("{}:{}", line.kind, line.key);
        if !latest.contains_key(&id) {
            order.push(id.clone());
        }
        latest.insert(id, line.clone());
    }
    let now = now_ms();
    let mut out = Vec::new();
    for id in order {
        let Some(line) = latest.get(&id) else {
            continue;
        };
        if line.state != State::Queued {
            continue;
        }
        let Some(payload) = line.payload.as_ref() else {
            continue;
        };
        let mut effect = payload.to_effect();
        let session = line.session.clone();
        match &mut effect {
            Effect::Launch { session: slot, .. } | Effect::Extend { session: slot, .. } => {
                *slot = session;
            }
            Effect::Relay { .. } => {}
        }
        out.push(Row {
            effect,
            seq: line.seq,
            attempts: line.attempts,
            due: due_instant(line.due_at, now),
            error: line.error.clone(),
        });
    }
    out.sort_by_key(|row| row.seq);
    out
}

/// Turn a stored `due_at` into an [`Instant`](std::time::Instant).
///
/// The log stores wall-clock milliseconds so a due time survives a restart; the
/// queue works in `Instant`s so a clock change cannot make a row never due.
/// Anything already past becomes "now".
fn due_instant(due_at_ms: u64, now_ms: u64) -> std::time::Instant {
    let delta_ms = due_at_ms.saturating_sub(now_ms);
    std::time::Instant::now() + std::time::Duration::from_millis(delta_ms)
}

/// The append-only log for one owner.
pub struct Log {
    path: PathBuf,
    lines: usize,
}

impl Log {
    /// Open (or create) the log for `owner`.
    pub fn open(effects_root: &Path, owner: &str) -> std::io::Result<Self> {
        let path = log_path(effects_root, owner);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lines = count_lines(&path).unwrap_or(0);
        Ok(Log { path, lines })
    }

    /// The file this log appends to.
    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read every row still outstanding.
    pub fn load(&self) -> Vec<Row> {
        let Ok(file) = File::open(&self.path) else {
            return Vec::new();
        };
        let reader = BufReader::new(file);
        let mut lines = Vec::new();
        for line in reader.lines() {
            let Ok(text) = line else { continue };
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            // A torn last line — or one from a newer format — is skipped, not
            // fatal: the rest of the log is still the truth.
            let Ok(value) = serde_json::from_str::<Line>(text) else {
                continue;
            };
            if value.v > VERSION {
                continue;
            }
            lines.push(value);
        }
        fold(&lines)
    }

    /// Record that `effect` is queued.
    pub fn append_queued(
        &mut self,
        effect: &Effect,
        seq: u64,
        session: Option<&str>,
        attempts: u32,
        due_at: u64,
        error: Option<&str>,
    ) -> std::io::Result<()> {
        let line = Line {
            v: VERSION,
            kind: kind_name(effect.kind()).to_string(),
            key: effect.key().to_string(),
            state: State::Queued,
            seq,
            attempts,
            due_at,
            payload: Some(Stored::from_effect(effect)),
            error: error.map(str::to_string),
            session: session.map(str::to_string),
        };
        self.append(&line)
    }

    /// Record that `kind`'s row for `key` reached `state`.
    pub fn append_state(
        &mut self,
        kind: Kind,
        key: &str,
        seq: u64,
        state: State,
        error: Option<&str>,
    ) -> std::io::Result<()> {
        let line = Line {
            v: VERSION,
            kind: kind_name(kind).to_string(),
            key: key.to_string(),
            state,
            seq,
            attempts: 0,
            due_at: 0,
            payload: None,
            error: error.map(str::to_string),
            session: None,
        };
        self.append(&line)
    }

    /// Append one line and flush it.
    ///
    /// Flushed rather than merely written: a crash between the two would lose the
    /// line, and the line is what makes the queue durable.
    fn append(&mut self, line: &Line) -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let mut text = serde_json::to_string(line).map_err(std::io::Error::other)?;
        text.push('\n');
        file.write_all(text.as_bytes())?;
        file.flush()?;
        self.lines += 1;
        if self.lines > COMPACT_AFTER {
            self.compact()?;
        }
        Ok(())
    }

    /// Rewrite the file with only the live rows.
    ///
    /// Written to a sibling temporary file and renamed over the original, so a
    /// reader — or a crash — sees one or the other, never a partial file.
    fn compact(&mut self) -> std::io::Result<()> {
        let live = self.load();
        let tmp = self.path.with_extension("jsonl.tmp");
        {
            let mut file = File::create(&tmp)?;
            let mut lines = 0usize;
            for row in &live {
                let line = Line {
                    v: VERSION,
                    kind: kind_name(row.effect.kind()).to_string(),
                    key: row.effect.key().to_string(),
                    state: State::Queued,
                    seq: row.seq,
                    attempts: row.attempts,
                    due_at: now_ms() + row.due.saturating_duration_since(std::time::Instant::now()).as_millis() as u64,
                    payload: Some(Stored::from_effect(&row.effect)),
                    error: row.error.clone(),
                    session: None,
                };
                let mut text = serde_json::to_string(&line).map_err(std::io::Error::other)?;
                text.push('\n');
                file.write_all(text.as_bytes())?;
                lines += 1;
            }
            file.flush()?;
            file.sync_all()?;
            self.lines = lines + 1;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// How many lines the file holds, cheaply.
fn count_lines(path: &Path) -> std::io::Result<usize> {
    let Ok(file) = File::open(path) else {
        return Ok(0);
    };
    let mut count = 0usize;
    for chunk in BufReader::new(file).lines() {
        if chunk.is_err() {
            break;
        }
        count += 1;
    }
    Ok(count)
}

/// Whether a row's outcome should be written as `Done` or left as `Queued`.
///
/// A deferred row stays queued with a new due time; a failed one stays queued
/// with a higher attempt count. Only a success retires it. Split out so the log
/// writer and the queue agree on one rule.
#[allow(dead_code)]
pub fn state_after(outcome: &Result<(), Outcome>) -> State {
    match outcome {
        Ok(()) => State::Done,
        Err(_) => State::Queued,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(task: &str) -> Effect {
        Effect::Launch {
            task_id: task.to_string(),
            nodes: vec!["a".into()],
            max_parallel: 2,
            binding: Binding {
                provider: Some("anthropic".into()),
                model: Some("claude-sonnet-5".into()),
                thinking: None,
            },
            session: Some("sess-1".into()),
        graph: None,
        rpi_exe: None,
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpi-superagent-log-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_queued_row_survives_a_reopen() {
        let dir = scratch("reopen");
        {
            let mut log = Log::open(&dir, "s1").unwrap();
            log.append_queued(&launch("T1"), 1, Some("sess-1"), 0, now_ms(), None)
                .unwrap();
        }
        // A fresh handle is what a restarted process sees.
        let log = Log::open(&dir, "s1").unwrap();
        let rows = log.load();
        assert_eq!(rows.len(), 1);
        match &rows[0].effect {
            Effect::Launch {
                task_id,
                max_parallel,
                binding,
                session,
                ..
            } => {
                assert_eq!(task_id, "T1");
                assert_eq!(*max_parallel, 2);
                assert_eq!(binding.provider.as_deref(), Some("anthropic"));
                // The session has to survive, or a resumed pass would clear the
                // graph it is meant to run.
                assert_eq!(session.as_deref(), Some("sess-1"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn a_done_line_retires_the_row() {
        let dir = scratch("done");
        let mut log = Log::open(&dir, "s1").unwrap();
        log.append_queued(&launch("T1"), 1, None, 0, now_ms(), None)
            .unwrap();
        log.append_state(Kind::Launch, "T1", 1, State::Done, None)
            .unwrap();
        assert!(log.load().is_empty());
    }

    #[test]
    fn a_cancelled_line_also_retires_the_row() {
        let dir = scratch("cancelled");
        let mut log = Log::open(&dir, "s1").unwrap();
        log.append_queued(&launch("T1"), 1, None, 0, now_ms(), None)
            .unwrap();
        log.append_state(Kind::Launch, "T1", 1, State::Cancelled, None)
            .unwrap();
        assert!(log.load().is_empty());
    }

    #[test]
    fn a_queued_line_after_a_done_line_wins() {
        // The log is state, not history: the newest line decides.
        let dir = scratch("requeue");
        let mut log = Log::open(&dir, "s1").unwrap();
        log.append_queued(&launch("T1"), 1, None, 0, now_ms(), None)
            .unwrap();
        log.append_state(Kind::Launch, "T1", 1, State::Done, None)
            .unwrap();
        log.append_queued(&launch("T1"), 2, None, 0, now_ms(), None)
            .unwrap();
        let rows = log.load();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].seq, 2, "the later queue position wins");
    }

    #[test]
    fn a_torn_line_is_skipped_not_fatal() {
        let dir = scratch("torn");
        let mut log = Log::open(&dir, "s1").unwrap();
        log.append_queued(&launch("T1"), 1, None, 0, now_ms(), None)
            .unwrap();
        // Simulate a write cut off mid-line.
        let mut file = OpenOptions::new().append(true).open(log.path()).unwrap();
        file.write_all(br#"{"v":1,"kind":"launch","key":"T2","sta"#).unwrap();
        drop(file);
        let rows = Log::open(&dir, "s1").unwrap().load();
        assert_eq!(rows.len(), 1, "the intact line still loads");
        assert_eq!(rows[0].effect.key(), "T1");
    }

    #[test]
    fn a_newer_format_line_is_ignored() {
        let dir = scratch("future");
        let mut log = Log::open(&dir, "s1").unwrap();
        log.append_queued(&launch("T1"), 1, None, 0, now_ms(), None)
            .unwrap();
        let mut file = OpenOptions::new().append(true).open(log.path()).unwrap();
        writeln!(
            file,
            r#"{{"v":{},"kind":"launch","key":"T9","state":"queued","seq":9}}"#,
            VERSION + 1
        )
        .unwrap();
        drop(file);
        let rows = Log::open(&dir, "s1").unwrap().load();
        assert_eq!(rows.len(), 1, "a future line is not misread");
    }

    #[test]
    fn two_owners_do_not_share_a_log() {
        let dir = scratch("owners");
        Log::open(&dir, "a")
            .unwrap()
            .append_queued(&launch("T1"), 1, None, 0, now_ms(), None)
            .unwrap();
        assert!(Log::open(&dir, "b").unwrap().load().is_empty());
    }

    #[test]
    fn a_path_unfriendly_owner_is_sanitised() {
        let dir = scratch("names");
        let root = effects_root(&dir);
        let path = log_path(&root, "../../etc/passwd");
        // The traversal cannot escape the effects directory: the separators and
        // the dots both became underscores.
        assert!(path.starts_with(&root));
        assert!(!path.to_string_lossy().contains(".."));
        assert_eq!(path.file_name().unwrap(), "______etc_passwd.jsonl");
    }

    #[test]
    fn compaction_keeps_the_live_rows() {
        let dir = scratch("compact");
        let mut log = Log::open(&dir, "s1").unwrap();
        log.append_queued(&launch("live"), 1, None, 0, now_ms(), None)
            .unwrap();
        for i in 0..COMPACT_AFTER {
            log.append_state(Kind::Launch, &format!("old{i}"), 0, State::Done, None)
                .unwrap();
        }
        let rows = Log::open(&dir, "s1").unwrap().load();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].effect.key(), "live");
    }

    #[test]
    fn a_missing_file_loads_as_empty() {
        let dir = scratch("missing");
        assert!(Log::open(&dir, "nothing").unwrap().load().is_empty());
    }
}
