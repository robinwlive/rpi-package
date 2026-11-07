//! Subprocess runner for a single delegation.
//!
//! The runtime already parallelises tool calls (`rpi-agent`'s
//! `execute_tool_calls_parallel`) and feeds every tool result back into the
//! orchestrator's context, so this module only has to launch, bound, and collect
//! one child `rpi`.
//!
//! The task travels on **stdin**, not as a positional argument: `rpi -p`
//! consumes the next non-flag token as its prompt, so a task beginning with `-`
//! would be parsed as a flag. Piped stdin becomes the initial prompt
//! (`rpi-cli::app::build_initial_message`).

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rpi_plugin_sdk::process::ProcessTree;
use wait_timeout::ChildExt;

use crate::control::Control;

/// Tools this package registers. Excluded from the child's tool set so a
/// subagent cannot recursively orchestrate (design §8 "递归守卫").
pub const EXCLUDED_TOOLS: &str = "superagent_delegate,superagent_dispatch,superagent_plan";

/// Upper bound on the text fed back to the orchestrator (design §8 "输出截断").
pub const MAX_OUTPUT_BYTES: usize = 40_000;

/// Turn budget env the host already enforces (`rpi-harness::run_budget`).
pub const MAX_TURNS_ENV: &str = "RPI_MAX_TURNS_PER_RUN";

/// Marker set on the child so the orchestrator guidance is not injected into a
/// subagent (which does not have `plan`/`dispatch`).
pub const SUBAGENT_ENV: &str = "RPI_SUPERAGENT_SUBAGENT";

/// Environment variable carrying the agent-spawn generation a worker runs at.
pub const TIER_ENV: &str = "RPI_SUPERAGENT_TIER";

/// The binding, duplicated as environment variables.
///
/// The flag path depends on a host query that can fail; `rpi` fills any absent
/// flag from these, so one failed query no longer demotes the worker to
/// `settings.json`'s default model. Explicit flags still win inside `rpi`.
/// Must match `rpi-cli/src/app.rs` (ENV_PROVIDER / ENV_MODEL / ENV_THINKING).
pub const ENV_PROVIDER: &str = "RPI_PROVIDER";
pub const ENV_MODEL: &str = "RPI_MODEL";
pub const ENV_THINKING: &str = "RPI_THINKING";

/// The Dash bridge's three switches, which a worker must not inherit.
///
/// `PI_DINGO_BRIDGE` is the switch; the other two are the parent session's
/// credentials. The bridge starts only when all three are present, so removing
/// the switch alone would be enough — but a `Command` inherits the whole
/// environment, so all three are removed and nothing is left to leak.
pub const BRIDGE_ENV: &[&str] = &["PI_DINGO_BRIDGE", "PI_DINGO_URL", "PI_DINGO_TOKEN"];

/// How often the wait loop re-checks cancellation/timeout. Bounds how long a
/// cancelled call can keep a child alive.
pub(crate) const POLL_SLICE: Duration = Duration::from_millis(100);

/// Build the child command. Separate from `run` so it is unit-testable without
/// [`child_command_with_session`] for an ephemeral delegation — no saved
/// session, nothing to resume.
///
/// `provider` / `model` are passed through as `--provider` / `--model`. The child
/// shares the orchestrator's agent dir but not its binding, so without them it
/// would resolve the *default* provider from settings — wrong (or broken)
/// whenever this session runs on anything else, or on a provider the default does
/// not authenticate. Callers therefore resolve the effective pair from
/// [`crate::host::parent_selection`] first and always pass it down.
#[cfg_attr(not(test), allow(dead_code))]
pub fn child_command(
    exe: &Path,
    max_turns: u64,
    provider: Option<&str>,
    model: Option<&str>,
    cwd: Option<&Path>,
) -> Command {
    child_command_with_session(exe, max_turns, provider, model, None, cwd, None, None, 0)
}

/// The saved session id matching `name`, if one was already created.
///
/// The host's `--session` **only resumes an existing session**; it refuses a
/// name it has never seen. A worker is created once with `--name` and every
/// later delegation resumes it by the id found here.
///
/// Sessions live in `<session_dir>/<cwd hash>/<timestamp>_<id>.jsonl`, and the
/// id is the file's first line while the name is a `fact` entry inside it. Both
/// are read rather than guessed, so a worker whose session was deleted simply
/// gets a new one.
pub fn find_session_id(session_dir: &Path, name: &str) -> Option<String> {
    let stack = [session_dir.to_path_buf()];
    let mut pending = std::collections::VecDeque::from(stack);
    // Bounded: a session directory holds at most a few thousand files, and a
    // symlink loop would otherwise spin forever.
    let mut budget = 10_000usize;
    while let Some(dir) = pending.pop_front() {
        let entries = std::fs::read_dir(&dir).ok()?;
        for entry in entries.flatten() {
            if budget == 0 {
                return None;
            }
            budget -= 1;
            let path = entry.path();
            if path.is_dir() {
                pending.push_back(path);
                continue;
            }
            if !path.to_string_lossy().ends_with(".jsonl")
                || path.to_string_lossy().ends_with(".progress.jsonl")
            {
                continue;
            }
            if let Some(id) = session_id_if_named(&path, name) {
                return Some(id);
            }
        }
    }
    None
}

