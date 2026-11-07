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

use std::io::Read;
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
    // `--mode rpc`, not `-p`. A one-shot `-p` run has **no way to answer a
    // question**: the extension `UiDialog` mailbox is never bridged to anything,
    // so a worker that asks sits there until the wall-clock deadline kills it
    // (the default is 300 s, and the failure looks like "the worker hung").
    //
    // rpc bridges that mailbox over stdin/stdout instead: the host writes
    // `{"type":"ui_dialog_request",…}` and accepts `{"type":"ui_dialog_response"}`
    // back. That is also what makes the worker steerable (SetModel / Abort) by a
    // caller later, which `-p` cannot do at all.
    command.arg("--mode").arg("rpc");
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
    // stdout is a JSONL event stream (rpc); stderr stays raw text.
    //
    // The question hook needs stdin, which is created below — so it is installed
    // after `stdin` exists. The reader thread starts first so no early question is
    // missed; questions that arrive before the hook is wired are still collected in
    // the outcome (they just cannot be auto-answered).
    let (question_tx, question_rx) = mpsc::channel::<Question>();
    let out_reader = child.stdout.take().map(|stdout| {
        spawn_event_reader_with(stdout, move |question| {
            // Best-effort: if the main loop is gone, the worker is gone too.
            let _ = question_tx.send(question);
        })
    });
    let err_reader = child.stderr.take().map(spawn_reader);

    // **stdin stays open.** In rpc mode the channel is bidirectional: a worker that
    // asks a question writes `ui_dialog_request` and then blocks reading stdin for
    // the answer. The old code did `child.stdin.take()` and dropped the handle after
    // the prompt, which closes the pipe — so even a handler that wanted to answer
    // could not, and the worker waited until the wall clock killed it.
    let mut stdin = child.stdin.take();
    write_line(&mut stdin, &prompt_frame(task));

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
        // Answer whatever the worker asked while we wait. This is the whole point
        // of keeping stdin open: a question that is not answered here becomes a
        // worker that dies at the deadline with its work unfinished.
        while let Ok(question) = question_rx.try_recv() {
            match auto_answer(&question.ui) {
                Some(answer) => {
                    write_line(&mut stdin, &dialog_response_frame(&question.request_id, &answer));
                }
                None => {
                    // Not ours to decide. Unblock the worker with a cancellation
                    // rather than letting it hang: it will finish its turn and
                    // report the refusal, which is far more useful than a timeout.
                    write_line(
                        &mut stdin,
                        &serde_json::json!({
                            "type": "ui_dialog_response",
                            "requestId": question.request_id,
                            "cancelled": true,
                        })
                        .to_string(),
                    );
                }
            }
        }
        let slice = deadline
            .saturating_duration_since(Instant::now())
            .min(POLL_SLICE);
        match child.wait_timeout(slice) {
            Ok(Some(status)) => {
                let outcome = match collect_outcome(out_reader, deadline, control) {
                    Ok(outcome) => outcome,
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
                let mut text = outcome.reply;
                // Questions nobody answered are **carried out, not dropped**.
                // Before `--mode rpc` these hung the worker until the deadline;
                // now the caller at least learns what was asked, so a person can
                // answer it on the next node instead of guessing why it stalled.
                if !outcome.questions.is_empty() {
                    text.push_str(&format!(
                        "\n\n[worker asked {} question(s), unanswered]\n- {}",
                        outcome.questions.len(),
                        outcome.questions.join("\n- ")
                    ));
                }
                if !status.success() && !stderr_text.trim().is_empty() {
                    text.push_str("\n[stderr]\n");
                    text.push_str(stderr_text.trim_end());
                }
                // Spill before truncating: the full output goes to disk so a caller
                // can read it later, and only the summary stays in the window.
                // The node name is the engine-assigned session name (see
                // `engine_session_name`), task is the long-task id.
                let (text, _spill) = spill_output(
                    text,
                    task_id,
                    session.filter(|v| !v.is_empty()).unwrap_or("ephemeral"),
                );
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

/// The one prompt frame an `rpc` worker receives, as a single JSONL line.
///
/// `-p` takes the prompt as raw stdin text; `--mode rpc` takes it as a `prompt`
/// *frame*. Sending raw text to an rpc host is not an error it reports — the
/// line is simply not a frame, so the worker sits waiting for a prompt that never
/// arrives and is finally killed by the wall clock. That is why this is a frame
/// builder and not a `format!` at the call site.
///
/// `content` is the accepted legacy form (`PromptPayload::Legacy`).
pub(crate) fn prompt_frame(task: &str) -> String {
    serde_json::json!({
        "type": "prompt",
        "content": task,
    })
    .to_string()
}

/// One question a worker asked mid-run, as it arrives.
///
/// Carried separately from [`RpcOutcome`] because it must be delivered **while the
/// worker is still blocked**, not collected for the final result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub request_id: String,
    /// The raw `ui` payload, for [`auto_answer`].
    pub ui: serde_json::Value,
}

/// What an `rpc` worker's stdout yielded, after folding its event stream.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RpcOutcome {
    /// The last turn's assistant text — what the worker ended up saying.
    pub reply: String,
    /// Questions the worker asked and nobody answered (`ui_dialog_request`
    /// without a matching close). Carried out so they are not silently lost.
    pub questions: Vec<String>,
    /// Whether any `turn_end` or `agent_end` arrived, i.e. the worker actually ran.
    pub finished: bool,
}

/// Fold one JSONL event line from an `rpc` worker.
///
/// Returns `false` for lines that are not events (`ready`, responses to our own
/// requests) so the caller can ignore them without treating them as garbage.
///
/// Kept pure and separate from the wait loop: the shape of these frames is the
/// contract with `rpi`, and a unit test is the only thing that will notice when
/// upstream renames a field.
pub fn fold_rpc_event(line: &str, out: &mut RpcOutcome) -> bool {
    fold_rpc_event_with(line, out, &mut |_| {})
}

/// [`fold_rpc_event`] plus a hook that fires for each question as it is parsed.
pub fn fold_rpc_event_with(
    line: &str,
    out: &mut RpcOutcome,
    on_question: &mut dyn FnMut(Question),
) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
        return false;
    };
    let Some(kind) = value.get("type").and_then(serde_json::Value::as_str) else {
        return false;
    };
    match kind {
        // The final assistant message of a turn. `message` mirrors the session
        // entry shape, so the text is in `content[]`.
        "turn_end" | "agent_end" => {
            if let Some(message) = value.get("message") {
                let text = message_text(message);
                if !text.is_empty() {
                    out.reply = text;
                }
                out.finished = true;
            } else if kind == "agent_end" {
                // `agent_end` carries `messages` (plural) instead of one message.
                if let Some(messages) = value.get("messages").and_then(|v| v.as_array()) {
                    if let Some(last) = messages
                        .iter()
                        .rev()
                        .find_map(|m| {
                            let t = message_text(m);
                            if t.is_empty() { None } else { Some(t) }
                        })
                    {
                        out.reply = last;
                    }
                }
                out.finished = true;
            }
        }
        // A question with no one to answer it. Collected, never discarded:
        // dropping it is what left workers hanging until the timeout.
        "ui_dialog_request" => {
            let id = value
                .get("requestId")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            // Deliver it now: the worker is blocked on this question, and the
            // answer has to go back on stdin before it will move again.
            if !id.is_empty() {
                on_question(Question {
                    request_id: id.to_string(),
                    ui: value.get("ui").cloned().unwrap_or(serde_json::Value::Null),
                });
            }
            let question = value
                .get("ui")
                .map(describe_ui)
                .unwrap_or_else(|| "(no payload)".into());
            out.questions.push(format!("requestId={id} {question}"));
        }
        // A question that got cancelled; nothing to report.
        "ui_dialog_closed" => {}
        _ => {}
    }
    true
}

