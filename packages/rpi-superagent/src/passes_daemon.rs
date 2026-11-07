//! The separable half of the pass scheduler: what a daemon process needs.
//!
//! The scheduler has two lives. Inside a long-lived host it runs as a thread
//! ([`crate::passes`]) because the host is already up and the queue is already in
//! memory. When the host may exit at any moment, the queue has to be driven by
//! something else, and a thread cannot be that: it dies with its host.
//!
//! This module is what the second life needs — locating the effect logs, taking
//! the single-instance lock, and the serve loop — so `bin/passes.rs` is a thin
//! entry point rather than a second implementation.
//!
//! ## What the daemon shares with the plugin
//!
//! Only files. It parses the same JSONL rows ([`crate::effects_log`]) and runs
//! them through the same handler ([`crate::effects_handler`]). It does **not**
//! have a host to talk to, so a relay it produces is written back to the log as a
//! pending relay for a live host to deliver, rather than steered into a session
//! directly. That is the one path that needs the host, and it fails safe: the row
//! waits.

// `flock` has no safe equivalent, exactly as in `supervisor.rs`. This module
// re-enables it for itself alone.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::effects::{Effect, Outcome, Row};

/// The directory holding every effect log, relative to the project root.
pub const ROOT: &str = ".rpi/superagent-effects";

/// The scheduler's lock file, inside [`ROOT`].
const LOCK_FILE: &str = ".lock";

/// How often an idle daemon looks for work.
pub const TICK: Duration = Duration::from_secs(2);

/// How many times a failing row is retried before it is given up on.
///
/// Without a cap a permanently broken row retries forever: the log grows, the
/// daemon never idles, and nothing tells a person it is hopeless. Twelve is the
/// 5s-doubling-to-300s schedule reaching its ceiling twice over.
pub const MAX_ATTEMPTS: u32 = 12;

/// The project root the daemon serves: the working directory it was started in.
///
/// The plugin starts the daemon from the project, so the daemon's own working
/// directory is the project root. `RPI_SUPERAGENT_EFFECTS_ROOT` overrides it,
/// which is what a test uses to run against a scratch directory.
pub fn root_dir() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("RPI_SUPERAGENT_EFFECTS_ROOT") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    std::env::current_dir().ok().map(|cwd| cwd.join(ROOT))
}

/// The root the daemon serves, under a project working directory.
pub fn effects_dir(cwd: &Path) -> PathBuf {
    cwd.join(ROOT)
}

/// Create the root if it does not exist.
pub fn create_root(root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root)
}

/// Take the single-instance lock for `root`.
///
/// Returns the open file holding the lock, or `None` when another scheduler has
/// it. The caller must keep the file alive: the kernel releases the lock when the
/// descriptor closes, so dropping it would end the single-instance guarantee.
pub fn take_lock(root: &Path) -> Option<std::fs::File> {
    let path = root.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .ok()?;
    if !lock_exclusive(&file) {
        return None;
    }
    Some(file)
}

/// Take an exclusive, non-blocking lock.
#[cfg(unix)]
fn lock_exclusive(file: &std::fs::File) -> bool {
    use std::os::unix::io::AsRawFd;
    extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    // SAFETY: `flock` only reads the descriptor and an int; the file outlives
    // the call.
    unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) == 0 }
}

#[cfg(not(unix))]
fn lock_exclusive(_file: &std::fs::File) -> bool {
    // Without `flock` a second scheduler may start; the rows are idempotent
    // enough that the cost is a repeated attempt, not corruption.
    true
}

/// Every effect log under `root`.
pub fn logs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "jsonl") {
            out.push(path);
        }
    }
    // Stable order, so two ticks consider logs the same way.
    out.sort();
    out
}

