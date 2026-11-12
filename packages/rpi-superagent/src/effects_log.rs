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
        /// Which wave of the task this is; part of the queue key.
        #[serde(default)]
        wave: u64,
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
                wave,
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
                wave: *wave,
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
                wave,
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
                wave: *wave,
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
            // The key this row was stored under, so its `done` line matches even
            // when the key scheme has changed since it was written.
            stored_key: Some(line.key.clone()),
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
    // A row that is already overdue keeps how long it has been overdue, rather than
    // being flattened to `now`. The daemon uses that age to decide whether an
    // abandoned row is safe to adopt, and "just became due" is indistinguishable
    // from "a live host is about to run it".
    if due_at_ms >= now_ms {
        std::time::Instant::now() + std::time::Duration::from_millis(due_at_ms - now_ms)
    } else {
        let overdue = now_ms - due_at_ms;
        std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(overdue))
            .unwrap_or_else(std::time::Instant::now)
    }
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
            // The row key, not the task id: a launch's carries its wave, and the
            // `done` line written later is keyed the same way. Mismatched keys are
            // why a finished row used to come back after a restart.
            key: effect.row_key(),
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
        wave: 0,
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
        // The key must be the row key the `queued` line used, not a bare task id:
        // rows are namespaced by the session that queued them.
        log.append_state(Kind::Launch, &launch("T1").row_key(), 1, State::Done, None)
            .unwrap();
        assert!(log.load().is_empty());
    }

    /// A row written before rows were namespaced by session must still retire.
    ///
    /// Its `queued` line carries the bare task key. Recomputing the key when the
    /// row finishes would produce the namespaced form, which matches no line, so
    /// the `done` line would retire nothing and the row would come back on every
    /// load — one zombie row per legacy entry, forever.
    ///
    /// The line is written by hand because every current writer already emits the
    /// namespaced key; a test that used one would not exercise the old format.
    #[test]
    fn a_row_written_under_the_old_bare_key_still_retires() {
        let dir = scratch("legacy-key");
        let path = log_path(&dir, "s1");
        let legacy = serde_json::json!({
            "v": VERSION,
            "kind": "launch",
            "key": "T1#0",
            "state": "queued",
            "seq": 1,
            "attempts": 0,
            "due_at": now_ms(),
            "payload": {"type": "launch", "task_id": "T1", "nodes": ["a"],
                        "max_parallel": 1, "wave": 0},
            "error": null,
            "session": "sess-1"
        });
        std::fs::write(&path, format!("{legacy}\n")).unwrap();

        let mut log = Log::open(&dir, "s1").unwrap();
        let rows = log.load();
        assert_eq!(rows.len(), 1, "the legacy row did not load");
        let row_key = rows[0].key();

        log.append_state(Kind::Launch, &row_key, 1, State::Done, None)
            .unwrap();
        assert!(
            Log::open(&dir, "s1").unwrap().load().is_empty(),
            "a legacy row could not retire: it will come back on every load"
        );
    }

    #[test]
    fn a_cancelled_line_also_retires_the_row() {
        let dir = scratch("cancelled");
        let mut log = Log::open(&dir, "s1").unwrap();
        log.append_queued(&launch("T1"), 1, None, 0, now_ms(), None)
            .unwrap();
        log.append_state(
            Kind::Launch,
            &launch("T1").row_key(),
            1,
            State::Cancelled,
            None,
        )
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
        log.append_state(Kind::Launch, "T1#0", 1, State::Done, None)
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
    fn an_overdue_row_keeps_its_age() {
        // The daemon adopts a row only after it has been due for a while, to avoid
        // racing a live host. If an overdue row were flattened to "due now", an
        // abandoned row would look like one about to be picked up — and never be.
        let now = now_ms();
        let overdue_by = 60_000u64;
        let due = due_instant(now.saturating_sub(overdue_by), now);
        let age = due.elapsed().as_millis() as u64;
        assert!(
            age >= overdue_by - 5_000,
            "an overdue row should read as ~{overdue_by}ms old, got {age}ms"
        );
    }

    #[test]
    fn a_future_row_still_lies_ahead() {
        let now = now_ms();
        let due = due_instant(now + 60_000, now);
        assert!(due > std::time::Instant::now(), "a future row is not yet due");
    }

    #[test]
    fn a_missing_file_loads_as_empty() {
        let dir = scratch("missing");
        assert!(Log::open(&dir, "nothing").unwrap().load().is_empty());
    }
}

