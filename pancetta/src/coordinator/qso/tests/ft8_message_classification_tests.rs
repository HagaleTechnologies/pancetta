use super::*;

fn base_message() -> pancetta_ft8::Ft8Message {
    pancetta_ft8::Ft8Message::default()
}

/// PR #344 round-2 Codex P2: a native i3=1/2 decode with an UNRESOLVED
/// 12-bit hash callsign (unpack28 renders "<...N>", literal dots --
/// distinct from a RESOLVED render like "<K5ARH>") must not be routed
/// as a real caller. `standard_type` is `Some(Report)` here (the
/// native path classified the SHAPE correctly), but the callsign
/// itself is unroutable -- this must fall through to the same
/// validated-text-parser path as `None`, which correctly rejects it.
#[test]
fn unresolved_hash_from_callsign_falls_back_to_non_standard() {
    let msg = pancetta_ft8::Ft8Message {
        standard_type: Some(pancetta_ft8::message::StandardMessageType::Report),
        to_callsign: Some("K5ARH".to_string()),
        from_callsign: Some("<...123>".to_string()),
        signal_report: Some(-10),
        ..base_message()
    };
    let result = ft8_message_to_qso_type(&msg, "K5ARH <...123> -10", "K5ARH");
    assert_eq!(
        result,
        pancetta_qso::states::MessageType::NonStandard {
            text: "K5ARH <...123> -10".to_string(),
        }
    );
}

fn dxpedition_reference_message(fox_call: Option<&str>) -> pancetta_ft8::Ft8Message {
    let contacts = pancetta_ft8::message::DxpeditionContacts {
        rr73_to: "K1ABC".to_string(),
        report_to: "W9XYZ".to_string(),
        fox_hash10: 0,
        fox_call: fox_call.map(str::to_string),
        report_db: -8,
    };
    pancetta_ft8::Ft8Message {
        from_callsign: fox_call.map(str::to_string),
        signal_report: Some(-8),
        text: Some(contacts.text()),
        dxpedition: Some(contacts),
        ..base_message()
    }
}

/// PAN-259 scenario 3: one resolved type 0.1 frame yields RR73 to its
/// first Hound and the -08 report to its second, both from the Fox.
#[test]
fn dxpedition_frame_yields_both_contacts_from_the_fox() {
    let msg = dxpedition_reference_message(Some("KH1/KH7Z"));
    let text = msg.text.clone().unwrap_or_default();
    assert_eq!(text, "K1ABC RR73; W9XYZ <KH1/KH7Z> -08");
    let result = ft8_message_to_qso_types(&msg, &text, "W9XYZ");
    assert_eq!(
        result,
        vec![
            pancetta_qso::states::MessageType::FinalConfirmation {
                to_station: "K1ABC".to_string(),
                from_station: "KH1/KH7Z".to_string(),
            },
            pancetta_qso::states::MessageType::SignalReport {
                to_station: "W9XYZ".to_string(),
                from_station: "KH1/KH7Z".to_string(),
                report: -8,
            },
        ]
    );
}

/// PAN-259: an unknown or collided Fox hash yields no QSO contact, never
/// a text-parsed sender.
#[test]
fn dxpedition_frame_with_unresolved_fox_yields_no_contact() {
    let msg = dxpedition_reference_message(None);
    let text = msg.text.clone().unwrap_or_default();
    assert_eq!(text, "K1ABC RR73; W9XYZ <...> -08");
    assert!(ft8_message_to_qso_types(&msg, &text, "K1ABC").is_empty());
}

/// PAN-259: ordinary traffic still goes through the single-contact path.
#[test]
fn dxpedition_plural_helper_delegates_ordinary_frames() {
    let msg = pancetta_ft8::Ft8Message {
        standard_type: Some(pancetta_ft8::message::StandardMessageType::Report),
        to_callsign: Some("K5ARH".to_string()),
        from_callsign: Some("W1ABC".to_string()),
        signal_report: Some(-10),
        ..base_message()
    };
    assert_eq!(
        ft8_message_to_qso_types(&msg, "K5ARH W1ABC -10", "K5ARH"),
        vec![ft8_message_to_qso_type(&msg, "K5ARH W1ABC -10", "K5ARH")]
    );
}

#[test]
fn unresolved_hash_to_callsign_falls_back_to_non_standard() {
    let msg = pancetta_ft8::Ft8Message {
        standard_type: Some(pancetta_ft8::message::StandardMessageType::Cq),
        from_callsign: Some("<...456>".to_string()),
        ..base_message()
    };
    let result = ft8_message_to_qso_type(&msg, "CQ <...456>", "K5ARH");
    assert_eq!(
        result,
        pancetta_qso::states::MessageType::NonStandard {
            text: "CQ <...456>".to_string(),
        }
    );
}

