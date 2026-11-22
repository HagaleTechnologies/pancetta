//! PAN-359: activation references reach the logged ADIF record.
//!
//! Each test builds a `QsoManager` (our call K5ARH, partner DL1ABC, dial
//! 14 074 000 Hz), completes a QSO through one of the two `QsoCompleted`
//! producers, appends the emitted metadata with `AdifLogWriter` the way the
//! coordinator does, parses `qsos.adi` back and asserts on the record.

use chrono::{Duration, Utc};
use pancetta_core::slot::SlotParity;
use pancetta_core::ResponseStep;
use pancetta_qso::{
    ActivationReference, AdifLogWriter, AdifProcessor, CompletionAired, MessageType, QsoEvent,
    QsoId, QsoManager, QsoManagerConfig, QsoMetadata, UnsentAttemptPayload, WorkedReference,
    WorkedReferenceBook,
};
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::broadcast;

const OUR: &str = "K5ARH";
const DX: &str = "DL1ABC";
const FREQ: f64 = 1500.0;
const SIG_FIELDS: [&str; 4] = ["MY_SIG", "MY_SIG_INFO", "SIG", "SIG_INFO"];

fn manager(activation: ActivationReference, book: Option<WorkedReferenceBook>) -> QsoManager {
    let mut manager = QsoManager::new(QsoManagerConfig {
        our_callsign: OUR.into(),
        our_grid: Some("EM12".into()),
        activation,
        ..QsoManagerConfig::default()
    });
    manager.set_dial_frequency_source(Arc::new(AtomicU64::new(14_074_000)));
    if let Some(book) = book {
        manager.set_worked_reference_source(book);
    }
    manager
}

async fn reply(manager: &QsoManager, step: ResponseStep) -> (QsoId, Option<u64>) {
    let (id, outcome) = manager
        .respond_to_caller_with_outcome(
            DX.into(),
            FREQ,
            Some(SlotParity::Even),
            step,
            Some(-8.0),
            Some(-4),
            None,
            false,
            None,
        )
        .await
        .expect("reply");
    (id, outcome.attempt_id().map(|a| a.0))
}

fn completed(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoMetadata> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let QsoEvent::QsoCompleted { metadata, .. } = ev {
            out.push(metadata);
        }
    }
    out
}

/// Held path: our 73 is queued, then the TX worker reports it aired.
async fn complete_held(manager: &QsoManager) -> QsoMetadata {
    let (id, _) = reply(manager, ResponseStep::ReportAck).await;
    let mut rx = manager.subscribe();
    let (_, attempt) = reply(manager, ResponseStep::SeventyThree).await;
    assert!(completed(&mut rx).is_empty(), "held until the 73 airs");
    let aired = manager
        .confirm_completion_aired(
            id,
            attempt.map(pancetta_qso::TxAttemptId),
            "",
            &UnsentAttemptPayload {
                frequency_hz: FREQ,
                remote_binding: None,
            },
            Utc::now(),
        )
        .await;
    assert_eq!(aired, CompletionAired::Committed);
    let mut logged = completed(&mut rx);
    assert_eq!(logged.len(), 1);
    logged.remove(0)
}

/// Immediate path: we sent RR73, the partner's bare 73 closes the QSO and
/// no closing frame is queued.
async fn complete_immediate(manager: &QsoManager) -> QsoMetadata {
    reply(manager, ResponseStep::Rr73).await;
    let mut rx = manager.subscribe();
    manager
        .process_message(
            MessageType::SeventyThree {
                to_station: OUR.into(),
                from_station: DX.into(),
            },
            format!("{OUR} {DX} 73"),
            FREQ,
            Some(-15.0),
        )
        .await
        .expect("73");
    let mut logged = completed(&mut rx);
    assert_eq!(logged.len(), 1, "a frame-less completion logs at once");
    logged.remove(0)
}

/// Append `metadata` to a fresh `qsos.adi` and return the parsed record's
/// fields, name to value.
async fn logged_fields(metadata: &QsoMetadata) -> HashMap<String, String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("qsos.adi");
    let writer = AdifLogWriter::open(&path).await.expect("open");
    writer.append(metadata).await.expect("append");
    let text = std::fs::read_to_string(&path).expect("read");
    let file = AdifProcessor::new().parse_string(&text).expect("parse");
    assert_eq!(file.records.len(), 1, "{text}");
    file.records[0]
        .fields
        .iter()
        .map(|(k, f)| (k.to_ascii_uppercase(), f.value.clone()))
        .collect()
}

fn field<'a>(fields: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    fields.get(name).map(String::as_str)
}

fn pota_book(seen_minutes_ago: i64) -> WorkedReferenceBook {
    let book = WorkedReferenceBook::new();
    book.record_feed(
        [(
            DX.to_string(),
            WorkedReference {
                sig: "POTA".into(),
                sig_info: "DA-0123".into(),
            },
        )],
        Utc::now() - Duration::minutes(seen_minutes_ago),
    );
    book
}