/// Pull the text out of a message payload (`content: [{type:"text",text}]`).
fn message_text(message: &serde_json::Value) -> String {
    match message.get("content") {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| {
                let ty = block.get("type").and_then(serde_json::Value::as_str)?;
                // Only text blocks: a tool call's arguments are not an answer.
                if ty != "text" {
                    return None;
                }
                block
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// What the engine can answer on a worker's behalf.
///
/// ## Why an answerer exists at all
///
/// A worker that stops to ask has two possible futures: it waits (and dies on the
/// wall clock, wasting the run), or something answers. The orchestrating **model**
/// cannot answer — this whole call is synchronous and the model is the party
/// blocked inside it. So either the engine answers a narrow class of questions, or
/// nobody does.
///
/// ## What it is allowed to answer
///
/// Only questions whose answer is **already decided by the node's own acceptance
/// criteria** — the "may I proceed" / "is this good enough" family. Those are
/// re-statements of what the plan already said, so answering them cannot change
/// the goal. Everything else (which library, what to delete, which of two designs)
/// changes the outcome and must go back to the caller as an unanswered question.
///
/// This is deliberately narrow. A wrong auto-answer is worse than a failed node:
/// the node reports success, and nobody looks at it again.
pub(crate) fn auto_answer(ui: &serde_json::Value) -> Option<String> {
    // Only confirmation-style dialogs have a safe answer. A `select` between
    // options is a decision, and a `text` prompt wants content we do not have.
    let kind = ui.get("kind").and_then(serde_json::Value::as_str).unwrap_or("");
    let question = ui
        .get("message")
        .or_else(|| ui.get("question"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();

    // The "may I continue / is this good enough" family. Matched on the question
    // text because that is all a dialog gives us.
    //
    // ⚠️ **Both languages.** A worker writes its question in whatever language the
    // task was written in, and this project's tasks are mostly Chinese. Matching
    // only English would mean every Chinese task's proceed-question falls through
    // to `cancelled` — the worker stops, the node is recorded failed, and the only
    // visible cause is "it asked something". That is a silent extra run, not an
    // error, which makes it easy to never notice.
    //
    // Chinese has no word boundaries, so `contains` is already the right test —
    // but it also means a short word can match inside a longer unrelated one.
    // Everything here is a full phrase for that reason (「继续吗」 rather than
    // 「继续」, which would also match 「不要继续」).
    const PROCEED: &[&str] = &[
        // English
        "continue",
        "proceed",
        "go ahead",
        "shall i proceed",
        "may i proceed",
        "is that ok",
        "is this ok",
        "are you ready",
        "shall i continue",
        // 中文（带「吗 / 么 / 呢」的是问句；不带的是祈使句，不在此列）
        "可以继续",
        "能否继续",
        "是否继续",
        "继续吗",
        "继续么",
        "要继续",
        "可以吗",
        "行吗",
        "对吗",
        "好不好",
        "是否可以",
        "是否可行",
        "可以开始",
        "就这样",
        "没问题吧",
    ];
    // ⚠️ **否定词优先判定**：中文没有词边界，「不要继续」里含「要继续」，
    // 只看 PROCEED 会把它答成 yes —— 恰好是相反的意思。
    //
    // 所以先扫否定词，命中就直接拒绝代答（交给 cancelled）。代价是
    // 「不要紧，继续吧」这种双重否定也会被拒，那没什么损失：多一次节点运行，
    // 比答反了强 —— 答反了节点会报成功，没有人再回头看它。
    const NEGATIONS: &[&str] = &[
        // 中文
        "不要",
        "不用",
        "不必",
        "不需要",
        "不能",
        "不可",
        "不行",
        "禁止",
        "停止",
        "取消",
        // ⚠️ 不要加「别」：它太短，会误伤「别的问题没有了，可以继续吗」
        // 「这个方案别处见过」这类正常问句。宁可放过一个真否定（多一次节点
        // 运行），也不要让常见问句永远答不上。
        // English
        "don't",
        "do not",
        "not yet",
        "stop",
        "cancel",
        "never",
    ];
    if NEGATIONS.iter().any(|n| question.contains(n)) {
        return None;
    }

    if matches!(kind, "confirm" | "boolean" | "") && PROCEED.iter().any(|p| question.contains(p)) {
        return Some("yes".into());
    }
    None
}

/// Build the `ui_dialog_response` frame for one request.
pub(crate) fn dialog_response_frame(request_id: &str, answer: &str) -> String {
    serde_json::json!({
        "type": "ui_dialog_response",
        "requestId": request_id,
        "answer": answer,
    })
    .to_string()
}

/// A one-line description of a dialog request, for the "unanswered" report.
fn describe_ui(ui: &serde_json::Value) -> String {
    for key in ["message", "prompt", "title", "text"] {
        if let Some(text) = ui.get(key).and_then(serde_json::Value::as_str) {
            if !text.trim().is_empty() {
                return text.trim().to_string();
            }
        }
    }
    ui.to_string()
}

/// Write one JSONL frame to a worker's stdin, best-effort.
///
/// `EPIPE` is not fatal: the worker may have exited between our deciding to answer
/// and the write landing. The exit status is what decides success, not this.
///
/// The handle stays open afterwards: in rpc mode the worker still needs the channel
/// to receive answers. (The one-shot `-p` path used to drop it here, which closed the
/// pipe and made answering impossible.)
fn write_line(stdin: &mut Option<std::process::ChildStdin>, frame: &str) {
    use std::io::Write;
    let Some(handle) = stdin.as_mut() else {
        return;
    };
    let mut line = frame.to_owned();
    if !line.ends_with('\n') {
        line.push('\n');
    }
    let _ = handle.write_all(line.as_bytes());
    let _ = handle.flush();
}

/// Collect an `rpc` worker's event stream into an [`RpcOutcome`].
///
/// Mirrors [`collect_reader`]'s deadline discipline: a descendant holding the pipe
/// must not extend the call past the wall clock.
pub(crate) fn collect_outcome(
    reader: Option<(Receiver<RpcOutcome>, JoinHandle<()>)>,
    deadline: Instant,
    control: &Control,
) -> Result<RpcOutcome, String> {
    control.check()?;
    let Some((receiver, _handle)) = reader else {
        return Ok(RpcOutcome::default());
    };
    let slice = deadline.saturating_duration_since(Instant::now());
    match receiver.recv_timeout(slice) {
        Ok(outcome) => Ok(outcome),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err("timed out reading the worker's event stream".into())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Ok(RpcOutcome::default()),
    }
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

/// Read an `rpc` worker's stdout as a JSONL event stream.
///
/// Unlike [`spawn_reader`], this does not return raw text: in rpc mode every line
/// is a frame, and the answer lives inside `turn_end` / `agent_end` rather than in
/// the bytes themselves. Returning the raw stream would hand the caller a wall of
/// JSON as if it were the worker's reply.
///
/// Lines that fail to parse are kept as `noise` rather than dropped: a non-frame
/// line usually means the host printed a diagnostic, and that is exactly what
/// explains a worker that "ran but said nothing".
/// Same as [`spawn_event_reader`], plus a hook for each dialog request as it
/// arrives.
///
/// The hook runs **on the reader thread, before the worker can be answered** —
/// that is what lets the caller write the answer back while the worker is still
/// blocked on it. Doing this after the stream ends would be too late: the worker
/// is waiting *now*.
pub(crate) fn spawn_event_reader_with<R, F>(
    mut reader: R,
    mut on_question: F,
) -> (Receiver<RpcOutcome>, JoinHandle<()>)
where
    R: Read + Send + 'static,
    F: FnMut(Question) + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut outcome = RpcOutcome::default();
        let mut pending = String::new();
        let mut buf = [0u8; 8192];
        let mut raw_bytes = 0usize;
        let mut raw_tail = String::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(count) => {
                    // Bound what we are willing to buffer: a worker that streams
                    // megabytes must not grow this thread without limit.
                    raw_bytes = raw_bytes.saturating_add(count);
                    pending.push_str(&String::from_utf8_lossy(&buf[..count]));
                    // Keep a bounded copy of the raw stream for the failure text.
                    if raw_tail.len() < 4_000 {
                        raw_tail.push_str(&String::from_utf8_lossy(&buf[..count]));
                        if raw_tail.len() > 4_000 {
                            let cut = raw_tail
                                .char_indices()
                                .map(|(i, _)| i)
                                .find(|i| *i > 4_000)
                                .unwrap_or(raw_tail.len());
                            raw_tail.truncate(cut);
                        }
                    }
                    while let Some(pos) = pending.find('\n') {
                        let line: String = pending.drain(..=pos).collect();
                        fold_rpc_event_with(&line, &mut outcome, &mut on_question);
                    }
                    if raw_bytes > MAX_OUTPUT_BYTES * 8 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // A last line without a trailing newline still counts.
        if !pending.trim().is_empty() {
            fold_rpc_event(&pending, &mut outcome);
        }
        if !outcome.finished && outcome.reply.is_empty() {
            // Nothing parsed as an answer: hand back what the host actually said,
            // so "worker produced nothing" has a cause attached.
            outcome.reply = format!(
                "[no turn_end / agent_end in the worker's output]\n{}",
                raw_tail.trim_end()
            );
        }
        let _ = sender.send(outcome);
    });
    (receiver, handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    static PROCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The prompt must be a `prompt` **frame**, not raw text.
    ///
    /// `-p` took raw stdin; `--mode rpc` parses JSONL. Text sent to an rpc host is
    /// not rejected — it simply is not a command, so the worker waits for a prompt
    /// that never arrives and dies on the wall clock. The failure looks like "the
    /// worker hung", which is why this gets a test rather than a comment.
    #[test]
    fn the_prompt_is_a_prompt_frame_not_raw_text() {
        let line = prompt_frame("do the thing");
        // One line: JSONL framing breaks if the payload embeds a newline.
        assert!(!line.contains('\n'), "frame must be one line: {line}");
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "prompt");
        assert_eq!(value["content"], "do the thing");
    }

    /// A multi-line task still produces a single line of JSON.
    #[test]
    fn a_multiline_task_stays_one_frame() {
        let line = prompt_frame("line one\nline two");
        assert_eq!(line.lines().count(), 1);
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["content"], "line one\nline two");
    }

    /// `turn_end` carries the answer inside `message.content[]`.
    #[test]
    fn turn_end_becomes_the_reply() {
        let mut out = RpcOutcome::default();
        let line = r#"{"type":"turn_end","message":{"content":[{"type":"text","text":"all done"}]},"toolResultCount":0,"toolResults":[]}"#;
        assert!(fold_rpc_event(line, &mut out));
        assert_eq!(out.reply, "all done");
        assert!(out.finished);
    }

    /// A tool call's arguments are not an answer: only `text` blocks count.
    #[test]
    fn tool_call_blocks_are_not_treated_as_the_reply() {
        let mut out = RpcOutcome::default();
        let line = r#"{"type":"turn_end","message":{"content":[{"type":"toolCall","name":"bash","arguments":{"command":"ls"}}]},"toolResultCount":1,"toolResults":[]}"#;
        fold_rpc_event(line, &mut out);
        assert_eq!(out.reply, "");
        assert!(out.finished, "the turn still ended");
    }

    /// An unanswered question is collected, not dropped.
    ///
    /// Dropping it is what left workers hanging to the 300 s deadline with no
    /// trace of what they wanted.
    #[test]
    fn a_dialog_request_is_carried_out() {
        let mut out = RpcOutcome::default();
        let line = r#"{"type":"ui_dialog_request","requestId":"pidingo:1","ui":{"message":"which file?"}}"#;
        fold_rpc_event(line, &mut out);
        assert_eq!(out.questions.len(), 1);
        assert!(out.questions[0].contains("requestId=pidingo:1"));
        assert!(out.questions[0].contains("which file?"));
    }

    // ── 提问的自动应答（B6 第二部分）──────────────────────────────

    /// 「可以继续吗」这类问题由引擎代答 yes。
    ///
    /// 依据是节点自己的验收条件已经说过"什么算完"，再问一遍是复述，不会改变目标。
    #[test]
    fn a_proceed_question_is_answered_yes() {
        let ui = serde_json::json!({"kind": "confirm", "message": "Shall I proceed?"});
        assert_eq!(auto_answer(&ui).as_deref(), Some("yes"));
        let ui = serde_json::json!({"kind": "confirm", "message": "May I proceed with the edit?"});
        assert_eq!(auto_answer(&ui).as_deref(), Some("yes"));
    }

    /// 中文的「可以继续吗」也要能答。
    ///
    /// 这个项目的任务大多用中文写，worker 也就用中文问。只匹配英文的话，
    /// 每个中文任务的 proceed 问题都会掉到 `cancelled` —— 节点被记成失败、
    /// 多花一次运行，而唯一的线索是「它问了点什么」。那是静默的额外开销，
    /// 不是报错，所以最容易一直没人发现。
    #[test]
    fn a_chinese_proceed_question_is_answered_yes() {
        for message in [
            "可以继续吗？",
            "我可以继续吗",
            "是否继续执行",
            "要继续吗",
            "这样做对吗？",
            "没问题吧",
            "可以开始吗",
        ] {
            let ui = serde_json::json!({"kind": "confirm", "message": message});
            assert_eq!(auto_answer(&ui).as_deref(), Some("yes"), "应当代答: {message}");
        }
    }

    /// 中文没有词边界，所以关键词必须是**整句短语** —— 短词会误中反向表达。
    ///
    /// 「不要继续」里含「继续」，若关键词是「继续」就会被答成 yes，
    /// 而它恰好是相反的意思。这条测试就是钉住这个原则。
    #[test]
    fn a_chinese_negation_is_never_answered_yes() {
        for message in ["不要继续", "不用继续", "停止继续运行"] {
            let ui = serde_json::json!({"kind": "confirm", "message": message});
            assert_eq!(auto_answer(&ui), None, "不该代答: {message}");
        }
    }

    /// 否定词不能太短，否则会吃掉正常问句。
    ///
    /// 「别」曾经在列表里，于是「别的问题没有了，可以继续吗」被判成否定、
    /// 永远答不上。放过一个真否定的代价是多跑一次节点；误伤常见问句的代价是
    /// 那类任务**永远**多跑一次。
    #[test]
    fn a_short_word_in_a_normal_question_does_not_block_the_answer() {
        for message in [
            "别的问题没有了，可以继续吗",
            "这个方案别处见过，可以继续吗",
        ] {
            let ui = serde_json::json!({"kind": "confirm", "message": message});
            assert_eq!(auto_answer(&ui).as_deref(), Some("yes"), "不该误判: {message}");
        }
    }

    /// 选一个方案是决策，不是复述 —— 引擎不能替它拍板。
    ///
    /// 答错的代价比节点失败更糟：节点会报成功，而没有人再回头看了一眼。
    #[test]
    fn a_choice_is_never_auto_answered() {
        for ui in [
            serde_json::json!({"kind": "select", "message": "Which library?", "options": ["a", "b"]}),
            serde_json::json!({"kind": "text", "message": "What should the file contain?"}),
            serde_json::json!({"kind": "confirm", "message": "Delete the old migrations?"}),
        ] {
            assert_eq!(auto_answer(&ui), None, "不该替人决策: {ui}");
        }
    }

    /// 空消息的 confirm 也不能瞎答。
    #[test]
    fn a_bare_confirm_gets_no_answer() {
        assert_eq!(auto_answer(&serde_json::json!({"kind": "confirm"})), None);
    }

    /// 应答帧的形状与 rpi 的 rpc 协议一致。
    #[test]
    fn the_response_frame_matches_the_rpc_shape() {
        let line = dialog_response_frame("pidingo:7", "yes");
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "ui_dialog_response");
        assert_eq!(value["requestId"], "pidingo:7");
        assert_eq!(value["answer"], "yes");
    }

    /// 提问在**解析当时**回调出去，而不是等流结束。
    ///
    /// 这是能回答它的前提：worker 正阻塞在这一帧上，晚一步它就已经超时了。
    #[test]
    fn a_question_is_delivered_as_it_is_parsed() {
        let mut out = RpcOutcome::default();
        let mut seen: Vec<Question> = Vec::new();
        let line = r#"{"type":"ui_dialog_request","requestId":"pidingo:1","ui":{"kind":"confirm","message":"Shall I proceed?"}}"#;
        fold_rpc_event_with(line, &mut out, &mut |q| seen.push(q));

        assert_eq!(seen.len(), 1, "回调必须当场触发");
        assert_eq!(seen[0].request_id, "pidingo:1");
        // 同一帧仍然进 outcome，供最终报告用。
        assert_eq!(out.questions.len(), 1);
    }

    /// Frames we do not act on must not be mistaken for data.
    ///
    /// `raise` is the return value: `true` means "this line was a well-formed
    /// frame", not "this line said something". Only unparseable lines are `false`
    /// (those are host diagnostics, kept for the failure text).
    #[test]
    fn frames_we_do_not_use_leave_the_outcome_empty() {
        let mut out = RpcOutcome::default();
        assert!(fold_rpc_event(r#"{"type":"ready"}"#, &mut out), "a ready frame parses");
        assert!(
            !fold_rpc_event("not json at all", &mut out),
            "garbage is not a frame"
        );
        // Neither one contributes an answer or a question.
        assert_eq!(out, RpcOutcome::default());
    }

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
            vec!["--mode", "rpc", "--no-session", "--exclude-tools", EXCLUDED_TOOLS]
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
                "--mode", "rpc",
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
                "--mode", "rpc",
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
            vec!["--mode", "rpc", "--no-session", "--exclude-tools", EXCLUDED_TOOLS]
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
                "--mode", "rpc",
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
            vec!["--mode", "rpc", "--no-session", "--exclude-tools", EXCLUDED_TOOLS]
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
