//! Pure bus→protocol translation functions.
//!
//! All functions are stateless and synchronous — no I/O, no async — so they
//! are trivially unit-testable without a running coordinator.
//!
//! `DecodedMessage` frames are intentionally NOT handled by
//! [`server_event_from_bus`]: decoded frames require caller-supplied context
//! (dial frequency, station lookup flags, our callsign) that the bus message
//! alone does not carry. The component calls [`decoded_to_view`] directly with
//! that enriched context and emits `ServerEvent::decoded(view)` itself.

use pancetta_ft8::DecodedMessage;
use pancetta_protocol::{DecodedView, PendingCall, QsoProgress, ServerEvent};

use crate::message_bus::{
    ActiveQsoSnapshotItem, MessageType, PendingCallSnapshotItem, RigControlMessage,
};

/// Convert a single active-QSO snapshot item into the wire `QsoProgress` type.
///
/// Field names match between `ActiveQsoSnapshotItem` and `QsoProgress` so this
/// is a direct copy; Strings are cloned, Copy types are copied.
pub(crate) fn qso_item_to_progress(item: &ActiveQsoSnapshotItem) -> QsoProgress {
    QsoProgress {
        qso_id: item.qso_id.clone(),
        their_callsign: item.their_callsign.clone(),
        state: item.state.clone(),
        frequency_hz: item.frequency_hz,
        tx_parity: item.tx_parity,
        ladder_labels: item.ladder_labels.clone(),
        ladder_ours: item.ladder_ours.clone(),
        ladder_index: item.ladder_index,
        now_line: item.now_line.clone(),
        next_line: item.next_line.clone(),
        last_tx_text: item.last_tx_text.clone(),
        last_rx_text: item.last_rx_text.clone(),
        report_sent: item.report_sent,
        report_received: item.report_received,
        dx_last_activity: item.dx_last_activity.clone(),
        started_at: item.started_at,
    }
}

/// Convert a pending cross-parity call queue item into the wire `PendingCall` type.
pub(crate) fn pending_item_to_call(item: &PendingCallSnapshotItem) -> PendingCall {
    PendingCall {
        callsign: item.callsign.clone(),
        dx_parity: item.dx_parity,
        waited_secs: item.waited_secs,
    }
}

/// Return `true` when `to` (the `to_callsign` field of a decoded frame) is
/// addressed to `our_callsign`.
///
/// Mirrors the logic in `coordinator/tui_relay.rs`: strip any `/`-suffix from
/// BOTH sides (take the part before the first `/`), compare
/// case-insensitively, require both to be non-empty. `None` → `false`.
pub(crate) fn directed_at_us(to: Option<&str>, our_callsign: &str) -> bool {
    let to = match to {
        Some(t) => t,
        None => return false,
    };
    let to_base = to.split('/').next().unwrap_or(to);
    let our_base = our_callsign.split('/').next().unwrap_or(our_callsign);
    !to_base.is_empty() && !our_base.is_empty() && to_base.eq_ignore_ascii_case(our_base)
}

/// Convert a decoded FT8 frame into the wire `DecodedView` type.
///
/// The caller supplies the enriched context that the bus message alone does
/// not carry:
/// - `dial_hz`: the rig's current RX dial frequency in Hz (absolute RF =
///   `dial_hz + msg.frequency_offset`).
/// - `our_callsign`: used to detect frames addressed to us.
/// - `worked_before`, `needed`, `atno`: from the shared station-lookup cache.
///
/// `priority_score` is `None` — deferred to a later sub-plan; the scorer is
/// kept out of this pure translation layer.
pub(crate) fn decoded_to_view(
    msg: &DecodedMessage,
    dial_hz: f64,
    our_callsign: &str,
    worked_before: bool,
    needed: bool,
    atno: bool,
) -> DecodedView {
    DecodedView {
        timestamp: chrono::DateTime::<chrono::Utc>::from(msg.timestamp),
        frequency_hz: dial_hz + msg.frequency_offset,
        snr: msg.snr_db.round() as i32,
        delta_time: msg.time_offset as f32,
        delta_freq: msg.frequency_offset as f32,
        call_sign: msg.message.from_callsign.clone(),
        grid_square: msg.message.grid_square.clone(),
        message: msg.text.clone(),
        slot_parity: msg.slot_parity,
        is_directed_at_us: directed_at_us(msg.message.to_callsign.as_deref(), our_callsign),
        worked_before,
        needed,
        atno,
        priority_score: None,
    }
}

