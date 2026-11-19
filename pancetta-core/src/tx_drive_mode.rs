//! Operator choice for automatic TX audio drive on one band.

/// Whether completed FT transmissions may adjust the band's audio drive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TxDriveMode {
    /// Learn the clean ALC knee from completed transmissions.
    #[default]
    Auto,
    /// Hold the gain chosen by the operator.
    Manual,
}

impl TxDriveMode {
    /// Switch between automatic learning and the operator's fixed setting.
    pub fn toggle(self) -> Self {
        match self {
            Self::Auto => Self::Manual,
            Self::Manual => Self::Auto,
        }
    }
}
