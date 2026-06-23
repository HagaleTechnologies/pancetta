# cqdx.io Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire pancetta to cqdx.io for live rarity, needed DXCC, priority spots, spot reporting, and QSO logging — replacing all hardcoded stubs.

**Architecture:** New `pancetta-cqdx` crate with `CqdxClient` (reqwest HTTP) and `CqdxCache` (in-memory). Coordinator fetches entities+needed on startup, polls priorities every 30s, reports spots+QSOs fire-and-forget. Degraded mode when no PAT configured.

**Tech Stack:** Rust, reqwest (workspace), tokio, serde, chrono. Tests use wiremock for HTTP mocking.

**Design Spec:** `docs/superpowers/specs/2026-04-03-cqdx-integration-design.md`

---

## File Structure

| File | Responsibility |
|------|---------------|
| **Create:** `pancetta-cqdx/Cargo.toml` | Crate manifest |
| **Create:** `pancetta-cqdx/src/lib.rs` | Module re-exports |
| **Create:** `pancetta-cqdx/src/types.rs` | API request/response structs |
| **Create:** `pancetta-cqdx/src/client.rs` | `CqdxClient` HTTP wrapper |
| **Create:** `pancetta-cqdx/src/cache.rs` | `CqdxCache` in-memory store implementing `WorkedStationLookup` |
| **Create:** `pancetta-cqdx/src/error.rs` | `CqdxError` enum |
| **Modify:** `Cargo.toml` (workspace root) | Add `pancetta-cqdx` to members |
| **Modify:** `pancetta-config/src/network.rs` | Add `CqdxConfig` section |
| **Modify:** `pancetta/Cargo.toml` | Add `pancetta-cqdx` dependency |
| **Modify:** `pancetta/src/lib.rs` | Add `pub mod cqdx_bridge;` |
| **Create:** `pancetta/src/cqdx_bridge.rs` | Coordinator-level wiring (startup, polling, reporting) |
| **Modify:** `pancetta/src/coordinator.rs` | Wire cqdx bridge into startup and event loop |
| **Modify:** `pancetta/src/priority_evaluator.rs` | Add rarity support to `CachedStationLookup` |

---

### Task 1: Create pancetta-cqdx Crate with Types and Error

**Files:**
- Create: `pancetta-cqdx/Cargo.toml`
- Create: `pancetta-cqdx/src/lib.rs`
- Create: `pancetta-cqdx/src/types.rs`
- Create: `pancetta-cqdx/src/error.rs`
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Create crate directory and Cargo.toml**

```bash
mkdir -p pancetta-cqdx/src
```

Write `pancetta-cqdx/Cargo.toml`:

```toml
[package]
name = "pancetta-cqdx"
version = "0.1.0"
edition = "2021"
description = "cqdx.io API client and cache for Pancetta"
license = "MIT OR Apache-2.0"

[dependencies]
pancetta-core = { path = "../pancetta-core" }

tokio = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
chrono = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
reqwest = { workspace = true }

[dev-dependencies]
tokio-test = "0.4"
wiremock = "0.6"
```

- [ ] **Step 2: Write error types**

Write `pancetta-cqdx/src/error.rs`:

```rust
//! Error types for the cqdx.io client.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CqdxError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON parsing failed: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Authentication failed: invalid or expired PAT")]
    Unauthorized,

    #[error("Server error: {status} — {message}")]
    Server { status: u16, message: String },

    #[error("Not configured: no PAT token provided")]
    NotConfigured,
}

pub type Result<T> = std::result::Result<T, CqdxError>;
```

- [ ] **Step 3: Write API types**

Write `pancetta-cqdx/src/types.rs`:

```rust
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
```

- [ ] **Step 4: Write lib.rs with module re-exports**

Write `pancetta-cqdx/src/lib.rs`:

```rust
//! cqdx.io API client and cache for Pancetta.
//!
//! Provides `CqdxClient` for HTTP communication with cqdx.io
//! and `CqdxCache` for in-memory session caching of entities,
//! needed status, and rarity scores.

pub mod cache;
pub mod client;
pub mod error;
pub mod types;

pub use cache::CqdxCache;
pub use client::CqdxClient;
pub use error::{CqdxError, Result};
pub use types::*;
```

- [ ] **Step 5: Add crate to workspace**

In the workspace root `Cargo.toml`, add `"pancetta-cqdx"` to both `members` and `default-members` arrays (after `"pancetta-dx"`).

- [ ] **Step 6: Verify it compiles**

Run: `cargo check -p pancetta-cqdx`
Expected: compiles with warnings about unused modules (client/cache not yet written)

- [ ] **Step 7: Commit**

```bash
git add pancetta-cqdx/ Cargo.toml
git commit -m "feat(cqdx): scaffold pancetta-cqdx crate with types and error handling"
```

---

### Task 2: CqdxClient — HTTP Client

**Files:**
- Create: `pancetta-cqdx/src/client.rs`

- [ ] **Step 1: Write tests for CqdxClient**

