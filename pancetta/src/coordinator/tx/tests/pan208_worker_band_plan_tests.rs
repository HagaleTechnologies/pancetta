use super::pan115_worker_gate_tests::{
    await_completion, await_ptt_on, request, worker_harness_with_config, Harness,
};
use super::pan208_band_plan_gap_tests::gapped_plan;
use pancetta_config::Config;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// The worker snapshots the band plan at spawn, so it goes in the config.
async fn gapped_plan_harness() -> Harness {
    let mut cfg = Config::default();
    cfg.rig.frequency.band_plan = gapped_plan();
    worker_harness_with_config(cfg).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gap_straddling_custom_plan_tx_is_refused_before_ptt() {
    let h = gapped_plan_harness().await;
    // Dial + 1,500 Hz audio offset = base tone 14,070,990 Hz; the FT8
    // top tone lands at 14,071,033.75 Hz, across the excluded gap.
    h.coordinator
        .operating_frequency_hz
        .store(14_069_490, Ordering::Relaxed);

    request(&h, "CQ N0CALL FN20").await;
    let (completed, diagnostics, ptt_on) =
        await_completion(&h, "CQ N0CALL FN20", Duration::from_secs(5)).await;
    assert_eq!(
        completed,
        Some(false),
        "a gap-straddling TX must complete as failed"
    );
    assert!(
        !ptt_on,
        "no SetPtt{{true}} may reach Hamlib for a gap-straddling TX"
    );
    assert!(
        diagnostics.iter().any(|d| d.contains(
            "TX blocked (TX signal 14070990-14071034 Hz extends outside the configured amateur band plan)"
        )),
        "the admission gate must name the occupied range, got {diagnostics:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fully_inside_custom_plan_tx_still_keys() {
    let h = gapped_plan_harness().await;
    // Base tone 14,071,100 Hz; top tone 14,071,143.75 Hz — one range.
    h.coordinator
        .operating_frequency_hz
        .store(14_069_600, Ordering::Relaxed);

    request(&h, "CQ N0CALL FN20").await;
    assert!(
        await_ptt_on(&h, Duration::from_secs(40)).await,
        "a TX wholly inside one custom range must still key"
    );
}
