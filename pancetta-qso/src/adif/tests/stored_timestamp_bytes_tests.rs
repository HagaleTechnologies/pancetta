//! PAN-441: ADIF timestamps keep their exact bytes across the chrono → jiff
//! migration. Written and passing on the chrono build first; the expected
//! bytes are literals and committed fixture files, never re-blessed.

use super::*;
use crate::contest::ledger::{ContestEvidence, LoggedContestContext, CONTEXT};
use crate::contest::scoring::ContestPeriod;
use pancetta_core::time::UtcTimestamp;

const FIXTURE: &str = include_str!("../../../tests/fixtures/pan441/qsos.adi");
const RENDERED: &str = include_str!("../../../tests/fixtures/pan441/qsos.rendered.adi");

/// The `APP_PANCETTA_CONTEST_CONTEXT` value of fixture record 5, exactly as
/// the chrono build serialized it.
const CONTEST_CONTEXT_JSON: &str = r#"{"version":1,"profile_id":"cqww-dx","operator_callsign":"K1ABC","operator_region":"MA","periods":[{"start":"2026-11-28T00:00:00Z","end":"2026-11-30T00:00:00Z"}],"evidence":{"their_region":"14","their_county":null,"our_county":null,"source":"exchange"},"contact_start":"2026-11-28T00:01:05.500Z"}"#;

fn fixture_records() -> Vec<AdifRecord> {
    let file = AdifProcessor::new()
        .parse_string(FIXTURE)
        .expect("PAN-441 fixture parses");
    assert_eq!(file.records.len(), 5, "fixture holds five records");
    file.records
}

fn round_trip(processor: &AdifProcessor, record: &AdifRecord) -> AdifRecord {
    let qso = processor
        .record_to_qso(record)
        .expect("fixture record converts");
    processor.qso_to_record(&qso)
}

fn value<'a>(record: &'a AdifRecord, name: &str) -> Option<&'a str> {
    record.fields.get(name).map(|field| field.value.as_str())
}

fn instant(secs: i64, nanos: u32) -> UtcTimestamp {
    UtcTimestamp::from_parts(secs, nanos).expect("in range")
}

#[test]
fn adif_fixture_round_trips_byte_for_byte() {
    let processor = AdifProcessor::new();
    let records = fixture_records();
    let round_tripped: Vec<AdifRecord> = records
        .iter()
        .map(|record| round_trip(&processor, record))
        .collect();

    let rendered: String = round_tripped
        .iter()
        .map(|record| processor.render_record(record).expect("renders"))
        .collect();
    assert_eq!(rendered, RENDERED, "rendered ADIF bytes changed");

    let date_time: Vec<(Option<&str>, Option<&str>)> = round_tripped
        .iter()
        .map(|record| (value(record, "QSO_DATE"), value(record, "TIME_ON")))
        .collect();
    assert_eq!(
        date_time,
        vec![
            (Some("20240229"), Some("120000")),
            (Some("20261104"), Some("143000")),
            (Some("19650101"), Some("000000")),
            (Some("20261104"), Some("235959")),
            (Some("20261128"), Some("000105")),
        ]
    );

    assert_eq!(value(&round_tripped[0], "QSO_DATE_OFF"), Some("20240229"));
    assert_eq!(value(&round_tripped[0], "TIME_OFF"), Some("120115"));
    assert_eq!(value(&round_tripped[3], "QSO_DATE_OFF"), None);
    assert_eq!(value(&round_tripped[3], "TIME_OFF"), Some("000014"));

    let context = value(&round_tripped[4], CONTEXT).expect("contest context kept");
    assert_eq!(context, CONTEST_CONTEXT_JSON);
    assert_eq!(context.len(), 295);
    assert!(context.contains("2026-11-28T00:01:05.500Z"));
    assert!(RENDERED.contains(&format!("<{CONTEXT}:295>{CONTEST_CONTEXT_JSON} ")));
}

/// The fixture's contest context is the JSON the logger writes for this
/// value, so the fixture is real logger output, not a hand-made string.
#[test]
fn adif_contest_context_fixture_is_logger_output() {
    let context = LoggedContestContext {
        version: 1,
        profile_id: "cqww-dx".into(),
        operator_callsign: "K1ABC".into(),
        operator_region: "MA".into(),
        periods: vec![ContestPeriod {
            start: instant(1_795_824_000, 0),
            end: instant(1_795_996_800, 0),
        }],
        evidence: ContestEvidence {
            their_region: Some("14".into()),
            their_county: None,
            our_county: None,
            source: Some("exchange".into()),
        },
        contact_start: Some(instant(1_795_824_065, 500_000_000)),
    };
    assert_eq!(
        serde_json::to_string(&context).unwrap(),
        CONTEST_CONTEXT_JSON
    );
    let read: LoggedContestContext = serde_json::from_str(CONTEST_CONTEXT_JSON).unwrap();
    assert_eq!(read, context);
}

#[test]
fn adif_qso_json_keeps_chrono_form() {
    let processor = AdifProcessor::new();
    let records = fixture_records();
    let qso = processor.record_to_qso(&records[0]).unwrap();
    let json = serde_json::to_value(&qso).unwrap();
    assert_eq!(json["qso_date"], "2024-02-29T12:00:00Z");
    assert_eq!(json["qso_date_off"], "2024-02-29T12:01:15Z");

    let pre_epoch = processor.record_to_qso(&records[2]).unwrap();
    assert_eq!(
        serde_json::to_value(&pre_epoch).unwrap()["qso_date"],
        "1965-01-01T00:00:00Z"
    );
}

fn single_record(body: &str) -> AdifRecord {
    let mut file = AdifProcessor::new()
        .parse_string(&format!("<ADIF_VER:5>3.1.4 <EOH>\n{body} <EOR>\n"))
        .expect("parses");
    file.records.remove(0)
}

#[test]
fn adif_leap_second_reads_as_59() {
    let processor = AdifProcessor::new();
    let record = single_record(
        "<CALL:5>K1ABC <QSO_DATE:8>20261231 <TIME_ON:6>235960 <MODE:3>FT8 <BAND:3>20M",
    );
    let round_tripped = round_trip(&processor, &record);
    assert_eq!(value(&round_tripped, "TIME_ON"), Some("235959"));
}

#[test]
fn adif_year_9999_is_rejected() {
    let processor = AdifProcessor::new();
    let record = single_record(
        "<CALL:5>K1ABC <QSO_DATE:8>99991231 <TIME_ON:6>120000 <MODE:3>FT8 <BAND:3>20M",
    );
    assert!(matches!(
        processor.record_to_qso(&record),
        Err(AdifError::InvalidDateTime { .. })
    ));
}