Add to the bottom of `pancetta-cqdx/src/client.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_client(base_url: &str) -> CqdxClient {
        CqdxClient::new(base_url.to_string(), "pat_test_token".to_string())
    }

    #[tokio::test]
    async fn test_fetch_entities() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/entities"))
            .and(header("Authorization", "Bearer pat_test_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entities": [{
                    "id": 291,
                    "name": "United States",
                    "prefix": "K",
                    "continent": "NA",
                    "cqZone": 5,
                    "ituZone": 8
                }]
            })))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let entities = client.fetch_entities().await.unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].prefix, "K");
        assert_eq!(entities[0].id, 291);
    }

    #[tokio::test]
    async fn test_fetch_needed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/entities/needed"))
            .and(header("Authorization", "Bearer pat_test_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "needed": [{
                    "entityId": 327,
                    "name": "Bouvet Island",
                    "prefix": "3Y/B"
                }]
            })))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let needed = client.fetch_needed().await.unwrap();
        assert_eq!(needed.len(), 1);
        assert_eq!(needed[0].entity_id, 327);
    }

    #[tokio::test]
    async fn test_fetch_priorities() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/spots/priorities"))
            .and(query_param("band", "20m"))
            .and(query_param("limit", "10"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "priorities": [{
                    "callsign": "3Y0J",
                    "grid": "JD15",
                    "frequency": 14074000_u64,
                    "mode": "FT8",
                    "snr": -12,
                    "entity": "Bouvet Island",
                    "rarity": 0.98,
                    "needed": true,
                    "lastSpotted": "2026-04-03T14:22:00Z",
                    "spotCount": 5
                }]
            })))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let spots = client.fetch_priorities(Some("20m"), None, 10).await.unwrap();
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].callsign, "3Y0J");
        assert!((spots[0].rarity - 0.98).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_report_spots() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/spots/ingest"))
            .and(header("Authorization", "Bearer pat_test_token"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let spots = vec![SpotReport {
            callsign: "W1ABC".to_string(),
            grid: Some("FN42".to_string()),
            frequency: 14074000,
            mode: "FT8".to_string(),
            snr: -12,
            timestamp: chrono::Utc::now(),
            reporter: "K2DEF".to_string(),
            reporter_grid: Some("FN31".to_string()),
        }];
        client.report_spots(spots).await.unwrap();
    }

    #[tokio::test]
    async fn test_report_qso() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/qsos"))
            .and(header("Authorization", "Bearer pat_test_token"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let qso = QsoRecord {
            callsign: "JA1ABC".to_string(),
            remote_grid: Some("PM95".to_string()),
            local_grid: Some("FN31".to_string()),
            frequency: 14074000,
            mode: "FT8".to_string(),
            rst_sent: Some("-10".to_string()),
            rst_received: Some("-14".to_string()),
            start_time: chrono::Utc::now(),
            end_time: chrono::Utc::now(),
        };
        client.report_qso(qso).await.unwrap();
    }

    #[tokio::test]
    async fn test_unauthorized_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/entities"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": { "code": "UNAUTHORIZED", "message": "Invalid token" }
            })))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let result = client.fetch_entities().await;
        assert!(matches!(result, Err(CqdxError::Unauthorized)));
    }

    #[tokio::test]
    async fn test_server_error_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/entities"))
            .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "error": { "code": "INTERNAL", "message": "Database unavailable" }
            })))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let result = client.fetch_entities().await;
        assert!(matches!(result, Err(CqdxError::Server { status: 500, .. })));
    }
}
```

- [ ] **Step 2: Implement CqdxClient**

Write the implementation portion of `pancetta-cqdx/src/client.rs` (above the tests module):

```rust
//! HTTP client for the cqdx.io REST API.

use crate::error::{CqdxError, Result};
use crate::types::*;
use reqwest::Client;
use tracing::{debug, warn};

/// HTTP client wrapping reqwest with Bearer token auth.
pub struct CqdxClient {
    http: Client,
    base_url: String,
    token: String,
}

impl CqdxClient {
    /// Create a new client. Caller is responsible for checking config first.
    pub fn new(base_url: String, token: String) -> Self {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");
        Self { http, base_url, token }
    }

    /// Fetch the full DXCC entity list.
    pub async fn fetch_entities(&self) -> Result<Vec<DxccEntity>> {
        let url = format!("{}/api/v1/entities", self.base_url);
        debug!("Fetching DXCC entities from {}", url);
        let resp = self.http.get(&url)
            .bearer_auth(&self.token)
            .send()
            .await?;
        let resp = self.check_status(resp).await?;
        let body: EntitiesResponse = resp.json().await?;
        Ok(body.entities)
    }

    /// Fetch DXCC entities the user still needs.
    pub async fn fetch_needed(&self) -> Result<Vec<NeededEntity>> {
        let url = format!("{}/api/v1/entities/needed", self.base_url);
        debug!("Fetching needed entities from {}", url);
        let resp = self.http.get(&url)
            .bearer_auth(&self.token)
            .send()
            .await?;
        let resp = self.check_status(resp).await?;
        let body: NeededResponse = resp.json().await?;
        Ok(body.needed)
    }

    /// Fetch prioritized spot targets.
    pub async fn fetch_priorities(
        &self,
        band: Option<&str>,
        mode: Option<&str>,
        limit: u32,
    ) -> Result<Vec<PrioritySpot>> {
        let mut url = format!("{}/api/v1/spots/priorities", self.base_url);
        let mut params = vec![("limit", limit.to_string())];
        if let Some(b) = band {
            params.push(("band", b.to_string()));
        }
        if let Some(m) = mode {
            params.push(("mode", m.to_string()));
        }
        debug!("Fetching priority spots from {}", url);
        let resp = self.http.get(&url)
            .bearer_auth(&self.token)
            .query(&params)
            .send()
            .await?;
        let resp = self.check_status(resp).await?;
        let body: PrioritiesResponse = resp.json().await?;
        Ok(body.priorities)
    }

    /// Report a batch of decoded spots.
    pub async fn report_spots(&self, spots: Vec<SpotReport>) -> Result<()> {
        let url = format!("{}/api/v1/spots/ingest", self.base_url);
        debug!("Reporting {} spots to {}", spots.len(), url);
        let req = SpotIngestRequest { spots };
        let resp = self.http.post(&url)
            .bearer_auth(&self.token)
            .json(&req)
            .send()
            .await?;
        self.check_status(resp).await?;
        Ok(())
    }

    /// Report a completed QSO.
    pub async fn report_qso(&self, qso: QsoRecord) -> Result<()> {
        let url = format!("{}/api/v1/qsos", self.base_url);
        debug!("Reporting QSO with {} to {}", qso.callsign, url);
        let req = QsoReportRequest { version: 1, qso };
        let resp = self.http.post(&url)
            .bearer_auth(&self.token)
            .json(&req)
            .send()
            .await?;
        self.check_status(resp).await?;
        Ok(())
    }

    /// Check HTTP response status, converting errors to CqdxError.
    async fn check_status(&self, resp: reqwest::Response) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        if status.as_u16() == 401 {
            return Err(CqdxError::Unauthorized);
        }
        let message = resp.text().await.unwrap_or_else(|_| "unknown".to_string());
        Err(CqdxError::Server {
            status: status.as_u16(),
            message,
        })
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p pancetta-cqdx`
Expected: all 7 tests pass

