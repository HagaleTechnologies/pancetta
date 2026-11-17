//! PAN-295: `OutputReopenPolicy`, driven step by step against an
//! `OutputWatchdog` with fake callback counts and clocks.
use super::*;

fn ms(t0: Instant, millis: u64) -> Instant {
    t0 + Duration::from_millis(millis)
}

/// An `OutputWatchdog` that has just seen progress at `t0 + 10 ms`.
fn alive_watchdog(t0: Instant) -> OutputWatchdog {
    let mut w = OutputWatchdog::new(t0);
    assert_eq!(w.observe(0, t0), OutputEdge::None);
    assert_eq!(w.observe(5, ms(t0, 10)), OutputEdge::None);
    assert!(w.is_alive(ms(t0, 10)));
    w
}

/// One audio-loop observation: watchdog, then policy.
fn step(
    w: &mut OutputWatchdog,
    p: &mut OutputReopenPolicy,
    count: u64,
    now: Instant,
) -> OutputReopenDecision {
    let edge = w.observe(count, now);
    p.decide(edge, w.is_alive(now), w.is_dead_announced(), now)
}

/// The audio thread's reaction to a `Request`: queue it, rebuild
/// (`rebuilt_at`), rebaseline and settle.
fn attempt(w: &mut OutputWatchdog, p: &mut OutputReopenPolicy, sent: Instant, rebuilt_at: Instant) {
    p.on_request_sent(sent);
    w.rebaseline(rebuilt_at);
    p.on_rebuild(rebuilt_at);
    p.on_request_settled(rebuilt_at);
}

/// Stall a healthy output (count 5 since 10 ms) until its dead edge at
/// `start + 2010 ms`, returning the decision at the edge.
fn stall_from(
    w: &mut OutputWatchdog,
    p: &mut OutputReopenPolicy,
    count: u64,
    start: Instant,
) -> OutputReopenDecision {
    assert_eq!(
        step(w, p, count, ms(start, 1999)),
        OutputReopenDecision::None
    );
    step(w, p, count, ms(start, 2010))
}

#[test]
fn pan295_retry_deadline_is_independent_of_dead_edge() {
    let t0 = Instant::now();
    let mut w = alive_watchdog(t0);
    let mut p = OutputReopenPolicy::new();
    assert_eq!(
        step(&mut w, &mut p, 5, ms(t0, 2009)),
        OutputReopenDecision::None
    );
    assert_eq!(
        step(&mut w, &mut p, 5, ms(t0, 2010)),
        OutputReopenDecision::Request
    );
    p.on_request_sent(ms(t0, 2010));
    // Outstanding: repeated observations ask for nothing.
    assert_eq!(
        step(&mut w, &mut p, 5, ms(t0, 2020)),
        OutputReopenDecision::None
    );
    assert!(p.is_outstanding());
    // The rebuild finishes at 3 s; the counter reset is not progress.
    w.rebaseline(ms(t0, 3000));
    p.on_rebuild(ms(t0, 3000));
    p.on_request_settled(ms(t0, 3000));
    assert_eq!(
        step(&mut w, &mut p, 0, ms(t0, 3001)),
        OutputReopenDecision::None
    );
    // The watchdog's latch holds: no second WentDead, ever.
    for t in [5000, 8000, 12_999] {
        assert_eq!(w.observe(0, ms(t0, t)), OutputEdge::None);
        assert_eq!(
            p.decide(OutputEdge::None, false, true, ms(t0, t)),
            OutputReopenDecision::None
        );
    }
    // Ten seconds after completion the retry is due anyway.
    assert_eq!(
        step(&mut w, &mut p, 0, ms(t0, 13_000)),
        OutputReopenDecision::Request
    );
}

#[test]
fn pan295_external_rebuild_postpones_retry_without_resetting_history() {
    let t0 = Instant::now();
    let mut w = alive_watchdog(t0);
    let mut p = OutputReopenPolicy::new();
    assert_eq!(
        stall_from(&mut w, &mut p, 5, t0),
        OutputReopenDecision::Request
    );
    attempt(&mut w, &mut p, ms(t0, 2010), ms(t0, 2100));
    // An operator pick (or input-stale reopen) at 10 s moves the retry.
    w.rebaseline(ms(t0, 10_000));
    p.on_rebuild(ms(t0, 10_000));
    assert_eq!(
        step(&mut w, &mut p, 0, ms(t0, 12_100)),
        OutputReopenDecision::None
    );
    assert_eq!(
        step(&mut w, &mut p, 0, ms(t0, 20_000)),
        OutputReopenDecision::Request
    );
    assert_eq!(
        p.attempts_in_window(),
        1,
        "the external rebuild did not count"
    );
}

