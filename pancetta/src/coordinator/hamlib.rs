use anyhow::Result;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{debug, error, info, span, warn, Level};

/// Maximum time to wait for the rig's initial connect + frequency read before
/// proceeding with startup.  A QSO takes ≥ 15 s (one FT8 slot) to complete,
/// so anything under that is safe.  If the rig misses this window the poll
/// loop (every 500 ms) will catch up; only the very first seconds of operation
/// may stamp the wrong band.
const RIG_INITIAL_READ_TIMEOUT: Duration = Duration::from_secs(8);

/// PAN-19 HIGH follow-up (round 3, Codex P1): maximum ADDITIONAL time to
/// wait -- on top of whatever `RIG_INITIAL_READ_TIMEOUT` above already
/// spent -- for the Hamlib task's message loop to confirm it has actually
/// started consuming commands, before `start_hamlib_component` returns.
///
/// `TxInhibitGuard` (health.rs) releases the moment `restart_component`'s
/// await -- which bottoms out in this function returning -- completes.
/// Since the HIGH fix above made `children_tx` fire BEFORE the connect/
/// PTT-off/frequency-read sequence (correctly, to stop a slow rig from
/// bailing startup), "children published" no longer implies "the message
/// loop exists to consume a `SetPtt` command" -- that sequence still
/// budgets ~11s worst case (5s connect + 3s PTT-off retry + 3s
/// frequency-read retry). Without this wait, TX could be un-inhibited up
/// to that ~11s BEFORE a queued PTT-on command can actually be consumed,
/// leaving it stuck unprocessed while TX starts keying audio.
///
/// This is independent of, and stacks after, `RIG_INITIAL_READ_TIMEOUT`
/// (both wait on the same underlying "connect/ptt/freq sequence finished"
/// moment, signaled by two separate oneshots fired together -- see
/// `loop_ready_tx`/`initial_read_tx` -- so in the common case this second
/// wait resolves instantly once the first one already did). 15s gives a
/// combined worst-case budget of up to 23s, comfortably past the
/// documented ~11s, before conceding.
///
/// Mirrors `RIG_INITIAL_READ_TIMEOUT`'s fail-safe shape: on timeout, log
/// and continue -- this must NEVER bail/fail the restart (that would
/// reintroduce the exact regression the HIGH fix closed: a slow rig must
/// never hard-fail startup). Being LATE to un-inhibit is safe (TX stays
/// inhibited a few seconds longer than ideal); being EARLY is not (TX
/// could un-mute with a queued PTT-on command still unprocessed) -- this
/// fails safe in the correct direction.
const HAMLIB_LOOP_READY_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) struct HamlibChildren {
    pub(crate) poll: tokio::task::AbortHandle,
    pub(crate) watchdog: tokio::task::AbortHandle,
}

/// How the wait for the Hamlib message loop's readiness signal
/// (`loop_ready_rx`, wrapped in a `tokio::time::timeout`) resolved.
///
/// PAN-19 round-5 review (Codex P1): a oneshot `Receiver<()>`'s `.await`
/// output is `Result<(), RecvError>`, so wrapped in `timeout()` the full
/// type is `Result<Result<(), RecvError>, Elapsed>`. An earlier version of
/// this fix matched a bare `Ok(_)` on the OUTER `Result` as "ready" --
/// which also matches `Ok(Err(RecvError))` (the timeout did NOT elapse,
/// but the sender was dropped WITHOUT sending -- e.g. the spawned Hamlib
/// task panicked or was cancelled after publishing its child handles but
/// before reaching the message loop). Extracted as a small, pure,
/// directly-testable classification so the three outcomes can't be
/// conflated again: only `Ready` is genuine readiness; `SenderDropped` and
/// `TimedOut` are both non-ready, but are NOT interchangeable --
/// `start_hamlib_component` treats a dropped sender as a hard failure
/// (bail; the generation is provably dead) and a timeout as a soft one
/// (log and proceed; the generation may just be slow).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopReadyOutcome {
    /// The message loop confirmed it's actually consuming commands.
    Ready,
    /// The sender was dropped without sending -- the task is gone
    /// (panicked/cancelled) and will never reach its message loop.
    SenderDropped,
    /// Neither happened within the bounded wait; genuinely unknown.
    TimedOut,
}

pub(crate) fn classify_loop_ready(
    result: Result<Result<(), tokio::sync::oneshot::error::RecvError>, tokio::time::error::Elapsed>,
) -> LoopReadyOutcome {
    match result {
        Ok(Ok(())) => LoopReadyOutcome::Ready,
        Ok(Err(_)) => LoopReadyOutcome::SenderDropped,
        Err(_) => LoopReadyOutcome::TimedOut,
    }
}

#[cfg(test)]
mod classify_loop_ready_tests {
    use super::*;

    #[tokio::test]
    async fn ready_when_the_signal_is_genuinely_received() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let _ = tx.send(());
        let result = tokio::time::timeout(Duration::from_millis(50), rx).await;
        assert_eq!(classify_loop_ready(result), LoopReadyOutcome::Ready);
    }

    /// PAN-19 round-5 review (Codex P1): the exact bug this guards
    /// against -- a dropped sender (the spawned Hamlib task panicked or
    /// was cancelled before it could signal ready) must NOT be classified
    /// as `Ready`. This is the discriminator `start_hamlib_component`'s
    /// match now relies on to bail (keeping TX inhibited) instead of
    /// reporting a dead generation as ready to un-inhibit TX.
    #[tokio::test]
    async fn sender_dropped_is_not_confused_with_ready() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        drop(tx);
        let result = tokio::time::timeout(Duration::from_millis(50), rx).await;
        assert_eq!(
            classify_loop_ready(result),
            LoopReadyOutcome::SenderDropped,
            "a dropped sender must be classified as SenderDropped, distinct from Ready"
        );
    }

    /// The flip side: a genuine timeout (sender alive, never sent) must
    /// classify separately from a dropped sender -- `start_hamlib_component`
    /// treats the two very differently (bail vs. soft log-and-continue).
    #[tokio::test]
    async fn genuine_timeout_is_classified_separately_from_a_dropped_sender() {
        let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
        // `_tx` stays alive (not dropped, not sent) -- the wait must
        // genuinely time out rather than resolve early for either reason.
        let result = tokio::time::timeout(Duration::from_millis(20), rx).await;
        assert_eq!(classify_loop_ready(result), LoopReadyOutcome::TimedOut);
    }
}

/// PAN-19 round-11 review (Codex P1): clears `hamlib_command_loop_ready`
/// back to `false` the instant the Hamlib message loop exits, for ANY
/// reason -- normal shutdown, a child-crash bail, a disconnected command
/// channel, or even an unexpected panic.
///
/// Rounds 8-9 correctly set the flag `true` when the loop starts, but
/// there was no symmetric guarantee it went back to `false` when the loop
/// died: `tx_restart_inhibit` isn't raised until the supervisor's own
/// up-to-5s health-check tick notices the finished task, so without this
/// a transmit could pass the TX key-time gate (`tx_hard_mute_reason` in
/// `tx.rs`) during that window and play audio with no PTT command
/// consumer left to key/unkey the rig.
///
/// Matches this codebase's existing RAII-cleanup-on-any-exit pattern --
/// `RemoteTxDisarmGuard` in `station_agent/mod.rs` is the precedent, bound
/// before a critical section starts and unconditionally cleaning up on
/// `Drop` regardless of how the scope exits (including panic-unwind), so
/// the exit path doesn't need to be enumerated one by one.
///
/// PAN-19 round-17 review (Codex P1): "keep readiness cleanup scoped to
/// its Hamlib generation". Round 16 gave the poll and watchdog child
/// tasks their OWN copy of this guard (constructed fresh each generation),
/// which is correct for the normal case -- but it missed `hamlib_orphans`:
/// a watchdog retained across a restart specifically because teardown
/// couldn't confirm PTT-off keeps running (and keeps holding ITS guard,
/// from the OLD generation) until a LATER, successful generation proves
/// itself PTT-safe and aborts it. When that abort finally lands, the OLD
/// guard's `Drop` would otherwise unconditionally clear the SAME shared
/// `hamlib_command_loop_ready` atomic the NEW generation already
/// correctly set `true` -- permanently muting an otherwise healthy
/// restart, since nothing else ever sets it back.
///
/// Fix: every guard now carries the generation it belongs to
/// (`generation`) alongside a shared, live view of whichever generation
/// is CURRENT (`current_generation`, `ApplicationCoordinator::
/// hamlib_generation`, bumped once at the top of every
/// `start_hamlib_component` call). `Drop` only clears the flag if those
/// still match -- a guard from a generation that's since been superseded
/// is a no-op, while a guard whose generation is STILL current (the
/// overwhelmingly common case: normal shutdown, a genuine crash within
/// the same generation) clears exactly as before.
struct HamlibLoopReadyGuard {
    flag: Arc<std::sync::atomic::AtomicBool>,
    generation: u64,
    current_generation: Arc<std::sync::atomic::AtomicU64>,
}

impl HamlibLoopReadyGuard {
    fn new(
        flag: Arc<std::sync::atomic::AtomicBool>,
        generation: u64,
        current_generation: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        Self {
            flag,
            generation,
            current_generation,
        }
    }
}

impl Drop for HamlibLoopReadyGuard {
    fn drop(&mut self) {
        if self.current_generation.load(Ordering::Acquire) == self.generation {
            self.flag.store(false, Ordering::Release);
        }
        // else: a newer generation has since taken over -- this guard
        // belongs to a superseded generation, so its clear is stale and
        // must be a no-op (the flag now reflects the NEW generation's own
        // state, which this guard has no authority over).
    }
}

/// PAN-19 round-17 review (Codex P1): "prevent stale readiness after the
/// liveness check". An ABA race in the message loop's own
/// readiness-reporting call site: `child_crashed()` (which the real call
/// site backs with `child_task_crashed`) could return `false` (all
/// children alive at check time), but a child could then exit in the
/// narrow window between THAT check and the `flag.store(true, ..)` write
/// -- its own per-child guard (round 16) would clear this SAME flag to
/// `false` in that instant, and the write here, already in flight, would
/// then resurrect it back to `true` for a generation that has already
/// lost a command consumer.
///
/// `JoinHandle::is_finished()` (what `child_task_crashed` ultimately
/// reads) is monotonic -- a task that has finished never becomes
/// unfinished again -- so re-running the SAME liveness check immediately
/// AFTER the store closes that exact window: if a child died in the gap,
/// this second check reliably catches it and corrects the flag back down
/// rather than trusting the now-stale positive write. Returns `true` only
/// when readiness was durably published (children were alive both before
/// AND immediately after the store); `false` otherwise, with the flag
/// left (or corrected) to `false`.
///
/// Takes the liveness check as an injectable closure (rather than calling
/// `child_task_crashed` directly) so this exact race is directly,
/// deterministically unit-testable -- a real concurrent child-exit-mid-
/// write race can't be reproduced reliably by timing alone, but a stub
/// closure that returns `false` then `true` reproduces the EXACT sequence
/// of observations the real race produces.
fn publish_loop_readiness_if_children_alive(
    flag: &Arc<std::sync::atomic::AtomicBool>,
    mut child_crashed: impl FnMut() -> bool,
) -> bool {
    if child_crashed() {
        return false;
    }
    flag.store(true, Ordering::Release);
    if child_crashed() {
        // A child died in the check-to-store gap -- the store above just
        // resurrected a stale `true`. Correct it back down.
        flag.store(false, Ordering::Release);
        return false;
    }
    true
}

#[cfg(test)]
mod publish_loop_readiness_if_children_alive_tests {
    use super::*;

    #[test]
    fn publishes_readiness_when_children_stay_alive_through_both_checks() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let published = publish_loop_readiness_if_children_alive(&flag, || false);
        assert!(
            published,
            "readiness must publish when nothing ever crashed"
        );
        assert!(
            flag.load(Ordering::Acquire),
            "the flag must end up true when readiness was published"
        );
    }

    #[test]
    fn withholds_readiness_when_a_child_is_already_dead_before_the_first_check() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let published = publish_loop_readiness_if_children_alive(&flag, || true);
        assert!(
            !published,
            "readiness must be withheld when a child is already dead"
        );
        assert!(
            !flag.load(Ordering::Acquire),
            "the flag must never be set true when a child was already dead"
        );
    }

    /// PAN-19 round-17 review (Codex P1) regression guard: the actual ABA
    /// race this function closes. The first check sees "alive" (`false`);
    /// by the time the second check runs (immediately after the store), a
    /// child has died in that gap (`true`). The stale `true` write must be
    /// corrected back down, not left resurrected.
    #[test]
    fn corrects_a_stale_resurrected_flag_when_a_child_dies_between_the_two_checks() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut call_count = 0;
        let published = publish_loop_readiness_if_children_alive(&flag, || {
            call_count += 1;
            // First call (pre-store check): alive. Second call
            // (post-store recheck): died in the gap.
            call_count > 1
        });
        assert!(
            !published,
            "readiness must be withheld when a child died in the check-to-store gap"
        );
        assert_eq!(call_count, 2, "test setup: both checks must have run");
        assert!(
            !flag.load(Ordering::Acquire),
            "a child dying in the check-to-store gap must correct the flag back to false, not \
             leave it resurrected to true by the in-flight store"
        );
    }
}

#[cfg(test)]
mod hamlib_loop_ready_guard_tests {
    use super::*;

    /// PAN-19 round-11 review (Codex P1) regression guard: after the loop
    /// has reported ready, forcing it to exit must clear
    /// `hamlib_command_loop_ready` back to `false` IMMEDIATELY -- not
    /// waiting for any external supervisor tick (the up-to-5s
    /// `check_task_handles` health-check cycle).
    #[test]
    fn drop_clears_the_flag_immediately() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        flag.store(true, Ordering::Release);
        let current_generation = Arc::new(std::sync::atomic::AtomicU64::new(1));
        {
            let _guard = HamlibLoopReadyGuard::new(flag.clone(), 1, current_generation.clone());
            assert!(
                flag.load(Ordering::Acquire),
                "test setup: flag should be true while guarded"
            );
        }
        assert!(
            !flag.load(Ordering::Acquire),
            "the guard's Drop must clear the readiness flag the instant its scope ends -- not \
             waiting for any external supervisor tick"
        );
    }

    /// The whole point of using an RAII guard here (matching
    /// `RemoteTxDisarmGuard`'s precedent) rather than an explicit
    /// end-of-function call: it must ALSO clear the flag when the scope
    /// exits via an unexpected panic, not just a normal return/bail --
    /// an explicit call at the "end" of the function would never run in
    /// that case.
    #[test]
    fn drop_clears_the_flag_even_on_panic_unwind() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        flag.store(true, Ordering::Release);
        let flag_for_panic = flag.clone();
        let current_generation = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let current_generation_for_panic = current_generation.clone();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = HamlibLoopReadyGuard::new(flag_for_panic, 1, current_generation_for_panic);
            panic!("simulated unexpected panic inside the Hamlib message loop");
        }));
        assert!(
            result.is_err(),
            "test setup: the closure should have panicked"
        );

        assert!(
            !flag.load(Ordering::Acquire),
            "the guard's Drop must clear the readiness flag even when the scope exits via a \
             panic, not just a normal return"
        );
    }

    /// PAN-19 round-17 review (Codex P1) regression guard: "keep readiness
    /// cleanup scoped to its Hamlib generation". The exact scenario the
    /// finding describes: an OLD generation's guard (e.g. an orphaned
    /// watchdog's, retained across a restart because teardown couldn't
    /// confirm PTT-off) is still alive when a NEW generation starts and
    /// correctly sets readiness `true`. When the OLD guard finally drops
    /// (the orphan gets aborted once the new generation proves itself
    /// safe), it must NOT clobber the flag back to `false` -- the flag
    /// belongs to the NEW generation now.
    #[test]
    fn a_stale_guard_from_a_superseded_generation_does_not_clobber_the_current_generations_flag() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let current_generation = Arc::new(std::sync::atomic::AtomicU64::new(1));

        // The OLD generation's guard, tagged generation 1 -- constructed
        // while generation 1 was current, mirroring an orphaned
        // watchdog's guard that outlives its own generation.
        let old_guard = HamlibLoopReadyGuard::new(flag.clone(), 1, current_generation.clone());

        // A NEW generation starts: bumps the shared counter and sets
        // readiness true for itself.
        current_generation.store(2, Ordering::Release);
        flag.store(true, Ordering::Release);

        // The OLD generation's guard is now dropped (its orphaned
        // watchdog finally gets aborted). It must NOT clear the flag --
        // it no longer belongs to the current generation.
        drop(old_guard);

        assert!(
            flag.load(Ordering::Acquire),
            "a stale guard from a superseded generation must not clobber the current \
             generation's readiness flag back to false"
        );
    }

    /// The flip side: a guard whose generation IS still current must keep
    /// clearing the flag exactly as before -- the fix must not become so
    /// conservative that it stops clearing altogether.
    #[test]
    fn a_guard_still_in_its_own_generation_still_clears_the_flag() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let current_generation = Arc::new(std::sync::atomic::AtomicU64::new(1));

        let guard = HamlibLoopReadyGuard::new(flag.clone(), 1, current_generation.clone());
        drop(guard);

        assert!(
            !flag.load(Ordering::Acquire),
            "a guard whose generation is still current must still clear the flag on drop"
        );
    }
}

/// PAN-19 round-16 review (Codex P1): "keep restored rig state pending
/// through CAT application". Bumps `hamlib_command_in_flight`'s count on
/// construction and back down on `Drop` -- unlike `HamlibLoopReadyGuard`
/// above (which only ever clears, never sets), this one brackets a
/// specific, short-lived critical section: the message loop's underlying
/// `set_frequency`/`set_split_freq`/`set_split` CAT call. Constructed
/// immediately before that call starts and dropped immediately after it
/// resolves (success OR failure -- `Drop` doesn't care which), so
/// `tx_hard_mute_reason`'s pending-state check sees "in flight" for the
/// CAT call's entire duration, closing the window round 14's pending-slot
/// check alone couldn't see: a pending command already handed off to the
/// channel (slot cleared) but not yet applied. Same RAII-cleanup-on-any-
/// exit precedent as `HamlibLoopReadyGuard` (`RemoteTxDisarmGuard` in
/// `station_agent/mod.rs`) -- panic-unwind or task cancellation during the
/// CAT call still clears it, so a crash mid-call can never leave PTT
/// permanently muted.
///
/// PAN-19 round-19 review (Codex P1): "count every pending command
/// handoff". `hamlib_command_in_flight` is now a count (`AtomicU32`), not
/// a boolean -- there can legitimately be TWO outstanding handoffs at
/// once (a pending `SetFrequency` AND `SetSplit` delivered together).
/// [`new`] increments on construction and decrements on drop, for a
/// FRESH handoff this guard is entirely responsible for counting.
/// [`adopt`] does NOT increment -- it takes over an EXISTING increment
/// someone else already made (the producer side, `mark_in_flight_then_send`,
/// for a message that turns out NOT to be superseded and is about to be
/// applied) and decrements it on drop, so that increment is retired
/// exactly once, not double-counted.
struct HamlibCommandInFlightGuard(Arc<std::sync::atomic::AtomicU32>);

impl HamlibCommandInFlightGuard {
    /// Increments on construction; use for a handoff this guard is
    /// entirely responsible for counting (nothing else incremented for
    /// it first).
    fn new(count: Arc<std::sync::atomic::AtomicU32>) -> Self {
        count.fetch_add(1, Ordering::AcqRel);
        Self(count)
    }

    /// Does NOT increment -- adopts (takes over responsibility for
    /// decrementing) an increment someone else already made. Use when
    /// this guard is retiring a handoff the producer side already
    /// counted via `mark_in_flight_then_send`, so the SAME increment
    /// isn't counted twice.
    fn adopt(count: Arc<std::sync::atomic::AtomicU32>) -> Self {
        Self(count)
    }
}

impl Drop for HamlibCommandInFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod hamlib_command_in_flight_guard_tests {
    use super::*;

    /// The count must be bumped for the guard's ENTIRE lifetime -- `new`
    /// increments immediately, not lazily.
    #[test]
    fn new_increments_the_count_immediately() {
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let _guard = HamlibCommandInFlightGuard::new(count.clone());
        assert_eq!(
            count.load(Ordering::Acquire),
            1,
            "constructing the guard must bump the in-flight count immediately"
        );
    }

    /// Symmetric with `HamlibLoopReadyGuard`: `Drop` decrements it,
    /// whether the CAT call this guard brackets succeeded or failed -- the
    /// guard itself is outcome-agnostic; `finish_rig_command` handles the
    /// outcome separately (pending slot / tracker), this guard only ever
    /// tracks "how many calls are currently in flight".
    #[test]
    fn drop_decrements_the_count_regardless_of_outcome() {
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        {
            let _guard = HamlibCommandInFlightGuard::new(count.clone());
            assert_eq!(
                count.load(Ordering::Acquire),
                1,
                "test setup: count should be 1 while guarded"
            );
        }
        assert_eq!(
            count.load(Ordering::Acquire),
            0,
            "the guard's Drop must decrement the in-flight count the instant its scope ends"
        );
    }

    /// Same panic-unwind safety net as `HamlibLoopReadyGuard` -- a panic
    /// mid-CAT-call must not leave PTT permanently muted by a stuck
    /// in-flight count.
    #[test]
    fn drop_decrements_the_count_even_on_panic_unwind() {
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_for_panic = count.clone();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = HamlibCommandInFlightGuard::new(count_for_panic);
            panic!("simulated unexpected panic mid-CAT-call");
        }));
        assert!(
            result.is_err(),
            "test setup: the closure should have panicked"
        );

        assert_eq!(
            count.load(Ordering::Acquire),
            0,
            "the guard's Drop must decrement the in-flight count even when the scope exits via \
             a panic"
        );
    }

    /// PAN-19 round-19 review (Codex P1): the actual bug finding #1
    /// describes. TWO outstanding handoffs (frequency + split) at once --
    /// the count must reach 2, and only drop to 0 once BOTH guards have
    /// dropped, not after just the first.
    #[test]
    fn two_concurrent_guards_both_contribute_and_both_must_drop_to_reach_zero() {
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let frequency_guard = HamlibCommandInFlightGuard::new(count.clone());
        let split_guard = HamlibCommandInFlightGuard::new(count.clone());
        assert_eq!(
            count.load(Ordering::Acquire),
            2,
            "two independent handoffs must both contribute to the count"
        );

        drop(frequency_guard);
        assert_eq!(
            count.load(Ordering::Acquire),
            1,
            "dropping only the FIRST of two guards must leave the count at 1, not 0 -- the \
             second handoff is still outstanding"
        );

        drop(split_guard);
        assert_eq!(
            count.load(Ordering::Acquire),
            0,
            "dropping the second guard must bring the count back to 0"
        );
    }

    /// `adopt` must NOT increment -- it takes over an existing increment
    /// (the producer's) rather than adding a new one.
    #[test]
    fn adopt_does_not_increment_but_still_decrements_on_drop() {
        let count = Arc::new(std::sync::atomic::AtomicU32::new(1)); // producer already incremented
        {
            let _guard = HamlibCommandInFlightGuard::adopt(count.clone());
            assert_eq!(
                count.load(Ordering::Acquire),
                1,
                "adopt must not add a SECOND increment on top of the producer's existing one"
            );
        }
        assert_eq!(
            count.load(Ordering::Acquire),
            0,
            "adopt's guard must still decrement (retire) the adopted increment on drop"
        );
    }
}

/// Whether a spawned Hamlib child task (poll loop or PTT watchdog)
/// terminated unexpectedly and the outer message-loop task should treat
/// that as a crash.
///
/// Both children exit cleanly (and expectedly) once `shutdown` flips. A
/// child can observe `shutdown` and return between the outer loop's own
/// `!shutdown.load(...)` guard and this check running -- without
/// re-checking `shutdown` here too, that race gets misclassified as a
/// crash: `teardown_hamlib()` runs (up to ~10s of PTT-off retries) and a
/// Hamlib restart gets dispatched while the process is shutting down
/// (PAN-19 MEDIUM #1).
///
/// Order matters here (PAN-19 round-1 review, Codex P2): evaluate
/// `is_finished()` FIRST and only THEN re-check `shutdown`, not the other
/// way around. Checking `shutdown` before `is_finished()` still leaves a
/// gap -- if `shutdown` flips true immediately after that read returns
/// `false`, a child can observe the flip, exit because of it, and have
/// `is_finished()` see that exit, but the bail decision would still be
/// gated on the STALE pre-flip `shutdown` read, so it fires anyway. By
/// checking `is_finished()` first (a child must actually be finished for
/// the `&&` to even reach the second operand) and re-reading `shutdown`
/// only at that point -- as close in time to the observed exit as
/// possible -- the second read is far more likely to already reflect a
/// flip that caused it.
///
/// PAN-28 (Codex round-1 on PR #303): generic over the two reads, rather
/// than a concrete `&AtomicBool`/`&[JoinHandle]` wrapper delegating to a
/// separately-tested ordering helper, so the check-order regression test
/// (`child_task_crashed_tests::fixed_order_survives_a_shutdown_flip_the_old_order_misses`)
/// drives THIS EXACT function with scripted reads instead of a real race
/// -- a prior version split the ordering into an extracted
/// `crashed_by_check_order` helper that the test drove directly, which
/// left the WIRING at each real call site (which closure is passed as
/// `is_finished` vs `shutdown_is_set`, or whether a future edit bypasses
/// the helper and inlines the check differently) completely untested: the
/// helper's own test would keep passing even if a call site's wiring, or
/// the body here, regressed to the old (broken) order. Collapsing them
/// into one generic function closes that gap — there is no other place
/// the ordering logic could live.
pub(crate) fn child_task_crashed(
    mut is_finished: impl FnMut() -> bool,
    mut shutdown_is_set: impl FnMut() -> bool,
) -> bool {
    is_finished() && !shutdown_is_set()
}

use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};

/// Rig connection state surfaced to the TUI as a station-panel badge.
///
/// The coordinator stores this in an [`std::sync::atomic::AtomicU8`] (see
/// [`ApplicationCoordinator::rig_conn_state`](super::ApplicationCoordinator))
/// written by the hamlib connect/poll loop. Round-trips via
/// [`RigConnState::as_u8`] / [`RigConnState::from_u8`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum RigConnState {
    /// No connection attempted yet, or rig control disabled (mock rig).
    #[default]
    NotConnected,
    /// Connected to rigctld and last poll succeeded.
    Connected,
    /// Was connected but recent polls are failing (rigctld may have crashed).
    PollingFailed,
}

impl RigConnState {
    /// Stable `u8` encoding for atomic storage (fixed mapping).
    pub(crate) fn as_u8(self) -> u8 {
        match self {
            RigConnState::NotConnected => 0,
            RigConnState::Connected => 1,
            RigConnState::PollingFailed => 2,
        }
    }

    /// Decode from the stable `u8` encoding; unknown values map to the safe
    /// default ([`RigConnState::NotConnected`]).
    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            1 => RigConnState::Connected,
            2 => RigConnState::PollingFailed,
            _ => RigConnState::NotConnected,
        }
    }
}

