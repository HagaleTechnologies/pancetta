use pancetta_ft8::{DecodedMessage, Ft8Message};

/// A synthetic directed decode — the shape the QSO handler's
/// `MessageType::DecodedMessage` arm receives. `captured_dial_hz`
/// starts `None`, exactly like `DecodedMessage::new` leaves it (and
/// like test scaffolding that never went through the decoder's
/// stamping site).
fn directed_decode() -> DecodedMessage {
    let mut msg = DecodedMessage::new(
        Ft8Message {
            from_callsign: Some("W1AW".to_string()),
            to_callsign: Some("K5ARH".to_string()),
            ..Ft8Message::default()
        },
        -10.0,  // snr_db
        0.9,    // confidence
        1500.0, // frequency_offset
        0.05,   // time_offset
    );
    msg.text = "K5ARH W1AW FN31".to_string();
    msg
}

#[test]
fn directed_decode_from_a_prior_band_is_stale_for_the_qso_handler() {
    // Old-band decode (40m) arriving after a switch to 20m must be dropped
    // before maybe_answer_caller can create a QSO on the new band.
    let mut msg = directed_decode();
    msg.captured_dial_hz = Some(7_074_000);
    assert!(crate::coordinator::is_stale_band_decode(
        msg.captured_dial_hz,
        14_074_000
    ));
}

#[test]
fn fresh_decode_and_unknown_dial_decodes_are_never_dropped_by_the_qso_handler() {
    let mut msg = directed_decode();
    msg.captured_dial_hz = Some(14_074_000); // same band  -> admitted
    assert!(!crate::coordinator::is_stale_band_decode(
        msg.captured_dial_hz,
        14_074_000
    ));
    msg.captured_dial_hz = None; // no CAT     -> admitted (fail open)
    assert!(!crate::coordinator::is_stale_band_decode(
        msg.captured_dial_hz,
        14_074_000
    ));
    msg.captured_dial_hz = Some(7_074_000); // live unknown -> admitted
    assert!(!crate::coordinator::is_stale_band_decode(
        msg.captured_dial_hz,
        0
    ));
}
