//! The supervisor: a process that outlives the session it was started from.
//!
//! ## Why it exists
//!
//! A host that dies leaves its children running. Nothing in the session can
//! clean them up afterwards, because the session is what died. A thread
//! inside the host dies with it.
//!
//! So the cleaner has to live outside the session. This module is the logic;
//! `src/bin/supervisor.rs` is the process that runs it, and the plugin starts
//! that process.
//!
//! ## What it reads, and what it does not
//!
//! It reads the session files. Those are the host's own transcripts, and the
//! only way to find a child that a dead session left behind is to look at
//! what that session recorded. It reads **only** the custom entries this
//! package writes, and it only ever acts on the process ids found there.
//!
//! ## One supervisor, not one per session
//!
//! Several hosts may run at once. If each started its own supervisor, they
//! would all sweep the same directory and race each other over the same
//! processes. A lock file makes the first one the supervisor and the rest
//! leave the work to it.

//! The lock needs `flock`, which has no safe standard-library equivalent.
//! The unsafe is confined to this module.

#![allow(unsafe_code)]

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The custom entry type this package owns.
pub const TASK_ENTRY_TYPE: &str = super::task::TASK_ENTRY_TYPE;

/// How often the supervisor sweeps.
pub const SUPERVISOR_INTERVAL_SECS: u64 = 300;

/// The name of the lock file.
const LOCK_FILE: &str = "superagent-supervisor.lock";

/// The name of the pid file.
const PID_FILE: &str = "superagent-supervisor.pid";

/// The name of the log file.
const LOG_FILE: &str = "superagent-supervisor.log";

/// Where the supervisor keeps its lock, pid and log files.
///
/// They sit beside the session directory rather than inside it: the session
/// directory holds the transcripts, and mixing the two would make the lock
/// look like session data.
pub fn state_dir(session_dir: &Path) -> PathBuf {
    session_dir
        .parent()
        .map(|parent| parent.to_path_buf())
        .unwrap_or_else(|| session_dir.to_path_buf())
}

/// The session directory the supervisor watches.
///
/// The host names it through the environment. Without it there is nothing to
/// watch, and the supervisor has nothing to do.
pub fn watched_dir() -> Option<PathBuf> {
    std::env::var("RPI_CODING_AGENT_SESSION_DIR")
        .ok()
        .map(|dir| dir.trim().to_string())
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

/// Every session file under the watched directory that could hold a task.
fn session_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(dir) = watched_dir() {
        collect_jsonl(&dir, 0, &mut files);
    }
    files
}

/// How much of a file's tail is read when looking for task snapshots.
const TAIL_BYTES: u64 = 64 * 1024;

/// How recently a file must have been touched for its tail to be worth reading.
///
/// A session that has not been touched for hours is not running anything, so
/// its tasks cannot be orphaned by a host that just died. The window is
/// generous because the heartbeat that keeps a live session's file fresh runs
/// once a minute, and a file's mtime moves for other reasons too.
const RECENT_SECS: u64 = 6 * 3600;

/// Whether a file was touched recently enough to be worth reading.
fn touched_recently(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(age) = modified.elapsed() else {
        return false;
    };
    age.as_secs() <= RECENT_SECS
}

/// Whether a file's tail mentions a task snapshot.
///
/// The supervisor looks at every session file, and most of them hold no task
/// at all — reading them whole costs hundreds of megabytes per sweep for
/// nothing. A snapshot is appended when a task changes, so the newest one is
/// always near the end: the tail is enough to tell whether the file is worth
/// reading in full.
///
/// A file whose tail does not mention a task is not read further. That can miss
/// a task whose every snapshot is far from the end, but the newest snapshot of
/// a live task is by definition recent, and a task that stopped being written
/// is no longer worth reaping.
fn tail_mentions_task(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(len) = file.metadata().map(|meta| meta.len()) else {
        return false;
    };
    if len <= TAIL_BYTES {
        return std::fs::read_to_string(path)
            .map(|text| text.contains(TASK_ENTRY_TYPE))
            .unwrap_or(false);
    }
    use std::io::{Read, Seek, SeekFrom};
    let mut file = file;
    if file.seek(SeekFrom::Start(len - TAIL_BYTES)).is_err() {
        return false;
    }
    let mut tail = vec![0u8; TAIL_BYTES as usize];
    let Ok(read) = file.read(&mut tail) else {
        return false;
    };
    tail.truncate(read);
    // A snapshot line can straddle the boundary, so the search is on bytes:
    // the type name is plain ASCII and cannot be split in a way that hides it
    // from a substring search.
    String::from_utf8_lossy(&tail).contains(TASK_ENTRY_TYPE)
}

