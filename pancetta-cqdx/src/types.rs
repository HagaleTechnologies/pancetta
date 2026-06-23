//! Request and response types for the cqdx.io REST API.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// --- Entities ---

#[derive(Debug, Clone, Deserialize)]
pub struct DxccEntity {
    #[serde(rename = "adifNumber")]
    pub adif_number: u32,
    #[serde(rename = "entityName")]
    pub entity_name: String,
    pub prefix: String,
    pub continent: String,
    #[serde(rename = "cqZone")]
    pub cq_zone: u8,
    #[serde(rename = "ituZone")]
    pub itu_zone: u8,
    #[serde(rename = "rarityRank")]
    pub rarity_rank: Option<u32>,
    #[serde(rename = "rarityTier")]
    pub rarity_tier: String,
    #[serde(rename = "isDeleted")]
    pub is_deleted: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EntitiesResponse {
    pub entities: Vec<DxccEntity>,
}

// --- Needed ---

#[derive(Debug, Clone, Deserialize)]
pub struct NeededEntity {
    #[serde(rename = "entityId")]
    pub entity_id: u32,
    pub name: String,
    pub prefix: String,
    /// True when the entity is needed on every band (an all-time new one, ATNO).
    /// False marks a band-fill (already confirmed on some other band). Defaults
    /// to false when the server omits the field (older responses).
    #[serde(default)]
    pub atno: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NeededResponse {
    pub needed: Vec<NeededEntity>,
}

/// Response envelope for `GET /api/v1/entities/needed-grids`.
///
/// The needed-grids endpoint returns the Maidenhead grid squares the operator
/// still needs as a flat list of grid-field strings (e.g. `["JD15", "FN42"]`).
/// Mirrors the `needed` envelope of [`NeededResponse`]. The endpoint may not
/// exist on the cqdx.io server yet — see `CqdxClient::fetch_needed_grids`,
/// which degrades gracefully (empty list) when it is absent.
#[derive(Debug, Clone, Deserialize)]
pub struct NeededGridsResponse {
    pub grids: Vec<String>,
}

// --- Live Spot Groups ---

/// A spot group from the cqdx.io live feed, aggregated by (dxCall, band, mode).
#[derive(Debug, Clone, Deserialize)]
pub struct SpotGroup {
    #[serde(rename = "dxCall")]
    pub dx_call: String,
    pub band: String,
    pub mode: String,
    #[serde(rename = "dxDxcc")]
    pub dx_dxcc: u32,
    #[serde(rename = "dxEntityName")]
    pub dx_entity_name: String,
    #[serde(rename = "dxContinent")]
    pub dx_continent: String,
    #[serde(rename = "dxCqZone")]
    pub dx_cq_zone: u8,
    #[serde(rename = "dxGrid")]
    pub dx_grid: Option<String>,
    #[serde(rename = "rarityRank")]
    pub rarity_rank: Option<u32>,
    #[serde(rename = "rarityTier")]
    pub rarity_tier: String,
    pub frequency: u64,
    #[serde(rename = "bestSnr")]
    pub best_snr: Option<i32>,
    #[serde(rename = "reporterCount")]
    pub reporter_count: u32,
    pub sources: Vec<String>,
    #[serde(rename = "firstSeen")]
    pub first_seen: i64,
    #[serde(rename = "lastSeen")]
    pub last_seen: i64,
    pub confidence: f64,
    #[serde(rename = "isNotable", default)]
    pub is_notable: bool,
    #[serde(rename = "notableType")]
    pub notable_type: Option<String>,
}

/// Envelope for `GET /api/v1/spots?live=true`.
///
/// # Assumed response shape
///
/// ```json
/// { "groups": [ { "dxCall": "3Y0J", "band": "20m", ... } ] }
/// ```
///
/// The top-level key `groups` has **not** been verified against the live cqdx.io API.
/// Run the `test_live_spots_envelope` integration test (requires `CQDX_TOKEN`) to confirm:
///
/// ```bash
/// CQDX_TOKEN=pat_xxx cargo test -p pancetta-cqdx test_live_spots_envelope -- --ignored --nocapture
/// ```
///
/// Note: the API requirements doc describes a `/spots/priorities` endpoint with a `priorities`
/// envelope key and a simpler flat struct. This client instead calls `/spots?live=true` with
/// richer `SpotGroup` objects — aligned with the Durable Object snapshot design, not the
/// initial requirements doc. The server-side endpoint shape must match this struct.
#[derive(Debug, Clone, Deserialize)]
pub struct LiveSpotsResponse {
    pub groups: Vec<SpotGroup>,
}

// --- Spot Reporting ---

#[derive(Debug, Clone, Serialize)]
pub struct SpotReport {
    pub callsign: String,
    pub grid: Option<String>,
    pub frequency: u64,
    pub mode: String,
    pub snr: i32,
    pub timestamp: DateTime<Utc>,
    pub reporter: String,
    #[serde(rename = "reporterGrid")]
    pub reporter_grid: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpotReportRequest {
    pub spots: Vec<SpotReport>,
}

// --- QSO Reporting ---

#[derive(Debug, Clone, Serialize)]
pub struct QsoRecord {
    pub callsign: String,
    #[serde(rename = "remoteGrid")]
    pub remote_grid: Option<String>,
    #[serde(rename = "localGrid")]
    pub local_grid: Option<String>,
    pub frequency: u64,
    pub mode: String,
    #[serde(rename = "rstSent")]
    pub rst_sent: Option<String>,
    #[serde(rename = "rstReceived")]
    pub rst_received: Option<String>,
    #[serde(rename = "startTime")]
    pub start_time: DateTime<Utc>,
    #[serde(rename = "endTime")]
    pub end_time: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QsoReportRequest {
    pub version: u32,
    pub qso: QsoRecord,
}

/// Outcome of a [`crate::CqdxClient::log_qso`] upload. A duplicate is a
/// successful, non-fatal result (the QSO is already in the logbook) — the
/// caller should treat it like `Logged`, not an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QsoUploadOutcome {
    /// The QSO was accepted and recorded in the cqdx.io logbook.
    Logged,
    /// cqdx.io already had this QSO; nothing to do.
    Duplicate,
}

/// Lenient view of an optional `POST /api/v1/qsos` 2xx response body, used only
/// to recognise an in-band duplicate signal. The endpoint contract does not
/// pin the body shape, so every field is optional and unknown fields are
/// ignored; a missing / non-JSON body simply means "logged".
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct QsoUploadBody {
    /// e.g. `"duplicate"`, `"logged"`, `"created"`.
    #[serde(default)]
    status: Option<String>,
    /// e.g. `{"duplicate": true}`.
    #[serde(default)]
    duplicate: Option<bool>,
}

impl QsoUploadBody {
    /// True when the body indicates the QSO was already on file.
    pub(crate) fn is_duplicate(&self) -> bool {
        self.duplicate.unwrap_or(false)
            || self
                .status
                .as_deref()
                .map(|s| s.eq_ignore_ascii_case("duplicate"))
                .unwrap_or(false)
    }
}

// --- Utilities ---

/// Convert a CQDX rarityRank (1=rarest, ~340=most common) to a 0.0–1.0 float
/// where 1.0 = rarest. Returns 0.5 (neutral) if rank is None.
pub fn rank_to_rarity(rank: Option<u32>) -> f64 {
    match rank {
        Some(r) => 1.0 - (r.saturating_sub(1) as f64) / 339.0,
        None => 0.5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rank_to_rarity_boundaries() {
        assert!((rank_to_rarity(Some(1)) - 1.0).abs() < f64::EPSILON);
        assert!((rank_to_rarity(Some(340)) - 0.0).abs() < 0.01);
        assert!((rank_to_rarity(None) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_rank_to_rarity_midpoint() {
        let mid = rank_to_rarity(Some(170));
        assert!(mid > 0.4 && mid < 0.6);
    }
}
