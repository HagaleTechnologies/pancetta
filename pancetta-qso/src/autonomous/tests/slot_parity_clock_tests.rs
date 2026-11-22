//! PAN-358: the untracked-parity fallback in the decode feed reads the
//! caller's `now` (the coordinator's slot-clock reading), never the system
//! clock.

use super::*;
use pancetta_core::time::UtcTimestamp;

fn untracked(text: &str, call: &str, freq: f64) -> DecodedMessageInfo {
    DecodedMessageInfo {
        callsign: Some(call.to_string()),
        frequency_hz: freq,
        snr: -5,
        message_text: text.to_string(),
        slot_parity: None,
        confidence: None,
        time_offset_s: None,
        decode_origin: None,
        captured_dial_hz: None,
    }
}

fn operator() -> AutonomousOperator {
    let config = AutonomousConfig {
        enabled: true,
        slot_parity: SlotParityConfig::Auto,
        min_dx_score: 0.3,
        ..AutonomousConfig::default()
    };
    AutonomousOperator::new(config, "K5ARH".into(), Some("FN42".into()))
}

#[test]
fn untracked_parity_uses_the_callers_now() {
    // Ten years from the real clock, so the system clock cannot agree by
    // accident: 00:00:15 is an odd FT8 slot, 00:00:30 an even one.
    let odd_now = UtcTimestamp::from_ymd_hms(2036, 1, 1, 0, 0, 20).unwrap();
    let mut op = operator();
    op.feed_decoded_messages_on_dial_at(
        &[
            untracked("K1ABC W1XYZ -12", "K1ABC", 1500.0),
            untracked("K2ABC W2XYZ -12", "K2ABC", 1600.0),
        ],
        &NullDxEvaluator,
        0,
        odd_now,
    );
    assert_eq!(op.slot_manager.auto_detect_odd_activity, 2);
    assert_eq!(op.slot_manager.auto_detect_even_activity, 0);

    let even_now = UtcTimestamp::from_ymd_hms(2036, 1, 1, 0, 0, 35).unwrap();
    let mut op = operator();
    op.feed_decoded_messages_on_dial_at(
        &[untracked("K3ABC W3XYZ -12", "K3ABC", 1700.0)],
        &NullDxEvaluator,
        0,
        even_now,
    );
    assert_eq!(op.slot_manager.auto_detect_even_activity, 1);
    assert_eq!(op.slot_manager.auto_detect_odd_activity, 0);
}
