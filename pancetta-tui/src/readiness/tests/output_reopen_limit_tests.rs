//! PAN-295: the audio blocker names the spent output reopen budget, and the
//! limit never changes the TX verdict.
use super::tests::{app, health, labels, snapshot, visible_labels};
use super::*;
use pancetta_core::time::UtcTimestamp;

fn pan295_decode() -> crate::app::DecodedMessageView {
    crate::app::DecodedMessageView {
        timestamp: UtcTimestamp::now(),
        frequency: 14_074_000.0,
        mode: "FT8".into(),
        snr: -10,
        delta_time: 0.0,
        delta_freq: 1500.0,
        call_sign: Some("K1ABC".into()),
        grid_square: None,
        message: "CQ K1ABC FN42".into(),
        distance: None,
        bearing: None,
        slot_parity: None,
        is_directed_at_us: false,
        worked_before: false,
        needed: false,
        atno: false,
        band_needed: false,
        priority_score: None,
        priority_factors: None,
        is_own_tx: false,
        external: false,
    }
}

const PAN295_LIMIT: &str = "⚠ TX AUDIO AUTO-REOPEN LIMIT: press d";
const PAN295_DEAD: &str = "⚠ TX AUDIO OUTPUT DEAD — press d";

#[tokio::test]
async fn pan295_exhaustion_label_persists_until_resolved() {
    let mut app = app().await;
    app.pipeline_health = Some(health(true, None));
    app.station_readiness = Some(StationReadiness {
        tx_mute_causes: vec![TxMuteCause::AudioOutputDead],
        audio_output_reopen_exhausted: true,
        clock: ClockState::Unreachable,
        ..StationReadiness::default()
    });
    let item = readiness_items(&app)
        .into_iter()
        .find(|i| i.key == ReadinessKey::TxAudioDead)
        .unwrap();
    assert_eq!(item.label, PAN295_LIMIT);
    assert_eq!(item.detail, "Automatic output reopen paused after 3 attempts in 10 minutes. TX stays muted until output callbacks resume. Press d to pick a device.");
    assert_eq!(item.severity, Severity::Blocker);
    assert!(!item.is_acknowledgeable());
    assert!(!labels(&app).contains(&PAN295_DEAD.to_string()));
    // Shown beside a simultaneous clock warning.
    assert!(labels(&app).contains(&"⚠ CLOCK UNVERIFIED".to_string()));

    // Acknowledging warnings, a status write, a decode and repeated
    // renders leave the blocker visible.
    assert_eq!(app.acknowledge_readiness(), 1, "only the clock warning");
    app.set_relay_status(
        "Audio device reopened".into(),
        pancetta_core::DiagnosticLevel::Info,
    );
    app.add_decoded_message(pan295_decode()).await.unwrap();
    for _ in 0..3 {
        assert_eq!(visible_labels(&app), vec![PAN295_LIMIT.to_string()]);
    }

    // Budget expiry: the limit clears, the dead blocker stays.
    app.station_readiness
        .as_mut()
        .unwrap()
        .audio_output_reopen_exhausted = false;
    assert_eq!(visible_labels(&app), vec![PAN295_DEAD.to_string()]);

    // The flag alone (no AudioOutputDead) renders nothing.
    app.station_readiness = Some(StationReadiness {
        audio_output_reopen_exhausted: true,
        ..StationReadiness::default()
    });
    assert!(readiness_items(&app).is_empty());

    // Callbacks resumed: neither label.
    app.station_readiness = Some(StationReadiness::default());
    assert!(readiness_items(&app).is_empty());
}

#[tokio::test]
async fn pan295_exhaustion_does_not_change_tx_verdict() {
    let mut app = app().await;
    app.pipeline_health = Some(health(true, None));
    for causes in [
        vec![],
        vec![TxMuteCause::AudioOutputDead],
        vec![TxMuteCause::AudioOutputDead, TxMuteCause::PolicyDisabled],
    ] {
        app.station_readiness = Some(snapshot(&causes));
        let plain = verdict(&app);
        app.station_readiness
            .as_mut()
            .unwrap()
            .audio_output_reopen_exhausted = true;
        assert_eq!(verdict(&app), plain, "{causes:?}");
    }
    app.station_readiness = Some(StationReadiness {
        tx_mute_causes: vec![TxMuteCause::AudioOutputDead],
        audio_output_reopen_exhausted: true,
        ..StationReadiness::default()
    });
    assert_eq!(verdict(&app), Verdict::Held);
}
