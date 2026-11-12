//! The durable side-effect queue.
//!
//! ## Why this exists
//!
//! A wave that runs inside `superagent_dispatch` blocks the turn that asked for
//! it: the model waits, the person waits, and no second wave can start. The way
//! out is DeepSeek's prototype (`src/app/effects.ts`): record the work that must
//! happen, commit it, and let a scheduler run it afterwards.
//!
//! This module is the queue and the scheduler, without the Ticket domain that
//! DeepSeek's version serves. It carries the parts that make that queue correct:
//!
//! - **One row per kind and key.** Enqueuing the same key again replaces its
//!   payload and makes it due at once, keeping its queue position. A node that
//!   is dispatched twice does not queue twice.
//! - **Exponential backoff.** A failing effect waits `retry[0]`, then doubles up
//!   to `retry[1]`. Only a success clears the attempt count.
//! - **`Later` defers without failing.** A wave that cannot start yet (all
//!   worker slots taken) reschedules at the initial delay and does **not**
//!   increment `attempts`, so waiting is never mistaken for breaking.
//! - **Ordered kinds.** A relay or notice is delivered in the order it was
//!   queued; a failed one holds back the later ones of its kind.
//! - **Per-kind limits.** How many of a kind may run at once, re-read on every
//!   dispatch, so a limit can change without a restart.
//!
//! ## What it does not have, and why
//!
//! DeepSeek keeps its rows in SQLite inside command transactions. This package
//! has no database, and its state already lives in the session branch (see
//! [`crate::task`]). The rows here are therefore **in memory**, which means a
//! queue that is waiting when the host dies is lost.
//!
//! That is a deliberate first step, not an oversight: the node results it
//! schedules are still recorded on the long task, so a lost queue costs a retry,
//! not the record of what happened. Durable rows are the next step, and the
//! shape of [`Row`] is what a persisted version would store.

// The public surface here is the queue's contract: `Later`, the kind table and
// the inspection methods are used by the scheduler, the tools and the tests at
// different times, so an unused-at-this-instant item is not dead code.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Consecutive failures before a kind is reported as broken.
pub const FAILURES_BEFORE_ERROR: u32 = 3;

/// How long a continuation waits before it may start.
///
/// A wave's workers exit as its last act, and each holds its session file until
/// then. A continuation queued in the same breath therefore fails with "already
/// open in another rpi process" — a scheduling race that looks like a session bug.
/// Two seconds is longer than a process needs to exit and shorter than anyone
/// watching would notice.
pub const SESSION_RELEASE_GRACE: Duration = Duration::from_secs(2);

/// Signals that an effect cannot run **now** — all worker slots are taken, or
/// processing is paused. The row is rescheduled at the kind's initial retry
/// delay and its attempt count is left alone: waiting is not failing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Later(pub String);

impl Later {
    pub fn new(reason: impl Into<String>) -> Self {
        Later(reason.into())
    }
}

/// The provider/model a queued wave must run its children on.
///
/// Captured from the asking session at enqueue time, because the background pass
/// that later runs the wave has no invocation handle and cannot ask the host.
/// Empty means "let the child resolve its own default" — the old behaviour, and
/// the right one only when the caller named a model on every node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Binding {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// The session's thinking level at enqueue time (`off` … `max`).
    ///
    /// Same reason as provider/model: the background pass cannot ask the host,
    /// so a level not captured here is lost and the worker thinks at the
    /// settings default.
    pub thinking: Option<String>,
}

impl Binding {
    /// Read the binding out of a tool call's `params`, at enqueue time.
    ///
    /// Called from the tool thread, which is the last moment the host's handle is
    /// available. A failed lookup falls back to the session's last known binding
    /// (see `host::parent_selection`), so a transient host failure does not
    /// permanently lose the binding the row was queued with.
    pub fn from_params(params: &serde_json::Value) -> Self {
        let selection = crate::host::parent_selection(params);
        let binding = Binding {
            provider: selection.provider,
            model: selection.model,
            thinking: selection.thinking,
        };
        // Persist it beside the effect log so the daemon — which has no
        // invocation handle and may be a different process entirely — can
        // recover it if the row goes out with an empty binding.
        if !binding.is_empty() {
            if let Some(session) = crate::session_of(params) {
                if let Ok(cwd) = std::env::current_dir() {
                    let root = crate::effects_log::effects_root(&cwd);
                    let _ = crate::effects_log::write_binding(
                        &root,
                        &session,
                        binding.provider.as_deref(),
                        binding.model.as_deref(),
                        binding.thinking.as_deref(),
                    );
                }
            }
        }
        binding
    }

    /// Whether anything was captured.
    pub fn is_empty(&self) -> bool {
        self.provider.is_none() && self.model.is_none() && self.thinking.is_none()
    }
}

