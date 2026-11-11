//! `read` + `edit` addressed by line number, guarded by a whole-file tag.
//!
//! The pair replaces both built-in tools, because a line number is only usable
//! if the tool that prints it and the tool that consumes it agree on the format.
//!
//! * `read` prints a header with a 4-hex snapshot tag, then numbered lines.
//! * `edit` takes `PUT` / `CUT` hunks addressed with those numbers.
//!
//! The payoff is on the output side: the model stops retyping the lines it
//! edits, and a stale tag is rejected before anything is written. Line numbers
//! are the address (unique by construction, even for identical lines); the tag
//! is the freshness check, which is a different question and gets its own
//! mechanism.
// The SDK's `export_plugin!` macro and the callback ABI write through raw
// `out` pointers; the host guarantees those pointers are valid for the call.
// Unsafe appears only at that boundary.
#![allow(unsafe_code)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

pub mod anchors;
pub mod conflicts;
pub mod edit_ops;

use rpi_plugin_sdk::callbacks::free_string;
use rpi_plugin_sdk::handles::ToolHandles;
use rpi_plugin_sdk::{
    FreeStringFn, StableToolSchema, StbString, StbStringRef, StepHandle, StepResult, ToolPartialCb,
};
use serde_json::{json, Value};
use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

/// Registered tool names.
///
/// Deliberately **not** `read` / `edit`: shadowing the built-ins would make
/// every read pay for line numbers, including the read-and-analyze work where
/// nothing is edited, and the built-in `edit`'s `replaceAll` has no line-number
/// equivalent. Both pairs live side by side and the model picks per task.
const READ_TOOL: &str = "hash_read";
const EDIT_TOOL: &str = "hash_edit";

const READ_DESCRIPTION: &str = "Read a UTF-8 text file. The header carries a snapshot tag `#XXXX` and every line is numbered, `12:<text>`. To change lines, address them in `hash_edit` (`PUT 12.=14:`, `CUT 8.=9`, `PUT >12:`) instead of retyping the text, and pass the tag so a stale read is rejected rather than misapplied. Use `offset`/`limit` to window a large file; printed line numbers stay absolute.";

const EDIT_DESCRIPTION: &str = "Edit a UTF-8 file by line number instead of copied text; call `hash_read` first to get the tag and line numbers. `patch` holds hunks: `PUT 12.=14:` then one `+`-prefixed row per replacement line (`PUT 12.=12:` changes one line, `PUT >12:` inserts after line 12, `PUT <5:` before line 5, `PUT >$:` at end of file); `CUT 8.=9` deletes an inclusive range. Numbers resolve against the original file, so hunks in one call cannot shift each other. Pass the `tag` from the `hash_read` header and a changed file is rejected instead of misapplied; nothing is written unless every hunk resolves. Indentation and whitespace never need reproducing. Writes through a same-directory temporary file and rename.";

/// `read`'s parameter contract.
const READ_PARAMETERS: &str = r#"{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Path to one UTF-8 text file." },
    "offset": { "type": "integer", "minimum": 1, "description": "1-based first line to print. Default 1." },
    "limit": { "type": "integer", "minimum": 1, "description": "Maximum number of lines to print. Default: through end of file." }
  },
  "required": ["path"]
}"#;

/// `edit`'s parameter contract. Must stay valid JSON: the host rejects a
/// malformed schema at registration and silently keeps the built-in tool.
const EDIT_PARAMETERS: &str = r#"{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Path to one existing UTF-8 text file." },
    "tag": { "type": "string", "description": "Snapshot tag from the read header, with or without the leading #, e.g. 1A2B. When given, the file must still hash to it or the edit is rejected." },
    "patch": { "type": "string", "description": "Hunks separated by newlines. 'PUT <address>:' writes the following '+' rows; address is N, N.=M, >N (after that line), >$ (end of file) or <N (before that line). 'CUT <address>' deletes an inclusive range." }
  },
  "required": ["path", "patch"]
}"#;

/// Host-injected working directory (`__rpi.cwd`), when the host sent one.
fn host_cwd(params: &Value) -> Option<PathBuf> {
    params
        .get(rpi_plugin_sdk::HOST_CONTEXT_KEY)?
        .get("cwd")?
        .as_str()
        .filter(|cwd| !cwd.is_empty())
        .map(PathBuf::from)
}