- [ ] **Step 4: Commit**

```bash
git add pancetta-cqdx/src/client.rs
git commit -m "feat(cqdx): implement CqdxClient with HTTP methods and tests"
```

---

### Task 3: CqdxCache — In-Memory Session Cache

**Files:**
- Create: `pancetta-cqdx/src/cache.rs`
- Modify: `pancetta-cqdx/Cargo.toml` (add `pancetta-qso` dependency)

- [ ] **Step 1: Add pancetta-qso dependency**

Add to `pancetta-cqdx/Cargo.toml` under `[dependencies]`:

```toml
pancetta-qso = { path = "../pancetta-qso" }
```

- [ ] **Step 2: Write tests for CqdxCache**

Add to the bottom of `pancetta-cqdx/src/cache.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use pancetta_qso::priority::WorkedStationLookup;

    fn sample_entities() -> Vec<DxccEntity> {
        vec![
            DxccEntity {
                id: 291, name: "United States".to_string(), prefix: "K".to_string(),
                continent: "NA".to_string(), cq_zone: 5, itu_zone: 8,
            },
            DxccEntity {
                id: 339, name: "Japan".to_string(), prefix: "JA".to_string(),
                continent: "AS".to_string(), cq_zone: 25, itu_zone: 45,
            },
            DxccEntity {
                id: 327, name: "Bouvet Island".to_string(), prefix: "3Y/B".to_string(),
                continent: "AF".to_string(), cq_zone: 38, itu_zone: 67,
            },
        ]
    }

    #[test]
    fn test_resolve_entity_by_prefix() {
        let mut cache = CqdxCache::new();
        cache.load_entities(sample_entities());
        assert_eq!(cache.resolve_entity("K1ABC"), Some(291));
        assert_eq!(cache.resolve_entity("JA1XYZ"), Some(339));
    }

    #[test]
    fn test_resolve_entity_longest_prefix_wins() {
        let mut cache = CqdxCache::new();
        cache.load_entities(sample_entities());
        // "3Y/B" should match Bouvet, not fall through to a shorter prefix
        assert_eq!(cache.resolve_entity("3Y/B1234"), Some(327));
    }

    #[test]
    fn test_resolve_entity_unknown_returns_none() {
        let mut cache = CqdxCache::new();
        cache.load_entities(sample_entities());
        assert_eq!(cache.resolve_entity("ZZ9ZZZ"), None);
    }

    #[test]
    fn test_rarity_from_priorities() {
        let mut cache = CqdxCache::new();
        cache.load_entities(sample_entities());
        cache.update_priorities(vec![PrioritySpot {
            callsign: "3Y0J".to_string(),
            grid: Some("JD15".to_string()),
            frequency: 14074000,
            mode: "FT8".to_string(),
            snr: Some(-12),
            entity: Some("Bouvet Island".to_string()),
            rarity: 0.98,
            needed: true,
            last_spotted: chrono::Utc::now(),
            spot_count: 5,
        }]);
        assert!((cache.rarity("3Y0J") - 0.98).abs() < f64::EPSILON);
    }

    #[test]
    fn test_rarity_unknown_callsign_returns_default() {
        let cache = CqdxCache::new();
        // Default rarity for unknown callsigns is 0.5 (same as old placeholder)
        assert!((cache.rarity("W1ABC") - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_is_needed_dxcc_with_data() {
        let mut cache = CqdxCache::new();
        cache.load_entities(sample_entities());
        cache.load_needed(vec![NeededEntity {
            entity_id: 327,
            name: "Bouvet Island".to_string(),
            prefix: "3Y/B".to_string(),
        }]);
        // Bouvet is needed
        assert!(cache.is_needed_dxcc("3Y/B1234"));
        // US is NOT needed (we've worked it)
        assert!(!cache.is_needed_dxcc("K1ABC"));
    }

    #[test]
    fn test_is_needed_dxcc_empty_means_all_needed() {
        let mut cache = CqdxCache::new();
        cache.load_entities(sample_entities());
        // No needed data loaded → conservative: everything is needed
        assert!(cache.is_needed_dxcc("K1ABC"));
        assert!(cache.is_needed_dxcc("JA1XYZ"));
    }

    #[test]
    fn test_priority_spots_accessor() {
        let mut cache = CqdxCache::new();
        assert!(cache.priority_spots().is_empty());
        cache.update_priorities(vec![PrioritySpot {
            callsign: "3Y0J".to_string(),
            grid: None,
            frequency: 14074000,
            mode: "FT8".to_string(),
            snr: None,
            entity: None,
            rarity: 0.9,
            needed: true,
            last_spotted: chrono::Utc::now(),
            spot_count: 1,
        }]);
        assert_eq!(cache.priority_spots().len(), 1);
    }
}
```

- [ ] **Step 3: Implement CqdxCache**

Write the implementation portion of `pancetta-cqdx/src/cache.rs` (above the tests module):