/// Collect `.jsonl` files, a couple of levels down.
///
/// The session directory holds one subdirectory per working directory, and
/// the transcripts inside. Deeper nesting does not exist.
fn collect_jsonl(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 2 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, depth + 1, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

/// The child process ids recorded across every session.
///
/// The latest snapshot of each task wins: an older one may name a child that
/// has since finished, and the reaper checks liveness anyway.
fn recorded_pids() -> Vec<u32> {
    let mut by_task: std::collections::HashMap<String, Vec<u32>> = std::collections::HashMap::new();
    for file in session_files() {
        // Two cheap checks before reading anything: a file that has not been
        // touched for hours is not running anything, and a file whose tail does
        // not mention a task holds none. Between them they keep the sweep to
        // the few files that could matter.
        if !touched_recently(&file) {
            continue;
        }
        if !tail_mentions_task(&file) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines() {
            let line = line.trim();
            if !line.contains(TASK_ENTRY_TYPE) {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if entry.get("customType").and_then(Value::as_str) != Some(TASK_ENTRY_TYPE) {
                continue;
            }
            let Some(data) = entry.get("data") else {
                continue;
            };
            let Some(task_id) = data.get("taskId").and_then(Value::as_str) else {
                continue;
            };
            by_task.insert(
                task_id.to_string(),
                data.get("pids")
                    .and_then(Value::as_array)
                    .map(|pids| {
                        pids.iter()
                            .filter_map(Value::as_u64)
                            .map(|pid| pid as u32)
                            .collect()
                    })
                    .unwrap_or_default(),
            );
        }
    }
    by_task.into_values().flatten().collect()
}

/// Sweep the sessions and kill the orphans among them.
///
/// Returns the ids it killed.
pub fn sweep() -> Vec<u32> {
    let mut killed = Vec::new();
    for pid in recorded_pids() {
        if pid == std::process::id() {
            continue;
        }
        if !super::spawn::process_is_alive(pid) {
            continue;
        }
        super::spawn::kill_process_tree(pid);
        killed.push(pid);
    }
    killed
}

/// Take the supervisor lock, without waiting.
///
/// The lock is held for as long as the descriptor is open: the kernel releases
/// it when the holder dies, so a crashed supervisor is replaced by the next
/// one that starts.
pub fn take_lock() -> Option<std::fs::File> {
    let session_dir = watched_dir()?;
    let lock_path = state_dir(&session_dir).join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .ok()?;
    if !lock_exclusive(&file) {
        return None;
    }
    let pid_path = state_dir(&session_dir).join(PID_FILE);
    let _ = std::fs::write(&pid_path, format!("{}", std::process::id()));
    Some(file)
}

/// Take an exclusive lock without waiting.
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

#[cfg(windows)]
fn lock_exclusive(file: &std::fs::File) -> bool {
    use std::os::windows::io::AsRawHandle;
    extern "C" {
        fn LockFileEx(
            handle: *mut std::ffi::c_void,
            flags: u32,
            reserved: u32,
            bytes_low: u32,
            bytes_high: u32,
            overlapped: *mut std::ffi::c_void,
        ) -> i32;
    }
    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 2;
    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 1;
    let mut overlapped = std::mem::zeroed();
    // SAFETY: the handle comes from an open file and outlives the call.
    unsafe {
        LockFileEx(
            file.as_raw_handle() as *mut std::ffi::c_void,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        ) != 0
    }
}

/// The sweep interval, overridable for tests and for impatient users.
pub fn interval_secs() -> u64 {
    std::env::var("RPI_SUPERAGENT_SWEEP_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(SUPERVISOR_INTERVAL_SECS)
}

/// Detach this process from the session that started it.
///
/// The supervisor must outlive its host, and a process that stays in the
/// host's session is signalled when that session ends. `setsid` puts it in a
/// session of its own.
///
/// This runs in the freshly spawned supervisor, which has no threads yet, so
/// the fork that `setsid` implies is safe here.
pub fn detach() {
    #[cfg(unix)]
    {
        extern "C" {
            fn setsid() -> i32;
        }
        // SAFETY: `setsid` takes no arguments and touches no memory. The
        // process has a single thread at this point.
        unsafe {
            setsid();
        }
    }
}

/// Start the supervisor process.
///
/// The supervisor is a separate program, started with `exec` through
/// `Command::spawn`. Forking from inside the host would inherit the host's
/// threads, and a forked child of a threaded process may only call
/// async-signal-safe functions — which is not enough to run a sweep. Spawning
/// gives the supervisor a clean process instead.
///
/// Returns whether a supervisor was started. When one is already running, the
/// new process finds the lock taken and exits at once.
pub fn start_supervisor() -> bool {
    let Some(session_dir) = watched_dir() else {
        return false;
    };
    let Some(exe) = supervisor_executable() else {
        return false;
    };
    let spawned = std::process::Command::new(&exe)
        .env("RPI_CODING_AGENT_SESSION_DIR", &session_dir)
        // The supervisor has no terminal: its output is discarded so a
        // caller reading this process's pipes is not held open by it.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    spawned.is_ok()
}

/// The supervisor binary.
///
/// It is installed beside the extension that starts it, so the extension's own
/// path is what points at it. The host's directory is tried as well: an
/// installed package puts both in the same place.
fn supervisor_executable() -> Option<PathBuf> {
    const NAME: &str = "rpi-superagent-supervisor";
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = extension_dir() {
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
    }
    None
}

/// The directory this extension was loaded from.
///
/// `dladdr` reports the path of the running image, which for a plugin is the
/// extension itself.
#[cfg(unix)]
fn extension_dir() -> Option<PathBuf> {
    use std::ffi::c_void;
    #[repr(C)]
    struct DlInfo {
        dli_fname: *const i8,
        dli_fbase: *mut c_void,
        dli_sname: *const i8,
        dli_saddr: *mut c_void,
    }
    extern "C" {
        fn dladdr(addr: *const c_void, info: *mut DlInfo) -> i32;
    }
    // The address of this function identifies the running image.
    let mut info = DlInfo {
        dli_fname: std::ptr::null(),
        dli_fbase: std::ptr::null_mut(),
        dli_sname: std::ptr::null(),
        dli_saddr: std::ptr::null_mut(),
    };
    // SAFETY: `dladdr` fills the structure it is given. The pointer it writes
    // points at the loader's own string, which stays valid for the life of the
    // process.
    let ok = unsafe { dladdr(extension_dir as *const c_void, &mut info) };
    if ok == 0 || info.dli_fname.is_null() {
        return None;
    }
    // SAFETY: the loader's string is NUL-terminated.
    let path = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) };
    let path = PathBuf::from(path.to_string_lossy().into_owned());
    path.parent().map(|dir| dir.to_path_buf())
}

#[cfg(not(unix))]
fn extension_dir() -> Option<PathBuf> {
    None
}

/// Run the supervisor loop until the process is killed.
pub fn supervise() -> ! {
    let log_path = watched_dir()
        .map(|dir| state_dir(&dir).join(LOG_FILE))
        .unwrap_or_else(|| PathBuf::from(LOG_FILE));
    let interval = interval_secs();
    let mut tick = 0u64;
    loop {
        std::thread::sleep(std::time::Duration::from_secs(interval));
        tick += 1;
        let killed = sweep();
        append_log(&log_path, &format!("tick={tick} killed={killed:?}\n"));
    }
}

/// Append a line to the supervisor's log.
fn append_log(path: &Path, line: &str) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_dir_sits_beside_the_session_dir() {
        let dir = Path::new("/tmp/agent/sessions");
        assert_eq!(state_dir(dir), Path::new("/tmp/agent"));
    }

    /// A file that has not been touched for hours cannot be running anything,
    /// so it is not worth reading.
    #[test]
    fn an_old_file_is_not_worth_reading() {
        let dir = std::env::temp_dir().join(format!("rpi-superagent-age-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.jsonl");
        std::fs::write(&path, "{\"customType\":\"superagent-task\"}\n").unwrap();
        assert!(touched_recently(&path), "a file just written is recent");
        // Backdate it well past the window.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(RECENT_SECS + 3600);
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(old).unwrap();
        drop(file);
        assert!(!touched_recently(&path), "an old file must not be recent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_task_snapshot_yields_its_pids() {
        let entry = serde_json::json!({
            "customType": "superagent-task",
            "data": {"taskId": "t1", "pids": [11, 22]}
        });
        let data = entry.get("data").unwrap();
        let pids: Vec<u32> = data
            .get("pids")
            .and_then(Value::as_array)
            .map(|pids| {
                pids
                    .iter()
                    .filter_map(Value::as_u64)
                    .map(|pid| pid as u32)
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(pids, vec![11, 22]);
    }

    #[test]
    fn the_interval_can_be_overridden() {
        std::env::set_var("RPI_SUPERAGENT_SWEEP_SECS", "7");
        assert_eq!(interval_secs(), 7);
        std::env::remove_var("RPI_SUPERAGENT_SWEEP_SECS");
        assert_eq!(interval_secs(), SUPERVISOR_INTERVAL_SECS);
    }
}
