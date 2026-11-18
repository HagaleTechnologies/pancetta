//! Events sent server→client.
//!
//! Tag values are camelCase (e.g. `"dxHunter"`, `"signalStrength"`,
//! `"controlState"`) per the dispensa rig-api.v1 schema (ADR-0003).
//!
//! The `Decoded` variant wraps its payload in a named field (`decoded`) so the
//! JSON nests as `{"event":"decoded","decoded":{...}}` matching the schema's
//! `required: ["event","decoded"]` shape.
//!
//! Note: serde's `rename_all = "camelCase"` on an internally-tagged enum only
//! renames the variant tag values, NOT struct-variant field names. Each
//! multi-word field therefore carries an explicit `#[serde(rename = "...")]`.
use crate::dto::{DecodedView, DxRow, PendingCall, Placement, QsoProgress, Spectrum};
use crate::wire_serde;
use serde::{Deserialize, Serialize};

/// A server→client event, tagged by `event` on the wire (rig-api.v1
/// `serverEvent`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum ServerEvent {
    /// A single FT8 decode — nested as `{"event":"decoded","decoded":{…}}`.
    Decoded {
        /// The decode, enriched for display.
        decoded: DecodedView,
    },
    /// DX Hunter list update.
    DxHunter {
        /// DX Hunter rows.
        rows: Vec<DxRow>,
    },
    /// Full replacement of the active-QSO and pending-call lists.
    ActiveQsos {
        /// QSOs currently active.
        qsos: Vec<QsoProgress>,
        /// Manual calls parked in the cross-parity queue, not yet started.
        pending: Vec<PendingCall>,
    },
    /// Rig dial frequency readback.
    Frequency {
        /// VFO index the reading is for (0 = VFO A).
        vfo: u8,
        /// Dial frequency in Hz.
        #[serde(rename = "frequencyHz")]
        frequency_hz: u64,
    },
    /// Split TX state changed.
    Split {
        /// Split TX dial frequency in Hz, or 0 for simplex.
        #[serde(rename = "txHz")]
        tx_hz: u64,
    },
    /// Station-wide operating mode changed (operator Shift+M). Additive,
    /// dispensa Q-0027 — see `StateSnapshot::mode`.
    Mode {
        /// New active operating mode (`"FT8"`, `"FT4"` or `"FT2"`).
        mode: String,
    },
    /// Rig S-meter reading.
    SignalStrength {
        /// Signal strength in dB relative to S9 (0 = S9, about -54 = S0,
        /// +20 = S9+20), per the hamlib `STRENGTH` convention.
        #[serde(rename = "dbOverS9")]
        db_over_s9: i32,
    },
    /// Waterfall/spectrum magnitude snapshot — nested as
    /// `{"event":"spectrum","spectrum":{…}}`. Additive, dispensa Q-0024.
    Spectrum {
        /// The spectrum frame.
        spectrum: Spectrum,
    },
    /// Per-window TX-placement/openness ranking — nested as
    /// `{"event":"placement","placement":{…}}`. Additive, dispensa Q-0026.
    Placement {
        /// The placement ranking.
        placement: Placement,
    },
    /// Transmit-activity indicator reported by the TX worker.
    TxStatus {
        /// `true` while a transmission is being keyed or aired, `false`
        /// otherwise. Software-reported — set before the final key-down
        /// checks — so it does not confirm hardware PTT.
        active: bool,
    },
    /// Global TX policy changed.
    TxPolicy {
        /// The current global TX policy.
        #[serde(with = "wire_serde::tx_policy")]
        policy: pancetta_core::TxPolicy,
    },
    /// Control/arm state for the receiving session.
    ControlState {
        /// Whether the receiving session holds the control role.
        #[serde(rename = "controlHeldByMe")]
        control_held_by_me: bool,
        /// Whether transmit is currently armed.
        #[serde(rename = "transmitArmed")]
        transmit_armed: bool,
    },
    /// Free-form status report from a server component.
    Status {
        /// Name of the reporting component.
        component: String,
        /// Human-readable status text.
        status: String,
    },
    /// Error report from a server component, such as a refused command or a
    /// denied remote TX.
    Error {
        /// Name of the reporting component (for example `"tx"` or
        /// `"stationAgent"`).
        component: String,
        /// Human-readable error text.
        message: String,
    },
}

