//! HTTP client for the cqdx.io REST API.

use crate::error::{CqdxError, Result};
use crate::types::*;
use reqwest::Client;
use std::fmt;
use tracing::debug;

/// PAT token wrapped to prevent accidental leakage via Debug or Display.
///
/// Tokens are only ever exposed through [`PatToken::expose_secret`]; the
/// `Debug` impl deliberately redacts the value so that derive(Debug) on
/// containing structs (CqdxClient, etc.) doesn't surface the token in
/// log output, panic messages, or error reports.
#[derive(Clone)]
pub struct PatToken(String);

impl PatToken {
    /// Borrow the underlying token string. Use sparingly — every call site
    /// is a potential leak vector. Prefer keeping the `PatToken` wrapped
    /// and reaching for this only at the network boundary.
    pub(crate) fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PatToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PatToken(***)")
    }
}

/// HTTP client wrapping reqwest with Bearer token auth.
#[derive(Clone)]
pub struct CqdxClient {
    http: Client,
    base_url: String,
    token: PatToken,
}

/// Maximum response body size accepted from the API (10 MB).
const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;

impl CqdxClient {
    /// Create a new client for the given base URL and PAT token.
    ///
    /// The base URL must use `https://` or `http://localhost` (for development).
    /// Returns an error if the URL scheme is not allowed.
    pub fn new(base_url: String, token: String) -> Result<Self> {
        // SSRF mitigation: only allow HTTPS or localhost HTTP
        let url_lower = base_url.to_lowercase();
        if !url_lower.starts_with("https://")
            && !url_lower.starts_with("http://localhost")
            && !url_lower.starts_with("http://127.0.0.1")
        {
            return Err(CqdxError::InvalidBaseUrl(format!(
                "base_url must use https:// or http://localhost, got: {}",
                base_url
            )));
        }

        // PAT format validation. cqdx.io PATs are at least 16 chars and
        // start with `pat_`. We tolerate short test fixtures by relaxing
        // the prefix check in #[cfg(test)] only; production callers should
        // see an early failure on a malformed token instead of a 401 at
        // the first request.
        if token.is_empty() {
            return Err(CqdxError::InvalidToken("token is empty"));
        }
        #[cfg(not(test))]
        {
            if token.len() < 16 {
                return Err(CqdxError::InvalidToken(
                    "token is suspiciously short (< 16 chars)",
                ));
            }
            if !token.starts_with("pat_") {
                return Err(CqdxError::InvalidToken("token must start with `pat_`"));
            }
        }

        // reqwest's Client::build can fail at runtime if the host TLS
        // stack is misconfigured. Surface as a Result instead of panicking
        // — pancetta runs as a long-lived daemon and a startup panic here
        // takes the whole TUI down with no operator-visible reason.
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(CqdxError::HttpInit)?;
        Ok(Self {
            http,
            base_url,
            token: PatToken(token),
        })
    }

    pub async fn fetch_entities(&self) -> Result<Vec<DxccEntity>> {
        let url = format!("{}/api/v1/entities", self.base_url);
        debug!("Fetching DXCC entities from {}", url);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(self.token.expose_secret())
            .send()
            .await?;
        let resp = self.check_status(resp).await?;
        let body: EntitiesResponse = self.checked_json(resp).await?;
        Ok(body.entities)
    }

    pub async fn fetch_needed(&self) -> Result<Vec<NeededEntity>> {
        let url = format!("{}/api/v1/entities/needed", self.base_url);
        debug!("Fetching needed entities from {}", url);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(self.token.expose_secret())
            .send()
            .await?;
        let resp = self.check_status(resp).await?;
        let body: NeededResponse = self.checked_json(resp).await?;
        Ok(body.needed)
    }

    /// Fetch live spot groups from the cqdx.io Durable Object snapshot.
    /// Edge-cached with 10s TTL — safe to poll every 30s.
    pub async fn fetch_live_spots(
        &self,
        band: Option<&str>,
        mode: Option<&str>,
    ) -> Result<Vec<SpotGroup>> {
        let mut params = vec![("live", "true".to_string())];
        if let Some(b) = band {
            params.push(("band", b.to_string()));
        }
        if let Some(m) = mode {
            params.push(("mode", m.to_string()));
        }
        let url = format!("{}/api/v1/spots", self.base_url);
        debug!("Fetching live spots from {}", url);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(self.token.expose_secret())
            .query(&params)
            .send()
            .await?;
        let resp = self.check_status(resp).await?;
        let body: LiveSpotsResponse = self.checked_json(resp).await?;
        Ok(body.groups)
    }