impl super::ApplicationCoordinator {
    pub(crate) async fn teardown_hamlib(&mut self) {
        for orphan in self.hamlib_orphans.drain(..) {
            orphan.abort();
        }

        let mut ptt_off = false;
        if let Some(rig) = self.rig_handle.as_ref() {
            for attempt in 1..=3 {
                match rig
                    .set_ptt(
                        pancetta_hamlib::Vfo::Current,
                        pancetta_hamlib::PttState::Off,
                    )
                    .await
                {
                    Ok(()) => {
                        ptt_off = true;
                        self.ptt_active.store(false, Ordering::Release);
                        break;
                    }
                    Err(error) => {
                        warn!("Hamlib teardown PTT-off attempt {attempt} failed: {error}");
                        // PAN-19 LOW / round-2 review (Codex P1): this loop
                        // runs on the coordinator's main supervision task
                        // (`run_main_loop`'s `select!` -> `check_task_handles`
                        // -> `handle_finished_task` -> `teardown_component` ->
                        // here), so all 3 attempts x up to 3s send + this
                        // 500ms sleep (up to ~10s worst case) stalls that
                        // task's own re-entry into `select!` -- a second,
                        // concurrent component failure sits undiscovered in
                        // `named_task_handles` until this returns.
                        //
                        // An earlier version of this fix skipped this sleep
                        // whenever another component's task had already
                        // finished, to shorten that stall. Codex's round-2
                        // review correctly flagged that as a regression: it
                        // let an unrelated failure elsewhere burn through all
                        // 3 PTT-off attempts back-to-back, with no pause for
                        // a transiently-unavailable rig link to recover
                        // between tries -- weakening the very confirmed-unkey
                        // guarantee this retry loop exists to provide. If the
                        // failure was the watchdog child itself, the
                        // retained-orphan path means nothing can retry later
                        // either, so the radio could stay keyed into the
                        // restart backoff. That's a correctness regression
                        // that outweighs the latency win, so it's reverted:
                        // this sleep is now unconditional again, regardless
                        // of what else is happening in the coordinator.
                        //
                        // Decoupling this loop from the supervision task
                        // properly (spawning it independently and having
                        // `start_hamlib_component` await its completion
                        // before a new generation's connect sequence, rather
                        // than the direct call chain awaiting it here) would
                        // require `hamlib_orphans`/`hamlib_children` to
                        // become thread-safely shared state so a spawned
                        // task can mutate them without `&mut self` -- a
                        // broader, riskier change to this same TX-safety
                        // -critical surface (including the MEDIUM #2 orphan
                        // -draining fix immediately below) than is
                        // justified for a latency-only improvement. Left as
                        // a read-only, non-mutating diagnostic instead: log
                        // (don't act on) a concurrent failure so it's at
                        // least VISIBLE in the log/diagnostic stream sooner,
                        // without shortening this loop's own pacing at all.
                        if self
                            .named_task_handles
                            .iter()
                            .any(|(id, handle)| *id != ComponentId::Hamlib && handle.is_finished())
                        {
                            warn!(
                                "Hamlib teardown PTT-off retry {attempt}/3: another component's \
                                 task has also finished and is waiting to be processed once \
                                 this teardown completes"
                            );
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }

        if let Some(children) = self.hamlib_children.take() {
            children.poll.abort();
            if ptt_off {
                children.watchdog.abort();
            } else {
                self.hamlib_orphans.push(children.watchdog);
                super::tx::emit_diagnostic(
                    &self.message_bus,
                    "supervisor",
                    pancetta_core::DiagnosticLevel::Error,
                    "Hamlib teardown could not force PTT off; watchdog retained".to_string(),
                    None,
                )
                .await;
            }
        }

        if let Ok((sender, receiver)) = self
            .message_bus
            .get_or_create_channel(ComponentId::Hamlib)
            .await
        {
            let mut safe_messages = Vec::new();
            while let Ok(message) = receiver.try_recv() {
                // A stale key-up is the only command that is unsafe after a
                // restart. Preserve unkeys, frequency, mode, and split state
                // so teardown cannot silently desynchronize the rig.
                if !matches!(
                    message.message_type,
                    MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                        state: true
                    })
                ) {
                    safe_messages.push(message);
                }
            }
            for message in safe_messages {
                self.replay_or_fallback(&sender, message).await;
            }
        }
    }

    /// Replay one drained-and-preserved Hamlib message back onto its
    /// channel, with a bounded retry (never an unbounded block) and,
    /// specifically for a preserved `SetPtt { state: false }` unkey, a
    /// last-resort fallback that commands the rig directly rather than
    /// silently dropping it.
    ///
    /// PAN-19 LOW / round-2 review (Codex P1): `Sender::send` on a bounded
    /// crossbeam channel BLOCKS the current thread if the channel is full.
    /// `teardown_hamlib`'s drain (immediately before this is called) just
    /// freed the same number of slots this replays, so normally there's
    /// room -- but a concurrent producer filling the channel in between
    /// (this whole sequence has `.await` points in it) could still leave it
    /// full by the time we get here, which would block this async fn's
    /// underlying executor thread instead of yielding.
    ///
    /// An earlier version of this fix used a single `try_send`, silently
    /// dropping the message on `Full`. Codex's round-2 review correctly
    /// flagged that as unsafe: the message being dropped can be a preserved
    /// `SetPtt { state: false }` unkey, or frequency/split state, and
    /// dropping it means the restarted consumer never receives it /
    /// operates with stale rig state. Fix (bounded, not unbounded, so this
    /// still can't reintroduce the original blocking risk): a few short
    /// `try_send` attempts with brief waits give a transient producer race
    /// a real chance to clear, so the common case still delivers the
    /// message rather than dropping on the first contention. `SetPtt {
    /// state: false }` additionally gets a last-resort fallback that
    /// bypasses the channel entirely -- these bounded attempts don't just
    /// preserve a MESSAGE, they preserve the actual safety property (the
    /// rig getting the unkey command), so if the channel still won't take
    /// it, command PTT off DIRECTLY via `self.rig_handle` (exactly what the
    /// direct retry loop in `teardown_hamlib` already does) rather than
    /// give up. Only non-safety-critical message types (frequency/mode/
    /// split) are allowed to log-and-drop as an absolute last resort.
    async fn replay_or_fallback(
        &mut self,
        sender: &crossbeam_channel::Sender<ComponentMessage>,
        message: ComponentMessage,
    ) {
        const REPLAY_RETRY_ATTEMPTS: u32 = 5;
        const REPLAY_RETRY_DELAY: Duration = Duration::from_millis(20);

        let is_ptt_off = matches!(
            message.message_type,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt { state: false })
        );

        // `Option` (not a bare `ComponentMessage`) so the borrow checker
        // can see that "not delivered" always leaves a value here to fall
        // back on, across every loop exit path (the `Ok(())` arm consumes
        // it with nothing to give back, but that path always sets
        // `delivered = true`, which we check before ever touching
        // `pending` again below).
        let mut delivered = false;
        let mut pending = Some(message);
        for attempt in 1..=REPLAY_RETRY_ATTEMPTS {
            let attempted = pending.take().expect(
                "pending is always Some at the top of each iteration: only the Ok(()) arm \
                 leaves it None, and that arm always breaks the loop",
            );
            match sender.try_send(attempted) {
                Ok(()) => {
                    delivered = true;
                    break;
                }
                Err(crossbeam_channel::TrySendError::Full(returned)) => {
                    pending = Some(returned);
                    if attempt < REPLAY_RETRY_ATTEMPTS {
                        tokio::time::sleep(REPLAY_RETRY_DELAY).await;
                    }
                }
                Err(crossbeam_channel::TrySendError::Disconnected(returned)) => {
                    // No receiver at all on THIS channel -- retrying here
                    // can't help, but `pending` is still reassigned so the
                    // fallback below (direct PTT-off command, or queuing
                    // SetFrequency/SetSplit for the NEXT generation) still
                    // has the message to work with.
                    pending = Some(returned);
                    break;
                }
            }
        }

        if delivered {
            return;
        }

        let pending = pending
            .expect("not delivered implies the Full/Disconnected arm above reassigned pending");

        if is_ptt_off {
            warn!(
                "Hamlib teardown: replay channel stayed full for a preserved PTT-off command -- \
                 commanding PTT off directly instead of dropping it"
            );
            if let Some(rig) = self.rig_handle.as_ref() {
                match rig
                    .set_ptt(
                        pancetta_hamlib::Vfo::Current,
                        pancetta_hamlib::PttState::Off,
                    )
                    .await
                {
                    Ok(()) => {
                        self.ptt_active.store(false, Ordering::Release);
                    }
                    Err(e) => {
                        error!(
                            "Hamlib teardown: direct PTT-off fallback also failed: {}",
                            e
                        );
                    }
                }
            }
        } else {
            // PAN-19 round-5 review (Codex P1): don't just log-and-drop
            // `SetFrequency`/`SetSplit` here -- stash the LATEST of each
            // into a supervisor-owned pending slot (`self.
            // hamlib_pending_frequency`/`hamlib_pending_split`, living on
            // `self`, not inside the spawned task, so it survives this
            // generation's teardown) and deliver it to the NEXT Hamlib
            // generation once its message loop confirms readiness (see
            // the `LoopReadyOutcome::Ready` arm in `start_hamlib_component`
            // below). `SetFrequency` is largely self-healing (the poll
            // loop re-reads/re-publishes the rig's actual frequency every
            // 500ms), but `SetSplit` is NOT -- nothing else in this
            // codebase re-asserts split state, so silently dropping it
            // could leave the rig holding stale split config (e.g. still
            // split-on with an old TX frequency) with nothing to notice or
            // correct it, a real off-frequency-TX risk. Only the latest of
            // each type is kept (overwrite, not append) -- no need to
            // replay a stale sequence of intermediate changes, just the
            // final desired state. Anything else that reaches here (in
            // practice nothing else survives the drain's safe-message
            // filter and this loop's own message-type handling) still
            // logs-and-drops as a last resort.
            match &pending.message_type {
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetFrequency {
                    vfo,
                    ..
                }) => {
                    warn!(
                        "Hamlib teardown: replay channel stayed full for a SetFrequency \
                         command -- queuing it for the next Hamlib generation instead of \
                         dropping it"
                    );
                    // PAN-19 round-19 review (Codex P1): "preserve the
                    // newest split in teardown fallback" -- same
                    // newest-wins comparison `finish_rig_command` already
                    // uses, reused here (not reimplemented) so this
                    // SEPARATE overwrite site can't revert an already-
                    // retained NEWER pending command to a stale older one.
                    //
                    // PAN-35: keyed by the message's own VFO -- an older
                    // VFO-A command draining through this fallback must
                    // never clobber (or be judged against) a still-correct
                    // pending VFO-B command, and vice versa.
                    let vfo = frequency_vfo(*vfo);
                    if let Ok(mut slots) = self.hamlib_pending_frequency.lock() {
                        if should_replace_pending_slot(&pending, slots.get(&vfo)) {
                            slots.insert(vfo, pending);
                        }
                    }
                }
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                    ..
                }) => {
                    warn!(
                        "Hamlib teardown: replay channel stayed full for a SetSplit command -- \
                         queuing it for the next Hamlib generation instead of dropping it \
                         (a dropped SetSplit is not self-healing and could leave the rig \
                         holding stale split state)"
                    );
                    // PAN-19 round-19 review (Codex P1): same reasoning as
                    // SetFrequency above.
                    if let Ok(mut slot) = self.hamlib_pending_split.lock() {
                        if should_replace_pending_slot(&pending, slot.as_ref()) {
                            *slot = Some(pending);
                        }
                    }
                }
                _ => {
                    warn!(
                        "Hamlib teardown: dropping a replayed message, channel still full \
                         after {REPLAY_RETRY_ATTEMPTS} attempts"
                    );
                }
            }
        }
    }

    /// Map rig model name to hamlib model number.
    /// See: https://github.com/Hamlib/Hamlib/wiki/Supported-Radios
    /// Public so `pancetta doctor` (in the bin crate) can pre-validate the
    /// configured model against the same table the spawner uses.
    #[cfg(feature = "pancetta-hamlib")]
    pub fn hamlib_model_id(model: &str) -> Option<u32> {
        match model.to_lowercase().replace(['-', ' '], "").as_str() {
            "ftdx10" => Some(1042),
            "ftdx101d" | "ftdx101mp" => Some(1040),
            "ft991" | "ft991a" => Some(1036),
            "ft710" => Some(1046),
            "ft891" => Some(1038),
            "ft857" | "ft857d" => Some(1022),
            "ft817" | "ft817nd" => Some(1020),
            "ic7300" => Some(3073),
            "ic7610" => Some(3078),
            "ic7851" => Some(3075),
            "ic705" => Some(3085),
            "ic9700" => Some(3081),
            "ts890" | "ts890s" => Some(2029),
            "ts590" | "ts590s" | "ts590sg" => Some(2026),
            _ => None,
        }
    }

    /// SECURITY (I-10 / I-11): validate the `station.interface.port` device
    /// spec before handing it to rigctld's `-r` argument. Accepts only shapes
    /// that look like a real serial device or a `host:port` network rig.
    /// Linux serial: `/dev/ttyUSB<N>`, `/dev/ttyACM<N>`, `/dev/ttyS<N>`.
    /// macOS serial: `/dev/cu.*`, `/dev/tty.*` (dev machine uses
    /// `/dev/cu.usbserial-*`). Windows serial: `COM<N>`. Network rig:
    /// `host:port`, where `port` parses as a `u16` in `1..=65535` (I-11
    /// port-range check). Everything else (bare `/dev/tty`, `/dev/null`,
    /// malformed/out-of-range network ports, arbitrary paths) is rejected.
    pub(crate) fn device_path_looks_safe(port_field: &str) -> bool {
        // Linux serial: /dev/ttyUSB<N>, /dev/ttyACM<N>, /dev/ttyS<N> with a
        // trailing all-digit index.
        let linux_serial = |prefix: &str| {
            port_field
                .strip_prefix(prefix)
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        };
        if linux_serial("/dev/ttyUSB") || linux_serial("/dev/ttyACM") || linux_serial("/dev/ttyS") {
            return true;
        }
        // macOS callout/tty devices: /dev/cu.* and /dev/tty.* (require a
        // non-empty suffix after the dot so a bare "/dev/tty" is rejected).
        if let Some(suffix) = port_field
            .strip_prefix("/dev/cu.")
            .or_else(|| port_field.strip_prefix("/dev/tty."))
        {
            return !suffix.is_empty();
        }
        // Windows serial: COM<N>.
        if let Some(n) = port_field.strip_prefix("COM") {
            return !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
        }
        // Network rig: host:port. Port must be a valid u16 (1..=65535).
        // rsplit so IPv6-ish hosts still parse on the final ':' segment;
        // host non-emptiness is required, but host *content* stays a warn
        // (see RIGCTLD_HOST handling) for remote-rig operability.
        if let Some((host, port)) = port_field.rsplit_once(':') {
            if host.is_empty() {
                return false;
            }
            return matches!(port.parse::<u16>(), Ok(p) if p >= 1);
        }
        false
    }

    #[cfg(feature = "pancetta-hamlib")]
    pub(crate) async fn start_hamlib_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_hamlib");
        let _enter = span.enter();

        info!("Starting Hamlib component");

        // PAN-19 round-7 review (Codex P1): reset the independent
        // TX-key-time readiness flag at the START of every call (first
        // boot AND every restart) -- PTT stays blocked (via
        // `tx_hard_mute_reason` in `tx.rs`) until it's explicitly set back
        // to `true` below, regardless of what `tx_restart_inhibit`/
        // `TxInhibitGuard` are doing. See `hamlib_command_loop_ready`'s
        // doc comment in `coordinator/mod.rs` for the full reasoning.
        self.hamlib_command_loop_ready
            .store(false, Ordering::Release);

        // PAN-19 round-17 review (Codex P1): "keep readiness cleanup
        // scoped to its Hamlib generation". Bump the generation epoch
        // ONCE, right here, before anything else this call spawns can
        // read it -- every `HamlibLoopReadyGuard` constructed during this
        // generation (message loop, poll, watchdog) tags itself with
        // `this_generation`, and its `Drop` becomes a no-op once a LATER
        // call bumps this counter again (e.g. an orphaned watchdog
        // retained from an OLD generation, per `hamlib_orphans`, whose
        // guard would otherwise clobber the flag a NEW generation already
        // correctly set). See `hamlib_generation`'s doc comment in
        // `coordinator/mod.rs`.
        let this_generation = self.hamlib_generation.fetch_add(1, Ordering::AcqRel) + 1;

        // PAN-19 round-17 review (Codex P1): "cover the pending-command
        // handoff before CAT starts". Reset alongside
        // `hamlib_command_loop_ready` above -- a stuck-nonzero
        // `hamlib_command_in_flight` (e.g. the producer-side handoff set
        // it, then the task that would have cleared it crashed before
        // ever reaching that point) must never survive past the NEXT
        // restart. See that field's doc comment in `coordinator/mod.rs`.
        self.hamlib_command_in_flight.store(0, Ordering::Release);

        if self
            .rigctld_process
            .as_mut()
            .is_some_and(|child| child.try_wait().ok().flatten().is_some())
        {
            self.rigctld_process.take();
        }

        let (hamlib_tx, hamlib_rx) = self
            .message_bus
            .get_or_create_channel(ComponentId::Hamlib)
            .await?;
        let message_bus = self.message_bus.clone();
        let display_feed_enabled = self.display_feed_enabled.clone();

        // Read rig config before spawning
        let rig_config = {
            let config = self.config.read().await;
            config.rig.clone()
        };

        // Use mock rig only if explicitly requested via env var
        let use_mock = std::env::var("PANCETTA_MOCK_RIG")
            .map(|v| v.to_lowercase() == "true" || v == "1")
            .unwrap_or(false);
        let rig_enabled = rig_config.interface.enabled && !use_mock;

        // Spawn rigctld as a managed child process if rig is enabled
        // and no external rigctld is already running
        let rigctld_port: u16 = std::env::var("RIGCTLD_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(4532);
        let rigctld_host =
            std::env::var("RIGCTLD_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());

        // SECURITY (I-11): rigctld talks to the radio over an unauthenticated
        // TCP socket. The default 127.0.0.1 keeps it loopback-only; if the
        // user explicitly sets RIGCTLD_HOST to a non-loopback address, anyone
        // who can reach that port can drive the rig (key TX, change frequency,
        // etc.). We deliberately keep this a *warning*, not a hard reject:
        // some operators legitimately run rigctld on a separate machine
        // (remote rig) and a hard block would break them. (Port-range
        // validation for any `host:port` device spec is enforced below.)
        if rigctld_host != "127.0.0.1" && rigctld_host != "localhost" && rigctld_host != "::1" {
            warn!(
                "RIGCTLD_HOST is set to a non-loopback address ({}). The \
                 rigctld TCP port is unauthenticated; anyone who can reach \
                 it can drive the radio. Use a firewall or revert to \
                 127.0.0.1 if you didn't intend this.",
                rigctld_host
            );
        }

        if rig_enabled {
            // SECURITY (I-10): rig_config.interface.port is interpolated into
            // the rigctld -r argument and identifies the serial device the
            // daemon will open. Args are passed as a vec (no shell), so
            // command-injection isn't a risk, but a hostile/typo'd config
            // could still ask rigctld to open an unrelated path. Restrict to
            // the shapes that look like a real serial / network rig spec
            // (see `device_path_looks_safe`):
            //   - /dev/ttyUSB<N> / ttyACM<N> / ttyS<N>   (Linux USB-serial)
            //   - /dev/cu.* and /dev/tty.*               (macOS — dev machine)
            //   - COM<N>                                 (Windows)
            //   - host:port                              (rigctld network rig)
            let port_field = &rig_config.interface.port;
            if !port_field.is_empty() && !Self::device_path_looks_safe(port_field) {
                warn!(
                    "Refusing to spawn rigctld with suspicious port path \
                     '{}'. Expected /dev/ttyUSB<N>|ttyACM<N>|ttyS<N>, \
                     /dev/cu.*, /dev/tty.*, COM<N>, or host:port (valid \
                     1-65535 port) — adjust station.interface.port in config.",
                    port_field
                );
                report_rig_error(
                    &self.message_bus,
                    format!(
                        "Rig CAT disabled: suspicious [rig.interface].port '{port_field}' — \
                         run `pancetta setup` to pick a real serial port."
                    ),
                )
                .await;
                return Ok(());
            }

            // Check if rigctld is already running
            let already_running =
                tokio::net::TcpStream::connect(format!("{}:{}", rigctld_host, rigctld_port))
                    .await
                    .is_ok();

            if already_running {
                info!(
                    "rigctld already running on {}:{}",
                    rigctld_host, rigctld_port
                );
            } else if let Some(model_id) = Self::hamlib_model_id(&rig_config.model) {
                // rigctld knows the correct serial parameters (stop bits, parity,
                // flow control) for each rig model -- we only need to specify
                // model, port, and baud rate.
                info!(
                    "Spawning rigctld: model={} (hamlib {}), port={}, baud={}",
                    rig_config.model,
                    model_id,
                    rig_config.interface.port,
                    rig_config.interface.baud_rate
                );

                match std::process::Command::new("rigctld")
                    .args([
                        "-m",
                        &model_id.to_string(),
                        "-r",
                        &rig_config.interface.port,
                        "-s",
                        &rig_config.interface.baud_rate.to_string(),
                        "-t",
                        &rigctld_port.to_string(),
                        "-T",
                        &rigctld_host,
                    ])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                {
                    Ok(child) => {
                        info!("rigctld spawned (PID {})", child.id());
                        self.rigctld_process = Some(child);
                        // Give rigctld time to bind the port and open the serial device
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to spawn rigctld: {}. Install hamlib: {}",
                            e,
                            hamlib_install_hint()
                        );
                        report_rig_error(
                            &self.message_bus,
                            format!(
                                "rigctld failed to start ({e}) — no CAT/PTT this session. \
                                 Fix: {}",
                                hamlib_install_hint()
                            ),
                        )
                        .await;
                    }
                }
            } else {
                warn!(
                    "Unknown rig model '{}' -- cannot determine hamlib ID. \
                     Set RIGCTLD_HOST/RIGCTLD_PORT to use an external rigctld.",
                    rig_config.model
                );
                report_rig_error(
                    &self.message_bus,
                    format!(
                        "Unknown rig model '{}' — cannot spawn rigctld (no CAT/PTT). \
                         Run `pancetta setup` to pick a supported model, or run an \
                         external rigctld and set RIGCTLD_HOST/RIGCTLD_PORT.",
                        rig_config.model
                    ),
                )
                .await;
            }
        }

        let rig: Arc<dyn pancetta_hamlib::RigControl + Send + Sync> = if !rig_enabled {
            info!("Rig control disabled, using mock rig");
            Arc::new(pancetta_hamlib::MockRig::default())
        } else {
            info!("Connecting to rigctld at {}:{}", rigctld_host, rigctld_port);
            Arc::new(pancetta_hamlib::RigctldClient::new(
                pancetta_hamlib::RigctldConfig {
                    host: rigctld_host.clone(),
                    port: rigctld_port,
                    ..Default::default()
                },
            ))
        };
        self.rig_handle = Some(rig.clone());

        let operating_frequency_hz = self.operating_frequency_hz.clone();
        let ptt_active = self.ptt_active.clone();
        let last_ptt_on_ms = self.last_ptt_on_ms.clone();
        let tx_restart_inhibit = self.tx_restart_inhibit.clone();
        let rig_conn_state = self.rig_conn_state.clone();
        // C9 dedup anchor (most recent pancetta-initiated SetFrequency) — the
        // poll loop reads it to tell an operator dial move (tear down) from a
        // pancetta-commanded change (already torn down by the TUI / autonomous
        // site; must NOT double-fire).
        let last_freq_command = self.last_freq_command.clone();
        // PAN-19 round-10 review (Codex P1): the poll loop retries
        // `deliver_pending_hamlib_state` every ~500ms tick for the rest of
        // this generation's lifetime, so a command that finds the channel
        // momentarily full on the FIRST attempt (in `start_hamlib_component`,
        // after this task starts) isn't stranded until the next restart --
        // see `deliver_pending_hamlib_state`'s doc comment.
        let hamlib_pending_frequency_for_polling = self.hamlib_pending_frequency.clone();
        let hamlib_pending_split_for_polling = self.hamlib_pending_split.clone();
        let hamlib_tx_for_polling = hamlib_tx.clone();
        // PAN-19 round-17 review (Codex P1): "cover the pending-command
        // handoff before CAT starts" -- see `deliver_pending_hamlib_state`'s
        // doc comment.
        let hamlib_command_in_flight_for_polling = self.hamlib_command_in_flight.clone();
        // PAN-19 round-15 review (Codex P1): "keep TX muted until restored
        // rig state is applied". The message loop's own SetFrequency/
        // SetSplit arms need to reach the pending slots directly -- on a
        // `rig_poll.set_frequency`/`set_split_freq`/`set_split` FAILURE,
        // the loop re-populates the corresponding slot (see those arms
        // below) so the PTT gate (`tx_hard_mute_reason`'s round-14 check)
        // stays closed and the polling task's round-10 retry gets another
        // real attempt at the CAT command -- not just a resend of a
        // message that was already "consumed" off the channel but never
        // actually accepted by the rig.
        let hamlib_pending_frequency_for_loop = self.hamlib_pending_frequency.clone();
        let hamlib_pending_split_for_loop = self.hamlib_pending_split.clone();
        // PAN-19 round-11 review (Codex P1): "do not deliver stale split
        // state after newer commands". The pending-queue retry above
        // doesn't know whether a NEWER SetFrequency/SetSplit has already
        // gone through the normal send path since the pending item was
        // captured -- naively redelivering it could apply a stale value
        // AFTER a correct, newer one, reverting good state (e.g. TX
        // frequency via split) to bad. Track the message `id` (a global,
        // strictly monotonic counter -- see `generate_message_id` in
        // message_bus.rs, already on every `ComponentMessage`) of the most
        // recent SetFrequency/SetSplit command THIS message loop has
        // actually seen, whether it arrived via the normal send path or a
        // pending-queue redelivery -- see the message loop's own
        // SetFrequency/SetSplit arms below for where this gets written,
        // and the poll loop's retry call for where it gets checked before
        // ever re-injecting a stale pending command. Fresh (`None`) each
        // generation -- a pending item surviving a restart is still
        // correctly compared against whatever the NEW generation applies,
        // since the compared `id`s are globally monotonic, not
        // per-generation.
        // PAN-35: keyed by `pancetta_hamlib::Vfo` -- see that field's doc
        // comment on `hamlib_pending_frequency` in `coordinator/mod.rs` for
        // why a single shared value conflated VFO A and VFO B.
        let last_applied_frequency_id: Arc<std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, u64>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let last_applied_split_id: Arc<std::sync::Mutex<Option<u64>>> =
            Arc::new(std::sync::Mutex::new(None));
        let last_applied_frequency_id_for_polling = last_applied_frequency_id.clone();
        let last_applied_split_id_for_polling = last_applied_split_id.clone();
        // A third clone for `start_hamlib_component`'s OWN (first,
        // outside-the-spawned-task) `deliver_pending_hamlib_state` call
        // below -- the other two are moved into the spawned task (one for
        // the message loop's own writes, one further into the poll loop's
        // nested spawn for reads), so this function's own later use needs
        // its own handle taken before that move happens.
        let last_applied_frequency_id_for_start = last_applied_frequency_id.clone();
        let last_applied_split_id_for_start = last_applied_split_id.clone();
        // PAN-19 round-19 review (Codex P1): "retire the handoff marker
        // when discarding stale state". Tracks the message `id` of
        // whichever SetFrequency/SetSplit handoff `mark_in_flight_then_send`
        // (producer side, in `deliver_pending_hamlib_state`) most recently
        // incremented `hamlib_command_in_flight` for -- so that WHOEVER
        // retires this specific message at the consumer side (whether it's
        // actually applied via `HamlibCommandInFlightGuard::adopt`, or
        // discarded as superseded by the message loop's own supersession
        // check without ever reaching a guard) can find and clear the ONE
        // matching increment, instead of leaving it stranded. See
        // `take_producer_mark_if_matching`'s doc comment for the full
        // reasoning. Same per-generation-fresh, per-kind-separate shape as
        // `last_applied_frequency_id`/`last_applied_split_id` above.
        // PAN-35: keyed by VFO, same reasoning as `last_applied_frequency_id`
        // above -- otherwise a VFO-B handoff's mark could overwrite VFO-A's
        // still-unretired one when `deliver_pending_hamlib_state` delivers
        // both in the same pass, stranding VFO-A's in-flight increment.
        let producer_marked_frequency_id: Arc<
            std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, u64>>,
        > = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let producer_marked_split_id: Arc<std::sync::Mutex<Option<u64>>> =
            Arc::new(std::sync::Mutex::new(None));
        let producer_marked_frequency_id_for_polling = producer_marked_frequency_id.clone();
        let producer_marked_split_id_for_polling = producer_marked_split_id.clone();
        let producer_marked_frequency_id_for_start = producer_marked_frequency_id.clone();
        let producer_marked_split_id_for_start = producer_marked_split_id.clone();

        // Oneshot used to gate startup: the spawned task signals here once the
        // initial connect + get_frequency have completed (or failed).  We await
        // this with a bounded timeout so the QSO pipeline doesn't go live while
        // the dial atomic is still 0, which would cause the first-slot QSO
        // completion to log the wrong band.
        let (initial_read_tx, initial_read_rx) = tokio::sync::oneshot::channel::<()>();
        let (children_tx, children_rx) = tokio::sync::oneshot::channel();
        // PAN-19 HIGH follow-up (round 3, Codex P1): a SEPARATE oneshot from
        // `initial_read_tx` -- both fire together, right as the spawned
        // task's message loop is about to start (see `loop_ready_tx.send`
        // below), but `initial_read_rx` is already consumed by its own
        // `tokio::time::timeout(...)` call further down (a `oneshot::
        // Receiver` can't be awaited twice, and a timeout drops the future
        // it wraps), so gating the SEPARATE "safe to un-inhibit TX" wait on
        // it directly isn't possible. See `HAMLIB_LOOP_READY_TIMEOUT` above
        // for why this wait exists at all.
        let (loop_ready_tx, loop_ready_rx) = tokio::sync::oneshot::channel::<()>();
        // PAN-19 round-12 review (Codex P1): "retain the old watchdog until
        // PTT-off is confirmed". A SEPARATE oneshot (not reusing
        // `initial_read_tx`, which is `!rig_enabled`-gated and used for an
        // unrelated purpose) that the spawned task fires right after its
        // own startup connect+PTT-off attempt, carrying whether it's now
        // safe to abort an orphaned watchdog retained from a prior failed
        // teardown: `true` if THIS generation's own `set_ptt(Off)`
        // succeeded, or if it started with an already-active PTT tracker
        // (a real timer its own watchdog will act on) -- see the send
        // site's comment below for the full reasoning. Only awaited when
        // there's actually an orphan to protect (see the drain site) so
        // ordinary startups (no prior failed teardown) never pay for it.
        let (ptt_safe_tx, ptt_safe_rx) = tokio::sync::oneshot::channel::<bool>();

        let hamlib_handle = {
            let shutdown = self.shutdown_signal.clone();
            // PAN-19 round-8 review (Codex P1): the spawned task itself is
            // the true source of truth for this flag -- see the comment at
            // the readiness-reporting call site below for why.
            let hamlib_command_loop_ready = self.hamlib_command_loop_ready.clone();
            // PAN-19 round-17 review (Codex P1): "keep readiness cleanup
            // scoped to its Hamlib generation" -- see `hamlib_generation`'s
            // doc comment in `coordinator/mod.rs`.
            let hamlib_generation = self.hamlib_generation.clone();
            // PAN-19 round-16 review (Codex P1): "keep restored rig state
            // pending through CAT application" -- see this field's doc
            // comment in `coordinator/mod.rs`.
            let hamlib_command_in_flight = self.hamlib_command_in_flight.clone();

            tokio::spawn(async move {
                // PAN-19 HIGH: the poll + PTT-watchdog child tasks are spawned
                // and their abort handles published via `children_tx` FIRST,
                // BEFORE the initial `rig.connect()` / PTT-off / frequency-read
                // sequence below -- deliberately, not incidentally. Spawning
                // them doesn't need a live connection: `rig_poll` is a cheap
                // `Arc::clone` of the same `Arc<dyn RigControl>` `rig` (no
                // network I/O to construct), and the poll loop already
                // tolerates a not-yet-connected rig fine (`get_status()`
                // returns `Disconnected` gracefully rather than erroring, so
                // the first tick or two just count as a `consecutive_failures`
                // poll miss, same as any other transient failure).
                //
                // Previously `children_tx.send` happened AFTER the connect
                // sequence, at essentially the same wall-clock time as
                // `initial_read_tx.send` (only a few non-blocking lines
                // later). Worst case that sequence budgets ~5s (rig connect)
                // + 3s (`set_ptt` retry) + 3s (`get_frequency` retry) ≈ 11s.
                // The caller first awaits `initial_read_rx` with an 8s
                // timeout (`RIG_INITIAL_READ_TIMEOUT`) and, on timeout, logs
                // a warning and continues (fine, working as intended) --
                // but it THEN awaits `children_rx` with a hardcoded 1s
                // timeout starting from t=8s. If T ≈ 11s, `children_tx`
                // fires outside that 8s-9s window, so the 1s wait times out
                // too -- and THAT timeout aborts the task and bails out of
                // `start_hamlib_component` entirely, failing startup (or
                // burning a restart attempt) on exactly the "rig is slow"
                // condition Hamlib restart exists to recover from. Spawning
                // the children up front means `children_tx` fires almost
                // immediately on task start, independent of how long
                // `rig.connect()`/`get_frequency()` take.
                // `message_bus` itself gets moved into the poll task below
                // (it's used there directly, not via a separately-named
                // clone); the connect sequence -- now running AFTER that
                // spawn -- needs its own clone to report a connect failure.
                let message_bus_for_connect = message_bus.clone();
                let rig_poll = Arc::clone(&rig);
                let rig_for_polling = Arc::clone(&rig_poll);
                let shutdown_for_polling = shutdown.clone();
                let op_freq_for_polling = operating_frequency_hz.clone();
                let ptt_active_poll = ptt_active.clone();
                let rig_conn_state_poll = rig_conn_state.clone();
                // C9 dial-poll teardown plumbing.
                let last_freq_command_poll = last_freq_command.clone();
                let bus_for_polling = message_bus.clone();
                let display_feed_enabled_poll = display_feed_enabled.clone();
                // PAN-19 round-16 review (Codex P1): "clear readiness as
                // soon as either Hamlib child exits" -- see the guard
                // construction at the top of this task's body below.
                let hamlib_command_loop_ready_for_polling = hamlib_command_loop_ready.clone();
                // PAN-19 round-17 review (Codex P1): "keep readiness
                // cleanup scoped to its Hamlib generation" -- see
                // `HamlibLoopReadyGuard`'s doc comment.
                let hamlib_generation_for_polling = hamlib_generation.clone();
                let mut spawned_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

                spawned_handles.push(tokio::spawn(async move {
                    // PAN-19 round-16 review (Codex P1): "clear readiness
                    // as soon as either Hamlib child exits". Round 11's
                    // `HamlibLoopReadyGuard` only lives on the message
                    // loop itself, so it only clears readiness when THAT
                    // loop notices (between messages) that a child died --
                    // if this poll task dies (panic, or falls out of its
                    // `while` loop) while the message loop is separately
                    // blocked awaiting a slow multi-second CAT call
                    // (`set_frequency`/`set_split`), readiness stays
                    // `true` for that whole window: a PTT-on could pass
                    // the gate and queue during it, then get consumed and
                    // key the rig once the CAT call finally returns and
                    // the message loop bails, having never actually
                    // checked this child's death before that PTT-on was
                    // already on its way to audio playback. Reusing
                    // `HamlibLoopReadyGuard` here (same guard type as the
                    // message loop's own, same underlying flag) means
                    // THIS child clears readiness itself, directly, the
                    // instant it ends for any reason -- independent of
                    // whether the message loop has gotten back around to
                    // its own `child_task_crashed` check yet.
                    let _poll_ready_guard = HamlibLoopReadyGuard::new(
                        hamlib_command_loop_ready_for_polling,
                        this_generation,
                        hamlib_generation_for_polling,
                    );
                    let mut poll_interval = interval(Duration::from_millis(500));
                    let mut consecutive_failures: u32 = 0;
                    const CRASH_WARN_THRESHOLD: u32 = 10; // 5 seconds of failures
                    // C9 dial-poll band-change detection: the frequency this
                    // poll loop last *accepted* as the current dial. Seeded to
                    // the rig's already-read startup frequency so the first poll
                    // doesn't false-fire (`is_band_change(0, _)` is also false,
                    // belt and braces). Updated only when the loop accepts a new
                    // reading, so a teardown fires at most once per dial move.
                    let mut last_seen_freq: u64 = op_freq_for_polling.load(Ordering::Relaxed);
                    // S-meter poll: every 4th frequency tick (one
                    // STRENGTH read per 2s). Modest on purpose — each
                    // read is a rigctld round-trip on the same serial
                    // CAT link the TX path uses, and the TUI only
                    // renders it for situational awareness.
                    const S_METER_EVERY_N_TICKS: u32 = 4;
                    let mut tick_count: u32 = 0;

                    while !shutdown_for_polling.load(Ordering::Acquire) {
                        poll_interval.tick().await;
                        tick_count = tick_count.wrapping_add(1);

                        // PAN-19 round-10 review (Codex P1): retry any
                        // still-pending SetFrequency/SetSplit command every
                        // tick for the rest of THIS generation's lifetime,
                        // not just once at startup (see
                        // `deliver_pending_hamlib_state`'s doc comment for
                        // the full reasoning). Cheap no-op when both
                        // pending slots are empty (the common case).
                        deliver_pending_hamlib_state(
                            &hamlib_pending_frequency_for_polling,
                            &hamlib_pending_split_for_polling,
                            &last_applied_frequency_id_for_polling,
                            &last_applied_split_id_for_polling,
                            &hamlib_tx_for_polling,
                            &hamlib_command_in_flight_for_polling,
                            &producer_marked_frequency_id_for_polling,
                            &producer_marked_split_id_for_polling,
                        );

                        let poll_ok = if let Ok(status) = rig_for_polling.get_status().await {
                            if status.connection_state
                                == pancetta_hamlib::ConnectionState::Connected
                            {
                                if let Ok(freq) = rig_for_polling
                                    .get_frequency(pancetta_hamlib::Vfo::Current)
                                    .await
                                {
                                    // Update shared operating frequency for spot reporters
                                    op_freq_for_polling.store(freq, Ordering::Relaxed);
                                    let message = ComponentMessage::new(
                                        ComponentId::Hamlib,
                                        ComponentId::Tui,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::FrequencyResponse {
                                                vfo: 0,
                                                frequency: freq,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    let _ = message_bus.send_message(message).await;
                                    super::remote_gateway::relay_to_gateway(
                                        &message_bus,
                                        &display_feed_enabled_poll,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::FrequencyResponse {
                                                vfo: 0,
                                                frequency: freq,
                                            },
                                        ),
                                    )
                                    .await;

                                    // C9 — operator turned the rig's dial. We
                                    // learn of the new dial freq by polling (not
                                    // via the TUI), so this is a band change
                                    // pancetta did not initiate. If it's a real
                                    // band change (not a tiny fine-tune wobble)
                                    // AND not attributable to a freq pancetta
                                    // itself just commanded (the TUI / autonomous
                                    // site already fired the teardown, and the
                                    // rig may still be settling), fire the same
                                    // BandChanged teardown. `last_seen_freq` is
                                    // only advanced once we've decided, so a
                                    // single dial move tears down at most once.
                                    if super::is_band_change(last_seen_freq, freq) {
                                        let cmd = last_freq_command_poll
                                            .lock()
                                            .ok()
                                            .and_then(|g| *g);
                                        let attributable =
                                            super::band_change_attributable_to_command(
                                                freq,
                                                cmd,
                                                Instant::now(),
                                            );
                                        if attributable {
                                            // pancetta commanded this (or the rig
                                            // is still slewing to it) — accept the
                                            // reading without a second teardown.
                                            last_seen_freq = freq;
                                        } else {
                                            info!(
                                                target: "operator.override",
                                                "Rig dial band change {} Hz -> {} Hz (operator) — tearing down active QSOs",
                                                last_seen_freq, freq
                                            );
                                            let teardown = ComponentMessage::new(
                                                ComponentId::Hamlib,
                                                ComponentId::Qso,
                                                MessageType::QsoMessage(
                                                    crate::message_bus::QsoMessage::BandChanged {
                                                        previous_hz: last_seen_freq,
                                                        new_hz: freq,
                                                    },
                                                ),
                                                Instant::now(),
                                            );
                                            if let Err(e) =
                                                bus_for_polling.send_message(teardown).await
                                            {
                                                warn!(
                                                    "Rig dial band change: failed to send teardown: {}",
                                                    e
                                                );
                                            }
                                            last_seen_freq = freq;
                                        }
                                    } else if freq != last_seen_freq {
                                        // Same-band fine-tune / wobble: track the
                                        // new reading but don't tear anything down.
                                        last_seen_freq = freq;
                                    }

                                    // Batch 95: real rig S-meter for the
                                    // TUI. Best-effort — a failed read
                                    // (rig busy, no STRENGTH support)
                                    // skips the update rather than
                                    // counting as a poll failure; the
                                    // TUI shows the reading as stale
                                    // after 10s of silence.
                                    if tick_count.is_multiple_of(S_METER_EVERY_N_TICKS) {
                                        if let Ok(db) = rig_for_polling.get_s_meter().await {
                                            let s_msg = ComponentMessage::new(
                                                ComponentId::Hamlib,
                                                ComponentId::Tui,
                                                MessageType::RigControl(
                                                    crate::message_bus::RigControlMessage::SignalStrengthResponse {
                                                        db_over_s9: db,
                                                    },
                                                ),
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(s_msg).await;
                                            super::remote_gateway::relay_to_gateway(
                                                &message_bus,
                                                &display_feed_enabled_poll,
                                                ComponentId::Hamlib,
                                                MessageType::RigControl(
                                                    crate::message_bus::RigControlMessage::SignalStrengthResponse {
                                                        db_over_s9: db,
                                                    },
                                                ),
                                            )
                                            .await;
                                        }
                                    }

                                    // SWR — only meaningful while keyed (needs
                                    // forward power). Poll every tick during TX so
                                    // the status bar tracks across the ~12.6s
                                    // burst; skipped entirely on RX. Best-effort,
                                    // like the S-meter read.
                                    if ptt_active_poll.load(Ordering::Acquire) {
                                        if let Ok(swr) = rig_for_polling.get_swr().await {
                                            let swr_msg = ComponentMessage::new(
                                                ComponentId::Hamlib,
                                                ComponentId::Tui,
                                                MessageType::RigControl(
                                                    crate::message_bus::RigControlMessage::SwrResponse {
                                                        swr,
                                                    },
                                                ),
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(swr_msg).await;
                                        }
                                    }
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        } else {
                            false
                        };

                        if poll_ok {
                            // Recovered (or steady) — reflect Connected for a
                            // real rig so a transient blip clears the badge.
                            if consecutive_failures > 0 && rig_enabled {
                                rig_conn_state_poll.store(
                                    RigConnState::Connected.as_u8(),
                                    Ordering::Relaxed,
                                );
                            }
                            consecutive_failures = 0;
                        } else {
                            consecutive_failures += 1;
                            if consecutive_failures == CRASH_WARN_THRESHOLD {
                                warn!(
                                    "Rig polling has failed {} consecutive times -- rigctld may have crashed. \
                                     Check rigctld process and restart Pancetta if needed.",
                                    consecutive_failures
                                );
                                // Surface the degraded state to the TUI badge.
                                if rig_enabled {
                                    rig_conn_state_poll.store(
                                        RigConnState::PollingFailed.as_u8(),
                                        Ordering::Relaxed,
                                    );
                                }
                            }
                        }
                    }
                }));

                // PTT safety watchdog: force PTT off if a transmission runs
                // longer than expected. FT8 transmissions are 12.64s within a
                // 15s slot, so 14s is a safe ceiling — long enough for any
                // legitimate FT8 TX, short enough to never bleed into the
                // next slot. Catches stuck/crashed pipelines.
                const PTT_SAFETY_TIMEOUT_SECS: u64 = 14;
                let initial_ptt_on = if ptt_active.load(Ordering::Acquire) {
                    let last_ms = super::now_epoch_ms()
                        .saturating_sub(last_ptt_on_ms.load(Ordering::Acquire));
                    let now = Instant::now();
                    Some(
                        now.checked_sub(Duration::from_millis(last_ms))
                            .unwrap_or(now),
                    )
                } else {
                    None
                };
                let ptt_on_since: Arc<RwLock<Option<Instant>>> =
                    Arc::new(RwLock::new(initial_ptt_on));

                // Spawn the PTT watchdog as a background task
                let rig_for_watchdog = Arc::clone(&rig_poll);
                let ptt_watchdog_tracker = ptt_on_since.clone();
                let shutdown_for_watchdog = shutdown.clone();
                let ptt_active_watchdog = ptt_active.clone();
                // PAN-19 round-16 review (Codex P1): same reasoning as the
                // poll task's own guard above -- see that comment.
                let hamlib_command_loop_ready_for_watchdog = hamlib_command_loop_ready.clone();
                // PAN-19 round-17 review (Codex P1): "keep readiness
                // cleanup scoped to its Hamlib generation" -- CRITICAL for
                // this specific task: if `teardown_hamlib` can't confirm
                // PTT-off, THIS watchdog is the one retained indefinitely
                // in `hamlib_orphans`, still running (and still holding
                // this guard) across however many FUTURE restarts happen
                // before it's finally aborted. Without generation-tagging,
                // its eventual drop would clobber whatever generation is
                // current AT THAT FUTURE POINT -- see
                // `HamlibLoopReadyGuard`'s doc comment.
                let hamlib_generation_for_watchdog = hamlib_generation.clone();
                spawned_handles.push(tokio::spawn(async move {
                    let _watchdog_ready_guard = HamlibLoopReadyGuard::new(
                        hamlib_command_loop_ready_for_watchdog,
                        this_generation,
                        hamlib_generation_for_watchdog,
                    );
                    let mut watchdog_interval = interval(Duration::from_secs(1));
                    loop {
                        watchdog_interval.tick().await;
                        if shutdown_for_watchdog.load(Ordering::Acquire) {
                            break;
                        }

                        let ptt_time = {
                            let guard = ptt_watchdog_tracker.read().await;
                            *guard
                        };

                        if let Some(on_since) = ptt_time {
                            if on_since.elapsed() > Duration::from_secs(PTT_SAFETY_TIMEOUT_SECS) {
                                error!(
                                    "PTT SAFETY WATCHDOG: PTT has been on for >{} seconds -- forcing OFF",
                                    PTT_SAFETY_TIMEOUT_SECS
                                );
                                match rig_for_watchdog
                                    .set_ptt(
                                        pancetta_hamlib::Vfo::Current,
                                        pancetta_hamlib::PttState::Off,
                                    )
                                    .await
                                {
                                    Ok(_) => {
                                        warn!("PTT SAFETY WATCHDOG: PTT forced off successfully");
                                        ptt_active_watchdog.store(false, Ordering::Release);
                                        // Only clear timer on success -- retry on next tick if it fails
                                        let mut guard = ptt_watchdog_tracker.write().await;
                                        *guard = None;
                                    }
                                    Err(e) => {
                                        error!(
                                            "PTT SAFETY WATCHDOG: failed to force PTT off: {} -- will retry in 1s",
                                            e
                                        );
                                    }
                                }
                            }
                        }
                    }
                }));

                let _ = children_tx.send(HamlibChildren {
                    poll: spawned_handles[0].abort_handle(),
                    watchdog: spawned_handles[1].abort_handle(),
                });

                // Initial connect + PTT-off + frequency-read sequence. Uses
                // `rig` directly (not `rig_poll`) -- both are clones of the
                // same underlying `Arc<dyn RigControl>`, so this is
                // equivalent, but keeps the polling task's own capture
                // (`rig_poll`/`rig_for_polling`) visibly separate from this
                // one-shot startup sequence.
                // PAN-19 round-12 review (Codex P1): tracks whether THIS
                // generation's own startup `set_ptt(Off)` succeeded, so it
                // can be combined with `initial_ptt_on` below into the
                // `ptt_safe_tx` signal that gates aborting an orphaned
                // watchdog retained from a prior failed teardown.
                let mut startup_ptt_off_succeeded = false;
                match rig.connect().await {
                    Ok(_) => {
                        info!("Rig connected successfully");
                        match rig
                            .set_ptt(
                                pancetta_hamlib::Vfo::Current,
                                pancetta_hamlib::PttState::Off,
                            )
                            .await
                        {
                            Ok(()) => startup_ptt_off_succeeded = true,
                            Err(e) => warn!("Startup PTT-off failed: {e}"),
                        }
                        // Only flag a *real* CAT link as Connected — a mock rig
                        // (rig control disabled) stays NotConnected so the TUI
                        // badge never claims a radio is attached when none is.
                        if rig_enabled {
                            rig_conn_state
                                .store(RigConnState::Connected.as_u8(), Ordering::Relaxed);
                        }
                        // Read the rig's current frequency immediately so we start
                        // on whatever band the radio is already tuned to, rather
                        // than assuming 20m.
                        match rig.get_frequency(pancetta_hamlib::Vfo::Current).await {
                            Ok(freq) => {
                                operating_frequency_hz.store(freq, Ordering::Relaxed);
                                info!(
                                    "Rig initial frequency: {} Hz ({:.3} MHz)",
                                    freq,
                                    freq as f64 / 1_000_000.0
                                );
                            }
                            Err(e) => {
                                warn!("Could not read initial rig frequency: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to connect to rig: {}. Continuing without.", e);
                        rig_conn_state.store(RigConnState::NotConnected.as_u8(), Ordering::Relaxed);
                        if rig_enabled {
                            report_rig_error(
                                &message_bus_for_connect,
                                format!(
                                    "Rig connect failed ({e}) — RIG badge is ✗. Check the radio \
                                     is powered on and the USB cable; verify with \
                                     `pancetta test-rig`, then restart pancetta."
                                ),
                            )
                            .await;
                        }
                    }
                }

                // PAN-19 round-12 review (Codex P1): "retain the old
                // watchdog until PTT-off is confirmed". Tell
                // `start_hamlib_component` whether it's now safe to abort
                // an orphaned watchdog retained from a prior failed
                // teardown -- `true` if THIS generation's own startup
                // PTT-off just succeeded above, OR if this generation
                // started with `initial_ptt_on` already `Some` (an
                // already-active PTT tracker with a real timer, so THIS
                // generation's own watchdog -- spawned above -- is already
                // capable of protecting against a stuck key on its own).
                // Neither condition holding means nothing has yet
                // established that PTT is safe for this new generation, so
                // the retained orphan -- possibly the only thing still
                // trying to unkey the radio -- must not be aborted yet.
                let _ = ptt_safe_tx.send(startup_ptt_off_succeeded || initial_ptt_on.is_some());

                // Signal that the initial connect + read sequence is done (or
                // gave up).  The receiver in start_hamlib_component is waiting
                // with a bounded timeout; send errors are harmless (timeout
                // already elapsed).
                let _ = initial_read_tx.send(());

                // PAN-19 round-8 review (Codex P1, two findings addressed
                // together at this one call site so neither reintroduces an
                // ordering issue against the other):
                //
                // 1) "Set readiness when the late Hamlib loop starts" -- an
                //    earlier version only sent `loop_ready_tx` here and let
                //    `start_hamlib_component`'s caller-side receive (within
                //    its bounded `HAMLIB_LOOP_READY_TIMEOUT`) be the ONLY
                //    place `hamlib_command_loop_ready` got set `true`. If
                //    that bounded wait had already elapsed (dropping the
                //    receiver) by the time we reach this point -- i.e. the
                //    combined ~23s of waits upstream in
                //    `start_hamlib_component` weren't enough -- this send
                //    would have no receiver, and the atomic would NEVER
                //    flip `true`, permanently muting TX (until the NEXT
                //    restart) even once the rig genuinely recovers. Fix:
                //    the atomic's true source of truth is THIS spawned task
                //    setting it directly, right here, independent of
                //    whether anyone is still listening on the oneshot.
                //    `start_hamlib_component`'s own wait/timeout stays only
                //    as a startup-latency signal for logging/classification
                //    (`LoopReadyOutcome`), not as the sole trigger for the
                //    atomic flag anymore.
                //
                // 2) "Verify child liveness before publishing loop
                //    readiness" -- reporting readiness unconditionally here
                //    ignored that the poll/watchdog children could have
                //    ALREADY crashed during the connect/PTT-off/frequency
                //    -read sequence above -- moments before the `while`
                //    loop's own `child_task_crashed` check (its very first
                //    line) would catch exactly that and bail the whole
                //    generation. That left a brief window where TX was
                //    reported ready/un-muted for a generation already about
                //    to die with no command consumer. Fix: reuse the SAME
                //    `child_task_crashed` check the loop uses on its first
                //    iteration -- if a child is already dead, withhold
                //    readiness entirely (neither the oneshot send nor the
                //    atomic flag) and fall straight through to the loop's
                //    own crash-handling bail below. The caller sees this as
                //    `loop_ready_tx` being dropped without sending, i.e.
                //    `LoopReadyOutcome::SenderDropped`, which already (round
                //    -5) correctly bails and keeps TX inhibited.
                // PAN-19 round-11 review (Codex P1): `_hamlib_loop_ready_guard`
                // lives for the rest of this async block (through the
                // `while` loop below, until the block returns/bails/panics)
                // -- when readiness WAS reported, its `Drop` clears
                // `hamlib_command_loop_ready` back to `false` the instant
                // this task ends, for any reason. `None` when readiness was
                // withheld (child already dead, per round-8): the flag was
                // never set true, so there's nothing to clear.
                //
                // PAN-19 round-17 review (Codex P1): "prevent stale
                // readiness after the liveness check". See
                // `publish_loop_readiness_if_children_alive`'s doc comment
                // for the ABA race this closes.
                let _hamlib_loop_ready_guard =
                    if publish_loop_readiness_if_children_alive(&hamlib_command_loop_ready, || {
                        child_task_crashed(
                            || spawned_handles.iter().any(|handle| handle.is_finished()),
                            || shutdown.load(Ordering::Acquire),
                        )
                    }) {
                        let _ = loop_ready_tx.send(());
                        Some(HamlibLoopReadyGuard::new(
                            hamlib_command_loop_ready.clone(),
                            this_generation,
                            hamlib_generation.clone(),
                        ))
                    } else {
                        // Either already dead before the check, or died in
                        // the check-to-store gap -- report nothing.
                        // `loop_ready_tx` drops without sending when this
                        // task ends (via the bail just below);
                        // `hamlib_command_loop_ready` is left/corrected to
                        // `false`.
                        None
                    };

                // Process messages
                while !shutdown.load(Ordering::Acquire) {
                    if child_task_crashed(
                        || spawned_handles.iter().any(|handle| handle.is_finished()),
                        || shutdown.load(Ordering::Acquire),
                    ) {
                        anyhow::bail!("Hamlib polling or PTT-watchdog child terminated");
                    }
                    match hamlib_rx.try_recv() {
                        Ok(message) => {
                            if let MessageType::RigControl(ref rig_msg) = message.message_type {
                                match rig_msg {
                                    crate::message_bus::RigControlMessage::SetFrequency {
                                        vfo,
                                        frequency,
                                    } => {
                                        // PAN-35: which VFO this specific
                                        // command targets -- every
                                        // supersession/in-flight-mark check
                                        // below is scoped to just this VFO's
                                        // own tracking, never the other's.
                                        let vfo_enum = frequency_vfo(*vfo);
                                        // PAN-19 round-11/12 review (Codex
                                        // P1): re-check supersession right
                                        // here, at the loop's single
                                        // consumption point, so a stale
                                        // pending command that slipped into
                                        // the channel BEHIND a newer one
                                        // already queued (a race the
                                        // send-time check in
                                        // `deliver_pending_hamlib_state`
                                        // can't see) still gets dropped
                                        // instead of applied.
                                        if is_frequency_superseded(
                                            &message,
                                            vfo_enum,
                                            &last_applied_frequency_id,
                                        ) {
                                            // PAN-19 round-19 review (Codex
                                            // P1): "retire the handoff
                                            // marker when discarding stale
                                            // state" -- if this discarded
                                            // message was the producer's
                                            // own marked handoff, nothing
                                            // else will ever retire its
                                            // increment (the guard that
                                            // normally does that is never
                                            // constructed on this
                                            // `continue` path), so retire
                                            // it here directly.
                                            if take_frequency_producer_mark_if_matching(
                                                &message,
                                                vfo_enum,
                                                &producer_marked_frequency_id,
                                            ) {
                                                hamlib_command_in_flight
                                                    .fetch_sub(1, Ordering::AcqRel);
                                            }
                                            warn!(
                                                target: "rig",
                                                "Discarding a stale SetFrequency command \
                                                 superseded by one already applied"
                                            );
                                            continue;
                                        }
                                        // PAN-19 round-15 review (Codex P1):
                                        // "keep TX muted until restored rig
                                        // state is applied". Only record
                                        // this message's id as applied
                                        // (`record_applied`) once
                                        // `set_frequency` has actually
                                        // SUCCEEDED -- recording it
                                        // unconditionally beforehand (the
                                        // pre-round-15 behavior) could mark
                                        // a still-wrong rig state as
                                        // "applied", wrongly superseding a
                                        // still-correct pending item. On
                                        // failure, (re)populate the pending
                                        // slot so the PTT gate stays closed
                                        // and the round-10 polling retry
                                        // gets another real attempt at the
                                        // CAT command -- not just a resend
                                        // of a message already consumed off
                                        // the channel. `finish_rig_command`
                                        // is the shared post-I/O step both
                                        // arms use.
                                        //
                                        // PAN-19 round-16 review (Codex
                                        // P1): "keep restored rig state
                                        // pending through CAT application".
                                        // `_in_flight_guard` marks
                                        // `hamlib_command_in_flight` true
                                        // for exactly the duration of the
                                        // I/O call below -- a PTT-on gated
                                        // through `tx_hard_mute_reason`
                                        // WHILE this await is in flight
                                        // must see the rig's state as not
                                        // yet confirmed, even though the
                                        // pending slot itself may already
                                        // be empty (cleared at hand-off
                                        // time by `deliver_pending_hamlib_state`,
                                        // before this call even started).
                                        let io_ok = {
                                            // PAN-19 round-19 review (Codex
                                            // P1): if this message was the
                                            // producer's own marked
                                            // handoff, ADOPT its existing
                                            // increment (don't add a
                                            // second one) -- otherwise this
                                            // is a fresh send with nothing
                                            // to adopt, so count it
                                            // ourselves.
                                            let _in_flight_guard =
                                                if take_frequency_producer_mark_if_matching(
                                                    &message,
                                                    vfo_enum,
                                                    &producer_marked_frequency_id,
                                                ) {
                                                    HamlibCommandInFlightGuard::adopt(
                                                        hamlib_command_in_flight.clone(),
                                                    )
                                                } else {
                                                    HamlibCommandInFlightGuard::new(
                                                        hamlib_command_in_flight.clone(),
                                                    )
                                                };
                                            match rig_poll.set_frequency(vfo_enum, *frequency).await
                                            {
                                                Ok(()) => true,
                                                Err(e) => {
                                                    error!("Failed to set frequency: {}", e);
                                                    false
                                                }
                                            }
                                        };
                                        finish_frequency_command(
                                            io_ok,
                                            &message,
                                            vfo_enum,
                                            &last_applied_frequency_id,
                                            &hamlib_pending_frequency_for_loop,
                                        );
                                    }
                                    crate::message_bus::RigControlMessage::SetPtt { state } => {
                                        if *state && tx_restart_inhibit.load(Ordering::Acquire) != 0
                                        {
                                            warn!(
                                                "Discarding PTT-on while Hamlib restart inhibit is active"
                                            );
                                            continue;
                                        }
                                        // Update PTT watchdog tracker
                                        {
                                            let mut guard = ptt_on_since.write().await;
                                            if *state {
                                                // PTT going on -- record the time
                                                if guard.is_none() {
                                                    *guard = Some(Instant::now());
                                                    debug!("PTT watchdog: PTT ON, timer started");
                                                }
                                            } else {
                                                // PTT going off -- clear the timer
                                                *guard = None;
                                                debug!("PTT watchdog: PTT OFF, timer cleared");
                                            }
                                        }

                                        let ptt = if *state {
                                            pancetta_hamlib::PttState::On
                                        } else {
                                            pancetta_hamlib::PttState::Off
                                        };
                                        match rig_poll
                                            .set_ptt(pancetta_hamlib::Vfo::Current, ptt)
                                            .await
                                        {
                                            Ok(()) => info!(
                                                target: "pancetta::tx.ptt",
                                                "rig set_ptt {} OK",
                                                if *state { "ON" } else { "OFF" }
                                            ),
                                            Err(e) => error!("Failed to set PTT: {}", e),
                                        }
                                    }
                                    crate::message_bus::RigControlMessage::SetSplit {
                                        enabled,
                                        tx_frequency,
                                    } => {
                                        // PAN-19 round-11/12 review (Codex
                                        // P1): same consumption-time
                                        // supersession re-check as
                                        // SetFrequency above.
                                        if is_superseded(&message, &last_applied_split_id) {
                                            // PAN-19 round-19 review (Codex
                                            // P1): same "retire the
                                            // producer's mark on discard"
                                            // reasoning as SetFrequency
                                            // above.
                                            if take_producer_mark_if_matching(
                                                &message,
                                                &producer_marked_split_id,
                                            ) {
                                                hamlib_command_in_flight
                                                    .fetch_sub(1, Ordering::AcqRel);
                                            }
                                            warn!(
                                                target: "rig",
                                                "Discarding a stale SetSplit command superseded \
                                                 by one already applied"
                                            );
                                            continue;
                                        }
                                        // PAN-19 round-15 review (Codex P1):
                                        // "keep TX muted until restored rig
                                        // state is applied". `split_applied`
                                        // aggregates BOTH underlying CAT
                                        // calls when enabling split
                                        // (`set_split_freq` + `set_split`)
                                        // -- either failing means the rig's
                                        // split state is NOT what this
                                        // message intended, so the message
                                        // is not recorded as applied and
                                        // gets (re)queued for the round-10
                                        // retry instead, exactly like
                                        // SetFrequency above.
                                        // PAN-19 round-16 review (Codex
                                        // P1): "keep restored rig state
                                        // pending through CAT application"
                                        // -- same `_in_flight_guard`
                                        // reasoning as SetFrequency above,
                                        // bracketing BOTH underlying CAT
                                        // calls (`set_split_freq` +
                                        // `set_split`) when enabling.
                                        let split_applied = {
                                            // PAN-19 round-19 review (Codex
                                            // P1): adopt the producer's
                                            // existing increment if this
                                            // is its marked handoff, same
                                            // reasoning as SetFrequency
                                            // above.
                                            let _in_flight_guard = if take_producer_mark_if_matching(
                                                &message,
                                                &producer_marked_split_id,
                                            ) {
                                                HamlibCommandInFlightGuard::adopt(
                                                    hamlib_command_in_flight.clone(),
                                                )
                                            } else {
                                                HamlibCommandInFlightGuard::new(
                                                    hamlib_command_in_flight.clone(),
                                                )
                                            };
                                            if *enabled {
                                                let freq_result =
                                                    rig_poll.set_split_freq(*tx_frequency).await;
                                                if let Err(e) = &freq_result {
                                                    warn!(target: "rig.split", "set_split_freq failed: {}", e);
                                                }
                                                let on_result = rig_poll
                                                    .set_split(true, pancetta_hamlib::Vfo::B)
                                                    .await;
                                                match &on_result {
                                                    Ok(()) => info!(
                                                        target: "rig.split",
                                                        "split ON, TX {} Hz",
                                                        tx_frequency
                                                    ),
                                                    Err(e) => warn!(
                                                        target: "rig.split",
                                                        "set_split(on) failed: {}",
                                                        e
                                                    ),
                                                }
                                                freq_result.is_ok() && on_result.is_ok()
                                            } else {
                                                match rig_poll
                                                    .set_split(false, pancetta_hamlib::Vfo::A)
                                                    .await
                                                {
                                                    Ok(()) => {
                                                        info!(target: "rig.split", "split OFF");
                                                        true
                                                    }
                                                    Err(e) => {
                                                        warn!(target: "rig.split", "set_split(off) failed: {}", e);
                                                        false
                                                    }
                                                }
                                            }
                                        };
                                        finish_rig_command(
                                            split_applied,
                                            &message,
                                            &last_applied_split_id,
                                            &hamlib_pending_split_for_loop,
                                        );
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                    }
                }

                // Cancel spawned polling/watchdog tasks on shutdown
                for handle in spawned_handles {
                    handle.abort();
                }

                info!("Hamlib component stopped");
                Ok(())
            })
        };

        let hamlib_abort = hamlib_handle.abort_handle();
        self.named_task_handles
            .push((ComponentId::Hamlib, hamlib_handle));

        // On the rig-enabled path: block here (bounded) until the spawned task
        // has completed its initial connect + get_frequency sequence.  This
        // guarantees that `operating_frequency_hz` is non-zero before
        // `start_qso_component` runs, so a QSO that completes in the first
        // slot logs the correct band.
        //
        // On the mock / rig-disabled path: the receiver fires immediately after
        // the spawned task calls `initial_read_tx.send(())`, so there's no
        // meaningful delay.
        //
        // If the rig is slow or absent the timeout fires, we log a warning and
        // carry on — the poll loop (every 500 ms) will catch up; only the first
        // few seconds of operation could stamp band=0.
        //
        // PAN-5 verify fix (hamlib.rs restart handshake race): this wait is NOT
        // gated on `first_start`. teardown_hamlib() never clears self.rig_handle,
        // so first_start is false on every restart — gating this wait on it meant
        // a restart fell straight through to the children_rx wait below with only
        // its hardcoded 1s timeout, no time for the spawned task's connect()
        // (RigctldClient budgets 5000ms + retries) + set_ptt(Off) + get_frequency()
        // to complete. That guaranteed the 1s timeout fired on every restart,
        // aborting the task and bail!-ing startup — on exactly the condition
        // (rig/rigctld unreachable) a Hamlib restart exists to recover from.
        // Waiting here on every start means children_tx.send has typically
        // already fired (or is about to) by the time the 1s children_rx wait
        // below runs, regardless of first_start.
        if rig_enabled {
            match tokio::time::timeout(RIG_INITIAL_READ_TIMEOUT, initial_read_rx).await {
                Ok(_) => {
                    info!(
                        target: "rig",
                        "Rig initial frequency read complete before QSO pipeline start"
                    );
                }
                Err(_) => {
                    warn!(
                        target: "rig",
                        "Rig frequency not read within {}s at startup — band may be wrong \
                         until the rig responds or you press a band key",
                        RIG_INITIAL_READ_TIMEOUT.as_secs()
                    );
                }
            }
        }

        self.hamlib_children = Some(
            match tokio::time::timeout(Duration::from_secs(1), children_rx).await {
                Ok(Ok(children)) => children,
                Ok(Err(_)) => {
                    hamlib_abort.abort();
                    anyhow::bail!("Hamlib task exited before publishing child teardown handles");
                }
                Err(_) => {
                    hamlib_abort.abort();
                    anyhow::bail!("Hamlib child teardown handles were not published within 1s");
                }
            },
        );

        // PAN-19 HIGH follow-up (round 3, Codex P1): don't return (and thus
        // don't let the caller's `TxInhibitGuard` release) until the
        // message loop has actually confirmed it can consume commands, or
        // this generous, NON-fatal timeout elapses -- see
        // `HAMLIB_LOOP_READY_TIMEOUT`'s doc comment for the full reasoning.
        // In the common case (rig responded before/around
        // `RIG_INITIAL_READ_TIMEOUT` above already elapsed, or the mock/
        // disabled-rig path) `loop_ready_tx` has already fired by now, so
        // this resolves immediately with no added latency. NEVER bail/fail
        // here on timeout -- a slow rig must still start up successfully;
        // it just means TX stays inhibited a little longer than ideal,
        // which is the safe direction to be wrong in.
        // PAN-19 round-8 review (Codex P1): `hamlib_command_loop_ready` is
        // no longer set from here at all -- the spawned task itself is now
        // the sole source of truth (it sets the atomic directly, right
        // before sending `loop_ready_tx`, guarded by its own
        // `child_task_crashed` check; see that call site above). Setting
        // it AGAIN here on `Ready` would just be a redundant (harmless,
        // but confusing) second writer of the same value; setting it
        // unconditionally on the `!rig_enabled` path (an earlier version
        // did both) could actually be WRONG -- it would report readiness
        // before the spawned task's own liveness check has necessarily
        // run. This wait now exists purely as a startup-latency signal for
        // logging/classification, not as a trigger for the atomic flag.
        if rig_enabled {
            // PAN-19 round-5 review (Codex P1): see `classify_loop_ready`'s
            // doc comment for why the three outcomes are handled
            // differently. `SenderDropped` (the generation is provably
            // dead, OR the spawned task's own child-liveness check withheld
            // readiness per round-8 above) mirrors `children_rx`'s own
            // `Ok(Err(_))` arm just above and bails the same way; that
            // propagates through `restart_component`'s `Err(e)` branch in
            // `health.rs`, where `TxInhibitGuard` is leaked (PAN-19 MEDIUM
            // #3) rather than released for a generation that was never
            // alive to consume anything. `self.hamlib_children`/
            // `self.rig_handle` are left set to this (now-dead)
            // generation's stale values on that path; that's fine --
            // `hamlib_handle` is already registered in `named_task_handles`,
            // so the supervisor's own next `check_task_handles` tick will
            // notice it finished, run `teardown_hamlib()` to clean up
            // properly, and dispatch a fresh restart through the normal
            // crash-recovery path. `TimedOut` stays the soft, non-bailing
            // path -- a slow rig must never hard-fail startup (the
            // regression the HIGH fix closed, which must stay closed) --
            // and, per round-8, no longer needs to do anything to the
            // atomic flag either: the spawned task will have already set
            // it (or not) by the time it actually reaches its loop,
            // independent of whether this wait is still listening.
            match classify_loop_ready(
                tokio::time::timeout(HAMLIB_LOOP_READY_TIMEOUT, loop_ready_rx).await,
            ) {
                LoopReadyOutcome::Ready => {
                    info!(target: "rig", "Hamlib message loop ready to consume commands");
                }
                LoopReadyOutcome::SenderDropped => {
                    hamlib_abort.abort();
                    anyhow::bail!(
                        "Hamlib task exited before its message loop could confirm readiness"
                    );
                }
                LoopReadyOutcome::TimedOut => {
                    warn!(
                        target: "rig",
                        "Hamlib message loop not confirmed ready within an additional {}s -- \
                         proceeding anyway; a queued PTT command may be processed late, and \
                         PTT stays gated at TX key-time until the loop confirms readiness",
                        HAMLIB_LOOP_READY_TIMEOUT.as_secs()
                    );
                }
            }
        }

        // PAN-19 round-5 review (Codex P1): deliver any `SetFrequency`/
        // `SetSplit` command a PRIOR generation's teardown couldn't
        // replay (see `replay_or_fallback`) to THIS generation now that
        // its message loop has either confirmed readiness or we're
        // proceeding best-effort past `HAMLIB_LOOP_READY_TIMEOUT` -- in
        // both cases `hamlib_tx` is a fresh channel the new loop will
        // drain, so a `try_send` here either lands immediately or sits
        // ready for the loop to pick up. Not reached on the
        // `LoopReadyOutcome::SenderDropped` bail path above (this
        // generation is dead; the pending item stays queued for the NEXT
        // restart attempt instead), nor on the mock/disabled-rig path's
        // own equally-fast startup -- called unconditionally here so
        // both get it. PAN-19 round-10 review (Codex P1): this is only
        // the FIRST attempt for this generation -- the Hamlib polling
        // task's own tick loop (see its spawn above) retries this every
        // ~500ms for the rest of the generation's lifetime, so a command
        // that finds the brand-new channel momentarily full right here
        // isn't stranded until the next restart.
        deliver_pending_hamlib_state(
            &self.hamlib_pending_frequency,
            &self.hamlib_pending_split,
            &last_applied_frequency_id_for_start,
            &last_applied_split_id_for_start,
            &hamlib_tx,
            &self.hamlib_command_in_flight,
            &producer_marked_frequency_id_for_start,
            &producer_marked_split_id_for_start,
        );

        // PAN-19 MEDIUM #2 (round 1) + round-12 review (Codex P1) "retain
        // the old watchdog until PTT-off is confirmed": `teardown_hamlib`
        // only drains+aborts `hamlib_orphans` at the START of the NEXT
        // teardown call. A watchdog orphaned by a teardown whose 3 PTT-off
        // attempts all failed (see `teardown_hamlib` below) would
        // otherwise survive past THIS successful restart, holding a stale
        // `ptt_on_since` from the OLD generation -- so it's tempting to
        // abort it as soon as this new generation's children are
        // published. But round-1's original fix did exactly that
        // unconditionally, and round-12 found the real gap: publishing
        // children only means the new poll/watchdog tasks exist, NOT that
        // PTT is confirmed safe -- the new generation's own startup
        // PTT-off (see `ptt_safe_tx.send` above) hasn't necessarily run or
        // succeeded yet, and if the TX worker had already cleared
        // `ptt_active` after queueing an unconsumed unkey, the new
        // watchdog could start with no active timer either. Aborting the
        // retained orphan in that scenario would remove the only task
        // still trying to unkey a physically-keyed radio.
        //
        // Only pay for the wait when there's actually an orphan to
        // protect -- ordinary startups (no prior failed teardown) skip it
        // entirely, so this adds no latency to the common case.
        if !self.hamlib_orphans.is_empty() {
            let confirmed_safe = orphan_safe_to_abort(
                tokio::time::timeout(RIG_INITIAL_READ_TIMEOUT, ptt_safe_rx).await,
            );
            // PAN-19 round-17 review (Codex P1): "recheck watchdog
            // liveness before releasing the orphan". `confirmed_safe`
            // above is a BUFFERED signal that can go stale between when
            // it was sent and now -- re-verify the replacement
            // watchdog's liveness FRESH, immediately before acting on
            // it, rather than trusting only the earlier confirmation.
            let replacement_watchdog_alive = self
                .hamlib_children
                .as_ref()
                .is_some_and(|children| !children.watchdog.is_finished());
            if orphan_release_is_safe(confirmed_safe, replacement_watchdog_alive) {
                for orphan in self.hamlib_orphans.drain(..) {
                    orphan.abort();
                }
            } else {
                warn!(
                    target: "rig",
                    "Hamlib restart: retaining {} orphaned PTT watchdog(s) from a prior \
                     failed teardown -- the new generation hasn't confirmed PTT-off is safe \
                     yet (or its replacement watchdog is no longer alive to act on it); will \
                     retry at the next teardown or restart",
                    self.hamlib_orphans.len()
                );
            }
        }

        info!("Hamlib component started");
        Ok(())
    }
}

/// A live rig-config-switch request (PAN-59), routed from the TUI
/// command-relay task (which only holds cloned `Arc`/channel handles, never
/// `&mut ApplicationCoordinator`) into `run_main_loop` (the only place that
/// already holds `&mut self` in a loop). See
/// `docs/superpowers/specs/2026-09-02-pan-59-live-rig-switch-design.md`.
pub struct HamlibReconnectRequest {
    pub respond: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
}

/// PAN-59 final-review fix (I-1a): thin wrapper the TUI command-relay task
/// (`tui_relay.rs`'s `TuiCommand::SelectRig` arm, which runs regardless of
/// whether `pancetta-hamlib` is compiled in) can call to validate a rig
/// model BEFORE persisting anything or requesting a live reconnect.
/// Without this, an unrecognized `rig.model` string was silently persisted
/// and `start_hamlib_component` would fall through to build a
/// `RigctldClient` pointing at a port nothing is listening on, still
/// reporting success.
///
/// When `pancetta-hamlib` IS compiled in, this defers to the same
/// recognized-model table `start_hamlib_component` itself uses
/// ([`ApplicationCoordinator::hamlib_model_id`]), so the two can never
/// silently disagree. When it is NOT compiled in, there's no model table
/// to check against and the reconnect will already fail with "rig control
/// not compiled in" regardless of the model string -- persisting a model
/// name by itself isn't unsafe, so this permissively returns `true`.
#[cfg(feature = "pancetta-hamlib")]
pub(crate) fn model_recognized(model: &str) -> bool {
    super::ApplicationCoordinator::hamlib_model_id(model).is_some()
}

#[cfg(not(feature = "pancetta-hamlib"))]
pub(crate) fn model_recognized(_model: &str) -> bool {
    true
}

impl super::ApplicationCoordinator {
    /// Handle a PAN-59 live rig-config-switch request: refuse while PTT is
    /// active (tearing down Hamlib mid-key-down would yank CAT/PTT control
    /// out from under an active transmission -- the same safety instinct as
    /// `TxInhibitGuard`), otherwise reconnect via the same
    /// teardown/restart pair the crash-restart path already uses so the
    /// freshly-persisted `self.config.rig` takes effect.
    #[cfg(feature = "pancetta-hamlib")]
    pub(crate) async fn handle_hamlib_reconnect_request(&mut self, req: HamlibReconnectRequest) {
        if self.ptt_active.load(Ordering::Acquire) {
            let _ = req.respond.send(Err(anyhow::anyhow!(
                "cannot switch rig while PTT is active -- release PTT and retry"
            )));
            return;
        }

        // I3 fix (PAN-59 review): the crash-restart path
        // (`health.rs::handle_finished_task`) raises `tx_restart_inhibit`
        // via this same guard BEFORE tearing down, so TX stays hard-muted
        // (through `tx_hard_mute_reason`) for the whole teardown/restart
        // window. The `ptt_active` check above is only a one-time
        // check-then-act load -- without this guard, a `TogglePtt` or the
        // TX worker could still key the rig between that load and
        // `teardown_hamlib`'s first `.await`. Construct it here, before
        // anything else, so the reconnect window is inhibited exactly like
        // a crash-restart window is.
        let tx_inhibit = super::health::TxInhibitGuard::for_component(
            ComponentId::Hamlib,
            self.tx_restart_inhibit.clone(),
        );

        // C2 fix (PAN-59 review): unlike the crash-restart path (which only
        // ever runs after `check_task_handles` has already removed the
        // finished task's entry from `named_task_handles`), this reconnect
        // runs against a Hamlib task that is still ALIVE. If we don't
        // remove+abort its entry here, `teardown_hamlib` aborting its
        // poll/watchdog children causes the OLD message loop to notice and
        // bail within ~10ms -- but its now-finished handle stays in
        // `named_task_handles` (with `start_hamlib_component` below having
        // ALSO pushed a fresh entry for the new generation), so the next
        // `check_task_handles` pass rediscovers the stale OLD handle and
        // processes it as a fresh "crash", dispatching another
        // teardown+restart against the brand-new generation -- which bails
        // the same way, repeating, burning `RestartBudget` slots until TX
        // is permanently inhibited. Removing (and aborting, so it stops
        // racing `teardown_hamlib`'s channel-drain for messages still on
        // the Hamlib bus) the current live entry here, before teardown even
        // starts, means only ONE Hamlib entry -- the new generation's --
        // ever exists once this call returns.
        if let Some(index) = self
            .named_task_handles
            .iter()
            .position(|(id, _)| *id == ComponentId::Hamlib)
        {
            let (_, old_handle) = self.named_task_handles.remove(index);
            old_handle.abort();
        }

        self.teardown_hamlib().await;

        // C1 fix (PAN-59 review): `start_hamlib_component`'s rigctld-spawn
        // logic only spawns a fresh `rigctld` when nothing is already
        // listening on the configured host:port -- an `already_running`
        // TCP-connect probe. The OLD managed `rigctld` (spawned with the
        // OLD `-m model -r port -s baud`) is still bound to that port at
        // this point (nothing else kills it), so without this the probe
        // finds it, skips spawning a new one, and the fresh
        // `RigctldClient` just reconnects to the SAME old daemon --
        // meaning none of the operator's model/port/baud changes ever take
        // effect, even though the call reports success. Kill it now, AFTER
        // `teardown_hamlib` (not before): `teardown_hamlib`'s PTT-off retry
        // loop is a real safety backstop that talks to the rig through
        // `self.rig_handle`, which for this (about-to-be-replaced)
        // generation is still a connection to THIS OLD rigctld -- killing
        // it first would sever that connection and defeat the very retry
        // loop that guarantees the rig gets unkeyed before we tear down.
        if let Some(mut child) = self.rigctld_process.take() {
            info!(
                "PAN-59 rig switch: stopping managed rigctld (PID {}) so a fresh one spawns \
                 with the new model/port/baud",
                child.id()
            );
            let _ = child.kill();
            let _ = child.wait();
        }

        let mut result = self.start_hamlib_component().await;

        // I-1b fix (PAN-59 final review): `start_hamlib_component` returns
        // `Ok(())` on essentially every rig-config failure -- an
        // unrecognized `rig.model` (falls through to build a
        // `RigctldClient` pointing at a port nothing is listening on) or a
        // port that fails `device_path_looks_safe` (returns `Ok(())`
        // *before* ever spawning the message loop, leaving
        // `hamlib_command_loop_ready` permanently `false`) both report
        // success today even though no real CAT/PTT control came up. When
        // rig control is enabled and not mocked, confirm the connection
        // actually came up before telling the operator the switch
        // succeeded.
        //
        // `start_hamlib_component` has already (on the `rig_enabled` path)
        // synchronously awaited its own `initial_read_rx` (connect
        // attempt) and `loop_ready_rx` (message loop confirmation) before
        // returning, so both `rig_conn_state` and
        // `hamlib_command_loop_ready` have normally already settled one
        // way or the other by the time we get here. The short poll below
        // is just a safety margin for the rare case where those internal
        // waits timed out right at their boundary.
        if result.is_ok() {
            let mock_rig = std::env::var("PANCETTA_MOCK_RIG")
                .map(|v| v.to_lowercase() == "true" || v == "1")
                .unwrap_or(false);
            let rig_enabled = {
                let config = self.config.read().await;
                config.rig.interface.enabled
            } && !mock_rig;

            if rig_enabled {
                let cat_up = |this: &Self| {
                    this.rig_conn_state.load(Ordering::Relaxed) == RigConnState::Connected.as_u8()
                        && this.hamlib_command_loop_ready.load(Ordering::Acquire)
                };
                let mut confirmed = cat_up(self);
                for _ in 0..5 {
                    if confirmed {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    confirmed = cat_up(self);
                }
                if !confirmed {
                    result = Err(anyhow::anyhow!(
                        "config saved but CAT did not come up — check rig connection"
                    ));
                }
            }
        }

        match &result {
            Ok(()) => {
                // Normal recovery: let `tx_inhibit` drop here, releasing
                // the inhibit exactly as the crash-restart path's `Ok(())`
                // arm does.
            }
            Err(_) => {
                // Terminal for this attempt: `start_hamlib_component`
                // failed, so (per its own early-return semantics) either no
                // fresh Hamlib task is running at all, or the one it did
                // push already aborted itself -- nothing is left consuming
                // the Hamlib bus channel right now. Leak the guard (mirror
                // `handle_finished_task`'s `leak_tx_inhibit` semantics
                // exactly) so TX stays inhibited rather than un-muting with
                // no confirmed PTT control, and track the leaked increment
                // so a later successful crash-restart recovery pays it back
                // (see `handle_finished_task`'s `Ok(())` arm).
                self.hamlib_leaked_tx_inhibits += 1;
                std::mem::forget(tx_inhibit);
            }
        }

        let _ = req.respond.send(result);
    }

    #[cfg(not(feature = "pancetta-hamlib"))]
    pub(crate) async fn handle_hamlib_reconnect_request(&mut self, req: HamlibReconnectRequest) {
        let _ = req.respond.send(Err(anyhow::anyhow!(
            "rig control not compiled in (pancetta-hamlib feature disabled)"
        )));
    }
}

/// PAN-35 (round-16 review, Codex P2): maps a `SetFrequency` message's raw
/// `vfo: u8` wire field to the physical VFO it targets. Single source for
/// the `0 => A, else => B` convention every SetFrequency call site already
/// used inline (the rig-I/O call below, and now the pending/supersession
/// bookkeeping too) -- reused rather than duplicated so the two can never
/// silently disagree about which VFO a given message means.
fn frequency_vfo(vfo: u8) -> pancetta_hamlib::Vfo {
    if vfo == 0 {
        pancetta_hamlib::Vfo::A
    } else {
        pancetta_hamlib::Vfo::B
    }
}

/// PAN-35 (round-16 review, Codex P2): the VFO-aware sibling of
/// [`is_superseded`]. A single shared `last_applied_id` conflated VFO A and
/// VFO B -- a newer VFO-A command applying would wrongly mark an older,
/// still-correct, still-pending VFO-B command as superseded (changing A
/// does not supersede B). Looks up only the entry for `vfo`, so each VFO's
/// supersession history is tracked and compared independently.
fn is_frequency_superseded(
    message: &ComponentMessage,
    vfo: pancetta_hamlib::Vfo,
    last_applied_id: &std::sync::Arc<std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, u64>>>,
) -> bool {
    last_applied_id
        .lock()
        .ok()
        .and_then(|last| last.get(&vfo).copied())
        .is_some_and(|last_id| last_id > message.id)
}

/// PAN-35: the VFO-aware sibling of [`record_applied`].
fn record_frequency_applied(
    message: &ComponentMessage,
    vfo: pancetta_hamlib::Vfo,
    last_applied_id: &std::sync::Arc<std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, u64>>>,
) {
    if let Ok(mut last) = last_applied_id.lock() {
        last.insert(vfo, message.id);
    }
}

/// PAN-35: the VFO-aware sibling of [`finish_rig_command`]. Reuses
/// [`should_replace_pending_slot`] unchanged (it only ever compared two
/// messages by id, never cared about storage shape) against this VFO's own
/// map entry, so a failed VFO-A retry can never be preserved over -- or
/// clobbered by -- VFO-B's independent retry state.
fn finish_frequency_command(
    io_ok: bool,
    message: &ComponentMessage,
    vfo: pancetta_hamlib::Vfo,
    last_applied_id: &std::sync::Arc<std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, u64>>>,
    pending_slots: &std::sync::Arc<
        std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, ComponentMessage>>,
    >,
) {
    if io_ok {
        record_frequency_applied(message, vfo, last_applied_id);
    } else if let Ok(mut slots) = pending_slots.lock() {
        if should_replace_pending_slot(message, slots.get(&vfo)) {
            slots.insert(vfo, message.clone());
        }
    }
}

/// PAN-35: the VFO-aware sibling of [`mark_in_flight_then_send`]. Marking
/// the in-flight handoff under a single shared `producer_marked_id` (as
/// `mark_in_flight_then_send` does for SetSplit, which has no VFO
/// dimension) would let a VFO-B handoff overwrite VFO-A's still-unretired
/// mark whenever both are delivered in the same
/// `deliver_pending_hamlib_state` pass -- stranding VFO-A's
/// `hamlib_command_in_flight` increment forever (the same failure class
/// round 19 fixed for the single-slot case). Keying by `vfo` keeps the two
/// handoffs' marks independent.
#[allow(clippy::result_large_err)] // ComponentMessage returned so it can be requeued; see mark_in_flight_then_send's own allow.
fn mark_frequency_in_flight_then_send(
    message: ComponentMessage,
    vfo: pancetta_hamlib::Vfo,
    hamlib_command_in_flight: &std::sync::Arc<std::sync::atomic::AtomicU32>,
    producer_marked_id: &std::sync::Arc<std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, u64>>>,
    mut send: impl FnMut(ComponentMessage) -> Result<(), ComponentMessage>,
) -> Result<(), ComponentMessage> {
    hamlib_command_in_flight.fetch_add(1, Ordering::AcqRel);
    if let Ok(mut marked) = producer_marked_id.lock() {
        marked.insert(vfo, message.id);
    }
    match send(message) {
        Ok(()) => Ok(()),
        Err(returned) => {
            hamlib_command_in_flight.fetch_sub(1, Ordering::AcqRel);
            if let Ok(mut marked) = producer_marked_id.lock() {
                marked.remove(&vfo);
            }
            Err(returned)
        }
    }
}

/// PAN-35: the VFO-aware sibling of [`take_producer_mark_if_matching`].
fn take_frequency_producer_mark_if_matching(
    message: &ComponentMessage,
    vfo: pancetta_hamlib::Vfo,
    producer_marked_id: &std::sync::Arc<std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, u64>>>,
) -> bool {
    let Ok(mut marked) = producer_marked_id.lock() else {
        return false;
    };
    if marked.get(&vfo) == Some(&message.id) {
        marked.remove(&vfo);
        true
    } else {
        false
    }
}

/// Deliver any pending `SetFrequency`/`SetSplit` command left over from a
/// prior failed teardown replay (see `replay_or_fallback`) onto `sender`,
/// instead of leaving it silently lost.
///
/// A free function (not a `&mut self` method) because PAN-19 round-10
/// review (Codex P1) needs this reachable from TWO independent call sites
/// that don't share a `&mut self`:
///   - `start_hamlib_component`, once at startup after the new
///     generation's message loop confirms (or is en route to confirming)
///     readiness.
///   - The Hamlib polling task's own ~500ms tick loop, which retries this
///     for the remainder of the CURRENT generation's lifetime -- the
///     original round-7 fix only ever attempted delivery once, at
///     startup; if the brand-new channel happened to be momentarily full
///     at that exact instant, the pending command sat stranded until the
///     NEXT full Hamlib restart even though the channel likely had room
///     again moments later. `pending_frequency`/`pending_split` are
///     `Arc<Mutex<..>>` specifically so both call sites can reach them
///     without `&mut self`.
///
/// Best-effort: a `try_send` failure here just means the channel is
/// somehow already under pressure right now -- log, put the message back
/// in its slot (round-7's fix), and let it try again on the next call
/// (round-10's fix) rather than retrying in a tight loop right here.
///
/// `last_applied_frequency_id`/`last_applied_split_id` are PAN-19 round-11
/// review (Codex P1): "do not deliver stale split state after newer
/// commands". Round-10's periodic retry didn't know whether a NEWER
/// SetFrequency/SetSplit had already gone through the normal send path
/// (applied by the message loop) since the pending item was captured --
/// blindly redelivering it could apply a stale value AFTER a correct,
/// newer one, reverting good state (e.g. TX frequency via split) to bad.
/// Each pending message carries its own globally-monotonic `id` (see
/// `generate_message_id` in message_bus.rs); if the message loop has
/// already applied a SetFrequency/SetSplit with a NEWER id than the
/// pending one, the pending item is provably stale -- drop it (don't put
/// it back in its slot) instead of delivering it.
///
/// PAN-19 round-17 review (Codex P1): "cover the pending-command handoff
/// before CAT starts". Round 16's `HamlibCommandInFlightGuard` only marks
/// `hamlib_command_in_flight` from the CONSUMER side -- when the message
/// loop actually starts the underlying CAT call. That still left a gap
/// one layer earlier: between this function clearing the pending slot and
/// successfully enqueuing the command (producer side, right here) and the
/// message loop actually picking it up and constructing its own guard
/// (consumer side), NEITHER the pending slot NOR the in-flight flag
/// reflected "something is happening" -- a concurrent PTT-on could slip
/// through `tx_hard_mute_reason` in exactly that handoff-to-consumption
/// window. `hamlib_command_in_flight` is set `true` HERE, at hand-off, not
/// just at CAT-call start -- so there is no gap between "no longer
/// pending" and "marked in-flight". The consumer's own guard
/// (`HamlibCommandInFlightGuard`) still owns clearing it once the CAT call
/// resolves (success or failure); setting it again there when the call
/// actually starts is a harmless, redundant re-affirmation of the same
/// `true` this function already set.
///
/// PAN-19 round-18 review (Codex P1): "establish the handoff marker
/// before publishing the command". Round 17's fix marked in-flight AFTER
/// `try_send` succeeded, not before -- on a fast consumer, the full cycle
/// (dequeue -> CAT call completes -> `HamlibCommandInFlightGuard::drop`
/// clears the flag) could finish BEFORE this producer's own delayed
/// `store(true, ..)` executed, letting that stale write resurrect the
/// flag to `true` with nothing left in the pending slot and no guard left
/// to ever clear it -- permanently muting an otherwise healthy
/// generation. Fix: the store now happens BEFORE `try_send`, not after --
/// by the time the consumer could possibly see the message (which can
/// only happen once `try_send` has actually run), the flag is already
/// `true`, so there is no window where the consumer's clear could ever
/// land before this producer's own set. Rolled back (cleared) ONLY on
/// `try_send` failure, when nothing was actually handed off to a
/// consumer -- symmetric with how `HamlibCommandInFlightGuard` clears on
/// the consumption side, so exactly one write establishes `true` for this
/// message's handoff, never two independent writers racing to do it.
/// `true` when the message loop has already applied a command of this kind
/// newer than `message` -- i.e. `message` is stale and must not be
/// (re)delivered or (re)applied. Fails safe toward delivering (returns
/// `false`, i.e. "not known to be stale") on a poisoned tracker lock or if
/// nothing has been applied yet.
///
/// PAN-19 round-12 review (Codex P1): hoisted out of
/// `deliver_pending_hamlib_state` (was a nested fn there) so the message
/// loop's own SetSplit arm can share it as its pre-I/O supersession gate --
/// see that arm's comments, and [`record_applied`], for the post-I/O half
/// of the mechanism (round 15).
///
/// PAN-35: SetFrequency no longer uses this directly -- a single shared
/// `last_applied_id` conflated VFO A and VFO B (changing A wrongly
/// superseded a pending B, or vice versa). Its VFO-keyed sibling,
/// [`is_frequency_superseded`], does the same comparison scoped to one
/// VFO's own tracking. Kept here, unchanged, for SetSplit (which has no
/// VFO dimension).
fn is_superseded(
    message: &ComponentMessage,
    last_applied_id: &std::sync::Arc<std::sync::Mutex<Option<u64>>>,
) -> bool {
    last_applied_id
        .lock()
        .ok()
        .and_then(|last| *last)
        .is_some_and(|last_id| last_id > message.id)
}

/// PAN-19 round-12 review (Codex P1): "discard pending state superseded by
/// queued commands". Round-11's `is_superseded` check ran only inside
/// `deliver_pending_hamlib_state`, at retry-SEND time -- but a newer
/// SetFrequency/SetSplit that had already been sent (via the normal path,
/// not a pending replay) and was sitting in `hamlib_rx` un-consumed at that
/// moment wasn't reflected in `last_applied_*` yet. If the channel had one
/// free slot, the stale pending message got appended BEHIND the newer one
/// already queued; the message loop would then apply the newer command
/// followed by the stale one, reverting good state.
///
/// The message loop's `while` loop is this generation's single
/// serialization point: every SetFrequency/SetSplit, however and whenever
/// it entered `hamlib_rx`, is consumed there one at a time, in true FIFO
/// order. So the loop itself -- not the retry-send site -- is the only
/// place that can see the FINAL order and give a correct, non-racy
/// answer. This is why the message loop's SetFrequency/SetSplit arms call
/// [`is_superseded`] on every message (not just replayed ones) immediately
/// BEFORE attempting to apply it -- skip a superseded one and leave the
/// tracker alone; otherwise attempt the underlying rig I/O.
///
/// PAN-19 round-15 review (Codex P1): "keep TX muted until restored rig
/// state is applied". This function used to ALSO advance the tracker
/// itself, unconditionally, right after the supersession check -- i.e.
/// BEFORE the caller's rig I/O call even ran. That was wrong: if the
/// underlying `set_frequency`/`set_split_freq`/`set_split` call then
/// FAILED (e.g. CAT still recovering mid-restart), the tracker had already
/// advanced past a still-pending, still-correct SetSplit/SetFrequency
/// waiting in the pending slot -- `deliver_pending_hamlib_state`'s own
/// `is_superseded` check would then wrongly judge that pending item stale
/// and drop it, and the PTT gate (`tx_hard_mute_reason`) would see a
/// cleared pending slot and permit TX with the rig still holding its
/// stale, pre-crash state. `record_applied` is now called by the message
/// loop's arms ONLY after the rig I/O has actually succeeded -- never
/// before, and never merely because the message was successfully consumed
/// off the channel.
fn record_applied(
    message: &ComponentMessage,
    last_applied_id: &std::sync::Arc<std::sync::Mutex<Option<u64>>>,
) {
    if let Ok(mut last) = last_applied_id.lock() {
        *last = Some(message.id);
    }
}

/// PAN-19 round-15 review (Codex P1): the shared post-I/O step both the
/// message loop's SetFrequency and SetSplit arms call once they know
/// whether the underlying rig I/O (`set_frequency`/`set_split_freq`/
/// `set_split`) succeeded (`io_ok`). On success, records the message as
/// applied ([`record_applied`]). On failure, (re)populates `pending_slot`
/// with this message so the PTT gate (`tx_hard_mute_reason`'s
/// `has_undelivered_pending_hamlib_state` check, round 14) stays closed
/// and the polling task's round-10 retry gets another real attempt at the
/// actual CAT command -- not just a resend of a message that was already
/// consumed off the channel but never actually accepted by the rig.
///
/// Extracted as its own function (rather than left inline in each arm) so
/// it's directly unit-testable: the message loop's arms live deep inside a
/// giant spawned task with no injectable rig, and `MockRig::set_split`/
/// `set_split_freq` never fail regardless of `failure_rate` (they don't
/// call `simulate_failure` at all), so a genuine CAT-level split failure
/// can't be driven through `start_hamlib_component` end to end -- see
/// `children_publish_race_tests`' doc comment for the same kind of
/// limitation on a different mechanism.
///
/// PAN-19 round-16 review (Codex P1): "preserve the newest failed rig
/// command". On failure, this used to overwrite `pending_slot`
/// unconditionally -- but if a stale (older) message's CAT call fails
/// AFTER a newer message's CAT call already failed and restored ITS OWN
/// (correct, current) state into the slot, the stale failure would
/// clobber it: a later retry would then re-apply the OLD frequency/split
/// instead of the newer desired one, and once THAT retry (wrongly)
/// succeeds, the pending slot clears and PTT is permitted with the wrong
/// state applied. Now only replaces the slot when the failing message's
/// id is NEWER than whatever failed message is already retained there --
/// reusing the same globally-monotonic id ordering [`is_superseded`] and
/// [`record_applied`] already rely on. A failure can still be recorded
/// over an EMPTY slot regardless of id (nothing to preserve), and a
/// failure with an OLDER id than what's retained is simply dropped (not
/// written back) -- the newer failure already correctly represents "what
/// still needs to be corrected".
fn finish_rig_command(
    io_ok: bool,
    message: &ComponentMessage,
    last_applied_id: &std::sync::Arc<std::sync::Mutex<Option<u64>>>,
    pending_slot: &std::sync::Arc<std::sync::Mutex<Option<ComponentMessage>>>,
) {
    if io_ok {
        record_applied(message, last_applied_id);
    } else if let Ok(mut slot) = pending_slot.lock() {
        if should_replace_pending_slot(message, slot.as_ref()) {
            *slot = Some(message.clone());
        }
    }
}

/// PAN-19 round-16 review (Codex P1) finding #2, extracted for round-19
/// review (Codex P1) finding #3: "preserve the newest split in teardown
/// fallback". The single, shared "is `candidate` newer than whatever's
/// already retained" comparison every site that might overwrite a pending
/// slot must use -- `finish_rig_command` (the message loop's own failure
/// path) was the ORIGINAL fix, but `teardown_hamlib`'s SEPARATE replay-
/// fallback path (draining a queued message during teardown, finding the
/// pending slot already occupied) had its own independent, unconditional
/// overwrite that never got the same fix: a newer split could already
/// have failed CAT application and populated the slot, while an OLDER
/// split remained queued behind it when the generation crashed; teardown
/// draining that older message and finding delivery full/disconnected
/// would silently replace the newer desired state with the stale one,
/// and the next generation could then successfully apply the obsolete TX
/// frequency and unmute PTT after the correct state was lost.
///
/// Reusing this ONE comparison everywhere (rather than a second inline
/// copy of `message.id > retained.id`) means a THIRD site that might
/// overwrite a pending slot in the future inherits the correct ordering
/// for free instead of needing to remember to reimplement it.
/// `retained: None` (nothing to preserve) always allows the replace.
fn should_replace_pending_slot(
    candidate: &ComponentMessage,
    retained: Option<&ComponentMessage>,
) -> bool {
    match retained {
        Some(retained) => candidate.id > retained.id,
        None => true,
    }
}

/// PAN-19 round-12 review (Codex P1): "retain the old watchdog until
/// PTT-off is confirmed". `true` only when the new generation's spawned
/// task explicitly confirmed `true` on `ptt_safe_tx` within the bound --
/// i.e. its own startup PTT-off succeeded, or it started with an
/// already-active PTT tracker. Fails SAFE (returns `false`, i.e. "do not
/// abort the retained orphan") on every other outcome: an explicit
/// `false` confirmation, the sender dropping without sending (the spawned
/// task died before reaching that call site), or the bounded wait timing
/// out entirely -- an orphan retained specifically because a prior
/// teardown couldn't confirm PTT-off must never be aborted on anything
/// less than a positive confirmation.
fn orphan_safe_to_abort(
    ptt_safe_outcome: Result<
        Result<bool, tokio::sync::oneshot::error::RecvError>,
        tokio::time::error::Elapsed,
    >,
) -> bool {
    matches!(ptt_safe_outcome, Ok(Ok(true)))
}

/// PAN-19 round-17 review (Codex P1): "recheck watchdog liveness before
/// releasing the orphan". `ptt_safe` (via [`orphan_safe_to_abort`]) can go
/// stale between when it was SENT and when it's actually acted on here --
/// if it vouched for safety only because the REPLACEMENT watchdog
/// inherited an already-active PTT timer (not because startup PTT-off
/// itself succeeded), that vouching is only meaningful while the
/// replacement watchdog is actually still alive to act on it. If that
/// watchdog has since died (e.g. it exited right after sending its
/// confirmation but before this wait was processed), the old orphan is
/// the only thing left that could ever retry the physical unkey -- so it
/// must NOT be released.
///
/// `true` only when BOTH `confirmed_safe` (the buffered `ptt_safe` value)
/// AND `replacement_watchdog_alive` (a FRESH liveness check, taken
/// immediately before this decision is acted on, not the earlier
/// buffered signal) hold. Fails safe (retains the orphan) the same as
/// [`orphan_safe_to_abort`] does on every other uncertain outcome.
fn orphan_release_is_safe(confirmed_safe: bool, replacement_watchdog_alive: bool) -> bool {
    confirmed_safe && replacement_watchdog_alive
}

// `ComponentMessage` (432 bytes) travels by value through `Result::Err` here
// the same way crossbeam's own `TrySendError<ComponentMessage>` already does
// in the `try_send` calls below -- carrying the un-sent message back to the
// caller so it can be requeued, not a new pattern this function introduces.
//
// PAN-19 round-19 review (Codex P1) added the two `producer_marked_*_id`
// params (finding #2, "retire the handoff marker when discarding stale
// state"), pushing this past clippy's default 7-argument threshold. Each
// param plays a distinct, already-documented role (two pending slots, two
// supersession trackers, the channel, the in-flight count, two producer-
// mark trackers) and the two kinds (frequency/split) can't share a single
// bundled struct without losing the "which kind is this" clarity at every
// call site -- allowed rather than forcing an artificial grouping.
#[allow(clippy::result_large_err, clippy::too_many_arguments)]
fn deliver_pending_hamlib_state(
    pending_frequency: &std::sync::Arc<
        std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, ComponentMessage>>,
    >,
    pending_split: &std::sync::Arc<std::sync::Mutex<Option<ComponentMessage>>>,
    last_applied_frequency_id: &std::sync::Arc<
        std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, u64>>,
    >,
    last_applied_split_id: &std::sync::Arc<std::sync::Mutex<Option<u64>>>,
    sender: &crossbeam_channel::Sender<ComponentMessage>,
    hamlib_command_in_flight: &std::sync::Arc<std::sync::atomic::AtomicU32>,
    producer_marked_frequency_id: &std::sync::Arc<
        std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, u64>>,
    >,
    producer_marked_split_id: &std::sync::Arc<std::sync::Mutex<Option<u64>>>,
) {
    // PAN-35: iterate every VFO with a pending command, not just one shared
    // slot -- a pending VFO-A command and a pending VFO-B command are both
    // delivered here, each judged against (and recorded into) only its own
    // VFO's supersession/in-flight-mark tracking, so neither can supersede
    // or clobber the other's independent retry state.
    if let Ok(mut slots) = pending_frequency.lock() {
        let mut retry = HashMap::new();
        for (vfo, message) in slots.drain() {
            if is_frequency_superseded(&message, vfo, last_applied_frequency_id) {
                info!(
                    target: "rig",
                    "Hamlib restart: dropping a pending SetFrequency command superseded by a \
                     newer command that already went through -- not reverting good state"
                );
            } else if let Err(returned) = mark_frequency_in_flight_then_send(
                message,
                vfo,
                hamlib_command_in_flight,
                producer_marked_frequency_id,
                |m| sender.try_send(m).map_err(|e| e.into_inner()),
            ) {
                warn!(
                    "Hamlib restart: failed to deliver pending SetFrequency command -- \
                     keeping it queued for the next attempt"
                );
                retry.insert(vfo, returned);
            } else {
                info!(
                    target: "rig",
                    "Hamlib restart: delivered pending SetFrequency command carried over from \
                     a prior failed teardown replay"
                );
            }
        }
        *slots = retry;
    }
    if let Ok(mut slot) = pending_split.lock() {
        if let Some(message) = slot.take() {
            if is_superseded(&message, last_applied_split_id) {
                info!(
                    target: "rig",
                    "Hamlib restart: dropping a pending SetSplit command superseded by a \
                     newer command that already went through -- not reverting good state"
                );
            } else if let Err(returned) = mark_in_flight_then_send(
                message,
                hamlib_command_in_flight,
                producer_marked_split_id,
                |m| sender.try_send(m).map_err(|e| e.into_inner()),
            ) {
                warn!(
                    "Hamlib restart: failed to deliver pending SetSplit command -- keeping it \
                     queued for the next attempt"
                );
                *slot = Some(returned);
            } else {
                info!(
                    target: "rig",
                    "Hamlib restart: delivered pending SetSplit command carried over from a \
                     prior failed teardown replay"
                );
            }
        }
    }
}

/// PAN-19 round-18 review (Codex P1): "establish the handoff marker
/// before publishing the command". Round 17's fix marked
/// `hamlib_command_in_flight` AFTER `try_send` succeeded, not before --
/// on a fast consumer, the full cycle (dequeue -> CAT call completes ->
/// `HamlibCommandInFlightGuard::drop` clears the flag) could finish
/// BEFORE this producer's own delayed `store(true, ..)` executed,
/// letting that stale write resurrect the flag to `true` with nothing
/// left in the pending slot and no guard left to ever clear it --
/// permanently muting an otherwise healthy generation.
///
/// Fix: mark in-flight BEFORE attempting the send, not after. By the
/// time a consumer could possibly see the message (which can only happen
/// once `send` has actually run), the flag is already `true` -- there is
/// no window where the consumer's clear could ever land before this
/// producer's own set. Rolled back (cleared) ONLY when `send` itself
/// fails, returning the message back to the caller (as `Err`) so it can
/// be put back in its pending slot -- nothing was actually handed off,
/// so no consumer could ever be racing against this specific rollback.
/// This is symmetric with how `HamlibCommandInFlightGuard` clears on the
/// consumption side: exactly one write establishes `true` for this
/// message's handoff, never two independent writers racing to do it.
///
/// Takes the channel send as an injectable closure (rather than a
/// concrete `crossbeam_channel::Sender`) so the exact ordering claim --
/// "the flag is true BEFORE send is attempted, not just eventually
/// true" -- is directly, deterministically testable: a test closure can
/// run arbitrary code (including a simulated consumer's full lifecycle)
/// at the exact instant "the message left this function", which a real
/// channel's internal timing can't be forced to reproduce reliably.
///
/// PAN-19 round-19 review (Codex P1): "count every pending command
/// handoff" + "retire the handoff marker when discarding stale state".
/// `hamlib_command_in_flight` is now a count, incremented here (not just
/// set `true`) so two simultaneous handoffs (frequency + split) both
/// register. `producer_marked_id` records THIS message's id as "who owns
/// the increment this call just made" -- on send failure it's cleared
/// here (the rollback also retires the mark, symmetric with the count
/// rollback); on send SUCCESS it's left set so whichever consumer-side
/// code path eventually handles this exact message (applies it via
/// `HamlibCommandInFlightGuard::adopt`, or discards it as superseded
/// without ever reaching a guard) can find and retire this SAME
/// increment via `take_producer_mark_if_matching` -- see that function's
/// doc comment.
#[allow(clippy::result_large_err)] // ComponentMessage returned so it can be requeued; see deliver_pending_hamlib_state's own allow.
fn mark_in_flight_then_send(
    message: ComponentMessage,
    hamlib_command_in_flight: &std::sync::Arc<std::sync::atomic::AtomicU32>,
    producer_marked_id: &std::sync::Arc<std::sync::Mutex<Option<u64>>>,
    mut send: impl FnMut(ComponentMessage) -> Result<(), ComponentMessage>,
) -> Result<(), ComponentMessage> {
    hamlib_command_in_flight.fetch_add(1, Ordering::AcqRel);
    if let Ok(mut marked) = producer_marked_id.lock() {
        *marked = Some(message.id);
    }
    match send(message) {
        Ok(()) => Ok(()),
        Err(returned) => {
            hamlib_command_in_flight.fetch_sub(1, Ordering::AcqRel);
            if let Ok(mut marked) = producer_marked_id.lock() {
                *marked = None;
            }
            Err(returned)
        }
    }
}

/// PAN-19 round-19 review (Codex P1): "retire the handoff marker when
/// discarding stale state". The message loop's own consumption-time
/// `is_superseded` check (round 12) can decide to DISCARD a message
/// (`continue`, without ever constructing a `HamlibCommandInFlightGuard`)
/// -- but if that message was the one `mark_in_flight_then_send`
/// (producer side) already incremented `hamlib_command_in_flight` for,
/// nothing else will EVER decrement it: the guard that would normally
/// retire it is never constructed on a discard path. That stranded
/// increment permanently mutes an otherwise healthy generation, the same
/// failure class as round 17/18's fixes, now specifically in the
/// supersession-discard branch.
///
/// If `message`'s id matches the currently-tracked producer-marked
/// handoff for this kind, clears the tracking and returns `true` -- the
/// caller now OWNS retiring the one increment the producer made (either
/// directly, on a discard path, or by adopting it into a
/// `HamlibCommandInFlightGuard::adopt` on an apply path, so it isn't
/// double-counted against a fresh `HamlibCommandInFlightGuard::new`).
/// Returns `false` (tracker untouched) for a message that was never
/// producer-marked (an ordinary fresh send, which the consumer's own
/// guard must count independently) -- and, on a poisoned tracker lock,
/// fails toward `false` (NOT retiring) rather than risk retiring a count
/// contribution that isn't actually this message's to retire: an extra
/// stranded `+1` (still-muted) is the safe-but-annoying failure, while a
/// wrongly-claimed retirement could under-count a GENUINELY outstanding
/// handoff and let PTT through too early.
fn take_producer_mark_if_matching(
    message: &ComponentMessage,
    producer_marked_id: &std::sync::Arc<std::sync::Mutex<Option<u64>>>,
) -> bool {
    let Ok(mut marked) = producer_marked_id.lock() else {
        return false;
    };
    if *marked == Some(message.id) {
        *marked = None;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod take_producer_mark_if_matching_tests {
    use super::*;

    fn msg(id_after: u64) -> ComponentMessage {
        // Construct `id_after` throwaway messages first so this one's own
        // globally-monotonic id is strictly greater than all of them --
        // gives tests a cheap way to get two messages with a known
        // relative ordering without depending on exact absolute ids.
        for _ in 0..id_after {
            let _ = ComponentMessage::new(
                ComponentId::Hamlib,
                ComponentId::Hamlib,
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                    enabled: true,
                    tx_frequency: 14_078_000,
                }),
                Instant::now(),
            );
        }
        ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000,
            }),
            Instant::now(),
        )
    }

    #[test]
    fn returns_true_and_clears_the_tracker_when_the_id_matches() {
        let message = msg(0);
        let producer_marked_id = Arc::new(std::sync::Mutex::new(Some(message.id)));

        assert!(take_producer_mark_if_matching(
            &message,
            &producer_marked_id
        ));
        assert!(
            producer_marked_id.lock().unwrap().is_none(),
            "a matching take must clear the tracker so nobody else can also claim it"
        );
    }

    #[test]
    fn returns_false_and_leaves_the_tracker_untouched_when_the_id_does_not_match() {
        let message = msg(0);
        let other_id = msg(1).id;
        let producer_marked_id = Arc::new(std::sync::Mutex::new(Some(other_id)));

        assert!(!take_producer_mark_if_matching(
            &message,
            &producer_marked_id
        ));
        assert_eq!(
            *producer_marked_id.lock().unwrap(),
            Some(other_id),
            "a non-matching message must not disturb whatever IS tracked -- it belongs to a \
             different handoff"
        );
    }

    #[test]
    fn returns_false_when_nothing_is_tracked() {
        let message = msg(0);
        let producer_marked_id: Arc<std::sync::Mutex<Option<u64>>> =
            Arc::new(std::sync::Mutex::new(None));

        assert!(!take_producer_mark_if_matching(
            &message,
            &producer_marked_id
        ));
    }

    /// PAN-19 round-19 review (Codex P1) regression guard: "retire the
    /// handoff marker when discarding stale state". The FULL scenario the
    /// finding describes: the producer marks a handoff (increments the
    /// count, records its id), but by the time the message loop actually
    /// consumes it, a NEWER command has already been applied -- the
    /// message loop's own `is_superseded` check discards it via
    /// `continue`, WITHOUT ever constructing a `HamlibCommandInFlightGuard`.
    /// Without retiring the producer's mark on that discard path, the
    /// count would be stranded at 1 forever. This drives the exact
    /// sequence `mark_in_flight_then_send` -> discard-as-superseded ->
    /// retire, proving the count correctly returns to 0.
    #[test]
    fn a_discarded_producer_marked_message_retires_its_own_increment() {
        let stale_split = msg(0);
        let hamlib_command_in_flight = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let producer_marked_split_id: Arc<std::sync::Mutex<Option<u64>>> =
            Arc::new(std::sync::Mutex::new(None));

        // Producer side: marks and "sends" successfully.
        let send_result = mark_in_flight_then_send(
            stale_split.clone(),
            &hamlib_command_in_flight,
            &producer_marked_split_id,
            |m| {
                drop(m);
                Ok(())
            },
        );
        assert!(
            send_result.is_ok(),
            "test setup: the simulated send must succeed"
        );
        assert_eq!(
            hamlib_command_in_flight.load(Ordering::Acquire),
            1,
            "test setup: the producer's mark must have incremented the count"
        );

        // Consumer side: the message loop's own `is_superseded` check
        // (simulated here as already true -- a newer command was applied
        // in the meantime) discards it via the SAME retire-on-discard
        // logic the real SetSplit arm now runs.
        if take_producer_mark_if_matching(&stale_split, &producer_marked_split_id) {
            hamlib_command_in_flight.fetch_sub(1, Ordering::AcqRel);
        }

        assert_eq!(
            hamlib_command_in_flight.load(Ordering::Acquire),
            0,
            "discarding a producer-marked message as superseded must retire its own increment \
             -- the count must return to 0, not strand permanently at 1"
        );
        assert!(
            producer_marked_split_id.lock().unwrap().is_none(),
            "the tracker itself must also be cleared once retired"
        );
    }
}

/// Platform-appropriate hamlib install hint (mirrors `pancetta doctor`).
fn hamlib_install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "brew install hamlib"
    } else if cfg!(target_os = "windows") {
        "install hamlib from https://hamlib.github.io and put rigctld.exe on PATH"
    } else {
        "sudo apt install libhamlib-utils"
    }
}

/// Surface a rig-setup failure to the operator: an ephemeral TUI error line
/// (`MessageType::Error` → error log/status) PLUS a retained
/// `DiagnosticEvent` (target `rig.cat`, Shift+D overlay) so the reason for a
/// RIG ✗ badge survives after the status line scrolls away. Previously these
/// failures were `warn!`-to-file only — invisible under the TUI's alternate
/// screen. Sends are best-effort (`let _ =`): headless runs have no TUI channel.
async fn report_rig_error(message_bus: &MessageBus, text: String) {
    let err = ComponentMessage::new(
        ComponentId::Hamlib,
        ComponentId::Tui,
        MessageType::Error {
            component_id: ComponentId::Hamlib,
            error_message: text.clone(),
            error_code: None,
        },
        Instant::now(),
    );
    let _ = message_bus.send_message(err).await;
    let diag = ComponentMessage::new(
        ComponentId::Hamlib,
        ComponentId::Tui,
        MessageType::DiagnosticEvent {
            target: "rig.cat",
            level: pancetta_core::DiagnosticLevel::Warn,
            text,
            qso_id: None,
            callsign: None,
        },
        Instant::now(),
    );
    let _ = message_bus.send_message(diag).await;
}

#[cfg(test)]
mod child_task_crashed_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// PAN-19 MEDIUM #1: a child that has already exited (expectedly, once
    /// `shutdown` is observed) must NOT be classified as a crash once
    /// `shutdown` is set -- even though the OUTER loop's own
    /// `!shutdown.load(...)` guard may have already been checked (and
    /// passed) before the child actually exited, this function is the
    /// second, authoritative check and must re-confirm `shutdown` itself.
    #[tokio::test]
    async fn ignores_a_child_that_exited_once_shutdown_is_set() {
        let shutdown = AtomicBool::new(true);
        let handle = tokio::spawn(async {});
        // Let the trivial task actually finish.
        for _ in 0..1000 {
            if handle.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            handle.is_finished(),
            "test setup: child should have finished"
        );

        assert!(
            !child_task_crashed(
                || std::slice::from_ref(&handle)
                    .iter()
                    .any(|h| h.is_finished()),
                || shutdown.load(Ordering::Acquire),
            ),
            "a child observed exiting during shutdown must not be treated as a crash"
        );
    }

    /// The flip side: outside of shutdown, a finished child IS a real crash
    /// and must still be flagged.
    #[tokio::test]
    async fn flags_a_real_crash_when_not_shutting_down() {
        let shutdown = AtomicBool::new(false);
        let handle = tokio::spawn(async {});
        for _ in 0..1000 {
            if handle.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            handle.is_finished(),
            "test setup: child should have finished"
        );

        assert!(
            child_task_crashed(
                || std::slice::from_ref(&handle)
                    .iter()
                    .any(|h| h.is_finished()),
                || shutdown.load(Ordering::Acquire),
            ),
            "a child that exits outside of shutdown must still be flagged as a crash"
        );
    }

    #[tokio::test]
    async fn no_crash_when_all_children_still_running() {
        let shutdown = AtomicBool::new(false);
        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        assert!(!handle.is_finished());

        assert!(!child_task_crashed(
            || std::slice::from_ref(&handle)
                .iter()
                .any(|h| h.is_finished()),
            || shutdown.load(Ordering::Acquire),
        ));
        handle.abort();
    }

    /// PAN-19 round-1 review (Codex P2): the two tests above don't actually
    /// distinguish CHECK ORDER -- `shutdown` is already fully settled to its
    /// final value before `child_task_crashed` is ever called, so both the
    /// old (`!shutdown.load() && is_finished()`) and new
    /// (`is_finished() && !shutdown.load()`) orderings return the same
    /// answer. The real bug only shows up in the tiny window where
    /// `shutdown` flips (and a child exits because of it) concurrently WITH
    /// the check itself: reading `shutdown` first can observe the stale
    /// `false` moments before the flip, then `is_finished()` (checked
    /// second, further "downstream" of the flip) observes the now-exited
    /// child -- misclassifying an expected shutdown exit as a crash. Reading
    /// `is_finished()` first closes this: a child can only be observed
    /// finished once its exit (and everything causally before it, including
    /// whatever flipped `shutdown`) is visible, so the SECOND read
    /// (`shutdown`, checked only once a finished child is already known)
    /// reliably sees the fresh value.
    ///
    /// PAN-28 (Codex round-2 on PR #254): this used to race a real
    /// background task against 20,000 timing trials, which was
    /// scheduler-dependent -- on a runtime where the two reads always
    /// landed both-pre-flip or both-post-flip, `old_order_misfired` could
    /// stay false forever even though `child_task_crashed` was correct,
    /// and the test's own failure message admitted as much. There's no
    /// timer to pause and no yield point inside `child_task_crashed`'s
    /// synchronous body to synchronize a real thread race on, so instead
    /// this scripts the two reads directly: `FlipTrace::read()` returns
    /// `false` on its first call and `true` on every call after --
    /// modelling "the shutdown flip (and the child exit it causes) lands
    /// in the single gap between whichever two reads the checked ordering
    /// performs" -- with zero dependence on real scheduling.
    ///
    /// PAN-28 round 1 (Codex): drives `child_task_crashed` ITSELF, not a
    /// separately-tested ordering helper it merely delegates to -- see
    /// that function's doc comment for why the two used to be split (and
    /// why splitting them left the wiring at each real call site
    /// untested). Reverting the production order flips this test's first
    /// assertion from pass to fail, no matter where in `child_task_crashed`
    /// that reversion happens.
    #[test]
    fn fixed_order_survives_a_shutdown_flip_the_old_order_misses() {
        struct FlipTrace(std::cell::Cell<u32>);
        impl FlipTrace {
            fn new() -> Self {
                Self(std::cell::Cell::new(0))
            }
            fn read(&self) -> bool {
                self.0.set(self.0.get() + 1);
                self.0.get() >= 2
            }
        }

        let trace = FlipTrace::new();
        assert!(
            !child_task_crashed(|| trace.read(), || trace.read()),
            "fixed order (is_finished first) misclassified a shutdown flip landing \
             between the two reads as a crash"
        );

        let trace = FlipTrace::new();
        let old_order_shutdown_first = !trace.read() && trace.read();
        assert!(
            old_order_shutdown_first,
            "expected the OLD check order (shutdown read before is_finished()) to \
             misclassify a shutdown flip landing between the two reads as a crash -- \
             if this doesn't fire, the trace no longer models the race"
        );
    }
}

#[cfg(all(test, feature = "pancetta-hamlib"))]
mod children_publish_race_tests {
    //! PAN-19 HIGH regression guard.
    //!
    //! `start_hamlib_component` doesn't accept an injectable rig, so these
    //! tests can't drive a genuinely slow connect through the real function
    //! end to end. Instead this mirrors the exact concurrency SHAPE the fix
    //! establishes -- spawn the child tasks and publish their abort handles
    //! via a oneshot FIRST, then await the (possibly slow) rig connect --
    //! using a real `pancetta_hamlib::MockRig` configured with a multi-second
    //! connect delay, so the "does the children publish depend on connect
    //! completing" property is exercised with genuine async timing, not
    //! just asserted by inspection.
    use super::*;
    use pancetta_hamlib::{MockRig, MockRigConfig, RigControl};

    #[tokio::test]
    async fn children_handles_publish_before_a_slow_rig_connect_completes() {
        let rig = Arc::new(MockRig::new(MockRigConfig {
            // Comfortably past the old hardcoded 1s `children_rx` timeout
            // that used to fire (and bail) under this exact condition, but
            // well under this test's own bound.
            connection_delay_ms: 1_500,
            ..Default::default()
        }));
        let (children_tx, children_rx) = tokio::sync::oneshot::channel::<()>();

        let rig_for_task = rig.clone();
        tokio::spawn(async move {
            // Fixed shape: children published BEFORE the connect sequence,
            // not after.
            let poll_stub = tokio::spawn(async { std::future::pending::<()>().await });
            let watchdog_stub = tokio::spawn(async { std::future::pending::<()>().await });
            let _ = children_tx.send(());
            poll_stub.abort();
            watchdog_stub.abort();

            // The slow part -- must NOT gate the send above.
            let _ = rig_for_task.connect().await;
        });

        tokio::time::timeout(Duration::from_millis(300), children_rx)
            .await
            .expect(
                "children handles must publish promptly, independent of how long \
                     rig.connect() takes",
            )
            .expect("children_tx sender must not have been dropped without sending");
    }

    /// PAN-19 HIGH follow-up (round 3, Codex P1) regression guard.
    ///
    /// The HIGH fix above closed the original bug (children publishing
    /// gated on a slow connect), but that also decoupled "children
    /// published" from "the message loop actually exists to consume a
    /// `SetPtt` command" -- `TxInhibitGuard` (health.rs) releases the
    /// moment `start_hamlib_component` returns, which used to happen
    /// shortly after children published. This pins the GAP between the two
    /// events that `HAMLIB_LOOP_READY_TIMEOUT`'s wait (in the real
    /// `start_hamlib_component`, just above this test module) exists to
    /// respect: children publish immediately, but the separate
    /// "loop ready" signal must NOT fire until AFTER the slow connect
    /// sequence completes -- proving a caller that gates TX un-inhibit on
    /// the LATTER (not just the former) stays correctly blocked for the
    /// whole gap, not just for the ~instant children-publish window.
    #[tokio::test]
    async fn loop_ready_signal_does_not_fire_until_after_a_slow_rig_connect_completes() {
        let rig = Arc::new(MockRig::new(MockRigConfig {
            connection_delay_ms: 300,
            ..Default::default()
        }));
        let (children_tx, children_rx) = tokio::sync::oneshot::channel::<()>();
        let (loop_ready_tx, mut loop_ready_rx) = tokio::sync::oneshot::channel::<()>();

        let rig_for_task = rig.clone();
        tokio::spawn(async move {
            let poll_stub = tokio::spawn(async { std::future::pending::<()>().await });
            let watchdog_stub = tokio::spawn(async { std::future::pending::<()>().await });
            let _ = children_tx.send(());
            poll_stub.abort();
            watchdog_stub.abort();

            // The slow part -- mirrors the real connect/PTT-off/frequency
            // -read sequence in `start_hamlib_component`'s spawned task.
            let _ = rig_for_task.connect().await;

            // Fires only after that slow sequence -- mirrors
            // `initial_read_tx`/`loop_ready_tx` firing together right
            // before the real message loop starts.
            let _ = loop_ready_tx.send(());
        });

        // Children publish promptly -- unaffected by this follow-up fix
        // (the HIGH fix's own guarantee still holds).
        tokio::time::timeout(Duration::from_millis(100), children_rx)
            .await
            .expect("children handles must still publish promptly")
            .expect("children_tx sender must not have been dropped without sending");

        // At this point the slow connect (300ms) is still in flight. A
        // caller gating TX un-inhibit on `loop_ready_rx` (rather than on
        // `children_rx`, which already fired) must still see it NOT
        // ready -- this is the exact gap the round-3 fix exists to
        // respect rather than race past.
        assert!(
            matches!(
                loop_ready_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "loop_ready_rx must not have fired yet -- children publishing must not imply \
             the message loop is ready to consume commands"
        );

        // ...and it DOES eventually fire, once the slow connect finishes --
        // proving the gap closes rather than the signal never arriving.
        tokio::time::timeout(Duration::from_secs(1), loop_ready_rx)
            .await
            .expect("loop_ready_rx must eventually fire once the slow connect finishes")
            .expect("loop_ready_tx sender must not have been dropped without sending");
    }

    /// PAN-19 round-8 review (Codex P1), finding #1: "Set readiness when
    /// the late Hamlib loop starts". An earlier version relied SOLELY on
    /// `start_hamlib_component`'s bounded receive on `loop_ready_rx` to set
    /// `hamlib_command_loop_ready`. If that wait had already elapsed
    /// (dropping the receiver) by the time the spawned task reaches the
    /// readiness point -- e.g. the combined ~23s of upstream waits weren't
    /// enough -- the later `loop_ready_tx.send(())` would have no receiver,
    /// and the atomic would NEVER flip `true`: TX stays muted forever
    /// (until the next restart) even once the rig genuinely recovers, far
    /// worse than the bounded over-caution window being fixed.
    ///
    /// Mirrors that exact scenario: drop `loop_ready_rx` FIRST (simulating
    /// the caller's bounded wait already having timed out), THEN let a
    /// slow-connecting task reach its own readiness point and set a real
    /// `Arc<AtomicBool>` directly -- proving the flag still ends up `true`,
    /// independent of whether anyone was still listening on the oneshot.
    #[tokio::test]
    async fn atomic_flag_is_set_by_the_spawned_task_even_after_the_caller_stopped_listening() {
        let rig = Arc::new(MockRig::new(MockRigConfig {
            connection_delay_ms: 200,
            ..Default::default()
        }));
        let hamlib_command_loop_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (loop_ready_tx, loop_ready_rx) = tokio::sync::oneshot::channel::<()>();

        // Simulate `start_hamlib_component`'s bounded wait having ALREADY
        // timed out (and thus dropped its receiver) before the spawned
        // task ever reaches the readiness point.
        drop(loop_ready_rx);

        let rig_for_task = rig.clone();
        let flag_for_task = hamlib_command_loop_ready.clone();
        let handle = tokio::spawn(async move {
            let _ = rig_for_task.connect().await; // the slow part
                                                  // Mirrors the real (fixed) spawned task: set the atomic
                                                  // DIRECTLY, independent of whether `loop_ready_tx` still has a
                                                  // receiver.
            flag_for_task.store(true, Ordering::Release);
            let _ = loop_ready_tx.send(()); // no receiver left -- harmless no-op
        });
        handle.await.expect("task should complete");

        assert!(
            hamlib_command_loop_ready.load(Ordering::Acquire),
            "the atomic flag must be set by the spawned task directly, not stuck false just \
             because the caller's receiver had already gone out of scope"
        );
    }

    /// PAN-19 round-8 review (Codex P1), finding #2: "Verify child
    /// liveness before publishing loop readiness". Reporting readiness
    /// unconditionally ignored that the poll/watchdog children could
    /// already be dead by the time the spawned task reaches the readiness
    /// point (e.g. one panicked during the connect/PTT-off/frequency-read
    /// sequence) -- moments before the message loop's own
    /// `child_task_crashed` check (its very first line) would catch
    /// exactly that and bail the whole generation. That left a window
    /// where TX was reported ready for a generation already about to die
    /// with no command consumer.
    ///
    /// Mirrors the fixed call site directly: a child handle that has
    /// already finished, checked via the real `child_task_crashed` helper
    /// BEFORE reporting readiness -- proving readiness (both the oneshot
    /// send and the atomic flag) is withheld, matching the real
    /// implementation's `if child_task_crashed(...) { /* withhold */ }
    /// else { /* report */ }` shape.
    #[tokio::test]
    async fn readiness_is_withheld_when_a_child_has_already_terminated() {
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hamlib_command_loop_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (loop_ready_tx, loop_ready_rx) = tokio::sync::oneshot::channel::<()>();

        // A poll/watchdog child that already terminated by the time the
        // spawned task reaches the readiness-reporting point.
        let already_dead: tokio::task::JoinHandle<()> = tokio::spawn(async {});
        for _ in 0..1000 {
            if already_dead.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            already_dead.is_finished(),
            "test setup: the child should have finished"
        );
        let still_running = tokio::spawn(async { std::future::pending::<()>().await });
        let spawned_handles = vec![already_dead, still_running];

        // Mirrors the real (fixed) call site exactly: check liveness
        // BEFORE reporting readiness.
        if child_task_crashed(
            || spawned_handles.iter().any(|handle| handle.is_finished()),
            || shutdown.load(Ordering::Acquire),
        ) {
            // Withhold: neither the atomic flag nor the oneshot send.
            // Explicitly drop `loop_ready_tx` without sending, mirroring
            // the real task going on to bail and end.
            drop(loop_ready_tx);
        } else {
            hamlib_command_loop_ready.store(true, Ordering::Release);
            let _ = loop_ready_tx.send(());
        }

        assert!(
            !hamlib_command_loop_ready.load(Ordering::Acquire),
            "the atomic flag must NOT be set when a child has already terminated -- readiness \
             must be withheld, not reported for a generation about to die"
        );
        assert!(
            loop_ready_rx.await.is_err(),
            "loop_ready_rx must resolve as a dropped sender (no readiness reported), not a \
             received value"
        );

        for handle in spawned_handles {
            handle.abort();
        }
    }

    /// PAN-19 round-16 review (Codex P1): "clear readiness as soon as
    /// either Hamlib child exits". `restart_orphan_tests` (below) proves
    /// the real wiring end to end -- aborting a published child's
    /// `AbortHandle` does clear `hamlib_command_loop_ready`. But in that
    /// test the message loop is otherwise idle (its own `while` loop is
    /// cycling a ~10ms `try_recv`-empty sleep), so round 11's PRE-EXISTING
    /// `HamlibLoopReadyGuard` on the message loop itself also notices the
    /// dead child within that same window -- the two mechanisms can't be
    /// timing-distinguished there, and `start_hamlib_component` has no
    /// injection point for a genuinely slow rig I/O call to hold the
    /// message loop busy for a controlled window (same limitation this
    /// module's own doc comment describes for a different mechanism).
    ///
    /// This test instead isolates the SPECIFIC property the finding
    /// describes -- a child's own guard fires independent of whatever the
    /// "message loop" is doing -- using genuine async concurrency: a
    /// simulated "message loop" spawned task sleeps for a controlled
    /// window (standing in for a slow `set_frequency`/`set_split` CAT
    /// await) and does NOT check anything during that window (mirroring
    /// that the real loop's `child_task_crashed` check ONLY runs between
    /// iterations, never during an in-flight I/O await); a separate
    /// simulated "child" task holds the exact real `HamlibLoopReadyGuard`
    /// type this round wires into the poll/watchdog tasks' spawned bodies,
    /// and ends immediately (simulating a crash). Readiness must already
    /// be false well before the simulated slow await completes -- proving
    /// the mechanism does not depend on the busy loop ever getting a
    /// chance to check.
    #[tokio::test]
    async fn a_childs_own_guard_clears_readiness_while_a_simulated_slow_cat_call_is_still_in_flight(
    ) {
        let hamlib_command_loop_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        hamlib_command_loop_ready.store(true, Ordering::Release);
        let current_generation = Arc::new(std::sync::atomic::AtomicU64::new(1));

        // The "message loop": simulates being blocked inside a slow CAT
        // call for 300ms, never checking child liveness during that
        // window (mirrors the real loop, which only calls
        // `child_task_crashed` between `while` iterations -- never while
        // an `.await` on `rig_poll.set_frequency`/`set_split_freq`/
        // `set_split` is in flight). Reports what it observes the instant
        // its simulated I/O "returns".
        let message_loop_flag = hamlib_command_loop_ready.clone();
        let message_loop_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            message_loop_flag.load(Ordering::Acquire)
        });

        // Give the "message loop" task a moment to actually start its
        // sleep before the "child" ends, so this isn't a same-tick race.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // The "child" (poll or watchdog): holds the SAME real
        // `HamlibLoopReadyGuard` type wired into their spawned bodies,
        // still in generation 1 (same as `current_generation`, so its
        // drop must still clear -- this test isn't exercising round 17's
        // cross-generation no-op, just the round-16 mechanism it wraps),
        // and ends immediately -- simulating a crash mid-CAT-await.
        let child_flag = hamlib_command_loop_ready.clone();
        let child_generation = current_generation.clone();
        tokio::spawn(async move {
            let _child_guard = HamlibLoopReadyGuard::new(child_flag, 1, child_generation);
        })
        .await
        .expect("simulated child task must not panic");

        // Readiness must ALREADY be false now -- ~280ms before the
        // simulated slow CAT call even returns, let alone before any
        // "message loop" code could have checked anything.
        assert!(
            !hamlib_command_loop_ready.load(Ordering::Acquire),
            "the child's own guard must clear readiness immediately on its own exit, without \
             waiting for the busy message loop to get back around to a liveness check"
        );

        let seen_once_the_slow_call_finally_returns = message_loop_task.await.unwrap();
        assert!(
            !seen_once_the_slow_call_finally_returns,
            "readiness must still read false once the simulated slow CAT call finishes -- it \
             was never the message loop's own check that cleared it"
        );
    }
}

#[cfg(all(test, feature = "pancetta-hamlib"))]
mod restart_orphan_tests {
    //! PAN-19 MEDIUM #2: a watchdog orphaned by a teardown whose 3 PTT-off
    //! attempts all failed (see `teardown_hamlib`'s `hamlib_orphans.push`)
    //! must not survive past the NEXT successful restart -- it would carry
    //! a stale `ptt_on_since` from the OLD generation and could still fire
    //! `set_ptt(Off)` on the NEW generation's rig client mid-transmission.
    use super::*;
    use pancetta_config::Config;
    use std::sync::atomic::AtomicBool;

    /// Local copy of the same direct-construction pattern used by
    /// `mod.rs`'s `test_coordinator_creation` / `health.rs`'s
    /// `test_coordinator` -- no shared helper exists yet for this module.
    async fn test_coordinator() -> super::super::ApplicationCoordinator {
        let config = Config::default();
        let shutdown = Arc::new(AtomicBool::new(false));
        super::super::ApplicationCoordinator::new(
            config,
            None,
            true,  // no_audio
            true,  // headless
            false, // metrics
            9090,
            None, // no WAV
            None, // no replay
            None, // no test-tx
            1500.0,
            shutdown,
            Vec::new(), // no config warnings
        )
        .await
        .expect("coordinator creation should succeed")
    }

    #[tokio::test]
    async fn successful_start_aborts_a_watchdog_orphaned_by_a_prior_failed_teardown() {
        let mut coordinator = test_coordinator().await;

        // Simulate a PRIOR teardown whose 3 PTT-off attempts all failed:
        // `teardown_hamlib` pushed the (still running) watchdog's
        // `AbortHandle` onto `hamlib_orphans` instead of aborting it.
        let orphan_task = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        coordinator.hamlib_orphans.push(orphan_task.abort_handle());
        assert!(!orphan_task.is_finished());

        // Config::default() has rig control disabled, so this takes the
        // mock-rig path -- no real hardware/rigctld needed, and it
        // completes quickly and deterministically.
        coordinator
            .start_hamlib_component()
            .await
            .expect("mock-rig hamlib start should succeed");

        assert!(
            coordinator.hamlib_orphans.is_empty(),
            "orphans must be drained once the new Hamlib generation successfully starts"
        );

        for _ in 0..1000 {
            if orphan_task.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            orphan_task.is_finished(),
            "the watchdog orphaned by a prior failed teardown must be aborted once the new \
             generation starts, not left running with a stale ptt_on_since from the old one"
        );

        // PAN-19 round-8 review: `hamlib_command_loop_ready` is now set by
        // the SPAWNED task itself, asynchronously, rather than
        // synchronously within `start_hamlib_component`'s own return path
        // (which on the mock-rig path returns almost immediately). Poll
        // for it briefly rather than asserting immediately -- proving the
        // real, end-to-end integration point (not just the mirrored-shape
        // tests in `children_publish_race_tests`) actually flips this flag
        // true for a genuinely successful start.
        // `yield_now()` alone doesn't reliably advance real time on a
        // `current_thread` runtime while the spawned task is genuinely
        // asleep inside `MockRig::connect()`'s ~100ms delay -- use a real
        // (short) sleep between polls so this doesn't spuriously fail
        // before the spawned task has had a real chance to run.
        for _ in 0..50 {
            if coordinator
                .hamlib_command_loop_ready
                .load(std::sync::atomic::Ordering::Acquire)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            coordinator
                .hamlib_command_loop_ready
                .load(std::sync::atomic::Ordering::Acquire),
            "hamlib_command_loop_ready must end up true once the real spawned task reaches \
             its message loop on a genuinely successful start"
        );

        coordinator
            .shutdown_signal
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// PAN-19 round-16 review (Codex P1) regression guard: "clear
    /// readiness as soon as either Hamlib child exits". Drives the REAL
    /// `start_hamlib_component` end to end (mock-rig path, same as the
    /// test above) to a genuinely-ready generation, then aborts the
    /// PUBLISHED poll child's `AbortHandle` directly -- exactly what a
    /// crash of that task looks like from the outside (tokio cancels it
    /// at its next await point, dropping its locals, including the
    /// `_poll_ready_guard` this round wired into its spawned body). Before
    /// this fix, only the message loop's OWN `HamlibLoopReadyGuard`
    /// existed, so nothing would clear readiness until the message loop's
    /// own between-message `child_task_crashed` check happened to run --
    /// this test proves the poll child clears it ITSELF, without the
    /// message loop doing anything (it's never even ticked here).
    #[tokio::test]
    async fn aborting_the_poll_child_clears_readiness_without_the_message_loop_doing_anything() {
        let mut coordinator = test_coordinator().await;
        coordinator
            .start_hamlib_component()
            .await
            .expect("mock-rig hamlib start should succeed");

        for _ in 0..50 {
            if coordinator
                .hamlib_command_loop_ready
                .load(std::sync::atomic::Ordering::Acquire)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            coordinator
                .hamlib_command_loop_ready
                .load(std::sync::atomic::Ordering::Acquire),
            "test setup invariant: readiness must be true before simulating the child crash"
        );

        // Simulate the poll child crashing/exiting -- abort it directly,
        // exactly as tokio would cancel it on a real panic or task end.
        coordinator
            .hamlib_children
            .as_ref()
            .expect("children must be published by a successful start")
            .poll
            .abort();

        // Readiness must clear promptly -- well within a couple of ticks,
        // NOT waiting for anything resembling the message loop's own
        // ~10ms `try_recv`-empty sleep interval to accumulate over many
        // iterations of an unrelated check.
        let mut cleared = false;
        for _ in 0..50 {
            if !coordinator
                .hamlib_command_loop_ready
                .load(std::sync::atomic::Ordering::Acquire)
            {
                cleared = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            cleared,
            "aborting the poll child must clear hamlib_command_loop_ready itself, directly -- \
             not depend on the message loop's own crash check ever running"
        );

        coordinator
            .shutdown_signal
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Same as above, for the watchdog child -- both children must clear
    /// readiness on their own exit, not just the poll task.
    #[tokio::test]
    async fn aborting_the_watchdog_child_clears_readiness_without_the_message_loop_doing_anything()
    {
        let mut coordinator = test_coordinator().await;
        coordinator
            .start_hamlib_component()
            .await
            .expect("mock-rig hamlib start should succeed");

        for _ in 0..50 {
            if coordinator
                .hamlib_command_loop_ready
                .load(std::sync::atomic::Ordering::Acquire)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            coordinator
                .hamlib_command_loop_ready
                .load(std::sync::atomic::Ordering::Acquire),
            "test setup invariant: readiness must be true before simulating the child crash"
        );

        coordinator
            .hamlib_children
            .as_ref()
            .expect("children must be published by a successful start")
            .watchdog
            .abort();

        let mut cleared = false;
        for _ in 0..50 {
            if !coordinator
                .hamlib_command_loop_ready
                .load(std::sync::atomic::Ordering::Acquire)
            {
                cleared = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            cleared,
            "aborting the watchdog child must clear hamlib_command_loop_ready itself, directly \
             -- not depend on the message loop's own crash check ever running"
        );

        coordinator
            .shutdown_signal
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(all(test, feature = "pancetta-hamlib"))]
mod orphan_ptt_confirmation_tests {
    //! PAN-19 round-12 review (Codex P1) regression guard: "retain the old
    //! watchdog until PTT-off is confirmed". `restart_orphan_tests` above
    //! (pre-existing, still green) already proves the POSITIVE case
    //! end-to-end through the real `start_hamlib_component` -- a
    //! successful mock-rig start (PTT-off succeeds by default) drains the
    //! orphan. What round 1's original fix got wrong was aborting the
    //! orphan on nothing stronger than "children published", BEFORE the
    //! new generation's own PTT-off had even been attempted -- which
    //! `start_hamlib_component`'s mock-rig path can't be made to
    //! reproduce directly (it always constructs a default, non-failing
    //! `MockRig`, with no injection point for a failing one -- see
    //! `children_publish_race_tests`' doc comment for the same limitation
    //! on a different mechanism).
    //!
    //! `orphan_safe_to_abort` is unit-tested directly below for the exact
    //! branch logic (real oneshot/timeout primitives, all four outcomes).
    //! The two integration-style tests after it mirror the orphan-drain
    //! call site's exact concurrency SHAPE -- real `AbortHandle`s, a real
    //! oneshot, a real bounded `timeout` -- to prove the retain/drain
    //! wiring itself, not just the branch predicate in isolation.
    use super::*;

    #[tokio::test]
    async fn orphan_safe_to_abort_is_true_only_for_an_explicit_positive_confirmation() {
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        let _ = tx.send(true);
        assert!(
            orphan_safe_to_abort(tokio::time::timeout(Duration::from_millis(50), rx).await),
            "an explicit `true` confirmation must be safe to abort on"
        );
    }

    #[tokio::test]
    async fn orphan_safe_to_abort_is_false_for_an_explicit_negative_confirmation() {
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        let _ = tx.send(false);
        assert!(
            !orphan_safe_to_abort(tokio::time::timeout(Duration::from_millis(50), rx).await),
            "an explicit `false` confirmation (PTT-off failed AND no active tracker) must \
             never be treated as safe to abort"
        );
    }

    #[tokio::test]
    async fn orphan_safe_to_abort_is_false_when_the_sender_drops_without_sending() {
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        drop(tx); // the spawned task died before reaching `ptt_safe_tx.send`
        assert!(
            !orphan_safe_to_abort(tokio::time::timeout(Duration::from_millis(50), rx).await),
            "a dropped confirmation sender must fail safe, not be treated as a green light"
        );
    }

    #[tokio::test]
    async fn orphan_safe_to_abort_is_false_when_the_wait_times_out() {
        let (_tx, rx) = tokio::sync::oneshot::channel::<bool>();
        // `_tx` is held (not sent, not dropped) so `rx` never resolves --
        // the bounded wait itself must elapse.
        assert!(
            !orphan_safe_to_abort(tokio::time::timeout(Duration::from_millis(20), rx).await),
            "a confirmation that never arrives in time must fail safe, not be treated as a \
             green light"
        );
    }

    #[test]
    fn orphan_release_is_safe_requires_both_conditions() {
        assert!(
            orphan_release_is_safe(true, true),
            "release must be safe when both the confirmation and a fresh liveness check agree"
        );
        assert!(
            !orphan_release_is_safe(false, true),
            "a live replacement watchdog alone is not enough without a positive confirmation"
        );
        assert!(
            !orphan_release_is_safe(true, false),
            "a positive (possibly stale) confirmation alone is not enough without a FRESH \
             liveness check confirming the replacement watchdog is still alive"
        );
        assert!(!orphan_release_is_safe(false, false));
    }

    /// Mirrors the real orphan-drain call site's exact shape: an orphan
    /// `AbortHandle`, a `ptt_safe_tx`/`ptt_safe_rx` oneshot that resolves
    /// to `false`, a live replacement watchdog, and the same bounded-
    /// `timeout` + `orphan_safe_to_abort` + `orphan_release_is_safe` +
    /// conditional-drain sequence `start_hamlib_component` runs. The
    /// orphan must survive: it may be the only task still trying to unkey
    /// a physically-keyed radio.
    #[tokio::test]
    async fn the_drain_call_site_retains_the_orphan_when_ptt_safety_is_not_confirmed() {
        let orphan_task = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let mut hamlib_orphans = vec![orphan_task.abort_handle()];
        let replacement_watchdog = tokio::spawn(async { std::future::pending::<()>().await });

        let (ptt_safe_tx, ptt_safe_rx) = tokio::sync::oneshot::channel::<bool>();
        let _ = ptt_safe_tx.send(false);

        if !hamlib_orphans.is_empty() {
            let confirmed_safe = orphan_safe_to_abort(
                tokio::time::timeout(Duration::from_millis(200), ptt_safe_rx).await,
            );
            let replacement_watchdog_alive = !replacement_watchdog.is_finished();
            if orphan_release_is_safe(confirmed_safe, replacement_watchdog_alive) {
                for orphan in hamlib_orphans.drain(..) {
                    orphan.abort();
                }
            }
        }

        assert!(
            !hamlib_orphans.is_empty(),
            "an orphan must be RETAINED (not drained) when the new generation hasn't \
             confirmed PTT-off is safe"
        );
        assert!(
            !orphan_task.is_finished(),
            "the retained orphan watchdog must still be running, not aborted"
        );

        orphan_task.abort();
        replacement_watchdog.abort();
    }

    /// The flip side, same shape: a `true` confirmation AND a live
    /// replacement watchdog drains (aborts) the orphan.
    #[tokio::test]
    async fn the_drain_call_site_drains_the_orphan_once_ptt_safety_is_confirmed() {
        let orphan_task = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let mut hamlib_orphans = vec![orphan_task.abort_handle()];
        let replacement_watchdog = tokio::spawn(async { std::future::pending::<()>().await });

        let (ptt_safe_tx, ptt_safe_rx) = tokio::sync::oneshot::channel::<bool>();
        let _ = ptt_safe_tx.send(true);

        if !hamlib_orphans.is_empty() {
            let confirmed_safe = orphan_safe_to_abort(
                tokio::time::timeout(Duration::from_millis(200), ptt_safe_rx).await,
            );
            let replacement_watchdog_alive = !replacement_watchdog.is_finished();
            if orphan_release_is_safe(confirmed_safe, replacement_watchdog_alive) {
                for orphan in hamlib_orphans.drain(..) {
                    orphan.abort();
                }
            }
        }

        assert!(
            hamlib_orphans.is_empty(),
            "the orphan must be drained once the new generation confirms PTT-off is safe"
        );
        for _ in 0..1000 {
            if orphan_task.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            orphan_task.is_finished(),
            "the drained orphan must actually be aborted"
        );

        replacement_watchdog.abort();
    }

    /// PAN-19 round-17 review (Codex P1) regression guard: "recheck
    /// watchdog liveness before releasing the orphan". The exact scenario
    /// the finding describes: `ptt_safe` confirmed `true` (buffered,
    /// e.g. because the replacement watchdog inherited an active PTT
    /// timer), but the replacement watchdog has since DIED -- by the time
    /// the drain site actually acts on that buffered confirmation, there
    /// is no longer anything alive to vouch for. The orphan must be
    /// RETAINED, not released: it may be the only thing left that could
    /// ever retry the physical unkey.
    #[tokio::test]
    async fn the_drain_call_site_retains_the_orphan_when_the_replacement_watchdog_has_died() {
        let orphan_task = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let mut hamlib_orphans = vec![orphan_task.abort_handle()];

        // The replacement watchdog already dead by the time the drain
        // site checks it -- simulates it having sent its `ptt_safe`
        // confirmation and then crashed/exited before this ran.
        let replacement_watchdog = tokio::spawn(async {});
        for _ in 0..1000 {
            if replacement_watchdog.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            replacement_watchdog.is_finished(),
            "test setup: the replacement watchdog should have finished"
        );

        let (ptt_safe_tx, ptt_safe_rx) = tokio::sync::oneshot::channel::<bool>();
        let _ = ptt_safe_tx.send(true);

        if !hamlib_orphans.is_empty() {
            let confirmed_safe = orphan_safe_to_abort(
                tokio::time::timeout(Duration::from_millis(200), ptt_safe_rx).await,
            );
            assert!(
                confirmed_safe,
                "test setup: the buffered ptt_safe confirmation must be positive"
            );
            let replacement_watchdog_alive = !replacement_watchdog.is_finished();
            if orphan_release_is_safe(confirmed_safe, replacement_watchdog_alive) {
                for orphan in hamlib_orphans.drain(..) {
                    orphan.abort();
                }
            }
        }

        assert!(
            !hamlib_orphans.is_empty(),
            "the orphan must be RETAINED when the replacement watchdog it was vouched by has \
             since died, even though the buffered ptt_safe confirmation was positive"
        );
        assert!(
            !orphan_task.is_finished(),
            "the retained orphan watchdog must still be running, not aborted"
        );

        orphan_task.abort();
    }
}

#[cfg(all(test, feature = "pancetta-hamlib"))]
mod teardown_stall_tests {
    //! PAN-19 LOW / round-2 review (Codex P1): `teardown_hamlib`'s PTT-off
    //! retry loop runs on the coordinator's main supervision task, so its
    //! up-to-~10s worst case (3 attempts x up to 3s send + a 500ms
    //! inter-attempt sleep) stalls `check_task_handles`'s own re-entry -- a
    //! second concurrent component failure sits undiscovered in
    //! `named_task_handles` until teardown returns.
    //!
    //! An earlier version of this fix skipped the inter-attempt sleep
    //! whenever another component's task had already finished, to shorten
    //! that stall. Codex's round-2 review correctly flagged that as a
    //! regression: it let an unrelated failure elsewhere burn through all 3
    //! PTT-off attempts back-to-back, with no pause for a
    //! transiently-unavailable rig link to recover between tries --
    //! weakening the confirmed-unkey guarantee. These tests now cover the
    //! reverted (correct) behavior: the retry loop's own pacing stays fully
    //! intact and uninterrupted regardless of what else is happening
    //! elsewhere in the coordinator -- the fix only adds a read-only
    //! diagnostic log, it never shortens or skips a wait.
    use super::*;
    use pancetta_config::Config;
    use pancetta_hamlib::{MockRig, RigControl};
    use std::sync::atomic::AtomicBool;

    async fn test_coordinator() -> super::super::ApplicationCoordinator {
        let config = Config::default();
        let shutdown = Arc::new(AtomicBool::new(false));
        super::super::ApplicationCoordinator::new(
            config,
            None,
            true,  // no_audio
            true,  // headless
            false, // metrics
            9090,
            None, // no WAV
            None, // no replay
            None, // no test-tx
            1500.0,
            shutdown,
            Vec::new(), // no config warnings
        )
        .await
        .expect("coordinator creation should succeed")
    }

    /// An UNconnected `MockRig`'s `set_ptt` deterministically fails every
    /// time ("Mock rig not connected"), so this exercises all 3 retry
    /// attempts (and both inter-attempt sleeps) every run.
    fn disconnected_mock_rig() -> Arc<MockRig> {
        Arc::new(MockRig::default())
    }

    /// The regression guard: a concurrent, unrelated component failure
    /// (already sitting finished in `named_task_handles` before teardown
    /// even starts -- the most favorable case for the reverted
    /// "skip the sleep" optimization to have kicked in) must NOT shorten or
    /// skip the PTT-off retry pacing at all.
    #[tokio::test]
    async fn concurrent_unrelated_failure_does_not_shorten_ptt_off_retry_pacing() {
        let mut coordinator = test_coordinator().await;
        coordinator.rig_handle = Some(disconnected_mock_rig());

        let finished: tokio::task::JoinHandle<anyhow::Result<()>> = tokio::spawn(async { Ok(()) });
        for _ in 0..1000 {
            if finished.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(finished.is_finished(), "test setup: should have finished");
        coordinator
            .named_task_handles
            .push((ComponentId::DxCluster, finished));

        let start = std::time::Instant::now();
        coordinator.teardown_hamlib().await;
        let elapsed = start.elapsed();

        // 2 inter-attempt sleeps x 500ms between the 3 (always-failing)
        // attempts -- this must hold even with another component's
        // finished task already sitting in `named_task_handles`.
        assert!(
            elapsed >= Duration::from_millis(900),
            "a concurrent unrelated-component failure must not shorten or skip the PTT-off \
             retry loop's own pacing -- took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn still_sleeps_between_attempts_when_nothing_else_is_pending() {
        let mut coordinator = test_coordinator().await;
        coordinator.rig_handle = Some(disconnected_mock_rig());

        let start = std::time::Instant::now();
        coordinator.teardown_hamlib().await;
        let elapsed = start.elapsed();

        assert!(
            elapsed >= Duration::from_millis(900),
            "teardown_hamlib should sleep between PTT-off retry attempts -- took {elapsed:?}"
        );
    }
}

#[cfg(all(test, feature = "pancetta-hamlib"))]
mod teardown_replay_tests {
    //! PAN-19 LOW / round-2 review (Codex P1): the message re-injection
    //! loop at the end of `teardown_hamlib` originally used a *blocking*
    //! `crossbeam_channel::Sender::send` inside an async fn. Normally
    //! harmless -- `teardown_hamlib`'s drain immediately before it just
    //! freed exactly as many slots as it collected messages to replay, so
    //! a fresh `teardown_hamlib` call can never itself construct a full
    //! channel at replay time (replay count is provably <= drain count <=
    //! channel capacity, with no `.await` between the two loops for
    //! anything else to interleave through). The real risk is a genuinely
    //! concurrent producer on another OS thread racing into that gap --
    //! which, being a true multi-thread race on a single-instruction-wide
    //! window, isn't reliably reproducible by a test driving real
    //! wall-clock timing (confirmed: an earlier version of this test tried
    //! exactly that with a sustained background "saturator" task and did
    //! not manage to trigger the blocking path even once).
    //!
    //! The fix (round 1) replaced the blocking `send` with a single
    //! `try_send`, dropping the message on `Full`. Round-2 review correctly
    //! flagged unconditional dropping as unsafe for a preserved `SetPtt {
    //! state: false }` unkey, so the replay step is now its own method,
    //! `replay_or_fallback` (bounded retry, plus a direct-rig fallback
    //! specifically for PTT-off) -- see its own doc comment for the full
    //! reasoning. Extracting it as a standalone method also makes the
    //! "channel stays full for the whole retry window" scenario
    //! deterministically testable in isolation from `teardown_hamlib`'s own
    //! drain (which otherwise structurally guarantees room, as above): a
    //! channel pre-filled by the TEST, then handed directly to
    //! `replay_or_fallback`, stays full for that entire isolated call since
    //! nothing else ever touches it -- no race needed. See
    //! `replay_or_fallback_commands_ptt_off_directly_when_channel_stays_full`
    //! and its sibling below.
    use super::*;
    use pancetta_config::Config;

    async fn test_coordinator() -> super::super::ApplicationCoordinator {
        let config = Config::default();
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        super::super::ApplicationCoordinator::new(
            config,
            None,
            true,  // no_audio
            true,  // headless
            false, // metrics
            9090,
            None, // no WAV
            None, // no replay
            None, // no test-tx
            1500.0,
            shutdown,
            Vec::new(), // no config warnings
        )
        .await
        .expect("coordinator creation should succeed")
    }

    fn set_freq_msg() -> ComponentMessage {
        ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetFrequency {
                vfo: 0,
                frequency: 14_074_000,
            }),
            Instant::now(),
        )
    }

    /// PAN-19 round-11 review (Codex P1): a fresh "nothing applied yet"
    /// supersession tracker for `deliver_pending_hamlib_state` -- tests
    /// that aren't specifically exercising the supersession check use
    /// this so delivery behaves exactly as it did before that check
    /// existed (never treats anything as stale).
    fn no_supersession() -> std::sync::Arc<std::sync::Mutex<Option<u64>>> {
        std::sync::Arc::new(std::sync::Mutex::new(None))
    }

    /// The frequency sibling of `no_supersession()` -- PAN-35 keyed
    /// `last_applied_frequency_id` by VFO, so its "nothing applied yet"
    /// state is an empty map rather than `None`.
    fn no_frequency_supersession(
    ) -> std::sync::Arc<std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, u64>>> {
        std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()))
    }

    /// PAN-19 round-17 review (Codex P1), updated round-19 (now a count,
    /// not a boolean): a fresh, not-in-flight tracker for
    /// `deliver_pending_hamlib_state`'s `hamlib_command_in_flight` param --
    /// tests not specifically exercising that count use this.
    fn not_in_flight() -> std::sync::Arc<std::sync::atomic::AtomicU32> {
        std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0))
    }

    /// PAN-19 round-19 review (Codex P1): a fresh "nothing producer-marked"
    /// tracker for `deliver_pending_hamlib_state`'s
    /// `producer_marked_frequency_id`/`producer_marked_split_id` params --
    /// tests not specifically exercising that retirement mechanism use
    /// this.
    fn no_producer_mark() -> std::sync::Arc<std::sync::Mutex<Option<u64>>> {
        std::sync::Arc::new(std::sync::Mutex::new(None))
    }

    /// The frequency sibling of `no_producer_mark()` -- PAN-35 keyed
    /// `producer_marked_frequency_id` by VFO.
    fn no_frequency_producer_mark(
    ) -> std::sync::Arc<std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, u64>>> {
        std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()))
    }

    /// The exact mechanism: `try_send` on a full bounded channel returns
    /// `Err(Full)` immediately rather than blocking the calling thread.
    #[test]
    fn try_send_on_a_full_channel_returns_immediately_instead_of_blocking() {
        let (sender, _receiver) = crossbeam_channel::bounded::<ComponentMessage>(1);
        sender
            .try_send(set_freq_msg())
            .expect("first send fills the one slot");

        let start = std::time::Instant::now();
        let result = sender.try_send(set_freq_msg());
        let elapsed = start.elapsed();

        assert!(
            matches!(result, Err(crossbeam_channel::TrySendError::Full(_))),
            "try_send on a full channel must return Err(Full), not succeed or panic"
        );
        assert!(
            elapsed < Duration::from_millis(50),
            "try_send must return immediately rather than blocking -- took {elapsed:?}"
        );
    }

    /// Normal-path integration coverage: `teardown_hamlib`'s replay of a
    /// drained "safe" message back onto a freshly-emptied channel must
    /// still work and complete quickly (the drain-then-replay sequence can
    /// never itself overflow the channel it just emptied -- see the module
    /// doc comment above).
    #[tokio::test]
    async fn teardown_replays_a_safe_message_and_completes_quickly() {
        let mut coordinator = test_coordinator().await;
        coordinator.rig_handle = None; // skip the PTT-off retry loop entirely

        let (sender, receiver) = coordinator
            .message_bus
            .get_or_create_channel(ComponentId::Hamlib)
            .await
            .unwrap();
        sender.try_send(set_freq_msg()).unwrap();

        let start = std::time::Instant::now();
        let result =
            tokio::time::timeout(Duration::from_secs(2), coordinator.teardown_hamlib()).await;
        let elapsed = start.elapsed();

        assert!(
            result.is_ok(),
            "teardown_hamlib should not hang on the normal path"
        );
        assert!(elapsed < Duration::from_millis(500), "took {elapsed:?}");
        assert!(
            receiver.try_recv().is_ok(),
            "the safe SetFrequency message should have been replayed back onto the channel"
        );
    }

    /// PAN-19 round-2 review (Codex P1) regression guard: a preserved
    /// `SetPtt { state: false }` unkey must NOT be silently dropped when
    /// the replay channel stays full -- it must reach the rig via the
    /// direct fallback.
    ///
    /// Tests `replay_or_fallback` directly rather than through the full
    /// `teardown_hamlib` drain+replay sequence: as established in the
    /// module doc comment above, a channel's own capacity is a hard,
    /// single limit -- draining it and replaying the SAME count back in
    /// can never itself overflow the channel it just emptied, so
    /// constructing "channel stays full for the entire replay window"
    /// deterministically requires isolating the replay call from the drain
    /// that would otherwise guarantee room. Pre-filling a fresh channel
    /// with an unrelated message BEFORE calling `replay_or_fallback`
    /// directly does exactly that: nothing else touches this channel
    /// during the isolated call, so it's provably full for the entire
    /// ~100ms bounded retry window -- no race required.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn replay_or_fallback_commands_ptt_off_directly_when_channel_stays_full() {
        use pancetta_hamlib::{PttState, RigControl, Vfo};

        let mut coordinator = test_coordinator().await;
        let mock = Arc::new(pancetta_hamlib::MockRig::default());
        mock.connect().await.expect("test rig should connect");
        // Start keyed, so "ends up Off" is a meaningful assertion below,
        // not just an already-Off default.
        mock.set_ptt(Vfo::Current, PttState::On)
            .await
            .expect("test rig should key");
        mock.reset_operation_count();
        coordinator.rig_handle = Some(mock.clone());

        // Capacity 1, pre-filled -- stays full for the whole isolated call
        // below since nothing else ever drains it.
        let (sender, _receiver) = crossbeam_channel::bounded::<ComponentMessage>(1);
        sender
            .try_send(set_freq_msg())
            .expect("pre-fill the one slot");

        let ptt_off_msg = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt { state: false }),
            Instant::now(),
        );

        let start = std::time::Instant::now();
        coordinator.replay_or_fallback(&sender, ptt_off_msg).await;
        let elapsed = start.elapsed();

        // 5 attempts x 20ms between them -- a bounded retry window, not an
        // unbounded block.
        assert!(
            elapsed < Duration::from_secs(1),
            "replay_or_fallback must not block indefinitely -- took {elapsed:?}"
        );
        assert_eq!(
            mock.get_operation_count(),
            1,
            "the direct PTT-off fallback must have fired exactly once when the channel \
             stayed full for the entire retry window"
        );
        assert_eq!(
            mock.get_ptt(Vfo::Current).await.expect("PTT state"),
            PttState::Off,
            "the rig must end up unkeyed via the fallback rather than silently left keyed"
        );
        assert!(
            !coordinator.ptt_active.load(Ordering::Acquire),
            "ptt_active must reflect the fallback's successful unkey"
        );
    }

    /// The flip side: a non-safety-critical message (frequency/split/mode)
    /// still logs-and-drops as a last resort when the channel stays full
    /// for the entire retry window -- it must not block indefinitely
    /// either, and it must not spuriously touch the rig (no PTT-off
    /// fallback for a non-PTT message).
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn replay_or_fallback_drops_non_ptt_messages_after_exhausting_retries() {
        let mut coordinator = test_coordinator().await;
        let mock = Arc::new(pancetta_hamlib::MockRig::default());
        coordinator.rig_handle = Some(mock.clone());
        // Deliberately NOT connected -- if this path incorrectly tried to
        // touch the rig, `get_operation_count()` would still read 0
        // (set_ptt on a disconnected mock fails without incrementing the
        // counter), so this alone doesn't prove non-interference, but a
        // disconnected rig at least ensures nothing here could succeed
        // silently.

        let (sender, _receiver) = crossbeam_channel::bounded::<ComponentMessage>(1);
        sender
            .try_send(set_freq_msg())
            .expect("pre-fill the one slot");

        let start = std::time::Instant::now();
        coordinator
            .replay_or_fallback(&sender, set_freq_msg())
            .await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(1),
            "replay_or_fallback must not block indefinitely on a non-PTT message either -- \
             took {elapsed:?}"
        );
        assert_eq!(
            mock.get_operation_count(),
            0,
            "a non-PTT message must never trigger the PTT-off rig fallback"
        );
    }

    /// PAN-19 round-5 review (Codex P1): a `SetFrequency` that exhausts its
    /// bounded retry (channel stayed full) must NOT be silently dropped --
    /// it must be queued in `self.hamlib_pending_frequency` for delivery
    /// to the next Hamlib generation (see `deliver_pending_hamlib_state`
    /// below).
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn replay_or_fallback_queues_a_stuck_set_frequency_instead_of_dropping_it() {
        let mut coordinator = test_coordinator().await;
        assert!(coordinator
            .hamlib_pending_frequency
            .lock()
            .unwrap()
            .is_empty());

        let (sender, _receiver) = crossbeam_channel::bounded::<ComponentMessage>(1);
        sender
            .try_send(set_freq_msg())
            .expect("pre-fill the one slot");

        coordinator
            .replay_or_fallback(&sender, set_freq_msg())
            .await;

        assert!(
            !coordinator
                .hamlib_pending_frequency
                .lock()
                .unwrap()
                .is_empty(),
            "a SetFrequency that couldn't be replayed must be queued, not dropped"
        );
    }

    /// The `SetSplit` sibling of the test above -- the non-self-healing
    /// case Codex specifically flagged: nothing else in this codebase
    /// re-asserts split state, so a dropped `SetSplit` could leave the rig
    /// silently holding stale split config.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn replay_or_fallback_queues_a_stuck_set_split_instead_of_dropping_it() {
        let mut coordinator = test_coordinator().await;
        assert!(coordinator.hamlib_pending_split.lock().unwrap().is_none());

        let (sender, _receiver) = crossbeam_channel::bounded::<ComponentMessage>(1);
        sender
            .try_send(set_freq_msg())
            .expect("pre-fill the one slot");

        let split_msg = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000,
            }),
            Instant::now(),
        );

        coordinator.replay_or_fallback(&sender, split_msg).await;

        assert!(
            coordinator.hamlib_pending_split.lock().unwrap().is_some(),
            "a SetSplit that couldn't be replayed must be queued, not dropped -- it isn't \
             self-healing like SetFrequency"
        );
    }

    /// PAN-19 round-19 review (Codex P1) regression guard: "preserve the
    /// newest split in teardown fallback". The exact scenario the finding
    /// describes: a NEWER split has already failed CAT application and
    /// populated `hamlib_pending_split` (e.g. via `finish_rig_command`),
    /// while an OLDER split remains queued behind it when the generation
    /// crashes; teardown drains that older message via `replay_or_fallback`,
    /// replay is full/disconnected, and this fallback path used to
    /// unconditionally overwrite the slot -- replacing the newer desired
    /// state with the stale one. The next generation could then
    /// successfully apply the obsolete TX frequency and unmute PTT after
    /// the correct state was lost. This proves the fallback now reuses
    /// the SAME newest-wins comparison `finish_rig_command` uses: the
    /// pending slot must still hold the NEWER split afterward, untouched.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn replay_or_fallback_never_lets_an_older_split_clobber_a_newer_retained_one() {
        let mut coordinator = test_coordinator().await;

        // Constructed FIRST, so its globally-monotonic id is strictly
        // smaller -- genuinely the older message.
        let older_split = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000, // stale, wrong TX frequency
            }),
            Instant::now(),
        );
        // Constructed SECOND, so its id is strictly larger.
        let newer_split = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_074_000, // the CORRECT, current TX frequency
            }),
            Instant::now(),
        );
        let newer_id = newer_split.id;
        assert!(newer_id > older_split.id, "test setup invariant");

        // Already retained -- simulates a newer split's CAT application
        // having already failed and populated the slot (finish_rig_command),
        // BEFORE the older message (queued behind it) drains through
        // teardown.
        *coordinator.hamlib_pending_split.lock().unwrap() = Some(newer_split);

        // Force the fallback path (replay never succeeds).
        let (sender, _receiver) = crossbeam_channel::bounded::<ComponentMessage>(1);
        sender
            .try_send(set_freq_msg())
            .expect("pre-fill the one slot");

        coordinator.replay_or_fallback(&sender, older_split).await;

        let retained = coordinator.hamlib_pending_split.lock().unwrap();
        assert_eq!(
            retained.as_ref().map(|m| m.id),
            Some(newer_id),
            "an older split draining through teardown's fallback must never clobber an \
             already-retained NEWER split -- doing so would lose the newer desired TX \
             frequency and let a later retry re-apply the old, wrong one"
        );
        assert!(
            matches!(
                retained.as_ref().map(|m| &m.message_type),
                Some(MessageType::RigControl(
                    crate::message_bus::RigControlMessage::SetSplit {
                        enabled: true,
                        tx_frequency: 14_074_000,
                    }
                ))
            ),
            "the retained pending command must still carry the NEWER, correct TX frequency"
        );
    }

    /// The flip side: when the fallback message genuinely IS newer than
    /// whatever's retained (or nothing is retained), it must still
    /// replace it -- the fix must not become a one-way ratchet that stops
    /// updating the slot.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn replay_or_fallback_still_replaces_an_older_retained_split_with_a_newer_one() {
        let mut coordinator = test_coordinator().await;

        let older_split = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000,
            }),
            Instant::now(),
        );
        *coordinator.hamlib_pending_split.lock().unwrap() = Some(older_split);

        let newer_split = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_074_000,
            }),
            Instant::now(),
        );
        let newer_id = newer_split.id;

        let (sender, _receiver) = crossbeam_channel::bounded::<ComponentMessage>(1);
        sender
            .try_send(set_freq_msg())
            .expect("pre-fill the one slot");

        coordinator.replay_or_fallback(&sender, newer_split).await;

        assert_eq!(
            coordinator
                .hamlib_pending_split
                .lock()
                .unwrap()
                .as_ref()
                .map(|m| m.id),
            Some(newer_id),
            "a newer fallback message must still replace an older retained one"
        );
    }

    /// PAN-19 round-5 review (Codex P1): a queued pending command must
    /// actually be DELIVERED once the next generation's message loop is
    /// ready -- not just held forever. Covers both `SetFrequency` and
    /// `SetSplit`, and confirms the pending slots are cleared (drained,
    /// not duplicated on a later call) once delivered.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn deliver_pending_hamlib_state_sends_queued_commands_to_the_new_generation() {
        let coordinator = test_coordinator().await;

        coordinator
            .hamlib_pending_frequency
            .lock()
            .unwrap()
            .insert(pancetta_hamlib::Vfo::A, set_freq_msg());
        let split_msg = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000,
            }),
            Instant::now(),
        );
        *coordinator.hamlib_pending_split.lock().unwrap() = Some(split_msg);

        let (sender, receiver) = crossbeam_channel::bounded::<ComponentMessage>(4);
        deliver_pending_hamlib_state(
            &coordinator.hamlib_pending_frequency,
            &coordinator.hamlib_pending_split,
            &no_frequency_supersession(),
            &no_supersession(),
            &sender,
            &not_in_flight(),
            &no_frequency_producer_mark(),
            &no_producer_mark(),
        );

        assert!(
            coordinator
                .hamlib_pending_frequency
                .lock()
                .unwrap()
                .is_empty(),
            "the pending frequency slot must be drained once delivered"
        );
        assert!(
            coordinator.hamlib_pending_split.lock().unwrap().is_none(),
            "the pending split slot must be drained once delivered"
        );

        let mut delivered_types: Vec<bool> = Vec::new(); // true = SetFrequency, false = SetSplit
        while let Ok(msg) = receiver.try_recv() {
            match msg.message_type {
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetFrequency {
                    ..
                }) => delivered_types.push(true),
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                    ..
                }) => delivered_types.push(false),
                other => panic!("unexpected message delivered: {other:?}"),
            }
        }
        assert_eq!(
            delivered_types.len(),
            2,
            "both the pending SetFrequency and SetSplit must have been delivered onto the \
             new generation's channel"
        );
        assert!(
            delivered_types.contains(&true),
            "SetFrequency must be delivered"
        );
        assert!(
            delivered_types.contains(&false),
            "SetSplit must be delivered"
        );
    }

    /// PAN-19 round-17 review (Codex P1) regression guard: "cover the
    /// pending-command handoff before CAT starts". A successful hand-off
    /// (the pending slot clears AND the message lands on the channel)
    /// must mark `hamlib_command_in_flight` `true` immediately -- BEFORE
    /// the message loop ever picks it up and constructs its own
    /// `HamlibCommandInFlightGuard` -- so there is no gap between "no
    /// longer pending" and "marked in-flight" for a concurrent PTT-on's
    /// gate-check to slip through.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn deliver_pending_hamlib_state_marks_in_flight_on_successful_handoff() {
        let coordinator = test_coordinator().await;
        *coordinator.hamlib_pending_split.lock().unwrap() = Some(set_freq_msg());

        let (sender, _receiver) = crossbeam_channel::bounded::<ComponentMessage>(4);
        let in_flight = not_in_flight();
        deliver_pending_hamlib_state(
            &coordinator.hamlib_pending_frequency,
            &coordinator.hamlib_pending_split,
            &no_frequency_supersession(),
            &no_supersession(),
            &sender,
            &in_flight,
            &no_frequency_producer_mark(),
            &no_producer_mark(),
        );

        assert_eq!(
            in_flight.load(Ordering::Acquire),
            1,
            "a successful hand-off to the channel must mark hamlib_command_in_flight true \
             immediately, at producer time -- not wait for the message loop to pick the \
             message up and construct its own guard"
        );
    }

    /// The flip side: a FAILED hand-off (channel full) must NOT mark
    /// in-flight -- nothing was actually enqueued, so there is nothing in
    /// flight; the pending slot itself (already correctly repopulated)
    /// remains the source of truth for "still needs delivery".
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn deliver_pending_hamlib_state_does_not_mark_in_flight_on_failed_handoff() {
        let coordinator = test_coordinator().await;
        *coordinator.hamlib_pending_split.lock().unwrap() = Some(set_freq_msg());

        // A full channel at delivery time.
        let (sender, _receiver) = crossbeam_channel::bounded::<ComponentMessage>(1);
        sender
            .try_send(set_freq_msg())
            .expect("pre-fill the one slot");

        let in_flight = not_in_flight();
        deliver_pending_hamlib_state(
            &coordinator.hamlib_pending_frequency,
            &coordinator.hamlib_pending_split,
            &no_frequency_supersession(),
            &no_supersession(),
            &sender,
            &in_flight,
            &no_frequency_producer_mark(),
            &no_producer_mark(),
        );

        assert!(
            coordinator.hamlib_pending_split.lock().unwrap().is_some(),
            "test setup: the hand-off must have failed (channel full)"
        );
        assert_eq!(
            in_flight.load(Ordering::Acquire),
            0,
            "a FAILED hand-off must not mark hamlib_command_in_flight -- nothing was actually \
             enqueued, so nothing is in flight"
        );
    }

    /// PAN-19 round-18 review (Codex P1) regression guard: "establish the
    /// handoff marker before publishing the command". A fast consumer --
    /// receives the message and runs its own full lifecycle (constructs
    /// `HamlibCommandInFlightGuard`, mirroring the message loop's own
    /// wrapping of the CAT call, then drops it immediately, mirroring an
    /// instant CAT call) concurrently with the producer's own call to
    /// `deliver_pending_hamlib_state`. With the pre-round-18 ordering
    /// (mark the flag AFTER `try_send` succeeds), a fast-enough consumer's
    /// clear could land BEFORE the producer's own delayed store, letting
    /// that stale write resurrect the flag to `true` with nothing left to
    /// ever clear it -- permanently muting an otherwise healthy
    /// generation. With the fix (mark BEFORE `try_send`), the flag must
    /// reliably end up `false` regardless of how fast the consumer is.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn deliver_pending_hamlib_state_does_not_strand_in_flight_true_for_a_fast_consumer() {
        let coordinator = test_coordinator().await;
        *coordinator.hamlib_pending_split.lock().unwrap() = Some(set_freq_msg());

        let (sender, receiver) = crossbeam_channel::bounded::<ComponentMessage>(4);
        let in_flight = not_in_flight();
        let producer_marked_split_id = no_producer_mark();

        // Spawn the "consumer" first so it's already blocked on `recv()`
        // -- ready to run its full lifecycle to completion the instant
        // the message arrives, maximizing the chance of the exact
        // interleaving the finding describes. Mirrors the REAL consumer
        // arm's round-19 logic exactly: adopt the producer's existing
        // increment for a producer-marked message rather than adding a
        // second one.
        let in_flight_for_consumer = in_flight.clone();
        let producer_marked_split_id_for_consumer = producer_marked_split_id.clone();
        let consumer = tokio::task::spawn_blocking(move || {
            let message = receiver.recv().expect("the message must be delivered");
            let guard =
                if take_producer_mark_if_matching(&message, &producer_marked_split_id_for_consumer)
                {
                    HamlibCommandInFlightGuard::adopt(in_flight_for_consumer)
                } else {
                    HamlibCommandInFlightGuard::new(in_flight_for_consumer)
                };
            drop(guard);
        });

        deliver_pending_hamlib_state(
            &coordinator.hamlib_pending_frequency,
            &coordinator.hamlib_pending_split,
            &no_frequency_supersession(),
            &no_supersession(),
            &sender,
            &in_flight,
            &no_frequency_producer_mark(),
            &producer_marked_split_id,
        );

        consumer.await.expect("consumer task must not panic");

        assert_eq!(
            in_flight.load(Ordering::Acquire),
            0,
            "a fast consumer that completes its full lifecycle concurrently with the \
             producer's own call must not leave hamlib_command_in_flight stranded true -- it \
             must end up false, not permanently mute an otherwise healthy generation"
        );
    }

    /// PAN-19 round-18 review (Codex P1) regression guard: proves the
    /// ORDER directly, not just the eventual value -- the marker must be
    /// set BEFORE the channel send is attempted. This is deterministic
    /// (not a timing-luck race): crossbeam channels establish a
    /// happens-before relationship between what the sender did before
    /// `try_send` and what the receiver observes once `recv()` returns,
    /// so with the fix, ANY receiver that successfully receives the
    /// message is GUARANTEED to see `hamlib_command_in_flight == true`
    /// the instant it checks -- regardless of real-time scheduling.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn deliver_pending_hamlib_state_marks_in_flight_before_the_channel_send_not_after() {
        let coordinator = test_coordinator().await;
        *coordinator.hamlib_pending_split.lock().unwrap() = Some(set_freq_msg());

        let (sender, receiver) = crossbeam_channel::bounded::<ComponentMessage>(4);
        let in_flight = not_in_flight();

        let in_flight_for_consumer = in_flight.clone();
        let consumer = tokio::task::spawn_blocking(move || {
            let _message = receiver.recv().expect("the message must be delivered");
            // Checked THE INSTANT the message is received -- proves the
            // producer's store happened BEFORE the send, not just
            // eventually before this check.
            in_flight_for_consumer.load(Ordering::Acquire)
        });

        deliver_pending_hamlib_state(
            &coordinator.hamlib_pending_frequency,
            &coordinator.hamlib_pending_split,
            &no_frequency_supersession(),
            &no_supersession(),
            &sender,
            &in_flight,
            &no_frequency_producer_mark(),
            &no_producer_mark(),
        );

        let observed_by_consumer = consumer.await.expect("consumer task must not panic");
        assert!(
            observed_by_consumer > 0,
            "the in-flight marker must be set BEFORE the channel send is attempted -- any \
             consumer that successfully receives the message must already observe it nonzero, \
             not race against a delayed post-send store"
        );
    }

    /// PAN-19 round-18 review (Codex P1) regression guard: a fully
    /// DETERMINISTIC version of the ordering claim above, exercising
    /// `mark_in_flight_then_send` directly. The two tests above rely on
    /// real thread scheduling to (maybe) reproduce the exact race -- a
    /// real crossbeam channel's send-to-receive latency turns out fast
    /// enough in practice that even the PRE-fix (mark-after-send)
    /// ordering usually "wins" the race by luck, so those tests can't be
    /// trusted to fail reliably against a regression (confirmed by
    /// hand: reverting the fix and running them repeatedly still mostly
    /// passes). This test instead injects a `send` closure that runs a
    /// simulated consumer's FULL lifecycle -- checking the flag, then
    /// running `HamlibCommandInFlightGuard`'s construct+drop -- synchronously,
    /// INSIDE the closure, at the exact instant the real function would
    /// call `sender.try_send`. This deterministically reproduces "the
    /// consumer runs to completion during the send call" on every single
    /// run, no scheduling luck required, and is what actually caught the
    /// round-17 -> round-18 regression class this finding describes.
    #[test]
    fn mark_in_flight_then_send_observes_true_synchronously_during_the_send_call() {
        let split_msg = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000,
            }),
            Instant::now(),
        );
        let in_flight: Arc<std::sync::atomic::AtomicU32> =
            Arc::new(std::sync::atomic::AtomicU32::new(0));
        let producer_marked_id = no_producer_mark();

        let observed_during_send = std::cell::Cell::new(None);
        let result =
            mark_in_flight_then_send(split_msg, &in_flight, &producer_marked_id, |message| {
                // Simulate the consumer's FULL lifecycle happening
                // synchronously "during" the send -- the tightest possible
                // interleaving, worse than any real scheduling could ever
                // produce, so a fix that's only correct "most of the time"
                // fails this every run. `adopt` (not `new`) mirrors the real
                // consumer arm's behavior for a producer-marked message.
                observed_during_send.set(Some(in_flight.load(Ordering::Acquire)));
                let guard = HamlibCommandInFlightGuard::adopt(in_flight.clone());
                drop(guard);
                // Standing in for successful delivery -- there's no real
                // channel in this test.
                drop(message);
                Ok(())
            });

        assert!(result.is_ok(), "the simulated send must succeed");
        assert_eq!(
            observed_during_send.get(),
            Some(1),
            "the in-flight count must already be nonzero the instant the send is attempted -- a \
             consumer running synchronously inside the send call must never observe zero"
        );
        assert_eq!(
            in_flight.load(Ordering::Acquire),
            0,
            "after the consumer's full lifecycle (adopt + drop) completes during the send, the \
             count must end up 0, not resurrected by a delayed producer-side write -- \
             mark_in_flight_then_send must not write anything AFTER the send call on success"
        );
    }

    /// The failure path: `send` returning `Err` must roll the marker
    /// back, and the message must come back out so it can be re-queued.
    #[test]
    fn mark_in_flight_then_send_rolls_back_on_failure() {
        let split_msg = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000,
            }),
            Instant::now(),
        );
        let sent_id = split_msg.id;
        let in_flight: Arc<std::sync::atomic::AtomicU32> =
            Arc::new(std::sync::atomic::AtomicU32::new(0));
        let producer_marked_id = no_producer_mark();

        let result = mark_in_flight_then_send(split_msg, &in_flight, &producer_marked_id, Err);

        let returned = result.expect_err("the simulated send must fail");
        assert_eq!(
            returned.id, sent_id,
            "the original message must be returned on failure so it can be re-queued"
        );
        assert_eq!(
            in_flight.load(Ordering::Acquire),
            0,
            "a failed send must roll the in-flight count back to 0 -- nothing was \
             actually handed off"
        );
        assert!(
            producer_marked_id.lock().unwrap().is_none(),
            "a failed send must also clear the producer-marked tracker -- nothing is \
             outstanding to retire later"
        );
    }

    /// PAN-19 round-19 review (Codex P1) regression guard: on a
    /// SUCCESSFUL send, the producer-marked tracker must be left SET
    /// (with this message's id) -- retiring it is the consumer's job
    /// (`take_producer_mark_if_matching`), not this function's. Leaving
    /// it set is what lets a later consumer-side discard (superseded) or
    /// apply (adopt) find and retire the SAME increment this call made.
    #[test]
    fn mark_in_flight_then_send_leaves_the_producer_mark_set_on_success() {
        let split_msg = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000,
            }),
            Instant::now(),
        );
        let sent_id = split_msg.id;
        let in_flight: Arc<std::sync::atomic::AtomicU32> =
            Arc::new(std::sync::atomic::AtomicU32::new(0));
        let producer_marked_id = no_producer_mark();

        let result = mark_in_flight_then_send(split_msg, &in_flight, &producer_marked_id, |m| {
            drop(m);
            Ok(())
        });

        assert!(result.is_ok(), "the simulated send must succeed");
        assert_eq!(
            *producer_marked_id.lock().unwrap(),
            Some(sent_id),
            "a successful send must leave the producer-marked tracker set to THIS message's \
             id -- it's the consumer's responsibility to retire it later, not this function's"
        );
    }

    /// PAN-19 round-7 review (Codex P1): an earlier version of
    /// `deliver_pending_hamlib_state` `take()`'d the pending slot, tried
    /// `try_send`, and on failure only logged -- discarding the
    /// `TrySendError`'s carried-back message entirely, so a command that
    /// arrived here right as another producer happened to fill the
    /// brand-new channel was permanently lost. This pins the fix: a full
    /// channel at delivery time must leave the pending command still
    /// queued afterward (not silently gone), and it must succeed once
    /// delivered again with room in the channel.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn deliver_pending_hamlib_state_requeues_on_a_full_channel_and_delivers_later() {
        let coordinator = test_coordinator().await;
        let split_msg = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000,
            }),
            Instant::now(),
        );
        *coordinator.hamlib_pending_split.lock().unwrap() = Some(split_msg);

        // A full channel at delivery time.
        let (sender, receiver) = crossbeam_channel::bounded::<ComponentMessage>(1);
        sender
            .try_send(set_freq_msg())
            .expect("pre-fill the one slot");

        deliver_pending_hamlib_state(
            &coordinator.hamlib_pending_frequency,
            &coordinator.hamlib_pending_split,
            &no_frequency_supersession(),
            &no_supersession(),
            &sender,
            &not_in_flight(),
            &no_frequency_producer_mark(),
            &no_producer_mark(),
        );

        assert!(
            coordinator.hamlib_pending_split.lock().unwrap().is_some(),
            "a failed delivery (channel full) must leave the pending command still queued, \
             not silently dropped"
        );

        // Drain the blocker, then retry delivery with room -- must succeed.
        receiver.try_recv().expect("drain the blocker");
        deliver_pending_hamlib_state(
            &coordinator.hamlib_pending_frequency,
            &coordinator.hamlib_pending_split,
            &no_frequency_supersession(),
            &no_supersession(),
            &sender,
            &not_in_flight(),
            &no_frequency_producer_mark(),
            &no_producer_mark(),
        );

        assert!(
            coordinator.hamlib_pending_split.lock().unwrap().is_none(),
            "the pending slot must be cleared once delivery succeeds"
        );
        let delivered = receiver
            .try_recv()
            .expect("the queued SetSplit must have been delivered once the channel had room");
        assert!(
            matches!(
                delivered.message_type,
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                    enabled: true,
                    tx_frequency: 14_078_000
                })
            ),
            "the delivered message must be the SAME SetSplit that failed to deliver earlier"
        );
    }

    /// PAN-19 round-11 review (Codex P1) regression guard: "do not deliver
    /// stale split state after newer commands". A pending `SetSplit` is
    /// queued (simulating a prior generation's failed teardown replay),
    /// then a NEWER `SetSplit` is sent through the normal path BEFORE the
    /// pending retry fires -- mirrored here by recording the newer
    /// message's globally-monotonic `id` as "last applied", exactly what
    /// the real message loop's own `SetSplit` arm does when it actually
    /// processes a normal command. The stale pending command must NEVER
    /// be delivered afterward: it must be dropped, so the rig ends up in
    /// the newer, correct state (not reverted to a stale TX frequency).
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn deliver_pending_hamlib_state_drops_a_stale_split_superseded_by_a_newer_one() {
        let coordinator = test_coordinator().await;

        // The STALE pending command, captured from a prior failed replay.
        let stale_split = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000, // stale, wrong TX frequency
            }),
            Instant::now(),
        );
        *coordinator.hamlib_pending_split.lock().unwrap() = Some(stale_split);

        // A NEWER SetSplit "sent through the normal path" -- constructed
        // AFTER the stale one above, so its globally-monotonic `id` is
        // larger (see `generate_message_id` in message_bus.rs). Recording
        // its id as "last applied" mirrors exactly what the real message
        // loop's SetSplit arm does when it actually processes a normal
        // command.
        let newer_split = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_074_000, // the CORRECT, current TX frequency
            }),
            Instant::now(),
        );
        let last_applied_split_id = Arc::new(std::sync::Mutex::new(Some(newer_split.id)));

        let (sender, receiver) = crossbeam_channel::bounded::<ComponentMessage>(4);
        deliver_pending_hamlib_state(
            &coordinator.hamlib_pending_frequency,
            &coordinator.hamlib_pending_split,
            &no_frequency_supersession(),
            &last_applied_split_id,
            &sender,
            &not_in_flight(),
            &no_frequency_producer_mark(),
            &no_producer_mark(),
        );

        assert!(
            coordinator.hamlib_pending_split.lock().unwrap().is_none(),
            "the stale pending SetSplit must be dropped (cleared), not left queued to retry \
             again"
        );
        assert!(
            receiver.try_recv().is_err(),
            "the stale pending SetSplit must never be delivered onto the channel once a newer \
             command has already gone through -- delivering it would revert the rig's split \
             state to the old, wrong TX frequency"
        );
    }

    /// The flip side: a pending command that is NOT superseded (nothing
    /// newer has been applied) must still deliver normally -- the
    /// supersession check must not become overly conservative and start
    /// dropping legitimate, still-current pending commands.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn deliver_pending_hamlib_state_still_delivers_when_not_superseded() {
        let coordinator = test_coordinator().await;

        let split_msg = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000,
            }),
            Instant::now(),
        );
        let pending_id = split_msg.id;
        *coordinator.hamlib_pending_split.lock().unwrap() = Some(split_msg);

        // "Last applied" reflects an OLDER id than the pending command
        // itself -- i.e. nothing newer has superseded it.
        let last_applied_split_id =
            Arc::new(std::sync::Mutex::new(Some(pending_id.saturating_sub(1))));

        let (sender, receiver) = crossbeam_channel::bounded::<ComponentMessage>(4);
        deliver_pending_hamlib_state(
            &coordinator.hamlib_pending_frequency,
            &coordinator.hamlib_pending_split,
            &no_frequency_supersession(),
            &last_applied_split_id,
            &sender,
            &not_in_flight(),
            &no_frequency_producer_mark(),
            &no_producer_mark(),
        );

        assert!(
            coordinator.hamlib_pending_split.lock().unwrap().is_none(),
            "a non-superseded pending command must still be delivered (slot drained)"
        );
        assert!(
            receiver.try_recv().is_ok(),
            "a non-superseded pending command must land on the channel"
        );
    }

    /// PAN-35 regression guard: `hamlib_pending_frequency` and
    /// `last_applied_frequency_id` are keyed by VFO -- a pending VFO-B
    /// command must survive a NEWER VFO-A command already having been
    /// applied (changing A does not supersede B). Before this fix, a
    /// single shared slot/tracker conflated the two VFOs, so this exact
    /// scenario would have wrongly dropped the still-correct pending
    /// VFO-B command as "superseded".
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn deliver_pending_hamlib_state_does_not_let_vfo_a_supersede_a_pending_vfo_b_command() {
        let coordinator = test_coordinator().await;

        // A pending VFO-B command, carried over from a prior failed replay.
        let pending_b = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetFrequency {
                vfo: 1,
                frequency: 7_074_000,
            }),
            Instant::now(),
        );
        let pending_b_id = pending_b.id;
        coordinator
            .hamlib_pending_frequency
            .lock()
            .unwrap()
            .insert(pancetta_hamlib::Vfo::B, pending_b);

        // A NEWER VFO-A command has already been applied through the
        // normal path -- recorded ONLY under VFO-A's own key, exactly what
        // the real message loop's SetFrequency arm does.
        let applied_a = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetFrequency {
                vfo: 0,
                frequency: 14_074_000,
            }),
            Instant::now(),
        );
        assert!(
            applied_a.id > pending_b_id,
            "test setup invariant: the applied VFO-A command must be genuinely newer"
        );
        let last_applied_frequency_id = Arc::new(std::sync::Mutex::new(HashMap::from([(
            pancetta_hamlib::Vfo::A,
            applied_a.id,
        )])));

        let (sender, receiver) = crossbeam_channel::bounded::<ComponentMessage>(4);
        deliver_pending_hamlib_state(
            &coordinator.hamlib_pending_frequency,
            &coordinator.hamlib_pending_split,
            &last_applied_frequency_id,
            &no_supersession(),
            &sender,
            &not_in_flight(),
            &no_frequency_producer_mark(),
            &no_producer_mark(),
        );

        assert!(
            coordinator
                .hamlib_pending_frequency
                .lock()
                .unwrap()
                .is_empty(),
            "the pending VFO-B command must still be delivered (slot drained) -- a newer \
             VFO-A command must never supersede it"
        );
        let delivered = receiver
            .try_recv()
            .expect("the pending VFO-B command must have been delivered onto the channel");
        assert_eq!(
            delivered.id, pending_b_id,
            "the delivered message must be the SAME pending VFO-B command"
        );
    }

    /// The mirror image of the test above: a pending VFO-A command must
    /// survive a newer VFO-B command already having been applied.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn deliver_pending_hamlib_state_does_not_let_vfo_b_supersede_a_pending_vfo_a_command() {
        let coordinator = test_coordinator().await;

        let pending_a = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetFrequency {
                vfo: 0,
                frequency: 14_074_000,
            }),
            Instant::now(),
        );
        let pending_a_id = pending_a.id;
        coordinator
            .hamlib_pending_frequency
            .lock()
            .unwrap()
            .insert(pancetta_hamlib::Vfo::A, pending_a);

        let applied_b = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetFrequency {
                vfo: 1,
                frequency: 7_074_000,
            }),
            Instant::now(),
        );
        assert!(
            applied_b.id > pending_a_id,
            "test setup invariant: the applied VFO-B command must be genuinely newer"
        );
        let last_applied_frequency_id = Arc::new(std::sync::Mutex::new(HashMap::from([(
            pancetta_hamlib::Vfo::B,
            applied_b.id,
        )])));

        let (sender, receiver) = crossbeam_channel::bounded::<ComponentMessage>(4);
        deliver_pending_hamlib_state(
            &coordinator.hamlib_pending_frequency,
            &coordinator.hamlib_pending_split,
            &last_applied_frequency_id,
            &no_supersession(),
            &sender,
            &not_in_flight(),
            &no_frequency_producer_mark(),
            &no_producer_mark(),
        );

        assert!(
            coordinator
                .hamlib_pending_frequency
                .lock()
                .unwrap()
                .is_empty(),
            "the pending VFO-A command must still be delivered (slot drained) -- a newer \
             VFO-B command must never supersede it"
        );
        let delivered = receiver
            .try_recv()
            .expect("the pending VFO-A command must have been delivered onto the channel");
        assert_eq!(
            delivered.id, pending_a_id,
            "the delivered message must be the SAME pending VFO-A command"
        );
    }

    /// The flip side of the two tests above: genuine SAME-VFO supersession
    /// must still work -- the VFO-keying fix must not accidentally disable
    /// the original round-11/12 supersession protection. A pending VFO-A
    /// command must still be dropped once a NEWER command for that SAME
    /// VFO-A has already been applied.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn deliver_pending_hamlib_state_still_drops_a_stale_same_vfo_frequency_command() {
        let coordinator = test_coordinator().await;

        let stale_a = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetFrequency {
                vfo: 0,
                frequency: 14_078_000, // stale, wrong frequency
            }),
            Instant::now(),
        );
        coordinator
            .hamlib_pending_frequency
            .lock()
            .unwrap()
            .insert(pancetta_hamlib::Vfo::A, stale_a);

        let newer_a = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetFrequency {
                vfo: 0,
                frequency: 14_074_000, // the correct, current frequency
            }),
            Instant::now(),
        );
        let last_applied_frequency_id = Arc::new(std::sync::Mutex::new(HashMap::from([(
            pancetta_hamlib::Vfo::A,
            newer_a.id,
        )])));

        let (sender, receiver) = crossbeam_channel::bounded::<ComponentMessage>(4);
        deliver_pending_hamlib_state(
            &coordinator.hamlib_pending_frequency,
            &coordinator.hamlib_pending_split,
            &last_applied_frequency_id,
            &no_supersession(),
            &sender,
            &not_in_flight(),
            &no_frequency_producer_mark(),
            &no_producer_mark(),
        );

        assert!(
            coordinator
                .hamlib_pending_frequency
                .lock()
                .unwrap()
                .is_empty(),
            "the stale same-VFO pending command must be dropped (cleared), not left queued"
        );
        assert!(
            receiver.try_recv().is_err(),
            "the stale same-VFO pending command must never be delivered onto the channel once \
             a newer command for that SAME VFO has already been applied"
        );
    }

    /// PAN-19 round-12 review (Codex P1): "discard pending state superseded
    /// by queued commands". Round-11's `is_superseded` check only ran at
    /// `deliver_pending_hamlib_state`'s retry-SEND time -- it couldn't see a
    /// newer command that had already been sent (normal path) but not yet
    /// CONSUMED by the message loop, so a stale pending message that got
    /// queued BEHIND an already-queued newer one would still be applied
    /// after it once both reached the loop.
    ///
    /// This drives `is_superseded` + `record_applied` -- the pre-I/O gate
    /// and post-I/O-success recorder the message loop's own SetFrequency/
    /// SetSplit arms now call around every rig I/O attempt (round 15 split
    /// what used to be a single `should_apply_and_record` call into these
    /// two steps, straddling the I/O call itself) -- through exactly that
    /// race, at the level the loop actually sees it: two SetSplit messages
    /// consumed in FIFO order, newer first (mirroring "already queued
    /// ahead of the stale replay"), stale second. The newer one must pass
    /// the gate and (once its simulated I/O succeeds) advance the tracker;
    /// the stale one, consumed after, must be rejected by the gate
    /// regardless of whatever the tracker looked like at the moment the
    /// stale one was ENQUEUED -- only the tracker's state at CONSUME time
    /// matters.
    #[test]
    fn stale_message_consumed_after_a_newer_one_is_rejected_by_the_gate() {
        let stale_split = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000, // stale, wrong TX frequency
            }),
            Instant::now(),
        );
        // Constructed after `stale_split`, so its id is strictly greater
        // (globally monotonic, see `generate_message_id`).
        let newer_split = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_074_000, // the CORRECT, current TX frequency
            }),
            Instant::now(),
        );
        assert!(
            newer_split.id > stale_split.id,
            "test setup invariant: newer_split must have a strictly larger id"
        );

        let last_applied_split_id: Arc<std::sync::Mutex<Option<u64>>> =
            Arc::new(std::sync::Mutex::new(None));

        // The loop consumes the ALREADY-QUEUED newer command first (the
        // race in the finding: it was sent, and queued, before the stale
        // pending retry ever ran) -- passes the pre-I/O gate, its
        // (simulated, successful) I/O completes, so it's recorded applied.
        assert!(
            !is_superseded(&newer_split, &last_applied_split_id),
            "the newer command must pass the gate"
        );
        record_applied(&newer_split, &last_applied_split_id);
        assert_eq!(
            *last_applied_split_id.lock().unwrap(),
            Some(newer_split.id),
            "the tracker must advance to the newer command's id"
        );

        // The loop then consumes the stale pending replay, which landed in
        // the channel BEHIND the newer one. Even though the send-time
        // `is_superseded` check in `deliver_pending_hamlib_state` may have
        // seen a stale (or empty) tracker before the newer command was
        // applied, the consumption-time gate must still reject it here,
        // using the tracker's CURRENT state -- so its I/O is never even
        // attempted, and the tracker is never touched for it.
        assert!(
            is_superseded(&stale_split, &last_applied_split_id),
            "a stale command consumed after a newer one must be rejected, not applied"
        );
        assert_eq!(
            *last_applied_split_id.lock().unwrap(),
            Some(newer_split.id),
            "rejecting the stale command must not move the tracker backward"
        );
    }

    /// PAN-19 round-15 review (Codex P1) regression guard: "keep TX muted
    /// until restored rig state is applied". The exact scenario the
    /// finding describes -- a `SetSplit` is consumed by the message loop
    /// (passes the pre-I/O `is_superseded` gate), but the underlying
    /// `set_split`/`set_split_freq` CAT call FAILS (e.g. CAT still
    /// recovering mid-restart). `finish_rig_command` must NOT record the
    /// message as applied, and must (re)populate the pending slot -- not
    /// silently swallow the failure with the tracker advanced and the slot
    /// left clear, which would let `tx_hard_mute_reason`'s pending-state
    /// check (round 14) wrongly report "clear, PTT is safe".
    #[test]
    fn finish_rig_command_repopulates_the_pending_slot_on_io_failure() {
        let split_msg = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_074_000,
            }),
            Instant::now(),
        );
        let last_applied_split_id: Arc<std::sync::Mutex<Option<u64>>> =
            Arc::new(std::sync::Mutex::new(None));
        let pending_split: Arc<std::sync::Mutex<Option<ComponentMessage>>> =
            Arc::new(std::sync::Mutex::new(None));

        // Simulate the CAT command FAILING (`io_ok = false`) -- e.g.
        // `set_split_freq`/`set_split` returned `Err` while the rig is
        // still recovering.
        finish_rig_command(false, &split_msg, &last_applied_split_id, &pending_split);

        assert_eq!(
            *last_applied_split_id.lock().unwrap(),
            None,
            "a failed CAT command must NOT be recorded as applied"
        );
        assert_eq!(
            pending_split.lock().unwrap().as_ref().map(|m| m.id),
            Some(split_msg.id),
            "a failed CAT command must (re)populate the pending slot so the round-10 retry \
             gets another real attempt, and so tx_hard_mute_reason's pending-state check keeps \
             PTT refused"
        );

        // Directly exercise the same PTT gate `tx_hard_mute_reason` (and
        // `tui_relay`'s `ptt_on_refusal`) route through, proving the
        // consequence the finding cares about: PTT actually stays refused
        // as a result of this pending slot being populated, not just that
        // the slot happens to be non-empty in isolation.
        let tx_policy = Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxPolicy::Full.as_u8(),
        ));
        let tx_restart_inhibit = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let hamlib_loop_ready = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let no_pending_frequency: Arc<
            std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, ComponentMessage>>,
        > = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let not_in_flight = Arc::new(std::sync::atomic::AtomicU32::new(0));
        assert!(
            super::super::tx::tx_hard_mute_reason(
                &tx_policy,
                &tx_restart_inhibit,
                &hamlib_loop_ready,
                &no_pending_frequency,
                &pending_split,
                &not_in_flight,
            )
            .is_some(),
            "PTT must stay refused while the pending slot a failed CAT command repopulated is \
             still unresolved"
        );
    }

    /// The flip side: a successful CAT command records the message as
    /// applied and does NOT touch the pending slot (it was already cleared
    /// by `deliver_pending_hamlib_state` at delivery time, if this was a
    /// redelivery, or never populated at all for a fresh send).
    #[test]
    fn finish_rig_command_records_applied_on_io_success() {
        let split_msg = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_074_000,
            }),
            Instant::now(),
        );
        let last_applied_split_id: Arc<std::sync::Mutex<Option<u64>>> =
            Arc::new(std::sync::Mutex::new(None));
        let pending_split: Arc<std::sync::Mutex<Option<ComponentMessage>>> =
            Arc::new(std::sync::Mutex::new(None));

        finish_rig_command(true, &split_msg, &last_applied_split_id, &pending_split);

        assert_eq!(
            *last_applied_split_id.lock().unwrap(),
            Some(split_msg.id),
            "a successful CAT command must be recorded as applied"
        );
        assert!(
            pending_split.lock().unwrap().is_none(),
            "a successful CAT command must not populate the pending slot"
        );
    }

    /// PAN-19 round-16 review (Codex P1) regression guard: "preserve the
    /// newest failed rig command". The exact scenario the finding
    /// describes: a stale command has been appended behind a newer normal
    /// command (the round-12 race), and BOTH of their CAT calls fail. The
    /// newer command's failure is processed FIRST (occupies the slot);
    /// the stale command's failure is processed SECOND. Before this fix,
    /// the stale (older) failure would unconditionally overwrite the slot,
    /// losing the newer command's restored state -- a later retry would
    /// then re-apply the OLD frequency/split, clear the slot, and permit
    /// TX while the newer desired state was never actually restored.
    #[test]
    fn finish_rig_command_never_lets_an_older_failure_clobber_a_newer_ones_restored_state() {
        let stale_split = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000, // stale, wrong TX frequency
            }),
            Instant::now(),
        );
        // Constructed after `stale_split`, so its id is strictly greater.
        let newer_split = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_074_000, // the CORRECT, current desired TX frequency
            }),
            Instant::now(),
        );
        assert!(newer_split.id > stale_split.id);

        let last_applied_split_id: Arc<std::sync::Mutex<Option<u64>>> =
            Arc::new(std::sync::Mutex::new(None));
        let pending_split: Arc<std::sync::Mutex<Option<ComponentMessage>>> =
            Arc::new(std::sync::Mutex::new(None));

        // The newer command's CAT call fails FIRST -- restores it into
        // the (currently empty) slot.
        finish_rig_command(false, &newer_split, &last_applied_split_id, &pending_split);
        assert_eq!(
            pending_split.lock().unwrap().as_ref().map(|m| m.id),
            Some(newer_split.id),
            "test setup: the newer command's failure must occupy the empty slot"
        );

        // The stale command's CAT call ALSO fails, processed SECOND --
        // must NOT clobber the newer command's already-restored state.
        finish_rig_command(false, &stale_split, &last_applied_split_id, &pending_split);

        assert_eq!(
            pending_split.lock().unwrap().as_ref().map(|m| m.id),
            Some(newer_split.id),
            "a stale (older) command's failure must never overwrite a newer command's already-\
             restored pending state -- doing so would lose the newer desired frequency/split \
             and let a later retry re-apply the old one instead"
        );
        let retained = pending_split.lock().unwrap();
        assert!(
            matches!(
                retained.as_ref().map(|m| &m.message_type),
                Some(MessageType::RigControl(
                    crate::message_bus::RigControlMessage::SetSplit {
                        enabled: true,
                        tx_frequency: 14_074_000,
                    }
                ))
            ),
            "the retained pending command must still carry the NEWER, correct TX frequency"
        );
    }

    /// The flip side: a failure with a NEWER id than what's already
    /// retained must still replace it -- the fix must not become so
    /// conservative that it stops ever updating the slot.
    #[test]
    fn finish_rig_command_still_replaces_an_older_retained_failure_with_a_newer_one() {
        let older_split = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000,
            }),
            Instant::now(),
        );
        let newer_split = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_074_000,
            }),
            Instant::now(),
        );
        assert!(newer_split.id > older_split.id);

        let last_applied_split_id: Arc<std::sync::Mutex<Option<u64>>> =
            Arc::new(std::sync::Mutex::new(None));
        let pending_split: Arc<std::sync::Mutex<Option<ComponentMessage>>> =
            Arc::new(std::sync::Mutex::new(None));

        finish_rig_command(false, &older_split, &last_applied_split_id, &pending_split);
        finish_rig_command(false, &newer_split, &last_applied_split_id, &pending_split);

        assert_eq!(
            pending_split.lock().unwrap().as_ref().map(|m| m.id),
            Some(newer_split.id),
            "a newer failure must still replace an older retained one -- ordering must not \
             become a one-way ratchet that stops updating the slot"
        );
    }

    /// End-to-end shape of the fix: a `SetSplit` that fails to replay
    /// during one generation's teardown is later delivered once the NEXT
    /// generation's channel is available -- not lost in between.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn a_stuck_set_split_survives_to_be_delivered_to_the_next_generation() {
        let mut coordinator = test_coordinator().await;

        // Simulate a failed teardown replay: the OLD generation's channel
        // stays full for the whole bounded retry.
        let (old_sender, _old_receiver) = crossbeam_channel::bounded::<ComponentMessage>(1);
        old_sender
            .try_send(set_freq_msg())
            .expect("pre-fill the one slot");
        let split_msg = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000,
            }),
            Instant::now(),
        );
        coordinator.replay_or_fallback(&old_sender, split_msg).await;
        assert!(
            coordinator.hamlib_pending_split.lock().unwrap().is_some(),
            "test setup: the SetSplit must have been queued, not delivered"
        );

        // The NEXT generation's (fresh, empty) channel -- delivery must
        // land here once it's confirmed ready.
        let (new_sender, new_receiver) = crossbeam_channel::bounded::<ComponentMessage>(4);
        deliver_pending_hamlib_state(
            &coordinator.hamlib_pending_frequency,
            &coordinator.hamlib_pending_split,
            &no_frequency_supersession(),
            &no_supersession(),
            &new_sender,
            &not_in_flight(),
            &no_frequency_producer_mark(),
            &no_producer_mark(),
        );

        let delivered = new_receiver
            .try_recv()
            .expect("the queued SetSplit must have been delivered to the new generation");
        assert!(
            matches!(
                delivered.message_type,
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                    enabled: true,
                    tx_frequency: 14_078_000
                })
            ),
            "the delivered message must be the SAME SetSplit that failed to replay earlier"
        );
        assert!(
            coordinator.hamlib_pending_split.lock().unwrap().is_none(),
            "the pending slot must be cleared once delivered"
        );
    }

    /// PAN-19 round-10 review (Codex P1) regression guard: a pending
    /// command must get MORE THAN ONE delivery attempt per generation --
    /// not be stranded until the next full Hamlib restart. `deliver_pending_hamlib_state`
    /// itself already retries correctly when called again (proven above);
    /// this specifically proves the retry is driven automatically WITHIN
    /// the current generation (the polling task's ~500ms tick, mirrored
    /// here) rather than requiring a brand new `start_hamlib_component`
    /// call.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn a_pending_command_gets_more_than_one_delivery_attempt_within_one_generation() {
        let coordinator = test_coordinator().await;
        let split_msg = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000,
            }),
            Instant::now(),
        );
        *coordinator.hamlib_pending_split.lock().unwrap() = Some(split_msg);

        // The channel is full for the FIRST attempt (mirrors the exact
        // scenario the finding describes: "a producer fills the bounded
        // channel at this instant").
        let (sender, receiver) = crossbeam_channel::bounded::<ComponentMessage>(1);
        let blocker = set_freq_msg();
        sender.try_send(blocker).expect("pre-fill the one slot");

        // Attempt #1 (mirrors the ORIGINAL call in start_hamlib_component):
        // fails, channel still full.
        deliver_pending_hamlib_state(
            &coordinator.hamlib_pending_frequency,
            &coordinator.hamlib_pending_split,
            &no_frequency_supersession(),
            &no_supersession(),
            &sender,
            &not_in_flight(),
            &no_frequency_producer_mark(),
            &no_producer_mark(),
        );
        assert!(
            coordinator.hamlib_pending_split.lock().unwrap().is_some(),
            "test setup: the first attempt must fail (channel full)"
        );

        // The channel drains on its own between attempts (the new
        // generation's message loop consuming the blocker) -- NO new
        // start_hamlib_component / restart call happens here.
        receiver.try_recv().expect("drain the blocker");

        // Attempt #2 (mirrors the polling task's next ~500ms tick, within
        // the SAME generation): must now succeed.
        deliver_pending_hamlib_state(
            &coordinator.hamlib_pending_frequency,
            &coordinator.hamlib_pending_split,
            &no_frequency_supersession(),
            &no_supersession(),
            &sender,
            &not_in_flight(),
            &no_frequency_producer_mark(),
            &no_producer_mark(),
        );

        assert!(
            coordinator.hamlib_pending_split.lock().unwrap().is_none(),
            "a pending command must not be stranded after only one failed attempt -- it must \
             be retried and delivered within the SAME generation"
        );
        let delivered = receiver
            .try_recv()
            .expect("the SetSplit must have been delivered on the second, later attempt");
        assert!(matches!(
            delivered.message_type,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000
            })
        ));
    }

    /// PAN-19 round-10 review (Codex P1) regression guard, end to end: the
    /// REAL Hamlib polling task (spawned by the REAL `start_hamlib_component`,
    /// not a mirrored/manual double-call) must automatically retry and
    /// eventually deliver a pending command that failed its FIRST attempt
    /// -- without any second `start_hamlib_component`/restart call.
    ///
    /// Pre-fills the (small-capacity) Hamlib channel with a blocker BEFORE
    /// calling `start_hamlib_component`, so its OWN internal
    /// `deliver_pending_hamlib_state` call (the only attempt that existed
    /// before this fix) finds the channel full and re-queues. Then drains
    /// the blocker via a receiver held by the test -- simulating "the
    /// channel likely has room again a moment later" from the finding --
    /// and polls for the real polling task's own ~500ms tick to pick it up
    /// on its own.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn poll_loop_automatically_retries_a_pending_command_within_the_same_generation() {
        let mut coordinator = test_coordinator().await;
        // Small capacity -- trivial to keep full for the first attempt.
        coordinator.message_bus = crate::message_bus::MessageBus::new(1).unwrap();

        let (sender, receiver) = coordinator
            .message_bus
            .get_or_create_channel(ComponentId::Hamlib)
            .await
            .unwrap();
        sender
            .try_send(set_freq_msg())
            .expect("pre-fill the one slot BEFORE start_hamlib_component runs");

        // Simulate a prior generation's failed teardown replay: a SetSplit
        // already queued before this (new) generation even starts.
        let split_msg = ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000,
            }),
            Instant::now(),
        );
        *coordinator.hamlib_pending_split.lock().unwrap() = Some(split_msg);

        coordinator
            .start_hamlib_component()
            .await
            .expect("mock-rig hamlib start should succeed");

        // The FIRST attempt (inside start_hamlib_component itself, before
        // it even returned) must have found the channel full and left the
        // command queued.
        assert!(
            coordinator.hamlib_pending_split.lock().unwrap().is_some(),
            "test setup: the first delivery attempt (inside start_hamlib_component) must have \
             failed -- channel full"
        );

        // Drain the blocker -- the channel has room again, exactly the
        // scenario the finding describes. NO new start_hamlib_component /
        // restart call happens from here on -- only the REAL, already
        // -running polling task for this SAME generation.
        receiver.try_recv().expect("drain the blocker");

        let mut delivered = None;
        for _ in 0..40 {
            if let Ok(msg) = receiver.try_recv() {
                delivered = Some(msg);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let delivered = delivered.expect(
            "the REAL polling task must automatically retry and deliver the pending SetSplit \
             within the current generation, without requiring another \
             start_hamlib_component/restart call",
        );
        assert!(matches!(
            delivered.message_type,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000
            })
        ));
        assert!(
            coordinator.hamlib_pending_split.lock().unwrap().is_none(),
            "the pending slot must be cleared once the polling task delivers it"
        );

        coordinator
            .shutdown_signal
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod device_path_tests {
    use super::*;

    #[test]
    fn accepts_real_serial_and_network_shapes() {
        let ok = [
            // Linux serial
            "/dev/ttyUSB0",
            "/dev/ttyUSB10",
            "/dev/ttyACM0",
            "/dev/ttyS0",
            // macOS (dev machine: /dev/cu.usbserial-*)
            "/dev/cu.usbserial-1410",
            "/dev/tty.usbserial-1410",
            // Windows
            "COM3",
            "COM12",
            // network rig
            "127.0.0.1:4532",
            "192.168.1.50:4532",
            "myrig.local:65535",
            "myrig.local:1",
        ];
        for p in ok {
            assert!(
                super::super::ApplicationCoordinator::device_path_looks_safe(p),
                "expected {p:?} to be accepted"
            );
        }
    }

    #[test]
    fn rejects_bogus_paths_and_bad_ports() {
        let bad = [
            // bare/loose serial roots that the old starts_with("/dev/tty") let through
            "/dev/tty",
            "/dev/ttyZZZ",
            "/dev/null",
            "/dev/cu.",
            "/dev/tty.",
            "/etc/passwd",
            "COM",
            "COMx",
            // network: bad / out-of-range / missing ports
            "myrig.local:0",
            "myrig.local:70000",
            "myrig.local:abc",
            "myrig.local:",
            ":4532",
            // arbitrary
            "rm -rf /",
            "hello",
        ];
        for p in bad {
            assert!(
                !super::super::ApplicationCoordinator::device_path_looks_safe(p),
                "expected {p:?} to be rejected"
            );
        }
    }
}

