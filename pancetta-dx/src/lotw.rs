//! LoTW (Logbook of the World) Integration
//!
//! This module provides integration with ARRL's Logbook of the World
//! for QSL confirmation and award tracking.

use crate::{ConfirmationStatus, DxError, DxQso, Result};
use chrono::{NaiveDate, Utc};
use reqwest::{multipart::Form, Client};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::fs;
use tracing::info;

/// LoTW configuration
#[derive(Debug, Clone)]
pub struct LotwConfig {
    /// LoTW username
    pub username: String,
    /// LoTW password
    pub password: String,
    /// Base URL for LoTW API
    pub base_url: String,
    /// Request timeout in seconds
    pub timeout_seconds: u64,
    /// Station callsign
    pub station_callsign: String,
    /// Certificate file path for signing
    pub certificate_path: Option<String>,
    /// Private key file path for signing
    pub private_key_path: Option<String>,
}

impl Default for LotwConfig {
    fn default() -> Self {
        Self {
            username: String::new(),
            password: String::new(),
            base_url: "https://lotw.arrl.org".to_string(),
            timeout_seconds: 30,
            station_callsign: String::new(),
            certificate_path: None,
            private_key_path: None,
        }
    }
}

/// LoTW QSL record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LotwQsl {
    /// Own callsign
    pub own_call: String,
    /// Contacted callsign
    pub call: String,
    /// QSO date
    pub qso_date: NaiveDate,
    /// QSO time (HHMM UTC)
    pub time_on: String,
    /// QSO band
    pub band: String,
    /// QSO mode
    pub mode: String,
    /// Frequency in MHz
    pub freq: Option<f64>,
    /// Station location (state/province/country)
    pub my_state: Option<String>,
    /// Contacted station location
    pub state: Option<String>,
    /// Grid square
    pub gridsquare: Option<String>,
    /// Contest ID
    pub contest_id: Option<String>,
    /// Confirmation date
    pub qsl_rcvd_date: Option<NaiveDate>,
    /// Confirmation status
    pub credit_granted: Option<String>,
}

/// LoTW ADIF upload response
#[derive(Debug, Deserialize)]
pub struct LotwUploadResponse {
    /// Response status
    pub status: String,
    /// Number of records processed
    pub processed: Option<u32>,
    /// Number of records accepted
    pub accepted: Option<u32>,
    /// Number of records rejected
    pub rejected: Option<u32>,
    /// Error messages
    pub errors: Option<Vec<String>>,
}

/// LoTW QSL download parameters
#[derive(Debug, Clone)]
pub struct LotwDownloadParams {
    /// Start date for QSL query
    pub start_date: NaiveDate,
    /// End date for QSL query
    pub end_date: NaiveDate,
    /// Own callsign
    pub own_callsign: String,
    /// QSL query mode (all, confirmed, etc.)
    pub qsl_query: String,
    /// Specific callsign to query
    pub call: Option<String>,
    /// Specific band
    pub band: Option<String>,
    /// Specific mode
    pub mode: Option<String>,
    /// DXCC entity
    pub dxcc: Option<u16>,
}

impl Default for LotwDownloadParams {
    fn default() -> Self {
        let end_date = Utc::now().date_naive();
        let start_date = end_date - chrono::Duration::days(365);

        Self {
            start_date,
            end_date,
            own_callsign: String::new(),
            qsl_query: "1".to_string(), // 1 = confirmed QSLs only
            call: None,
            band: None,
            mode: None,
            dxcc: None,
        }
    }
}

// TODO(lotw): per-QSO auto-upload to LoTW is deferred. Unlike ClubLog/QRZ
// (a raw ADIF POST — see `qso_upload.rs`), LoTW requires each record to be
// digitally signed with the operator's TQSL certificate (TQSL produces a
// signed .tq8). Until TQSL signing is wired in, the coordinator does not
// auto-upload completed QSOs to LoTW; ClubLog + QRZ are the supported targets.
/// LoTW client
pub struct LotwClient {
    /// HTTP client
    client: Client,
    /// Configuration
    config: LotwConfig,
    /// Authentication cookie (populated on login, used for subsequent requests)
    _auth_cookie: Option<String>,
}