/// What an effect of each kind carries.
///
/// The key names the subject, so the same subject enqueued twice coalesces
/// rather than piling up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Run one ready wave of a task's graph.
    ///
    /// Key: the task id. Payload: the nodes that were ready when the wave was
    /// requested, so a row does not silently run work the caller never saw.
    ///
    /// `binding` is the provider/model the **asking session** runs on, captured
    /// when the row was enqueued. It has to be captured then: the background
    /// pass has no tool invocation handle, so it cannot ask the host which model
    /// the session uses. Without it the child would resolve the *default*
    /// provider, which is a different model and may not even be authenticated.
    Launch {
        task_id: String,
        /// The ready nodes this wave covers, by id.
        nodes: Vec<String>,
        max_parallel: usize,
        binding: Binding,
        /// The session that asked for this wave.
        ///
        /// Required, not optional: the graph is keyed by session, and a pass
        /// running with `None` would switch the shared state to "no session" and
        /// **clear the very plan it was queued for**. Carrying the id is what
        /// keeps the background wave pointed at the same graph.
        session: Option<String>,
        /// The whole graph, as [`crate::graph::Graph::to_details`] writes it.
        ///
        /// Carried because the graph otherwise lives only in the asking host's
        /// memory. A scheduler in another process — the daemon, or the same host
        /// after a restart — has no way to ask for it, so a wave it could read
        /// but not rebuild was a wave it could not run at all.
        graph: Option<serde_json::Value>,
        /// The `rpi` executable to run node workers with.
        ///
        /// Also captured, for the same reason: `std::env::current_exe()` inside
        /// the daemon names *the daemon*, which would make it spawn itself as a
        /// worker. The host knows the right path; the daemon does not.
        rpi_exe: Option<String>,
        /// Which wave of the task this is, counting from zero.
        ///
        /// Part of the queue key, not decoration. A wave that queues its own
        /// continuation would otherwise enqueue under the same key as the row it
        /// is running inside — `enqueue` would replace that row, and the queue's
        /// "this row finished" write would then remove it. The plan would stop
        /// after one wave, with nothing logged to say why.
        wave: u64,
    },
    /// Add a node to a running task and run the wave it becomes ready in.
    ///
    /// Key: the task id. The payload is the node as the caller wrote it, so the
    /// queued work survives a `plan` that replaces the graph meanwhile.
    Extend {
        task_id: String,
        node: serde_json::Value,
        /// See [`Effect::Launch`]: captured at enqueue time for the same reason.
        binding: Binding,
        /// See [`Effect::Launch`]: the graph is keyed by session.
        session: Option<String>,
    },
    /// Hand a finished wave's outcome back to the session that asked for it.
    ///
    /// Key: the task id. This is DeepSeek's `relay`: the report is steered into
    /// the main session's turn rather than returned to a caller that has gone.
    Relay { task_id: String, text: String },
}

impl Effect {
    /// The key this effect coalesces on.
    ///
    /// A launch is keyed per wave so its continuation does not collide with the
    /// row currently running; a relay and an extend are keyed per task.
    pub fn key(&self) -> &str {
        match self {
            Effect::Launch { task_id, .. } => task_id,
            Effect::Extend { task_id, .. } | Effect::Relay { task_id, .. } => task_id,
        }
    }

    /// The key as the queue stores it: kind plus subject, with the wave number for
    /// a launch.
    ///
    /// `key()` stays the human-facing subject (a task id, which is what a caller
    /// names); this is what decides coalescing.
    pub fn row_key(&self) -> String {
        // Namespaced by the session that queued it.
        //
        // All main sessions of a project share one queue (they share
        // `RPI_CODING_AGENT_DIR`, and the queue file is keyed by that). Task ids,
        // however, are chosen by the model in each session, so two sessions can
        // both call their task `T1` — and a row keyed by task id alone made the
        // second launch **replace** the first. That session's work then never
        // ran, while its task still read `working`.
        //
        // The session is the right scope: it is what makes two `T1`s different
        // jobs rather than one job submitted twice. A row with no session (a
        // legacy row, or a one-shot run) keeps the old bare key.
        let scope = self.session().map(|id| format!("{id}/")).unwrap_or_default();
        match self {
            Effect::Launch { task_id, wave, .. } => format!("{scope}{task_id}#{wave}"),
            other => format!("{scope}{}", other.key()),
        }
    }

    /// The session that queued this effect, when it names one.
    fn session(&self) -> Option<&str> {
        match self {
            Effect::Launch { session, .. } | Effect::Extend { session, .. } => session.as_deref(),
            // A relay goes back to the session its task belongs to, which the
            // row cannot know; keeping it unscoped means a relay never displaces
            // another session's relay.
            Effect::Relay { .. } => None,
        }
    }

