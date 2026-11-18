//! Commands sent client→server. v1 control surface (call/answer/CQ/frequency)
//! plus session/control primitives (enforced server-side in a later sub-plan).
//!
//! Tag values are camelCase (e.g. `"setFrequency"`, `"answerCaller"`) per the
//! dispensa rig-api.v1 schema (ADR-0003).
//!
//! Note: serde's `rename_all = "camelCase"` on an internally-tagged enum only
//! renames the variant tag values, NOT the struct-variant field names. Each
//! multi-word field inside a struct variant therefore carries an explicit
//! `#[serde(rename = "...")]`.
use crate::wire_serde;
use pancetta_core::{slot::SlotParity, ResponseStep};
use serde::{Deserialize, Serialize};

/// A client→server command, tagged by `cmd` on the wire (rig-api.v1
/// `clientCommand`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
pub enum ClientCommand {
    /// Set the RX dial (Hz). `vfo` 0 = A.
    SetFrequency {
        /// Target VFO index (0 = VFO A).
        vfo: u8,
        /// Target dial frequency in Hz.
        #[serde(rename = "frequencyHz")]
        frequency_hz: u64,
    },
    /// Enable/disable split; `tx_frequency_hz` ignored when disabled.
    SetSplit {
        /// Whether split operation is enabled.
        enabled: bool,
        /// TX dial frequency in Hz; meaningful only when `enabled`.
        #[serde(rename = "txFrequencyHz")]
        tx_frequency_hz: u64,
    },
    /// Call a station selected from the DX Hunter list.
    CallStation {
        /// Callsign of the DX station to call.
        callsign: String,
        /// The DX station's audio offset in Hz within the passband, where it
        /// was heard.
        #[serde(rename = "frequencyHz")]
        frequency_hz: u64,
        /// The DX station's TX slot parity; `None` when unknown.
        #[serde(rename = "dxParity", with = "wire_serde::slot_parity_opt")]
        dx_parity: Option<SlotParity>,
    },
    /// Answer a station calling us, opening at `step`.
    AnswerCaller {
        /// Callsign of the station calling us.
        callsign: String,
        /// The caller's audio offset in Hz within the passband.
        #[serde(rename = "frequencyHz")]
        frequency_hz: u64,
        /// The caller's TX slot parity; `None` when unknown.
        #[serde(rename = "dxParity", with = "wire_serde::slot_parity_opt")]
        dx_parity: Option<SlotParity>,
        /// Exchange step our first reply opens at.
        #[serde(with = "wire_serde::response_step")]
        step: ResponseStep,
        /// The caller's decoded SNR in dB; `None` when the client did not
        /// supply one.
        snr: Option<f32>,
    },
    /// Start a manual CQ at the given audio offset (Hz).
    StartCq {
        /// Audio offset in Hz within the passband to call CQ on.
        #[serde(rename = "frequencyOffsetHz")]
        frequency_offset_hz: f64,
    },
    /// Stop an unanswered manual CQ.
    StopCq,
    /// Request the control role (one control operator at a time). The station
    /// agent grants it only to a peer holding a currently-valid `qsy` or `tx`
    /// capability; a status-only request is refused without a reply.
    TakeControl,
    /// Release the control role.
    ReleaseControl,
    /// Legacy disarm command. Arming is NOT done through it: the station
    /// agent maps `armed: false` to a disarm of any current arm and treats
    /// `armed: true` as unsupported, because a real arm requires the
    /// separately-signed `txArm` frame (e2e-auth.v1).
    SetTransmitArmed {
        /// `false` disarms transmit; `true` is unsupported (arming needs a
        /// signed `txArm`, not this command).
        armed: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_roundtrips_tagged() {
        let cmds = vec![
            ClientCommand::SetFrequency {
                vfo: 0,
                frequency_hz: 14_074_000,
            },
            ClientCommand::CallStation {
                callsign: "D2UY".into(),
                frequency_hz: 14_074_000,
                dx_parity: Some(SlotParity::Even),
            },
            ClientCommand::AnswerCaller {
                callsign: "ZL3IO".into(),
                frequency_hz: 14_075_500,
                dx_parity: Some(SlotParity::Odd),
                step: ResponseStep::Report,
                snr: Some(-12.0),
            },
            ClientCommand::StartCq {
                frequency_offset_hz: 1500.0,
            },
            ClientCommand::SetTransmitArmed { armed: true },
        ];
        for c in cmds {
            let j = serde_json::to_string(&c).unwrap();
            assert_eq!(
                serde_json::from_str::<ClientCommand>(&j).unwrap(),
                c,
                "json was {j}"
            );
        }
    }

    #[test]
    fn command_tag_values_are_camel_case() {
        let j = serde_json::to_string(&ClientCommand::SetFrequency {
            vfo: 0,
            frequency_hz: 14_074_000,
        })
        .unwrap();
        assert!(
            j.contains(r#""cmd":"setFrequency""#),
            "expected setFrequency in: {j}"
        );
        assert!(
            j.contains(r#""frequencyHz""#),
            "expected frequencyHz in: {j}"
        );

        let j = serde_json::to_string(&ClientCommand::CallStation {
            callsign: "D2UY".into(),
            frequency_hz: 14_074_000,
            dx_parity: None,
        })
        .unwrap();
        assert!(
            j.contains(r#""cmd":"callStation""#),
            "expected callStation in: {j}"
        );
        assert!(
            j.contains(r#""frequencyHz""#),
            "expected frequencyHz in: {j}"
        );

        let j = serde_json::to_string(&ClientCommand::AnswerCaller {
            callsign: "ZL3IO".into(),
            frequency_hz: 14_075_500,
            dx_parity: Some(SlotParity::Odd),
            step: ResponseStep::SeventyThree,
            snr: None,
        })
        .unwrap();
        assert!(
            j.contains(r#""cmd":"answerCaller""#),
            "expected answerCaller in: {j}"
        );
        assert!(
            j.contains(r#""step":"seventyThree""#),
            "expected seventyThree step in: {j}"
        );
        assert!(
            j.contains(r#""dxParity":"odd""#),
            "expected odd dxParity in: {j}"
        );

        let j = serde_json::to_string(&ClientCommand::SetTransmitArmed { armed: true }).unwrap();
        assert!(
            j.contains(r#""cmd":"setTransmitArmed""#),
            "expected setTransmitArmed in: {j}"
        );

        let j = serde_json::to_string(&ClientCommand::SetSplit {
            enabled: true,
            tx_frequency_hz: 14_075_000,
        })
        .unwrap();
        assert!(
            j.contains(r#""cmd":"setSplit""#),
            "expected setSplit in: {j}"
        );
        assert!(
            j.contains(r#""txFrequencyHz""#),
            "expected txFrequencyHz in: {j}"
        );

        let j = serde_json::to_string(&ClientCommand::StartCq {
            frequency_offset_hz: 1500.0,
        })
        .unwrap();
        assert!(j.contains(r#""cmd":"startCq""#), "expected startCq in: {j}");
        assert!(
            j.contains(r#""frequencyOffsetHz""#),
            "expected frequencyOffsetHz in: {j}"
        );
    }
}
