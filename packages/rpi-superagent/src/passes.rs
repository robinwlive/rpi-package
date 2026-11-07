//! The pass scheduler: a thread that drives the effect queue.
//!
//! DeepSeek's prototype has the same shape (`src/app/passes.ts`): a timer that
//! calls `effects.kick()` and advances due work, coalescing concurrent requests
//! into one follow-up pass so passes never pile up.
//!
//! ## Why a thread rather than the host's turn
//!
//! The whole point is that work continues while the person is not waiting on a
//! tool call. A pass driven by the model would need the model present, which is
//! exactly the dependency being removed.
//!
//! The thread owns nothing it cannot recreate: it reads the shared [`Effects`]
//! queue and calls a handler. Stopping it is dropping the [`Passes`] value or
//! calling [`Passes::stop`]; a dropped host ends the thread with the process.
//!
//! ## Tick interval
//!
//! Two seconds, matching the prototype. A pass that finds nothing due costs a
//! lock and a comparison, so the idle cost is negligible; a row enqueued by a
//! tool also calls [`Passes::request`], so work does not wait for the tick.

// `effects()` is part of the scheduler's contract even when only the tools and
// tests reach for it.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::effects::{Effect, Effects, Outcome};

/// How often an idle scheduler looks for due work.
pub const TICK: Duration = Duration::from_secs(2);

/// The scheduler. One per process, shared through [`crate::passes::shared`].
pub struct Passes {
    effects: Effects,
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Woken when a tool enqueues, so a fresh effect is not held for a tick.
    kick: Arc<AtomicBool>,
}

impl Passes {
    /// Start a scheduler driving `effects` with `run`.
    ///
    /// `run` is called for each due row; it is the same handler
    /// [`Effects::dispatch`] takes.
    pub fn start<F>(effects: Effects, run: F) -> Self
    where
        F: Fn(&Effect) -> Result<(), Outcome> + Send + 'static,
    {
        let running = Arc::new(AtomicBool::new(true));
        let kick = Arc::new(AtomicBool::new(false));
        let thread_running = Arc::clone(&running);
        let thread_kick = Arc::clone(&kick);
        let thread_effects = effects.clone();
        let handle = std::thread::Builder::new()
            .name("superagent-passes".into())
            .spawn(move || {
                while thread_running.load(Ordering::Acquire) {
                    thread_effects.dispatch(&run);
                    // A tool that enqueued meanwhile sets the flag, so the work
                    // starts now instead of on the next boundary.
                    if thread_kick.swap(false, Ordering::AcqRel) {
                        continue;
                    }
                    // Sleep the whole tick, or less when the soonest row is due
                    // earlier. Sliced so a cancelled scheduler ends the sleep at
                    // once instead of waiting out the whole interval.
                    let wait = thread_effects.next_due_in().unwrap_or(TICK).min(TICK);
                    let slice = Duration::from_millis(100);
                    let mut slept = Duration::ZERO;
                    while slept < wait && thread_running.load(Ordering::Acquire) {
                        std::thread::sleep(slice.min(wait - slept));
                        slept += slice;
                        if thread_kick.load(Ordering::Acquire) {
                            break;
                        }
                    }
                }
            })
            .map_err(|error| {
                // A scheduler that cannot start is a silent failure otherwise: the
                // queue fills and nothing runs. Say so, loudly, once.
                eprintln!("rpi-superagent: could not start the pass scheduler: {error}");
                error
            })
            .ok();
        Passes {
            effects,
            running,
            handle,
            kick,
        }
    }

    /// Ask the scheduler to look for work now.
    pub fn request(&self) {
        self.kick.store(true, Ordering::Release);
    }

    /// The queue this scheduler drives.
    pub fn effects(&self) -> &Effects {
        &self.effects
    }

    /// Stop the scheduler and wait for its thread.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Passes {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn launch(task: &str) -> Effect {
        Effect::Launch {
            task_id: task.to_string(),
            nodes: vec!["a".into()],
            max_parallel: 1,
            binding: crate::effects::Binding::default(),
            session: None,
            graph: None,
            rpi_exe: None,
        }
    }

    #[test]
    fn a_pass_runs_a_queued_effect_without_a_caller() {
        let effects = Effects::new();
        let ran = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ran);
        let mut passes = Passes::start(effects.clone(), move |_| {
            counter.fetch_add(1, Ordering::AcqRel);
            Ok(())
        });
        effects.enqueue(launch("t1"));
        passes.request();
        // The scheduler runs on its own thread; give it a moment to see the work.
        for _ in 0..50 {
            if ran.load(Ordering::Acquire) > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        passes.stop();
        assert!(ran.load(Ordering::Acquire) > 0, "the pass never ran");
        assert!(effects.is_empty(), "a successful effect leaves the queue");
    }

    #[test]
    fn stopping_ends_the_thread() {
        let effects = Effects::new();
        let mut passes = Passes::start(effects, |_| Ok(()));
        passes.stop();
        // A second stop is a no-op rather than a panic.
        passes.stop();
    }
}