/// The binding a session last ran on, persisted beside its effect log.
///
/// Why on disk rather than only in memory: the background pass may run in a
/// different process (the daemon) than the session that queued the wave. An
/// in-process cache dies with the host; the file does not. Written whenever the
/// host reports a binding, read back when a queued row carried none.
///
/// Deliberately separate from the log: the log is append-only queue state, and
/// a binding is a "latest value" fact. Mixing them would make reading one
/// require replaying the other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindingRecord {
    /// Format version; a newer one is ignored rather than misread.
    pub v: u32,
    /// The session this binding belongs to.
    pub session: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
}

/// Where a session's last-known binding is recorded.
pub fn binding_path(effects_root: &Path, session: &str) -> PathBuf {
    effects_root.join(format!("{}.binding.json", sanitize(session)))
}

/// Write the binding for `session`, replacing any previous one.
///
/// Written via a temporary file and renamed, so a reader never sees a half-written
/// record: the daemon may read it at any moment.
pub fn write_binding(
    effects_root: &Path,
    session: &str,
    provider: Option<&str>,
    model: Option<&str>,
    thinking: Option<&str>,
) -> std::io::Result<()> {
    if provider.is_none() && model.is_none() {
        // Nothing worth recording: a binding without a provider/model cannot
        // name the account a worker would run on.
        return Ok(());
    }
    std::fs::create_dir_all(effects_root)?;
    let record = BindingRecord {
        v: VERSION,
        session: session.to_string(),
        provider: provider.map(str::to_string),
        model: model.map(str::to_string),
        thinking: thinking.map(str::to_string),
    };
    let final_path = binding_path(effects_root, session);
    let temp_path = final_path.with_extension("binding.tmp");
    let mut text = serde_json::to_string(&record).map_err(std::io::Error::other)?;
    text.push('\n');
    {
        let mut file = File::create(&temp_path)?;
        file.write_all(text.as_bytes())?;
        file.flush()?;
    }
    std::fs::rename(&temp_path, &final_path)
}

/// Read back the binding recorded for `session`, if any.
///
/// A malformed or newer-versioned file yields `None` rather than an error: this
/// is a best-effort fallback, and failing a wave because a cache file is
/// unreadable would be worse than running unbound (which `run_launch` refuses
/// for nodes that name nothing).
pub fn read_binding(effects_root: &Path, session: &str) -> Option<BindingRecord> {
    let text = std::fs::read_to_string(binding_path(effects_root, session)).ok()?;
    let record: BindingRecord = serde_json::from_str(text.trim()).ok()?;
    if record.v > VERSION || record.session != session {
        return None;
    }
    Some(record)
}

#[cfg(test)]
mod binding_tests {
    use super::*;

