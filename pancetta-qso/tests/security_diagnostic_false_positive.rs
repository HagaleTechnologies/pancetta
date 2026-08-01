//! phase-verify probe (PAN-3): does the new `qso.security` rejection-diagnostic
//! path fire on ORDINARY multi-QSO traffic?
//!
//! Pancetta explicitly supports several concurrent active QSOs. Every partner
//! reply is addressed to us, so this exercises whether QSO A's legitimate reply
//! is reported as a sender-verification rejection against QSO B.
//!
//! Run: `cargo test -p pancetta-qso --test security_diagnostic_false_positive`.

use pancetta_qso::qso_manager::{QsoManager, QsoManagerConfig};
use pancetta_qso::{
    AutoSequenceConfig, DuplicateCheckConfig, HoundRegions, MessageType, QsoEvent, TimeoutConfig,
};

const US: &str = "K5ARH";

fn manager() -> QsoManager {
    QsoManager::new(QsoManagerConfig {
        our_callsign: US.into(),
        our_grid: Some("EM10".into()),
        timeouts: TimeoutConfig::default(),
        contest_mode: None,
        auto_sequence: AutoSequenceConfig::default(),
        duplicate_checking: DuplicateCheckConfig::default(),
        hound: HoundRegions::default(),
        active_mode: "FT8".to_string(),
    })
}

/// Two concurrent QSOs on well-separated frequencies. QSO A's partner (K9ZZ)
/// sends us a perfectly legitimate signal report. Nothing about that frame is a
/// security event — but QSO B is also active, in a state whose relevance arm
/// matches `SignalReport`, and its partner is W1AW, so the frame is classified
/// against QSO B as `SenderNotPartner`.
///
// KNOWN BUG (PAN-3 phase-verify): fails today. `classify_relevance` runs for
// every active QSO with no frequency gate and no knowledge of whether the frame
// already matched a different QSO, so QSO A's legitimate partner reply is
// reported as a security rejection against QSO B. Assertion left intact.
#[ignore]
#[tokio::test]
async fn legit_partner_reply_is_not_reported_as_a_security_rejection() {
    let manager = manager();
    let qso_a = manager
        .respond_to_cq("K9ZZ".into(), 1500.0, None)
        .await
        .expect("open QSO A");
    let qso_b = manager
        .respond_to_cq("W1AW".into(), 2400.0, None)
        .await
        .expect("open QSO B");
    assert_ne!(qso_a, qso_b);

    // Drain the QSO-open events, then listen only to what the decode produces.
    let mut events = manager.subscribe();

    // K9ZZ answers OUR call, on QSO A's own frequency. Entirely legitimate.
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: US.into(),
                from_station: "K9ZZ".into(),
                report: -12,
            },
            format!("{US} K9ZZ -12"),
            1500.0,
            Some(-12.0),
        )
        .await
        .expect("process legit report");

    let mut rejections = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let QsoEvent::MessageRejected {
            qso_id,
            reason,
            from_callsign,
            to_callsign,
        } = event
        {
            rejections.push(format!(
                "qso={qso_id} reason={} from={from_callsign:?} to={to_callsign:?}",
                reason.as_str()
            ));
        }
    }

    assert!(
        rejections.is_empty(),
        "legitimate partner traffic produced {} operator-facing security \
         rejection(s) against the OTHER active QSO (qso_a={qso_a}, qso_b={qso_b}): {rejections:#?}",
        rejections.len(),
    );
}

/// Catalog scenario B1: our own partner is working a third party
/// (`W9ZZZ K9ZZ -07`). This is routine on-air behavior, not an attack.
///
// KNOWN BUG (PAN-3 phase-verify): fails today with `addressee-not-us`.
// `classify_relevance`'s `verify` closure filters only
// `SenderAndAddresseeMismatch`, so a frame FROM our partner TO a third party
// still emits an operator-facing Warn. Assertion left intact.
#[ignore]
#[tokio::test]
async fn partner_working_a_third_party_is_not_reported_as_a_security_rejection() {
    let manager = manager();
    manager
        .respond_to_cq("K9ZZ".into(), 1500.0, None)
        .await
        .expect("open QSO");
    let mut events = manager.subscribe();

    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "W9ZZZ".into(),
                from_station: "K9ZZ".into(),
                report: -7,
            },
            "W9ZZZ K9ZZ -07".into(),
            1500.0,
            Some(-7.0),
        )
        .await
        .expect("process third-party frame");

    let mut rejections = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let QsoEvent::MessageRejected { reason, .. } = event {
            rejections.push(reason.as_str());
        }
    }
    assert!(
        rejections.is_empty(),
        "our partner working someone else was flagged as a security rejection: {rejections:?}"
    );
}

/// Control: the case the feature EXISTS for. A single active QSO with K9ZZ, and
/// an impostor (NF4KE) sends us a report on that frequency. This SHOULD produce
/// exactly one operator-visible rejection.
#[tokio::test]
async fn single_qso_impostor_is_reported() {
    let manager = manager();
    manager
        .respond_to_cq("K9ZZ".into(), 1500.0, None)
        .await
        .expect("open QSO");
    let mut events = manager.subscribe();

    manager
        .process_message(
            MessageType::SignalReport {
                to_station: US.into(),
                from_station: "NF4KE".into(),
                report: -12,
            },
            format!("{US} NF4KE -12"),
            1500.0,
            Some(-12.0),
        )
        .await
        .expect("process impostor frame");

    let mut rejections = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let QsoEvent::MessageRejected { reason, .. } = event {
            rejections.push(reason.as_str());
        }
    }
    assert_eq!(
        rejections,
        vec!["sender-not-partner"],
        "the intended impostor case must still be reported exactly once"
    );
}
