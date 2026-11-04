//! Operator pins and expiry for automatically remembered DX candidates.
use crate::{ConfigError, ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchlistConfig {
    pub ttl_secs: u64,
    pub pinned: Vec<String>,
}
impl Default for WatchlistConfig {
    fn default() -> Self {
        Self {
            ttl_secs: 150,
            pinned: Vec::new(),
        }
    }
}
impl ConfigSection for WatchlistConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        if self.ttl_secs == 0 {
            return Err(ConfigError::InvalidValue {
                field: "watchlist.ttl_secs".into(),
                value: "0".into(),
            });
        }
        Ok(())
    }
    fn merge_with(&mut self, other: Self) {
        *self = other;
    }
}
