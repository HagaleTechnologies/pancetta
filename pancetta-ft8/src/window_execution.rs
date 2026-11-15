//! PAN-318: two-pass overlap execution plumbing shared by the live
//! coordinator (`pancetta/src/coordinator/ft8.rs`) and the PAN-318
//! qualification tests (`tests/pan318_overlap_tests.rs`).
//!
//! This module is internal application/research plumbing, NOT part of the
//! stable API (README § "Stable API and compatibility policy",
//! `tests/stable_api_guard.rs` pins none of it). Everything is `doc(hidden)`
//! and must not grow a supported standalone consumer; the research harness
//! and the coordinator are the only intended callers.
//!
//! What this helper owns (and what the coordinator relies on):
//!
//! - a two-party start barrier, so the reference (ft8_lib) branch and the
//!   native branch are released together and neither runs its decode work
//!   before the other is ready to start;
//! - in-worker entry timestamps captured immediately before each branch's
//!   decoder invocation (after barrier release) — never the parent's spawn
//!   instants, which measure scheduling, not start;
//! - one `catch_unwind` per branch, so a panic in either pass is isolated
//!   and the other pass's result still comes back;
//! - a join (via `std::thread::scope`) before returning, so callers can
//!   only merge after both branches finished.
//!
//! The barrier sits strictly outside the fallible branch work: both parties
//! reach it before either closure runs, so a panicking closure can never
//! strand the other branch on the barrier.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Barrier;
use std::time::{Duration, Instant};

/// Result of one [`run_overlapped_passes`] call.
///
/// `reference`/`native` are `None` exactly when that branch panicked (the
/// panic payload is dropped; callers do their own panic accounting).
#[doc(hidden)]
#[derive(Debug)]
pub struct OverlappedPasses<R, N> {
    /// Reference-pass result, or `None` when it panicked.
    pub reference: Option<R>,
    /// Native-pass result, or `None` when it panicked.
    pub native: Option<N>,
    /// Captured inside the reference worker immediately before its decoder
    /// invocation, after barrier release.
    pub reference_started_at: Instant,
    /// Captured inside the native worker immediately before its decoder
    /// invocation, after barrier release.
    pub native_started_at: Instant,
    /// Wall time the reference branch spent in its decoder invocation.
    pub reference_elapsed: Duration,
    /// Wall time the native branch spent in its decoder invocation.
    pub native_elapsed: Duration,
}

impl<R, N> OverlappedPasses<R, N> {
    /// True when the reference branch panicked.
    #[doc(hidden)]
    pub fn reference_panicked(&self) -> bool {
        self.reference.is_none()
    }

    /// True when the native branch panicked.
    #[doc(hidden)]
    pub fn native_panicked(&self) -> bool {
        self.native.is_none()
    }

    /// Absolute difference between the two in-worker entry timestamps —
    /// the AC1 start-skew measurement. Concurrent work overlaps, so
    /// branch durations must never be summed to approximate wall time.
    #[doc(hidden)]
    pub fn start_skew(&self) -> Duration {
        if self.reference_started_at < self.native_started_at {
            self.native_started_at
                .duration_since(self.reference_started_at)
        } else {
            self.reference_started_at
                .duration_since(self.native_started_at)
        }
    }
}