    /// Which handler runs it.
    pub fn kind(&self) -> Kind {
        match self {
            Effect::Launch { .. } => Kind::Launch,
            Effect::Extend { .. } => Kind::Extend,
            Effect::Relay { .. } => Kind::Relay,
        }
    }
}

/// The kinds of effect, each with its own handler, limit and retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Launch,
    Extend,
    Relay,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Launch => "launch",
            Kind::Extend => "extend",
            Kind::Relay => "relay",
        }
    }

    /// Every kind, in the order a dispatch considers them.
    pub const ALL: &'static [Kind] = &[Kind::Extend, Kind::Launch, Kind::Relay];

    /// How many of this kind may run at once.
    ///
    /// A wave is CPU-, process- and provider-bound, so two at once is already
    /// generous; a relay only writes into one session and must not interleave
    /// with itself, which `ordered` enforces on top of this.
    pub fn limit(self) -> usize {
        match self {
            Kind::Launch => 2,
            Kind::Extend => 1,
            Kind::Relay => 1,
        }
    }

    /// Initial and maximum backoff, in the same shape as DeepSeek's table.
    pub fn retry(self) -> (Duration, Duration) {
        match self {
            // A launch retries like a fresh attempt: a provider hiccup should not
            // cost the wave.
            Kind::Launch => (Duration::from_secs(5), Duration::from_secs(300)),
            Kind::Extend => (Duration::from_secs(5), Duration::from_secs(300)),
            // A relay only fails when the host cannot take a message.
            Kind::Relay => (Duration::from_secs(2), Duration::from_secs(60)),
        }
    }

    /// Whether the kind delivers in queue order and holds later rows behind a
    /// failed one.
    pub fn ordered(self) -> bool {
        matches!(self, Kind::Relay)
    }
}

/// One queued effect.
///
/// `seq` preserves the queue position across a payload replacement, so a
/// re-enqueued subject keeps its place; `due` is when it may run next.
#[derive(Debug, Clone)]
pub struct Row {
    pub effect: Effect,
    pub seq: u64,
    pub attempts: u32,
    pub due: Instant,
    /// The last failure, kept so the row can explain itself.
    pub error: Option<String>,
    /// The key this row was stored under, when it came from the log.
    ///
    /// Rows are namespaced by session now, but a log written before that change
    /// holds bare task keys. Recomputing `row_key()` for such a row would not
    /// match the `queued` line it was loaded from, so its `done` line would retire
    /// nothing and the row would come back on every load — forever. Carrying the
    /// key it was read under is what lets an old row finish normally.
    ///
    /// `None` for a row created in this process: its key is `row_key()`.
    pub stored_key: Option<String>,
}

impl Row {
    /// The key this row is stored under.
    ///
    /// The log's key when it came from disk, `row_key()` for a fresh row — so a
    /// row keeps matching the line that created it even across the session
    /// namespacing change.
    pub fn key(&self) -> String {
        self.stored_key
            .clone()
            .unwrap_or_else(|| self.effect.row_key())
    }

    fn new(effect: Effect, seq: u64, now: Instant) -> Self {
        Row {
            stored_key: None,
            effect,
            seq,
            attempts: 0,
            due: now,
            error: None,
        }
    }
}

/// The queue. Cloneable and shared; every method takes the lock briefly.
#[derive(Clone, Default)]
pub struct Effects {
    rows: Arc<Mutex<Vec<Row>>>,
    seq: Arc<Mutex<u64>>,
    /// Kinds whose most recent run failed, and how many times in a row.
    broken: Arc<Mutex<HashMap<Kind, u32>>>,
    /// The keys running right now, so one subject never runs twice at once.
    running: Arc<Mutex<Vec<(Kind, String)>>>,
    /// Where rows are recorded, when this queue is durable.
    ///
    /// `None` for a queue that only lives in memory, which is what the tests use
    /// and what a host without a working directory falls back to. With a log,
    /// every change is appended so a restarted process resumes the queue.
    log: Option<Arc<Mutex<crate::effects_log::Log>>>,
}

impl Effects {
    pub fn new() -> Self {
        Self::default()
    }

    /// A queue that records every change to `log` and starts from what it holds.
    ///
    /// The rows already in the log are loaded immediately, so a restarted
    /// process picks up exactly where the last one stopped — including the
    /// attempt counts, which is what keeps a retry budget across a restart.
    pub fn durable(log: crate::effects_log::Log) -> Self {
        let rows = log.load();
        let seq = rows.iter().map(|row| row.seq).max().unwrap_or(0);
        Effects {
            rows: Arc::new(Mutex::new(rows)),
            seq: Arc::new(Mutex::new(seq)),
            broken: Arc::default(),
            running: Arc::default(),
            log: Some(Arc::new(Mutex::new(log))),
        }
    }

