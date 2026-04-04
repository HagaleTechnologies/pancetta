//! Request and response types for the cqdx.io REST API.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// --- Entities ---

#[derive(Debug, Clone, Deserialize)]
pub struct DxccEntity {
    pub id: u32,
    pub name: String,
    pub prefix: String,
    pub continent: String,
    #[serde(rename = "cqZone")]
    pub cq_zone: u8,
    #[serde(rename = "ituZone")]
    pub itu_zone: u8,
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct NeededResponse {
    pub needed: Vec<NeededEntity>,
}

// --- Priority Spots ---

#[derive(Debug, Clone, Deserialize)]
pub struct PrioritySpot {
    pub callsign: String,
    pub grid: Option<String>,
    pub frequency: u64,
    pub mode: String,
    pub snr: Option<i32>,
    pub entity: Option<String>,
    pub rarity: f64,
    pub needed: bool,
    #[serde(rename = "lastSpotted")]
    pub last_spotted: DateTime<Utc>,
    #[serde(rename = "spotCount")]
    pub spot_count: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrioritiesResponse {
    pub priorities: Vec<PrioritySpot>,
}

// --- Spot Ingest ---

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
pub struct SpotIngestRequest {
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
