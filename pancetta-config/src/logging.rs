//! Attribution settings for newly logged contacts.
use crate::{ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};

/// Rendering preferences for the QSO log.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// Append the Pancetta/cqdx software attribution to new ADIF comments.
    pub attribution_comment: bool,
}

impl ConfigSection for LoggingConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        Ok(())
    }
    fn merge_with(&mut self, other: Self) {
        self.attribution_comment = other.attribution_comment;
    }
}