    /// Whether every change is being recorded.
    pub fn is_durable(&self) -> bool {
        self.log.is_some()
    }

    /// Record a change of `effect`'s row, best effort.
    ///
    /// A log write that fails does not fail the queue: the work still runs, it
    /// just will not survive a restart. Losing durability is better than
    /// dropping the work, and the failure is reported on stderr so it is not
    /// silent.
    fn record(&self, effect: &Effect, seq: u64, attempts: u32, error: Option<&str>) {
        let Some(log) = self.log.as_ref() else {
            return;
        };
        let session = match effect {
            Effect::Launch { session, .. } | Effect::Extend { session, .. } => session.clone(),
            Effect::Relay { .. } => None,
        };
        // The due time is what makes a retry budget survive a restart, so it is
        // read from the row rather than assumed to be "now".
        let due_at = {
            let rows = self.rows.lock().unwrap_or_else(|error| error.into_inner());
            rows.iter()
                .find(|row| {
                    row.effect.kind() == effect.kind() && row.effect.row_key() == effect.row_key()
                })
                .map(|row| {
                    let wait = row.due.saturating_duration_since(Instant::now());
                    crate::effects_log::now_ms() + wait.as_millis() as u64
                })
                .unwrap_or_else(crate::effects_log::now_ms)
        };
        let mut guard = log.lock().unwrap_or_else(|error| error.into_inner());
        if let Err(write_error) = guard.append_queued(
            effect,
            seq,
            session.as_deref(),
            attempts,
            due_at,
            error,
        ) {
            eprintln!("rpi-superagent: could not record an effect: {write_error}");
        }
    }

    /// Record that `kind`'s row for `key` reached `state`.
    fn record_state(&self, kind: Kind, key: &str, seq: u64, state: crate::effects_log::State) {
        let Some(log) = self.log.as_ref() else {
            return;
        };
        let mut guard = log.lock().unwrap_or_else(|error| error.into_inner());
        if let Err(error) = guard.append_state(kind, key, seq, state, None) {
            eprintln!("rpi-superagent: could not record an effect state: {error}");
        }
    }

    /// Queue an effect, replacing any waiting row of the same kind and key.
    ///
    /// The replacement keeps the original queue position and becomes due at
    /// once — the same rule DeepSeek states: *"Enqueuing the same waiting key
    /// replaces its payload and makes it due immediately without changing queue
    /// order."*
    pub fn enqueue(&self, effect: Effect) {
        self.enqueue_due(effect, Instant::now());
    }

    /// Queue an effect, not runnable before `due`.
    ///
    /// Used for a wave that continues another: its workers still hold their
    /// sessions while the previous wave's children exit, so starting now would
    /// collide on the session file.
    pub fn enqueue_due(&self, effect: Effect, due: Instant) {
        let now = due;
        let kind = effect.kind();
        let key = effect.row_key();
        let seq;
        {
            let mut rows = self.rows.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(index) = rows
                .iter()
                .position(|row| row.effect.kind() == kind && row.effect.row_key() == key)
            {
                // Replacing a waiting row keeps its queue position.
                seq = rows[index].seq;
                rows[index].effect = effect.clone();
                rows[index].attempts = 0;
                rows[index].error = None;
                rows[index].due = now;
                drop(rows);
                self.record(&effect, seq, 0, None);
                return;
            }
            let mut counter = self.seq.lock().unwrap_or_else(|e| e.into_inner());
            *counter += 1;
            seq = *counter;
            rows.push(Row::new(effect.clone(), seq, now));
        }
        self.record(&effect, seq, 0, None);
    }