/// PAN-359 AC1 (scenario 1): the held-completion path writes the
/// operator's own MY_SIG / MY_SIG_INFO.
#[tokio::test]
async fn held_completion_carries_my_sig_and_my_sig_info() {
    let manager = manager(
        ActivationReference::new(Some("POTA"), Some("K-1234")),
        None,
    );
    let metadata = complete_held(&manager).await;
    let fields = logged_fields(&metadata).await;
    assert_eq!(field(&fields, "CALL"), Some(DX));
    assert_eq!(field(&fields, "MY_SIG"), Some("POTA"));
    assert_eq!(field(&fields, "MY_SIG_INFO"), Some("K-1234"));
    assert_eq!(field(&fields, "SIG"), None);
    assert_eq!(field(&fields, "SIG_INFO"), None);
}

/// PAN-359 AC2: the immediate completion path writes them too.
#[tokio::test]
async fn immediate_completion_carries_my_sig_and_my_sig_info() {
    let manager = manager(
        ActivationReference::new(Some("POTA"), Some("K-1234")),
        None,
    );
    let metadata = complete_immediate(&manager).await;
    let fields = logged_fields(&metadata).await;
    assert_eq!(field(&fields, "CALL"), Some(DX));
    assert_eq!(field(&fields, "MY_SIG"), Some("POTA"));
    assert_eq!(field(&fields, "MY_SIG_INFO"), Some("K-1234"));
}

/// PAN-359 AC3: SOTA is stamped the same way.
#[tokio::test]
async fn sota_activation_is_stamped_the_same_way() {
    let manager = manager(
        ActivationReference::new(Some("SOTA"), Some("W2/WE-003")),
        None,
    );
    let metadata = complete_held(&manager).await;
    let fields = logged_fields(&metadata).await;
    assert_eq!(field(&fields, "MY_SIG"), Some("SOTA"));
    assert_eq!(field(&fields, "MY_SIG_INFO"), Some("W2/WE-003"));
}

/// PAN-359 AC4: unset, empty or blank keys and an empty book write none of
/// the four fields; only `my_sig` set writes only MY_SIG; values are
/// trimmed and keep their case.
#[tokio::test]
async fn unset_or_blank_activation_writes_no_sig_fields() {
    for activation in [
        ActivationReference::default(),
        ActivationReference::new(Some(""), Some("")),
        ActivationReference::new(Some("   "), Some("\t")),
        ActivationReference {
            my_sig: Some("  ".into()),
            my_sig_info: Some(String::new()),
        },
    ] {
        let manager = manager(activation.clone(), Some(WorkedReferenceBook::new()));
        let metadata = complete_held(&manager).await;
        let fields = logged_fields(&metadata).await;
        assert_eq!(field(&fields, "CALL"), Some(DX));
        for name in SIG_FIELDS {
            assert!(
                !fields.contains_key(name),
                "{activation:?} wrote {name}: {fields:?}"
            );
        }
    }

    let manager = manager(ActivationReference::new(Some("  pota "), None), None);
    let metadata = complete_immediate(&manager).await;
    let fields = logged_fields(&metadata).await;
    assert_eq!(field(&fields, "MY_SIG"), Some("pota"));
    for name in ["MY_SIG_INFO", "SIG", "SIG_INFO"] {
        assert!(!fields.contains_key(name), "wrote {name}: {fields:?}");
    }
}

/// PAN-359 AC7 (scenario 2): a worked activator the book listed within
/// 30 minutes of the QSO's end is written as SIG / SIG_INFO, alongside the
/// operator's own reference.
#[tokio::test]
async fn worked_activator_reference_is_written_as_sig_and_sig_info() {
    let manager = manager(
        ActivationReference::new(Some("POTA"), Some("K-1234")),
        Some(pota_book(29)),
    );
    let metadata = complete_held(&manager).await;
    let fields = logged_fields(&metadata).await;
    assert_eq!(field(&fields, "CALL"), Some(DX));
    assert_eq!(field(&fields, "SIG"), Some("POTA"));
    assert_eq!(field(&fields, "SIG_INFO"), Some("DA-0123"));
    assert_eq!(field(&fields, "MY_SIG"), Some("POTA"));
    assert_eq!(field(&fields, "MY_SIG_INFO"), Some("K-1234"));

    // The immediate path reads the same book.
    let manager = manager(ActivationReference::default(), Some(pota_book(0)));
    let metadata = complete_immediate(&manager).await;
    let fields = logged_fields(&metadata).await;
    assert_eq!(field(&fields, "SIG"), Some("POTA"));
    assert_eq!(field(&fields, "SIG_INFO"), Some("DA-0123"));
    assert_eq!(field(&fields, "MY_SIG"), None);
}

/// PAN-359 AC8: no SIG / SIG_INFO without a fresh entry for the partner.
#[tokio::test]
async fn no_sig_without_a_fresh_matching_activation() {
    // An entry for another station only.
    let other = WorkedReferenceBook::new();
    other.record_feed(
        [(
            "K1XYZ".to_string(),
            WorkedReference {
                sig: "POTA".into(),
                sig_info: "K-0001".into(),
            },
        )],
        Utc::now(),
    );
    // Last seen 31 minutes before the QSO ends.
    for book in [other, pota_book(31)] {
        let manager = manager(ActivationReference::default(), Some(book));
        let metadata = complete_held(&manager).await;
        let fields = logged_fields(&metadata).await;
        assert_eq!(field(&fields, "CALL"), Some(DX));
        for name in SIG_FIELDS {
            assert!(!fields.contains_key(name), "wrote {name}: {fields:?}");
        }
    }
}
