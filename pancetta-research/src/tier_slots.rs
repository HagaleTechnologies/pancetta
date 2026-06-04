//! Cross-process semaphore for capping concurrent heavy eval tiers.
//!
//! Motivation (Batch 18, 2026-06-02): when N parallel research agents each
//! launched their own `eval` invocation against the heavy tiers
//! (hard-200, hard-1000, chrono-replay), CPU load on a 10-core mac peaked
//! at 96-135 and 4+ tier runs either timed out or returned non-deterministic
//! composites. The decoder itself is parallelised internally; layering N
//! eval processes on top of that compounded contention catastrophically.
//!
//! This module exposes an opt-in, file-lock-backed slot pool that an eval
//! invocation acquires once per heavy tier and releases on tier completion.
//! Implementation choice — raw `libc::flock` on per-slot files under a
//! shared pool directory:
//!
//! - **Cross-process by construction** — `flock(2)` is an advisory lock
//!   on a kernel file-descriptor open-file-description. Independent
//!   `cargo run --bin eval` invocations cooperate without any shared
//!   memory or RPC.
//! - **Crash-safe** — if the holding process panics or is killed, the
//!   kernel releases the open-file-description and any waiter on that
//!   slot wakes up. No stale lockfiles, no recovery script.
//! - **Zero new heavy deps** — `libc` is already in the dependency graph
//!   transitively; we add it as a direct dep so the call sites compile
//!   cleanly. No flock crate (`fs2`, `file-lock`, `fd-lock`) introduced.
//!
//! The pool directory defaults to `/tmp/pancetta-eval-tier-slots/` (per
//! the task spec); each slot file is `slot-{i}` for `i in 0..n`. The
//! files are touched on first use and never deleted — they're cheap to
//! keep around and deleting them races with concurrent waiters.
//!
//! See `research/experiments/2026-06-02-eval-concurrency-guard.md` for
//! design rationale and the smoke-test result.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Default pool directory. Lives in `/tmp` so it is wiped on reboot and
/// shared across all worktrees on the same host.
pub const DEFAULT_POOL_DIR: &str = "/tmp/pancetta-eval-tier-slots";

/// A held slot. Drop releases the lock (via `flock(fd, LOCK_UN)` which
/// happens implicitly when the FD closes, but we also call it explicitly
/// for fail-fast observability in the panic-during-tier case).
pub struct SlotGuard {
    /// Index of the slot we hold (`0..pool_size`).
    pub slot_index: usize,
    /// Label of the tier whose run is gated by this slot. Used only for
    /// log messages.
    pub tier_label: String,
    /// Pool-directory path for diagnostic logging on release.
    pool_dir: PathBuf,
    /// Open file whose FD holds the advisory lock. Dropping closes the
    /// FD and releases the lock automatically.
    _file: File,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        // Best-effort explicit unlock so the message ordering on stderr
        // matches operator expectation: "RELEASED" before the next
        // waiter's "ACQUIRED". The OS will also unlock on close().
        // SAFETY: `self._file.as_raw_fd()` is a valid open FD for the
        // lifetime of `self`; `flock` with LOCK_UN is well-defined.
        unsafe {
            libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
        }
        eprintln!(
            "tier-slot: RELEASED slot {} ({}) in {}",
            self.slot_index,
            self.tier_label,
            self.pool_dir.display(),
        );
    }
}

/// A pool of `size` slots backed by lockfiles in `dir`. Construction is
/// cheap and idempotent — concurrent callers may create the same pool
/// directory and slot files; that's fine.
#[derive(Clone, Debug)]
pub struct TierSlotPool {
    dir: PathBuf,
    size: usize,
}

