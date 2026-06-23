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
//!
//! - [`EqslClient`] posts a rendered ADIF record to eQSL.cc's ADIF import
//!   endpoint (`https://www.eqsl.cc/qslcard/importADIF.cfm`). eQSL takes the
//!   account credentials and (optional) QTH nickname as ADIF *header* fields
//!   prepended to the record (`<EQSL_USER:..>`/`<EQSL_PSWD:..>`/`<EQSL_QTHNICKNAME:..>`),
//!   followed by `<EOH>` and the one `<EOR>`-terminated record. The response is
//!   a short HTML/text page: a success contains "Result: 1 out of 1 ... Added";
//!   an already-uploaded record contains "Duplicate"; a bad login contains
//!   "bad" / "error" referencing the user/password. A duplicate is a
//!   **non-fatal** outcome. See
//!   <https://www.eqsl.cc/qslcard/ImportADIF.txt>.
//!
//! - [`LotwClient`] does NOT raw-POST: LoTW requires every record to be
//!   digitally signed with the operator's TQSL certificate. This client shells
//!   out (`tokio::process::Command`) to the operator's installed `tqsl` CLI,
//!   which signs the temp ADIF and uploads the resulting `.tq8` to LoTW. The
//!   exact invocation is marked `OPERATOR-CONFIRM(lotw)` because it cannot be
//!   exercised without the operator's certificate. A missing/failing `tqsl`
//!   never panics — it maps to a failed [`QsoUploadOutcome`].

// rationale: the crate-wide `DxError` is intentionally a flat (non-boxed) enum for
// ergonomic `?`; boxing it crate-wide to satisfy this lint is out of scope here.
#![allow(clippy::result_large_err)]

use crate::{DxError, Result};
use reqwest::Client;
use std::time::Duration;
use tracing::{debug, warn};

/// Default request timeout for log uploads (seconds).
const UPLOAD_TIMEOUT_SECS: u64 = 30;

fn build_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(UPLOAD_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new())
}

/// Outcome of a single per-QSO upload to a service that distinguishes a fresh
/// insert from a re-upload of an already-logged QSO.
///
/// A [`Duplicate`](QsoUploadOutcome::Duplicate) is a normal, **non-fatal**
/// result — it means the QSO was already in the logbook. Used by the eQSL and
/// LoTW clients (ClubLog/QRZ keep their own service-specific outcome types).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QsoUploadOutcome {
    /// The QSO was accepted/added by the service.
    Logged,
    /// The QSO already existed in the logbook. Non-fatal.
    Duplicate,
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

// ---------------------------------------------------------------------------
// eQSL.cc
// ---------------------------------------------------------------------------

/// Client for eQSL.cc per-QSO uploads via the ADIF import endpoint.
///
/// Construct from the operator's local config and call [`upload_adif`] once per
/// completed QSO with exactly one `<EOR>`-terminated ADIF record. The eQSL
/// account credentials are sent as ADIF header fields prepended to the record;
/// they are **never logged**.
///
/// [`upload_adif`]: EqslClient::upload_adif
pub struct EqslClient {
    username: String,
    password: String,
    /// Optional QTH nickname (eQSL "Profile" name) when the account has more
    /// than one location configured.
    qth_nickname: Option<String>,
    endpoint: String,
    client: Client,
}

impl EqslClient {
    /// eQSL.cc ADIF import endpoint.
    const ENDPOINT: &'static str = "https://www.eqsl.cc/qslcard/importADIF.cfm";

    /// Build a new client from the operator's eQSL username/password and an
    /// optional QTH nickname.
    pub fn new(
        username: impl Into<String>,
        password: impl Into<String>,
        qth_nickname: Option<String>,
    ) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
            qth_nickname: qth_nickname.filter(|s| !s.is_empty()),
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