impl ServerEvent {
    /// Convenience constructor — wraps `DecodedView` in the named-field variant.
    pub fn decoded(view: DecodedView) -> Self {
        ServerEvent::Decoded { decoded: view }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::DecodedView;
    use chrono::Utc;
    use pancetta_core::TxPolicy;

    #[test]
    fn event_roundtrips() {
        let e = ServerEvent::TxPolicy {
            policy: TxPolicy::Full,
        };
        let j = serde_json::to_string(&e).unwrap();
        assert!(
            j.contains(r#""event":"txPolicy""#),
            "expected txPolicy tag in: {j}"
        );
        assert!(
            j.contains(r#""policy":"full""#),
            "expected full policy in: {j}"
        );
        assert_eq!(serde_json::from_str::<ServerEvent>(&j).unwrap(), e);

        let d = ServerEvent::Decoded {
            decoded: DecodedView {
                timestamp: Utc::now(),
                frequency_hz: 14_075_931.0,
                snr: -8,
                delta_time: 0.2,
                delta_freq: 0.0,
                call_sign: Some("D2UY".into()),
                grid_square: None,
                message: "CQ D2UY JI64".into(),
                slot_parity: None,
                is_directed_at_us: false,
                worked_before: false,
                needed: true,
                atno: true,
                priority_score: Some(720),
            },
        };
        let dj = serde_json::to_string(&d).unwrap();
        // Schema shape: {"event":"decoded","decoded":{…}}
        assert!(
            dj.contains(r#""event":"decoded""#),
            "expected decoded tag in: {dj}"
        );
        assert!(
            dj.contains(r#""decoded":{"#),
            "expected nested decoded object in: {dj}"
        );
        assert_eq!(serde_json::from_str::<ServerEvent>(&dj).unwrap(), d);
    }

    #[test]
    fn event_tag_values_are_camel_case() {
        let cases: &[(&str, ServerEvent)] = &[
            ("dxHunter", ServerEvent::DxHunter { rows: vec![] }),
            (
                "activeQsos",
                ServerEvent::ActiveQsos {
                    qsos: vec![],
                    pending: vec![],
                },
            ),
            (
                "frequency",
                ServerEvent::Frequency {
                    vfo: 0,
                    frequency_hz: 14_074_000,
                },
            ),
            ("split", ServerEvent::Split { tx_hz: 0 }),
            (
                "mode",
                ServerEvent::Mode {
                    mode: "FT4".to_string(),
                },
            ),
            (
                "signalStrength",
                ServerEvent::SignalStrength { db_over_s9: 5 },
            ),
            (
                "spectrum",
                ServerEvent::Spectrum {
                    spectrum: Spectrum {
                        bin_start_hz: 14_074_000.0,
                        bin_width_hz: 5.86,
                        mags_db: vec![-104.2, -98.1],
                        seq: 1,
                        timestamp: Utc::now(),
                    },
                },
            ),
            (
                "placement",
                ServerEvent::Placement {
                    placement: Placement {
                        slices: vec![],
                        openness: vec![],
                        bin_hz: 5.86,
                        range: (200.0, 3000.0),
                    },
                },
            ),
            ("txStatus", ServerEvent::TxStatus { active: true }),
            (
                "txPolicy",
                ServerEvent::TxPolicy {
                    policy: TxPolicy::RespondOnly,
                },
            ),
            (
                "controlState",
                ServerEvent::ControlState {
                    control_held_by_me: true,
                    transmit_armed: false,
                },
            ),
            (
                "status",
                ServerEvent::Status {
                    component: "audio".into(),
                    status: "ok".into(),
                },
            ),
            (
                "error",
                ServerEvent::Error {
                    component: "rig".into(),
                    message: "timeout".into(),
                },
            ),
        ];
        for (expected_tag, event) in cases {
            let j = serde_json::to_string(event).unwrap();
            assert!(
                j.contains(&format!(r#""event":"{}""#, expected_tag)),
                "expected tag {expected_tag} in: {j}"
            );
            assert_eq!(
                serde_json::from_str::<ServerEvent>(&j).unwrap(),
                *event,
                "roundtrip failed for tag {expected_tag}, json: {j}"
            );
        }
    }

    #[test]
    fn signal_strength_field_is_camel_case() {
        let e = ServerEvent::SignalStrength { db_over_s9: 12 };
        let j = serde_json::to_string(&e).unwrap();
        assert!(j.contains(r#""dbOverS9""#), "expected dbOverS9 in: {j}");
    }

    #[test]
    fn control_state_fields_are_camel_case() {
        let e = ServerEvent::ControlState {
            control_held_by_me: true,
            transmit_armed: false,
        };
        let j = serde_json::to_string(&e).unwrap();
        assert!(
            j.contains(r#""controlHeldByMe""#),
            "expected controlHeldByMe in: {j}"
        );
        assert!(
            j.contains(r#""transmitArmed""#),
            "expected transmitArmed in: {j}"
        );
    }

    #[test]
    fn respond_only_policy_wire_value() {
        let e = ServerEvent::TxPolicy {
            policy: TxPolicy::RespondOnly,
        };
        let j = serde_json::to_string(&e).unwrap();
        assert!(
            j.contains(r#""policy":"respondOnly""#),
            "expected respondOnly in: {j}"
        );
    }

    #[test]
    fn placement_field_is_camel_case() {
        let e = ServerEvent::Placement {
            placement: Placement {
                slices: vec![crate::dto::PlacementSlice {
                    offset_hz: 1500.0,
                    score: 10.0,
                    clear_first: true,
                    clear_second: true,
                }],
                openness: vec![3],
                bin_hz: 5.86,
                range: (200.0, 3000.0),
            },
        };
        let j = serde_json::to_string(&e).unwrap();
        assert!(
            j.contains(r#""event":"placement""#),
            "expected placement tag in: {j}"
        );
        assert!(j.contains(r#""offsetHz""#), "expected offsetHz in: {j}");
        assert!(j.contains(r#""binHz""#), "expected binHz in: {j}");
    }
}
