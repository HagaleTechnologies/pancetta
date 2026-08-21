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