    /// Render the ADIF payload eQSL expects: an ADIF header carrying the
    /// account credentials (and optional QTH nickname), `<EOH>`, then the one
    /// QSO record.
    ///
    // OPERATOR-CONFIRM: eQSL documents passing EQSL_USER / EQSL_PSWD as ADIF
    // header fields embedded in the uploaded file. This is the standard,
    // documented mechanism (see ImportADIF.txt); some integrations additionally
    // send them as multipart form fields. We use the documented header-field
    // approach. The exact field names/casing should be confirmed against a live
    // eQSL account before relying on it in production.
    fn build_payload(&self, adif_record: &str) -> String {
        let mut header = String::new();
        header.push_str(&adif_field("EQSL_USER", &self.username));
        header.push_str(&adif_field("EQSL_PSWD", &self.password));
        if let Some(nick) = &self.qth_nickname {
            header.push_str(&adif_field("EQSL_QTHNICKNAME", nick));
        }
        header.push_str("<EOH>\n");
        header.push_str(adif_record.trim_start());
        header
    }

    /// Upload a single ADIF record to eQSL.cc.
    ///
    /// Returns [`QsoUploadOutcome::Logged`] for a fresh add, or
    /// [`QsoUploadOutcome::Duplicate`] when eQSL reports the QSO already exists.
    /// A bad login, an HTTP error, or an unrecognised response is mapped to a
    /// [`DxError`]. **No credential value is included in the error.**
    pub async fn upload_adif(&self, adif_record: &str) -> Result<QsoUploadOutcome> {
        let payload = self.build_payload(adif_record);

        // OPERATOR-CONFIRM: eQSL's importADIF.cfm accepts the ADIF file either
        // as a raw POST body or as a multipart "Filename"/"ADIFData" field. We
        // POST the rendered ADIF as the request body; confirm the field
        // plumbing against a live account if eQSL rejects this form.
        let response = self
            .client
            .post(&self.endpoint)
            .header(reqwest::header::CONTENT_TYPE, "text/plain")
            .body(payload)
            .send()
            .await
            .map_err(DxError::Network)?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if !status.is_success() {
            return Err(DxError::ExternalService(format!(
                "eQSL upload failed: HTTP {} — {}",
                status,
                body.trim()
            )));
        }

        parse_eqsl_response(&body)
    }
}

/// Parse an eQSL.cc importADIF.cfm response into an outcome.
///
/// eQSL returns a short HTML/text page. Defensive, case-insensitive matching:
/// "duplicate" → [`QsoUploadOutcome::Duplicate`]; an "added" / "result: N out
/// of N" success → [`QsoUploadOutcome::Logged`]; a body mentioning a bad
/// user/password → [`DxError`]; anything else is an error carrying the body.
pub fn parse_eqsl_response(body: &str) -> Result<QsoUploadOutcome> {
    let lower = body.to_lowercase();

    // Order matters: a duplicate page can also say "Result: ...", so check the
    // duplicate marker first.
    if lower.contains("duplicate") {
        return Ok(QsoUploadOutcome::Duplicate);
    }

    // Authentication failure: eQSL surfaces a "bad" user/password message.
    if (lower.contains("bad") || lower.contains("error") || lower.contains("invalid"))
        && (lower.contains("user") || lower.contains("password") || lower.contains("login"))
    {
        return Err(DxError::ExternalService(format!(
            "eQSL upload rejected (authentication): {}",
            body.trim()
        )));
    }

    // Success: "Result: 1 out of 1 ... Added" (or any "added" acknowledgement).
    if lower.contains("added") || (lower.contains("result:") && lower.contains("out of")) {
        return Ok(QsoUploadOutcome::Logged);
    }

    Err(DxError::Parse(format!(
        "Unrecognised eQSL response: {}",
        body.trim()
    )))
}

/// Render one ADIF field (`<TAG:len>value`) — used to build the eQSL header.
fn adif_field(tag: &str, value: &str) -> String {
    format!("<{}:{}>{}", tag, value.len(), value)
}

// ---------------------------------------------------------------------------
// LoTW (TQSL-signed upload)
// ---------------------------------------------------------------------------

/// Client for LoTW per-QSO uploads via the operator's installed TQSL CLI.
///
/// LoTW requires every record to be digitally signed with the operator's
/// certificate, so we cannot raw-POST an ADIF record the way ClubLog/QRZ/eQSL
/// do. Instead, [`upload_adif`] writes the rendered ADIF record to a temp file
/// and shells out to `tqsl` to sign + upload it.
///
/// [`upload_adif`]: LotwClient::upload_adif
pub struct LotwClient {
    /// Path to the operator's `tqsl` binary.
    tqsl_path: String,
    /// The TQSL "Station Location" name the operator configured in TQSL.
    station_location: String,
}

