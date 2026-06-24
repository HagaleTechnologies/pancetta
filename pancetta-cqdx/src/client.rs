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
        // SSRF / token-exfil mitigation: parse the URL properly (a prefix check
        // like `starts_with("http://localhost")` is bypassable by
        // `http://localhost.attacker.com/`). Require https, OR http only for a
        // genuine loopback host. The bearer token rides every request, so this
        // guards against the operator's base_url being pointed somewhere it
        // would leak in cleartext.
        let parsed = reqwest::Url::parse(&base_url).map_err(|e| {
            CqdxError::InvalidBaseUrl(format!("base_url is not a valid URL: {e} ({base_url})"))
        })?;
        let host = parsed.host_str().unwrap_or("");
        let is_loopback = matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]");
        let scheme_ok = parsed.scheme() == "https" || (parsed.scheme() == "http" && is_loopback);
        if !scheme_ok {
            return Err(CqdxError::InvalidBaseUrl(format!(
                "base_url must use https://, or http:// only for a loopback host \
                 (localhost/127.0.0.1/::1), got: {}",
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
            // Never follow redirects: a 3xx could relocate the bearer token to
            // another host. The cqdx API does not redirect.
            .redirect(reqwest::redirect::Policy::none())
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

    /// Fetch the DXCC entities the user still needs.
    ///
    /// With `band = None`, returns all-time-needed (ATNO) entities — those not
    /// worked on any band. With `band = Some("20m")`, returns per-band fills —
    /// entities not yet confirmed on that band, even if worked or confirmed on
    /// other bands. Each [`NeededEntity`] carries an `atno` flag to distinguish
    /// a true ATNO from a band-fill.
    pub async fn fetch_needed(&self, band: Option<&str>) -> Result<Vec<NeededEntity>> {
        let url = format!("{}/api/v1/entities/needed", self.base_url);
        debug!("Fetching needed entities from {} (band={:?})", url, band);
        // Keep main's redacted-token handling (PatToken::expose_secret) and add
        // the per-band query param from the cqdx branch.
        let mut req = self.http.get(&url).bearer_auth(self.token.expose_secret());
        if let Some(b) = band {
            req = req.query(&[("band", b)]);
        }
        let resp = req.send().await?;
        let resp = self.check_status(resp).await?;
        let body: NeededResponse = self.checked_json(resp).await?;
        Ok(body.needed)
    }

    /// Fetch the operator's needed Maidenhead grid squares from cqdx.io.
    ///
    /// GET `/api/v1/entities/needed-grids` → `{"grids": ["JD15", "FN42", ...]}`.
    /// Returns the grid fields as-is (caller normalizes to the 4-char field).
    ///
    /// Graceful degradation: this endpoint is a roadmap item and may not be
    /// live on the cqdx.io server yet. A `404 Not Found` (endpoint absent) is
    /// treated as "no needed-grids data" and returns an empty Vec rather than
    /// an error — mirroring the "empty set = inert" contract of the
    /// `needed_grids` priority set, so the rest of startup is unaffected.
    /// Other failures (auth, 5xx, transport) still propagate as errors so the
    /// caller can log them; the caller leaves the set empty on any error.
    pub async fn fetch_needed_grids(&self) -> Result<Vec<String>> {
        let url = format!("{}/api/v1/entities/needed-grids", self.base_url);
        debug!("Fetching needed grids from {}", url);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(self.token.expose_secret())
            .send()
            .await?;
        // Endpoint may not exist yet — treat 404 as "no data" rather than an error.
        if resp.status().as_u16() == 404 {
            debug!("needed-grids endpoint not available (404); leaving grid set empty");
            return Ok(Vec::new());
        }
        let resp = self.check_status(resp).await?;
        let body: NeededGridsResponse = self.checked_json(resp).await?;
        Ok(body.grids)
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
            let body = resp.text().await.unwrap_or_else(|_| "unknown".to_string());
            return Err(CqdxError::Server {
                status: status.as_u16(),
                message: sanitize_error_body(&body),
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
        let body = resp.text().await.unwrap_or_else(|_| "unknown".to_string());
        Err(CqdxError::Server {
            status: status.as_u16(),
            message: sanitize_error_body(&body),
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

/// Maximum length (in chars) of a sanitized error body embedded in
/// [`CqdxError::Server`].
const MAX_ERROR_BODY_CHARS: usize = 200;

/// Sanitize a server error-response body before embedding it in an error that
/// may be logged.
///
/// Belt-and-suspenders defense: if the cqdx server ever (bug) echoed the bearer
/// token back in an error body, embedding the raw body in `CqdxError::Server`
/// would surface it in pancetta logs. This redacts any `pat_<...>` token-shaped
/// substrings to `pat_***REDACTED***` and truncates the result to
/// [`MAX_ERROR_BODY_CHARS`] chars (appending `…` if truncated).
///
/// The `pat_` redaction is done with a manual scan (no regex dependency).
fn sanitize_error_body(body: &str) -> String {
    // Redact `pat_` followed by one-or-more token chars ([A-Za-z0-9_]).
    let mut redacted = String::with_capacity(body.len());
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if body[i..].starts_with("pat_") {
            // Consume the trailing token characters.
            let mut j = i + 4;
            while j < bytes.len() {
                let c = bytes[j];
                if c.is_ascii_alphanumeric() || c == b'_' {
                    j += 1;
                } else {
                    break;
                }
            }
            // Only redact if there was at least one token char after `pat_`.
            if j > i + 4 {
                redacted.push_str("pat_***REDACTED***");
                i = j;
                continue;
            }
        }
        // Push this single character (handle multi-byte UTF-8 correctly).
        let ch_len = match body[i..].chars().next() {
            Some(c) => c.len_utf8(),
            None => 1,
        };
        redacted.push_str(&body[i..i + ch_len]);
        i += ch_len;
    }

    // Truncate to MAX_ERROR_BODY_CHARS chars, appending an ellipsis if cut.
    if redacted.chars().count() > MAX_ERROR_BODY_CHARS {
        let mut out: String = redacted.chars().take(MAX_ERROR_BODY_CHARS).collect();
        out.push('…');
        out
    } else {
        redacted
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

    #[test]
    fn base_url_guard_parses_host_not_prefix() {
        let ok = |u: &str| CqdxClient::new(u.to_string(), "pat_test_token".to_string()).is_ok();
        // Accepted: https anywhere, http only for genuine loopback.
        assert!(ok("https://cqdx.io"));
        assert!(ok("https://api.cqdx.io/v1"));
        assert!(ok("http://localhost:8080"));
        assert!(ok("http://127.0.0.1:3000"));
        // Rejected: the prefix-bypass that `starts_with` would have allowed.
        assert!(!ok("http://localhost.attacker.com/"));
        assert!(!ok("http://127.0.0.1.evil.com/"));
        // Rejected: plain http to a non-loopback host (cleartext token).
        assert!(!ok("http://cqdx.io"));
        // Rejected: not a URL at all.
        assert!(!ok("cqdx.io"));
    }

    #[test]
    fn test_sanitize_error_body_redacts_pat_token() {
        let body = "auth failed for token pat_abc123DEF_456 sorry";
        let out = sanitize_error_body(body);
        assert!(
            !out.contains("pat_abc123DEF_456"),
            "raw token leaked: {out}"
        );
        assert!(
            out.contains("pat_***REDACTED***"),
            "no redaction marker: {out}"
        );
        assert_eq!(out, "auth failed for token pat_***REDACTED*** sorry");
    }

    #[test]
    fn test_sanitize_error_body_redacts_multiple_and_at_edges() {
        let body = "pat_first middle pat_SECOND_99";
        let out = sanitize_error_body(body);
        assert_eq!(out, "pat_***REDACTED*** middle pat_***REDACTED***");
    }

    #[test]
    fn test_sanitize_error_body_truncates_long_body() {
        let body = "x".repeat(500);
        let out = sanitize_error_body(&body);
        assert_eq!(out.chars().count(), MAX_ERROR_BODY_CHARS + 1); // +1 for ellipsis
        assert!(out.ends_with('…'));
        assert_eq!(
            out.chars().take(MAX_ERROR_BODY_CHARS).collect::<String>(),
            "x".repeat(MAX_ERROR_BODY_CHARS)
        );
    }

    #[test]
    fn test_sanitize_error_body_clean_passthrough() {
        let body = "internal server error: database unavailable";
        let out = sanitize_error_body(body);
        assert_eq!(out, body);
    }

    #[test]
    fn test_sanitize_error_body_bare_pat_prefix_untouched() {
        // "pat_" with no following token char is not a token shape; leave as-is.
        let body = "pat_ alone";
        let out = sanitize_error_body(body);
        assert_eq!(out, "pat_ alone");
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
                    "prefix": "3Y/B",
                    "atno": true
                }]
            })))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let needed = client.fetch_needed(None).await.unwrap();
        assert_eq!(needed.len(), 1);
        assert_eq!(needed[0].entity_id, 327);
        assert!(needed[0].atno);
    }

    #[tokio::test]
    async fn test_fetch_needed_by_band() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/entities/needed"))
            .and(query_param("band", "20m"))
            .and(header("Authorization", "Bearer pat_test_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "needed": [
                    { "entityId": 327, "name": "Bouvet Island", "prefix": "3Y/B", "atno": true },
                    // Worked on another band, so a band-fill on 20m, not an ATNO.
                    { "entityId": 339, "name": "Japan", "prefix": "JA", "atno": false }
                ]
            })))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let needed = client.fetch_needed(Some("20m")).await.unwrap();
        assert_eq!(needed.len(), 2);
        assert!(needed[0].atno); // ATNO
        assert!(!needed[1].atno); // band-fill
    }

    #[tokio::test]
    async fn test_fetch_needed_atno_defaults_false_when_absent() {
        // Older server responses without the atno field must still deserialize.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/entities/needed"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "needed": [{ "entityId": 327, "name": "Bouvet Island", "prefix": "3Y/B" }]
            })))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let needed = client.fetch_needed(None).await.unwrap();
        assert!(!needed[0].atno);
    }

    #[tokio::test]
    async fn test_fetch_needed_grids() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/entities/needed-grids"))
            .and(header("Authorization", "Bearer pat_test_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "grids": ["JD15", "FN42", "PM95"]
            })))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let grids = client.fetch_needed_grids().await.unwrap();
        assert_eq!(grids.len(), 3);
        assert_eq!(grids[0], "JD15");
        assert!(grids.contains(&"PM95".to_string()));
    }

    #[tokio::test]
    async fn test_fetch_needed_grids_404_graceful() {
        // The needed-grids endpoint is a roadmap item; a server that hasn't
        // shipped it yet returns 404. We degrade to an empty Vec, not an error.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/entities/needed-grids"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let grids = client.fetch_needed_grids().await.unwrap();
        assert!(grids.is_empty());
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