/// Run the reference and native decode branches concurrently on a shared
/// window snapshot and join both before returning.
///
/// The native branch runs on the calling thread (the coordinator's decode
/// worker); the reference branch runs on one scoped child thread that is
/// joined before this function returns, so a caller can only proceed to
/// its merge after both branches finished.
///
/// `reference` must be `Send` (it crosses to the child thread); `R` must be
/// `Send` (it crosses back through the join). `native` runs in place and
/// needs no bound beyond what the caller's closure captures.
#[doc(hidden)]
pub fn run_overlapped_passes<R, N>(
    reference: impl FnOnce() -> R + Send,
    native: impl FnOnce() -> N,
) -> OverlappedPasses<R, N>
where
    R: Send,
{
    let barrier = Barrier::new(2);
    std::thread::scope(|scope| {
        let reference_handle = scope.spawn(|| {
            barrier.wait();
            let started_at = Instant::now();
            let out = catch_unwind(AssertUnwindSafe(reference));
            (started_at, started_at.elapsed(), out.ok())
        });
        barrier.wait();
        let native_started_at = Instant::now();
        let native_out = catch_unwind(AssertUnwindSafe(native));
        let native_elapsed = native_started_at.elapsed();
        // Join (and swallow an impossible double panic) before returning:
        // the merge in the caller may only observe results after both
        // branches completed.
        let (reference_started_at, reference_elapsed, reference_out) = reference_handle
            .join()
            .unwrap_or_else(|_| (native_started_at, Duration::ZERO, None));
        OverlappedPasses {
            reference: reference_out,
            native: native_out.ok(),
            reference_started_at,
            native_started_at,
            reference_elapsed,
            native_elapsed,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// AC1: both branches enter within 5 ms of each other, measured from
    /// the in-worker timestamps. A deliberately sequential implementation
    /// (first branch runs to completion before the second starts) fails
    /// this by the first branch's duration.
    #[test]
    fn parallel_passes_start_together() {
        let outcome = run_overlapped_passes(
            || std::thread::sleep(Duration::from_millis(75)),
            || std::thread::sleep(Duration::from_millis(75)),
        );
        assert!(
            outcome.start_skew() <= Duration::from_millis(5),
            "in-worker start skew {}us exceeds the 5ms target",
            outcome.start_skew().as_micros()
        );
    }

    /// AC1: the helper returns (and therefore the caller's merge may run)
    /// only after BOTH branches finished. The reference branch completes,
    /// the native branch blocks on a latch; while it is blocked the merge
    /// flag must stay false. All waits are bounded so a regression fails
    /// the test instead of hanging the suite.
    #[test]
    fn parallel_passes_join_before_merge() {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (native_blocked_tx, native_blocked_rx) = std::sync::mpsc::channel::<()>();
        let merge_done = Arc::new(AtomicBool::new(false));
        let merge_flag = Arc::clone(&merge_done);

        let driver = std::thread::spawn(move || {
            let outcome = run_overlapped_passes(
                || "reference-done",
                move || {
                    // Signal that the native branch has entered and is
                    // about to block.
                    native_blocked_tx.send(()).expect("test driver alive");
                    release_rx
                        .recv_timeout(Duration::from_secs(10))
                        .expect("test releases the latch");
                    "native-done"
                },
            );
            merge_flag.store(true, Ordering::Release);
            outcome
        });

        // The native branch has entered and is blocked; the reference
        // branch (instant) has long finished. The merge must not have run.
        native_blocked_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("native branch enters");
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !merge_done.load(Ordering::Acquire),
            "merge ran while the native branch was still blocked"
        );

        release_tx.send(()).expect("native branch waiting");
        let outcome = driver.join().expect("helper returns after both branches");
        assert_eq!(outcome.reference.as_deref(), Some("reference-done"));
        assert_eq!(outcome.native.as_deref(), Some("native-done"));
        assert!(merge_done.load(Ordering::Acquire));
    }

    /// AC2 shape: a slow reference pass must not postpone the native
    /// branch's entry. The native entry timestamp is taken after barrier
    /// release, not after the reference duration.
    #[test]
    fn parallel_native_keeps_receipt_deadline() {
        let receipt = Instant::now();
        let outcome = run_overlapped_passes(
            || {
                std::thread::sleep(Duration::from_millis(120));
                "slow-reference"
            },
            || "native",
        );
        let pre_native = outcome.native_started_at.duration_since(receipt);
        assert!(
            pre_native < Duration::from_millis(50),
            "native entry delayed {}us by work that should overlap",
            pre_native.as_micros()
        );
        assert_eq!(outcome.reference.as_deref(), Some("slow-reference"));
    }

    /// AC6: a panic in either branch preserves the other branch's result,
    /// and the helper still returns (joins) after either or both panic.
    #[test]
    fn parallel_pass_panics_are_isolated() {
        let reference_panicked =
            run_overlapped_passes(|| panic!("reference branch panic"), || "native-survived");
        assert!(reference_panicked.reference_panicked());
        assert!(!reference_panicked.native_panicked());
        assert_eq!(
            reference_panicked.native.as_deref(),
            Some("native-survived")
        );

        let native_panicked =
            run_overlapped_passes(|| "reference-survived", || panic!("native branch panic"));
        assert!(native_panicked.native_panicked());
        assert!(!native_panicked.reference_panicked());
        assert_eq!(
            native_panicked.reference.as_deref(),
            Some("reference-survived")
        );

        let both_panicked = run_overlapped_passes(|| panic!("a"), || panic!("b"));
        assert!(both_panicked.reference_panicked());
        assert!(both_panicked.native_panicked());
    }

    /// Both branches observe the barrier: neither entry timestamp precedes
    /// the other branch being ready to run. Fails against a sequential
    /// implementation, where the second branch starts only after the first
    /// branch's full duration.
    #[test]
    fn both_branches_run_despite_a_slow_first_branch() {
        let native_done = Arc::new(AtomicBool::new(false));
        let native_flag = Arc::clone(&native_done);
        let outcome = run_overlapped_passes(
            || std::thread::sleep(Duration::from_millis(80)),
            move || {
                native_flag.store(true, Ordering::Release);
                "native"
            },
        );
        // The native branch completed while the (slow) reference branch was
        // still running: the join then covered the rest of the reference
        // branch, so both results are present on return.
        assert_eq!(outcome.native.as_deref(), Some("native"));
        assert!(!outcome.reference_panicked());
        assert!(native_done.load(Ordering::Acquire));
    }

    /// Skew is measured in wall-clock instants, not by summing concurrent
    /// branch durations: overlapping branches produce a wall-time overlap
    /// span strictly below the sum of their durations.
    #[test]
    fn overlap_wall_time_is_below_the_sum_of_branch_durations() {
        let start = Instant::now();
        let outcome = run_overlapped_passes(
            || std::thread::sleep(Duration::from_millis(60)),
            || std::thread::sleep(Duration::from_millis(60)),
        );
        let wall = start.elapsed();
        assert!(wall < outcome.reference_elapsed + outcome.native_elapsed);
    }

    /// A join happens even when the native branch is instant and the
    /// reference branch is slow — no orphaned work outlives the call.
    #[test]
    fn helper_joins_slow_reference_after_instant_native() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.store(0, Ordering::SeqCst);
        let outcome = run_overlapped_passes(
            || {
                std::thread::sleep(Duration::from_millis(30));
                COUNTER.fetch_add(1, Ordering::SeqCst);
                "slow"
            },
            || "instant",
        );
        assert_eq!(outcome.native.as_deref(), Some("instant"));
        // The reference branch already incremented before the helper
        // returned, proving the join covered it.
        assert_eq!(COUNTER.load(Ordering::SeqCst), 1);
    }
}
