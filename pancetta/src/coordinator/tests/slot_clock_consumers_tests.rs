//! PAN-358 ticket scenario: a clock step is observed by every consumer at
//! once.
//!
//! One `ManualClock` and one `SlotClock` feed the six consumers this test
//! drives: the autonomous slot tick (`autonomous_slot_poll` on a
//! `SlotTimer`), the DSP decode-window cutter (`decode_window_clock_step` on
//! a `SlotTimer` at the mode's production decode phase), the QSO watchdog
//! (`QsoManager::watchdog_pass` with one active QSO), the queued-call TTL
//! (the production partition over two queued calls), the clock-skew monitor
//! (`MonitorStepCursor` feeding `ClockSkewMonitorCore`) and the TX worker's
//! pre-PTT wait (`ptt_wait_step` on a frame `schedule_tx` placed at the first
//! reading, PAN-483). The test polls all six every 250 ms of virtual time,
//! four times before the step and for two slots after it; the TX wait stops
//! polling once it reaches its PTT instant or drops the frame, as the
//! worker's wait does.
//!
//! PAN-477: the decode-parity rows cut a window from the DSP timer on the
//! same clock, carry it through a real `bounded(2)` channel, step the wall
//! before the decoder receives it, and check that the decoder stamps the
//! parity of the slot the DSP cut, not of the receipt time.

use super::autonomous::autonomous_slot_poll;
use super::clock_monitor::MonitorStepCursor;
use super::dsp::{decode_window_clock_step, decode_window_slot};
use super::pipeline::{forward_or_drop, DecodeWindow, ForwardOutcome, DECODE_FORWARD_TIMEOUT};
use super::qso::queued_call_ttl_partition_for_test;
use super::tx::{ptt_wait_step, schedule_tx, PttWaitStep};
use crate::clock_skew::ClockSkewMonitorCore;
use jiff::SignedDuration;
use pancetta_core::slot::{
    current_slot_start_with_period, next_phase_with_period, next_slot_start_with_period, SlotParity,
};
use pancetta_core::slot_clock::{ClockEvent, ClockStamp, ManualClock};
use pancetta_core::slot_timer::{Reanchor, SlotTimer};
use pancetta_core::time::UtcTimestamp;
use pancetta_ft8::ProtocolParams;
use pancetta_qso::{QsoManager, QsoManagerConfig};
use std::sync::atomic::AtomicI64;
use std::sync::Arc;

const FT8_NS: i64 = 15_000_000_000;
const FT4_NS: i64 = 7_500_000_000;
const FT2_NS: i64 = 3_200_000_000;
const POLL: std::time::Duration = std::time::Duration::from_millis(250);
/// Default `station.ptt_lead_ms`.
const PTT_LEAD: SignedDuration = SignedDuration::from_millis(80);