impl LotwClient {
    /// Create new LoTW client
    pub fn new(username: Option<String>) -> Self {
        let mut config = LotwConfig::default();
        if let Some(username) = username {
            config.username = username;
        }

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_seconds))
            .cookie_store(true)
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            client,
            config,
            _auth_cookie: None,
        }
    }

    /// Create LoTW client with configuration
    pub fn with_config(config: LotwConfig) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_seconds))
            .cookie_store(true)
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            client,
            config,
            _auth_cookie: None,
        }
    }

    /// Update configuration
    pub fn update_config(&mut self, config: LotwConfig) {
        self.config = config;
    }

    /// Get current configuration
    pub fn config(&self) -> &LotwConfig {
        &self.config
    }

    /// Login to LoTW
    pub async fn login(&mut self) -> Result<()> {
        if self.config.username.is_empty() || self.config.password.is_empty() {
            return Err(DxError::Configuration(
                "LoTW username and password required".to_string(),
            ));
        }

        // Refuse to send credentials over plaintext. LoTW's real endpoint is
        // HTTPS; an HTTP base_url is either a misconfiguration or a hostile
        // override, and either way we should fail closed before transmitting
        // username/password in form data.
        let url_lower = self.config.base_url.to_lowercase();
        if !url_lower.starts_with("https://") {
            return Err(DxError::Configuration(format!(
                "LoTW base_url must use https:// (got: {}). Refusing to \
                 send credentials over an unencrypted connection.",
                self.config.base_url
            )));
        }

        info!("Logging into LoTW as {}", self.config.username);

        let login_url = format!("{}/lotw/login", self.config.base_url);

        let params = [
            ("username", self.config.username.as_str()),
            ("password", self.config.password.as_str()),
            ("actn", "login"),
        ];

        let response = self
            .client
            .post(&login_url)
            .form(&params)
            .send()
            .await
            .map_err(|e| DxError::Network(e))?;

        if response.status().is_success() {
            let text = response.text().await.map_err(|e| DxError::Network(e))?;

            if text.contains("Invalid username") || text.contains("Invalid password") {
                return Err(DxError::ExternalService(
                    "Invalid LoTW credentials".to_string(),
                ));
            }

            if text.contains("Welcome") || text.contains("My LoTW") {
                info!("Successfully logged into LoTW");
                return Ok(());
            }
        }

        Err(DxError::ExternalService("LoTW login failed".to_string()))
    }

    /// Upload QSOs to LoTW (ADIF format)
    pub async fn upload_adif(&mut self, adif_data: &str) -> Result<LotwUploadResponse> {
        // Ensure we're logged in
        self.login().await?;

        info!("Uploading ADIF data to LoTW ({} bytes)", adif_data.len());

        let upload_url = format!("{}/lotw/upload", self.config.base_url);

        let form = Form::new()
            .text("uploadfile", adif_data.to_string())
            .text("actn", "upload");

        let response = self
            .client
            .post(&upload_url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| DxError::Network(e))?;

        let text = response.text().await.map_err(|e| DxError::Network(e))?;

        // Parse the response (LoTW returns HTML, so we need to parse it)
        self.parse_upload_response(&text)
    }

    /// Upload QSOs from file
    pub async fn upload_adif_file<P: AsRef<Path>>(
        &mut self,
        file_path: P,
    ) -> Result<LotwUploadResponse> {
        let adif_data = fs::read_to_string(file_path)
            .await
            .map_err(|e| DxError::ExternalService(format!("Failed to read ADIF file: {}", e)))?;

        self.upload_adif(&adif_data).await
    }

    /// Download QSL confirmations from LoTW
    pub async fn download_qsls(&mut self, params: LotwDownloadParams) -> Result<Vec<LotwQsl>> {
        // Ensure we're logged in
        self.login().await?;

        info!("Downloading QSLs from LoTW for {}", params.own_callsign);

        let download_url = format!("{}/lotw/lotwreport.adi", self.config.base_url);

        let start_date_str = params.start_date.format("%Y-%m-%d").to_string();
        let mut query_params = vec![
            ("login", self.config.username.as_str()),
            ("password", self.config.password.as_str()),
            ("qso_query", "1"),
            ("qso_qsl", &params.qsl_query),
            ("qso_qslsince", &start_date_str),
            ("qso_qsldetail", "yes"),
            ("qso_owncall", &params.own_callsign),
        ];

        if let Some(call) = &params.call {
            query_params.push(("qso_call", call));
        }

        if let Some(band) = &params.band {
            query_params.push(("qso_band", band));
        }

        if let Some(mode) = &params.mode {
            query_params.push(("qso_mode", mode));
        }

        let dxcc_str;
        if let Some(dxcc) = params.dxcc {
            dxcc_str = dxcc.to_string();
            query_params.push(("qso_dxcc", &dxcc_str));
        }

        let response = self
            .client
            .get(&download_url)
            .query(&query_params)
            .send()
            .await
            .map_err(|e| DxError::Network(e))?;

        if !response.status().is_success() {
            return Err(DxError::ExternalService(format!(
                "LoTW download failed: HTTP {}",
                response.status()
            )));
        }

        let adif_data = response.text().await.map_err(|e| DxError::Network(e))?;

        if adif_data.contains("No records found") {
            tracing::warn!("No QSL records found in LoTW");
            return Ok(Vec::new());
        }

        // Parse ADIF data
        self.parse_adif_qsls(&adif_data)
    }

    /// Get QSL confirmations for specific callsign
    pub async fn get_confirmations_for_callsign(
        &mut self,
        callsign: &str,
        own_callsign: &str,
    ) -> Result<Vec<LotwQsl>> {
        let params = LotwDownloadParams {
            call: Some(callsign.to_uppercase()),
            own_callsign: own_callsign.to_uppercase(),
            ..Default::default()
        };

        self.download_qsls(params).await
    }

    /// Get recent confirmations
    pub async fn get_recent_confirmations(
        &mut self,
        own_callsign: &str,
        days_back: i64,
    ) -> Result<Vec<LotwQsl>> {
        let end_date = Utc::now().date_naive();
        let start_date = end_date - chrono::Duration::days(days_back);

        let params = LotwDownloadParams {
            start_date,
            end_date,
            own_callsign: own_callsign.to_uppercase(),
            ..Default::default()
        };

        self.download_qsls(params).await
    }

    /// Convert QSOs to ADIF format for upload
    pub fn qsos_to_adif(&self, qsos: &[DxQso]) -> String {
        let mut adif = String::new();

        // ADIF header
        adif.push_str("ADIF Export from Pancetta DX\n");
        adif.push_str(&format!(
            "Created: {}\n",
            Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
        ));
        adif.push_str("<ADIF_VER:5>3.1.4\n");
        adif.push_str("<PROGRAMID:11>Pancetta-DX\n");
        adif.push_str("<EOH>\n\n");

        // QSO records
        for qso in qsos {
            adif.push_str(&self.qso_to_adif_record(qso));
            adif.push('\n');
        }

        adif
    }

    /// Convert single QSO to ADIF record
    fn qso_to_adif_record(&self, qso: &DxQso) -> String {
        let mut record = String::new();

        // Callsign
        record.push_str(&format!("<CALL:{}>{}", qso.callsign.len(), qso.callsign));

        // Date and time
        let date_str = qso.datetime.format("%Y%m%d").to_string();
        let time_str = qso.datetime.format("%H%M").to_string();
        record.push_str(&format!("<QSO_DATE:{}>{}", date_str.len(), date_str));
        record.push_str(&format!("<TIME_ON:{}>{}", time_str.len(), time_str));

        // Frequency and band
        let freq_mhz = qso.frequency as f64 / 1_000_000.0;
        let freq_str = format!("{:.6}", freq_mhz);
        record.push_str(&format!("<FREQ:{}>{}", freq_str.len(), freq_str));

        let band_str = qso.band.to_string();
        record.push_str(&format!("<BAND:{}>{}", band_str.len(), band_str));

        // Mode
        let mode_str = qso.mode.to_string();
        record.push_str(&format!("<MODE:{}>{}", mode_str.len(), mode_str));

        // Signal reports
        record.push_str(&format!(
            "<RST_SENT:{}>{}",
            qso.rst_sent.len(),
            qso.rst_sent
        ));
        record.push_str(&format!(
            "<RST_RCVD:{}>{}",
            qso.rst_received.len(),
            qso.rst_received
        ));

        // Optional fields
        if let Some(grid) = &qso.grid_square {
            record.push_str(&format!("<GRIDSQUARE:{}>{}", grid.len(), grid));
        }

        if let Some(qth) = &qso.qth {
            record.push_str(&format!("<QTH:{}>{}", qth.len(), qth));
        }

        if let Some(name) = &qso.name {
            record.push_str(&format!("<NAME:{}>{}", name.len(), name));
        }

        if let Some(notes) = &qso.notes {
            record.push_str(&format!("<NOTES:{}>{}", notes.len(), notes));
        }

        // Confirmation status
        if !matches!(qso.confirmation_status, ConfirmationStatus::None) {
            record.push_str("<QSL_RCVD:1>Y");
            if let Some(conf_date) = qso.confirmation_date {
                let conf_date_str = conf_date.format("%Y%m%d").to_string();
                record.push_str(&format!(
                    "<QSLRDATE:{}>{}",
                    conf_date_str.len(),
                    conf_date_str
                ));
            }
        }

        record.push_str("<EOR>\n");
        record
    }

    /// Parse ADIF QSL data
    fn parse_adif_qsls(&self, adif_data: &str) -> Result<Vec<LotwQsl>> {
        let mut qsls = Vec::new();

        // Simple ADIF parser - split by <EOR>
        let records: Vec<&str> = adif_data.split("<EOR>").collect();

        for record in records {
            if record.trim().is_empty() || record.contains("<EOH>") {
                continue;
            }

            if let Ok(qsl) = self.parse_adif_record(record) {
                qsls.push(qsl);
            }
        }

        info!("Parsed {} QSL records from LoTW", qsls.len());
        Ok(qsls)
    }

    /// Parse single ADIF record
    fn parse_adif_record(&self, record: &str) -> Result<LotwQsl> {
        let mut qsl = LotwQsl {
            own_call: self.config.station_callsign.clone(),
            call: String::new(),
            qso_date: Utc::now().date_naive(),
            time_on: "0000".to_string(),
            band: String::new(),
            mode: String::new(),
            freq: None,
            my_state: None,
            state: None,
            gridsquare: None,
            contest_id: None,
            qsl_rcvd_date: None,
            credit_granted: None,
        };

        // Extract ADIF fields using regex or simple parsing
        for field in self.extract_adif_fields(record) {
            match field.tag.to_uppercase().as_str() {
                "CALL" => qsl.call = field.value,
                "QSO_DATE" => {
                    if let Ok(date) = NaiveDate::parse_from_str(&field.value, "%Y%m%d") {
                        qsl.qso_date = date;
                    }
                }
                "TIME_ON" => qsl.time_on = field.value,
                "BAND" => qsl.band = field.value,
                "MODE" => qsl.mode = field.value,
                "FREQ" => qsl.freq = field.value.parse().ok(),
                "MY_STATE" => qsl.my_state = Some(field.value),
                "STATE" => qsl.state = Some(field.value),
                "GRIDSQUARE" => qsl.gridsquare = Some(field.value),
                "CONTEST_ID" => qsl.contest_id = Some(field.value),
                "QSLRDATE" => {
                    if let Ok(date) = NaiveDate::parse_from_str(&field.value, "%Y%m%d") {
                        qsl.qsl_rcvd_date = Some(date);
                    }
                }
                "CREDIT_GRANTED" => qsl.credit_granted = Some(field.value),
                _ => {} // Ignore unknown fields
            }
        }

        if qsl.call.is_empty() {
            return Err(DxError::Parse("Missing required CALL field".to_string()));
        }

        Ok(qsl)
    }

    /// Extract ADIF fields from record
    fn extract_adif_fields(&self, record: &str) -> Vec<AdifField> {
        let mut fields = Vec::new();
        let mut chars = record.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '<' {
                // Parse field
                let mut field_def = String::new();

                // Read until '>'
                while let Some(ch) = chars.next() {
                    if ch == '>' {
                        break;
                    }
                    field_def.push(ch);
                }

                // Parse field definition (TAG:LENGTH)
                let parts: Vec<&str> = field_def.split(':').collect();
                if parts.len() >= 2 {
                    let tag = parts[0].to_string();
                    if let Ok(length) = parts[1].parse::<usize>() {
                        // Read field value
                        let mut value = String::new();
                        for _ in 0..length {
                            if let Some(ch) = chars.next() {
                                value.push(ch);
                            }
                        }

                        fields.push(AdifField { tag, value });
                    }
                }
            }
        }

        fields
    }

    /// Parse upload response HTML
    fn parse_upload_response(&self, html: &str) -> Result<LotwUploadResponse> {
        let status = if html.contains("Upload successful") || html.contains("File uploaded") {
            "success".to_string()
        } else if html.contains("Error") || html.contains("Failed") {
            "error".to_string()
        } else {
            "unknown".to_string()
        };

        let mut processed = None;
        let mut accepted = None;
        let mut rejected = None;
        let mut errors: Vec<String> = Vec::new();

        // Strip HTML tags so we work on plain text, then scan line by line.
        let plain = strip_html_tags(html);

        for line in plain.lines() {
            let lower = line.to_lowercase();

            // "N records were processed" | "processed N records" | "processed N"
            if lower.contains("processed") && processed.is_none() {
                processed = extract_count(line, "processed");
            }

            // "N QSOs were accepted" | "accepted N"
            if lower.contains("accepted") && accepted.is_none() {
                accepted = extract_count(line, "accepted");
            }

            // "N QSOs were rejected" | "rejected N"
            if lower.contains("rejected") && rejected.is_none() {
                rejected = extract_count(line, "rejected");
            }

            // Collect error lines (but not lines that are just counts)
            if lower.contains("error") || lower.contains("invalid") || lower.contains("failed") {
                let trimmed = line.trim().to_string();
                if !trimmed.is_empty() {
                    errors.push(trimmed);
                }
            }
        }

        Ok(LotwUploadResponse {
            status,
            processed,
            accepted,
            rejected,
            errors: if errors.is_empty() {
                None
            } else {
                Some(errors)
            },
        })
    }

    /// Check LoTW service status
    pub async fn check_service_status(&self) -> Result<bool> {
        let status_url = format!("{}/lotw/", self.config.base_url);

        match self.client.get(&status_url).send().await {
            Ok(response) => Ok(response.status().is_success()),
            Err(_) => Ok(false),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers for parse_upload_response
// ---------------------------------------------------------------------------

/// Remove HTML tags from `s`, returning plain text.
///
/// Each closing `>` is turned into a newline so that content from different
/// HTML block elements ends up on separate lines, which makes per-line keyword
/// scanning reliable even when the HTML has no whitespace between tags.
fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                // newline keeps block elements on separate lines
                out.push('\n');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Search `line` for a decimal number that appears adjacent to `keyword` (case-
/// insensitive).  Handles both orderings:
///   "42 QSOs were accepted"  →  number before keyword
///   "accepted 42"            →  number after keyword
/// Returns the first number found on the line when the keyword is present.
pub fn extract_count(line: &str, keyword: &str) -> Option<u32> {
    let lower = line.to_lowercase();
    if !lower.contains(&keyword.to_lowercase()) {
        return None;
    }

    // Collect all whitespace-delimited tokens; try to parse each as u32.
    // We return the first numeric token found on the line, which works for
    // both "N ... keyword" and "keyword N ..." layouts.
    for token in line.split_whitespace() {
        // Strip any trailing punctuation (comma, period, colon, parentheses)
        let trimmed = token.trim_matches(|c: char| !c.is_ascii_digit());
        if let Ok(n) = trimmed.parse::<u32>() {
            return Some(n);
        }
    }
    None
}

// ---------------------------------------------------------------------------

/// ADIF field structure
#[derive(Debug)]
struct AdifField {
    tag: String,
    value: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Band, Mode};
    use chrono::{DateTime, Utc};

    #[test]
    fn test_client_creation() {
        let client = LotwClient::new(Some("W1ABC".to_string()));
        assert_eq!(client.config.username, "W1ABC");
    }

    #[test]
    fn test_config_default() {
        let config = LotwConfig::default();
        assert!(config.base_url.contains("lotw.arrl.org"));
        assert_eq!(config.timeout_seconds, 30);
    }

    #[test]
    fn test_download_params_default() {
        let params = LotwDownloadParams::default();
        assert!(params.start_date < params.end_date);
        assert_eq!(params.qsl_query, "1");
    }

    #[test]
    fn test_qso_to_adif() {
        let client = LotwClient::new(None);
        let qso = DxQso {
            id: None,
            callsign: "JA1ABC".to_string(),
            datetime: DateTime::parse_from_rfc3339("2023-01-01T12:34:56Z")
                .unwrap()
                .with_timezone(&Utc),
            frequency: 14_074_000,
            band: Band::Band20m,
            mode: Mode::FT8,
            rst_sent: "599".to_string(),
            rst_received: "599".to_string(),
            grid_square: Some("PM95".to_string()),
            qth: Some("Tokyo".to_string()),
            name: Some("Taro".to_string()),
            qsl_route: None,
            confirmation_status: ConfirmationStatus::Lotw,
            confirmation_date: Some(
                DateTime::parse_from_rfc3339("2023-01-02T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            dxcc_entity: 61,
            contest_id: None,
            notes: None,
        };

        let adif_record = client.qso_to_adif_record(&qso);

        assert!(adif_record.contains("JA1ABC"));
        assert!(adif_record.contains("20230101"));
        assert!(adif_record.contains("1234"));
        assert!(adif_record.contains("14.074000"));
        assert!(adif_record.contains("20m"));
        assert!(adif_record.contains("FT8"));
        assert!(adif_record.contains("PM95"));
        assert!(adif_record.contains("Tokyo"));
        assert!(adif_record.contains("Taro"));
        assert!(adif_record.contains("QSL_RCVD:1>Y"));
        assert!(adif_record.contains("<EOR>"));
    }

    #[test]
    fn test_qsos_to_adif() {
        let client = LotwClient::new(None);
        let qsos = vec![DxQso {
            id: None,
            callsign: "JA1ABC".to_string(),
            datetime: Utc::now(),
            frequency: 14_074_000,
            band: Band::Band20m,
            mode: Mode::FT8,
            rst_sent: "599".to_string(),
            rst_received: "599".to_string(),
            grid_square: None,
            qth: None,
            name: None,
            qsl_route: None,
            confirmation_status: ConfirmationStatus::None,
            confirmation_date: None,
            dxcc_entity: 61,
            contest_id: None,
            notes: None,
        }];

        let adif = client.qsos_to_adif(&qsos);

        assert!(adif.contains("ADIF Export"));
        assert!(adif.contains("Pancetta DX"));
        assert!(adif.contains("<ADIF_VER:5>3.1.4"));
        assert!(adif.contains("<PROGRAMID:11>Pancetta-DX"));
        assert!(adif.contains("<EOH>"));
        assert!(adif.contains("JA1ABC"));
    }

    #[test]
    fn test_adif_field_extraction() {
        let client = LotwClient::new(None);
        let record = "<CALL:6>JA1ABC<QSO_DATE:8>20230101<TIME_ON:4>1234<EOR>";

        let fields = client.extract_adif_fields(record);

        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0].tag, "CALL");
        assert_eq!(fields[0].value, "JA1ABC");
        assert_eq!(fields[1].tag, "QSO_DATE");
        assert_eq!(fields[1].value, "20230101");
        assert_eq!(fields[2].tag, "TIME_ON");
        assert_eq!(fields[2].value, "1234");
    }

    // --- extract_count ---

    #[test]
    fn test_extract_count_number_before_keyword() {
        assert_eq!(extract_count("42 QSOs were accepted", "accepted"), Some(42));
    }

    #[test]
    fn test_extract_count_number_after_keyword() {
        assert_eq!(extract_count("accepted 7", "accepted"), Some(7));
    }

    #[test]
    fn test_extract_count_processed_phrase() {
        assert_eq!(
            extract_count("3 records were processed", "processed"),
            Some(3)
        );
    }

    #[test]
    fn test_extract_count_keyword_absent() {
        assert_eq!(extract_count("42 QSOs were accepted", "rejected"), None);
    }

    #[test]
    fn test_extract_count_no_number() {
        assert_eq!(extract_count("no records accepted", "accepted"), None);
    }

    #[test]
    fn test_extract_count_trailing_punctuation() {
        // e.g. "accepted: 5," — comma stripped
        assert_eq!(extract_count("accepted: 5,", "accepted"), Some(5));
    }

    // --- strip_html_tags ---

    #[test]
    fn test_strip_html_tags_basic() {
        let html = "<p>42 QSOs were <b>accepted</b></p>";
        let plain = strip_html_tags(html);
        assert!(!plain.contains('<'));
        assert!(plain.contains("42"));
        assert!(plain.contains("accepted"));
    }

    // --- parse_upload_response ---

    #[test]
    fn test_parse_upload_response_success_counts() {
        let client = LotwClient::new(None);
        let html = "<html><body>\
            <p>Upload successful</p>\
            <p>10 records were processed</p>\
            <p>8 QSOs were accepted</p>\
            <p>2 QSOs were rejected</p>\
            </body></html>";

        let resp = client.parse_upload_response(html).unwrap();
        assert_eq!(resp.status, "success");
        assert_eq!(resp.processed, Some(10));
        assert_eq!(resp.accepted, Some(8));
        assert_eq!(resp.rejected, Some(2));
        assert!(resp.errors.is_none());
    }

    #[test]
    fn test_parse_upload_response_empty() {
        let client = LotwClient::new(None);
        let resp = client.parse_upload_response("").unwrap();
        assert_eq!(resp.status, "unknown");
        assert_eq!(resp.processed, None);
        assert_eq!(resp.accepted, None);
        assert_eq!(resp.rejected, None);
    }

    #[test]
    fn test_parse_upload_response_error_message() {
        let client = LotwClient::new(None);
        let html = "<html><body><p>Error: invalid certificate</p></body></html>";
        let resp = client.parse_upload_response(html).unwrap();
        assert_eq!(resp.status, "error");
        assert!(resp.errors.is_some());
        let errs = resp.errors.unwrap();
        assert!(errs.iter().any(|e| e.contains("invalid certificate")));
    }

    #[test]
    fn test_parse_upload_response_reverse_word_order() {
        // "processed N" layout (keyword before number)
        let client = LotwClient::new(None);
        let html = "processed 5 records\naccepted 3\nrejected 2";
        let resp = client.parse_upload_response(html).unwrap();
        assert_eq!(resp.processed, Some(5));
        assert_eq!(resp.accepted, Some(3));
        assert_eq!(resp.rejected, Some(2));
    }

    #[test]
    fn test_adif_record_parsing() {
        let client = LotwClient::new(None);
        let record = "<CALL:6>JA1ABC<QSO_DATE:8>20230101<TIME_ON:4>1234<BAND:3>20m<MODE:3>FT8<GRIDSQUARE:4>PM95<EOR>";

        let qsl = client.parse_adif_record(record).unwrap();

        assert_eq!(qsl.call, "JA1ABC");
        assert_eq!(qsl.qso_date, NaiveDate::from_ymd_opt(2023, 1, 1).unwrap());
        assert_eq!(qsl.time_on, "1234");
        assert_eq!(qsl.band, "20m");
        assert_eq!(qsl.mode, "FT8");
        assert_eq!(qsl.gridsquare, Some("PM95".to_string()));
    }
}
