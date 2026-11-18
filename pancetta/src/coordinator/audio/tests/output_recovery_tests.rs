//! PAN-295: the audio thread's bounded output reopen, driven through the
//! production helpers the loop calls, a real request channel and fake
//! callback counters / clocks.
use super::*;
use crate::coordinator::audio_recovery::{OutputWatchdog, OUTPUT_REOPEN_RETRY_AFTER};
use std::sync::atomic::AtomicBool;

fn ms(t0: Instant, millis: u64) -> Instant {
    t0 + Duration::from_millis(millis)
}

/// The audio-thread state the loop keeps, plus the drain end of the FIFO.
struct Harness {
    watch: OutputWatchdog,
    recovery: OutputStallRecovery,
    alive: Arc<AtomicBool>,
    alive_published: bool,
    exhausted: Arc<AtomicBool>,
    tx: crossbeam_channel::Sender<AudioReopenRequest>,
    rx: crossbeam_channel::Receiver<AudioReopenRequest>,
    count: u64,
    requests: usize,
}

impl Harness {
    /// A healthy output: callbacks seen up to `t0 + 10 ms`.
    fn healthy(t0: Instant) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let alive = Arc::new(AtomicBool::new(false));
        let exhausted = Arc::new(AtomicBool::new(false));
        let mut h = Self {
            watch: OutputWatchdog::new(t0),
            recovery: OutputStallRecovery::new(tx.clone(), exhausted.clone()),
            alive,
            alive_published: false,
            exhausted,
            tx,
            rx,
            count: 0,
            requests: 0,
        };
        h.observe(t0);
        h.progress(ms(t0, 10));
        assert!(h.alive.load(Ordering::Acquire));
        h
    }

    /// One successful-observation iteration (both `process_audio` `Ok`
    /// branches end here).
    fn observe(&mut self, now: Instant) -> bool {
        let edge = self.watch.observe(self.count, now);
        let queued = finish_output_observation(
            &self.watch,
            &mut self.recovery,
            &self.alive,
            &mut self.alive_published,
            Some(edge),
            now,
            &|_, _, _| {},
        );
        if queued {
            self.requests += 1;
        }
        queued
    }

    fn progress(&mut self, now: Instant) -> bool {
        self.count += 1;
        self.observe(now)
    }

    /// The drain loop: take one request, "rebuild" with `result`,
    /// rebaseline (counter restarts at 0) and reply.
    fn drain_one(&mut self, now: Instant, result: Result<(), String>) -> AudioReopenRequest {
        let req = self.rx.try_recv().expect("a queued reopen request");
        self.count = 0;
        after_output_reopen(
            &mut self.watch,
            &mut self.recovery,
            &self.alive,
            &mut self.alive_published,
            now,
        );
        let AudioReopenRequest {
            input,
            output,
            force,
            respond,
        } = req;
        let _ = respond.send(result);
        self.recovery.poll_pending(now);
        let (respond, _rx) = tokio::sync::oneshot::channel();
        AudioReopenRequest {
            input,
            output,
            force,
            respond,
        }
    }

    fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

#[test]
fn pan295_output_stall_requests_one_forced_reopen() {
    // Both `Ok(Some(samples))` (input batches every ~170 ms keep
    // arriving) and `Ok(None)` (1 ms idle spins) end in the same tail
    // with `observed = true`; only the observation cadence differs.
    for (path, step_ms) in [("Ok(Some(samples))", 170u64), ("Ok(None)", 1u64)] {
        let t0 = Instant::now();
        let mut h = Harness::healthy(t0);
        let mut t = 10 + step_ms;
        while t < 2010 {
            assert!(
                !h.observe(ms(t0, t)),
                "{path}: no request before 2 s ({t} ms)"
            );
            t += step_ms;
        }
        assert!(h.rx.is_empty(), "{path}");
        assert!(h.observe(ms(t0, 2010)), "{path}: request at the dead edge");
        assert!(!h.alive(), "{path}: TX muted");
        let req = h.rx.try_recv().unwrap();
        assert_eq!(req.input, None, "{path}");
        assert_eq!(req.output, None, "{path}");
        assert!(req.force, "{path}");
        let mut t = 2010 + step_ms;
        while t < 4000 {
            assert!(!h.observe(ms(t0, t)), "{path}: one request only");
            t += step_ms;
        }
        assert!(h.rx.is_empty(), "{path}");
        assert!(!h.exhausted.load(Ordering::Acquire));
    }
    // Healthy observations never request.
    let t0 = Instant::now();
    let mut h = Harness::healthy(t0);
    for t in (20..20_000).step_by(100) {
        assert!(!h.progress(ms(t0, t)));
    }
    assert!(h.rx.is_empty());
}

#[test]
fn pan295_pending_request_does_not_duplicate() {
    let t0 = Instant::now();
    let mut h = Harness::healthy(t0);
    assert!(h.observe(ms(t0, 2010)));
    // The drain has not run yet: the request stays the only one.
    for t in (2011..60_000).step_by(7) {
        h.observe(ms(t0, t));
    }
    assert_eq!(h.rx.len(), 1);
    assert_eq!(h.requests, 1);
}

