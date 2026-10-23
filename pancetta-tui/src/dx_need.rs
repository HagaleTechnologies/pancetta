//! Prepared, local worked-history indicators for DX Hunter.
//! Providers read memory only; no database or network calls belong in a frame.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DxNeedStatus {
    /// None means missing/invalid data or unavailable history.
    pub grid_needed: Option<bool>,
    pub state: Option<String>,
    pub state_needed: Option<bool>,
    /// PAN-93: cqdx.io needed-entity verdict; None while the list is not loaded.
    pub dxcc_needed: Option<bool>,
    /// PAN-93: the loaded list's scope and time, when `dxcc_needed` is known.
    pub dxcc_list: Option<NeedListSource>,
}

/// PAN-93: where a needed-entity verdict came from, so reason surfaces can
/// name the cqdx list's band scope and age.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeedListSource {
    /// Per-band scope (e.g. "20M"); None for an all-bands list.
    pub band: Option<String>,
    pub fetched_at: chrono::DateTime<chrono::Utc>,
}

pub trait DxNeedLookup: Send + Sync {
    fn lookup_need(
        &self,
        callsign: &str,
        grid: Option<&str>,
        known_state: Option<&str>,
        current_frequency_hz: f64,
    ) -> DxNeedStatus;
}

/// The display and cache adapter use the same validated four-character square.
/// Validate ASCII before the existing parser or any byte slicing sees network text.
pub fn canonical_grid_square(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if !raw.is_ascii() || raw.len() < 4 {
        return None;
    }
    let grid = pancetta_core::gridsquare::GridSquare::new(raw).ok()?;
    Some(grid.grid()[..4].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_square_requires_a_complete_valid_ascii_locator() {
        for value in ["em34", " EM34ab ", "EM34cd12"] {
            assert_eq!(canonical_grid_square(value).as_deref(), Some("EM34"));
        }
        for value in ["", "EM", "EM3", "ZZ34", "EM34zz", "ééé", "EM34☃"] {
            assert_eq!(canonical_grid_square(value), None, "{value}");
        }
    }
}