#[test]
fn pan295_external_rebuilds_do_not_hide_exhaustion() {
    let t0 = Instant::now();
    let mut w = alive_watchdog(t0);
    let mut p = OutputReopenPolicy::new();
    assert_eq!(
        stall_from(&mut w, &mut p, 5, t0),
        OutputReopenDecision::Request
    );
    attempt(&mut w, &mut p, ms(t0, 2010), ms(t0, 2010));
    for t in [12_010, 22_010] {
        assert_eq!(
            step(&mut w, &mut p, 0, ms(t0, t)),
            OutputReopenDecision::Request
        );
        attempt(&mut w, &mut p, ms(t0, t), ms(t0, t));
    }
    // An input-stale reopen every ~10 s keeps a rebuild settling, yet
    // the spent budget is shown at the policy's own retry time.
    w.rebaseline(ms(t0, 31_000));
    p.on_rebuild(ms(t0, 31_000));
    assert_eq!(
        step(&mut w, &mut p, 0, ms(t0, 32_010)),
        OutputReopenDecision::Refused
    );
    assert!(p.is_exhausted());
    w.rebaseline(ms(t0, 41_000));
    p.on_rebuild(ms(t0, 41_000));
    assert_eq!(
        step(&mut w, &mut p, 0, ms(t0, 41_001)),
        OutputReopenDecision::Refused
    );
    assert!(p.is_exhausted());
}

#[test]
fn pan295_stream_error_latch_counts_as_a_stall() {
    let t0 = Instant::now();
    let mut w = alive_watchdog(t0);
    let mut p = OutputReopenPolicy::new();
    // The processing-error branch consumes the WentDead edge and does its
    // own rebuild; the policy only sees later successful observations.
    assert_eq!(w.mark_stream_error(ms(t0, 500)), OutputEdge::WentDead);
    w.rebaseline(ms(t0, 500));
    p.on_rebuild(ms(t0, 500));
    assert_eq!(
        step(&mut w, &mut p, 0, ms(t0, 501)),
        OutputReopenDecision::None
    );
    assert_eq!(
        step(&mut w, &mut p, 0, ms(t0, 10_499)),
        OutputReopenDecision::None
    );
    assert_eq!(
        step(&mut w, &mut p, 0, ms(t0, 10_500)),
        OutputReopenDecision::Request
    );
}

#[test]
fn pan295_fourth_stall_is_refused_in_rolling_window() {
    let t0 = Instant::now();
    let mut w = alive_watchdog(t0);
    let mut p = OutputReopenPolicy::new();
    let mut count = 5;
    let mut start = t0;
    for n in 1..=3 {
        assert_eq!(
            stall_from(&mut w, &mut p, count, start),
            OutputReopenDecision::Request,
            "stall {n}"
        );
        attempt(&mut w, &mut p, ms(start, 2010), ms(start, 2100));
        // The rebuilt stream calls back: Recovered ends the episode.
        assert_eq!(w.observe(count, ms(start, 2200)), OutputEdge::None);
        count += 1;
        assert_eq!(w.observe(count, ms(start, 2300)), OutputEdge::Recovered);
        assert_eq!(
            p.decide(OutputEdge::Recovered, true, false, ms(start, 2300)),
            OutputReopenDecision::None
        );
        assert!(!p.is_exhausted());
        start = ms(start, 60_000);
        // Healthy until the next stall.
        count += 1;
        assert_eq!(
            step(&mut w, &mut p, count, ms(start, 10)),
            OutputReopenDecision::None
        );
    }
    assert_eq!(p.attempts_in_window(), 3);
    // A fourth separate stall inside ten minutes is refused at its edge.
    assert_eq!(
        stall_from(&mut w, &mut p, count, start),
        OutputReopenDecision::Refused
    );
    assert!(p.is_exhausted());
    assert_eq!(
        step(&mut w, &mut p, count, ms(start, 30_000)),
        OutputReopenDecision::Refused
    );
    assert!(
        p.is_exhausted(),
        "the limit indication persists while stalled"
    );
}

#[test]
fn pan295_continuous_stall_stops_after_three_attempts() {
    let t0 = Instant::now();
    let mut w = alive_watchdog(t0);
    let mut p = OutputReopenPolicy::new();
    assert_eq!(
        stall_from(&mut w, &mut p, 5, t0),
        OutputReopenDecision::Request
    );
    attempt(&mut w, &mut p, ms(t0, 2010), ms(t0, 2010));
    assert_eq!(
        step(&mut w, &mut p, 0, ms(t0, 12_009)),
        OutputReopenDecision::None
    );
    assert_eq!(
        step(&mut w, &mut p, 0, ms(t0, 12_010)),
        OutputReopenDecision::Request
    );
    attempt(&mut w, &mut p, ms(t0, 12_010), ms(t0, 12_010));
    assert_eq!(
        step(&mut w, &mut p, 0, ms(t0, 22_010)),
        OutputReopenDecision::Request
    );
    attempt(&mut w, &mut p, ms(t0, 22_010), ms(t0, 22_010));
    assert!(!p.is_exhausted());
    // The next retry of the same stall is refused, and keeps being refused.
    assert_eq!(
        step(&mut w, &mut p, 0, ms(t0, 32_009)),
        OutputReopenDecision::None
    );
    assert_eq!(
        step(&mut w, &mut p, 0, ms(t0, 32_010)),
        OutputReopenDecision::Refused
    );
    assert!(p.is_exhausted());
    for t in [40_000, 100_000, 500_000] {
        assert_eq!(
            step(&mut w, &mut p, 0, ms(t0, t)),
            OutputReopenDecision::Refused
        );
    }
    assert_eq!(p.attempts_in_window(), 3);
}

