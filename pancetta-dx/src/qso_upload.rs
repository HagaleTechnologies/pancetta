//! Per-QSO log upload clients for ClubLog and QRZ Logbook.
//!
//! Each completed QSO can be uploaded as a single ADIF record to the operator's
//! online logbooks. Both integrations are opt-in (default disabled) and keep all
//! credentials local on the pancetta host; **no credential value is ever placed
//! in an error message or log line**.
//!
//! - [`ClubLogClient`] posts to ClubLog's real-time endpoint
//!   (`https://clublog.org/realtime.php`). Form fields: `email`, `password`,
//!   `callsign`, `api`, `adif`. HTTP 200 = accepted (including the
//!   "QSO Duplicate" case, which ClubLog still returns as 200); any non-2xx
//!   status is mapped to a descriptive error (status + body, no secrets).
//!   See <https://clublog.freshdesk.com/support/solutions/articles/54906-how-to-upload-qsos-in-real-time>.
//!
//! - [`QrzLogbookClient`] posts to the QRZ Logbook API
//!   (`https://logbook.qrz.com/api`). Form fields: `KEY`, `ACTION=INSERT`,
//!   `ADIF`, and optional `OPTION`. The response is an
//!   `application/x-www-form-urlencoded`-style `k=v&k=v` string with a `RESULT`
//!   field: `OK` (inserted), `REPLACE` (duplicate overwritten via
//!   `OPTION=REPLACE`), `FAIL` (not inserted; `REASON` describes why —
//!   a plain duplicate without `OPTION=REPLACE` surfaces here with a REASON
//!   mentioning "duplicate"), or `AUTH` (bad/insufficient key). Parsed
//!   defensively (case-insensitive keys). A duplicate is a **non-fatal**
//!   outcome, not an error. See
//!   <https://www.qrz.com/docs/logbook/QRZLogbookAPI.html>.

// rationale: the crate-wide `DxError` is intentionally a flat (non-boxed) enum for
// ergonomic `?`; boxing it crate-wide to satisfy this lint is out of scope here.
#![allow(clippy::result_large_err)]

use crate::{DxError, Result};
use reqwest::Client;
use std::time::Duration;
use tracing::debug;

/// Default request timeout for log uploads (seconds).
const UPLOAD_TIMEOUT_SECS: u64 = 30;

fn build_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(UPLOAD_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new())
}

// ---------------------------------------------------------------------------
// ClubLog
// ---------------------------------------------------------------------------

/// Client for ClubLog real-time per-QSO uploads.
///
/// Construct from the operator's local config and call [`upload_adif`] once per
/// completed QSO with exactly one ADIF record (terminated by `<EOR>`).
///
/// [`upload_adif`]: ClubLogClient::upload_adif
pub struct ClubLogClient {
    email: String,
    password: String,
    callsign: String,
    api_key: String,
    endpoint: String,
    client: Client,
}

impl ClubLogClient {
    /// ClubLog real-time upload endpoint.
    const ENDPOINT: &'static str = "https://clublog.org/realtime.php";

