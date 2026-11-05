//! Finding the super-agent extension so a reply process can load it.
//!
//! ## Why this exists
//!
//! A Feishu reply runs in its own `rpi --print` child process, so the reply
//! has a conversation of its own — one per topic, which is what makes topics
//! independent. That child normally starts with `--no-extensions`, because a
//! child that loaded *this* extension would open a second Feishu connection,
//! and that connection would spawn another child, and so on.
//!
//! But without any extension the child has no orchestration tools: it cannot
//! create a task, cannot plan a DAG, cannot dispatch work.
//!
//! `--local-only` is the way out: it loads **only the extensions named
//! explicitly** with `-e`. Naming the super-agent extension and nothing else
//! gives the child its tools without the recursion.
//!
//! ## How the extension is found
//!
//! The host puts extensions in `<agent dir>/extensions`, where the agent dir
//! is `RPI_CODING_AGENT_DIR` when set and `~/.rpi/agent` otherwise. That is
//! the same rule the host uses, so a plugin can reproduce it.

use std::path::PathBuf;

/// The extension file name, without the platform's library prefix or suffix.
const EXTENSION_STEM: &str = "rpi_superagent";

/// The agent directory: `RPI_CODING_AGENT_DIR`, else `~/.rpi/agent`.
///
/// Mirrors the host's rule so both agree on where extensions live.
pub fn agent_dir() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("RPI_CODING_AGENT_DIR") {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            return Some(path);
        }
        // The host rejects a relative override; so do we, rather than
        // resolving it against a different working directory.
        return None;
    }
    let home = std::env::var_os("HOME")?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return None;
    }
    Some(home.join(".rpi").join("agent"))
}

/// The directories that hold installed extensions.
pub fn extension_dirs() -> Vec<PathBuf> {
    match agent_dir() {
        Some(dir) => vec![dir.join("extensions")],
        None => Vec::new(),
    }
}

/// The super-agent extension's library, if it is installed.
///
/// The host names the file `librpi_superagent.dylib` on unix and
/// `librpi_superagent.dll` on windows, and the supervisor beside it as
/// `rpi-superagent-supervisor`. Only the library is needed here.
pub fn superagent_extension() -> Option<PathBuf> {
    for dir in extension_dirs() {
        for name in library_names() {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// The library file names to look for, most specific first.
fn library_names() -> Vec<String> {
    let extension = if cfg!(target_os = "windows") {
        "dll"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    vec![
        format!("lib{EXTENSION_STEM}.{extension}"),
        format!("{EXTENSION_STEM}.{extension}"),
    ]
}

/// The `-e` and `--local-only` arguments that load only the super-agent
/// extension.
///
/// Empty when the extension is not installed: the caller then runs the child
/// without any extension, which is what it did before.
pub fn superagent_extension_args() -> Vec<String> {
    match superagent_extension() {
        Some(path) => vec![
            "--extension".to_owned(),
            path.to_string_lossy().into_owned(),
            // Load only what was named above. Loading everything would bring
            // this extension along and start a second Feishu connection.
            "--local-only".to_owned(),
        ],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_dir_follows_the_host_rule() {
        // The rule is the host's: an absolute override wins, otherwise
        // `~/.rpi/agent`.
        let dir = agent_dir().expect("an agent dir");
        assert!(dir.is_absolute(), "{dir:?}");
        if std::env::var_os("RPI_CODING_AGENT_DIR").is_some() {
            assert!(dir.ends_with("rpi-agent") || dir.to_string_lossy().contains("rpi-agent"));
        }
    }

    #[test]
    fn the_extension_dirs_sit_under_the_agent_dir() {
        let dirs = extension_dirs();
        if let Some(dir) = agent_dir() {
            assert_eq!(dirs, vec![dir.join("extensions")]);
        }
    }

    #[test]
    fn naming_an_extension_also_means_local_only() {
        // Without `--local-only` the child would load every installed
        // extension, including this one, and the Feishu connection would
        // spawn another child.
        let args = superagent_extension_args();
        if !args.is_empty() {
            assert!(args.iter().any(|arg| arg == "--extension"));
            assert!(args.iter().any(|arg| arg == "--local-only"));
        }
    }

    #[test]
    fn a_missing_extension_yields_no_arguments() {
        // Nothing installed means the child runs as it did before.
        if superagent_extension().is_none() {
            assert!(superagent_extension_args().is_empty());
        }
    }
}