    /// A unique directory under the system temp dir, removed by `Dir`'s drop.
    /// `tempfile` is not a dependency of this crate, and pulling one in for
    /// four tests would be the tail wagging the dog.
    struct Dir(std::path::PathBuf);
    impl Dir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rpi-superagent-binding-{}-{name}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Dir(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_binding_round_trips() {
        let dir = Dir::new("round-trip");
        let root = dir.path();
        write_binding(root, "s1", Some("magpie"), Some("m-1"), Some("high")).unwrap();
        let read = read_binding(root, "s1").expect("binding should be there");
        assert_eq!(read.session, "s1");
        assert_eq!(read.provider.as_deref(), Some("magpie"));
        assert_eq!(read.model.as_deref(), Some("m-1"));
        assert_eq!(read.thinking.as_deref(), Some("high"));
    }

    /// A second write replaces the first: the record is a latest-value fact, not
    /// a log. This is what makes a model switch visible to the daemon.
    #[test]
    fn the_newest_binding_wins() {
        let dir = Dir::new("the_newest_binding_wins");
        let root = dir.path();
        write_binding(root, "s1", Some("magpie"), Some("first"), None).unwrap();
        write_binding(root, "s1", Some("magpie"), Some("second"), Some("off")).unwrap();
        let read = read_binding(root, "s1").unwrap();
        assert_eq!(read.model.as_deref(), Some("second"));
        assert_eq!(read.thinking.as_deref(), Some("off"));
    }

    /// Bindings are per session. Handing one session's model to another is the
    /// cross-account mistake this whole mechanism exists to prevent.
    #[test]
    fn bindings_do_not_leak_across_sessions() {
        let dir = Dir::new("bindings_do_not_leak_across_sessions");
        let root = dir.path();
        write_binding(root, "s1", Some("magpie"), Some("m-1"), None).unwrap();
        assert!(read_binding(root, "s2").is_none());
    }

    /// A binding without a provider and model cannot name an account, so it is
    /// not worth recording.
    #[test]
    fn a_binding_without_a_model_is_not_recorded() {
        let dir = Dir::new("a_binding_without_a_model_is_not_recorded");
        let root = dir.path();
        write_binding(root, "s1", None, None, Some("high")).unwrap();
        assert!(read_binding(root, "s1").is_none());
    }

    /// A malformed file is a cache miss, not a failure: refusing a wave because
    /// a cache file is unreadable would be worse than running without it.
    #[test]
    fn a_malformed_binding_file_is_ignored() {
        let dir = Dir::new("a_malformed_binding_file_is_ignored");
        let root = dir.path();
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(binding_path(root, "s1"), b"{not json").unwrap();
        assert!(read_binding(root, "s1").is_none());
    }
}

#[cfg(test)]
mod binding_recovery_tests {
    use super::*;

    /// The daemon recovers a binding the row lost, using what the session wrote
    /// earlier. This is the whole point of the on-disk record: the row itself
    /// arrives empty when the host reported nothing at enqueue time.
    #[test]
    fn a_lost_binding_is_recoverable_from_disk() {
        let dir = std::env::temp_dir().join(format!(
            "rpi-superagent-recover-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A successful capture records the binding.
        write_binding(&dir, "parent", Some("magpie"), Some("m-1"), Some("high")).unwrap();

        // A later row arrives with nothing in it.
        let carried = crate::effects::Binding::default();
        assert!(carried.is_empty());

        // The recovery path finds the earlier record.
        let recovered = read_binding(&dir, "parent").expect("binding should be recoverable");
        assert_eq!(recovered.provider.as_deref(), Some("magpie"));
        assert_eq!(recovered.model.as_deref(), Some("m-1"));
        assert_eq!(recovered.thinking.as_deref(), Some("high"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A model switch is visible to whoever reads the record next.
    ///
    /// This is the scenario the whole mechanism exists for: the settings default
    /// is rate-limited, the user picks another model in the UI, and the worker
    /// must follow the session rather than the settings file.
    #[test]
    fn a_model_switch_replaces_the_recorded_binding() {
        let dir = std::env::temp_dir().join(format!(
            "rpi-superagent-switch-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        write_binding(&dir, "s", Some("magpie"), Some("rate-limited"), Some("off")).unwrap();
        write_binding(&dir, "s", Some("magpie"), Some("healthy"), Some("high")).unwrap();

        let read = read_binding(&dir, "s").unwrap();
        assert_eq!(read.model.as_deref(), Some("healthy"));
        assert_eq!(read.thinking.as_deref(), Some("high"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
