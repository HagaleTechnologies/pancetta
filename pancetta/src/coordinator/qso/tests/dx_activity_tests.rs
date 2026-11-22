use super::dx_activity_summary;
use pancetta_qso::states::MessageType as Mt;

const OUR: &str = "K5ARH";

#[test]
fn cq_summarizes_as_calling_cq() {
    let m = Mt::Cq {
        callsign: "JA1ABC".to_string(),
        grid: None,
    };
    let (from, s) = dx_activity_summary(&m, OUR).unwrap();
    assert_eq!(from, "JA1ABC");
    assert_eq!(s, "calling CQ");
}

#[test]
fn report_to_third_party_names_them() {
    let m = Mt::SignalReport {
        to_station: "W1XYZ".to_string(),
        from_station: "JA1ABC".to_string(),
        report: -12,
    };
    let (from, s) = dx_activity_summary(&m, OUR).unwrap();
    assert_eq!(from, "JA1ABC");
    assert_eq!(s, "→ W1XYZ -12");
}

#[test]
fn report_to_us_says_us() {
    let m = Mt::ReportAck {
        to_station: OUR.to_string(),
        from_station: "JA1ABC".to_string(),
        report: 3,
    };
    let (from, s) = dx_activity_summary(&m, OUR).unwrap();
    assert_eq!(from, "JA1ABC");
    assert_eq!(s, "→ us R+3");
}

#[test]
fn nonstandard_has_no_summary() {
    let m = Mt::NonStandard {
        text: "blah".to_string(),
    };
    assert!(dx_activity_summary(&m, OUR).is_none());
}
