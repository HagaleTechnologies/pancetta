//! Transmit meter readings shared by the Hamlib poll loop and the TUI (PAN-291).
//!
//! The coordinator reads the rig's forward-power and ALC meters while keyed and
//! the Shift+S station health panel shows them. `None` always means "not
//! reported or not read"; no reading is ever substituted.

/// Forward power from the rig's RF power meter (PAN-291).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ForwardPower {
    /// Watts, from Hamlib's `RFPOWER_METER_WATTS` level.
    Watts(f32),
    /// A 0..1 fraction of the rig's maximum power, from `RFPOWER_METER`.
    FractionOfMax(f32),
}

/// One keyed poll's TX meter readings. `None` means not reported or not read;
/// never a substitute.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TxMeterReading {
    /// Forward power, when the rig reported a valid value.
    pub forward_power: Option<ForwardPower>,
    /// ALC on Hamlib's 0..1 scale, when the rig reported a valid value.
    pub alc: Option<f32>,
}

/// What this Hamlib session will try to read, and the ALC warning limit when
/// the overdrive warning is on.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TxMeterCapability {
    /// The session will read a forward-power meter while keyed.
    pub forward_power: bool,
    /// The session will read the ALC meter while keyed.
    pub alc: bool,
    /// `Some(max_alc)` only when the ALC overdrive warning is on.
    pub alc_limit: Option<f32>,
}
