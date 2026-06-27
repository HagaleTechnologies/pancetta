//! Serde helper modules for pancetta-core enum types that use PascalCase
//! serialization internally but must appear in camelCase on the wire per
//! the dispensa contracts/rig/rig-api.v1 schema (ADR-0003).
//!
//! These helpers are used via `#[serde(with = "wire_serde::foo")]` on
//! individual fields. They do NOT change the internal pancetta-core types.

use pancetta_core::{slot::SlotParity, ResponseStep, TxPolicy};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

// ── SlotParity ───────────────────────────────────────────────────────────────
// Schema enum: ["even", "odd"]

/// Wire-format representation of `SlotParity` (camelCase per schema).
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum SlotParityWire {
    Even,
    Odd,
}

impl From<SlotParity> for SlotParityWire {
    fn from(p: SlotParity) -> Self {
        match p {
            SlotParity::Even => SlotParityWire::Even,
            SlotParity::Odd => SlotParityWire::Odd,
        }
    }
}

impl From<SlotParityWire> for SlotParity {
    fn from(w: SlotParityWire) -> Self {
        match w {
            SlotParityWire::Even => SlotParity::Even,
            SlotParityWire::Odd => SlotParity::Odd,
        }
    }
}

/// `#[serde(with = "wire_serde::slot_parity")]` — required field.
#[allow(dead_code)]
pub mod slot_parity {
    use super::*;

    pub fn serialize<S: Serializer>(v: &SlotParity, s: S) -> Result<S::Ok, S::Error> {
        SlotParityWire::from(*v).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SlotParity, D::Error> {
        SlotParityWire::deserialize(d).map(Into::into)
    }
}

/// `#[serde(with = "wire_serde::slot_parity_opt")]` — Option<SlotParity>.
pub mod slot_parity_opt {
    use super::*;

    pub fn serialize<S: Serializer>(v: &Option<SlotParity>, s: S) -> Result<S::Ok, S::Error> {
        v.map(SlotParityWire::from).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<SlotParity>, D::Error> {
        Option::<SlotParityWire>::deserialize(d).map(|o| o.map(Into::into))
    }
}

// ── ResponseStep ─────────────────────────────────────────────────────────────
// Schema enum: ["grid", "report", "reportAck", "rr73", "seventyThree"]

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum ResponseStepWire {
    Grid,
    Report,
    ReportAck,
    Rr73,
    SeventyThree,
}

impl From<ResponseStep> for ResponseStepWire {
    fn from(r: ResponseStep) -> Self {
        match r {
            ResponseStep::Grid => ResponseStepWire::Grid,
            ResponseStep::Report => ResponseStepWire::Report,
            ResponseStep::ReportAck => ResponseStepWire::ReportAck,
            ResponseStep::Rr73 => ResponseStepWire::Rr73,
            ResponseStep::SeventyThree => ResponseStepWire::SeventyThree,
        }
    }
}

impl From<ResponseStepWire> for ResponseStep {
    fn from(w: ResponseStepWire) -> Self {
        match w {
            ResponseStepWire::Grid => ResponseStep::Grid,
            ResponseStepWire::Report => ResponseStep::Report,
            ResponseStepWire::ReportAck => ResponseStep::ReportAck,
            ResponseStepWire::Rr73 => ResponseStep::Rr73,
            ResponseStepWire::SeventyThree => ResponseStep::SeventyThree,
        }
    }
}

/// `#[serde(with = "wire_serde::response_step")]`.
pub mod response_step {
    use super::*;

    pub fn serialize<S: Serializer>(v: &ResponseStep, s: S) -> Result<S::Ok, S::Error> {
        ResponseStepWire::from(*v).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ResponseStep, D::Error> {
        ResponseStepWire::deserialize(d).map(Into::into)
    }
}

// ── TxPolicy ─────────────────────────────────────────────────────────────────
// Schema enum: ["full", "respondOnly", "disabled"]

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum TxPolicyWire {
    Full,
    RespondOnly,
    Disabled,
}

impl From<TxPolicy> for TxPolicyWire {
    fn from(p: TxPolicy) -> Self {
        match p {
            TxPolicy::Full => TxPolicyWire::Full,
            TxPolicy::RespondOnly => TxPolicyWire::RespondOnly,
            TxPolicy::Disabled => TxPolicyWire::Disabled,
        }
    }
}

impl From<TxPolicyWire> for TxPolicy {
    fn from(w: TxPolicyWire) -> Self {
        match w {
            TxPolicyWire::Full => TxPolicy::Full,
            TxPolicyWire::RespondOnly => TxPolicy::RespondOnly,
            TxPolicyWire::Disabled => TxPolicy::Disabled,
        }
    }
}

/// `#[serde(with = "wire_serde::tx_policy")]`.
pub mod tx_policy {
    use super::*;

    pub fn serialize<S: Serializer>(v: &TxPolicy, s: S) -> Result<S::Ok, S::Error> {
        TxPolicyWire::from(*v).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<TxPolicy, D::Error> {
        TxPolicyWire::deserialize(d).map(Into::into)
    }
}

/// `#[serde(with = "wire_serde::tx_policy_opt")]` — Option<TxPolicy>.
#[allow(dead_code)]
pub mod tx_policy_opt {
    use super::*;

    pub fn serialize<S: Serializer>(v: &Option<TxPolicy>, s: S) -> Result<S::Ok, S::Error> {
        v.map(TxPolicyWire::from).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<TxPolicy>, D::Error> {
        Option::<TxPolicyWire>::deserialize(d).map(|o| o.map(Into::into))
    }
}

/// Verify that the wire-format helpers round-trip correctly.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_parity_wire_casing() {
        assert_eq!(
            serde_json::to_string(&SlotParityWire::Even).unwrap(),
            r#""even""#
        );
        assert_eq!(
            serde_json::to_string(&SlotParityWire::Odd).unwrap(),
            r#""odd""#
        );
    }

    #[test]
    fn response_step_wire_casing() {
        let cases = [
            (ResponseStep::Grid, r#""grid""#),
            (ResponseStep::Report, r#""report""#),
            (ResponseStep::ReportAck, r#""reportAck""#),
            (ResponseStep::Rr73, r#""rr73""#),
            (ResponseStep::SeventyThree, r#""seventyThree""#),
        ];
        for (step, expected) in cases {
            let w = ResponseStepWire::from(step);
            assert_eq!(serde_json::to_string(&w).unwrap(), expected);
            let back: ResponseStep =
                ResponseStepWire::deserialize(&mut serde_json::Deserializer::from_str(expected))
                    .unwrap()
                    .into();
            assert_eq!(back, step);
        }
    }

    #[test]
    fn tx_policy_wire_casing() {
        let cases = [
            (TxPolicy::Full, r#""full""#),
            (TxPolicy::RespondOnly, r#""respondOnly""#),
            (TxPolicy::Disabled, r#""disabled""#),
        ];
        for (pol, expected) in cases {
            let w = TxPolicyWire::from(pol);
            assert_eq!(serde_json::to_string(&w).unwrap(), expected);
            let back: TxPolicy =
                TxPolicyWire::deserialize(&mut serde_json::Deserializer::from_str(expected))
                    .unwrap()
                    .into();
            assert_eq!(back, pol);
        }
    }
}