/// I-1a fix (PAN-59 final review): `model_recognized` is the gate
/// `tui_relay.rs`'s `TuiCommand::SelectRig` handler calls before
/// persisting/reconnecting anything -- it must agree exactly with
/// `hamlib_model_id`'s recognized-model table (the one
/// `start_hamlib_component` itself uses), since the whole point is to
/// catch a bad model BEFORE the live-switch attempt.
#[cfg(all(test, feature = "pancetta-hamlib"))]
mod model_recognized_tests {
    use super::*;

    #[test]
    fn recognizes_every_model_in_the_hamlib_id_table() {
        for model in [
            "FTdx10", "ftdx10", "FT-DX10", "FTdx101D", "FT991", "ft991a", "FT710", "FT891",
            "FT857", "FT817", "IC-7300", "ic7610", "IC7851", "IC705", "IC9700", "TS890", "ts590sg",
        ] {
            assert!(
                model_recognized(model),
                "expected {model:?} to be recognized (it's in hamlib_model_id's table)"
            );
        }
    }

    #[test]
    fn rejects_an_unrecognized_model() {
        assert!(!model_recognized("totally-bogus-unrecognized-model"));
        assert!(!model_recognized(""));
    }
}

#[cfg(all(test, feature = "pancetta-hamlib"))]
mod pan_59_reconnect_tests {
    //! PAN-59: `handle_hamlib_reconnect_request` is the coordinator-side
    //! handler for a live rig-config switch. Two things must hold: it must
    //! refuse to tear down Hamlib while PTT is active (that would yank
    //! CAT/PTT control out from under an active transmission), and it must
    //! otherwise reconnect successfully via the same teardown/restart pair
    //! the crash-restart path already uses.
    use super::*;
    use pancetta_config::Config;
    use std::sync::atomic::AtomicBool;