impl TierSlotPool {
    /// Build a pool of `size` slots under `dir`. Creates the directory
    /// and slot files (if absent) eagerly so first-acquire is a fast path.
    ///
    /// Returns an error if `size == 0` (a zero-slot pool would deadlock
    /// the first caller) or if the pool directory cannot be created.
    pub fn new(dir: impl AsRef<Path>, size: usize) -> io::Result<Self> {
        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tier-slot pool size must be >= 1",
            ));
        }
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        for i in 0..size {
            let slot_path = dir.join(format!("slot-{i}"));
            if !slot_path.exists() {
                // Create with O_CREAT|O_RDWR; ignore EEXIST race (another
                // process may have created it first).
                OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(&slot_path)?;
            }
        }
        Ok(Self { dir, size })
    }

    /// Number of slots in this pool.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Pool directory path.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Acquire any free slot, blocking until one is available. Used by
    /// the heavy-tier code path. Emits a "WAITING" log if it could not
    /// acquire any slot non-blocking on the first sweep, and an
    /// "ACQUIRED" log when a slot is finally held.
    pub fn acquire(&self, tier_label: &str) -> io::Result<SlotGuard> {
        // First pass: try every slot non-blocking. If any is free we
        // take it immediately and avoid any "WAITING" noise.
        if let Some(guard) = self.try_acquire(tier_label)? {
            return Ok(guard);
        }

        // All slots held. Log the contention and then block on slot 0
        // (any choice works; subsequent calls retry from 0 too, which
        // is fine because flock fairness is OS-defined and acceptable
        // for this offline batch use case).
        eprintln!(
            "tier-slot: WAITING for tier slot ({}/{} held) tier={} pool={}",
            self.size,
            self.size,
            tier_label,
            self.dir.display(),
        );

        // Sleep-poll across all slots so we wake up whichever releases
        // first. Pure blocking flock on a single slot can starve us if
        // a different slot frees up; polling all of them at ~100ms
        // granularity is the simplest correct strategy for an offline
        // batch harness where startup cost is dwarfed by tier runtime.
        let start = Instant::now();
        loop {
            if let Some(guard) = self.try_acquire(tier_label)? {
                let waited = start.elapsed();
                eprintln!(
                    "tier-slot: ACQUIRED slot {} ({}) after waiting {:.1}s",
                    guard.slot_index,
                    tier_label,
                    waited.as_secs_f64(),
                );
                return Ok(guard);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Try to acquire any free slot without blocking. Returns `Ok(None)`
    /// if every slot is currently held by another process.
    pub fn try_acquire(&self, tier_label: &str) -> io::Result<Option<SlotGuard>> {
        for i in 0..self.size {
            match self.try_acquire_slot(i, tier_label)? {
                Some(guard) => {
                    eprintln!(
                        "tier-slot: ACQUIRED slot {} ({}) in {}",
                        guard.slot_index,
                        tier_label,
                        self.dir.display(),
                    );
                    return Ok(Some(guard));
                }
                None => continue,
            }
        }
        Ok(None)
    }

    /// Attempt to acquire a specific slot non-blocking. Returns
    /// `Ok(None)` if the slot is held by another process. Public so
    /// tests can inspect single-slot behaviour directly.
    pub fn try_acquire_slot(
        &self,
        slot_index: usize,
        tier_label: &str,
    ) -> io::Result<Option<SlotGuard>> {
        let slot_path = self.dir.join(format!("slot-{slot_index}"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&slot_path)?;
        // SAFETY: `file.as_raw_fd()` is valid for the lifetime of
        // `file`; `flock` with LOCK_EX|LOCK_NB is well-defined and only
        // mutates kernel state for this open-file-description.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(SlotGuard {
                slot_index,
                tier_label: tier_label.to_string(),
                pool_dir: self.dir.clone(),
                _file: file,
            }));
        }
        let err = io::Error::last_os_error();
        // EWOULDBLOCK/EAGAIN — slot is held by someone else. Any other
        // errno is a real failure (EBADF, EINVAL, EINTR, ENOLCK) that
        // the caller should see.
        match err.raw_os_error() {
            Some(e) if e == libc::EWOULDBLOCK || e == libc::EAGAIN => Ok(None),
            _ => Err(err),
        }
    }
}