/// Map enrichment-free `MessageType` variants to `ServerEvent`.
///
/// Returns `None` for every variant that either requires caller-supplied
/// context (e.g. `DecodedMessage` — handled via [`decoded_to_view`]) or is
/// not relevant to remote clients.
pub(crate) fn server_event_from_bus(msg: &MessageType) -> Option<ServerEvent> {
    match msg {
        MessageType::ActiveQsosSnapshot { qsos, pending } => Some(ServerEvent::ActiveQsos {
            qsos: qsos.iter().map(qso_item_to_progress).collect(),
            pending: pending.iter().map(pending_item_to_call).collect(),
        }),
        MessageType::RigControl(RigControlMessage::FrequencyResponse { vfo, frequency }) => {
            Some(ServerEvent::Frequency {
                vfo: *vfo,
                frequency_hz: *frequency,
            })
        }
        MessageType::RigControl(RigControlMessage::SignalStrengthResponse { db_over_s9 }) => {
            Some(ServerEvent::SignalStrength {
                db_over_s9: *db_over_s9,
            })
        }
        MessageType::SplitStatus { tx_hz } => Some(ServerEvent::Split { tx_hz: *tx_hz }),
        MessageType::TxStatus { active } => Some(ServerEvent::TxStatus { active: *active }),
        _ => None,
    }
}