    async fn test_coordinator() -> super::super::ApplicationCoordinator {
        let config = Config::default();
        let shutdown = Arc::new(AtomicBool::new(false));
        super::super::ApplicationCoordinator::new(
            config,
            None,
            true,  // no_audio
            true,  // headless
            false, // metrics
            9090,
            None, // no WAV
            None, // no replay
            None, // no test-tx
            1500.0,
            shutdown,
            Vec::new(), // no config warnings
        )
        .await
        .expect("coordinator creation should succeed")
    }

    #[tokio::test]
    async fn refuses_reconnect_while_ptt_is_active() {
        let mut coordinator = test_coordinator().await;
        coordinator.ptt_active.store(true, Ordering::Release);
        // M10 (PAN-59 final review): capture the generation counter BEFORE
        // the call -- both `teardown_hamlib()` and `start_hamlib_component()`
        // bump it (see `start_hamlib_component`'s `this_generation` fetch_add
        // near its top), so an unchanged value after a refused call is
        // direct proof neither ever ran, not just that the call returned an
        // error for some other reason.
        let generation_before = coordinator.hamlib_generation.load(Ordering::Acquire);

        let (tx, rx) = tokio::sync::oneshot::channel();
        coordinator
            .handle_hamlib_reconnect_request(HamlibReconnectRequest { respond: tx })
            .await;

        let result = rx.await.expect("handler must always respond");
        assert!(
            result.is_err(),
            "must refuse a live rig reconnect while PTT is active -- tearing down Hamlib \
             mid-key-down would yank CAT/PTT control out from under an active transmission"
        );
        assert_eq!(
            coordinator.hamlib_generation.load(Ordering::Acquire),
            generation_before,
            "a refused reconnect must never bump hamlib_generation -- proving \
             teardown_hamlib()/start_hamlib_component() genuinely never ran, not just that \
             the call returned an error"
        );
    }