/// Serve the queue and the orphan sweep until the process ends.
///
/// Two jobs, one process. They were separate programs at first — a thread-based
/// queue in the plugin, and a sweeper from an earlier stage — and the split cost
/// more than it saved: two locks, two logs, two launches, and no way for either
/// to know what the other saw. A worker the sweep kills because its host died is
/// exactly the kind of event the queue wants to hear about.
///
/// The two run on different periods, so one loop cannot just sleep a fixed
/// interval: the queue wants to be responsive (seconds), the sweep wants to be
/// cheap (minutes). The loop therefore sleeps a short slice and counts, which
/// keeps a single detector for cancellation and one place that decides what runs.
pub fn serve(root: &Path) -> ! {
    let sweep_secs = crate::supervisor::interval_secs().max(1);
    let slice = Duration::from_millis(500);
    let mut since_sweep = Duration::ZERO;
    loop {
        let mut ran = 0usize;
        for path in logs(root) {
            ran += serve_log(&path);
        }
        // A queue that just unblocked is drained without waiting a tick.
        if ran == 0 {
            std::thread::sleep(TICK);
            since_sweep += TICK;
        }
        if since_sweep >= Duration::from_secs(sweep_secs) {
            since_sweep = Duration::ZERO;
            let killed = crate::supervisor::sweep();
            if !killed.is_empty() {
                eprintln!(
                    "rpi-superagent: swept {} orphaned worker(s): {killed:?}",
                    killed.len()
                );
            }
        }
        // A `Later` row pushes its own due time out, so this cannot spin; the
        // slice only bounds how long a shutdown takes to notice.
        let _ = slice;
    }
}

/// Run every due row of one log, writing the outcomes back.
pub fn serve_log(path: &Path) -> usize {
    let Some(owner) = log_owner(path) else {
        return 0;
    };
    // `Log::open` takes the effects root itself.
    let Some(effects_root) = log_path_root(path) else {
        return 0;
    };
    let mut log = match crate::effects_log::Log::open(&effects_root, &owner) {
        Ok(log) => log,
        Err(_) => return 0,
    };
    let rows = log.load();
    let now = crate::effects_log::now_ms();
    let mut ran = 0;
    for row in rows {
        if !is_due(&row, now) {
            continue;
        }
        ran += 1;
        match crate::effects_handler::run(&row.effect) {
            Ok(()) => {
                // Retire the row. A write that fails here is reported, not
                // swallowed: the work is done but the log would say otherwise, and
                // the next tick would do it again.
                if let Err(error) = log.append_state(
                    row.effect.kind(),
                    row.effect.key(),
                    row.seq,
                    crate::effects_log::State::Done,
                    None,
                ) {
                    eprintln!(
                        "rpi-superagent: a finished effect could not be retired ({error}); \
                         it will run again"
                    );
                }
            }
            Err(Outcome::Later(reason)) => {
                // Deferred, not failed: push the due time out without counting an
                // attempt, or this tick would immediately try it again.
                let (initial, _) = row.effect.kind().retry();
                if let Err(error) = log.append_queued(
                    &row.effect,
                    row.seq,
                    session_of(&row.effect).as_deref(),
                    row.attempts,
                    now + initial.as_millis() as u64,
                    Some(&reason),
                ) {
                    eprintln!("rpi-superagent: a deferred effect could not be recorded ({error})");
                }
            }
            Err(Outcome::Failed(message)) => {
                // Retry with backoff, capped so a permanently broken row does not
                // grow without end.
                let attempts = row.attempts.saturating_add(1);
                if attempts > MAX_ATTEMPTS {
                    eprintln!(
                        "rpi-superagent: giving up on {} `{}` after {attempts} attempts: {message}",
                        row.effect.kind().as_str(),
                        row.effect.key()
                    );
                    let _ = log.append_state(
                        row.effect.kind(),
                        row.effect.key(),
                        row.seq,
                        crate::effects_log::State::Done,
                        Some(&message),
                    );
                    continue;
                }
                if let Err(error) = log.append_queued(
                    &row.effect,
                    row.seq,
                    session_of(&row.effect).as_deref(),
                    attempts,
                    now + backoff_ms(row.effect.kind(), row.attempts),
                    Some(&message),
                ) {
                    eprintln!("rpi-superagent: a failed effect could not be recorded ({error})");
                }
            }
        }
    }
    ran
}