    /// Build a new client. `callsign` is the station call the log is uploaded
    /// into (the coordinator passes its `our_callsign` fallback when the
    /// configured value is empty).
    pub fn new(
        email: impl Into<String>,
        password: impl Into<String>,
        callsign: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            email: email.into(),
            password: password.into(),
            callsign: callsign.into(),
            api_key: api_key.into(),
            endpoint: Self::ENDPOINT.to_string(),
            client: build_client(),
        }
    }

    /// Override the endpoint (used only by tests / mock servers).
    #[cfg(test)]
    fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Upload a single ADIF record to ClubLog.
    ///
    /// HTTP 200 is treated as success (ClubLog returns 200 even for the
    /// "QSO Duplicate" case, which is harmless). Any non-success status is
    /// mapped to a [`DxError::ExternalService`] carrying the status code and
    /// response body. **No credential value is included in the error.**
    pub async fn upload_adif(&self, adif_record: &str) -> Result<()> {
        let params = [
            ("email", self.email.as_str()),
            ("password", self.password.as_str()),
            ("callsign", self.callsign.as_str()),
            ("api", self.api_key.as_str()),
            ("adif", adif_record),
        ];

        let response = self
            .client
            .post(&self.endpoint)
            .form(&params)
            .send()
            .await
            .map_err(DxError::Network)?;

        let status = response.status();
        // Body may carry an operator-facing message; safe to surface (it does
        // not echo our POSTed credentials).
        let body = response.text().await.unwrap_or_default();

        if status.is_success() {
            debug!(target: "qso.upload", "ClubLog accepted QSO (HTTP {})", status);
            Ok(())
        } else {
            Err(DxError::ExternalService(format!(
                "ClubLog upload failed: HTTP {} — {}",
                status,
                body.trim()
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// QRZ Logbook
// ---------------------------------------------------------------------------

/// Outcome of a QRZ Logbook INSERT.
///
/// A [`Duplicate`](QrzInsertOutcome::Duplicate) is a normal, non-fatal result:
/// QRZ rejects re-inserts of an existing QSO, which is expected when a record
/// has already been uploaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QrzInsertOutcome {
    /// The QSO was inserted (`RESULT=OK`), optionally with the assigned LOGID.
    Inserted { logid: Option<String> },
    /// The QSO already existed (either `RESULT=REPLACE`, or `RESULT=FAIL` with a
    /// REASON mentioning a duplicate). Non-fatal.
    Duplicate { reason: Option<String> },
}

/// Client for QRZ Logbook per-QSO uploads (`ACTION=INSERT`).
pub struct QrzLogbookClient {
    api_key: String,
    endpoint: String,
    client: Client,
}

impl QrzLogbookClient {
    /// QRZ Logbook API endpoint.
    const ENDPOINT: &'static str = "https://logbook.qrz.com/api";

    /// Build a new client from the per-logbook API access key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            endpoint: Self::ENDPOINT.to_string(),
            client: build_client(),
        }
    }

    /// Override the endpoint (used only by tests / mock servers).
    #[cfg(test)]
    fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Upload a single ADIF record to QRZ Logbook via `ACTION=INSERT`.
    ///
    /// Returns a [`QrzInsertOutcome`] distinguishing inserted vs. duplicate.
    /// `RESULT=FAIL` for a non-duplicate reason, `RESULT=AUTH`, an HTTP error,
    /// or any unrecognised response is mapped to [`DxError`] carrying the raw
    /// response (which does not echo the API key).
    pub async fn upload_adif(&self, adif_record: &str) -> Result<QrzInsertOutcome> {
        let params = [
            ("KEY", self.api_key.as_str()),
            ("ACTION", "INSERT"),
            ("ADIF", adif_record),
        ];

        let response = self
            .client
            .post(&self.endpoint)
            .form(&params)
            .send()
            .await
            .map_err(DxError::Network)?;

        let status = response.status();
        let body = response.text().await.map_err(DxError::Network)?;

        if !status.is_success() {
            return Err(DxError::ExternalService(format!(
                "QRZ Logbook upload failed: HTTP {} — {}",
                status,
                body.trim()
            )));
        }

        parse_qrz_response(&body)
    }
}

/// Parse a QRZ Logbook `k=v&k=v` response into an outcome.
///
/// Defensive: keys are matched case-insensitively and URL-decoded, and the
/// `RESULT`/`STATUS` field is tolerated under either name. A `FAIL` whose
/// `REASON` mentions a duplicate is reclassified as a (non-fatal) duplicate.
/// Anything else is an [`Err`] carrying the raw response.
pub fn parse_qrz_response(body: &str) -> Result<QrzInsertOutcome> {
    let mut result: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut logid: Option<String> = None;

    for pair in body.trim().split('&') {
        let mut it = pair.splitn(2, '=');
        let key = it.next().unwrap_or("").trim();
        let value = it.next().unwrap_or("").trim();
        if key.is_empty() {
            continue;
        }
        let value = url_decode(value);
        match key.to_ascii_uppercase().as_str() {
            // Accept either RESULT (documented) or STATUS (defensive alias).
            "RESULT" | "STATUS" => result = Some(value),
            "REASON" => reason = Some(value),
            // Documented as LOGID; LOGIDS is the multi-record variant.
            "LOGID" | "LOGIDS" => logid = Some(value),
            _ => {}
        }
    }

    let result_upper = result.as_deref().unwrap_or("").to_ascii_uppercase();
    let reason_is_dupe = reason
        .as_deref()
        .map(|r| r.to_ascii_lowercase().contains("dup"))
        .unwrap_or(false);

    match result_upper.as_str() {
        "OK" => Ok(QrzInsertOutcome::Inserted { logid }),
        // REPLACE = duplicate that was overwritten (only when OPTION=REPLACE).
        "REPLACE" => Ok(QrzInsertOutcome::Duplicate { reason }),
        // A plain duplicate without OPTION=REPLACE comes back as FAIL with a
        // REASON mentioning "duplicate"; treat that as non-fatal.
        "FAIL" if reason_is_dupe => Ok(QrzInsertOutcome::Duplicate { reason }),
        "FAIL" => Err(DxError::ExternalService(format!(
            "QRZ Logbook insert failed: {}",
            reason.unwrap_or_else(|| "unknown reason".to_string())
        ))),
        "AUTH" => Err(DxError::ExternalService(
            "QRZ Logbook insert rejected: invalid or insufficient API key (RESULT=AUTH)"
                .to_string(),
        )),
        _ => Err(DxError::Parse(format!(
            "Unrecognised QRZ Logbook response: {}",
            body.trim()
        ))),
    }
}

/// Minimal `application/x-www-form-urlencoded` value decoder (`%XX` + `+`).
///
/// QRZ REASON text can contain spaces/percent-escapes; this keeps the message
/// readable without pulling in a new dependency.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clublog_client_constructs() {
        let c = ClubLogClient::new("op@example.com", "secret", "K5ARH", "appkey")
            .with_endpoint("http://127.0.0.1:0/realtime.php");
        assert_eq!(c.email, "op@example.com");
        assert_eq!(c.callsign, "K5ARH");
        assert!(c.endpoint.ends_with("/realtime.php"));
    }

    #[test]
    fn qrz_client_constructs() {
        let c = QrzLogbookClient::new("qrzkey").with_endpoint("http://127.0.0.1:0/api");
        assert_eq!(c.api_key, "qrzkey");
        assert!(c.endpoint.ends_with("/api"));
    }

    #[test]
    fn parse_qrz_ok_with_logid() {
        let out = parse_qrz_response("RESULT=OK&LOGID=130877825&COUNT=1").unwrap();
        assert_eq!(
            out,
            QrzInsertOutcome::Inserted {
                logid: Some("130877825".to_string())
            }
        );
    }

    #[test]
    fn parse_qrz_ok_without_logid() {
        let out = parse_qrz_response("RESULT=OK").unwrap();
        assert_eq!(out, QrzInsertOutcome::Inserted { logid: None });
    }

    #[test]
    fn parse_qrz_replace_is_duplicate() {
        let out = parse_qrz_response("RESULT=REPLACE&COUNT=1&LOGID=42").unwrap();
        assert_eq!(out, QrzInsertOutcome::Duplicate { reason: None });
    }

    #[test]
    fn parse_qrz_fail_duplicate_is_nonfatal() {
        // Plain duplicate without OPTION=REPLACE: FAIL + REASON mentioning dup.
        let out = parse_qrz_response(
            "RESULT=FAIL&REASON=Unable+to+add+QSO+to+database%3A+duplicate&COUNT=0",
        )
        .unwrap();
        match out {
            QrzInsertOutcome::Duplicate { reason } => {
                assert!(reason.unwrap().to_lowercase().contains("duplicate"));
            }
            other => panic!("expected Duplicate, got {other:?}"),
        }
    }

    #[test]
    fn parse_qrz_fail_other_is_error() {
        let err = parse_qrz_response("RESULT=FAIL&REASON=Missing+band").unwrap_err();
        let msg = err.to_string();
        assert!(msg.to_lowercase().contains("missing band"), "msg: {msg}");
    }

    #[test]
    fn parse_qrz_auth_is_error() {
        let err = parse_qrz_response("RESULT=AUTH").unwrap_err();
        assert!(err.to_string().to_uppercase().contains("AUTH"));
    }

    #[test]
    fn parse_qrz_unknown_is_error() {
        let err = parse_qrz_response("FOO=BAR").unwrap_err();
        assert!(matches!(err, DxError::Parse(_)));
    }

    #[test]
    fn parse_qrz_status_alias_ok() {
        // Defensive: tolerate STATUS as an alias for RESULT.
        let out = parse_qrz_response("STATUS=OK&LOGID=7").unwrap();
        assert_eq!(
            out,
            QrzInsertOutcome::Inserted {
                logid: Some("7".to_string())
            }
        );
    }

    #[test]
    fn parse_qrz_case_insensitive_keys() {
        let out = parse_qrz_response("result=ok&logid=99").unwrap();
        assert_eq!(
            out,
            QrzInsertOutcome::Inserted {
                logid: Some("99".to_string())
            }
        );
    }

    #[test]
    fn url_decode_handles_percent_and_plus() {
        assert_eq!(url_decode("a+b%3Ac"), "a b:c");
        assert_eq!(url_decode("plain"), "plain");
        // Malformed escape is passed through, not panicked on.
        assert_eq!(url_decode("100%"), "100%");
    }
}