    #[tokio::test]
    async fn reconnects_successfully_when_ptt_is_idle() {
        let mut coordinator = test_coordinator().await;
        assert!(!coordinator.ptt_active.load(Ordering::Acquire));

        let (tx, rx) = tokio::sync::oneshot::channel();
        coordinator
            .handle_hamlib_reconnect_request(HamlibReconnectRequest { respond: tx })
            .await;

        let result = rx.await.expect("handler must always respond");
        assert!(
            result.is_ok(),
            "must succeed reconnecting via the mock rig path when PTT is idle: {:?}",
            result.err()
        );
    }

    /// I-1 fix (PAN-59 final review), failure class 2 (the "worse" one
    /// flagged by the review): before the fix, `start_hamlib_component`'s
    /// `device_path_looks_safe` gate returned `Ok(())` EARLY -- before ever
    /// spawning the message loop or restoring `hamlib_command_loop_ready`
    /// to `true` (it's forced `false` at the very top of every call) --
    /// so a bad-port config reported success to the operator while leaving
    /// TX permanently hard-muted with no Hamlib task even registered to
    /// notice/restart it. `handle_hamlib_reconnect_request` must now
    /// convert that into a real `Err` instead of relaying the false `Ok`.
    #[tokio::test]
    async fn refuses_reconnect_when_configured_port_fails_the_safety_check() {
        let mut coordinator = test_coordinator().await;
        {
            let mut config = coordinator.config.write().await;
            config.rig.interface.enabled = true;
            // Fails `device_path_looks_safe`: not a recognized
            // serial/network device shape.
            config.rig.interface.port = "/dev/not-a-real-serial-device".to_string();
        }

        let (tx, rx) = tokio::sync::oneshot::channel();
        coordinator
            .handle_hamlib_reconnect_request(HamlibReconnectRequest { respond: tx })
            .await;

        let result = rx.await.expect("handler must always respond");
        assert!(
            result.is_err(),
            "a rig-enabled reconnect with a port that fails device_path_looks_safe must \
             report failure, not silently succeed with no real CAT/PTT control: {:?}",
            result
        );
        assert!(
            !coordinator
                .hamlib_command_loop_ready
                .load(Ordering::Acquire),
            "the early-return path never restores hamlib_command_loop_ready to true -- TX \
             must stay muted after this refusal"
        );
    }

