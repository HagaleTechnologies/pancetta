//! Attribution and N1MM UDP broadcast settings for newly logged contacts.
use crate::{ConfigError, ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};

/// Rendering preferences for the QSO log.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// Append the Pancetta/cqdx software attribution to new ADIF comments.
    pub attribution_comment: bool,
    /// IP address and port that receives one N1MM Logger+ contactinfo
    /// datagram per logged QSO. Absent or blank: off (PAN-382).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n1mm_udp: Option<String>,
}

impl LoggingConfig {
    /// The N1MM contactinfo destination, or `None` when the key is absent or
    /// blank. `validate_section` guarantees a non-blank value parses.
    pub fn n1mm_udp_destination(&self) -> Option<std::net::SocketAddr> {
        let raw = self.n1mm_udp.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }
        raw.parse().ok()
    }
}

impl ConfigSection for LoggingConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        if let Some(raw) = self.n1mm_udp.as_deref() {
            let trimmed = raw.trim();
            // Require an explicit recipient and a nonzero port. Wildcard
            // addresses name no recipient and their send behavior can vary by OS.
            let usable = trimmed
                .parse::<std::net::SocketAddr>()
                .is_ok_and(|addr| addr.port() != 0 && !addr.ip().is_unspecified());
            if !trimmed.is_empty() && !usable {
                return Err(ConfigError::InvalidValue {
                    field: "logging.n1mm_udp".to_string(),
                    value: format!(
                        "{raw} (must be an IP address and port, such as 127.0.0.1:12060)"
                    ),
                });
            }
        }
        Ok(())
    }
    fn merge_with(&mut self, other: Self) {
        self.attribution_comment = other.attribution_comment;
        self.n1mm_udp = other.n1mm_udp;
    }
}

#[cfg(test)]
#[path = "logging/tests/n1mm_udp_tests.rs"]
mod n1mm_udp_tests;