```rust
//! In-memory session cache for cqdx.io data.
//!
//! Holds DXCC entities, needed status, rarity scores, and priority spots.
//! Populated on startup from cqdx.io API, refreshed by polling.

use crate::types::{DxccEntity, NeededEntity, PrioritySpot};
use std::collections::{HashMap, HashSet};

/// In-memory cache of cqdx.io data for the current session.
#[derive(Debug, Clone)]
pub struct CqdxCache {
    /// All DXCC entities indexed by prefix (longest-prefix-first for matching).
    prefixes: Vec<(String, u32)>,
    /// Entity details by ID.
    entities: HashMap<u32, DxccEntity>,
    /// Entity IDs the user still needs. Empty = all needed (conservative).
    needed_entity_ids: Option<HashSet<u32>>,
    /// Rarity scores from priority spots, keyed by uppercase callsign.
    rarity_scores: HashMap<String, f64>,
    /// Latest priority spot poll results.
    priorities: Vec<PrioritySpot>,
}

impl CqdxCache {
    pub fn new() -> Self {
        Self {
            prefixes: Vec::new(),
            entities: HashMap::new(),
            needed_entity_ids: None,
            rarity_scores: HashMap::new(),
            priorities: Vec::new(),
        }
    }

    /// Load DXCC entity table. Sorts prefixes longest-first for matching.
    pub fn load_entities(&mut self, entities: Vec<DxccEntity>) {
        self.entities.clear();
        self.prefixes.clear();
        for entity in &entities {
            self.entities.insert(entity.id, entity.clone());
            self.prefixes.push((entity.prefix.to_uppercase(), entity.id));
        }
        // Sort longest prefix first so "3Y/B" matches before "3Y"
        self.prefixes.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    }

    /// Load needed entity IDs. Calling this with an empty vec means nothing is needed.
    pub fn load_needed(&mut self, needed: Vec<NeededEntity>) {
        let ids: HashSet<u32> = needed.iter().map(|n| n.entity_id).collect();
        self.needed_entity_ids = Some(ids);
    }

    /// Update priority spots from latest poll. Also updates rarity cache.
    pub fn update_priorities(&mut self, spots: Vec<PrioritySpot>) {
        self.rarity_scores.clear();
        for spot in &spots {
            self.rarity_scores.insert(spot.callsign.to_uppercase(), spot.rarity);
        }
        self.priorities = spots;
    }

    /// Resolve a callsign to its DXCC entity ID using longest-prefix matching.
    pub fn resolve_entity(&self, callsign: &str) -> Option<u32> {
        let upper = callsign.to_uppercase();
        for (prefix, id) in &self.prefixes {
            if upper.starts_with(prefix.as_str()) {
                return Some(*id);
            }
        }
        None
    }

    /// Get rarity score for a callsign. Returns 0.5 (default) if unknown.
    pub fn rarity(&self, callsign: &str) -> f64 {
        self.rarity_scores
            .get(&callsign.to_uppercase())
            .copied()
            .unwrap_or(0.5)
    }

    /// Check if a callsign's DXCC entity is still needed.
    /// Returns true if: no needed data loaded (conservative), or entity is in needed set.
    pub fn is_needed_dxcc(&self, callsign: &str) -> bool {
        match &self.needed_entity_ids {
            None => true, // No data loaded = conservative: everything needed
            Some(ids) => {
                match self.resolve_entity(callsign) {
                    Some(entity_id) => ids.contains(&entity_id),
                    None => false, // Can't resolve = can't be needed
                }
            }
        }
    }

    /// Get current priority spots for frequency nudge decisions.
    pub fn priority_spots(&self) -> &[PrioritySpot] {
        &self.priorities
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p pancetta-cqdx`
Expected: all tests pass (client + cache)

- [ ] **Step 5: Commit**

```bash
git add pancetta-cqdx/
git commit -m "feat(cqdx): implement CqdxCache with entity resolution, rarity, and needed lookups"
```

---

### Task 4: CqdxConfig — Configuration Section

**Files:**
- Modify: `pancetta-config/src/network.rs`

- [ ] **Step 1: Write test for CqdxConfig**

Add to the existing `#[cfg(test)] mod tests` block at the bottom of `pancetta-config/src/network.rs`:

```rust
    #[test]
    fn test_cqdx_defaults() {
        let config = CqdxConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.base_url, "https://cqdx.io");
        assert!(config.token.is_none());
        assert_eq!(config.poll_interval_secs, 30);
    }

    #[test]
    fn test_cqdx_validation_enabled_without_token() {
        let mut config = NetworkConfig::default();
        config.cqdx.enabled = true;
        config.cqdx.token = None;
        assert!(config.validate_section().is_err());
    }

    #[test]
    fn test_cqdx_validation_enabled_with_token() {
        let mut config = NetworkConfig::default();
        config.cqdx.enabled = true;
        config.cqdx.token = Some("pat_abc123".to_string());
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_cqdx_validation_disabled_no_token_ok() {
        let config = NetworkConfig::default();
        // disabled + no token = fine
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_cqdx_validation_poll_interval_bounds() {
        let mut config = NetworkConfig::default();
        config.cqdx.enabled = true;
        config.cqdx.token = Some("pat_abc123".to_string());
        config.cqdx.poll_interval_secs = 5; // too low
        assert!(config.validate_section().is_err());

        config.cqdx.poll_interval_secs = 30;
        assert!(config.validate_section().is_ok());
    }
```

- [ ] **Step 2: Add CqdxConfig struct**

Add the struct definition after the existing `ClublogConfig` struct in `pancetta-config/src/network.rs`:

```rust
/// cqdx.io integration configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CqdxConfig {
    /// Enable cqdx.io integration
    pub enabled: bool,

    /// cqdx.io base URL
    pub base_url: String,

    /// Personal Access Token for authentication
    pub token: Option<String>,

    /// Priority spot poll interval in seconds
    pub poll_interval_secs: u64,
}

impl Default for CqdxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "https://cqdx.io".to_string(),
            token: None,
            poll_interval_secs: 30,
        }
    }
}
```

- [ ] **Step 3: Add `cqdx` field to `NetworkConfig`**

Add to the `NetworkConfig` struct:

```rust
    /// cqdx.io integration configuration
    pub cqdx: CqdxConfig,
```

- [ ] **Step 4: Add validation for CqdxConfig**

In the `validate_section` method of `impl ConfigSection for NetworkConfig`, add after the existing PSKReporter validation:

```rust
        // cqdx.io validation
        if self.cqdx.enabled {
            if self.cqdx.token.is_none() || self.cqdx.token.as_ref().map_or(true, |t| t.is_empty()) {
                return Err(ConfigError::Validation(
                    "cqdx.io integration enabled but no PAT token configured".to_string(),
                ));
            }
            if self.cqdx.poll_interval_secs < 10 {
                return Err(ConfigError::Validation(
                    "cqdx.io poll interval must be at least 10 seconds".to_string(),
                ));
            }
        }
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p pancetta-config`
Expected: all existing tests + 5 new tests pass

- [ ] **Step 6: Commit**

```bash
git add pancetta-config/src/network.rs
git commit -m "feat(config): add CqdxConfig section for cqdx.io integration"
```