    /// I-1 fix (PAN-59 final review), failure class 1: an unrecognized
    /// `rig.model` string makes `hamlib_model_id` return `None`.
    /// `start_hamlib_component` logs a warning and reports a rig error, but
    /// (before this fix) fell through to build a `RigctldClient` pointing
    /// at a port nothing is listening on and returned `Ok(())` anyway.
    /// Uses a genuinely free TCP port (bound then immediately released) for
    /// `RIGCTLD_PORT`/`RIGCTLD_HOST` so the resulting real connect attempt
    /// fails fast and deterministically, instead of risking collision with
    /// the default 4532 (which a developer's own rig session could have
    /// bound) or a slow connect() against an unrelated live service.
    #[tokio::test]
    async fn refuses_reconnect_when_rig_model_is_unrecognized() {
        // SAFETY: this test mutates process-wide env vars (RIGCTLD_PORT/
        // RIGCTLD_HOST). No other test in this module reads them (the mock
        // -rig path used elsewhere never reaches the code that does), and
        // `cargo test` runs each `#[tokio::test]` on its own task, but env
        // vars are still process-global -- restore them unconditionally
        // below so another test elsewhere in the binary is never left
        // seeing a stale value.
        let free_port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("failed to bind an ephemeral port for the test");
            listener
                .local_addr()
                .expect("local_addr should succeed")
                .port()
            // listener drops here, releasing the port back to the OS.
        };
        let prev_port = std::env::var("RIGCTLD_PORT").ok();
        let prev_host = std::env::var("RIGCTLD_HOST").ok();
        std::env::set_var("RIGCTLD_PORT", free_port.to_string());
        std::env::set_var("RIGCTLD_HOST", "127.0.0.1");