/// Resolve `path` against `cwd`, falling back to the process working directory.
fn resolve_path(cwd: Option<&Path>, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match cwd {
        Some(cwd) => cwd.join(path),
        None => std::env::current_dir()
            .map(|current| current.join(path))
            .unwrap_or_else(|_| path.to_path_buf()),
    }
}

/// Run one `read` call, returning the model-facing text.
fn run_read(params: &Value) -> Result<String, String> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .ok_or("error: `path` must be a non-empty string")?;
    let absolute = resolve_path(host_cwd(params).as_deref(), path);
    let content = fs::read_to_string(&absolute)
        .map_err(|error| format!("error: could not read {path}: {error}"))?;

    let lines = anchors::split_lines(&content);
    let total = lines.len();
    let tag = anchors::snapshot_tag(&content);
    if total == 0 {
        return Ok(format!("{path} #{tag} - empty file (0 lines)"));
    }
    let offset = params
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1) as usize;
    if offset > total {
        return Err(format!(
            "error: offset {offset} is past the end of {path} ({total} lines)"
        ));
    }
    let end = match params.get("limit").and_then(Value::as_u64) {
        Some(limit) => (offset + limit as usize).min(total + 1),
        None => total + 1,
    };

    let mut out = format!("{path} #{tag} - {total} lines\n");
    for (index, text) in lines[offset - 1..end - 1].iter().enumerate() {
        out.push_str(&anchors::numbered_line(offset + index, text));
        out.push('\n');
    }
    if end <= total {
        out.push_str(&format!(
            "... {} more lines (continue with offset: {end})\n",
            total + 1 - end
        ));
    }
    Ok(out)
}

/// Run one `edit` call, returning the model-facing text.
fn run_edit(params: &Value) -> Result<String, String> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .ok_or("error: `path` must be a non-empty string")?;
    let patch = params
        .get("patch")
        .and_then(Value::as_str)
        .filter(|patch| !patch.trim().is_empty())
        .ok_or("error: `patch` must be a non-empty string of PUT/CUT hunks")?;

    let absolute = resolve_path(host_cwd(params).as_deref(), path);
    let raw = fs::read_to_string(&absolute)
        .map_err(|error| format!("error: could not read {path}: {error}"))?;

    if let Some(tag) = params
        .get("tag")
        .and_then(Value::as_str)
        .filter(|tag| !tag.trim().is_empty())
    {
        // Accept `#DE7C` as well as `DE7C`: the tag is printed with the `#` and
        // models copy it back verbatim.
        let tag = tag.trim().trim_start_matches('#');
        anchors::verify_tag(&raw, tag).map_err(|error| error.to_string())?;
    }

    let total = anchors::split_lines(&raw).len();
    let hunks = edit_ops::parse(patch, total)?;
    let applied = edit_ops::apply(&raw, &hunks);

    // The snapshot the hunks resolved against must still be what is on disk.
    let current = fs::read_to_string(&absolute)
        .map_err(|error| format!("error: could not re-read {path}: {error}"))?;
    if current != raw {
        return Err(format!(
            "error: {path} changed while the edit was applied; read it again and retry"
        ));
    }
    write_atomically(&absolute, &applied.content)
        .map_err(|error| format!("error: could not write {path}: {error}"))?;

    let tag = anchors::snapshot_tag(&applied.content);
    Ok(format!(
        "Edited {path} #{tag}: {} line(s) replaced, {} inserted, {} deleted.",
        applied.replaced, applied.inserted, applied.deleted
    ))
}

/// Replace `path`'s contents through a temporary file in the same directory, so
/// the target is either the old file or the new one - never a partial write.
fn write_atomically(path: &Path, content: &str) -> std::io::Result<()> {
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let temp = directory.join(format!(".{name}.rpi-hashline.tmp"));
    fs::write(&temp, content)?;
    if let Ok(metadata) = fs::metadata(path) {
        let _ = fs::set_permissions(&temp, metadata.permissions());
    }
    match fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(error)
        }
    }
}

// ---------------------------------------------------------------------------
// Plugin ABI glue
// ---------------------------------------------------------------------------

/// One in-flight tool call.
struct Drive {
    params: Value,
    run: fn(&Value) -> Result<String, String>,
    done: bool,
}