---

### Task 5: Add Rarity to CachedStationLookup

**Files:**
- Modify: `pancetta/src/priority_evaluator.rs`

- [ ] **Step 1: Add rarity field and methods**

In `pancetta/src/priority_evaluator.rs`, add a new field to `CachedStationLookup`:

```rust
    /// Rarity scores from cqdx.io, keyed by uppercase callsign.
    rarity_scores: Arc<RwLock<HashMap<String, f64>>>,
```

Add the `HashMap` import to the existing `use std::collections::HashSet;`:

```rust
use std::collections::{HashMap, HashSet};
```

Add to `CachedStationLookup::new()`:

```rust
            rarity_scores: Arc::new(RwLock::new(HashMap::new())),
```

Add a new public method:

```rust
    pub fn update_rarity_scores(&self, scores: HashMap<String, f64>) {
        *self.rarity_scores.write().unwrap() = scores;
    }

    pub fn rarity(&self, callsign: &str) -> f64 {
        self.rarity_scores
            .read()
            .unwrap()
            .get(&callsign.to_uppercase())
            .copied()
            .unwrap_or(0.5)
    }
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p pancetta`
Expected: all existing tests pass (rarity method is additive, no breakage)

- [ ] **Step 3: Commit**

```bash
git add pancetta/src/priority_evaluator.rs
git commit -m "feat: add rarity score support to CachedStationLookup"
```

---

### Task 6: Wire Rarity into PriorityScorer

**Files:**
- Modify: `pancetta-qso/src/priority.rs`

The `PriorityScorer::score_cq_detailed` currently has `let rarity = 0.5;` hardcoded. We need to add a rarity lookup to the `WorkedStationLookup` trait so the scorer can get live rarity.

- [ ] **Step 1: Add rarity method to WorkedStationLookup trait**

In `pancetta-qso/src/priority.rs`, add to the `WorkedStationLookup` trait:

```rust
    /// Get rarity score for a callsign (0.0 = common, 1.0 = rare).
    /// Returns 0.5 as default if unknown.
    fn rarity(&self, callsign: &str) -> f64 {
        let _ = callsign;
        0.5
    }
```

This is a default method so existing implementations (`NullLookup`, `CachedStationLookup`, test lookups) continue to work without changes.

- [ ] **Step 2: Update score_cq_detailed to use trait method**

In `PriorityScorer::score_cq_detailed`, change:

```rust
        let rarity = 0.5; // Placeholder — Phase 4 will integrate pancetta-dx RarityScorer
```

to:

```rust
        let rarity = self.lookup.rarity(callsign);
```

- [ ] **Step 3: Add rarity test**

Add a new test to the existing `#[cfg(test)] mod tests` block:

```rust
    struct RarityLookup {
        rarity_map: HashMap<String, f64>,
    }

    impl WorkedStationLookup for RarityLookup {
        fn is_duplicate(&self, _callsign: &str, _freq_hz: f64) -> bool { false }
        fn is_recent_failure(&self, _callsign: &str) -> bool { false }
        fn is_needed_dxcc(&self, _callsign: &str) -> bool { false }
        fn is_needed_grid(&self, _grid: &str) -> bool { false }
        fn rarity(&self, callsign: &str) -> f64 {
            self.rarity_map.get(callsign).copied().unwrap_or(0.5)
        }
    }

    #[test]
    fn test_rarity_affects_score() {
        let mut rarity_map = HashMap::new();
        rarity_map.insert("3Y0J".to_string(), 0.98);

        let weights = PriorityWeights {
            needed_dxcc: 0.0, needed_grid: 0.0, pota_sota: 0.0,
            rarity: 1.0, signal_strength: 0.0, duplicate_penalty: 0.0,
            recent_failure_penalty: 0.0,
        };

        let scorer_rare = PriorityScorer::new(weights.clone(), Box::new(RarityLookup {
            rarity_map: rarity_map.clone(),
        }));
        let scorer_common = PriorityScorer::new(weights, Box::new(NullLookup));

        let score_rare = scorer_rare.evaluate_cq("3Y0J", None, -10, 14074000.0);
        let score_common = scorer_common.evaluate_cq("W1ABC", None, -10, 14074000.0);

        assert!(score_rare > score_common,
            "Rare station should score higher: {} vs {}", score_rare, score_common);
        assert!((score_rare - 0.98).abs() < 0.01,
            "Rarity-only score should be ~0.98, got {}", score_rare);
    }
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p pancetta-qso -- priority`
Expected: all existing priority tests pass + new rarity test passes

- [ ] **Step 5: Implement rarity in CachedStationLookup**

In `pancetta/src/priority_evaluator.rs`, add to the `impl WorkedStationLookup for CachedStationLookup` block:

```rust
    fn rarity(&self, callsign: &str) -> f64 {
        self.rarity_scores
            .read()
            .unwrap()
            .get(&callsign.to_uppercase())
            .copied()
            .unwrap_or(0.5)
    }
```

- [ ] **Step 6: Run all tests**

Run: `cargo test -p pancetta-qso -p pancetta`
Expected: all tests pass

- [ ] **Step 7: Commit**

```bash
git add pancetta-qso/src/priority.rs pancetta/src/priority_evaluator.rs
git commit -m "feat: wire live rarity scores into PriorityScorer via WorkedStationLookup trait"
```

---

### Task 7: Coordinator cqdx Bridge — Startup and Polling

**Files:**
- Create: `pancetta/src/cqdx_bridge.rs`
- Modify: `pancetta/src/lib.rs`
- Modify: `pancetta/Cargo.toml`

- [ ] **Step 1: Add pancetta-cqdx dependency**

Add to `pancetta/Cargo.toml` under `[dependencies]`:

```toml
pancetta-cqdx = { path = "../pancetta-cqdx" }
```

- [ ] **Step 2: Add module declaration**

Add to `pancetta/src/lib.rs`:

```rust
pub mod cqdx_bridge;
```

- [ ] **Step 3: Write cqdx_bridge.rs**

Write `pancetta/src/cqdx_bridge.rs`:

```rust
//! Coordinator-level wiring for cqdx.io integration.
//!
//! Handles startup (fetch entities + needed), periodic priority polling,
//! and fire-and-forget spot/QSO reporting.

use crate::priority_evaluator::CachedStationLookup;
use pancetta_cqdx::{CqdxClient, CqdxCache, SpotReport, QsoRecord};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::RwLock;
use tokio::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// Manages the cqdx.io integration lifecycle.
pub struct CqdxBridge {
    client: CqdxClient,
    cache: Arc<RwLock<CqdxCache>>,
    cached_lookup: Arc<CachedStationLookup>,
    poll_interval: Duration,
}

impl CqdxBridge {
    /// Create a bridge from config. Returns None if cqdx.io is not configured.
    pub fn from_config(
        config: &pancetta_config::network::CqdxConfig,
        cached_lookup: Arc<CachedStationLookup>,
    ) -> Option<Self> {
        if !config.enabled {
            info!("cqdx.io integration disabled");
            return None;
        }
        let token = config.token.as_ref()?;
        if token.is_empty() {
            warn!("cqdx.io enabled but token is empty, skipping");
            return None;
        }
        let client = CqdxClient::new(config.base_url.clone(), token.clone());
        Some(Self {
            client,
            cache: Arc::new(RwLock::new(CqdxCache::new())),
            cached_lookup,
            poll_interval: Duration::from_secs(config.poll_interval_secs),
        })
    }

    /// Fetch entities and needed data on startup. Populates cache and CachedStationLookup.
    pub async fn startup(&self) -> pancetta_cqdx::Result<()> {
        // Fetch entities
        let entities = self.client.fetch_entities().await?;
        info!("Loaded {} DXCC entities from cqdx.io", entities.len());

        // Fetch needed
        let needed = self.client.fetch_needed().await?;
        info!("Loaded {} needed entities from cqdx.io", needed.len());

        // Populate cache
        let mut cache = self.cache.write().await;
        cache.load_entities(entities);
        cache.load_needed(needed.clone());

        // Update CachedStationLookup needed_dxcc with prefix strings
        let needed_prefixes: std::collections::HashSet<String> = needed
            .iter()
            .map(|n| n.prefix.to_uppercase())
            .collect();
        self.cached_lookup.update_needed_dxcc(needed_prefixes);

        Ok(())
    }

    /// Spawn a background task that polls priority spots every N seconds.
    /// Stops polling if no decode activity for 2 hours (watchdog).
    pub fn spawn_priority_poller(
        &self,
        shutdown: Arc<AtomicBool>,
        last_decode: Arc<RwLock<Option<Instant>>>,
        band: Option<String>,
        mode: Option<String>,
    ) -> tokio::task::JoinHandle<()> {
        let client = self.client.clone();
        let cache = self.cache.clone();
        let cached_lookup = self.cached_lookup.clone();
        let interval = self.poll_interval;
        let watchdog_timeout = Duration::from_secs(2 * 60 * 60); // 2 hours

        tokio::spawn(async move {
            let mut timer = tokio::time::interval(interval);
            let mut consecutive_failures: u32 = 0;
            let mut polling_paused = false;

            loop {
                timer.tick().await;

                if shutdown.load(Ordering::Acquire) {
                    break;
                }

                // Watchdog: check last decode activity
                let last = last_decode.read().await;
                if let Some(ts) = *last {
                    if ts.elapsed() > watchdog_timeout {
                        if !polling_paused {
                            info!("cqdx.io watchdog: no decode activity for 2h, pausing polling");
                            polling_paused = true;
                        }
                        continue;
                    } else if polling_paused {
                        info!("cqdx.io watchdog: decode activity resumed, resuming polling");
                        polling_paused = false;
                        consecutive_failures = 0;
                    }
                }
                drop(last);

                // Stop polling after 3 consecutive failures
                if consecutive_failures >= 3 {
                    continue;
                }

                match client.fetch_priorities(
                    band.as_deref(),
                    mode.as_deref(),
                    20,
                ).await {
                    Ok(spots) => {
                        consecutive_failures = 0;
                        debug!("Polled {} priority spots from cqdx.io", spots.len());

                        // Update rarity scores in CachedStationLookup
                        let rarity_map: HashMap<String, f64> = spots
                            .iter()
                            .map(|s| (s.callsign.to_uppercase(), s.rarity))
                            .collect();
                        cached_lookup.update_rarity_scores(rarity_map);

                        // Update cache
                        let mut c = cache.write().await;
                        c.update_priorities(spots);
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        warn!(
                            "cqdx.io priority poll failed ({}/3): {}",
                            consecutive_failures, e
                        );
                        if consecutive_failures >= 3 {
                            warn!("cqdx.io polling stopped after 3 consecutive failures");
                        }
                    }
                }
            }
        })
    }

    /// Report a batch of spots to cqdx.io. Fire-and-forget (spawns a task).
    pub fn report_spots(&self, spots: Vec<SpotReport>) {
        if spots.is_empty() {
            return;
        }
        let client = self.client.clone();
        tokio::spawn(async move {
            if let Err(e) = client.report_spots(spots).await {
                debug!("Failed to report spots to cqdx.io: {}", e);
            }
        });
    }

    /// Report a completed QSO to cqdx.io. Fire-and-forget (spawns a task).
    pub fn report_qso(&self, qso: QsoRecord) {
        let client = self.client.clone();
        tokio::spawn(async move {
            if let Err(e) = client.report_qso(qso).await {
                debug!("Failed to report QSO to cqdx.io: {}", e);
            }
        });
    }

    /// Get a clone of the cache for read access.
    pub fn cache(&self) -> Arc<RwLock<CqdxCache>> {
        self.cache.clone()
    }
}

// CqdxClient needs to be Clone for fire-and-forget spawns
```

- [ ] **Step 4: Make CqdxClient Clone**

In `pancetta-cqdx/src/client.rs`, add `#[derive(Clone)]` to `CqdxClient`:

```rust
#[derive(Clone)]
pub struct CqdxClient {
    http: Client,
    base_url: String,
    token: String,
}
```

(`reqwest::Client` is already `Clone` — it uses an internal `Arc`.)