#[test]
fn pan295_failed_reopen_retries_after_ten_seconds() {
    let t0 = Instant::now();
    let mut h = Harness::healthy(t0);
    assert!(h.observe(ms(t0, 2010)));
    h.drain_one(ms(t0, 2500), Err("device busy".into()));
    assert!(!h.observe(ms(t0, 2501)));
    assert!(!h.observe(ms(t0, 12_499)));
    assert!(
        h.observe(ms(t0, 12_500)),
        "retry 10 s after the rebuild finished"
    );
    assert_eq!(h.rx.len(), 1);
}

#[test]
fn pan295_rebuilt_but_stalled_output_retries_after_ten_seconds() {
    let t0 = Instant::now();
    let mut h = Harness::healthy(t0);
    assert!(h.observe(ms(t0, 2010)));
    h.drain_one(ms(t0, 3000), Ok(()));
    // The rebuilt stream never calls back: no second WentDead edge.
    for t in (3001..13_000).step_by(50) {
        assert!(!h.observe(ms(t0, t)), "{t} ms");
    }
    assert!(h.observe(ms(t0, 13_000)));
    assert!(!h.alive());
}

#[test]
fn pan295_reopen_result_does_not_unmute_without_progress() {
    for result in [Ok(()), Err("rolled back".to_string())] {
        let t0 = Instant::now();
        let mut h = Harness::healthy(t0);
        assert!(!h.observe(ms(t0, 1000)) && h.alive());
        assert!(h.observe(ms(t0, 2010)));
        assert!(!h.alive(), "dead before the request is drained");
        // A drained reopen publishes false itself, before TX commands.
        h.alive.store(true, Ordering::Release);
        h.alive_published = true;
        h.drain_one(ms(t0, 2100), result.clone());
        assert!(!h.alive(), "{result:?}: not alive on the reopen result");
        // The first counter read after the rebuild is a baseline only.
        assert!(!h.observe(ms(t0, 2200)));
        assert!(!h.alive(), "{result:?}: baseline is not progress");
        assert!(!h.observe(ms(t0, 2300)));
        assert!(!h.alive(), "{result:?}");
        // Only fresh callback progress restores output health.
        h.progress(ms(t0, 2400));
        assert!(h.alive(), "{result:?}");
        assert!(!h.exhausted.load(Ordering::Acquire));
    }
}

#[test]
fn pan295_three_failed_requests_exhaust_budget() {
    let t0 = Instant::now();
    let mut h = Harness::healthy(t0);
    assert!(h.observe(ms(t0, 2010)));
    let mut rebuilt = ms(t0, 2100);
    h.drain_one(rebuilt, Err("busy".into()));
    for _ in 0..2 {
        let due = rebuilt + OUTPUT_REOPEN_RETRY_AFTER;
        assert!(h.observe(due));
        rebuilt = due + Duration::from_millis(90);
        h.drain_one(rebuilt, Err("busy".into()));
    }
    assert_eq!(h.requests, 3);
    assert!(!h.exhausted.load(Ordering::Acquire));
    // The fourth retry is refused and the display flag goes up.
    assert!(!h.observe(rebuilt + OUTPUT_REOPEN_RETRY_AFTER));
    assert!(h.exhausted.load(Ordering::Acquire));
    // Until the first attempt (2010 ms) ages out at 602_010 ms.
    for s in 1..55 {
        assert!(!h.observe(rebuilt + Duration::from_secs(10 * s)));
    }
    assert!(h.rx.is_empty());
    assert_eq!(h.requests, 3);
    assert!(h.exhausted.load(Ordering::Acquire));
    assert!(!h.alive());
}

/// Exhaust the budget on one continuous stall; returns the time of the
/// refused fourth retry.
fn exhausted(h: &mut Harness, t0: Instant) -> Instant {
    assert!(h.observe(ms(t0, 2010)));
    let mut rebuilt = ms(t0, 2100);
    h.drain_one(rebuilt, Ok(()));
    for _ in 0..2 {
        assert!(h.observe(rebuilt + OUTPUT_REOPEN_RETRY_AFTER));
        rebuilt += OUTPUT_REOPEN_RETRY_AFTER;
        h.drain_one(rebuilt, Ok(()));
    }
    let refused = rebuilt + OUTPUT_REOPEN_RETRY_AFTER;
    assert!(!h.observe(refused));
    assert!(h.exhausted.load(Ordering::Acquire));
    refused
}