/// `read` and `edit` each need their own handle table: `ToolHandles` is the
/// SDK's storage, so an unknown handle stays a clean error instead of a guess
/// about which tool was meant.
static READ_HANDLES: ToolHandles<Drive> = ToolHandles::new();
static EDIT_HANDLES: ToolHandles<Drive> = ToolHandles::new();

extern "C" fn read_execute(
    _: StbStringRef,
    params: StbString,
    free: Option<FreeStringFn>,
) -> StepHandle {
    insert(&READ_HANDLES, params, free, run_read)
}

extern "C" fn edit_execute(
    _: StbStringRef,
    params: StbString,
    free: Option<FreeStringFn>,
) -> StepHandle {
    insert(&EDIT_HANDLES, params, free, run_edit)
}

/// Parse the params and park a drive in `table`.
fn insert(
    table: &ToolHandles<Drive>,
    params: StbString,
    free: Option<FreeStringFn>,
    run: fn(&Value) -> Result<String, String>,
) -> StepHandle {
    let text = params.to_string_lossy();
    params.free_with(free);
    table.insert(Drive {
        params: serde_json::from_str(&text).unwrap_or(Value::Null),
        run,
        done: false,
    })
}

extern "C" fn read_poll(handle: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
    poll(&READ_HANDLES, handle, "read")
}

extern "C" fn edit_poll(handle: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
    poll(&EDIT_HANDLES, handle, "edit")
}

/// Run the parked drive once and report its result.
fn poll(table: &ToolHandles<Drive>, handle: StepHandle, tool: &str) -> StepResult {
    if handle.is_null() {
        return StepResult::err(StbString::from_string(format!("null {tool} handle")));
    }
    let Some(entry) = table.get(handle) else {
        return StepResult::err(StbString::from_string(
            "invalid or destroyed tool handle".into(),
        ));
    };
    let mut drive = entry.lock();
    if entry.cancelled.load(Ordering::SeqCst) {
        return StepResult::err(StbString::from_string(format!("{tool} cancelled")));
    }
    if drive.done {
        return StepResult::err(StbString::from_string(format!(
            "{tool} polled after completion"
        )));
    }
    drive.done = true;
    let run = drive.run;
    match run(&drive.params) {
        Ok(text) => StepResult::done(StbString::from_string(
            json!({ "content": [{ "type": "text", "text": text }] }).to_string(),
        )),
        Err(message) => StepResult::err(StbString::from_string(message)),
    }
}

extern "C" fn read_cancel(handle: StepHandle) {
    READ_HANDLES.cancel(handle);
}
extern "C" fn edit_cancel(handle: StepHandle) {
    EDIT_HANDLES.cancel(handle);
}
extern "C" fn read_destroy(handle: StepHandle) {
    drop(READ_HANDLES.remove(handle));
}
extern "C" fn edit_destroy(handle: StepHandle) {
    drop(EDIT_HANDLES.remove(handle));
}

/// Build the provider-facing schema for one tool.
fn schema(name: &str, description: &str, parameters: &str) -> StableToolSchema {
    StableToolSchema {
        name: StbString::from_string(name.into()),
        description: StbString::from_string(description.into()),
        parameters: StbString::from_string(parameters.into()),
    }
}

rpi_plugin_sdk::export_plugin!(|api| {
    let Some(register) = api.register_tool else {
        return 1;
    };
    // Both names must land or neither is useful: the anchor format is only
    // coherent when the printer and the consumer are both ours.
    let mut rc = 0;
    for (name, description, parameters, execute, poll, cancel, destroy) in [
        (
            EDIT_TOOL,
            EDIT_DESCRIPTION,
            EDIT_PARAMETERS,
            edit_execute as _,
            edit_poll as _,
            edit_cancel as _,
            edit_destroy as _,
        ),
        (
            READ_TOOL,
            READ_DESCRIPTION,
            READ_PARAMETERS,
            read_execute as _,
            read_poll as _,
            read_cancel as _,
            read_destroy as _,
        ),
    ] {
        let schema = Box::new(schema(name, description, parameters));
        let status = register(&*schema, execute, poll, cancel, destroy, free_string);
        drop(schema);
        if status != 0 {
            eprintln!("rpi-hashline: the host refused to register `{name}` (status {status})");
            rc = status;
        }
    }

    rc
});

#[cfg(test)]
mod tests;