- [ ] **Step 5: Verify it compiles**

Run: `cargo check -p pancetta`
Expected: compiles (bridge is not yet wired into coordinator)

- [ ] **Step 6: Commit**

```bash
git add pancetta/src/cqdx_bridge.rs pancetta/src/lib.rs pancetta/Cargo.toml pancetta-cqdx/src/client.rs
git commit -m "feat: add CqdxBridge for coordinator-level cqdx.io wiring"
```

---

### Task 8: Wire CqdxBridge into Coordinator

**Files:**
- Modify: `pancetta/src/coordinator.rs`

This task wires the bridge into the coordinator's startup sequence and event loop.

- [ ] **Step 1: Add cqdx_bridge field to ApplicationCoordinator**

Add a new field to the `ApplicationCoordinator` struct (after `cached_lookup`):

```rust
    cqdx_bridge: Option<crate::cqdx_bridge::CqdxBridge>,
```

- [ ] **Step 2: Initialize in constructor**

In the `ApplicationCoordinator::new()` or equivalent initialization method, set:

```rust
            cqdx_bridge: None,
```

- [ ] **Step 3: Wire startup in the component initialization**

Find the coordinator's startup sequence (where `start_autonomous_component` is called). Add cqdx.io initialization **before** the autonomous component starts, so that rarity/needed data is available when `PriorityScorer` is created.

Add after reading the config but before `start_autonomous_component`:

```rust
        // Initialize cqdx.io integration
        {
            let config = self.config.read().await;
            if let Some(bridge) = crate::cqdx_bridge::CqdxBridge::from_config(
                &config.network.cqdx,
                self.cached_lookup.clone(),
            ) {
                drop(config);
                match bridge.startup().await {
                    Ok(()) => {
                        info!("cqdx.io integration initialized");
                        // Start priority poller
                        let poller_handle = bridge.spawn_priority_poller(
                            self.shutdown_signal.clone(),
                            self.last_decode_timestamp.clone(),
                            None, // band — could read from config
                            None, // mode — could read from config
                        );
                        self.cqdx_bridge = Some(bridge);
                    }
                    Err(e) => {
                        warn!("cqdx.io startup failed, running in degraded mode: {}", e);
                    }
                }
            } else {
                drop(config);
                info!("cqdx.io integration not configured, running in degraded mode");
            }
        }
```

- [ ] **Step 4: Wire QSO reporting**

In the QSO event handler (around line 1477 in `coordinator.rs`), after the `QsoCompleted` handling that calls `qso_lookup.record_worked(their_call)`, add cqdx.io QSO reporting:

```rust
                                // Report QSO to cqdx.io
                                if let Some(ref bridge) = /* access to cqdx_bridge */ {
                                    bridge.report_qso(pancetta_cqdx::QsoRecord {
                                        callsign: their_call.clone(),
                                        remote_grid: metadata.their_grid.clone(),
                                        local_grid: metadata.our_grid.clone(),
                                        frequency: metadata.frequency.unwrap_or(0) as u64,
                                        mode: "FT8".to_string(), // TODO: get from metadata
                                        rst_sent: metadata.rst_sent.clone(),
                                        rst_received: metadata.rst_received.clone(),
                                        start_time: metadata.start_time.unwrap_or_else(chrono::Utc::now),
                                        end_time: chrono::Utc::now(),
                                    });
                                }
```

Note: The exact field access patterns depend on what `metadata` contains. The implementer should read the `QsoMetadata` struct to get the correct field names. The key point is: after `record_worked`, also call `bridge.report_qso()`.

- [ ] **Step 5: Wire spot reporting**

In the autonomous operator's decode handler (the `slot_interval.tick()` branch around line 2318), after decoded messages are processed, batch and report spots to cqdx.io.

The implementer should add spot collection to the decode event processing. After each decode cycle collects `slot_messages`, build spot reports and call `bridge.report_spots()`:

```rust
                                // Report decoded spots to cqdx.io
                                if let Some(ref bridge) = /* access to cqdx_bridge */ {
                                    let spot_reports: Vec<pancetta_cqdx::SpotReport> = slot_messages
                                        .iter()
                                        .map(|msg| pancetta_cqdx::SpotReport {
                                            callsign: msg.callsign.clone(),
                                            grid: msg.grid.clone(),
                                            frequency: msg.frequency as u64,
                                            mode: "FT8".to_string(),
                                            snr: msg.snr as i32,
                                            timestamp: chrono::Utc::now(),
                                            reporter: our_callsign.clone(),
                                            reporter_grid: our_grid.clone(),
                                        })
                                        .collect();
                                    bridge.report_spots(spot_reports);
                                }
```

Note: The exact field names on `DecodedMessageInfo` should be verified by the implementer. The pattern is: convert each decoded message to a `SpotReport` and batch-send.

- [ ] **Step 6: Verify it compiles**

Run: `cargo check -p pancetta`
Expected: compiles

- [ ] **Step 7: Run existing tests**

Run: `cargo test -p pancetta`
Expected: all existing tests pass (cqdx bridge is `None` in tests, no behavior change)

- [ ] **Step 8: Commit**

```bash
git add pancetta/src/coordinator.rs
git commit -m "feat: wire CqdxBridge into coordinator startup, QSO reporting, and spot reporting"
```

---

### Task 9: Integration Tests

**Files:**
- Create: `pancetta-cqdx/tests/integration.rs`

- [ ] **Step 1: Write integration tests**

Write `pancetta-cqdx/tests/integration.rs`:

```rust
//! Integration tests for the cqdx.io client + cache interaction.

use pancetta_cqdx::{CqdxClient, CqdxCache};
use pancetta_cqdx::types::*;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Full startup flow: fetch entities, fetch needed, verify cache state.
#[tokio::test]
async fn test_startup_flow_entities_and_needed() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/entities"))
        .and(header("Authorization", "Bearer pat_test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entities": [
                { "id": 291, "name": "United States", "prefix": "K", "continent": "NA", "cqZone": 5, "ituZone": 8 },
                { "id": 339, "name": "Japan", "prefix": "JA", "continent": "AS", "cqZone": 25, "ituZone": 45 },
                { "id": 327, "name": "Bouvet Island", "prefix": "3Y/B", "continent": "AF", "cqZone": 38, "ituZone": 67 }
            ]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/entities/needed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "needed": [
                { "entityId": 327, "name": "Bouvet Island", "prefix": "3Y/B" }
            ]
        })))
        .mount(&server)
        .await;

    let client = CqdxClient::new(server.uri(), "pat_test".to_string());
    let mut cache = CqdxCache::new();

    // Simulate startup flow
    let entities = client.fetch_entities().await.unwrap();
    assert_eq!(entities.len(), 3);
    cache.load_entities(entities);

    let needed = client.fetch_needed().await.unwrap();
    assert_eq!(needed.len(), 1);
    cache.load_needed(needed);

    // Verify cache state
    assert!(cache.is_needed_dxcc("3Y/B1234")); // Bouvet is needed
    assert!(!cache.is_needed_dxcc("K1ABC"));    // US is not needed
    assert!(!cache.is_needed_dxcc("JA1XYZ"));   // Japan is not needed
}

/// Priority poll updates rarity scores in cache.
#[tokio::test]
async fn test_priority_poll_updates_rarity() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/spots/priorities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "priorities": [
                {
                    "callsign": "3Y0J",
                    "grid": "JD15",
                    "frequency": 14074000_u64,
                    "mode": "FT8",
                    "snr": -12,
                    "entity": "Bouvet Island",
                    "rarity": 0.98,
                    "needed": true,
                    "lastSpotted": "2026-04-03T14:22:00Z",
                    "spotCount": 5
                },
                {
                    "callsign": "K1ABC",
                    "frequency": 14074000_u64,
                    "mode": "FT8",
                    "rarity": 0.02,
                    "needed": false,
                    "lastSpotted": "2026-04-03T14:22:00Z",
                    "spotCount": 1
                }
            ]
        })))
        .mount(&server)
        .await;

    let client = CqdxClient::new(server.uri(), "pat_test".to_string());
    let mut cache = CqdxCache::new();

    let spots = client.fetch_priorities(None, None, 20).await.unwrap();
    cache.update_priorities(spots);

    assert!((cache.rarity("3Y0J") - 0.98).abs() < f64::EPSILON);
    assert!((cache.rarity("K1ABC") - 0.02).abs() < f64::EPSILON);
    assert!((cache.rarity("UNKNOWN") - 0.5).abs() < f64::EPSILON); // default
}

/// Spot and QSO reporting doesn't fail on valid server response.
#[tokio::test]
async fn test_spot_and_qso_reporting() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/v1/spots/ingest"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/v1/qsos"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let client = CqdxClient::new(server.uri(), "pat_test".to_string());

    // Report spots
    let spots = vec![SpotReport {
        callsign: "W1ABC".to_string(),
        grid: Some("FN42".to_string()),
        frequency: 14074000,
        mode: "FT8".to_string(),
        snr: -12,
        timestamp: chrono::Utc::now(),
        reporter: "K2DEF".to_string(),
        reporter_grid: Some("FN31".to_string()),
    }];
    client.report_spots(spots).await.unwrap();

    // Report QSO
    let qso = QsoRecord {
        callsign: "JA1ABC".to_string(),
        remote_grid: Some("PM95".to_string()),
        local_grid: Some("FN31".to_string()),
        frequency: 14074000,
        mode: "FT8".to_string(),
        rst_sent: Some("-10".to_string()),
        rst_received: Some("-14".to_string()),
        start_time: chrono::Utc::now(),
        end_time: chrono::Utc::now(),
    };
    client.report_qso(qso).await.unwrap();
}

/// Degraded mode: CqdxCache with no data returns conservative defaults.
#[test]
fn test_degraded_mode_defaults() {
    let cache = CqdxCache::new();
    // No entities loaded: can't resolve anything
    assert_eq!(cache.resolve_entity("K1ABC"), None);
    // No needed data: everything is needed (conservative)
    assert!(cache.is_needed_dxcc("K1ABC"));
    // No rarity data: default 0.5
    assert!((cache.rarity("K1ABC") - 0.5).abs() < f64::EPSILON);
    // No priority spots
    assert!(cache.priority_spots().is_empty());
}
```

- [ ] **Step 2: Run integration tests**

Run: `cargo test -p pancetta-cqdx`
Expected: all unit tests + integration tests pass

- [ ] **Step 3: Commit**

```bash
git add pancetta-cqdx/tests/integration.rs
git commit -m "test(cqdx): add integration tests for startup flow, polling, reporting, and degraded mode"
```

---

### Task 10: Final Verification

**Files:** None (verification only)

- [ ] **Step 1: Run full workspace tests**

Run: `cargo test --workspace`
Expected: all tests pass across all crates

- [ ] **Step 2: Check for compiler warnings**

Run: `cargo check --workspace 2>&1 | grep warning`
Expected: no new warnings (existing warnings are OK)

- [ ] **Step 3: Verify degraded mode (no config change)**

The existing test suite should pass without any `[cqdx]` config because `CqdxBridge::from_config` returns `None` when disabled (the default). All existing behavior is preserved.

- [ ] **Step 4: Commit any remaining fixes**

If any warnings or test failures were found, fix and commit:

```bash
git add -A
git commit -m "fix: resolve warnings and test failures from cqdx integration"
```

---

## Summary of Tasks

| Task | What | Tests |
|------|------|-------|
| 1 | Scaffold pancetta-cqdx crate + types + errors | Compile check |
| 2 | CqdxClient HTTP methods | 7 unit tests (wiremock) |
| 3 | CqdxCache in-memory store | 8 unit tests |
| 4 | CqdxConfig section | 5 config tests |
| 5 | Add rarity to CachedStationLookup | Existing tests pass |
| 6 | Wire rarity into PriorityScorer | 1 new test + existing pass |
| 7 | CqdxBridge coordinator module | Compile check |
| 8 | Wire bridge into coordinator | Existing tests pass |
| 9 | Integration tests | 4 integration tests |
| 10 | Final workspace verification | Full test suite |