#[test]
fn pan295_manual_reopen_remains_available_after_exhaustion() {
    let t0 = Instant::now();
    let mut h = Harness::healthy(t0);
    let refused = exhausted(&mut h, t0);
    // The operator's device pick goes through the same channel.
    let (respond, mut reply) = tokio::sync::oneshot::channel();
    h.tx.send(AudioReopenRequest {
        input: None,
        output: Some("USB Audio CODEC".into()),
        force: true,
        respond,
    })
    .unwrap();
    let manual = h.drain_one(refused + Duration::from_millis(5), Ok(()));
    assert_eq!(manual.output.as_deref(), Some("USB Audio CODEC"));
    assert_eq!(reply.try_recv(), Ok(Ok(())));
    assert_eq!(h.recovery.policy.attempts_in_window(), 3, "not charged");
    assert!(
        h.exhausted.load(Ordering::Acquire),
        "not cleared by the pick"
    );
    // Callbacks return on the new device: dead and limit both clear.
    h.observe(refused + Duration::from_millis(10));
    h.progress(refused + Duration::from_millis(20));
    assert!(h.alive());
    assert!(!h.exhausted.load(Ordering::Acquire));
    // The budget is still spent: a new stall is refused at its edge.
    assert!(!h.observe(refused + Duration::from_millis(2020)));
    assert!(h.exhausted.load(Ordering::Acquire));
    assert_eq!(h.requests, 3);
}

#[test]
fn pan295_other_reopens_do_not_reset_output_budget() {
    let t0 = Instant::now();
    let mut h = Harness::healthy(t0);
    assert!(h.observe(ms(t0, 2010)));
    h.drain_one(ms(t0, 2100), Err("busy".into()));
    assert_eq!(h.recovery.policy.attempts_in_window(), 1);
    // An input-stale self-triggered reopen (relay task's sender).
    let (respond, _r) = tokio::sync::oneshot::channel();
    h.tx.send(AudioReopenRequest {
        input: None,
        output: None,
        force: true,
        respond,
    })
    .unwrap();
    h.drain_one(ms(t0, 5000), Ok(()));
    // A processing-error direct reopen.
    h.watch.mark_stream_error(ms(t0, 6000));
    after_output_reopen(
        &mut h.watch,
        &mut h.recovery,
        &h.alive,
        &mut h.alive_published,
        ms(t0, 6100),
    );
    assert_eq!(
        h.recovery.policy.attempts_in_window(),
        1,
        "neither charged nor replenished"
    );
    assert_eq!(h.requests, 1);
    // The external rebuilds postponed the retry to 10 s after the last.
    assert!(!h.observe(ms(t0, 16_099)));
    assert!(h.observe(ms(t0, 16_100)));
    assert_eq!(h.recovery.policy.attempts_in_window(), 2);
}

#[test]
fn pan295_processing_error_does_not_double_reopen() {
    let t0 = Instant::now();
    let mut h = Harness::healthy(t0);
    assert!(h.observe(ms(t0, 2010)));
    h.drain_one(ms(t0, 2100), Err("busy".into()));
    // The retry is due at 12_100, but that iteration hits a processing
    // error: the loop marks the stream error, does its own direct
    // reopen and finishes with `observed = false`.
    let now = ms(t0, 12_100);
    assert_eq!(h.watch.mark_stream_error(now), OutputEdge::None);
    h.watch.rebaseline(now);
    h.recovery.note_rebuild(now);
    let queued = finish_output_observation(
        &h.watch,
        &mut h.recovery,
        &h.alive,
        &mut h.alive_published,
        None,
        now,
        &|_, _, _| {},
    );
    assert!(!queued);
    assert!(
        h.rx.is_empty(),
        "no output-watchdog reopen beside the direct one"
    );
    assert!(!h.alive());
    // The direct rebuild postpones the next automatic attempt.
    assert!(!h.observe(ms(t0, 12_101)));
    assert!(!h.observe(ms(t0, 22_099)));
    assert!(h.observe(ms(t0, 22_100)));
    assert_eq!(h.recovery.policy.attempts_in_window(), 2);
}

#[test]
fn pan295_silent_output_after_processing_error_rebuild_is_reopened() {
    let t0 = Instant::now();
    let mut h = Harness::healthy(t0);
    // A processing-error iteration: stream error, direct reopen, no
    // output-policy request.
    let now = ms(t0, 1000);
    assert_eq!(h.watch.mark_stream_error(now), OutputEdge::WentDead);
    h.count = 0;
    h.watch.rebaseline(now);
    h.recovery.note_rebuild(now);
    assert!(!finish_output_observation(
        &h.watch,
        &mut h.recovery,
        &h.alive,
        &mut h.alive_published,
        None,
        now,
        &|_, _, _| {},
    ));
    // The rebuilt output never calls back: treated as a silent stall.
    for t in (1001..11_000).step_by(100) {
        assert!(!h.observe(ms(t0, t)), "{t} ms");
    }
    assert!(h.observe(ms(t0, 11_000)));
    assert!(!h.alive());
    assert_eq!(h.recovery.policy.attempts_in_window(), 1);
}

#[test]
fn pan295_dropping_recovery_clears_exhaustion() {
    let t0 = Instant::now();
    let mut h = Harness::healthy(t0);
    exhausted(&mut h, t0);
    let flag = h.exhausted.clone();
    drop(h);
    assert!(!flag.load(Ordering::Acquire));
}
