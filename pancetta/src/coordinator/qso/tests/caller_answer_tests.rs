use super::{classify_caller_answer, CallerAnswer};
use pancetta_core::ResponseStep;
use pancetta_qso::states::MessageType as Mt;

const OUR: &str = "K5ARH";

#[test]
fn cqresponse_to_us_opens_at_report() {
    let m = Mt::CqResponse {
        calling_station: OUR.to_string(),
        responding_station: "JA1ABC".to_string(),
        grid: None,
    };
    assert_eq!(
        classify_caller_answer(&m, OUR),
        Some(CallerAnswer {
            their_call: "JA1ABC".to_string(),
            step: ResponseStep::Report,
            their_report: None,
        })
    );
}

#[test]
fn signal_report_to_us_opens_at_reportack_with_report() {
    let m = Mt::SignalReport {
        to_station: OUR.to_string(),
        from_station: "JA1ABC".to_string(),
        report: -12,
    };
    assert_eq!(
        classify_caller_answer(&m, OUR),
        Some(CallerAnswer {
            their_call: "JA1ABC".to_string(),
            step: ResponseStep::ReportAck,
            their_report: Some(-12),
        })
    );
}

#[test]
fn reportack_to_us_opens_at_rr73() {
    let m = Mt::ReportAck {
        to_station: OUR.to_string(),
        from_station: "JA1ABC".to_string(),
        report: -3,
    };
    let a = classify_caller_answer(&m, OUR).unwrap();
    assert_eq!(a.step, ResponseStep::Rr73);
    assert_eq!(a.their_report, Some(-3));
}

#[test]
fn final_confirmation_to_us_opens_at_73() {
    let m = Mt::FinalConfirmation {
        to_station: OUR.to_string(),
        from_station: "JA1ABC".to_string(),
    };
    assert_eq!(
        classify_caller_answer(&m, OUR).map(|a| a.step),
        Some(ResponseStep::SeventyThree)
    );
}

#[test]
fn compound_call_to_us_is_recognized() {
    // Their frame addresses our base call from a compound call.
    let m = Mt::SignalReport {
        to_station: OUR.to_string(),
        from_station: "EA8/G8BCG".to_string(),
        report: -7,
    };
    assert_eq!(
        classify_caller_answer(&m, OUR).map(|a| a.their_call),
        Some("EA8/G8BCG".to_string())
    );
}

#[test]
fn message_to_someone_else_is_ignored() {
    let m = Mt::SignalReport {
        to_station: "W1XYZ".to_string(),
        from_station: "JA1ABC".to_string(),
        report: -12,
    };
    assert_eq!(classify_caller_answer(&m, OUR), None);
}

#[test]
fn cq_and_seventythree_are_not_caller_answers() {
    // A CQ is an initiation, not a direct call to us.
    let cq = Mt::Cq {
        callsign: "JA1ABC".to_string(),
        grid: None,
    };
    assert_eq!(classify_caller_answer(&cq, OUR), None);
    // A 73 to us needs no reply (the QSO is closing).
    let seventythree = Mt::SeventyThree {
        to_station: OUR.to_string(),
        from_station: "JA1ABC".to_string(),
    };
    assert_eq!(classify_caller_answer(&seventythree, OUR), None);
}