/// Companion: a RESOLVED hash render ("<K5ARH>", no dots) is a real,
/// routable callsign (PAN-17) and must NOT hit the unresolved-hash
/// guard -- it takes the ordinary typed-field path like any other
/// callsign.
#[test]
fn resolved_hash_callsign_is_not_treated_as_unresolved() {
    let msg = pancetta_ft8::Ft8Message {
        standard_type: Some(pancetta_ft8::message::StandardMessageType::Report),
        to_callsign: Some("<K5ARH>".to_string()),
        from_callsign: Some("W1ABC".to_string()),
        signal_report: Some(-10),
        ..base_message()
    };
    let result = ft8_message_to_qso_type(&msg, "<K5ARH> W1ABC -10", "K5ARH");
    assert_eq!(
        result,
        pancetta_qso::states::MessageType::SignalReport {
            to_station: "<K5ARH>".to_string(),
            from_station: "W1ABC".to_string(),
            report: -10,
        }
    );
}

#[test]
fn cq_maps_to_cq() {
    let msg = pancetta_ft8::Ft8Message {
        standard_type: Some(pancetta_ft8::message::StandardMessageType::Cq),
        from_callsign: Some("W1ABC".to_string()),
        grid_square: Some("FN42".to_string()),
        ..base_message()
    };
    let result = ft8_message_to_qso_type(&msg, "CQ W1ABC FN42", "K5ARH");
    assert_eq!(
        result,
        pancetta_qso::states::MessageType::Cq {
            callsign: "W1ABC".to_string(),
            grid: Some("FN42".to_string()),
        }
    );
}

#[test]
fn reply_maps_to_cq_response() {
    let msg = pancetta_ft8::Ft8Message {
        standard_type: Some(pancetta_ft8::message::StandardMessageType::Reply),
        to_callsign: Some("W1ABC".to_string()),
        from_callsign: Some("K1DEF".to_string()),
        grid_square: Some("FN31".to_string()),
        ..base_message()
    };
    let result = ft8_message_to_qso_type(&msg, "W1ABC K1DEF FN31", "K5ARH");
    assert_eq!(
        result,
        pancetta_qso::states::MessageType::CqResponse {
            calling_station: "W1ABC".to_string(),
            responding_station: "K1DEF".to_string(),
            grid: Some("FN31".to_string()),
        }
    );
}

/// PAN-49: the structural fix. Before this design, no regex in
/// `QSO_PATTERNS` produced `ContestReply` from inbound text at all --
/// a real `ReplyWithR` decode fell through to `NonStandard` and the
/// QSO never advanced. This proves the fix is now direct and correct
/// on the first pass, not a workaround.
#[test]
fn reply_with_r_maps_to_contest_reply_pan_49_regression() {
    let msg = pancetta_ft8::Ft8Message {
        standard_type: Some(pancetta_ft8::message::StandardMessageType::ReplyWithR),
        to_callsign: Some("K1ABC".to_string()),
        from_callsign: Some("W9XYZ".to_string()),
        grid_square: Some("EN37".to_string()),
        ..base_message()
    };
    let result = ft8_message_to_qso_type(&msg, "K1ABC W9XYZ R EN37", "K5ARH");
    assert_eq!(
        result,
        pancetta_qso::states::MessageType::ContestReply {
            to_station: "K1ABC".to_string(),
            from_station: "W9XYZ".to_string(),
            grid: "EN37".to_string(),
            is_ack: true,
        }
    );
}

#[test]
fn report_maps_to_signal_report() {
    let msg = pancetta_ft8::Ft8Message {
        standard_type: Some(pancetta_ft8::message::StandardMessageType::Report),
        to_callsign: Some("K1DEF".to_string()),
        from_callsign: Some("W1ABC".to_string()),
        signal_report: Some(-15),
        ..base_message()
    };
    let result = ft8_message_to_qso_type(&msg, "K1DEF W1ABC -15", "K5ARH");
    assert_eq!(
        result,
        pancetta_qso::states::MessageType::SignalReport {
            to_station: "K1DEF".to_string(),
            from_station: "W1ABC".to_string(),
            report: -15,
        }
    );
}

#[test]
fn report_with_r_maps_to_report_ack() {
    let msg = pancetta_ft8::Ft8Message {
        standard_type: Some(pancetta_ft8::message::StandardMessageType::ReportWithR),
        to_callsign: Some("W1ABC".to_string()),
        from_callsign: Some("K1DEF".to_string()),
        signal_report: Some(-12),
        ..base_message()
    };
    let result = ft8_message_to_qso_type(&msg, "W1ABC K1DEF R-12", "K5ARH");
    assert_eq!(
        result,
        pancetta_qso::states::MessageType::ReportAck {
            to_station: "W1ABC".to_string(),
            from_station: "K1DEF".to_string(),
            report: -12,
        }
    );
}

