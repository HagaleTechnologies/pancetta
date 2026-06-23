use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Ft8,
    // Future: Ft4, Js8, Jt9, Jt65, Msk144. Add when their decoders exist.
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Ft8 => "ft8",
        }
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
        assert!("ft4".parse::<Mode>().is_err());
    }
}