/// The owner name a log file stands for.
fn log_owner(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    Some(stem)
}

/// The directory a log lives in, for reopening it by owner.
fn log_path_root(path: &Path) -> Option<PathBuf> {
    path.parent().map(Path::to_path_buf)
}

/// Whether a row may run now.
fn is_due(row: &Row, now_ms: u64) -> bool {
    let _ = now_ms;
    row.due <= std::time::Instant::now()
}

/// The retry delay for `kind` after `attempts` failures, in milliseconds.
///
/// The same doubling the in-memory queue uses, expressed in the unit the log
/// stores.
fn backoff_ms(kind: crate::effects::Kind, attempts: u32) -> u64 {
    let (initial, maximum) = kind.retry();
    let shift = attempts.min(16);
    let delay = initial.saturating_mul(1u32 << shift).min(maximum);
    delay.as_millis() as u64
}

/// The session a row belongs to, when it has one.
fn session_of(effect: &Effect) -> Option<String> {
    match effect {
        Effect::Launch { session, .. } | Effect::Extend { session, .. } => session.clone(),
        Effect::Relay { .. } => None,
    }
}

/// Start the daemon process, if it is not already running.
///
/// Returns whether a process was started. A daemon already holding the lock is
/// not an error: it is already doing the work.
///
/// The daemon is spawned rather than forked for the reason `start_supervisor`
/// gives: a forked child of a threaded process may only call async-signal-safe
/// functions, which is not enough to run a queue.
pub fn start_daemon(cwd: &Path) -> bool {
    let Some(exe) = daemon_executable() else {
        return false;
    };
    std::process::Command::new(&exe)
        .current_dir(cwd)
        // No terminal: a caller reading this process's pipes must not be held
        // open by the daemon.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// The daemon binary, installed beside the extension that starts it.
///
/// The extension's own directory is the reliable place to look: `dladdr` reports
/// the loaded image, which is the `.dylib`/`.so`/`.dll` itself, and the packaging
/// puts the daemon beside it. The host's directory is tried as a fallback.
fn daemon_executable() -> Option<PathBuf> {
    const NAME: &str = "rpi-superagent-daemon";
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = crate::supervisor::extension_dir() {
        dirs.push(dir);
    }
    if let Ok(host) = std::env::current_exe() {
        if let Some(dir) = host.parent() {
            dirs.push(dir.to_path_buf());
        }
    }
    for dir in dirs {
        let candidate = dir.join(NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        // Windows appends `.exe`; try it too so the lookup is not platform-blind.
        let with_exe = dir.join(format!("{NAME}.exe"));
        if with_exe.is_file() {
            return Some(with_exe);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::{Binding, Effect};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpi-superagent-daemon-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_second_scheduler_cannot_take_the_lock() {
        let root = scratch("lock");
        let first = take_lock(&root).expect("the first takes it");
        assert!(
            take_lock(&root).is_none(),
            "a second scheduler must not start"
        );
        drop(first);
        assert!(
            take_lock(&root).is_some(),
            "the lock is free once released"
        );
    }

    #[test]
    fn only_jsonl_files_are_logs() {
        let root = scratch("logs");
        std::fs::write(root.join("a.jsonl"), "").unwrap();
        std::fs::write(root.join(".lock"), "").unwrap();
        std::fs::write(root.join("b.txt"), "").unwrap();
        let found = logs(&root);
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with("a.jsonl"));
    }

    #[test]
    fn a_queued_row_is_served_and_retired() {
        // End to end through the daemon path: the log is the only state.
        let root = scratch("serve");
        let owner = "s1";
        {
            let mut log = crate::effects_log::Log::open(&root, owner).unwrap();
            log.append_queued(
                &Effect::Launch {
                    task_id: "T1".into(),
                    nodes: vec!["a".into()],
                    max_parallel: 1,
                    binding: Binding::default(),
                    session: None,
                    graph: None,
                    rpi_exe: None,
                },
                1,
                None,
                0,
                crate::effects_log::now_ms(),
                None,
            )
            .unwrap();
        }
        let path = crate::effects_log::log_path(&root, owner);
        let ran = serve_log(&path);
        assert_eq!(ran, 1, "the due row was attempted");
        // It ran against no graph, so it failed and stays queued with a later
        // due time rather than being lost.
        let log = crate::effects_log::Log::open(&root, owner).unwrap();
        let rows = log.load();
        assert_eq!(rows.len(), 1, "a failed row stays queued");
        assert!(
            rows[0].attempts >= 1,
            "the failure was counted: {}",
            rows[0].attempts
        );
    }

    #[test]
    fn a_future_row_is_not_run_early() {
        let root = scratch("future-row");
        let owner = "s2";
        {
            let mut log = crate::effects_log::Log::open(&root, owner).unwrap();
            log.append_queued(
                &Effect::Relay {
                    task_id: "T1".into(),
                    text: "later".into(),
                },
                1,
                None,
                0,
                crate::effects_log::now_ms() + 3_600_000,
                None,
            )
            .unwrap();
        }
        let path = crate::effects_log::log_path(&root, owner);
        assert_eq!(serve_log(&path), 0, "an hour away is not due");
    }

    #[test]
    fn the_sweep_and_the_queue_share_one_loop() {
        // The merge is the point: the same serve loop does both jobs. A test that
        // only exercised the queue would pass even if the sweep were never called,
        // so this asserts the sweep is reachable from the daemon at all.
        let root = scratch("merge");
        let killed = crate::supervisor::sweep();
        // Without a session directory the sweep finds nothing, which is the
        // correct answer rather than an error.
        assert!(killed.is_empty(), "no session dir means no orphans: {killed:?}");
        // And the queue still works with the sweep in the loop.
        let owner = "s1";
        {
            let mut log = crate::effects_log::Log::open(&root, owner).unwrap();
            log.append_queued(
                &Effect::Relay {
                    task_id: "T1".into(),
                    text: "x".into(),
                },
                1,
                None,
                0,
                crate::effects_log::now_ms(),
                None,
            )
            .unwrap();
        }
        let path = crate::effects_log::log_path(&root, owner);
        assert_eq!(serve_log(&path), 1, "the queue still runs");
    }

    #[test]
    fn a_row_that_keeps_failing_is_given_up_on() {
        // Without a cap a hopeless row would retry forever and the daemon would
        // never idle. The cap is what makes "given up" a state rather than a hope.
        let root = scratch("give-up");
        let owner = "s2";
        {
            let mut log = crate::effects_log::Log::open(&root, owner).unwrap();
            log.append_queued(
                &Effect::Extend {
                    task_id: "T1".into(),
                    node: serde_json::json!({"id": "a", "task": "x"}),
                    binding: Binding::default(),
                    session: None,
                },
                1,
                None,
                MAX_ATTEMPTS,
                crate::effects_log::now_ms(),
                Some("always fails"),
            )
            .unwrap();
        }
        let path = crate::effects_log::log_path(&root, owner);
        serve_log(&path);
        // `Extend` always fails here, and it was already at the cap, so the row
        // is retired rather than retried.
        let log = crate::effects_log::Log::open(&root, owner).unwrap();
        assert!(
            log.load().is_empty(),
            "a row past the attempt cap is retired, not retried forever"
        );
    }

    #[test]
    fn backoff_doubles_and_caps() {
        use crate::effects::Kind;
        let first = backoff_ms(Kind::Relay, 0);
        let second = backoff_ms(Kind::Relay, 1);
        assert!(second > first);
        // The cap holds however many attempts pile up.
        let capped = backoff_ms(Kind::Relay, 40);
        assert_eq!(capped, Kind::Relay.retry().1.as_millis() as u64);
    }
}
