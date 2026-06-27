//! Commands sent client→server. v1 control surface (call/answer/CQ/frequency)
//! plus session/control primitives (enforced server-side in a later sub-plan).
use pancetta_core::{slot::SlotParity, ResponseStep};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ClientCommand {
    /// Set the RX dial (Hz). `vfo` 0 = A.
    SetFrequency { vfo: u8, frequency_hz: u64 },
    /// Enable/disable split; `tx_frequency_hz` ignored when disabled.
    SetSplit { enabled: bool, tx_frequency_hz: u64 },
    /// Call a station selected from the DX Hunter list.
    CallStation {
        callsign: String,
        frequency_hz: u64,
        dx_parity: Option<SlotParity>,
    },
    /// Answer a station calling us, opening at `step`.
    AnswerCaller {
        callsign: String,
        frequency_hz: u64,
        dx_parity: Option<SlotParity>,
        step: ResponseStep,
        snr: Option<f32>,
    },
    /// Start a manual CQ at the given audio offset (Hz).
    StartCq { frequency_offset_hz: f64 },
    /// Stop an unanswered manual CQ.
    StopCq,
    /// Request the control role (one control operator at a time).
    TakeControl,
    /// Release the control role.
    ReleaseControl,
    /// Arm/disarm transmit for this control session (must be armed before any TX).
    SetTransmitArmed { armed: bool },
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
}
