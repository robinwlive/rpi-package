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
}

impl Binding {
    /// Read the binding out of a tool call's `params`, at enqueue time.
    ///
    /// Called from the tool thread, which is the last moment the host's handle is
    /// available.
    pub fn from_params(params: &serde_json::Value) -> Self {
        let selection = crate::host::parent_selection(params);
        Binding {
            provider: selection.provider,
            model: selection.model,
        }
    }

    /// Whether anything was captured.
    pub fn is_empty(&self) -> bool {
        self.provider.is_none() && self.model.is_none()
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
    pub fn key(&self) -> &str {
        match self {
            Effect::Launch { task_id, .. }
            | Effect::Extend { task_id, .. }
            | Effect::Relay { task_id, .. } => task_id,
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
}

impl Row {
    fn new(effect: Effect, seq: u64, now: Instant) -> Self {
        Row {
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
}

impl Effects {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue an effect, replacing any waiting row of the same kind and key.
    ///
    /// The replacement keeps the original queue position and becomes due at
    /// once — the same rule DeepSeek states: *"Enqueuing the same waiting key
    /// replaces its payload and makes it due immediately without changing queue
    /// order."*
    pub fn enqueue(&self, effect: Effect) {
        let now = Instant::now();
        let mut rows = self.rows.lock().unwrap_or_else(|e| e.into_inner());
        let kind = effect.kind();
        let key = effect.key().to_string();
        if let Some(row) = rows
            .iter_mut()
            .find(|row| row.effect.kind() == kind && row.effect.key() == key)
        {
            row.effect = effect;
            row.attempts = 0;
            row.error = None;
            row.due = now;
            return;
        }
        let mut seq = self.seq.lock().unwrap_or_else(|e| e.into_inner());
        *seq += 1;
        rows.push(Row::new(effect, *seq, now));
    }

    /// Every row, in queue order. For inspection and tests.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = self.rows.lock().unwrap_or_else(|e| e.into_inner()).clone();
        rows.sort_by_key(|row| row.seq);
        rows
    }

    /// Whether a row for this kind and key is waiting.
    pub fn pending(&self, kind: Kind, key: &str) -> bool {
        self.rows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|row| row.effect.kind() == kind && row.effect.key() == key)
    }

    /// Drop a waiting row. A row already running finishes normally.
    pub fn cancel(&self, kind: Kind, key: &str) {
        self.rows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|row| !(row.effect.kind() == kind && row.effect.key() == key));
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
                if running.iter().any(|(k, key)| {
                    *k == kind && key == row.effect.key()
                }) {
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
            let key = row.effect.key().to_string();
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
        let now = Instant::now();
        let mut rows = self.rows.lock().unwrap_or_else(|e| e.into_inner());
        let Some(index) = rows
            .iter()
            .position(|row| row.effect.kind() == kind && row.effect.key() == key && row.seq == seq)
        else {
            // The row was cancelled while it ran, or replaced. Its result is for
            // a payload that is no longer queued, so drop it.
            return;
        };
        match outcome {
            Ok(()) => {
                rows.remove(index);
                self.broken
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&kind);
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
                row.error = Some(message);
                let mut broken = self.broken.lock().unwrap_or_else(|e| e.into_inner());
                let count = broken.entry(kind).or_default();
                *count = count.saturating_add(1);
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
            },
            session: None,
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
        }
        .is_empty());
    }
}