        let mut coordinator = test_coordinator().await;
        {
            let mut config = coordinator.config.write().await;
            config.rig.interface.enabled = true;
            config.rig.interface.port = "/dev/ttyUSB0".to_string();
            config.rig.model = "totally-bogus-unrecognized-model".to_string();
        }

        let (tx, rx) = tokio::sync::oneshot::channel();
        coordinator
            .handle_hamlib_reconnect_request(HamlibReconnectRequest { respond: tx })
            .await;
        let result = rx.await.expect("handler must always respond");

        match prev_port {
            Some(v) => std::env::set_var("RIGCTLD_PORT", v),
            None => std::env::remove_var("RIGCTLD_PORT"),
        }
        match prev_host {
            Some(v) => std::env::set_var("RIGCTLD_HOST", v),
            None => std::env::remove_var("RIGCTLD_HOST"),
        }

        assert!(
            result.is_err(),
            "a rig-enabled reconnect with an unrecognized model must report failure, not \
             silently succeed while connected to nothing: {:?}",
            result
        );
    }

    /// I5 fix (PAN-59 review): the two tests above call
    /// `handle_hamlib_reconnect_request` against a brand-new coordinator
    /// that has never started Hamlib, so `named_task_handles` has no PRIOR
    /// live entry and `rigctld_process` has no prior managed child --
    /// neither the C2 (stale task-handle) nor the C1 (stale managed
    /// rigctld) bug can manifest without a real, still-alive PRIOR
    /// generation. This test creates one for real (a genuine first
    /// `start_hamlib_component()` call, still via the mock-rig path so it
    /// stays fast/hermetic -- exercising the real rigctld *spawn* path
    /// would need the `rigctld` binary installed and a real serial device,
    /// neither available/hermetic here), seeds a real stand-in OS process
    /// into `rigctld_process` (the field is only ever `Some` when pancetta
    /// itself spawned a managed rigctld -- this stands in for that spawn
    /// without needing the real binary, exercising exactly the code this
    /// fix added: killing whatever is currently tracked there), then
    /// reconnects and asserts both fixes actually fired.
    #[tokio::test]
    async fn reconnect_replaces_stale_task_handle_and_kills_managed_rigctld() {
        fn live_hamlib_entries(c: &super::super::ApplicationCoordinator) -> usize {
            c.named_task_handles
                .iter()
                .filter(|(id, h)| *id == ComponentId::Hamlib && !h.is_finished())
                .count()
        }
        fn total_hamlib_entries(c: &super::super::ApplicationCoordinator) -> usize {
            c.named_task_handles
                .iter()
                .filter(|(id, _)| *id == ComponentId::Hamlib)
                .count()
        }

        let mut coordinator = test_coordinator().await;
        coordinator
            .start_hamlib_component()
            .await
            .expect("initial Hamlib start should succeed via the mock rig path");
        assert_eq!(
            live_hamlib_entries(&coordinator),
            1,
            "setup: exactly one live Hamlib task handle expected after the first start"
        );

        // Seed a stand-in "previously spawned managed rigctld": a real,
        // long-lived OS process, exactly the shape `rigctld_process` holds
        // when `start_hamlib_component`'s real spawn path populated it.
        let stand_in = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("failed to spawn stand-in process for the test");
        let stand_in_pid = stand_in.id();
        coordinator.rigctld_process = Some(stand_in);

        let (tx, rx) = tokio::sync::oneshot::channel();
        coordinator
            .handle_hamlib_reconnect_request(HamlibReconnectRequest { respond: tx })
            .await;
        let result = rx.await.expect("handler must always respond");
        assert!(
            result.is_ok(),
            "reconnect should succeed: {:?}",
            result.err()
        );

        // C2: exactly ONE live Hamlib entry after reconnect, and no stale
        // finished entry left alongside it either -- two entries (one
        // stale-finished, one live) is exactly the state that fed the
        // PAN-59 restart cascade (check_task_handles rediscovers the stale
        // one as a fresh "crash" and tears down/restarts the brand-new
        // generation too).
        assert_eq!(
            live_hamlib_entries(&coordinator),
            1,
            "reconnect must leave exactly one live Hamlib task handle"
        );
        assert_eq!(
            total_hamlib_entries(&coordinator),
            1,
            "reconnect must remove the old generation's handle entirely, not merely leave it \
             alongside the new one"
        );

        // C1: the stand-in must actually have been killed -- proving the
        // reconnect path terminates whatever was tracked in
        // `rigctld_process` rather than leaking it (an un-killed OLD
        // rigctld is exactly what let `already_running` find it and skip
        // spawning a fresh one with the operator's new model/port/baud).
        let still_alive = std::process::Command::new("kill")
            .args(["-0", &stand_in_pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("failed to run `kill -0` to check stand-in liveness")
            .success();
        assert!(
            !still_alive,
            "the old managed rigctld stand-in (PID {stand_in_pid}) must be killed during \
             reconnect, not left running for a fresh RigctldClient to (re)find"
        );
        assert!(
            coordinator.rigctld_process.is_none(),
            "rigctld_process must not still reference the killed stand-in after reconnect \
             (the mock-rig path on the new generation doesn't spawn a replacement)"
        );
    }
}
