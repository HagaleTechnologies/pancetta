//! One live station per data directory (PAN-376).
//!
//! A live station (no subcommand or `run`, not `--wav`/`--replay`) holds an OS
//! file lock on `<data-dir>/pancetta.lock` for its whole life, and writes its
//! pid to the sibling `pancetta.pid`. A second live station on the same
//! directory fails to take the lock and refuses with
//! `another pancetta is running (pid N) with data directory D`.
//!
//! The pid lives in a separate file because Windows locks are mandatory: a
//! loser cannot read a file another process holds locked (the same reason
//! `pancetta_qso::logbook_lock` keeps its lock file empty).
//!
//! `pancetta.lock` is never deleted. Deleting a lock file lets two processes
//! lock different inodes under one name. The OS releases the lock when the
//! holder exits or crashes, so a killed station leaves no stale lock; the
//! next station overwrites the stale `pancetta.pid`.
//!
//! This lock is per process; `qsos.adi.lock` stays the per-write lock that
//! the live writer and `pancetta import` share by design (PAN-214).
//! Subcommands never take this one, so `import` beside a running station
//! keeps working.

use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// The lock file in the data directory. Empty; never deleted.
pub const LOCK_FILE_NAME: &str = "pancetta.lock";
/// The holder's pid, beside the lock file.
pub const PID_FILE_NAME: &str = "pancetta.pid";

/// Holds the data directory's instance lock until dropped.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

/// Why the instance lock could not be taken.
#[derive(Debug)]
pub enum InstanceLockError {
    /// Another process holds the lock. `pid` is what its `pancetta.pid`
    /// says, or `None` when that file is missing or unreadable.
    Held { pid: Option<u32>, data_dir: PathBuf },
    /// The data directory or its lock file could not be created or opened.
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for InstanceLockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstanceLockError::Held { pid, data_dir } => {
                match pid {
                    Some(pid) => write!(f, "another pancetta is running (pid {pid})")?,
                    None => write!(f, "another pancetta is running (pid unknown)")?,
                }
                write!(
                    f,
                    " with data directory {}\nStop it first, or give this station its own \
                     directory with --data-dir.",
                    data_dir.display()
                )
            }
            InstanceLockError::Io { path, source } => {
                write!(f, "cannot lock data directory {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for InstanceLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            InstanceLockError::Held { .. } => None,
            InstanceLockError::Io { source, .. } => Some(source),
        }
    }
}

impl InstanceLock {
    /// Take the instance lock on `data_dir`, creating the directory when it
    /// is missing. On success the current pid is written to `pancetta.pid`.
    /// A filesystem that cannot lock files runs unlocked, with a warning.
    pub fn acquire(data_dir: &Path) -> Result<Self, InstanceLockError> {
        let io_err = |source| InstanceLockError::Io {
            path: data_dir.to_path_buf(),
            source,
        };
        std::fs::create_dir_all(data_dir).map_err(io_err)?;
        let lock_path = data_dir.join(LOCK_FILE_NAME);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(io_err)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(InstanceLockError::Held {
                    pid: read_pid(&data_dir.join(PID_FILE_NAME)),
                    data_dir: display_dir(data_dir),
                });
            }
            Err(TryLockError::Error(e)) => {
                // A filesystem without file locks (some NFS, SMB or FUSE
                // mounts) must not stop a station that ran before PAN-376;
                // run unlocked and say so, as `qsos.adi.lock` writes anyway
                // after its wait (PAN-214).
                eprintln!(
                    "WARNING: cannot lock {} ({e}); a second pancetta on this data \
                     directory will not be refused",
                    lock_path.display()
                );
            }
        }
        // Best effort: the lock is what keeps a second station out; the pid
        // only names the holder in the refusal.
        let pid_path = data_dir.join(PID_FILE_NAME);
        if let Err(e) = pancetta_config::Config::write_secure_atomic(
            &pid_path,
            &format!("{}\n", std::process::id()),
        ) {
            eprintln!("WARNING: could not write {}: {e}", pid_path.display());
        }
        Ok(Self { _file: file })
    }
}

/// The directory as the refusal names it: resolved by the OS on Unix, so a
/// `..` or symlink spelling of the same directory reads plainly. Windows keeps
/// the absolute spelling, since its canonical form is a `\\?\` path.
fn display_dir(data_dir: &Path) -> PathBuf {
    if cfg!(windows) {
        data_dir.to_path_buf()
    } else {
        std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf())
    }
}

/// The pid recorded in `path`, or `None` when it is missing or garbled.
fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
#[path = "instance_lock/tests/instance_lock_tests.rs"]
mod instance_lock_tests;
