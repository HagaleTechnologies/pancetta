//! Cross-process turns at writing the ADIF logbook (PAN-214).
//!
//! Pancetta's live logging and `pancetta import` both append to `qsos.adi`,
//! and an import undo rewrites it. Each holds an exclusive lock on the empty
//! sibling file `qsos.adi.lock` while it writes, so an import's records form
//! one block no live QSO lands inside. The lock is on a separate file because
//! Windows locks are mandatory: locking `qsos.adi` itself would break other
//! programs reading it. `File::lock` is flock on Unix and LockFileEx on
//! Windows; the OS releases it when the holder exits or crashes.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often [`LogbookLock::lock_within`] retries a busy lock.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// An open handle on a logbook's lock file. Opening it locks nothing; take
/// the lock with [`LogbookLock::try_lock`] or [`LogbookLock::lock_within`].
#[derive(Debug)]
pub struct LogbookLock {
    file: File,
    path: PathBuf,
}

/// Holds the logbook lock until it is dropped.
#[derive(Debug)]
pub struct LogbookGuard<'a> {
    lock: &'a LogbookLock,
}

impl LogbookLock {
    /// The lock file for `logbook`: its full file name plus `.lock`, beside it.
    pub fn path_for(logbook: &Path) -> PathBuf {
        let mut path = logbook.as_os_str().to_owned();
        path.push(".lock");
        PathBuf::from(path)
    }

    /// Open (creating it and its directory when missing) the lock file for
    /// `logbook`. The file stays empty; on Unix a new one is owner-only.
    pub fn open(logbook: &Path) -> io::Result<Self> {
        let path = Self::path_for(logbook);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = OpenOptions::new();
        options.create(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        Ok(Self { file, path })
    }

    /// The lock file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Take the lock if nobody holds it; `Ok(None)` when another handle does.
    pub fn try_lock(&self) -> io::Result<Option<LogbookGuard<'_>>> {
        match self.file.try_lock() {
            Ok(()) => Ok(Some(LogbookGuard { lock: self })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }

    /// Poll every 50 ms until `wait` passes; `on_busy` runs once, on the first busy try.
    ///
    /// `Ok(None)` when the lock is still held once `wait` has passed.
    pub async fn lock_within(
        &self,
        wait: Duration,
        on_busy: impl FnOnce(),
    ) -> io::Result<Option<LogbookGuard<'_>>> {
        let deadline = tokio::time::Instant::now() + wait;
        let mut on_busy = Some(on_busy);
        loop {
            if let Some(guard) = self.try_lock()? {
                return Ok(Some(guard));
            }
            if let Some(on_busy) = on_busy.take() {
                on_busy();
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(POLL_INTERVAL.min(deadline - now)).await;
        }
    }
}

impl Drop for LogbookGuard<'_> {
    fn drop(&mut self) {
        let _ = self.lock.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::Instant;
    use tempfile::tempdir;

    #[test]
    fn the_lock_file_sits_beside_the_logbook() {
        assert_eq!(
            LogbookLock::path_for(Path::new("/x/qsos.adi")),
            PathBuf::from("/x/qsos.adi.lock")
        );
    }

    #[test]
    fn open_creates_the_directory_and_an_empty_file() {
        let tmp = tempdir().unwrap();
        let logbook = tmp.path().join("missing").join("qsos.adi");

        let lock = LogbookLock::open(&logbook).unwrap();

        assert_eq!(
            lock.path(),
            tmp.path().join("missing").join("qsos.adi.lock")
        );
        let meta = std::fs::metadata(lock.path()).unwrap();
        assert!(meta.is_file());
        assert_eq!(meta.len(), 0);
        assert!(
            !logbook.exists(),
            "opening the lock must not create the logbook"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(meta.permissions().mode() & 0o077, 0);
        }
    }

    #[tokio::test]
    async fn a_second_handle_waits_then_gives_up() {
        let tmp = tempdir().unwrap();
        let logbook = tmp.path().join("qsos.adi");
        let a = LogbookLock::open(&logbook).unwrap();
        let b = LogbookLock::open(&logbook).unwrap();
        let _held = a.try_lock().unwrap().expect("A takes the free lock");

        let busy_calls = Cell::new(0);
        let started = Instant::now();
        let got = b
            .lock_within(Duration::from_millis(150), || {
                busy_calls.set(busy_calls.get() + 1)
            })
            .await
            .unwrap();

        assert!(got.is_none(), "B must give up while A holds the lock");
        assert_eq!(busy_calls.get(), 1);
        assert!(started.elapsed() >= Duration::from_millis(150));
    }

    #[tokio::test]
    async fn a_waiting_handle_gets_the_lock_once_it_is_released() {
        let tmp = tempdir().unwrap();
        let logbook = tmp.path().join("qsos.adi");
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let holder = {
            let logbook = logbook.clone();
            std::thread::spawn(move || {
                let a = LogbookLock::open(&logbook).unwrap();
                let guard = a.try_lock().unwrap().expect("A takes the free lock");
                held_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(200));
                drop(guard);
            })
        };
        held_rx.recv().unwrap();
        let b = LogbookLock::open(&logbook).unwrap();

        let busy_calls = Cell::new(0);
        let got = b
            .lock_within(Duration::from_secs(5), || {
                busy_calls.set(busy_calls.get() + 1)
            })
            .await
            .unwrap();
        assert!(got.is_some(), "B gets the lock once A releases it");
        assert_eq!(busy_calls.get(), 1);
        drop(got);
        holder.join().unwrap();

        let got_free = b
            .lock_within(Duration::from_secs(5), || panic!("the lock is free"))
            .await
            .unwrap();
        assert!(got_free.is_some());
    }

    #[test]
    fn dropping_the_guard_releases_the_lock() {
        let tmp = tempdir().unwrap();
        let logbook = tmp.path().join("qsos.adi");
        let a = LogbookLock::open(&logbook).unwrap();
        let b = LogbookLock::open(&logbook).unwrap();

        let guard = a.try_lock().unwrap().expect("A takes the free lock");
        assert!(b.try_lock().unwrap().is_none());
        drop(guard);

        assert!(b.try_lock().unwrap().is_some());
    }
}