/// Regression companion to `exchange.rs`'s own
/// `parse_message` test at line ~1148-1161: bare "RRR" (no digits,
/// syntactically distinct from a grid) must classify the same as
/// RR73, matching today's regex behavior exactly.
#[test]
fn rrr_maps_to_final_confirmation_same_as_rr73() {
    let msg = pancetta_ft8::Ft8Message {
        standard_type: Some(pancetta_ft8::message::StandardMessageType::Rrr),
        to_callsign: Some("K5ARH".to_string()),
        from_callsign: Some("K9HJZ".to_string()),
        ..base_message()
    };
    let result = ft8_message_to_qso_type(&msg, "K5ARH K9HJZ RRR", "K5ARH");
    assert_eq!(
        result,
        pancetta_qso::states::MessageType::FinalConfirmation {
            to_station: "K5ARH".to_string(),
            from_station: "K9HJZ".to_string(),
        }
    );
}

#[test]
fn rr73_maps_to_final_confirmation() {
    let msg = pancetta_ft8::Ft8Message {
        standard_type: Some(pancetta_ft8::message::StandardMessageType::RR73),
        to_callsign: Some("K1DEF".to_string()),
        from_callsign: Some("W1ABC".to_string()),
        ..base_message()
    };
    let result = ft8_message_to_qso_type(&msg, "K1DEF W1ABC RR73", "K5ARH");
    assert_eq!(
        result,
        pancetta_qso::states::MessageType::FinalConfirmation {
            to_station: "K1DEF".to_string(),
            from_station: "W1ABC".to_string(),
        }
    );
}

#[test]
fn final73_maps_to_seventy_three() {
    let msg = pancetta_ft8::Ft8Message {
        standard_type: Some(pancetta_ft8::message::StandardMessageType::Final73),
        to_callsign: Some("W1ABC".to_string()),
        from_callsign: Some("K1DEF".to_string()),
        ..base_message()
    };
    let result = ft8_message_to_qso_type(&msg, "W1ABC K1DEF 73", "K5ARH");
    assert_eq!(
        result,
        pancetta_qso::states::MessageType::SeventyThree {
            to_station: "W1ABC".to_string(),
            from_station: "K1DEF".to_string(),
        }
    );
}

/// `None` + text that genuinely parses as nothing still lands on
/// `NonStandard`. "HELLO WORLD" is two callsign-shaped TOKENS as far as
/// `QSO_PATTERNS`' character class is concerned, but `validate_callsign`
/// rejects both (no digit, not compound, not a hash render), so the text
/// fallback errors out and we collapse to the verbatim rendered text.
#[test]
fn none_standard_type_maps_to_non_standard_with_rendered_text() {
    let msg = pancetta_ft8::Ft8Message {
        standard_type: None,
        text: Some("HELLO WORLD".to_string()),
        ..base_message()
    };
    let result = ft8_message_to_qso_type(&msg, "HELLO WORLD", "K5ARH");
    assert_eq!(
        result,
        pancetta_qso::states::MessageType::NonStandard {
            text: "HELLO WORLD".to_string(),
        }
    );
}

/// The `NonStandard` fallback must use the PASSED-IN rendered text,
/// not re-derive it from `Ft8Message` -- proven by making the two
/// diverge (a real `Display` impl could never do this, but the
/// conversion function must not assume that and re-render anyway).
/// Also pins the text parser's own `NonStandard` result being
/// discarded: it uppercases and re-emits its input, which would
/// silently substitute a DIFFERENT string than what was logged.
#[test]
fn none_standard_type_uses_the_passed_in_text_not_a_re_render() {
    let msg = pancetta_ft8::Ft8Message {
        standard_type: None,
        text: Some("SOMETHING ELSE ENTIRELY".to_string()),
        ..base_message()
    };
    let result = ft8_message_to_qso_type(&msg, "EXACT LOGGED TEXT", "K5ARH");
    assert_eq!(
        result,
        pancetta_qso::states::MessageType::NonStandard {
            text: "EXACT LOGGED TEXT".to_string(),
        }
    );
}

// ====================================================================
// i3=4 NonStdCall (`standard_type == None`) -- PAN-17 regression guard.
//
// `parse_nonstd_call` (pancetta-ft8/src/message.rs) NEVER sets
// `standard_type`, so every compound/DXpedition-callsign frame arrives
// here with `None`. These are real, workable QSO messages, not exotic
// shapes: classifying them `NonStandard` makes a compound-callsign
// station's CQ, reply, and close all invisible to the QSO engine --
// exactly the "TX-only, not a working QSO" gap PAN-17 fixed.
// ====================================================================

