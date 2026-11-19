//! The process data directory (PAN-376).
//!
//! Every piece of station state (config, logbook, logs, station keys, the
//! upload outbox, session and TUI state, caches) lives in one data directory.
//! Without `--data-dir` that is `<home>/.pancetta`, exactly as before; with
//! `pancetta --data-dir DIR` it is `DIR`.
//!
//! The directory is process-wide: the `pancetta` binary's `main()` resolves
//! the flag once with [`resolve_flag`] and stores it with
//! [`set_process_data_dir`] before anything else opens a file. Every
//! default-path resolver in every crate reads [`try_current`] or [`current`]
//! instead of joining `.pancetta` onto a home directory.
//!
//! Rule: only the `pancetta` binary's `main()` calls [`set_process_data_dir`].
//! Unit tests never set it (it would leak into every other test in the same
//! binary); only one-test integration binaries do.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Name of the default data directory under the home directory.
pub const DEFAULT_DIR_NAME: &str = ".pancetta";

static PROCESS_DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Why a `--data-dir` value could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataDirError {
    /// The value was empty or whitespace only.
    Empty,
    /// The value started with `~` but no home directory was found.
    NoHomeForTilde(PathBuf),
}

impl fmt::Display for DataDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataDirError::Empty => write!(f, "--data-dir must name a directory"),
            DataDirError::NoHomeForTilde(raw) => write!(
                f,
                "--data-dir {} starts with ~ but no home directory was found",
                raw.display()
            ),
        }
    }
}

impl std::error::Error for DataDirError {}

/// Resolve a raw `--data-dir` value to an absolute path.
///
/// A leading `~`, `~/` or `~\` expands against `home` (so a quoted value or
/// Windows `cmd`, which do not expand `~`, still work); `~user` is not
/// expanded. A relative path is made absolute against the working directory.
/// An empty or whitespace-only value is an error.
pub fn resolve_flag(raw: &Path, home: Option<&Path>) -> Result<PathBuf, DataDirError> {
    let text = raw.to_string_lossy();
    if text.trim().is_empty() {
        return Err(DataDirError::Empty);
    }
    let expanded = if text == "~" {
        home.map(Path::to_path_buf)
            .ok_or_else(|| DataDirError::NoHomeForTilde(raw.to_path_buf()))?
    } else if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        home.map(|h| h.join(rest))
            .ok_or_else(|| DataDirError::NoHomeForTilde(raw.to_path_buf()))?
    } else {
        raw.to_path_buf()
    };
    match std::path::absolute(&expanded) {
        Ok(abs) => Ok(abs),
        // `absolute` only fails for an empty path or when the working
        // directory cannot be read; keep the value as given then.
        Err(_) => Ok(expanded),
    }
}

/// The default data directory for a home directory: `<home>/.pancetta`.
pub fn default_for_home(home: Option<&Path>) -> Option<PathBuf> {
    home.map(|h| h.join(DEFAULT_DIR_NAME))
}

/// The home directory: `dirs::home_dir()`, then `$HOME`.
fn home_dir() -> Option<PathBuf> {
    dirs::home_dir().or_else(|| std::env::var_os("HOME").map(PathBuf::from))
}

/// Store the process data directory. Only the `pancetta` binary's `main()`
/// calls this, once, before anything opens a file.
///
/// Returns `Err(dir)` with the rejected value when it was already set; the
/// stored value is unchanged then.
pub fn set_process_data_dir(dir: PathBuf) -> Result<(), PathBuf> {
    PROCESS_DATA_DIR.set(dir)
}

/// The process data directory, or `<home>/.pancetta` when `--data-dir` was
/// not given. `None` only when no flag was given and no home directory is
/// known.
pub fn try_current() -> Option<PathBuf> {
    match PROCESS_DATA_DIR.get() {
        Some(dir) => Some(dir.clone()),
        None => default_for_home(home_dir().as_deref()),
    }
}

/// Like [`try_current`], but falls back to `./.pancetta` when no home
/// directory is known.
pub fn current() -> PathBuf {
    try_current().unwrap_or_else(|| PathBuf::from(".").join(DEFAULT_DIR_NAME))
}

#[cfg(test)]
#[path = "data_dir/tests/data_dir_tests.rs"]
mod data_dir_tests;
