//! Opt-in decode alerts. Tier names follow the shared DX scorer vocabulary.
use crate::{ConfigError, ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DxTier {
    #[default]
    Atno,
    PerBandDxccNew,
    SpecialStation,
    PerBandGridNew,
    Standard,
    Suspect,
}

impl std::fmt::Display for DxTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Atno => "atno",
            Self::PerBandDxccNew => "per_band_dxcc_new",
            Self::SpecialStation => "special_station",
            Self::PerBandGridNew => "per_band_grid_new",
            Self::Standard => "standard",
            Self::Suspect => "suspect",
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BellAlertConfig {
    pub enabled: bool,
    pub min_tier: DxTier,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NtfyAlertConfig {
    pub enabled: bool,
    pub min_tier: DxTier,
    pub server: String,
    pub topic: String,
}
impl Default for NtfyAlertConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_tier: DxTier::Atno,
            server: "https://ntfy.sh".into(),
            topic: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WebhookAlertConfig {
    pub enabled: bool,
    pub min_tier: DxTier,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertsConfig {
    pub dedupe_secs: u64,
    pub bell: BellAlertConfig,
    pub ntfy: NtfyAlertConfig,
    pub webhook: WebhookAlertConfig,
}
impl Default for AlertsConfig {
    fn default() -> Self {
        Self {
            dedupe_secs: 300,
            bell: BellAlertConfig::default(),
            ntfy: NtfyAlertConfig::default(),
            webhook: WebhookAlertConfig::default(),
        }
    }
}
impl ConfigSection for AlertsConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        if self.dedupe_secs == 0 {
            return Err(ConfigError::InvalidValue {
                field: "alerts.dedupe_secs".into(),
                value: "0".into(),
            });
        }
        if self.ntfy.enabled && self.ntfy.topic.trim().is_empty() {
            return Err(ConfigError::MissingRequired("alerts.ntfy.topic".into()));
        }
        for (enabled, value, field) in [
            (
                self.webhook.enabled,
                &self.webhook.url,
                "alerts.webhook.url",
            ),
            (self.ntfy.enabled, &self.ntfy.server, "alerts.ntfy.server"),
        ] {
            if enabled
                && !url::Url::parse(value).is_ok_and(|url| {
                    matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
                })
            {
                return Err(ConfigError::InvalidValue {
                    field: field.into(),
                    value: value.clone(),
                });
            }
        }
        Ok(())
    }
    fn merge_with(&mut self, other: Self) {
        *self = other;
    }
}