impl LotwClient {
    /// Build a new client from the operator's `tqsl` path and configured TQSL
    /// Station Location name.
    pub fn new(tqsl_path: impl Into<String>, station_location: impl Into<String>) -> Self {
        Self {
            tqsl_path: tqsl_path.into(),
            station_location: station_location.into(),
        }
    }

    /// Build the `tqsl` command-line arguments for signing + uploading
    /// `adif_path`.
    ///
    // OPERATOR-CONFIRM(lotw): the standard batch/non-interactive invocation is
    //   tqsl -d -a all -l "<station_location>" -u <input.adi>
    // where:
    //   -d        suppress all dialogs (batch mode)
    //   -a all    automatically handle duplicate QSOs without prompting
    //   -l NAME   use the named TQSL Station Location
    //   -u FILE   sign AND upload the file to LoTW (vs. -x = sign to file only)
    // This exact invocation can only be verified against a real TQSL install
    // with the operator's certificate; confirm flag spelling/behavior there.
    fn build_args(&self, adif_path: &str) -> Vec<String> {
        vec![
            "-d".to_string(),
            "-a".to_string(),
            "all".to_string(),
            "-l".to_string(),
            self.station_location.clone(),
            "-u".to_string(),
            adif_path.to_string(),
        ]
    }

    /// Write `adif_record` to a temp file, invoke `tqsl` to sign + upload it,
    /// and map the result to a [`QsoUploadOutcome`].
    ///
    /// Best-effort: a missing/erroring `tqsl`, a non-zero exit, or an I/O error
    /// returns a [`DxError`] (logged by the caller) rather than panicking. The
    /// temp file is removed regardless of outcome. Never blocks the pipeline.
    pub async fn upload_adif(&self, adif_record: &str) -> Result<QsoUploadOutcome> {
        use tokio::process::Command;

        // Write the single record to a uniquely-named temp file. We avoid a new
        // crate dependency: a PID + nanosecond-timestamp name in the system temp
        // dir is collision-safe for our once-per-QSO cadence.
        let mut path = std::env::temp_dir();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        path.push(format!(
            "pancetta-lotw-{}-{}.adi",
            std::process::id(),
            stamp
        ));

        // Render a minimal ADIF file (header + the one record) so tqsl has a
        // well-formed input.
        let mut file_body = String::new();
        file_body.push_str("<ADIF_VER:5>3.1.4\n<PROGRAMID:8>Pancetta\n<EOH>\n");
        file_body.push_str(adif_record.trim_start());
        if !file_body.ends_with('\n') {
            file_body.push('\n');
        }

        tokio::fs::write(&path, &file_body)
            .await
            .map_err(DxError::Io)?;

        let path_str = path.to_string_lossy().to_string();
        let args = self.build_args(&path_str);

        let result = Command::new(&self.tqsl_path).args(&args).output().await;

        // Always attempt cleanup, ignoring errors.
        let _ = tokio::fs::remove_file(&path).await;

        let output = match result {
            Ok(o) => o,
            Err(e) => {
                // tqsl missing / not executable — non-fatal, best-effort.
                warn!(
                    target: "qso.upload",
                    "LoTW: failed to invoke tqsl ({}): {}", self.tqsl_path, e
                );
                return Err(DxError::ExternalService(format!(
                    "LoTW: tqsl invocation failed ({}): {}",
                    self.tqsl_path, e
                )));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // tqsl exit code 0 = success. A combined-output mention of "duplicate"
        // is treated as a non-fatal duplicate (we pass -a all, but surface it).
        if output.status.success() {
            let combined = format!("{} {}", stdout, stderr).to_lowercase();
            if combined.contains("duplicate") {
                Ok(QsoUploadOutcome::Duplicate)
            } else {
                Ok(QsoUploadOutcome::Logged)
            }
        } else {
            Err(DxError::ExternalService(format!(
                "LoTW: tqsl exited with {} — {}",
                output.status,
                stderr.trim()
            )))
        }
    }
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

    // --- eQSL ---

    #[test]
    fn eqsl_client_constructs() {
        let c = EqslClient::new("K5ARH", "secret", Some("HOME".to_string()))
            .with_endpoint("http://127.0.0.1:0/importADIF.cfm");
        assert_eq!(c.username, "K5ARH");
        assert_eq!(c.qth_nickname.as_deref(), Some("HOME"));
        assert!(c.endpoint.ends_with("/importADIF.cfm"));
    }

    #[test]
    fn eqsl_empty_qth_nickname_is_none() {
        let c = EqslClient::new("K5ARH", "secret", Some(String::new()));
        assert!(c.qth_nickname.is_none());
    }

    #[test]
    fn eqsl_payload_embeds_credentials_in_header() {
        let c = EqslClient::new("K5ARH", "pw123", Some("HOME".to_string()));
        let payload = c.build_payload("<CALL:5>JA1AB<EOR>\n");
        assert!(payload.contains("<EQSL_USER:5>K5ARH"));
        assert!(payload.contains("<EQSL_PSWD:5>pw123"));
        assert!(payload.contains("<EQSL_QTHNICKNAME:4>HOME"));
        assert!(payload.contains("<EOH>"));
        // The record follows the header.
        assert!(payload.contains("<CALL:5>JA1AB<EOR>"));
        let eoh = payload.find("<EOH>").unwrap();
        let call = payload.find("<CALL").unwrap();
        assert!(eoh < call, "record must come after <EOH>");
    }

    #[test]
    fn eqsl_payload_omits_nickname_when_absent() {
        let c = EqslClient::new("K5ARH", "pw", None);
        let payload = c.build_payload("<CALL:5>JA1AB<EOR>");
        assert!(!payload.contains("EQSL_QTHNICKNAME"));
    }

    #[test]
    fn parse_eqsl_added_is_logged() {
        let out = parse_eqsl_response("Result: 1 out of 1 records added").unwrap();
        assert_eq!(out, QsoUploadOutcome::Logged);
    }

    #[test]
    fn parse_eqsl_duplicate_is_nonfatal() {
        let out =
            parse_eqsl_response("Result: 1 out of 1 records: Duplicate, marked as such").unwrap();
        assert_eq!(out, QsoUploadOutcome::Duplicate);
    }

    #[test]
    fn parse_eqsl_bad_login_is_error() {
        let err = parse_eqsl_response("Error: Bad username/password").unwrap_err();
        assert!(matches!(err, DxError::ExternalService(_)));
    }

    #[test]
    fn parse_eqsl_unknown_is_error() {
        let err = parse_eqsl_response("<html>something unexpected</html>").unwrap_err();
        assert!(matches!(err, DxError::Parse(_)));
    }

    #[test]
    fn adif_field_renders_length() {
        assert_eq!(adif_field("EQSL_USER", "K5ARH"), "<EQSL_USER:5>K5ARH");
        assert_eq!(adif_field("X", ""), "<X:0>");
    }

    // --- LoTW (command-line construction only; tqsl is never executed) ---

    #[test]
    fn lotw_client_constructs() {
        let c = LotwClient::new("/usr/bin/tqsl", "Home Station");
        assert_eq!(c.tqsl_path, "/usr/bin/tqsl");
        assert_eq!(c.station_location, "Home Station");
    }

    #[test]
    fn lotw_build_args_matches_expected_invocation() {
        let c = LotwClient::new("/usr/bin/tqsl", "Home Station");
        let args = c.build_args("/tmp/x.adi");
        // OPERATOR-CONFIRM(lotw): tqsl -d -a all -l "Home Station" -u /tmp/x.adi
        assert_eq!(
            args,
            vec![
                "-d".to_string(),
                "-a".to_string(),
                "all".to_string(),
                "-l".to_string(),
                "Home Station".to_string(),
                "-u".to_string(),
                "/tmp/x.adi".to_string(),
            ]
        );
    }

    #[test]
    fn lotw_build_args_preserves_station_location_spaces() {
        let c = LotwClient::new("tqsl", "My Multi Word Location");
        let args = c.build_args("/tmp/y.adi");
        // The station location is passed as a single argv element (no shell
        // splitting), so embedded spaces are preserved.
        assert!(args.contains(&"My Multi Word Location".to_string()));
    }
}