/// The id of the session in `path`, if its name is `name`.
///
/// Line one is the header and carries the id; the name is an early `fact`
/// entry. A bounded scan keeps a long session from being read to the end, and
/// a file whose name never appears is not this session.
fn session_id_if_named(path: &Path, name: &str) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut id = None;
    let mut matched = false;
    for line in reader.lines().take(64) {
        let line = line.ok()?;
        let value: serde_json::Value = serde_json::from_str(&line).ok()?;
        if id.is_none() {
            id = value
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
        }
        if value.get("kind").and_then(serde_json::Value::as_str) == Some("fact")
            && value.get("fact").and_then(serde_json::Value::as_str) == Some("name")
        {
            if value.get("name").and_then(serde_json::Value::as_str) != Some(name) {
                return None;
            }
            matched = true;
            break;
        }
    }
    // A session with no name fact belongs to nobody, so it is not a match.
    if !matched {
        return None;
    }
    id
}

/// [`child_command`] with a choice of session handling.
///
/// `session` names the worker's saved session. The host refuses `--session` for
/// a name it has not seen, so this looks the name up first: a session that
/// exists is **resumed** by id, and a new one is **created** by name. Either
/// way the worker keeps its history across delegations, which is what lets a
/// Ticket be continued instead of rebuilt — DeepSeek's prototype does the same
/// (`worker-<ticket id>`, `src/executor/dsh.ts`).
///
/// `session_dir` says where saved sessions live. It is passed explicitly rather
/// than left to the default because the host's default is the *parent's project*
/// `.rpi/sessions/`, and a worker would then leave its history in the user's
/// repository.
///
/// With `session` unset the child stays ephemeral (`--no-session`), which is the
/// old behaviour: one delegation, nothing written.
///
/// The saved-session cost is small — a measured 20-message session is 6.9 KB, not
/// the ~2 MB an earlier note in this file guessed. A host opens one session for a
/// Ticket, not one per delegation.
// The argument list mirrors the child's flags plus the session choice; a struct
// would mean rewriting every caller and every test for one more field.
#[allow(clippy::too_many_arguments)]
pub fn child_command_with_session(
    exe: &Path,
    max_turns: u64,
    provider: Option<&str>,
    model: Option<&str>,
    thinking: Option<&str>,
    cwd: Option<&Path>,
    session: Option<&str>,
    session_dir: Option<&Path>,
    tier: u32,
) -> Command {
    let mut command = Command::new(exe);
    command.arg("-p");
    match session.filter(|value| !value.is_empty()) {
        Some(session) => {
            if let Some(dir) = session_dir {
                command.arg("--session-dir").arg(dir);
                // Resume when the name is known, create when it is not. The
                // host refuses `--session` for an unknown name, so this is a
                // lookup, not a fallback.
                match find_session_id(dir, session) {
                    Some(id) if !id.trim().is_empty() => command.arg("--session").arg(id),
                    // An empty id is not a session to resume: passing an empty
                    // --session makes the host fail the lookup and the worker
                    // never starts, so fall through to creating one.
                    _ => command.arg("--name").arg(session),
                };
            } else {
                // No directory to search: the name cannot be resolved, so it is
                // used as a name (create).
                command.arg("--name").arg(session);
            }
        }
        // Ephemeral: one delegation, no saved history.
        None => {
            command.arg("--no-session");
        }
    }
    command
        // Recursion guard: the child must not offer `subagent`/`plan`/`dispatch`.
        .arg("--exclude-tools")
        .arg(EXCLUDED_TOOLS);
    if let Some(provider) = provider.filter(|value| !value.is_empty()) {
        command.arg("--provider").arg(provider);
    }
    if let Some(model) = model.filter(|value| !value.is_empty()) {
        command.arg("--model").arg(model);
    }
    if let Some(thinking) = thinking.filter(|value| !value.is_empty()) {
        command.arg("--thinking").arg(thinking);
    }
    // The flags above depend on a host query that can fail silently. Repeating
    // the same binding as environment variables gives `rpi` a second channel, so
    // one failed query no longer demotes the worker to settings.json's default
    // model — a different account, and for this provider possibly an exhausted
    // one. Explicit flags still win inside `rpi`: these are fills, not overrides.
    if let Some(provider) = provider.filter(|value| !value.is_empty()) {
        command.env(ENV_PROVIDER, provider);
    }
    if let Some(model) = model.filter(|value| !value.is_empty()) {
        command.env(ENV_MODEL, model);
    }
    if let Some(thinking) = thinking.filter(|value| !value.is_empty()) {
        command.env(ENV_THINKING, thinking);
    }
    // A worker must never carry the Dash bridge. It runs inside the parent's
    // tmux pane, so it inherits `PI_DINGO_BRIDGE=on` plus the parent's URL and
    // token — and would then register against Dash **as the parent**, evicting
    // the parent's bridge (observed 2026-11-07). Dash guards against it too; the
    // root fix is here.
    for name in BRIDGE_ENV {
        command.env_remove(name);
    }
    command
        // Hard turn ceiling enforced by the harness, not the model.
        .env(MAX_TURNS_ENV, max_turns.to_string())
        .env(SUBAGENT_ENV, "1")
        // One deeper than the wave being run: this worker's own output, if it
        // queues more work, belongs to the next generation.
        .env(TIER_ENV, tier.saturating_add(1).to_string())
        // A node that asked for isolation works in its own directory, so it
        // cannot overwrite what another node is writing.
        .current_dir(cwd.unwrap_or_else(|| Path::new(".")))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group, so cancelling can kill the child *and its descendants*
    // instead of orphaning whatever it spawned.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
}

