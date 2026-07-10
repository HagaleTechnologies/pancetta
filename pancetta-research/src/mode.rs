use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Ft8,
    /// Task W0.4 (2026-07-07): FT4 evaluation tier. Wraps the same
    /// `pancetta_ft8::Ft8Decoder`/`Ft8Encoder`/`Ft8Modulator` with
    /// `Protocol::Ft4` (see `pancetta_ft8::ProtocolParams::ft4()`) —
    /// FT8/FT4/FT2 share one codec, differing only in protocol params.
    Ft4,
    // Future: Js8, Jt9, Jt65, Msk144. Add when their decoders exist.
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Ft8 => "ft8",
            Mode::Ft4 => "ft4",
        }
    }
}

/// Task W0.4: `Mode::Ft8` is the default — every synth-corpus config
/// written before FT4 support existed has no `mode` key at all, and must
/// keep deserializing as FT8 (`SynthConfig.mode`'s `#[serde(default)]`).
impl Default for Mode {
    fn default() -> Self {
        Mode::Ft8
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "ft8" => Ok(Mode::Ft8),
            "ft4" => Ok(Mode::Ft4),
            other => Err(format!("unknown mode: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_json() {
        let json = serde_json::to_string(&Mode::Ft8).unwrap();
        assert_eq!(json, "\"ft8\"");
        let back: Mode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Mode::Ft8);
    }

    #[test]
    fn parse_from_string() {
        assert_eq!("ft8".parse::<Mode>().unwrap(), Mode::Ft8);
        assert_eq!("FT8".parse::<Mode>().unwrap(), Mode::Ft8);
        assert_eq!("ft4".parse::<Mode>().unwrap(), Mode::Ft4);
        assert_eq!("FT4".parse::<Mode>().unwrap(), Mode::Ft4);
        assert!("ft65".parse::<Mode>().is_err());
    }

    #[test]
    fn ft4_round_trip_json() {
        let json = serde_json::to_string(&Mode::Ft4).unwrap();
        assert_eq!(json, "\"ft4\"");
        let back: Mode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Mode::Ft4);
    }
}
