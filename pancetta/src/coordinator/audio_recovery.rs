//! Pure, hardware-independent recovery-decision logic for the audio thread.
//!
//! Kept separate from `audio.rs` (which owns the actual cpal/thread wiring)
//! so the backoff and watchdog-edge-detection policies are unit-testable
//! without a real `AudioManager`.

use std::time::Duration;

/// Capped-exponential backoff for repeated `reopen_devices` attempts after a
/// `process_audio` error. The first call after construction (or after a
/// [`reset`](Self::reset)) returns [`Duration::ZERO`] — a StreamError should
/// trigger an immediate reopen attempt, not a wasted sleep, since the common
/// case (a brief USB blip) recovers on the very first try. Only a *failed*
/// attempt should pay the backoff delay before the next one.
pub struct RecoveryBackoff {
    delay: Duration,
    attempts: u32,
}

const INITIAL_DELAY: Duration = Duration::from_millis(250);
const MAX_DELAY: Duration = Duration::from_secs(5);

impl RecoveryBackoff {
    pub fn new() -> Self {
        Self {
            delay: Duration::ZERO,
            attempts: 0,
        }
    }

    /// Returns the delay to sleep before the *next* reopen attempt, then
    /// advances the internal schedule (doubling, capped at [`MAX_DELAY`]) and
    /// increments the attempt counter.
    pub fn next_delay(&mut self) -> Duration {
        let d = self.delay;
        self.delay = if self.delay.is_zero() {
            INITIAL_DELAY
        } else {
            (self.delay * 2).min(MAX_DELAY)
        };
        self.attempts += 1;
        d
    }

    /// Reset after a successful recovery — the next `next_delay()` call will
    /// again return `Duration::ZERO` (immediate retry on the next failure).
    pub fn reset(&mut self) {
        self.delay = Duration::ZERO;
        self.attempts = 0;
    }

    /// How many attempts have been made since construction/the last reset.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }
}

impl Default for RecoveryBackoff {
    fn default() -> Self {
        Self::new()
    }
}

/// Edge-detects "just went stale" for the audio relay task's 2-second
/// no-data timeout. Without this, a persistently wedged device would fire a
/// self-triggered `AudioReopenRequest` every single 2s tick forever — each
/// one a real cpal teardown+rebuild, which is wasteful and can itself cause
/// thrashing on a device that's slow to reopen. `on_timeout` returns `true`
/// only on the first tick of a stale episode; `on_data` (called whenever a
/// fresh sample batch arrives) re-arms it for the next episode.
///
/// A device reopen can succeed at the cpal level (the stream rebuilds
/// cleanly) while the device still never delivers data afterward — still
/// wedged for some other reason. Since `on_data` never fires in that case,
/// a pure one-shot latch would abandon the device forever after a single
/// attempt. To avoid that, the watchdog also re-arms itself after
/// [`RETRY_AFTER_TICKS`] consecutive stale ticks with no intervening
/// `on_data` call — a bounded retry, not a permanent latch.
pub struct StaleWatchdog {
    already_signaled: bool,
    ticks_since_signal: u32,
}

/// After this many consecutive stale ticks with no re-arming data, the
/// watchdog fires again even though the device is still in the same stale
/// episode — a bounded retry so a device that reopens cleanly at the cpal
/// level but never actually resumes delivering data (e.g. still physically
/// wedged) isn't abandoned forever after its first attempt. At the relay
/// task's 2s tick cadence this is roughly a 10s retry cadence while stale.
const RETRY_AFTER_TICKS: u32 = 5;

impl StaleWatchdog {
    pub fn new() -> Self {
        Self {
            already_signaled: false,
            ticks_since_signal: 0,
        }
    }

    /// Call on every stale-timeout tick. Returns `true` on the first tick of
    /// a stale episode, and again every [`RETRY_AFTER_TICKS`] ticks
    /// thereafter while still stale (bounded retry) — `false` on every tick
    /// in between.
    pub fn on_timeout(&mut self) -> bool {
        if !self.already_signaled {
            self.already_signaled = true;
            self.ticks_since_signal = 0;
            true
        } else {
            self.ticks_since_signal += 1;
            if self.ticks_since_signal >= RETRY_AFTER_TICKS {
                self.ticks_since_signal = 0;
                true
            } else {
                false
            }
        }
    }

    /// Call whenever fresh data arrives, re-arming the watchdog for the next
    /// stale episode (and resetting the bounded-retry counter).
    pub fn on_data(&mut self) {
        self.already_signaled = false;
        self.ticks_since_signal = 0;
    }
}

impl Default for StaleWatchdog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_delay_is_immediate() {
        let mut b = RecoveryBackoff::new();
        assert_eq!(b.next_delay(), Duration::ZERO);
        assert_eq!(b.attempts(), 1);
    }

    #[test]
    fn delay_doubles_and_caps() {
        let mut b = RecoveryBackoff::new();
        let seq: Vec<Duration> = (0..8).map(|_| b.next_delay()).collect();
        assert_eq!(
            seq,
            vec![
                Duration::ZERO,
                Duration::from_millis(250),
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(5), // capped, not 8s
                Duration::from_secs(5), // stays capped
            ]
        );
        assert_eq!(b.attempts(), 8);
    }

    #[test]
    fn reset_returns_to_immediate() {
        let mut b = RecoveryBackoff::new();
        b.next_delay();
        b.next_delay();
        assert!(b.attempts() >= 2);
        b.reset();
        assert_eq!(b.next_delay(), Duration::ZERO);
        assert_eq!(b.attempts(), 1);
    }

    #[test]
    fn watchdog_fires_once_per_stale_episode() {
        let mut w = StaleWatchdog::new();
        assert!(w.on_timeout(), "first timeout tick must signal");
        assert!(
            !w.on_timeout(),
            "second consecutive tick must not re-signal"
        );
        assert!(!w.on_timeout(), "third consecutive tick must not re-signal");
    }

    #[test]
    fn watchdog_rearms_after_data() {
        let mut w = StaleWatchdog::new();
        assert!(w.on_timeout());
        w.on_data();
        assert!(
            w.on_timeout(),
            "must signal again after a fresh data arrival"
        );
    }

    #[test]
    fn watchdog_does_not_fire_before_first_timeout() {
        let w = StaleWatchdog::new();
        // Constructing alone must not have signaled anything — only a real
        // on_timeout() call can. (No API to observe this directly without a
        // call, so this test just documents the invariant via on_timeout's
        // own first-call-returns-true behavior above; kept as a named test
        // so a future refactor that changes the default can't silently flip
        // this without a failing test.)
        let mut w = w;
        assert!(w.on_timeout());
    }

    #[test]
    fn watchdog_retries_after_bounded_stale_ticks_without_data() {
        let mut w = StaleWatchdog::new();
        assert!(w.on_timeout(), "first tick signals");
        // Ticks 2..RETRY_AFTER_TICKS (exclusive of the retry tick itself) must not re-signal.
        for _ in 0..(RETRY_AFTER_TICKS - 1) {
            assert!(
                !w.on_timeout(),
                "must not re-signal before the retry threshold"
            );
        }
        // The RETRY_AFTER_TICKS-th consecutive stale tick (with no on_data() call
        // in between) must fire again — this is the bounded-retry guarantee.
        assert!(
            w.on_timeout(),
            "must retry after RETRY_AFTER_TICKS consecutive stale ticks with no data"
        );
    }
}
