//! HTTP client for the cqdx.io REST API.

use crate::error::{CqdxError, Result};
use crate::types::*;
use reqwest::Client;
use tracing::debug;

/// HTTP client wrapping reqwest with Bearer token auth.
pub struct CqdxClient {
    http: Client,
    base_url: String,
    token: String,
}

impl CqdxClient {
    pub fn new(base_url: String, token: String) -> Self {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");
        Self { http, base_url, token }
    }

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

    pub async fn fetch_priorities(
        &self,
        band: Option<&str>,
        mode: Option<&str>,
        limit: u32,
    ) -> Result<Vec<PrioritySpot>> {
        let mut params = vec![("limit", limit.to_string())];
        if let Some(b) = band {
            params.push(("band", b.to_string()));
        }
        if let Some(m) = mode {
            params.push(("mode", m.to_string()));
        }
        let url = format!("{}/api/v1/spots/priorities", self.base_url);
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
