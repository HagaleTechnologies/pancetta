// PAN-414.
use super::{odd_injection_delay, ODD_INJECTION_CLOSES_BEFORE_EVEN, ODD_INJECTION_OPENS_AFTER};
use pancetta_core::slot::SlotParity;
use std::time::Duration;

/// Sample one Even-to-Even cycle at 10 ms intervals and check the
/// landing window, maximum delay and immediate return once inside.
#[test]
fn odd_injection_delay_reaches_the_odd_window_from_any_start() {
    let even =
        pancetta_core::time::UtcTimestamp::from_millisecond(1_800_000_000_000).expect("in range");
    assert_eq!(SlotParity::of(even), SlotParity::Even);
    for step_ms in (0..30_000).step_by(10) {
        let now = even + jiff::SignedDuration::from_millis(step_ms);
        let delay = odd_injection_delay(now);
        assert!(
            delay <= Duration::from_secs(20),
            "start +{step_ms} ms waits {delay:?}"
        );
        let lands = now + jiff::SignedDuration::try_from(delay).expect("delay fits");
        assert_eq!(
            SlotParity::of(lands),
            SlotParity::Odd,
            "start +{step_ms} ms lands outside an Odd slot"
        );
        let odd_start = pancetta_core::slot::current_slot_start(lands);
        let next_even = odd_start + jiff::SignedDuration::from_secs(15);
        assert!(
            lands - odd_start >= ODD_INJECTION_OPENS_AFTER,
            "start +{step_ms} ms lands {:?} after the Odd boundary",
            lands - odd_start
        );
        assert!(
            next_even - lands >= ODD_INJECTION_CLOSES_BEFORE_EVEN,
            "start +{step_ms} ms lands {:?} before the Even boundary",
            next_even - lands
        );
        assert_eq!(
            odd_injection_delay(lands),
            Duration::ZERO,
            "start +{step_ms} ms: the landing instant is not inside the window"
        );
    }
}