/// i3=4 CQ. `parse_nonstd_call`'s `icq` branch sets
/// `to_callsign = Some("CQ")` and `from_callsign` to the exact 58-bit
/// base-38 compound call (never a hash -- see that branch), rendering
/// as `"CQ YS/WE9G"`.
#[test]
fn nonstd_call_cq_classifies_as_cq_not_non_standard() {
    let msg = pancetta_ft8::Ft8Message {
        message_type: pancetta_ft8::message::MessageType::NonStdCall,
        standard_type: None,
        to_callsign: Some("CQ".to_string()),
        from_callsign: Some("YS/WE9G".to_string()),
        ..base_message()
    };
    let result = ft8_message_to_qso_type(&msg, "CQ YS/WE9G", "K5ARH");
    assert_eq!(
        result,
        pancetta_qso::states::MessageType::Cq {
            callsign: "YS/WE9G".to_string(),
            grid: None,
        }
    );
}

/// i3=4 reply carrying a RESOLVED 12-bit hash render of OUR callsign.
/// The classified `to_station` must be the NORMALIZED plain call
/// (`K5ARH`), not the bracketed literal -- a bracketed callsign latched
/// into QSO state self-sabotages via the unencodable-message watchdog
/// (`<`/`>` are outside the wire charset). Mirrors
/// `exchange.rs::parse_message_routes_resolved_hash_reply_with_report`.
#[test]
fn nonstd_call_resolved_hash_reply_strips_brackets_and_classifies() {
    let msg = pancetta_ft8::Ft8Message {
        message_type: pancetta_ft8::message::MessageType::NonStdCall,
        standard_type: None,
        to_callsign: Some("<K5ARH>".to_string()),
        from_callsign: Some("YS/WE9G".to_string()),
        ..base_message()
    };
    let result = ft8_message_to_qso_type(&msg, "<K5ARH> YS/WE9G -12", "K5ARH");
    assert_eq!(
        result,
        pancetta_qso::states::MessageType::SignalReport {
            to_station: "K5ARH".to_string(),
            from_station: "YS/WE9G".to_string(),
            report: -12,
        }
    );
}

/// i3=4 close. `parse_nonstd_call` decodes the 2-bit `nrpt` field into
/// `contest_exchange` (`RRR`/`RR73`/`73`), which `Display` appends after
/// the two calls. All three must reach the QSO engine as real closes --
/// a dropped RR73 stalls the QSO one message short of completion.
#[test]
fn nonstd_call_closes_classify_as_confirmation_and_73() {
    let close = |exchange: &str, text: &str| {
        let msg = pancetta_ft8::Ft8Message {
            message_type: pancetta_ft8::message::MessageType::NonStdCall,
            standard_type: None,
            to_callsign: Some("<K5ARH>".to_string()),
            from_callsign: Some("YS/WE9G".to_string()),
            contest_exchange: Some(exchange.to_string()),
            ..base_message()
        };
        ft8_message_to_qso_type(&msg, text, "K5ARH")
    };

    assert_eq!(
        close("RR73", "<K5ARH> YS/WE9G RR73"),
        pancetta_qso::states::MessageType::FinalConfirmation {
            to_station: "K5ARH".to_string(),
            from_station: "YS/WE9G".to_string(),
        }
    );
    assert_eq!(
        close("RRR", "<K5ARH> YS/WE9G RRR"),
        pancetta_qso::states::MessageType::FinalConfirmation {
            to_station: "K5ARH".to_string(),
            from_station: "YS/WE9G".to_string(),
        }
    );
    assert_eq!(
        close("73", "<K5ARH> YS/WE9G 73"),
        pancetta_qso::states::MessageType::SeventyThree {
            to_station: "K5ARH".to_string(),
            from_station: "YS/WE9G".to_string(),
        }
    );
}

/// The UNRESOLVED hash-miss placeholder (`<...>`) must also route --
/// there is no real callsign behind it, but the surrounding message
/// shape is still a real close. `normalize_callsign_token` deliberately
/// leaves it bracketed (nothing to normalize to); higher-level sender
/// verification (`callsigns_match`) is what refuses to match it against
/// a specific station.
#[test]
fn nonstd_call_unresolved_hash_close_still_routes() {
    let msg = pancetta_ft8::Ft8Message {
        message_type: pancetta_ft8::message::MessageType::NonStdCall,
        standard_type: None,
        to_callsign: Some("<...>".to_string()),
        from_callsign: Some("YS/WE9G".to_string()),
        contest_exchange: Some("RR73".to_string()),
        ..base_message()
    };
    let result = ft8_message_to_qso_type(&msg, "<...> YS/WE9G RR73", "K5ARH");
    assert_eq!(
        result,
        pancetta_qso::states::MessageType::FinalConfirmation {
            to_station: "<...>".to_string(),
            from_station: "YS/WE9G".to_string(),
        }
    );
}