    /// Every row, in queue order. For inspection and tests.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = self.rows.lock().unwrap_or_else(|e| e.into_inner()).clone();
        rows.sort_by_key(|row| row.seq);
        rows
    }

    /// Whether a row with this task id is waiting.
    ///
    /// Matched on the task, not the row key: a launch's row key carries its wave
    /// number, and a caller asking "is task T1 still queued" should not have to
    /// know which wave it is on.
    pub fn pending(&self, kind: Kind, key: &str) -> bool {
        self.rows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|row| row.effect.kind() == kind && row.effect.key() == key)
    }

    /// Drop every waiting row a task has of this kind. A row already running
    /// finishes normally.
    pub fn cancel(&self, kind: Kind, task_id: &str) {
        // Matched on the effect's own `key()` — the bare task id a caller names —
        // rather than on the row key. Rows are namespaced by session now, and a
        // caller asking to cancel `T1` should not have to know which session's
        // queue it landed in.
        let seq = {
            let mut rows = self.rows.lock().unwrap_or_else(|e| e.into_inner());
            let found = rows
                .iter()
                .find(|row| row.effect.kind() == kind && row.effect.key() == task_id)
                .map(|row| (row.seq, row.effect.row_key()));
            rows.retain(|row| !(row.effect.kind() == kind && row.effect.key() == task_id));
            found
        };
        if let Some((seq, row_key)) = seq {
            // The log's `done`/`cancelled` line must carry the same key the
            // `queued` line used, or it retires nothing and the row reappears.
            self.record_state(kind, &row_key, seq, crate::effects_log::State::Cancelled);
        }
    }

    /// Whether a kind is reported as broken (its last `FAILURES_BEFORE_ERROR`
    /// runs all failed).
    pub fn broken(&self, kind: Kind) -> bool {
        self.broken
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&kind)
            .is_some_and(|count| *count >= FAILURES_BEFORE_ERROR)
    }

    /// Run every row that is due and has room under its kind's limit.
    ///
    /// `run` is called once per row and answers `Ok` when the effect is done,
    /// `Err(Later)` when it could not start yet, or `Err(Message)` when it
    /// failed. The row is removed on success, deferred on `Later`, and backed
    /// off on failure.
    ///
    /// Returns how many rows ran. Called by [`crate::passes`] on every tick and
    /// by the tools that enqueue, so a fresh effect does not wait for a tick.
    pub fn dispatch(&self, mut run: impl FnMut(&Effect) -> Result<(), Outcome>) -> usize {
        let now = Instant::now();
        // Snapshot what may run, then run it outside the lock: a handler calls
        // back in (`enqueue`, `pending`) and must not deadlock against itself.
        let ready: Vec<Row> = {
            let rows = self.rows.lock().unwrap_or_else(|e| e.into_inner());
            let running = self.running.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let mut taken: HashMap<Kind, usize> = HashMap::new();
            for (kind, _) in &running {
                *taken.entry(*kind).or_default() += 1;
            }
            let mut out = Vec::new();
            // Queue order, so an ordered kind sees its oldest row first.
            let mut ordered: Vec<&Row> = rows.iter().collect();
            ordered.sort_by_key(|row| row.seq);
            for row in ordered {
                let kind = row.effect.kind();
                let count = taken.entry(kind).or_default();
                if *count >= kind.limit() {
                    continue;
                }
                if row.due > now {
                    continue;
                }
                if kind.ordered() && *count > 0 {
                    // An ordered kind never runs twice at once.
                    continue;
                }
                if running
                    .iter()
                    .any(|(k, key)| *k == kind && *key == row.key())
                {
                    continue;
                }
                *count += 1;
                out.push(row.clone());
            }
            out
        };

        let mut ran = 0;
        for row in ready {
            let kind = row.effect.kind();
            let key = row.key();
            self.running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((kind, key.clone()));
            let outcome = run(&row.effect);
            self.running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(k, k2)| !(*k == kind && *k2 == key));
            self.settle(kind, &key, row.seq, outcome);
            ran += 1;
        }
        ran
    }

    /// Apply a row's outcome: remove it, defer it, or back it off.
    fn settle(&self, kind: Kind, key: &str, seq: u64, outcome: Result<(), Outcome>) {
        // Normalised once, so the two matches below cannot fight over ownership.
        let outcome = match outcome {
            Ok(()) => Ok(()),
            Err(Outcome::Later(reason)) => Err(Outcome::Later(reason)),
            Err(Outcome::Failed(message)) => Err(Outcome::Failed(message)),
        };
        let now = Instant::now();
        {
            let mut rows = self.rows.lock().unwrap_or_else(|e| e.into_inner());
            // `key` is the row key (a launch's carries its wave), which is what
            // `dispatch` passed in.
            let Some(index) = rows.iter().position(|row| {
                row.effect.kind() == kind && row.effect.row_key() == key && row.seq == seq
            }) else {
                // The row was cancelled while it ran, or replaced. Its result is
                // for a payload that is no longer queued, so drop it.
                return;
            };
            match &outcome {
                Ok(()) => {
                    rows.remove(index);
                }
                Err(Outcome::Later(_reason)) => {
                    // Deferred, not failed: no attempt is counted.
                    let (initial, _) = kind.retry();
                    rows[index].due = now + initial;
                }
                Err(Outcome::Failed(message)) => {
                    let (initial, maximum) = kind.retry();
                    let row = &mut rows[index];
                    row.attempts = row.attempts.saturating_add(1);
                    let shift = row.attempts.min(16);
                    let delay = initial.saturating_mul(1u32 << shift).min(maximum);
                    row.due = now + delay;
                    row.error = Some(message.clone());
                }
            }
        }
        // Bookkeeping outside the rows lock, and recorded so the log matches
        // what the queue now holds.
        match &outcome {
            Ok(()) => {
                self.broken
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&kind);
                self.record_state(kind, key, seq, crate::effects_log::State::Done);
            }
            Err(Outcome::Later(_)) => {
                // A deferral is not a state change worth a line: the row is still
                // queued with the same attempts, and the due time is recomputed
                // from the retry policy on the next load.
            }
            Err(Outcome::Failed(message)) => {
                self.broken
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(kind)
                    .and_modify(|count| *count = count.saturating_add(1))
                    .or_insert(1);
                // Looked up by the **row** key, the same one `settle` was given
                // and the same one the row is stored under. Matching on
                // `effect.key()` (the bare task id) found nothing whenever a
                // session was named or the wave was not 0 — so the log recorded a
                // constant `attempts` instead of the real count.
                let found = self
                    .rows
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .find(|row| row.effect.kind() == kind && row.key() == key)
                    .map(|row| (row.attempts, row.effect.clone()));
                let (attempts, effect) = match found {
                    Some(pair) => pair,
                    // The row was removed between the two locks (cancelled while
                    // its failure was being recorded). Nothing to record against.
                    None => return,
                };
                self.record(&effect, seq, attempts, Some(message));
            }
        }
    }

    /// When the next row becomes due, if any is waiting.
    ///
    /// Lets a scheduler sleep exactly as long as it can instead of polling.
    pub fn next_due_in(&self) -> Option<Duration> {
        let now = Instant::now();
        self.rows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|row| row.due.saturating_duration_since(now))
            .min()
    }

    /// Whether anything is waiting.
    pub fn is_empty(&self) -> bool {
        self.rows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

/// How a single row's run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Cannot run now; try again after the initial delay without counting a
    /// failure.
    Later(String),
    /// Ran and failed; back off and count an attempt.
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(task: &str) -> Effect {
        Effect::Launch {
            task_id: task.to_string(),
            nodes: vec!["a".into()],
            max_parallel: 1,
            binding: Binding::default(),
            session: None,
            graph: None,
            rpi_exe: None,
            wave: 0,
        }
    }

    #[test]
    fn a_successful_row_leaves_the_queue() {
        let queue = Effects::new();
        queue.enqueue(launch("t1"));
        assert!(queue.pending(Kind::Launch, "t1"));
        assert_eq!(queue.dispatch(|_| Ok(())), 1);
        assert!(!queue.pending(Kind::Launch, "t1"));
        assert!(queue.is_empty());
    }

    #[test]
    fn enqueuing_the_same_key_keeps_one_row_and_its_position() {
        let queue = Effects::new();
        queue.enqueue(launch("t1"));
        queue.enqueue(Effect::Relay {
            task_id: "t2".into(),
            text: "x".into(),
        });
        // Re-enqueue t1: it must stay ahead of t2, not move to the back.
        queue.enqueue(Effect::Launch {
            task_id: "t1".into(),
            nodes: vec!["b".into()],
            max_parallel: 2,
            binding: Binding::default(),
            session: None,
            graph: None,
            rpi_exe: None,
            wave: 0,
        });
        let rows = queue.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].effect.key(), "t1");
        // The payload is the new one.
        match &rows[0].effect {
            Effect::Launch { nodes, max_parallel, .. } => {
                assert_eq!(nodes, &vec!["b".to_string()]);
                assert_eq!(*max_parallel, 2);
            }
            other => panic!("unexpected effect: {other:?}"),
        }
    }

    #[test]
    fn later_defers_without_counting_a_failure() {
        let queue = Effects::new();
        queue.enqueue(launch("t1"));
        assert_eq!(
            queue.dispatch(|_| Err(Outcome::Later("no slot".into()))),
            1
        );
        let rows = queue.rows();
        assert_eq!(rows.len(), 1, "a deferred row stays queued");
        assert_eq!(rows[0].attempts, 0, "deferring is not failing");
        assert!(!queue.broken(Kind::Launch));
    }

    #[test]
    fn a_failure_backs_off_and_counts_attempts() {
        let queue = Effects::new();
        queue.enqueue(launch("t1"));
        queue.dispatch(|_| Err(Outcome::Failed("boom".into())));
        let rows = queue.rows();
        assert_eq!(rows[0].attempts, 1);
        assert_eq!(rows[0].error.as_deref(), Some("boom"));
        // Not due yet on the next tick.
        assert_eq!(queue.dispatch(|_| Ok(())), 0);
    }

    #[test]
    fn repeated_failures_mark_the_kind_broken() {
        let queue = Effects::new();
        queue.enqueue(launch("t1"));
        for _ in 0..FAILURES_BEFORE_ERROR {
            // Force it due so each attempt counts.
            {
                let mut rows = queue.rows.lock().unwrap_or_else(|e| e.into_inner());
                rows[0].due = Instant::now();
            }
            queue.dispatch(|_| Err(Outcome::Failed("boom".into())));
        }
        assert!(queue.broken(Kind::Launch));
        // A success clears it.
        {
            let mut rows = queue.rows.lock().unwrap_or_else(|e| e.into_inner());
            rows[0].due = Instant::now();
        }
        queue.dispatch(|_| Ok(()));
        assert!(!queue.broken(Kind::Launch));
    }

    /// Two main sessions share one queue, and each picks its own task ids.
    ///
    /// Nothing stops two of them from both calling their task `T1` — and when
    /// they do, a row keyed only by task id makes the second launch **replace**
    /// the first. One session's work then silently never runs, while the person
    /// sees a task marked `working` that no child is doing.
    #[test]
    fn two_sessions_with_the_same_task_id_do_not_replace_each_other() {
        let queue = Effects::new();
        let launch = |session: &str, node: &str| Effect::Launch {
            task_id: "T1".into(),
            nodes: vec![node.into()],
            max_parallel: 1,
            binding: Binding::default(),
            session: Some(session.into()),
            graph: None,
            rpi_exe: None,
            wave: 0,
        };
        queue.enqueue(launch("session-A", "a"));
        queue.enqueue(launch("session-B", "b"));
        assert_eq!(
            queue.rows().len(),
            2,
            "one session's launch replaced the other's"
        );
    }

    /// A finished row must actually leave the queue.
    ///
    /// `dispatch` passes the **row** key (`sess/T1#0`) into `settle`, but the
    /// bookkeeping afterwards looked the row up by `effect.key()` (bare `T1`).
    /// Those differ whenever a session is named or a wave is not 0, so the lookup
    /// found nothing, `attempts` fell back to a constant, and the failure was
    /// recorded under the wrong key. A row whose outcome cannot be matched is
    /// silently dropped — the work is done and the queue still holds it.
    #[test]
    fn a_settled_failure_is_recorded_under_the_row_key() {
        let dir = std::env::temp_dir().join(format!(
            "rpi-superagent-settle-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = crate::effects_log::Log::open(&dir, "owner").unwrap();
        let queue = Effects::durable(log);
        queue.enqueue(Effect::Launch {
            task_id: "T1".into(),
            nodes: vec!["a".into()],
            max_parallel: 1,
            binding: Binding::default(),
            session: Some("sess-A".into()),
            graph: None,
            rpi_exe: None,
            wave: 3,
        });
        // Fails once: the row must stay queued with attempts = 1.
        queue.dispatch(|_| Err(Outcome::Failed("boom".into())));
        let rows = queue.rows();
        assert_eq!(rows.len(), 1, "the failed row vanished: {rows:?}");
        assert_eq!(
            rows[0].attempts, 1,
            "the attempt was not counted on the row it belongs to"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The logged retry count must be the row's real count.
    ///
    /// The bookkeeping looked the row up by bare task id while the row was stored
    /// under `sess/T1#wave`, so the lookup missed and a constant was logged. A
    /// person reading the log then sees "attempt 1" forever, on a row that has
    /// been retried six times.
    #[test]
    fn the_log_records_the_rows_real_attempt_count() {
        let dir = std::env::temp_dir().join(format!(
            "rpi-superagent-attempts-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = crate::effects_log::Log::open(&dir, "owner").unwrap();
        let queue = Effects::durable(log);
        queue.enqueue(Effect::Launch {
            task_id: "T1".into(),
            nodes: vec!["a".into()],
            max_parallel: 1,
            binding: Binding::default(),
            session: Some("sess-A".into()),
            graph: None,
            rpi_exe: None,
            wave: 2,
        });
        queue.dispatch(|_| Err(Outcome::Failed("boom".into())));
        // Make it due again so the second attempt actually runs.
        {
            let mut rows = queue.rows.lock().unwrap_or_else(|e| e.into_inner());
            rows[0].due = Instant::now();
        }
        queue.dispatch(|_| Err(Outcome::Failed("boom".into())));

        let reloaded = crate::effects_log::Log::open(&dir, "owner").unwrap().load();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(
            reloaded[0].attempts, 2,
            "the log recorded the wrong attempt count for the row"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_limited_kind_runs_only_its_limit_at_once() {
        let queue = Effects::new();
        // Two launches share the limit of two; a third waits.
        for id in ["t1", "t2", "t3"] {
            queue.enqueue(Effect::Launch {
                task_id: id.into(),
                nodes: vec!["a".into()],
                max_parallel: 1,
                binding: Binding::default(),
                session: None,
                graph: None,
                rpi_exe: None,
                wave: 0,
            });
        }
        let mut seen = Vec::new();
        let ran = queue.dispatch(|effect| {
            seen.push(effect.key().to_string());
            Err(Outcome::Later("hold".into()))
        });
        assert_eq!(ran, Kind::Launch.limit());
        assert_eq!(seen.len(), Kind::Launch.limit());
    }

    #[test]
    fn cancelling_a_waiting_row_removes_it() {
        let queue = Effects::new();
        queue.enqueue(launch("t1"));
        queue.cancel(Kind::Launch, "t1");
        assert!(queue.is_empty());
    }

    #[test]
    fn next_due_reports_the_soonest_row() {
        let queue = Effects::new();
        assert_eq!(queue.next_due_in(), None);
        queue.enqueue(launch("t1"));
        assert!(queue.next_due_in().is_some_and(|d| d.is_zero()));
    }

    #[test]
    fn a_launch_carries_the_binding_it_was_queued_with() {
        // The background pass has no invocation handle, so the asking session's
        // provider/model has to travel in the row. Losing it makes every child
        // resolve the default provider instead.
        let queue = Effects::new();
        queue.enqueue(Effect::Launch {
            task_id: "t1".into(),
            nodes: vec!["a".into()],
            max_parallel: 1,
            binding: Binding {
                provider: Some("anthropic".into()),
                model: Some("claude-sonnet-5".into()),
                thinking: None,
            },
            session: None,
        graph: None,
        rpi_exe: None,
        wave: 0,
        });
        let rows = queue.rows();
        match &rows[0].effect {
            Effect::Launch { binding, .. } => {
                assert_eq!(binding.provider.as_deref(), Some("anthropic"));
                assert_eq!(binding.model.as_deref(), Some("claude-sonnet-5"));
                assert!(!binding.is_empty());
            }
            other => panic!("unexpected effect: {other:?}"),
        }
    }

    #[test]
    fn a_durable_queue_resumes_after_a_restart() {
        // The point of the log: a process that dies mid-queue does not lose the
        // work. A second `Effects` over the same log is what a restart sees.
        let dir = std::env::temp_dir().join(format!(
            "rpi-superagent-durable-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        {
            let log = crate::effects_log::Log::open(&dir, "s1").unwrap();
            let queue = Effects::durable(log);
            assert!(queue.is_durable());
            queue.enqueue(Effect::Launch {
                task_id: "T1".into(),
                nodes: vec!["a".into()],
                max_parallel: 2,
                binding: Binding {
                    provider: Some("anthropic".into()),
                    model: Some("claude-sonnet-5".into()),
                    thinking: None,
                },
                session: Some("sess-9".into()),
            graph: None,
            rpi_exe: None,
            wave: 0,
            });
        }

        // A fresh handle stands in for a new process.
        let log = crate::effects_log::Log::open(&dir, "s1").unwrap();
        let resumed = Effects::durable(log);
        let rows = resumed.rows();
        assert_eq!(rows.len(), 1, "the queued row came back");
        match &rows[0].effect {
            Effect::Launch {
                task_id,
                binding,
                session,
                ..
            } => {
                assert_eq!(task_id, "T1");
                assert_eq!(binding.model.as_deref(), Some("claude-sonnet-5"));
                assert_eq!(session.as_deref(), Some("sess-9"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn a_finished_row_does_not_come_back_after_a_restart() {
        let dir = std::env::temp_dir().join(format!(
            "rpi-superagent-durable-done-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        {
            let log = crate::effects_log::Log::open(&dir, "s1").unwrap();
            let queue = Effects::durable(log);
            queue.enqueue(launch("T1"));
            assert_eq!(queue.dispatch(|_| Ok(())), 1);
        }

        let log = crate::effects_log::Log::open(&dir, "s1").unwrap();
        assert!(
            Effects::durable(log).rows().is_empty(),
            "a completed row must not be retried after a restart"
        );
    }

    #[test]
    fn a_launch_carries_the_session_it_was_queued_for() {
        // The graph is keyed by session. A pass that ran without the id would
        // switch the shared state to "no session" and clear the plan it was
        // queued to execute, which is exactly how the first version silently
        // did nothing.
        let queue = Effects::new();
        queue.enqueue(Effect::Launch {
            task_id: "t1".into(),
            nodes: vec!["a".into()],
            max_parallel: 1,
            binding: Binding::default(),
            session: Some("sess-42".into()),
            graph: None,
            rpi_exe: None,
            wave: 0,
        });
        match &queue.rows()[0].effect {
            Effect::Launch { session, .. } => {
                assert_eq!(session.as_deref(), Some("sess-42"));
            }
            other => panic!("unexpected effect: {other:?}"),
        }
    }

    #[test]
    fn an_empty_binding_is_recognised() {
        // No binding means the caller either named a model per node or the host
        // could not answer; both fall back to the child's own resolution.
        assert!(Binding::default().is_empty());
        assert!(!Binding {
            provider: Some("p".into()),
            model: None,
            thinking: None,
        }
        .is_empty());
        // A thinking level alone is a real binding: it changes how the child
        // runs even when the model is inherited.
        assert!(!Binding {
            provider: None,
            model: None,
            thinking: Some("high".into()),
        }
        .is_empty());
    }
}
