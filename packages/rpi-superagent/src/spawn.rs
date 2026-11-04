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
pub const EXCLUDED_TOOLS: &str = "superagent_delegate,superagent_plan,superagent_dispatch";

/// Upper bound on the text fed back to the orchestrator (design §8 "输出截断").
pub const MAX_OUTPUT_BYTES: usize = 40_000;

/// Turn budget env the host already enforces (`rpi-harness::run_budget`).
pub const MAX_TURNS_ENV: &str = "RPI_MAX_TURNS_PER_RUN";

/// Marker set on the child so the orchestrator guidance is not injected into a
/// subagent (which does not have `plan`/`dispatch`).
pub const SUBAGENT_ENV: &str = "RPI_SUPERAGENT_SUBAGENT";

/// How often the wait loop re-checks cancellation/timeout. Bounds how long a
/// cancelled call can keep a child alive.
pub(crate) const POLL_SLICE: Duration = Duration::from_millis(100);

/// Build the child command. Separate from `run` so it is unit-testable without
/// spawning anything.
///
/// `provider` / `model` are passed through as `--provider` / `--model`. The child
/// shares the orchestrator's agent dir but not its binding, so without them it
/// would resolve the *default* provider from settings — wrong (or broken)
/// whenever this session runs on anything else, or on a provider the default does
/// not authenticate. Callers therefore resolve the effective pair from
/// [`crate::host::parent_selection`] first and always pass it down.
pub fn child_command(
    exe: &Path,
    max_turns: u64,
    provider: Option<&str>,
    model: Option<&str>,
) -> Command {
    let mut command = Command::new(exe);
    command
        .arg("-p")
        // Ephemeral session: a one-shot worker has nothing to resume, and its
        // session file would otherwise be written into the parent's project
        // `.rpi/sessions/` directory — one ~2 MB file per delegation.
        .arg("--no-session")
        // Recursion guard: the child must not offer `subagent`/`plan`/`dispatch`.
        .arg("--exclude-tools")
        .arg(EXCLUDED_TOOLS);
    if let Some(provider) = provider.filter(|value| !value.is_empty()) {
        command.arg("--provider").arg(provider);
    }
    if let Some(model) = model.filter(|value| !value.is_empty()) {
        command.arg("--model").arg(model);
    }
    command
        // Hard turn ceiling enforced by the harness, not the model.
        .env(MAX_TURNS_ENV, max_turns.to_string())
        .env(SUBAGENT_ENV, "1")
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
    };
    format!(
        "subagent exited with {} (selection={}):\n{}",
        code.unwrap_or(-1),
        selection.label(),
        text
    )
}

/// Truncate to [`MAX_OUTPUT_BYTES`] on a char boundary, appending a marker.
pub fn truncate_output(mut text: String) -> String {
    const MARKER: &str = "\n[subagent output truncated]";
    if text.len() <= MAX_OUTPUT_BYTES {
        return text;
    }
    let mut end = MAX_OUTPUT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(MARKER);
    text
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
    // Fail before launching anything if already cancelled.
    control.check()?;
    let (mut child, tree) = spawn_command(&mut child_command(exe, max_turns, provider, model))
        .map_err(|err| format!("failed to start subagent `{}`: {err}", exe.display()))?;

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
                let text = truncate_output(text);
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

/// Best-effort recursive SIGKILL of `pid` and everything below it (unix).
#[cfg(unix)]
fn kill_descendants(pid: u32) {
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

    fn collect_after_parent_exit(cancel: bool) {
        let _serial = PROCESS_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let pid_file = std::env::temp_dir().join(format!(
            "rpi-orchestrator-pipes-{}-{}.pid",
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
            "rpi-orchestrator-child-{}-{}.pid",
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
        let command = child_command(Path::new("/usr/bin/rpi"), 7, None, None);
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
                (MAX_TURNS_ENV.to_string(), "7".to_string()),
                (SUBAGENT_ENV.to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn child_command_passes_provider_and_model_through() {
        let command = child_command(
            Path::new("/usr/bin/rpi"),
            8,
            Some("deepseek"),
            Some("deepseek-flash"),
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
        let bare = child_command(Path::new("/usr/bin/rpi"), 8, Some(""), None);
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
        };
        let selection = parent.effective(None, None);
        let command = child_command(
            Path::new("/usr/bin/rpi"),
            8,
            selection.provider(),
            selection.model(),
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

    #[test]
    fn truncate_output_is_bounded_and_char_safe() {
        let short = "ok".to_string();
        assert_eq!(truncate_output(short.clone()), short);

        // Multi-byte chars: the cut must land on a boundary, not split a char.
        let long = "あ".repeat(MAX_OUTPUT_BYTES);
        let out = truncate_output(long);
        assert!(out.starts_with('あ'));
        assert!(out.ends_with("[subagent output truncated]"));
        assert!(out.len() <= MAX_OUTPUT_BYTES + "\n[subagent output truncated]".len());
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
            Path::new("/nonexistent/orchestrator-child"),
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
