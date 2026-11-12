//! Prepared, local worked-history indicators for DX Hunter.
//! Providers read memory only; no database or network calls belong in a frame.
//! Providers also receive the row's DXCC entity, resolved once when the row
//! was created, and must not resolve the callsign's entity again (PAN-284).

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DxNeedStatus {
    /// None means missing/invalid data or unavailable history.
    pub grid_needed: Option<bool>,
    pub state: Option<String>,
    pub state_needed: Option<bool>,
    /// Needed-entity verdict from cqdx or the local logbook according to
    /// the configured policy; None while neither has answered.
    pub dxcc_needed: Option<bool>,
    /// PAN-93: the loaded cqdx list's scope and time, when cqdx answered.
    pub dxcc_list: Option<NeedListSource>,
    /// PAN-347: who answered `dxcc_needed`; None exactly when `dxcc_needed` is None.
    pub dxcc_source: Option<pancetta_qso::priority::NeedSource>,
    /// PAN-347: the row's entity in the local logbook; None before the log
    /// loads or without an entity.
    pub entity_standing: Option<EntityStanding>,
    /// PAN-347: no contact with the entity in the row's mode on any band.
    /// None without a locally decoded mode, the log, or an entity, and for
    /// the home entity.
    pub new_mode: Option<bool>,
    /// Local confirmation policy is active after a successful logbook seed.
    pub unconfirmed_policy: Option<bool>,
}

/// PAN-347: an entity's standing in the local logbook, all bands and modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityStanding {
    NotWorked,
    WorkedUnconfirmed,
    Confirmed,
}

impl EntityStanding {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotWorked => "not worked",
            Self::WorkedUnconfirmed => "worked-unconfirmed",
            Self::Confirmed => "confirmed",
        }
    }
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
    /// `entity` is the row's `DxStation::offline_entity`, resolved once when
    /// the row was created. Frames call this per row: use `entity`, never
    /// resolve `callsign` again. `mode` is the row's locally decoded mode
    /// (PAN-347), None for rows with no local decode.
    fn lookup_need(
        &self,
        callsign: &str,
        entity: Option<&'static str>,
        grid: Option<&str>,
        known_state: Option<&str>,
        current_frequency_hz: f64,
        mode: Option<&str>,
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