/// The failure text for a child that exited non-zero, naming the binding it ran
/// on. Split out so the shape is testable on every platform.
pub(crate) fn failure_message(
    code: Option<i32>,
    provider: Option<&str>,
    model: Option<&str>,
    text: &str,
) -> String {
    let selection = crate::host::Selection {
        provider: provider.map(str::to_string),
        model: model.map(str::to_string),
        thinking: None,
        exe: None,
    };
    format!(
        "subagent exited with {} (selection={}):\n{}",
        code.unwrap_or(-1),
        selection.label(),
        text
    )
}

/// Spill a worker's full output to disk and keep a summary in the window.
///
/// ## Why it spills instead of truncating
///
/// The old version dropped everything past [`MAX_OUTPUT_BYTES`] — the marker was
/// the only trace, and the bytes were gone. That made "record what the worker
/// did" impossible, because the data was destroyed before anything could record
/// it. It also left nothing for a caller to point at.
///
/// This is the same shape `rpi`'s own `bash` tool uses
/// (`rpi-tools/src/tools/bash.rs::build_bash_output`): keep a bounded head in the
/// window and put the full text in a file, naming the path in the marker. The
/// model can then read the file when it actually needs the rest, instead of
/// paying for those tokens on every turn whether it needs them or not.
///
/// ## Where
///
/// `~/.pidd/node-output/<task>/<node>.txt`. Under the Dash home rather than the
/// project: this is Dash's debug artifact, not the user's source, and putting it
/// in the repository would dirty `git status` for something nobody authored.
///
/// Returns the summary plus the path when it spilled, and the text unchanged
/// when it did not.
pub fn spill_output(text: String, task: &str, node: &str) -> (String, Option<String>) {
    if text.len() <= MAX_OUTPUT_BYTES {
        return (text, None);
    }
    let total_lines = text.lines().count();
    let total_bytes = text.len();
    let _ = total_bytes;
    let path = match spill_path(task, node) {
        Some(path) => path,
        // No home directory to spill into: fall back to the old truncation rather
        // than handing the model an unbounded string.
        None => {
            let mut clipped = text;
            let mut end = MAX_OUTPUT_BYTES;
            while end > 0 && !clipped.is_char_boundary(end) {
                end -= 1;
            }
            clipped.truncate(end);
            clipped.push_str("\n[subagent output truncated; no spill directory]");
            return (clipped, None);
        }
    };
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            let mut clipped = text;
            let mut end = MAX_OUTPUT_BYTES.min(clipped.len());
            while end > 0 && !clipped.is_char_boundary(end) {
                end -= 1;
            }
            clipped.truncate(end);
            clipped.push_str(&format!(
                "\n[subagent output truncated; could not write {}]",
                path.display()
            ));
            return (clipped, None);
        }
    }
    if std::fs::write(&path, text.as_bytes()).is_err() {
        let mut clipped = text;
        let mut end = MAX_OUTPUT_BYTES.min(clipped.len());
        while end > 0 && !clipped.is_char_boundary(end) {
            end -= 1;
        }
        clipped.truncate(end);
        clipped.push_str("\n[subagent output truncated; write failed]");
        return (clipped, None);
    }

    // Cut on a char boundary: slicing at a byte offset can land inside a
    // multi-byte char and panic. The old truncation had this guard; the spill
    // path must keep it.
    let mut cut = MAX_OUTPUT_BYTES.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let kept = &text[..cut];
    let shown_lines = kept.lines().count();
    let mut summary = kept.to_string();
    summary.push_str(&format!(
        "\n\n[Showing lines 1-{shown_lines} of {total_lines} ({size} limit). \
Full output: {display}]",
        size = human_bytes(MAX_OUTPUT_BYTES),
        display = path.display()
    ));
    (summary, Some(path.to_string_lossy().to_string()))
}

/// Where a spilled worker output lives.
fn spill_path(task: &str, node: &str) -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let safe = |value: &str| -> String {
        value
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect()
    };
    let task = safe(task.trim());
    let node = safe(node.trim());
    if task.is_empty() || node.is_empty() {
        return None;
    }
    Some(
        std::path::PathBuf::from(home)
            .join(".pidd")
            .join("node-output")
            .join(task)
            .join(format!("{node}.txt")),
    )
}

/// Render a byte count the way the truncation markers do.
fn human_bytes(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{}KB", bytes / 1024)
    }
}

/// Collect a reader's bounded output, giving up at `deadline` instead of
/// blocking on a descendant that still holds the pipe.
pub(crate) fn collect_reader(
    reader: Option<(Receiver<String>, JoinHandle<()>)>,
    deadline: Instant,
    control: &Control,
) -> Result<String, String> {
    control.check()?;
    let Some((receiver, _handle)) = reader else {
        return Ok(String::new());
    };
    loop {
        control.check()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("subagent timed out while collecting output".into());
        }
        match receiver.recv_timeout(remaining.min(POLL_SLICE)) {
            Ok(text) => return Ok(text),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(String::new()),
        }
    }
}

