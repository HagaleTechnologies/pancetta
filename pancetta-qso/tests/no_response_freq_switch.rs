//! PAN-37: no-response CQ-streak frequency switch, driven end-to-end through
//! the real [`Sim`]/`SimClock` window-by-window harness (not just direct
//! `decide_at` calls — see `autonomous.rs`'s unit tests for those).
//!
//! Run: `cargo test -p pancetta-qso --test no_response_freq_switch`.

use pancetta_qso::sim::Sim;
use pancetta_qso::{AutonomousConfig, AutonomousOperator, SlotParityConfig, SpectralSnapshot};

const US: &str = "K5ARH";
const GRID: &str = "EM10";

fn auto_operator(cq_after_idle_cycles: u32, switch_after: u32) -> AutonomousOperator {
    let config = AutonomousConfig {
        enabled: true,
        slot_parity: SlotParityConfig::Even,
        cq_after_idle_cycles,
        cq_no_response_switch_after: switch_after,
        listen_cycle: pancetta_qso::ListenCycleConfig {
            initial_interval: 1000, // never listen-jitter mid-test
            ..pancetta_qso::ListenCycleConfig::default()
        },
        ..AutonomousConfig::default()
    };

    let mut op = AutonomousOperator::new(config, US.to_string(), Some(GRID.to_string()));
    op.set_tx_freq_mode_source(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
        pancetta_core::TxFreqMode::Auto.as_u8(),
    )));
    op.update_spectral(SpectralSnapshot {
        power_bins: vec![0.0f32; 140],
        freq_min_hz: 200.0,
        freq_max_hz: 2800.0,
    });
    op
}

fn hold_operator(cq_after_idle_cycles: u32, switch_after: u32) -> AutonomousOperator {
    let config = AutonomousConfig {
        enabled: true,
        slot_parity: SlotParityConfig::Even,
        cq_after_idle_cycles,
        cq_no_response_switch_after: switch_after,
        listen_cycle: pancetta_qso::ListenCycleConfig {
            initial_interval: 1000,
            ..pancetta_qso::ListenCycleConfig::default()
        },
        ..AutonomousConfig::default()
    };
    // Default TxFreqMode is Hold — no set_tx_freq_mode_source call.
    AutonomousOperator::new(config, US.to_string(), Some(GRID.to_string()))
}

// S1 — pure silence in Auto mode: after enough self-CQs with no reply, the
// operator switches to a different TX frequency for a subsequent CQ.
#[tokio::test]
async fn s1_auto_mode_switches_frequency_after_silent_cq_streak() {
    let op = auto_operator(2, 3);
    let mut sim = Sim::new(US, Some(GRID)).await.with_autonomous(op);

    // Enough silent ticks to fire well past 3 no-response CQ rounds
    // (cq_after_idle_cycles=2 means one idle tick + one CQ tick per round).
    sim.tick_n(20).await;

    let tl = sim.into_timeline();
    let cq_freqs: Vec<f64> = tl
        .transmissions
        .iter()
        .filter(|t| t.text.starts_with("CQ"))
        .map(|t| t.freq_hz)
        .collect();

    assert!(
        cq_freqs.len() >= 4,
        "expected at least 4 self-CQ transmissions in 20 silent ticks, got {}\n{tl}",
        cq_freqs.len()
    );
    let first = cq_freqs[0];
    assert!(
        cq_freqs.iter().any(|&f| (f - first).abs() >= 75.0),
        "expected at least one CQ on a different frequency after the no-response streak\n{tl}"
    );
}

// S2 — same silence, Hold mode: frequency never changes across any number
// of self-CQs.
#[tokio::test]
async fn s2_hold_mode_never_switches_despite_silent_cq_streak() {
    let op = hold_operator(2, 2); // low threshold — would trip almost immediately in Auto
    let mut sim = Sim::new(US, Some(GRID)).await.with_autonomous(op);

    sim.tick_n(20).await;

    let tl = sim.into_timeline();
    let cq_freqs: Vec<f64> = tl
        .transmissions
        .iter()
        .filter(|t| t.text.starts_with("CQ"))
        .map(|t| t.freq_hz)
        .collect();
    assert!(cq_freqs.len() >= 2, "expected repeated self-CQs\n{tl}");
    let first = cq_freqs[0];
    assert!(
        cq_freqs.iter().all(|&f| (f - first).abs() < 1.0),
        "Hold mode must transmit every CQ on the same frequency\n{tl}"
    );
}

// S3 — a genuine decoded reply to our CQ, arriving before the no-response
// threshold trips, must reset the streak (not just the higher, easier-to-
// fake `active_qso_count` signal our own pending self-CQ can also set).
// switch_after is deliberately high relative to the 20-tick budget so that
// ANY switch observed can only be explained by the reset failing to hold.
#[tokio::test]
async fn s3_directed_reply_resets_streak_and_prevents_switch() {
    let op = auto_operator(2, 10);
    let mut sim = Sim::new(US, Some(GRID)).await.with_autonomous(op);

    // Let one no-response self-CQ round happen first.
    sim.tick_n(4).await;
    let after_first_cq = sim
        .timeline()
        .transmissions
        .iter()
        .filter(|t| t.text.starts_with("CQ"))
        .count();
    assert!(after_first_cq >= 1, "expected at least one self-CQ by tick 4");

    // A station answers our CQ: standard "<us> <them> <report>" reply.
    // Injected far from our CQ frequency (1500 Hz) so it doesn't itself
    // pollute DecodeHistory's occupancy scoring near our TX offset — this
    // test is about the streak reset, not the allocator's independent
    // (and entirely legitimate) tendency to avoid recently-active spots.
    sim.inject_decode("K5ARH K9ZZ -05", 2200.0, -8.0, 0.1);
    sim.tick().await; // deliver the injected decode

    // Continue in silence for the rest of the budget. Bounded so that even
    // a freshly-reset streak (starting from 0) cannot reach switch_after=10
    // again within the remaining ticks.
    sim.tick_n(15).await;

    let tl = sim.into_timeline();
    let cq_freqs: Vec<f64> = tl
        .transmissions
        .iter()
        .filter(|t| t.text.starts_with("CQ"))
        .map(|t| t.freq_hz)
        .collect();
    assert!(
        cq_freqs.len() >= 2,
        "expected further self-CQs after the reply\n{tl}"
    );
    let first = cq_freqs[0];
    assert!(
        cq_freqs.iter().all(|&f| (f - first).abs() < 1.0),
        "the directed reply must have reset the streak — no frequency switch expected\n{tl}"
    );
}