#[cfg(test)]
// rationale: test-only builder structs assigned field-by-field after default();
// sequential assignment reads clearer than a struct-update splat.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use chrono::Utc;
    use pancetta_core::slot::SlotParity;
    use pancetta_ft8::{DecodedMessage, Ft8Message};

    // ── directed_at_us ───────────────────────────────────────────────────────

    #[test]
    fn directed_at_us_exact_match() {
        assert!(directed_at_us(Some("K5ARH"), "K5ARH"));
    }

    #[test]
    fn directed_at_us_suffix_on_to() {
        assert!(directed_at_us(Some("K5ARH/P"), "K5ARH"));
    }

    #[test]
    fn directed_at_us_suffix_on_ours() {
        assert!(directed_at_us(Some("K5ARH"), "K5ARH/M"));
    }

    #[test]
    fn directed_at_us_different_call() {
        assert!(!directed_at_us(Some("W1XYZ"), "K5ARH"));
    }

    #[test]
    fn directed_at_us_none() {
        assert!(!directed_at_us(None, "K5ARH"));
    }

    #[test]
    fn directed_at_us_empty_string() {
        assert!(!directed_at_us(Some(""), "K5ARH"));
    }

    // ── qso_item_to_progress ─────────────────────────────────────────────────

    fn make_snapshot_item() -> ActiveQsoSnapshotItem {
        ActiveQsoSnapshotItem {
            qso_id: "qso-001".into(),
            their_callsign: "ZL3IO".into(),
            state: "WaitingForReport".into(),
            frequency_hz: 1500.0,
            tx_parity: Some(SlotParity::Odd),
            ladder_labels: vec!["Grid".into(), "Rpt".into()],
            ladder_ours: vec![true, false],
            ladder_index: 1,
            now_line: "TX: ZL3IO K5ARH R-09".into(),
            next_line: "RR73".into(),
            last_tx_text: Some("ZL3IO K5ARH R-09".into()),
            last_tx_at: None,
            last_rx_text: Some("K5ARH ZL3IO -12".into()),
            last_rx_at: None,
            snr_rx: Some(-12),
            report_sent: Some(-9),
            report_received: Some(-12),
            exchange_count: 3,
            initiated_by: "Manual".into(),
            call_count: 2,
            max_calls: 25,
            watchdog_deadline: None,
            dx_last_activity: Some("\u{2192} us -12".into()),
            started_at: Utc::now(),
            hound: false,
        }
    }

    #[test]
    fn qso_item_to_progress_copies_key_fields() {
        let item = make_snapshot_item();
        let prog = qso_item_to_progress(&item);

        assert_eq!(prog.qso_id, "qso-001");
        assert_eq!(prog.their_callsign, "ZL3IO");
        assert_eq!(prog.state, "WaitingForReport");
        assert_eq!(prog.frequency_hz, 1500.0);
        assert_eq!(prog.tx_parity, Some(SlotParity::Odd));
        assert_eq!(prog.ladder_index, 1);
        assert_eq!(prog.report_sent, Some(-9));
        assert_eq!(prog.dx_last_activity, Some("\u{2192} us -12".into()));
        assert_eq!(prog.started_at, item.started_at);
    }

    // ── pending_item_to_call ──────────────────────────────────────────────────

    #[test]
    fn pending_item_to_call_copies_all_fields() {
        let item = PendingCallSnapshotItem {
            callsign: "VK9XX".into(),
            dx_parity: Some(SlotParity::Even),
            waited_secs: 45,
        };
        let call = pending_item_to_call(&item);
        assert_eq!(call.callsign, "VK9XX");
        assert_eq!(call.dx_parity, Some(SlotParity::Even));
        assert_eq!(call.waited_secs, 45);
    }

    // ── decoded_to_view ───────────────────────────────────────────────────────

    #[test]
    fn decoded_to_view_fields() {
        let mut ft8_msg = Ft8Message::default();
        ft8_msg.from_callsign = Some("D2UY".into());
        ft8_msg.to_callsign = Some("K5ARH".into());
        ft8_msg.grid_square = Some("JI64".into());

        let mut msg = DecodedMessage::new(ft8_msg, -9.4, 1.0, 1500.0, 0.3);
        // Override text so it reflects the message we injected.
        msg.text = "K5ARH D2UY -09".into();
        msg.slot_parity = Some(SlotParity::Even);

        let view = decoded_to_view(&msg, 14_074_000.0, "K5ARH", true, false, true);

        assert_eq!(view.frequency_hz, 14_074_000.0 + 1500.0);
        assert_eq!(view.snr, -9);
        assert_eq!(view.delta_freq, 1500.0_f32);
        assert_eq!(view.delta_time, 0.3_f32);
        assert_eq!(view.call_sign, Some("D2UY".into()));
        assert_eq!(view.grid_square, Some("JI64".into()));
        assert_eq!(view.message, "K5ARH D2UY -09");
        assert_eq!(view.slot_parity, Some(SlotParity::Even));
        assert!(view.is_directed_at_us);
        assert!(view.worked_before);
        assert!(!view.needed);
        assert!(view.atno);
        assert!(view.priority_score.is_none());
    }

    // ── server_event_from_bus ─────────────────────────────────────────────────

    #[test]
    fn active_qsos_snapshot_maps_to_active_qsos_event() {
        let msg = MessageType::ActiveQsosSnapshot {
            qsos: vec![make_snapshot_item()],
            pending: vec![],
        };
        match server_event_from_bus(&msg) {
            Some(ServerEvent::ActiveQsos { qsos, pending }) => {
                assert_eq!(qsos.len(), 1);
                assert_eq!(pending.len(), 0);
            }
            other => panic!("expected ActiveQsos, got {:?}", other),
        }
    }

    #[test]
    fn frequency_response_maps_to_frequency_event() {
        let msg = MessageType::RigControl(RigControlMessage::FrequencyResponse {
            vfo: 0,
            frequency: 14_074_000,
        });
        match server_event_from_bus(&msg) {
            Some(ServerEvent::Frequency { vfo, frequency_hz }) => {
                assert_eq!(vfo, 0);
                assert_eq!(frequency_hz, 14_074_000);
            }
            other => panic!("expected Frequency, got {:?}", other),
        }
    }

    #[test]
    fn tx_status_maps_to_tx_status_event() {
        let msg = MessageType::TxStatus { active: true };
        match server_event_from_bus(&msg) {
            Some(ServerEvent::TxStatus { active }) => assert!(active),
            other => panic!("expected TxStatus, got {:?}", other),
        }
    }

    #[test]
    fn decoded_message_returns_none() {
        let msg = MessageType::DecodedMessage(DecodedMessage::new(
            Ft8Message::default(),
            0.0,
            1.0,
            0.0,
            0.0,
        ));
        assert!(server_event_from_bus(&msg).is_none());
    }
}