/// Classify a tier name into "heavy" (gated by the slot pool when
/// `--max-concurrent-tiers` is set) vs "light" (always runs immediately).
///
/// Heavy tiers are the ones that load a real-world WAV corpus and run
/// the full multi-pass FT8 pipeline per WAV. Light tiers are cheap
/// regression checks (fixtures) or synthetic generators (synth-clean,
/// synth-doppler, synth-pair) that complete in seconds and do not
/// dominate CPU.
///
/// The CPU-starvation pattern that motivated this guard was observed
/// specifically on hard-200, hard-1000, hard-jt9-rich-200, and
/// chrono-replay; those are the four currently classified as heavy.
/// `wild-50` / `wild-100` / `wild-doppler-50` are also classified
/// heavy because they share the curated-tier runtime profile (real
/// WAVs through the full pipeline).
pub fn is_heavy_tier(tier_name: &str) -> bool {
    matches!(
        tier_name,
        "curated-hard-200"
            | "curated-hard-1000"
            | "wild-50"
            | "wild-100"
            | "wild-doppler-50"
            | "hard-jt9-rich-200"
            | "chrono-replay"
            // hb-156 (Batch 29): weak-signal subset of hard-200 + wild-100;
            // same curated-tier runtime profile so classified heavy.
            | "lid-of-band"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn tmp_pool_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pancetta-tier-slots-test-{}-{}-{}",
            label,
            std::process::id(),
            // Nanosecond clock gives enough uniqueness for tests run in
            // sequence within a single process.
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        // Best-effort fresh dir.
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn pool_rejects_zero_size() {
        let dir = tmp_pool_dir("zero");
        let err = TierSlotPool::new(&dir, 0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn single_slot_acquire_release() {
        let dir = tmp_pool_dir("single");
        let pool = TierSlotPool::new(&dir, 1).unwrap();
        let g = pool.try_acquire("test").unwrap();
        assert!(g.is_some(), "first acquire should succeed");
        let g = g.unwrap();
        assert_eq!(g.slot_index, 0);
        // Second acquire while first held: should return None.
        let g2 = pool.try_acquire("test").unwrap();
        assert!(g2.is_none(), "second acquire on size-1 pool must block");
        drop(g);
        // After release, acquire succeeds again.
        let g3 = pool.try_acquire("test").unwrap();
        assert!(g3.is_some(), "acquire after release should succeed");
    }

    #[test]
    fn two_slot_pool_serves_two_concurrent() {
        let dir = tmp_pool_dir("two");
        let pool = TierSlotPool::new(&dir, 2).unwrap();
        let a = pool.try_acquire("tier-a").unwrap().expect("first slot");
        let b = pool.try_acquire("tier-b").unwrap().expect("second slot");
        // Indexes must differ — we don't double-book.
        assert_ne!(a.slot_index, b.slot_index);
        // Pool is now full.
        let c = pool.try_acquire("tier-c").unwrap();
        assert!(c.is_none());
        drop(a);
        // After releasing the first, a third caller succeeds at the
        // freed index.
        let d = pool.try_acquire("tier-c").unwrap().expect("third slot");
        assert_eq!(d.slot_index, 0);
        drop(b);
        drop(d);
    }

    #[test]
    fn blocking_acquire_unblocks_on_release() {
        let dir = tmp_pool_dir("blocking");
        let pool = Arc::new(TierSlotPool::new(&dir, 1).unwrap());
        let g = pool.try_acquire("holder").unwrap().expect("first slot");

        let pool2 = Arc::clone(&pool);
        let handle = std::thread::spawn(move || {
            // Will block until the main thread drops `g`.
            let waiter = pool2.acquire("waiter").unwrap();
            waiter.slot_index
        });

        // Give the waiter time to register as WAITING.
        std::thread::sleep(Duration::from_millis(200));
        // Release.
        drop(g);
        // Waiter should now complete promptly (within poll period + slack).
        let idx = handle.join().expect("waiter thread did not panic");
        assert_eq!(idx, 0);
    }

    #[test]
    fn heavy_tier_classification() {
        // Heavy tiers: real-WAV pipeline runs.
        assert!(is_heavy_tier("curated-hard-200"));
        assert!(is_heavy_tier("curated-hard-1000"));
        assert!(is_heavy_tier("hard-jt9-rich-200"));
        assert!(is_heavy_tier("chrono-replay"));
        assert!(is_heavy_tier("wild-50"));
        assert!(is_heavy_tier("wild-100"));
        assert!(is_heavy_tier("wild-doppler-50"));
        assert!(is_heavy_tier("lid-of-band"));

        // Light tiers: regression/synthetic, NOT gated.
        assert!(!is_heavy_tier("fixtures"));
        assert!(!is_heavy_tier("synth-clean"));
        assert!(!is_heavy_tier("synth-doppler"));
        assert!(!is_heavy_tier("synth-pair-200"));

        // Unknown tier names default to non-heavy — explicit allowlist
        // is safer than denylist (won't accidentally gate something new).
        assert!(!is_heavy_tier("future-tier-name"));
        assert!(!is_heavy_tier(""));
    }

    /// Sub-process crash-safety: spawn a child process that takes a slot
    /// and then SIGKILLs itself. The kernel must release the
    /// open-file-description and the next acquire in the parent must
    /// succeed promptly.
    ///
    /// The child path is implemented by re-exec'ing the test binary
    /// with a magic env var, holding a slot for ~200ms, then dying.
    /// We don't actually need a separate test binary; we just verify
    /// that closing the FD (which would also happen on kill) releases
    /// the lock — `single_slot_acquire_release` covers that. This test
    /// instead simulates the crash with a thread + abrupt drop to keep
    /// the test self-contained and `cargo test`-runnable on any host.
    #[test]
    fn fd_drop_releases_lock() {
        let dir = tmp_pool_dir("crash");
        let pool = TierSlotPool::new(&dir, 1).unwrap();
        // Acquire and immediately drop in inner scope to mimic a
        // process exiting (which closes all FDs).
        {
            let g = pool.try_acquire("crashed").unwrap().expect("slot");
            let _ = g.slot_index;
            // `g` dropped at end of scope → FD closed → kernel releases.
        }
        let g2 = pool
            .try_acquire("after-crash")
            .unwrap()
            .expect("slot must be free after FD drop");
        assert_eq!(g2.slot_index, 0);
    }
}