/// Run one delegation using the current executable as the child `rpi`.
#[allow(clippy::too_many_arguments)]
pub fn run(
    task: &str,
    max_turns: u64,
    timeout_secs: u64,
    provider: Option<&str>,
    model: Option<&str>,
    control: &Control,
) -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("cannot resolve the rpi executable: {err}"))?;
    run_with_exe(
        &exe,
        task,
        max_turns,
        timeout_secs,
        provider,
        model,
        control,
    )
}

/// Run one delegation with an explicit executable (test seam).
#[allow(clippy::too_many_arguments)]
pub fn run_with_exe(
    exe: &Path,
    task: &str,
    max_turns: u64,
    timeout_secs: u64,
    provider: Option<&str>,
    model: Option<&str>,
    control: &Control,
) -> Result<String, String> {
    run_with_exe_reporting(
        exe, task, max_turns, timeout_secs, provider, model, control, None, &mut |_| {},
    )
}

/// [`run_with_exe`], reporting the child's process id once it starts.
///
/// The id lets a caller find the child later — for example to clean up after
/// the host died and left it running.
// The argument list mirrors `run_with_exe` plus the callback; splitting it into
// a struct would mean rewriting every caller for one extra field.
#[allow(clippy::too_many_arguments)]
pub fn run_with_exe_reporting(
    exe: &Path,
    task: &str,
    max_turns: u64,
    timeout_secs: u64,
    provider: Option<&str>,
    model: Option<&str>,
    control: &Control,
    cwd: Option<&Path>,
    on_started: &mut dyn FnMut(u32),
) -> Result<String, String> {
    run_child_reporting(
        exe, task, "untitled", max_turns, timeout_secs, provider, model, None, control, cwd, None,
        None, 0, on_started,
    )
}

/// [`run_with_exe_reporting`] with a choice of saved session.
///
/// `session` names the worker's session, so the next call with the same name
/// resumes it; `session_dir` says where those sessions live. Both unset keeps
/// the child ephemeral.
#[allow(clippy::too_many_arguments)]
pub fn run_child_reporting(
    exe: &Path,
    task: &str,
    // Long-task id, used only to name the spill file.
    task_id: &str,
    max_turns: u64,
    timeout_secs: u64,
    provider: Option<&str>,
    model: Option<&str>,
    thinking: Option<&str>,
    control: &Control,
    cwd: Option<&Path>,
    session: Option<&str>,
    session_dir: Option<&Path>,
    tier: u32,
    on_started: &mut dyn FnMut(u32),
) -> Result<String, String> {
    // Fail before launching anything if already cancelled.
    control.check()?;
    let (mut child, tree) = spawn_command(&mut child_command_with_session(
        exe,
        max_turns,
        provider,
        model,
        thinking,
        cwd,
        session,
        session_dir,
        tier,
    ))
    .map_err(|err| format!("failed to start subagent `{}`: {err}", exe.display()))?;
    on_started(child.id());

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    // Drain stdout/stderr on their own threads so a chatty child cannot block on
    // a full pipe while we wait for it to exit. Each reader caps what it keeps
    // and signals completion through a channel, so collecting output can never
    // outwait the deadline: a descendant that inherits the pipe and outlives the
    // direct child must not extend the call.
    let out_reader = child.stdout.take().map(spawn_reader);
    let err_reader = child.stderr.take().map(spawn_reader);

    // Writing is part of the bounded lifecycle too: a child that never reads
    // stdin must not make the tool hang before the wait loop starts.
    write_stdin(child.stdin.take(), task);

    // Wait in slices so cancellation and the wall-clock deadline are both observed
    // while the child is still running.
    loop {
        if control.is_cancelled() {
            kill_and_reap(&mut child, &tree);
            return Err("subagent cancelled".into());
        }
        if Instant::now() >= deadline {
            kill_and_reap(&mut child, &tree);
            return Err(format!("subagent timed out after {timeout_secs}s"));
        }
        let slice = deadline
            .saturating_duration_since(Instant::now())
            .min(POLL_SLICE);
        match child.wait_timeout(slice) {
            Ok(Some(status)) => {
                let mut text = match collect_reader(out_reader, deadline, control) {
                    Ok(text) => text,
                    Err(error) => {
                        kill_and_reap(&mut child, &tree);
                        return Err(error);
                    }
                };
                let stderr_text = match collect_reader(err_reader, deadline, control) {
                    Ok(text) => text,
                    Err(error) => {
                        kill_and_reap(&mut child, &tree);
                        return Err(error);
                    }
                };
                if !status.success() && !stderr_text.trim().is_empty() {
                    text.push_str("\n[stderr]\n");
                    text.push_str(stderr_text.trim_end());
                }
                // Spill before truncating: the full output goes to disk so a caller
                // can read it later, and only the summary stays in the window.
                // The node name is the engine-assigned session name (see
                // `engine_session_name`), task is the long-task id.
                let (text, _spill) = spill_output(text, task_id, session.filter(|v| !v.is_empty()).unwrap_or("ephemeral"));
                if !status.success() {
                    // Name the binding in the failure: without it a `No API key` or
                    // `No model matches` from the child is unattributable, and the
                    // caller can only guess again.
                    return Err(failure_message(status.code(), provider, model, &text));
                }
                return Ok(text);
            }
            Ok(None) => continue,
            Err(err) => {
                kill_and_reap(&mut child, &tree);
                return Err(format!("failed while waiting for subagent: {err}"));
            }
        }
    }
}