    pub async fn report_spots(&self, spots: Vec<SpotReport>) -> Result<()> {
        let url = format!("{}/api/v1/spots/report", self.base_url);
        debug!("Reporting {} spots to {}", spots.len(), url);
        let req = SpotReportRequest { spots };
        let resp = self
            .http
            .post(&url)
            .bearer_auth(self.token.expose_secret())
            .json(&req)
            .send()
            .await?;
        self.check_status(resp).await?;
        Ok(())
    }

    /// Report a completed QSO to cqdx.io, discarding the success/duplicate
    /// distinction. Retained for the fire-and-forget spot-poller bridge path;
    /// new logbook-upload callers should prefer [`CqdxClient::log_qso`], which
    /// surfaces the duplicate-vs-inserted outcome.
    pub async fn report_qso(&self, qso: QsoRecord) -> Result<()> {
        self.log_qso(qso).await.map(|_| ())
    }

    /// Upload a completed QSO to the operator's cqdx.io logbook
    /// (`POST /api/v1/qsos`, the clublog-style logging endpoint documented in
    /// `docs/cqdx-api-requirements.md`).
    ///
    /// Response handling is defensive so the caller can treat a duplicate as a
    /// benign no-op rather than a failure:
    ///   - `201 Created` / any other `2xx` → [`QsoUploadOutcome::Logged`]
    ///   - `200 OK` whose JSON body marks the record as a duplicate, **or**
    ///     `409 Conflict` → [`QsoUploadOutcome::Duplicate`]
    ///   - `401` → [`CqdxError::Unauthorized`]
    ///   - any other non-2xx → [`CqdxError::Server`]
    ///
    /// The body is parsed leniently: the endpoint contract only promises a
    /// status code, so a missing / non-JSON body on a 2xx is still treated as a
    /// successful log. The PAT token is sent via `Authorization: Bearer` and is
    /// never logged.
    pub async fn log_qso(&self, qso: QsoRecord) -> Result<QsoUploadOutcome> {
        let url = format!("{}/api/v1/qsos", self.base_url);
        debug!("Logging QSO with {} to {}", qso.callsign, url);
        let req = QsoReportRequest { version: 1, qso };
        let resp = self
            .http
            .post(&url)
            .bearer_auth(self.token.expose_secret())
            .json(&req)
            .send()
            .await?;

        let status = resp.status();

        // 401 is always an auth failure regardless of body.
        if status.as_u16() == 401 {
            return Err(CqdxError::Unauthorized);
        }
        // 409 Conflict is the conventional "already logged" signal — treat as
        // a non-fatal duplicate.
        if status.as_u16() == 409 {
            return Ok(QsoUploadOutcome::Duplicate);
        }
        if !status.is_success() {
            let message = resp.text().await.unwrap_or_else(|_| "unknown".to_string());
            return Err(CqdxError::Server {
                status: status.as_u16(),
                message,
            });
        }

        // 2xx: a duplicate may still be reported in-band with a 200 body
        // (`{"status":"duplicate"}` / `{"duplicate":true}` — the exact shape is
        // not pinned by the API doc, so we sniff leniently). Read the body
        // best-effort; a missing / non-JSON body just means "logged".
        let body = resp.text().await.unwrap_or_default();
        if !body.is_empty() {
            if let Ok(parsed) = serde_json::from_str::<QsoUploadBody>(&body) {
                if parsed.is_duplicate() {
                    return Ok(QsoUploadOutcome::Duplicate);
                }
            }
        }
        Ok(QsoUploadOutcome::Logged)
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

    /// Deserialize a JSON response body with a size limit to prevent OOM.
    async fn checked_json<T: serde::de::DeserializeOwned>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T> {
        // Check Content-Length header first (fast reject)
        if let Some(len) = resp.content_length() {
            if len > MAX_RESPONSE_BYTES as u64 {
                return Err(CqdxError::ResponseTooLarge(len));
            }
        }
        // Read bytes with actual size check
        let bytes = resp.bytes().await?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(CqdxError::ResponseTooLarge(bytes.len() as u64));
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_client(base_url: &str) -> CqdxClient {
        CqdxClient::new(base_url.to_string(), "pat_test_token".to_string()).unwrap()
    }

    #[tokio::test]
    async fn test_fetch_entities() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/entities"))
            .and(header("Authorization", "Bearer pat_test_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entities": [{
                    "adifNumber": 291,
                    "entityName": "United States",
                    "prefix": "K",
                    "continent": "NA",
                    "cqZone": 5,
                    "ituZone": 8,
                    "rarityRank": 340,
                    "rarityTier": "common",
                    "isDeleted": false
                }]
            })))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let entities = client.fetch_entities().await.unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].prefix, "K");
        assert_eq!(entities[0].adif_number, 291);
        assert_eq!(entities[0].entity_name, "United States");
        assert_eq!(entities[0].rarity_rank, Some(340));
        assert!(!entities[0].is_deleted);
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
    async fn test_fetch_live_spots() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/spots"))
            .and(query_param("live", "true"))
            .and(query_param("band", "20m"))
            .and(header("Authorization", "Bearer pat_test_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "groups": [{
                    "dxCall": "3Y0J",
                    "band": "20m",
                    "mode": "FT8",
                    "dxDxcc": 327,
                    "dxEntityName": "Bouvet Island",
                    "dxContinent": "AF",
                    "dxCqZone": 38,
                    "dxGrid": "JD15",
                    "rarityRank": 1,
                    "rarityTier": "legendary",
                    "frequency": 14074000_u64,
                    "bestSnr": -12,
                    "reporterCount": 5,
                    "sources": ["pskreporter"],
                    "firstSeen": 1743688920_i64,
                    "lastSeen": 1743689040_i64,
                    "confidence": 4.2
                }]
            })))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let groups = client.fetch_live_spots(Some("20m"), None).await.unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].dx_call, "3Y0J");
        assert_eq!(groups[0].rarity_rank, Some(1));
        assert_eq!(groups[0].reporter_count, 5);
        assert_eq!(groups[0].best_snr, Some(-12));
    }

    #[tokio::test]
    async fn test_report_spots() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/spots/report"))
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

    /// Build a representative completed-QSO record for the logbook tests.
    fn sample_qso() -> QsoRecord {
        QsoRecord {
            callsign: "JA1ABC".to_string(),
            remote_grid: Some("PM95".to_string()),
            local_grid: Some("FN31".to_string()),
            frequency: 14074000,
            mode: "FT8".to_string(),
            rst_sent: Some("-10".to_string()),
            rst_received: Some("-14".to_string()),
            start_time: chrono::Utc::now(),
            end_time: chrono::Utc::now(),
        }
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
        client.report_qso(sample_qso()).await.unwrap();
    }

    /// `log_qso` posts the documented `{version, qso}` envelope and reports a
    /// bare 201 as `Logged`.
    #[tokio::test]
    async fn test_log_qso_logged_201() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/qsos"))
            .and(header("Authorization", "Bearer pat_test_token"))
            .and(body_partial_json(serde_json::json!({
                "version": 1,
                "qso": { "callsign": "JA1ABC", "mode": "FT8", "frequency": 14074000_u64 }
            })))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let outcome = client.log_qso(sample_qso()).await.unwrap();
        assert_eq!(outcome, QsoUploadOutcome::Logged);
    }

    /// A 409 Conflict is a non-fatal duplicate, not an error.
    #[tokio::test]
    async fn test_log_qso_duplicate_409() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/qsos"))
            .respond_with(ResponseTemplate::new(409))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let outcome = client.log_qso(sample_qso()).await.unwrap();
        assert_eq!(outcome, QsoUploadOutcome::Duplicate);
    }

    /// A 200 whose body marks the record as a duplicate is also non-fatal.
    #[tokio::test]
    async fn test_log_qso_duplicate_in_band_200() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/qsos"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "status": "duplicate" })),
            )
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let outcome = client.log_qso(sample_qso()).await.unwrap();
        assert_eq!(outcome, QsoUploadOutcome::Duplicate);
    }

    /// A 401 is surfaced as `Unauthorized` so the caller can stop retrying.
    #[tokio::test]
    async fn test_log_qso_unauthorized_401() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/qsos"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": { "code": "UNAUTHORIZED", "message": "Invalid token" }
            })))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let result = client.log_qso(sample_qso()).await;
        assert!(matches!(result, Err(CqdxError::Unauthorized)));
    }

    /// A 500 is a hard server error.
    #[tokio::test]
    async fn test_log_qso_server_error_500() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/qsos"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let result = client.log_qso(sample_qso()).await;
        assert!(matches!(result, Err(CqdxError::Server { status: 500, .. })));
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

    #[tokio::test]
    #[ignore] // Run manually: cargo test -p pancetta-cqdx test_live_spots_envelope -- --ignored
    async fn test_live_spots_envelope() {
        // Requires CQDX_TOKEN env var
        let token = std::env::var("CQDX_TOKEN").expect("Set CQDX_TOKEN to run this test");
        let client = CqdxClient::new("https://cqdx.io".to_string(), token).unwrap();

        // Try fetching live spots — this validates the real envelope
        match client.fetch_live_spots(Some("20m"), Some("FT8")).await {
            Ok(groups) => {
                println!("SUCCESS: Got {} spot groups", groups.len());
                for g in groups.iter().take(3) {
                    println!(
                        "  {} on {} @ {} Hz (rarity: {:?})",
                        g.dx_call, g.band, g.frequency, g.rarity_rank
                    );
                }
            }
            Err(e) => {
                panic!("FAILED: Spots endpoint returned error: {}", e);
            }
        }
    }
}