#[test]
fn pan295_callback_recovery_keeps_attempt_history() {
    let t0 = Instant::now();
    let mut w = alive_watchdog(t0);
    let mut p = OutputReopenPolicy::new();
    assert_eq!(
        stall_from(&mut w, &mut p, 5, t0),
        OutputReopenDecision::Request
    );
    attempt(&mut w, &mut p, ms(t0, 2010), ms(t0, 2100));
    assert_eq!(
        step(&mut w, &mut p, 0, ms(t0, 2200)),
        OutputReopenDecision::None
    );
    assert_eq!(w.observe(1, ms(t0, 2300)), OutputEdge::Recovered);
    assert_eq!(
        p.decide(OutputEdge::Recovered, true, false, ms(t0, 2300)),
        OutputReopenDecision::None
    );
    assert_eq!(p.attempts_in_window(), 1, "recovery keeps the history");
    assert!(!p.is_outstanding());
}

#[test]
fn pan295_budget_expires_at_exactly_ten_minutes() {
    let t0 = Instant::now();
    let mut w = alive_watchdog(t0);
    let mut p = OutputReopenPolicy::new();
    // Three attempts at 2010, 12_010, 22_010 ms on one continuous stall.
    assert_eq!(
        stall_from(&mut w, &mut p, 5, t0),
        OutputReopenDecision::Request
    );
    let first = ms(t0, 2010);
    attempt(&mut w, &mut p, first, first);
    for t in [12_010, 22_010] {
        assert_eq!(
            step(&mut w, &mut p, 0, ms(t0, t)),
            OutputReopenDecision::Request
        );
        attempt(&mut w, &mut p, ms(t0, t), ms(t0, t));
    }
    let expiry = first + OUTPUT_REOPEN_WINDOW;
    assert_eq!(
        step(&mut w, &mut p, 0, expiry - Duration::from_millis(1)),
        OutputReopenDecision::Refused
    );
    assert!(p.is_exhausted());
    // Age exactly 600 s: the first attempt no longer counts, and the
    // still-stalled output gets one new request without a new edge.
    assert_eq!(
        step(&mut w, &mut p, 0, expiry),
        OutputReopenDecision::Request
    );
    assert!(!p.is_exhausted(), "expiry clears the limit indication");
    assert_eq!(p.attempts_in_window(), 2);
    attempt(&mut w, &mut p, expiry, expiry);
    assert_eq!(p.attempts_in_window(), 3);
    // The new attempt waits out its retry delay; by then the 12_010
    // attempt has aged out too, so the stall is still bounded by the window.
    assert_eq!(
        step(
            &mut w,
            &mut p,
            0,
            expiry + OUTPUT_REOPEN_RETRY_AFTER - Duration::from_millis(1)
        ),
        OutputReopenDecision::None
    );
    assert_eq!(
        step(&mut w, &mut p, 0, ms(t0, 12_010) + OUTPUT_REOPEN_WINDOW),
        OutputReopenDecision::Request
    );
    assert_eq!(p.attempts_in_window(), 2);
}

#[test]
fn pan295_healthy_output_does_not_retry_at_budget_expiry() {
    let t0 = Instant::now();
    let mut w = alive_watchdog(t0);
    let mut p = OutputReopenPolicy::new();
    assert_eq!(
        stall_from(&mut w, &mut p, 5, t0),
        OutputReopenDecision::Request
    );
    attempt(&mut w, &mut p, ms(t0, 2010), ms(t0, 2100));
    assert_eq!(
        step(&mut w, &mut p, 0, ms(t0, 2200)),
        OutputReopenDecision::None
    );
    // Callbacks resume and continue past the window's end.
    let mut count = 0;
    let mut t = 2300;
    while t <= 2010 + 600_000 + 5000 {
        count += 1;
        assert_eq!(
            step(&mut w, &mut p, count, ms(t0, t)),
            OutputReopenDecision::None,
            "at {t} ms"
        );
        t += 1000;
    }
    assert_eq!(
        p.attempts_in_window(),
        0,
        "history expired on healthy ticks"
    );
    assert!(!p.is_outstanding());
    assert!(!p.is_exhausted());
}

#[test]
fn pan295_send_failure_does_not_consume_budget_or_spin() {
    let t0 = Instant::now();
    let mut w = alive_watchdog(t0);
    let mut p = OutputReopenPolicy::new();
    assert_eq!(
        stall_from(&mut w, &mut p, 5, t0),
        OutputReopenDecision::Request
    );
    p.on_send_failed(ms(t0, 2010));
    assert_eq!(p.attempts_in_window(), 0);
    assert_eq!(
        step(&mut w, &mut p, 5, ms(t0, 2011)),
        OutputReopenDecision::None
    );
    assert_eq!(
        step(&mut w, &mut p, 5, ms(t0, 12_010)),
        OutputReopenDecision::Request
    );
}