/// Start under an owned process tree before any user code can spawn children.
/// Windows keeps the job handle alive even after the direct child exits.
pub(crate) fn spawn_command(command: &mut Command) -> Result<(Child, ProcessTree), String> {
    ProcessTree::prepare(command);
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    match ProcessTree::attach(&child) {
        Ok(tree) => Ok((child, tree)),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
}

pub(crate) fn kill_and_reap(child: &mut Child, tree: &ProcessTree) {
    // Walk while the parent is still attached: Unix descendants can put
    // themselves in another group. The owned group/job also covers descendants
    // whose direct parent already exited while we were collecting output.
    #[cfg(unix)]
    kill_descendants(child.id());
    tree.kill();
    let _ = child.kill();
    let _ = child.wait();
}

/// Kill a process and everything it spawned.
///
/// The child is signalled directly rather than through its process group:
/// a group kill needs the caller to be a member of that group, and an orphan
/// left by a dead host belongs to a group nobody else is in. Signalling the
/// pid works from anywhere, for the same user.
///
/// The descendants are killed first, because the child's own death would
/// reparent them and hide them from the parent lookup.
pub(crate) fn kill_process_tree(pid: u32) {
    #[cfg(unix)]
    {
        kill_descendants(pid);
        let _ = std::process::Command::new("kill")
            .arg("-KILL")
            .arg(pid.to_string())
            .status();
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .status();
    }
}

/// Whether a process is still running.
///
/// A zombie counts as gone: it has finished, and only the bookkeeping entry
/// remains. `kill -0` reports a zombie as present, so the state is checked
/// too — otherwise a dead child would look alive forever.
pub(crate) fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let alive = std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !alive {
            return false;
        }
        // A zombie is finished work: `ps` reports it as `Z`.
        let state = std::process::Command::new("ps")
            .args(["-o", "state=", "-p", &pid.to_string()])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_default();
        !state.starts_with('Z')
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}")])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
}

/// Best-effort recursive SIGKILL of `pid` and everything below it (unix).
#[cfg(unix)]
pub(crate) fn kill_descendants(pid: u32) {
    if let Ok(output) = std::process::Command::new("pgrep")
        .arg("-P")
        .arg(pid.to_string())
        .output()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Ok(child) = line.trim().parse::<u32>() {
                kill_descendants(child);
            }
        }
    }
    let _ = std::process::Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status();
}

fn write_stdin(mut stdin: Option<std::process::ChildStdin>, task: &str) {
    // A child that never reads (or exits first) must not block the caller, so the
    // write happens on its own thread; EPIPE is not fatal — the exit status is
    // what decides success. Dropping the handle closes the pipe so the child
    // observes EOF.
    let task = task.to_owned();
    std::thread::spawn(move || {
        if let Some(stdin) = stdin.as_mut() {
            let _ = stdin.write_all(task.as_bytes());
            let _ = stdin.flush();
        }
    });
}