/// 2026-01-01 00:00:00 UTC, a boundary for FT8, FT4 and FT2.
fn base() -> UtcTimestamp {
    UtcTimestamp::from_ymd_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

fn ms(ms: i64) -> SignedDuration {
    SignedDuration::from_millis(ms)
}

/// The DSP cutter's production decode phase for a scenario slot length:
/// `derive_dsp_timing` on that mode's protocol parameters, as the coordinator
/// derives it at startup (FT8 13 s, FT4 6.5 s, FT2 2.2 s).
fn dsp_decode_phase(slot_ns: i64) -> SignedDuration {
    let params = match slot_ns {
        FT8_NS => ProtocolParams::ft8(),
        FT4_NS => ProtocolParams::ft4(),
        #[cfg(feature = "ft2")]
        FT2_NS => ProtocolParams::ft2(),
        // Without the `ft2` feature there are no FT2 parameters. FT4's with
        // FT2's 3.2 s cycle derive the same timing: every non-FT8 mode
        // decodes 1 s before the next boundary.
        #[cfg(not(feature = "ft2"))]
        FT2_NS => {
            let mut params = ProtocolParams::ft4();
            params.cycle_duration = 3.2;
            params
        }
        other => panic!("no scenario mode has a {other} ns slot"),
    };
    let timing = super::derive_dsp_timing(&params);
    assert_eq!(
        timing.slot_ns, slot_ns,
        "decode timing for the scenario's slot"
    );
    timing.decode_phase
}

/// What each consumer did after the step.
#[derive(Debug, Default)]
struct Observed {
    /// Every event a consumer reported, tagged by consumer.
    events: Vec<(&'static str, ClockEvent)>,
    /// `reading.now` of every autonomous fire after the step.
    autonomous_fires_after: Vec<UtcTimestamp>,
    /// Whether the first post-step autonomous poll fired.
    autonomous_fired_on_first_post_step_poll: bool,
    /// Autonomous re-anchors after the step.
    autonomous_reanchors: Vec<Reanchor>,
    /// Whether any poll asked to clear the stale decodes.
    autonomous_cleared: bool,
    /// DSP cutter re-anchors after the step.
    dsp_reanchors: Vec<Reanchor>,
    /// DSP cutter polls after the step that asked to flush the buffer.
    dsp_flushes: usize,
    /// Whether the first post-step DSP poll was due.
    dsp_due_on_first_post_step_poll: bool,
    /// `reading.now` of every due DSP poll after the step.
    dsp_due_after: Vec<UtcTimestamp>,
    /// The watchdog's applied epoch after its first post-step pass.
    watchdog_epoch_after_first_pass: u64,
    /// Active QSOs at the end of the run.
    active_qsos_at_end: usize,
    /// Callsigns expired on the first post-step TTL pass.
    ttl_expired_on_first_pass: Vec<String>,
    /// Monitor `clock.step` lines after the step.
    monitor_lines: Vec<String>,
    /// The TX frame's PTT engage instant (`target_slot - ptt_lead_ms`).
    tx_ptt_target: Option<UtcTimestamp>,
    /// The post-step poll index and `reading.now` at which the TX wait
    /// ended, and how.
    tx_wait_end: Option<(i64, UtcTimestamp, PttWaitStep)>,
}

/// Run the six consumers on one clock: pre-step polls from `p` seconds
/// into the slot at 00:00:00, then `step`, then two periods of polls.
async fn run_scenario(slot_ns: i64, p_ms: i64, step: SignedDuration) -> (Observed, UtcTimestamp) {
    let start = base() + ms(p_ms);
    let manual = ManualClock::new(start);
    let clock = manual.slot_clock(Arc::new(AtomicI64::new(slot_ns)));

    // Autonomous slot tick.
    let mut timer = SlotTimer::new(&clock.read(), SignedDuration::ZERO, slot_ns);

    // DSP decode-window cutter at the mode's production decode phase. The
    // scenario checks the window timer, not sample accounting: it treats the
    // buffer as full, so a due poll stands for an emit and advances the
    // timer. Production also waits for a full buffer after a flush.
    let decode_phase = dsp_decode_phase(slot_ns);
    let mut dsp_timer = SlotTimer::new(&clock.read(), decode_phase, slot_ns);

    // QSO watchdog with one active QSO.
    let mut manager = QsoManager::new(QsoManagerConfig {
        our_callsign: "W1ABC".to_string(),
        our_grid: Some("FN42".to_string()),
        ..Default::default()
    });
    manager.set_slot_clock(clock.clone());
    manager
        .respond_to_cq("K1ABC".to_string(), 1500.0, None)
        .await
        .expect("open a QSO");
    assert_eq!(manager.active_qso_count().await, 1);

    // Queued-call TTL: one call queued 9 min 30 s ago, one queued now.
    let fresh = clock.stamp();
    let old = ClockStamp {
        wall: fresh.wall - SignedDuration::from_secs(570),
        mono: fresh.mono,
    };
    let queue = [("JA1OLD", old), ("JA1NEW", fresh)];

    // Clock-skew monitor.
    let mut cursor = MonitorStepCursor::new(&clock.read());
    let mut monitor = ClockSkewMonitorCore::new();
    let mut mono_since_start = std::time::Duration::ZERO;

    // TX worker: a frame scheduled at the first reading, as the worker's
    // dequeue does, for the first boundary after the last pre-step poll plus
    // the PTT lead, so every case's PTT instant lies after the step.
    let tx_reading = clock.read();
    let last_pre_step_poll = start + SignedDuration::try_from(POLL * 4).unwrap();
    let tx_boundary =
        next_slot_start_with_period(last_pre_step_poll + PTT_LEAD, SignedDuration::ZERO, slot_ns);
    let tx_parity = SlotParity::of_with_period(tx_boundary, slot_ns);
    let tx_schedule = schedule_tx(tx_reading.now, tx_parity, 3000, 12_000, slot_ns);
    assert_eq!(tx_schedule.target_slot, tx_boundary, "TX frame target slot");
    let tx_ptt_target = tx_schedule.target_slot - PTT_LEAD;
    let tx_scheduled_epoch = tx_reading.resync_epoch;

    let mut obs = Observed {
        tx_ptt_target: Some(tx_ptt_target),
        ..Observed::default()
    };

    // Four pre-step polls: nothing re-anchors, nothing flushes, nothing
    // expires.
    for _ in 0..4 {
        manual.advance(POLL);
        mono_since_start += POLL;
        let reading = clock.read();
        let tick = autonomous_slot_poll(&mut timer, &reading, slot_ns);
        assert!(tick.reanchor.is_none(), "no re-anchor before the step");
        if tick.due {
            timer.advance(&reading);
        }
        let window = decode_window_clock_step(&mut dsp_timer, &reading, slot_ns);
        assert!(
            window.reanchor.is_none() && !window.flush,
            "no DSP re-anchor or flush before the step"
        );
        if window.due {
            dsp_timer.advance(&reading);
        }
        assert!(
            matches!(
                ptt_wait_step(&reading, tx_ptt_target, tx_scheduled_epoch),
                PttWaitStep::Sleep(_)
            ),
            "the TX frame waits before the step"
        );
        manager.watchdog_pass().await;
        let (_, expired) = queued_call_ttl_partition_for_test(&queue, clock.stamp());
        assert!(expired.is_empty(), "nothing expires before the step");
        let outcome = monitor.on_tick(mono_since_start, cursor.step(&clock.read()));
        assert!(outcome.step_diag.is_none());
    }
    assert_eq!(manager.applied_clock_epoch(), 0);

    let pre_step_now = manual.now();
    manual.step_wall(step);

    let polls = (2 * slot_ns / POLL.as_nanos() as i64) + 4;
    for i in 0..polls {
        manual.advance(POLL);
        mono_since_start += POLL;

        let reading = clock.read();
        let tick = autonomous_slot_poll(&mut timer, &reading, slot_ns);
        if tick.clear_stale_decodes {
            obs.autonomous_cleared = true;
        }
        if let Some(reanchor) = tick.reanchor {
            if let Reanchor::ClockStep(event) = reanchor {
                obs.events.push(("autonomous", event));
            }
            obs.autonomous_reanchors.push(reanchor);
        }
        if tick.due {
            if i == 0 {
                obs.autonomous_fired_on_first_post_step_poll = true;
            }
            obs.autonomous_fires_after.push(reading.now);
            timer.advance(&reading);
        }

        let window = decode_window_clock_step(&mut dsp_timer, &reading, slot_ns);
        if window.flush {
            obs.dsp_flushes += 1;
        }
        if let Some(reanchor) = window.reanchor {
            if let Reanchor::ClockStep(event) = reanchor {
                obs.events.push(("DSP cutter", event));
            }
            obs.dsp_reanchors.push(reanchor);
        }
        if window.due {
            if i == 0 {
                obs.dsp_due_on_first_post_step_poll = true;
            }
            obs.dsp_due_after.push(reading.now);
            dsp_timer.advance(&reading);
        }

        if obs.tx_wait_end.is_none() {
            let step = ptt_wait_step(&reading, tx_ptt_target, tx_scheduled_epoch);
            if !matches!(step, PttWaitStep::Sleep(_)) {
                obs.tx_wait_end = Some((i, reading.now, step));
            }
        }

        manager.watchdog_pass().await;
        if i == 0 {
            obs.watchdog_epoch_after_first_pass = manager.applied_clock_epoch();
            if let Some(event) = clock.read().last_event {
                if obs.watchdog_epoch_after_first_pass == event.epoch {
                    obs.events.push(("watchdog", event));
                }
            }
        }

        let (_, expired) = queued_call_ttl_partition_for_test(&queue, clock.stamp());
        if i == 0 {
            obs.ttl_expired_on_first_pass = expired;
            if let Some(event) = clock.read().last_event {
                obs.events.push(("queued-call TTL", event));
            }
        }

        let monitor_reading = clock.read();
        if let Some(event) = cursor.step(&monitor_reading) {
            obs.events.push(("clock-skew monitor", event));
            let outcome = monitor.on_tick(mono_since_start, Some(event));
            if let Some(diag) = outcome.step_diag {
                obs.monitor_lines.push(diag.text);
            }
        } else {
            let outcome = monitor.on_tick(mono_since_start, None);
            if let Some(diag) = outcome.step_diag {
                obs.monitor_lines.push(diag.text);
            }
        }
    }
    obs.active_qsos_at_end = manager.active_qso_count().await;
    (obs, pre_step_now)
}

/// The assertions every resync case shares.
async fn assert_resync_case(slot_ns: i64, p_ms: i64, step: SignedDuration) {
    let label = format!("slot {slot_ns} ns, p {p_ms} ms, step {step}");
    let (obs, pre_step_now) = run_scenario(slot_ns, p_ms, step).await;

    // The step lands in the slot of `pre-step time + step`.
    let post_step_now = pre_step_now + step + ms(250);
    let expected_slot = current_slot_start_with_period(post_step_now, slot_ns);

    // Every consumer reports the same event: epoch 1, the real step size,
    // the same post-step slot.
    let consumers: Vec<&str> = obs.events.iter().map(|(c, _)| *c).collect();
    for consumer in [
        "autonomous",
        "DSP cutter",
        "watchdog",
        "queued-call TTL",
        "clock-skew monitor",
    ] {
        assert_eq!(
            obs.events.iter().filter(|(c, _)| *c == consumer).count(),
            1,
            "{label}: {consumer} must see the step exactly once (saw {consumers:?})"
        );
    }
    let first = obs.events[0].1;
    for (consumer, event) in &obs.events {
        assert_eq!(
            *event, first,
            "{label}: {consumer} must see the same event as the others"
        );
    }
    assert_eq!(first.epoch, 1, "{label}");
    assert!(first.resync, "{label}");
    assert_eq!(first.skew, step, "{label}: the event carries the real step");
    assert_eq!(
        first.slot_start(),
        expected_slot,
        "{label}: every consumer re-anchors in the same post-step slot"
    );

    // Autonomous: no fire on the first post-step poll (the pre-step deadline
    // is never acted on); the first fire is the first boundary after the
    // step; stale decodes are cleared.
    assert!(
        !obs.autonomous_fired_on_first_post_step_poll,
        "{label}: autonomous must not fire a deadline computed before the step"
    );
    assert!(obs.autonomous_cleared, "{label}: stale decodes are cleared");
    assert_eq!(
        obs.autonomous_reanchors,
        vec![Reanchor::ClockStep(first)],
        "{label}: autonomous re-anchors once, on the clock event"
    );
    let first_boundary = next_phase_with_period(first.observed_at, SignedDuration::ZERO, slot_ns);
    let first_fire = *obs
        .autonomous_fires_after
        .first()
        .unwrap_or_else(|| panic!("{label}: autonomous fires after the step"));
    assert!(
        first_fire >= first_boundary && first_fire - first_boundary < ms(250),
        "{label}: first fire {first_fire} must be the first poll at or after {first_boundary}"
    );
    for pair in obs.autonomous_fires_after.windows(2) {
        let slot = current_slot_start_with_period(pair[1], slot_ns);
        assert!(
            pair[0] < slot,
            "{label}: one fire per boundary ({} then {})",
            pair[0],
            pair[1]
        );
    }

    // DSP cutter: one re-anchor on the clock event and one flush of the
    // buffer that spans the step; the pre-step deadline is never emitted, and
    // the first emit is the first decode phase after the step.
    assert_eq!(
        obs.dsp_reanchors,
        vec![Reanchor::ClockStep(first)],
        "{label}: the DSP cutter re-anchors once, on the clock event"
    );
    assert_eq!(obs.dsp_flushes, 1, "{label}: the DSP cutter flushes once");
    assert!(
        !obs.dsp_due_on_first_post_step_poll,
        "{label}: the DSP cutter must not emit a window computed before the step"
    );
    let decode_phase = dsp_decode_phase(slot_ns);
    let first_decode = next_phase_with_period(first.observed_at, decode_phase, slot_ns);
    let first_due = *obs
        .dsp_due_after
        .first()
        .unwrap_or_else(|| panic!("{label}: the DSP cutter emits after the step"));
    assert!(
        first_due >= first_decode && first_due - first_decode < ms(250),
        "{label}: first DSP emit {first_due} must be the first poll at or after {first_decode}"
    );
    for pair in obs.dsp_due_after.windows(2) {
        let deadline = next_phase_with_period(pair[0], decode_phase, slot_ns);
        assert!(
            pair[1] >= deadline,
            "{label}: one DSP emit per decode phase ({} then {})",
            pair[0],
            pair[1]
        );
    }

    // QSO watchdog: the event is applied on the first pass, budgets restart
    // at the step, and the QSO stays active.
    assert_eq!(obs.watchdog_epoch_after_first_pass, 1, "{label}");
    assert_eq!(
        obs.active_qsos_at_end, 1,
        "{label}: the watchdog budget restarts at the step"
    );

    // Queued-call TTL (PAN-289): a forward step ages the older call past the
    // 10 min TTL on the first post-step pass; a backward step never ages it.
    let expected_expired: Vec<String> = if step >= SignedDuration::from_secs(30) {
        vec!["JA1OLD".to_string()]
    } else {
        Vec::new()
    };
    assert_eq!(
        obs.ttl_expired_on_first_pass, expected_expired,
        "{label}: TTL pass on the first post-step reading"
    );

    // Clock-skew monitor: one re-anchored line with the step size.
    assert_eq!(
        obs.monitor_lines.len(),
        1,
        "{label}: {:?}",
        obs.monitor_lines
    );
    assert!(
        obs.monitor_lines[0].ends_with("slot timing re-anchored to UTC"),
        "{label}: {}",
        obs.monitor_lines[0]
    );

    // TX worker: the frame scheduled before the step is dropped on the first
    // post-step poll with the same event's real step size, never keyed.
    let (tx_poll, _, tx_end) = obs
        .tx_wait_end
        .unwrap_or_else(|| panic!("{label}: the TX wait ends after the step"));
    assert_eq!(
        (tx_poll, tx_end),
        (0, PttWaitStep::ClockJumped(first.jump())),
        "{label}: the TX wait drops the frame on the first post-step poll with the shared event"
    );
}

#[tokio::test]
async fn every_consumer_reanchors_in_the_same_slot_after_a_60_s_forward_step() {
    assert_resync_case(FT8_NS, 5_000, SignedDuration::from_secs(60)).await;

    // The ticket's concrete numbers: FT8, 5 s into the 00:00:00 slot, +60 s.
    let (obs, _) = run_scenario(FT8_NS, 5_000, SignedDuration::from_secs(60)).await;
    let event = obs.events[0].1;
    assert_eq!(
        event.slot_start(),
        UtcTimestamp::from_ymd_hms(2026, 1, 1, 0, 1, 0).unwrap()
    );
    assert_eq!(
        obs.autonomous_fires_after[0],
        UtcTimestamp::from_ymd_hms(2026, 1, 1, 0, 1, 15).unwrap(),
        "the first autonomous fire is 00:01:15, never the pre-step 00:00:15"
    );
    assert_eq!(
        obs.monitor_lines[0],
        "system clock jumped +60.0 s (sleep/wake or NTP step) — slot timing re-anchored to UTC"
    );
}

#[tokio::test]
async fn every_consumer_reanchors_in_the_same_slot_for_each_mode_direction_and_slot_position() {
    for (slot_ns, p_ms, step) in [
        (FT8_NS, 5_000, SignedDuration::from_secs(-60)),
        (FT8_NS, 5_000, ms(67_500)),
        (FT4_NS, 2_000, SignedDuration::from_secs(60)),
        (FT2_NS, 1_000, SignedDuration::from_secs(60)),
        (FT8_NS, 1_000, SignedDuration::from_secs(20)),
        (FT8_NS, 5_000, SignedDuration::from_secs(20)),
        (FT8_NS, 14_000, SignedDuration::from_secs(20)),
    ] {
        assert_resync_case(slot_ns, p_ms, step).await;
    }
}

#[tokio::test]
async fn sub_second_step_changes_no_slot_consumer() {
    let (obs, _) = run_scenario(FT8_NS, 5_000, ms(500)).await;

    // The step is an event, but not a resync.
    assert!(obs
        .events
        .iter()
        .all(|(_, e)| e.epoch == 1 && !e.resync && e.skew == ms(500)));

    // Autonomous: no re-anchor, no cleared decodes, one fire per boundary
    // exactly as without a step (00:00:15, 00:00:30; the run ends at
    // 00:00:37.5).
    assert!(obs.autonomous_reanchors.is_empty());
    assert!(!obs.autonomous_cleared);
    let boundaries: Vec<UtcTimestamp> = obs
        .autonomous_fires_after
        .iter()
        .map(|now| current_slot_start_with_period(*now, FT8_NS))
        .collect();
    assert_eq!(
        boundaries,
        vec![
            base() + SignedDuration::from_secs(15),
            base() + SignedDuration::from_secs(30),
        ]
    );
    for (fire, boundary) in obs.autonomous_fires_after.iter().zip(&boundaries) {
        assert!(*fire - *boundary < ms(250), "fired late: {fire}");
    }

    // DSP cutter: no re-anchor, no flush, one emit per decode phase exactly
    // as without a step (00:00:13, 00:00:28), each within one poll of its
    // deadline.
    assert!(obs.dsp_reanchors.is_empty(), "{:?}", obs.dsp_reanchors);
    assert_eq!(obs.dsp_flushes, 0);
    let decode_phase = dsp_decode_phase(FT8_NS);
    let deadlines: Vec<UtcTimestamp> = obs
        .dsp_due_after
        .iter()
        .map(|now| current_slot_start_with_period(*now - decode_phase, FT8_NS) + decode_phase)
        .collect();
    assert_eq!(
        deadlines,
        vec![
            base() + SignedDuration::from_secs(13),
            base() + SignedDuration::from_secs(28),
        ]
    );
    for (due, deadline) in obs.dsp_due_after.iter().zip(&deadlines) {
        assert!(
            *due >= *deadline && *due - *deadline < ms(250),
            "emitted late: {due}"
        );
    }

    // TX worker: the frame keeps its slot and reaches its PTT instant on the
    // first poll at or after it (00:00:14.92 for the 00:00:15 slot).
    let ptt_target = obs.tx_ptt_target.expect("TX frame scheduled");
    assert_eq!(
        ptt_target,
        base() + SignedDuration::from_secs(15) - PTT_LEAD
    );
    let (_, reached_at, tx_end) = obs.tx_wait_end.expect("the TX wait ends");
    assert_eq!(tx_end, PttWaitStep::Reached);
    assert!(
        reached_at >= ptt_target && reached_at - ptt_target < ms(250),
        "reached at {reached_at}, PTT instant {ptt_target}"
    );

    // The watchdog still applies the event (PAN-296), and the QSO stays.
    assert_eq!(obs.watchdog_epoch_after_first_pass, 1);
    assert_eq!(obs.active_qsos_at_end, 1);

    // No queued call expires.
    assert!(obs.ttl_expired_on_first_pass.is_empty());

    // The monitor says slot timing was kept.
    assert_eq!(
        obs.monitor_lines,
        vec![
            "system clock jumped +0.5 s (sleep/wake or NTP step) — under 1 s, slot timing kept"
                .to_string()
        ]
    );
}

/// PAN-477: what one decode-parity run observed.
struct DecodeParityRun {
    /// The slot the DSP cut the window from.
    cut_start: UtcTimestamp,
    /// The parity the decoder stamps from the received window.
    received_parity: SlotParity,
    /// The pre-PAN-477 parity: the receipt read minus the decode phase.
    receipt_parity: SlotParity,
    /// The clock's resync epoch at receipt.
    resync_epoch: u64,
}

/// PAN-477: cut the window the DSP emits at the first decode phase after
/// 00:00:00 (polling every 250 ms from 1 s before it), send it through a
/// real `bounded(2)` decode channel, step the wall by `step`, then receive
/// it as the decoder does.
fn decode_parity_run(slot_ns: i64, step: SignedDuration) -> DecodeParityRun {
    let decode_phase = dsp_decode_phase(slot_ns);
    let manual = ManualClock::new(base() + decode_phase - ms(1_000));
    let clock = manual.slot_clock(Arc::new(AtomicI64::new(slot_ns)));
    let mut dsp_timer = SlotTimer::new(&clock.read(), decode_phase, slot_ns);
    assert_eq!(dsp_timer.deadline(), base() + decode_phase);

    let (tx, rx) = crossbeam_channel::bounded::<DecodeWindow>(2);
    loop {
        let reading = clock.read();
        let poll = decode_window_clock_step(&mut dsp_timer, &reading, slot_ns);
        assert!(!poll.flush, "no step before the cut");
        if poll.due {
            let slot = decode_window_slot(&dsp_timer, decode_phase, slot_ns);
            let window = DecodeWindow {
                samples: vec![0.0; 4],
                dial_hz: 14_074_000,
                sweep: None,
                slot,
            };
            assert_eq!(
                forward_or_drop(&tx, window, DECODE_FORWARD_TIMEOUT),
                ForwardOutcome::Sent
            );
            dsp_timer.advance(&reading);
            break;
        }
        manual.advance(POLL);
    }

    manual.step_wall(step);
    let reading = clock.read();
    let received = rx.recv().expect("the window crosses the channel");
    DecodeParityRun {
        cut_start: received.slot.start,
        received_parity: received.slot.parity(),
        receipt_parity: SlotParity::of_with_period(reading.now - decode_phase, slot_ns),
        resync_epoch: reading.resync_epoch,
    }
}

/// PAN-477 ticket scenario: FT4 with one slot clock; the window the DSP
/// emits at 00:00:06.5 is received after a +7.5 s clock step and carries
/// the parity of the slot the DSP cut it from.
#[test]
fn ft4_decode_parity_follows_the_dsp_slot_across_a_7_5_s_step() {
    let run = decode_parity_run(FT4_NS, ms(7_500));
    assert_eq!(dsp_decode_phase(FT4_NS), ms(6_500));
    assert_eq!(run.cut_start, base(), "the DSP cut the 00:00:00 slot");
    assert_eq!(SlotParity::of_with_period(base(), FT4_NS), SlotParity::Even);
    assert_eq!(run.resync_epoch, 1, "the clock reported the step");
    assert_eq!(run.received_parity, SlotParity::Even);
    // The pre-PAN-477 formula, the receipt read minus the decode phase,
    // lands in the next FT4 slot.
    assert_eq!(run.receipt_parity, SlotParity::Odd);
}

/// PAN-477: the decoder stamps the DSP slot's parity for every mode and
/// for steps of either direction, including steps where the receipt
/// formula would also have been right and a sub-second step.
#[test]
fn decode_parity_follows_the_dsp_slot_for_each_mode_and_step() {
    let cases: [(&str, i64, i64, u64); 9] = [
        ("FT4 +7.5 s", FT4_NS, 7_500, 1),
        ("FT4 -7.5 s", FT4_NS, -7_500, 1),
        ("FT4 +60 s", FT4_NS, 60_000, 1),
        ("FT8 +15 s", FT8_NS, 15_000, 1),
        ("FT8 -15 s", FT8_NS, -15_000, 1),
        ("FT8 +7.5 s", FT8_NS, 7_500, 1),
        ("FT2 +3.2 s", FT2_NS, 3_200, 1),
        ("FT2 -3.2 s", FT2_NS, -3_200, 1),
        ("FT8 +0.5 s", FT8_NS, 500, 0),
    ];
    for (name, slot_ns, step_ms, resync_epoch) in cases {
        let run = decode_parity_run(slot_ns, ms(step_ms));
        assert_eq!(
            run.cut_start,
            base(),
            "{name}: the DSP cut the 00:00:00 slot"
        );
        assert_eq!(run.resync_epoch, resync_epoch, "{name}: resync epoch");
        assert_eq!(
            run.received_parity,
            SlotParity::of_with_period(base(), slot_ns),
            "{name}: the decoder stamps the cut slot's parity"
        );
    }
}