/// Read a child's stream with a hard cap, signalling completion over a channel so
/// the caller can apply its own deadline. The thread keeps draining after the cap
/// so the child never blocks on a full pipe, and stops as soon as the child (and
/// any descendant that inherited the pipe) closes it.
pub(crate) fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
) -> (Receiver<String>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut buf = [0u8; 8192];
        let mut truncated = false;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(count) => {
                    if output.len() < MAX_OUTPUT_BYTES {
                        let keep = count.min(MAX_OUTPUT_BYTES - output.len());
                        output.extend_from_slice(&buf[..keep]);
                        truncated |= keep < count;
                    } else {
                        truncated = true;
                    }
                }
                Err(_) => break,
            }
        }
        let mut text = String::from_utf8_lossy(&output).into_owned();
        if truncated {
            text.push_str("\n[output truncated]");
        }
        let _ = sender.send(text);
    });
    (receiver, handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    static PROCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A fresh directory under the system temp dir, holding sessions for one
    /// test. Named per test so two runs do not share one.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpi-superagent-sessions-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn collect_after_parent_exit(cancel: bool) {
        let _serial = PROCESS_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let pid_file = std::env::temp_dir().join(format!(
            "rpi-superagent-pipes-{}-{}.pid",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("powershell.exe");
            command.args(["-NoProfile", "-NonInteractive", "-Command",
                "$i = New-Object System.Diagnostics.ProcessStartInfo; $i.FileName = 'ping.exe'; $i.Arguments = '-n 61 127.0.0.1'; $i.UseShellExecute = $false; $i.CreateNoWindow = $true; $p = [System.Diagnostics.Process]::Start($i); [System.IO.File]::WriteAllText($env:RPI_TEST_PID_FILE, [string]$p.Id)"]);
            command
        };
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-c", "sleep 60 & echo $! > \"$RPI_TEST_PID_FILE\""]);
            command
        };
        command
            .env("RPI_TEST_PID_FILE", &pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (mut parent, tree) = spawn_command(&mut command).unwrap();
        let stdout = parent.stdout.take().map(spawn_reader);
        let stderr = parent.stderr.take().map(spawn_reader);
        let exited = parent
            .wait_timeout(Duration::from_secs(15))
            .unwrap()
            .is_some();
        if !exited {
            kill_and_reap(&mut parent, &tree);
        }
        assert!(exited, "fixture parent did not exit");
        let descendant: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let _ = std::fs::remove_file(pid_file);

        let control = Control::default();
        let cancel_handle = if cancel {
            let canceller = control.clone();
            Some(std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                canceller.cancel();
            }))
        } else {
            None
        };
        let start = Instant::now();
        let budget = if cancel {
            Duration::from_secs(10)
        } else {
            Duration::from_millis(150)
        };
        let result = collect_reader(stdout, start + budget, &control);
        let elapsed = start.elapsed();
        if let Some(handle) = cancel_handle {
            handle.join().unwrap();
        }
        // The direct parent is already reaped; its job/group must still kill
        // the descendant and close both inherited pipes.
        kill_and_reap(&mut parent, &tree);
        let drained = collect_reader(
            stderr,
            Instant::now() + Duration::from_secs(3),
            &Control::default(),
        );
        #[cfg(windows)]
        let survived = rpi_plugin_sdk::process::process_is_running(descendant).unwrap();
        #[cfg(unix)]
        let _ = descendant;
        assert!(
            drained.is_ok(),
            "descendant kept the stderr pipe open: {drained:?}"
        );
        #[cfg(windows)]
        assert!(!survived, "descendant survived after its parent exited");
        let expected = if cancel { "cancelled" } else { "timed out" };
        assert!(result.unwrap_err().contains(expected));
        assert!(
            elapsed < Duration::from_secs(2),
            "collect ignored cancellation/deadline: {elapsed:?}"
        );
    }

    #[test]
    fn output_collection_observes_cancellation_after_parent_exit() {
        collect_after_parent_exit(true);
    }

    #[test]
    fn output_timeout_cleans_descendants_after_parent_exit() {
        collect_after_parent_exit(false);
    }

    #[cfg(windows)]
    #[test]
    fn kill_and_reap_terminates_windows_descendants() {
        let _serial = PROCESS_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use std::os::windows::process::CommandExt;
        const NO_WINDOW: u32 = 0x08000000;
        let pid_file = std::env::temp_dir().join(format!(
            "rpi-superagent-child-{}-{}.pid",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Pass the path as an environment value, not interpolated shell code.
        let mut command = Command::new("powershell.exe");
        command
            .args(["-NoProfile", "-NonInteractive", "-Command",
                "$ErrorActionPreference = 'Stop'; $i = New-Object System.Diagnostics.ProcessStartInfo; $i.FileName = 'ping.exe'; $i.Arguments = '-n 61 127.0.0.1'; $i.UseShellExecute = $false; $i.CreateNoWindow = $true; $p = [System.Diagnostics.Process]::Start($i); [System.IO.File]::WriteAllText($env:RPI_TEST_PID_FILE, ([string]$p.Id + [Environment]::NewLine)); Start-Sleep -Seconds 60"])
            .env("RPI_TEST_PID_FILE", &pid_file)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped())
            .creation_flags(NO_WINDOW);
        let (mut parent, tree) = spawn_command(&mut command).expect("start parent");
        let diagnostics = parent.stderr.take().map(spawn_reader);
        let deadline = Instant::now() + Duration::from_secs(15);
        let descendant = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .filter(|text| text.ends_with('\n'))
                .and_then(|text| text.trim().parse::<u32>().ok())
            {
                break Some(pid);
            }
            if parent.try_wait().unwrap().is_some() || Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        fn alive(pid: u32) -> bool {
            rpi_plugin_sdk::process::process_is_running(pid).expect("query descendant process")
        }
        let was_alive = descendant.is_some_and(alive);
        kill_and_reap(&mut parent, &tree);
        let survived = descendant.is_some_and(alive);
        // Clean up even when the regression fails.
        if survived {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &descendant.unwrap().to_string()])
                .creation_flags(NO_WINDOW)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = std::fs::remove_file(pid_file);
        let diagnostics = collect_reader(
            diagnostics,
            Instant::now() + Duration::from_secs(3),
            &Control::default(),
        )
        .unwrap_or_else(|error| error);
        assert!(was_alive, "test descendant did not start: {diagnostics}");
        assert!(!survived, "cancellation left the descendant running");
        assert!(parent.try_wait().unwrap().is_some());
    }

    #[test]
    fn child_command_sets_recursion_guard_and_turn_budget() {
        let command = child_command(Path::new("/usr/bin/rpi"), 7, None, None, None);
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec!["-p", "--no-session", "--exclude-tools", EXCLUDED_TOOLS]
        );

        let envs: Vec<(String, String)> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value
                        .map(|value| value.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            envs,
            vec![
                // The bridge switches are explicitly **cleared** (I6). A `Command`
                // inherits the parent's environment, and the parent runs inside a
                // tmux pane that carries `PI_DINGO_BRIDGE=on` plus its URL and
                // token. Inherited, the worker would load `pidingo.dylib` and
                // register against Dash **with the parent's token**, repeatedly
                // replacing the parent's bridge entry (observed 2026-11-07).
                ("PI_DINGO_BRIDGE".to_string(), String::new()),
                ("PI_DINGO_TOKEN".to_string(), String::new()),
                ("PI_DINGO_URL".to_string(), String::new()),
                (MAX_TURNS_ENV.to_string(), "7".to_string()),
                (SUBAGENT_ENV.to_string(), "1".to_string()),
                // A plan dispatched from a person's session is generation zero, so
                // its workers run at one.
                (TIER_ENV.to_string(), "1".to_string()),
            ]
        );
    }

    /// The excluded list is part of the child's prompt, so its order has to be a
    /// property of the set rather than of the edit that added a name. A different
    /// order is a different prefix, which loses the provider's prefix cache — a
    /// 30-50x difference on input for this provider.
    #[test]
    fn the_excluded_tools_are_sorted() {
        let names: Vec<&str> = EXCLUDED_TOOLS.split(',').collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(
            names, sorted,
            "EXCLUDED_TOOLS must be sorted, or the child's prompt changes for no reason"
        );
    }




    #[test]
    fn an_unknown_session_name_is_created_rather_than_resumed() {
        // The host refuses `--session` for a name it has never seen, so a new
        // worker must be created with `--name`; its history still lands in the
        // worker's own directory, not the user's repository.
        let dir = scratch_dir("unknown-session");
        let command = child_command_with_session(
            Path::new("/usr/bin/rpi"),
            5,
            None,
            None,
            None,
            None,
            Some("worker-t1"),
            Some(&dir),
            0,
        );
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "-p",
                "--session-dir",
                dir.to_str().unwrap(),
                "--name",
                "worker-t1",
                "--exclude-tools",
                EXCLUDED_TOOLS,
            ]
        );
        // The two modes are exclusive: naming a session must not also ask for an
        // ephemeral one, which the host would refuse.
        assert!(!args.iter().any(|arg| arg == "--no-session"));
    }

    #[test]
    fn a_known_session_name_is_resumed_by_id() {
        // Once the session exists, the next delegation resumes it by id — that
        // is what keeps a worker's history across turns.
        let dir = scratch_dir("known-session");
        let nested = dir.join("--project--");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("2026-01-01T00-00-00-000Z_abc.jsonl"),
            concat!(
                r#"{"kind":"header","id":"abc-123","cwd":"/p"}"#,
                "\n",
                r#"{"kind":"fact","seq":1,"fact":"name","name":"worker-t1"}"#,
                "\n",
            ),
        )
        .unwrap();
        // The progress file beside it must not be mistaken for a session.
        std::fs::write(nested.join("2026-01-01T00-00-00-000Z_abc.jsonl.progress.jsonl"), "").unwrap();

        let command = child_command_with_session(
            Path::new("/usr/bin/rpi"),
            5,
            None,
            None,
            None,
            None,
            Some("worker-t1"),
            Some(&dir),
            0,
        );
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "-p",
                "--session-dir",
                dir.to_str().unwrap(),
                "--session",
                "abc-123",
                "--exclude-tools",
                EXCLUDED_TOOLS,
            ]
        );
    }

    #[test]
    fn a_session_without_the_name_is_not_resumed() {
        // Another worker's session must not be adopted just because the
        // directory exists.
        let dir = scratch_dir("other-session");
        std::fs::write(
            dir.join("2026-01-01T00-00-00-000Z_other.jsonl"),
            concat!(
                r#"{"kind":"header","id":"other-id","cwd":"/p"}"#,
                "\n",
                r#"{"kind":"fact","seq":1,"fact":"name","name":"someone-else"}"#,
                "\n",
            ),
        )
        .unwrap();
        assert_eq!(find_session_id(&dir, "worker-t1"), None);
    }

    #[test]
    fn an_empty_session_name_stays_ephemeral() {
        // Callers pass "" for "no session"; that must not become `--session`.
        let command = child_command_with_session(
            Path::new("/usr/bin/rpi"),
            5,
            None,
            None,
            None,
            None,
            Some(""),
            None,
            0,
        );
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec!["-p", "--no-session", "--exclude-tools", EXCLUDED_TOOLS]
        );
    }

    #[test]
    fn child_command_passes_provider_and_model_through() {
        let command = child_command(
            Path::new("/usr/bin/rpi"),
            8,
            Some("deepseek"),
            Some("deepseek-flash"),
            None,
        );
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "-p",
                "--no-session",
                "--exclude-tools",
                EXCLUDED_TOOLS,
                "--provider",
                "deepseek",
                "--model",
                "deepseek-flash",
            ]
        );

        // Empty strings must be dropped rather than emitted as bare flags.
        let bare = child_command(Path::new("/usr/bin/rpi"), 8, Some(""), None, None);
        let bare_args: Vec<String> = bare
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            bare_args,
            vec!["-p", "--no-session", "--exclude-tools", EXCLUDED_TOOLS]
        );
    }

    #[test]
    fn an_inherited_parent_binding_reaches_the_child_command() {
        // A call that names neither provider nor model runs on this session's
        // binding, so the child must receive it explicitly: its own default
        // provider is not the session's and need not be authenticated at all.
        let parent = crate::host::Selection {
            provider: Some("routeryo-copy".into()),
            model: Some("gpt-6.1-sol".into()),
            thinking: None,
            exe: None,
        };
        let selection = parent.effective(None, None, None);
        let command = child_command(
            Path::new("/usr/bin/rpi"),
            8,
            selection.provider(),
            selection.model(),
            None,
        );
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--provider", "routeryo-copy"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--model", "gpt-6.1-sol"]));
    }

    #[test]
    fn a_failure_names_the_binding_that_was_used() {
        let named = failure_message(Some(2), Some("openai"), Some("gpt-6-astra"), "boom");
        assert!(named.contains("exited with 2"), "{named}");
        assert!(named.contains("selection=openai/gpt-6-astra"), "{named}");
        assert!(named.ends_with("boom"), "{named}");

        // A call that passed nothing (settings default) still reports its exit
        // code instead of looking like a crash without a cause.
        let unset = failure_message(None, None, None, "");
        assert!(unset.contains("exited with -1"), "{unset}");
        assert!(unset.contains("selection=-"), "{unset}");
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_child_names_the_attempted_selection() {
        // `No API key` / `No model matches` from the child used to surface with
        // no attribution at all, so the caller could only guess again.
        // A failing child is written to a temp script instead of a fixed path:
        // `/bin/false` does not exist on macOS, where `false` lives in /usr/bin.
        let script = std::env::temp_dir().join(format!("rpi-superagent-false-{}", std::process::id()));
        std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        let error = run_with_exe(
            &script,
            "task",
            1,
            5,
            Some("openai"),
            Some("gpt-6-astra"),
            &Control::default(),
        )
        .unwrap_err();
        let _ = std::fs::remove_file(&script);
        assert!(
            error.contains("selection=openai/gpt-6-astra"),
            "got: {error}"
        );
    }

    /// Short output passes through untouched — nothing is spilled for a small
    /// result, so the common case pays no file write.
    #[test]
    fn short_output_is_not_spilled() {
        let (text, path) = spill_output("ok".to_string(), "T1", "a");
        assert_eq!(text, "ok");
        assert_eq!(path, None);
    }

    /// Long output is written in full and only a bounded summary stays behind.
    ///
    /// This is the whole point of the change: the old version dropped everything
    /// past the limit, so "what did the worker actually produce" was
    /// unanswerable. The summary must name the file so a caller can go read it.
    #[test]
    fn long_output_spills_to_a_file_and_the_summary_names_it() {
        let long = "あ".repeat(MAX_OUTPUT_BYTES);
        let (summary, path) = spill_output(long.clone(), "T1", "node-a");
        let path = path.expect("long output must spill");
        assert!(path.contains("node-output"), "path was {path}");
        assert!(path.ends_with("/T1/node-a.txt"), "path was {path}");

        // The file holds the full text, byte for byte.
        let written = std::fs::read_to_string(&path).expect("spill file exists");
        assert_eq!(written.len(), long.len());

        // The summary is bounded, char-safe, and points at the file.
        assert!(summary.starts_with('あ'));
        assert!(summary.contains("Full output:"));
        assert!(summary.contains(&path));
        assert!(summary.len() < long.len());
        let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap().parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_kills_the_whole_process_group() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        // The child backgrounds a `sleep` (a grandchild) and records its pid, then
        // waits. Cancelling must kill the group, not just the direct child.
        let dir = std::env::temp_dir();
        let script = dir.join(format!("orch-group-{}.sh", std::process::id()));
        let pidfile = dir.join(format!("orch-group-{}.pid", std::process::id()));
        {
            let mut file = std::fs::File::create(&script).unwrap();
            writeln!(file, "#!/bin/sh").unwrap();
            writeln!(file, "sleep 30 &").unwrap();
            writeln!(file, "echo $! > {}", pidfile.display()).unwrap();
            writeln!(file, "wait").unwrap();
        }
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let control = Control::default();
        let canceller = control.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            canceller.cancel();
        });
        let _ = run_with_exe(&script, "ignored", 8, 30, None, None, &control);
        let _ = handle.join();

        let grandchild = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok());
        std::thread::sleep(Duration::from_millis(300));
        if let Some(pid) = grandchild {
            let alive = std::process::Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            assert!(!alive, "grandchild {pid} survived the process-group kill");
        }
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&pidfile);
    }

    #[test]
    fn cancelled_control_aborts_the_call() {
        // A pre-cancelled control aborts before any child is launched, so this
        // holds even where the fake executable path does not exist.
        let control = Control::default();
        control.cancel();
        let err = run_with_exe(
            Path::new("/nonexistent/superagent-child"),
            "ignored",
            8,
            5,
            None,
            None,
            &control,
        )
        .unwrap_err();
        assert!(err.contains("cancelled"), "got: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_kills_a_running_child() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        // A stand-in child that ignores our args and blocks, so the wait loop has
        // something to kill. `exec` makes the script process become the sleeper,
        // so killing the direct child leaves no orphan.
        let script = std::env::temp_dir().join(format!("orch-cancel-{}.sh", std::process::id()));
        {
            let mut file = std::fs::File::create(&script).unwrap();
            writeln!(file, "#!/bin/sh").unwrap();
            writeln!(file, "exec sleep 30").unwrap();
        }
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let control = Control::default();
        let canceller = control.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            canceller.cancel();
        });
        let start = Instant::now();
        let err = run_with_exe(&script, "ignored", 8, 30, None, None, &control).unwrap_err();
        let elapsed = start.elapsed();
        let _ = handle.join();
        let _ = std::fs::remove_file(&script);

        assert!(err.contains("cancelled"), "got: {err}");
        assert!(
            elapsed < Duration::from_secs(10),
            "cancel took too long: {elapsed:?}"
        );
    }
}
