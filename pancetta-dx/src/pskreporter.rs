//! PSKReporter Client
//!
//! This module provides integration with PSKReporter for real-time
//! digital mode activity monitoring and spot collection.

use crate::{Band, DxError, DxErrorInner, DxSpot, Mode, Result};
use chrono::{DateTime, Duration, Utc};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use tokio::time::{sleep, Duration as TokioDuration};
use tracing::{debug, error, info, warn};

/// Most bytes read from one `query_spots` JSON response.
const PSKREPORTER_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// PSKReporter API configuration
#[derive(Debug, Clone)]
pub struct PskReporterConfig {
    /// Base URL for PSKReporter API
    pub base_url: String,
    /// Request timeout in seconds
    pub timeout_seconds: u64,
    /// Rate limiting delay between requests (milliseconds)
    pub rate_limit_ms: u64,
    /// Maximum number of spots to request per query
    pub max_spots: u32,
    /// Time window for queries (minutes)
    pub time_window_minutes: i64,
    /// Callsign for identification
    pub callsign: Option<String>,
    /// Software identifier
    pub software: String,
}

impl Default for PskReporterConfig {
    fn default() -> Self {
        Self {
            base_url: "https://retrieve.pskreporter.info/query".to_string(),
            timeout_seconds: 30,
            rate_limit_ms: 2000, // 2 seconds between requests
            max_spots: 1000,
            time_window_minutes: 60,
            callsign: None,
            software: "Pancetta-DX".to_string(),
        }
    }
}

/// PSKReporter spot data from API
#[derive(Debug, Clone, Deserialize)]
struct PskReporterSpot {
    /// Transmitter callsign
    #[serde(rename = "transmitterCall")]
    transmitter_call: String,
    /// Frequency in Hz
    #[serde(rename = "frequency")]
    frequency: u64,
    /// Mode string
    #[serde(rename = "mode")]
    mode: String,
    /// Reporter callsign
    #[serde(rename = "receiverCall")]
    receiver_call: String,
    /// Report time (Unix timestamp)
    #[serde(rename = "flowStartSeconds")]
    flow_start_seconds: i64,
    /// Signal-to-noise ratio
    #[serde(rename = "sNR")]
    snr: Option<i32>,
    /// Transmitter locator (grid square)
    #[serde(rename = "transmitterLocator")]
    transmitter_locator: Option<String>,
    /// Receiver locator (grid square) — captured for completeness; not used in spot conversion
    #[serde(rename = "receiverLocator")]
    #[allow(dead_code)]
    receiver_locator: Option<String>,
}

/// PSKReporter API response
#[derive(Debug, Deserialize)]
struct PskReporterResponse {
    /// Response status
    #[serde(rename = "status")]
    status: String,
    /// Error message if any
    #[serde(rename = "error")]
    error: Option<String>,
    /// Reception reports
    #[serde(rename = "receptionReport")]
    reception_reports: Option<Vec<PskReporterSpot>>,
    /// Statistics — present in API response but not yet consumed
    #[serde(rename = "statistics")]
    #[allow(dead_code)]
    statistics: Option<HashMap<String, serde_json::Value>>,
}

/// Query parameters for PSKReporter API
#[derive(Debug, Clone, Serialize)]
pub struct QueryParams {
    /// Start time for query
    pub start_time: DateTime<Utc>,
    /// End time for query
    pub end_time: DateTime<Utc>,
    /// Specific callsign to search for
    pub callsign: Option<String>,
    /// Minimum frequency in Hz
    pub min_frequency: Option<u64>,
    /// Maximum frequency in Hz
    pub max_frequency: Option<u64>,
    /// Specific band
    pub band: Option<Band>,
    /// Specific mode
    pub mode: Option<Mode>,
    /// Transmitter grid square
    pub transmitter_grid: Option<String>,
    /// Receiver grid square
    pub receiver_grid: Option<String>,
    /// Minimum SNR
    pub min_snr: Option<i32>,
    /// Maximum number of results
    pub limit: Option<u32>,
}

impl Default for QueryParams {
    fn default() -> Self {
        let now = Utc::now();
        Self {
            start_time: now - Duration::hours(1),
            end_time: now,
            callsign: None,
            min_frequency: None,
            max_frequency: None,
            band: None,
            mode: None,
            transmitter_grid: None,
            receiver_grid: None,
            min_snr: None,
            limit: None,
        }
    }
}

/// Statistics from PSKReporter query
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PskReporterStats {
    /// Total number of reports
    pub total_reports: u32,
    /// Number of unique transmitters
    pub unique_transmitters: u32,
    /// Number of unique receivers
    pub unique_receivers: u32,
    /// Mode distribution
    pub mode_distribution: HashMap<String, u32>,
    /// Band distribution
    pub band_distribution: HashMap<String, u32>,
    /// Time range covered
    pub time_range: (DateTime<Utc>, DateTime<Utc>),
}

/// PSKReporter client
pub struct PskReporterClient {
    /// HTTP client
    client: Client,
    /// Configuration
    config: PskReporterConfig,
    /// Last request time for rate limiting
    last_request: Option<DateTime<Utc>>,
}

impl PskReporterClient {
    /// Create new PSKReporter client
    pub fn new() -> Self {
        Self::with_config(PskReporterConfig::default())
    }

    /// Create new PSKReporter client with custom configuration
    pub fn with_config(config: PskReporterConfig) -> Self {
        let client = Client::builder()
            .timeout(TokioDuration::from_secs(config.timeout_seconds))
            .user_agent(format!("{}/1.0", config.software))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            client,
            config,
            last_request: None,
        }
    }

    /// Update configuration
    pub fn update_config(&mut self, config: PskReporterConfig) {
        self.config = config;
    }

    /// Get current configuration
    pub fn config(&self) -> &PskReporterConfig {
        &self.config
    }

    /// Query spots with parameters
    pub async fn query_spots(&mut self, params: QueryParams) -> Result<Vec<DxSpot>> {
        self.rate_limit().await?;

        let url = self.build_query_url(&params)?;
        debug!("Querying PSKReporter: {}", url);

        let response = self.client.get(url).send().await?;

        if !response.status().is_success() {
            return Err(DxError::from(DxErrorInner::ExternalService(format!(
                "PSKReporter API error: HTTP {}",
                response.status()
            ))));
        }

        let bytes = crate::body::bytes_within(response, PSKREPORTER_MAX_BODY_BYTES).await?;
        let psk_response: PskReporterResponse = serde_json::from_slice(&bytes).map_err(|e| {
            DxError::from(DxErrorInner::Parse(format!(
                "Failed to parse PSKReporter response: {}",
                e
            )))
        })?;

        if psk_response.status != "OK" {
            return Err(DxError::from(DxErrorInner::ExternalService(format!(
                "PSKReporter API error: {}",
                psk_response.error.unwrap_or("Unknown error".to_string())
            ))));
        }

        let reports = psk_response.reception_reports.unwrap_or_default();
        info!("Retrieved {} spots from PSKReporter", reports.len());

        // Convert to DxSpot format
        let mut spots = Vec::new();
        for report in reports {
            match self.convert_spot(report) {
                Ok(spot) => spots.push(spot),
                Err(e) => {
                    warn!("Failed to convert PSKReporter spot: {}", e);
                    continue;
                }
            }
        }

        self.last_request = Some(Utc::now());
        Ok(spots)
    }

    /// Query recent activity for a specific callsign
    pub async fn query_callsign_activity(
        &mut self,
        callsign: &str,
        hours_back: i64,
    ) -> Result<Vec<DxSpot>> {
        let now = Utc::now();
        let params = QueryParams {
            start_time: now - Duration::hours(hours_back),
            end_time: now,
            callsign: Some(callsign.to_uppercase()),
            limit: Some(self.config.max_spots),
            ..Default::default()
        };

        self.query_spots(params).await
    }

    /// Query activity on a specific band
    pub async fn query_band_activity(
        &mut self,
        band: Band,
        hours_back: i64,
    ) -> Result<Vec<DxSpot>> {
        let now = Utc::now();
        let (min_freq, max_freq) = band.frequency_range();

        let params = QueryParams {
            start_time: now - Duration::hours(hours_back),
            end_time: now,
            min_frequency: Some(min_freq),
            max_frequency: Some(max_freq),
            band: Some(band),
            limit: Some(self.config.max_spots),
            ..Default::default()
        };

        self.query_spots(params).await
    }

    /// Query activity for a specific mode
    pub async fn query_mode_activity(
        &mut self,
        mode: Mode,
        hours_back: i64,
    ) -> Result<Vec<DxSpot>> {
        let now = Utc::now();
        let params = QueryParams {
            start_time: now - Duration::hours(hours_back),
            end_time: now,
            mode: Some(mode),
            limit: Some(self.config.max_spots),
            ..Default::default()
        };

        self.query_spots(params).await
    }

    /// Query activity in a grid square area
    pub async fn query_grid_activity(
        &mut self,
        grid_square: &str,
        hours_back: i64,
    ) -> Result<Vec<DxSpot>> {
        let now = Utc::now();
        let params = QueryParams {
            start_time: now - Duration::hours(hours_back),
            end_time: now,
            transmitter_grid: Some(grid_square.to_uppercase()),
            limit: Some(self.config.max_spots),
            ..Default::default()
        };

        self.query_spots(params).await
    }

    /// Get statistics for a query
    pub async fn get_statistics(&mut self, params: QueryParams) -> Result<PskReporterStats> {
        // For now, we'll get statistics from a regular query
        // A real implementation might use a dedicated statistics endpoint
        let spots = self.query_spots(params.clone()).await?;

        let mut mode_distribution = HashMap::new();
        let mut band_distribution = HashMap::new();
        let mut unique_transmitters = std::collections::HashSet::new();
        let mut unique_receivers = std::collections::HashSet::new();
        let mut min_time = params.end_time;
        let mut max_time = params.start_time;

        for spot in &spots {
            // Count modes
            if let Some(mode) = &spot.mode {
                *mode_distribution.entry(mode.to_string()).or_insert(0) += 1;
            }

            // Count bands
            if let Some(band) = Band::from_frequency(spot.frequency) {
                *band_distribution.entry(band.to_string()).or_insert(0) += 1;
            }

            // Track unique callsigns
            unique_transmitters.insert(spot.callsign.clone());
            unique_receivers.insert(spot.spotter.clone());

            // Track time range
            if spot.time < min_time {
                min_time = spot.time;
            }
            if spot.time > max_time {
                max_time = spot.time;
            }
        }

        Ok(PskReporterStats {
            total_reports: spots.len() as u32,
            unique_transmitters: unique_transmitters.len() as u32,
            unique_receivers: unique_receivers.len() as u32,
            mode_distribution,
            band_distribution,
            time_range: (min_time, max_time),
        })
    }

    /// Monitor real-time activity with callback
    pub async fn monitor_realtime<F>(
        &mut self,
        mut callback: F,
        poll_interval_seconds: u64,
    ) -> Result<()>
    where
        F: FnMut(Vec<DxSpot>) -> Result<()>,
    {
        let mut last_check = Utc::now() - Duration::minutes(self.config.time_window_minutes);

        info!(
            "Starting PSKReporter real-time monitoring (poll interval: {}s)",
            poll_interval_seconds
        );

        loop {
            let now = Utc::now();
            let params = QueryParams {
                start_time: last_check,
                end_time: now,
                limit: Some(self.config.max_spots),
                ..Default::default()
            };

            match self.query_spots(params).await {
                Ok(spots) => {
                    if !spots.is_empty() {
                        debug!("PSKReporter monitor: {} new spots", spots.len());
                        if let Err(e) = callback(spots) {
                            error!("Error in PSKReporter callback: {}", e);
                        }
                    }
                    last_check = now;
                }
                Err(e) => {
                    error!("Error querying PSKReporter: {}", e);
                    // Continue monitoring despite errors
                }
            }

            sleep(TokioDuration::from_secs(poll_interval_seconds)).await;
        }
    }

    /// Check if PSKReporter service is available
    pub async fn check_service_status(&self) -> Result<bool> {
        let url = format!("{}/status", self.config.base_url);

        match self.client.get(&url).send().await {
            Ok(response) => Ok(response.status().is_success()),
            Err(_) => Ok(false),
        }
    }

    /// Build query URL from parameters
    fn build_query_url(&self, params: &QueryParams) -> Result<Url> {
        let mut url = Url::parse(&self.config.base_url).map_err(|e| {
            DxError::from(DxErrorInner::Configuration(format!(
                "Invalid base URL: {}",
                e
            )))
        })?;

        let start_timestamp = params.start_time.timestamp();
        let end_timestamp = params.end_time.timestamp();

        {
            let mut query_pairs = url.query_pairs_mut();

            // Required parameters
            query_pairs.append_pair("flowStartSeconds", &start_timestamp.to_string());
            query_pairs.append_pair("flowEndSeconds", &end_timestamp.to_string());
            query_pairs.append_pair("format", "json");

            // Optional identification
            if let Some(callsign) = &self.config.callsign {
                query_pairs.append_pair("rrCall", callsign);
            }

            // Optional filters
            if let Some(callsign) = &params.callsign {
                query_pairs.append_pair("txCall", callsign);
            }

            if let Some(min_freq) = params.min_frequency {
                query_pairs.append_pair("fLow", &min_freq.to_string());
            }

            if let Some(max_freq) = params.max_frequency {
                query_pairs.append_pair("fHigh", &max_freq.to_string());
            }

            if let Some(mode) = &params.mode {
                query_pairs.append_pair("mode", &mode.to_string());
            }

            if let Some(grid) = &params.transmitter_grid {
                query_pairs.append_pair("txLoc", grid);
            }

            if let Some(grid) = &params.receiver_grid {
                query_pairs.append_pair("rxLoc", grid);
            }

            if let Some(limit) = params.limit {
                query_pairs.append_pair("rLimit", &limit.to_string());
            }
        }

        Ok(url)
    }

    /// Convert PSKReporter spot to DxSpot
    fn convert_spot(&self, psk_spot: PskReporterSpot) -> Result<DxSpot> {
        let mode = self.parse_mode(&psk_spot.mode);

        let time = DateTime::from_timestamp(psk_spot.flow_start_seconds, 0)
            .ok_or_else(|| DxError::from(DxErrorInner::Parse("Invalid timestamp".to_string())))?;

        Ok(DxSpot {
            callsign: psk_spot.transmitter_call,
            frequency: psk_spot.frequency,
            mode,
            spotter: psk_spot.receiver_call,
            time,
            comment: psk_spot.snr.map(|snr| format!("SNR: {}", snr)),
            dxcc_entity: None, // Will be filled in by DX Hunter
            grid_square: psk_spot.transmitter_locator,
            distance_km: None,     // Will be calculated by DX Hunter
            bearing_degrees: None, // Will be calculated by DX Hunter
            rarity_score: None,    // Will be calculated by DX Hunter
        })
    }

    /// Parse mode string from PSKReporter
    fn parse_mode(&self, mode_str: &str) -> Option<Mode> {
        match mode_str.to_uppercase().as_str() {
            "FT8" => Some(Mode::FT8),
            "FT4" => Some(Mode::FT4),
            "JT65" => None,   // JT65 not in unified Mode
            "JT9" => None,    // JT9 not in unified Mode
            "MSK144" => None, // MSK144 not in unified Mode,
            "JS8" => Some(Mode::JS8),
            "PSK31" => Some(Mode::PSK31),
            "PSK63" => Some(Mode::PSK63),
            "RTTY" => Some(Mode::RTTY),
            "OLIVIA" => None,    // OLIVIA not in unified Mode
            "CONTESTIA" => None, // CONTESTIA not in unified Mode
            "THOR" => None,      // THOR not in unified Mode
            "DOMINO" => None,    // DOMINO not in unified Mode
            "HELL" => None,      // HELL not in unified Mode,
            "MFSK" => None,      // MFSK not in unified Mode,
            "CW" => Some(Mode::CW),
            _ => None, // Unknown modes return None
        }
    }

    /// Rate limiting
    async fn rate_limit(&mut self) -> Result<()> {
        if let Some(last_request) = self.last_request {
            let elapsed = Utc::now().signed_duration_since(last_request);
            let min_interval = Duration::milliseconds(self.config.rate_limit_ms as i64);

            if elapsed < min_interval {
                let sleep_duration = (min_interval - elapsed)
                    .to_std()
                    .unwrap_or(std::time::Duration::from_millis(100));

                debug!("Rate limiting: sleeping for {:?}", sleep_duration);
                sleep(sleep_duration).await;
            }
        }

        Ok(())
    }
}

impl Default for PskReporterClient {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// PSKReporter Upload (UDP binary protocol, IPFIX-based)
// =============================================================================

/// A reception report to upload to PSKReporter
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceptionReport {
    /// Transmitter callsign (the station we heard)
    pub tx_callsign: String,
    /// Frequency in Hz
    pub frequency: u64,
    /// SNR in dB (optional)
    pub snr: Option<i32>,
    /// Mode string (e.g., "FT8")
    pub mode: String,
    /// Transmitter grid square (if known)
    pub tx_grid: Option<String>,
    /// Time of reception (Unix timestamp)
    pub timestamp: i64,
}

/// Default for [`PskReporterUploadConfig::max_pending_reports`] (PAN-285).
pub const DEFAULT_MAX_PENDING_REPORTS: usize = 2000;

/// Largest datagram Pancetta sends to PSKReporter, IPFIX header included
/// (PAN-285).
pub const MAX_DATAGRAM_BYTES: usize = 1400;

/// Configuration for PSKReporter upload
#[derive(Debug, Clone)]
pub struct PskReporterUploadConfig {
    /// Reporter callsign (our station)
    pub reporter_callsign: String,
    /// Reporter grid square
    pub reporter_grid: String,
    /// Antenna description
    pub antenna: String,
    /// Software name and version
    pub software: String,
    /// Upload server hostname
    pub server: String,
    /// Upload server port
    pub port: u16,
    /// Minimum interval between uploads (seconds)
    pub upload_interval_secs: u64,
    /// Most reception reports held while they cannot be sent. When the queue
    /// is full, `add_report` drops the oldest report first. 0 acts as 1
    /// (PAN-285).
    pub max_pending_reports: usize,
}

impl Default for PskReporterUploadConfig {
    fn default() -> Self {
        Self {
            reporter_callsign: String::new(),
            reporter_grid: String::new(),
            antenna: String::new(),
            software: "Pancetta/0.1".to_string(),
            server: "report.pskreporter.info".to_string(),
            port: 4739,
            upload_interval_secs: 300, // 5 minutes per PSKReporter guidelines
            max_pending_reports: DEFAULT_MAX_PENDING_REPORTS,
        }
    }
}

// --- IPFIX wire layout (https://pskreporter.info/pskdev.html) ---

/// IPFIX protocol version in the message header.
const IPFIX_VERSION: u16 = 10;
/// IANA Private Enterprise Number registered to PSKReporter.
const PSKREPORTER_PEN: u32 = 30351;
/// IPFIX field length meaning "variable length".
const VARIABLE_LENGTH: u16 = 0xFFFF;
/// IPFIX Set ID of a Template Set.
const SET_ID_TEMPLATE: u16 = 2;
/// IPFIX Set ID of an Options Template Set.
const SET_ID_OPTIONS_TEMPLATE: u16 = 3;
/// Template (and Data Set) ID of the receiver descriptor (our station).
const RECEIVER_TEMPLATE_ID: u16 = 0x5002;
/// Template (and Data Set) ID of the sender records (the stations we heard).
const SENDER_TEMPLATE_ID: u16 = 0x5003;
/// `informationSource` 1 means "automatically extracted", the only value
/// pskdev.html says the server counts.
const INFORMATION_SOURCE_AUTOMATIC: u8 = 1;

/// One field specifier in a template record.
#[derive(Clone, Copy)]
struct Field {
    element: u16,
    length: u16,
    enterprise: bool,
}

impl Field {
    /// A PSKReporter enterprise field (PEN 30351).
    const fn pen(element: u16, length: u16) -> Self {
        Self {
            element,
            length,
            enterprise: true,
        }
    }

    /// A standard IANA IPFIX field (no enterprise number).
    const fn iana(element: u16, length: u16) -> Self {
        Self {
            element,
            length,
            enterprise: false,
        }
    }
}

/// Receiver descriptor fields: receiverCallsign, receiverLocator,
/// decoderSoftware, antennaInformation.
const RECEIVER_FIELDS: [Field; 4] = [
    Field::pen(2, VARIABLE_LENGTH),
    Field::pen(4, VARIABLE_LENGTH),
    Field::pen(8, VARIABLE_LENGTH),
    Field::pen(9, VARIABLE_LENGTH),
];

/// Sender record fields: senderCallsign, frequency, sNR, mode,
/// informationSource, flowStartSeconds (IANA 150).
const SENDER_FIELDS: [Field; 6] = [
    Field::pen(1, VARIABLE_LENGTH),
    Field::pen(5, 4),
    Field::pen(6, 1),
    Field::pen(10, VARIABLE_LENGTH),
    Field::pen(11, 1),
    Field::iana(150, 4),
];

/// `len` rounded up to a multiple of four (IPFIX set padding).
fn padded(len: usize) -> usize {
    len.div_ceil(4) * 4
}

/// Writes `len` as a big-endian u16 at `buf[at..at + 2]`, or fails instead of
/// wrapping (PAN-285).
fn patch_len(buf: &mut [u8], at: usize, len: usize) -> Result<()> {
    let len = u16::try_from(len).map_err(|_| {
        DxError::from(DxErrorInner::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("PSKReporter length {len} does not fit in 16 bits"),
        )))
    })?;
    buf[at..at + 2].copy_from_slice(&len.to_be_bytes());
    Ok(())
}

/// Writes the 16-byte IPFIX message header. The length is patched by
/// [`finish_datagram`].
fn write_header(buf: &mut Vec<u8>, export_time: u32, sequence: u32, observation_domain: u32) {
    buf.extend_from_slice(&IPFIX_VERSION.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&export_time.to_be_bytes());
    buf.extend_from_slice(&sequence.to_be_bytes());
    buf.extend_from_slice(&observation_domain.to_be_bytes());
}

/// Opens a set: writes its ID and a length placeholder, and returns where the
/// set starts, for [`end_set`].
fn begin_set(buf: &mut Vec<u8>, set_id: u16) -> usize {
    let start = buf.len();
    buf.extend_from_slice(&set_id.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    start
}

/// Pads with zero bytes to a multiple of 4 and patches the set length
/// (padding included).
fn end_set(buf: &mut Vec<u8>, start: usize) -> Result<()> {
    let len = padded(buf.len() - start);
    buf.resize(start + len, 0);
    patch_len(buf, start + 2, len)
}

/// Template Set (ID 2, `scope_fields: None`) or Options Template Set
/// (ID 3, `Some(n)`), padded to 4 with its length patched.
fn write_template_set(
    buf: &mut Vec<u8>,
    set_id: u16,
    template_id: u16,
    scope_fields: Option<u16>,
    fields: &[Field],
) -> Result<()> {
    let start = begin_set(buf, set_id);
    buf.extend_from_slice(&template_id.to_be_bytes());
    let start_of_count = buf.len();
    buf.extend_from_slice(&0u16.to_be_bytes());
    patch_len(buf, start_of_count, fields.len())?;
    if let Some(scope) = scope_fields {
        buf.extend_from_slice(&scope.to_be_bytes());
    }
    for field in fields {
        let specifier = if field.enterprise {
            0x8000 | field.element
        } else {
            field.element
        };
        buf.extend_from_slice(&specifier.to_be_bytes());
        buf.extend_from_slice(&field.length.to_be_bytes());
        if field.enterprise {
            buf.extend_from_slice(&PSKREPORTER_PEN.to_be_bytes());
        }
    }
    end_set(buf, start)
}

/// Patches the IPFIX header's message length to the datagram's length.
fn finish_datagram(buf: &mut [u8]) -> Result<()> {
    let len = buf.len();
    patch_len(buf, 2, len)
}

/// Where `flush_to` writes datagrams: the connected UDP socket in production,
/// an in-memory recorder in tests (PAN-285).
trait DatagramSink {
    async fn send_datagram(&self, datagram: &[u8]) -> std::io::Result<()>;
}

impl DatagramSink for tokio::net::UdpSocket {
    async fn send_datagram(&self, datagram: &[u8]) -> std::io::Result<()> {
        let sent = self.send(datagram).await?;
        if sent != datagram.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                format!("UDP send wrote {sent} of {} bytes", datagram.len()),
            ));
        }
        Ok(())
    }
}

/// PSKReporter uploader that batches and sends reception reports
pub struct PskReporterUploader {
    config: PskReporterUploadConfig,
    /// Pending reports to be uploaded, oldest first; never longer than
    /// [`Self::max_pending_reports`] (PAN-285).
    pending_reports: VecDeque<ReceptionReport>,
    /// Reports the cap evicted since this uploader was created (PAN-285).
    dropped_reports: u64,
    /// Random identifier for this session
    session_id: u32,
    /// Sequence number of the next datagram; advances once per datagram sent
    sequence_number: u32,
}

impl PskReporterUploader {
    /// Create a new uploader with the given configuration
    pub fn new(config: PskReporterUploadConfig) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        std::time::SystemTime::now().hash(&mut hasher);
        let session_id = hasher.finish() as u32;

        Self {
            config,
            pending_reports: VecDeque::new(),
            dropped_reports: 0,
            session_id,
            sequence_number: 0,
        }
    }

    /// Add a reception report to the upload queue. When the queue already
    /// holds [`Self::max_pending_reports`] reports, the oldest are dropped
    /// first and counted in [`Self::dropped_count`] (PAN-285).
    pub fn add_report(&mut self, report: ReceptionReport) {
        let cap = self.max_pending_reports();
        while self.pending_reports.len() >= cap {
            self.pending_reports.pop_front();
            self.dropped_reports += 1;
        }
        self.pending_reports.push_back(report);
    }

    /// Get number of pending reports
    pub fn pending_count(&self) -> usize {
        self.pending_reports.len()
    }

    /// Reports the cap evicted since this uploader was created.
    pub fn dropped_count(&self) -> u64 {
        self.dropped_reports
    }

    /// The effective cap (`max_pending_reports`, at least 1).
    pub fn max_pending_reports(&self) -> usize {
        self.config.max_pending_reports.max(1)
    }

    /// Writes the part every datagram repeats: the IPFIX header (with the
    /// current sequence number), the receiver options template, the sender
    /// template and the receiver record.
    fn write_fixed_part(&self, buf: &mut Vec<u8>) -> Result<()> {
        let export_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;
        write_header(buf, export_time, self.sequence_number, self.session_id);
        write_template_set(
            buf,
            SET_ID_OPTIONS_TEMPLATE,
            RECEIVER_TEMPLATE_ID,
            Some(1),
            &RECEIVER_FIELDS,
        )?;
        write_template_set(
            buf,
            SET_ID_TEMPLATE,
            SENDER_TEMPLATE_ID,
            None,
            &SENDER_FIELDS,
        )?;
        let receiver = begin_set(buf, RECEIVER_TEMPLATE_ID);
        Self::write_variable_field(buf, self.config.reporter_callsign.as_bytes());
        Self::write_variable_field(buf, self.config.reporter_grid.as_bytes());
        Self::write_variable_field(buf, self.config.software.as_bytes());
        Self::write_variable_field(buf, self.config.antenna.as_bytes());
        end_set(buf, receiver)
    }

    /// Encodes one sender record into `out` (cleared first), in
    /// [`SENDER_FIELDS`] order.
    fn encode_record(report: &ReceptionReport, out: &mut Vec<u8>) {
        out.clear();
        Self::write_variable_field(out, report.tx_callsign.as_bytes());
        out.extend_from_slice(&(report.frequency as u32).to_be_bytes());
        out.push(report.snr.unwrap_or(0) as u8);
        Self::write_variable_field(out, report.mode.as_bytes());
        out.push(INFORMATION_SOURCE_AUTOMATIC);
        out.extend_from_slice(&(report.timestamp as u32).to_be_bytes());
    }

    /// Builds the next datagram from the front of the queue without removing
    /// anything, and returns it with the number of reports it carries, or
    /// `None` when the queue is empty. The datagram is at most
    /// [`MAX_DATAGRAM_BYTES`], padding included. A report too large to fit
    /// even alone is removed and logged, since it never will fit (PAN-285).
    /// Call [`Self::mark_sent`] once the datagram has been sent.
    fn next_datagram(&mut self) -> Result<Option<(Vec<u8>, usize)>> {
        if self.pending_reports.is_empty() {
            return Ok(None);
        }
        let mut datagram = Vec::with_capacity(MAX_DATAGRAM_BYTES);
        self.write_fixed_part(&mut datagram)?;
        let set_start = begin_set(&mut datagram, SENDER_TEMPLATE_ID);
        let mut record = Vec::with_capacity(64);
        let mut count = 0usize;
        while count < self.pending_reports.len() {
            Self::encode_record(&self.pending_reports[count], &mut record);
            let set_len = datagram.len() - set_start + record.len();
            if set_start + padded(set_len) <= MAX_DATAGRAM_BYTES {
                datagram.extend_from_slice(&record);
                count += 1;
                continue;
            }
            if count > 0 {
                break;
            }
            // Nothing is in this datagram yet, so this report cannot fit in
            // any datagram (the fixed part never changes): drop it.
            if let Some(dropped) = self.pending_reports.pop_front() {
                warn!(
                    "PSKReporter: dropped the report for {}; it does not fit in a {}-byte datagram",
                    dropped.tx_callsign, MAX_DATAGRAM_BYTES
                );
            }
        }
        if count == 0 {
            return Ok(None);
        }
        end_set(&mut datagram, set_start)?;
        finish_datagram(&mut datagram)?;
        Ok(Some((datagram, count)))
    }

    /// Removes the `n` reports a sent datagram carried and advances the
    /// sequence number (PAN-285).
    fn mark_sent(&mut self, n: usize) {
        let n = n.min(self.pending_reports.len());
        self.pending_reports.drain(..n);
        self.sequence_number = self.sequence_number.wrapping_add(1);
    }

    /// Write a variable-length field: 1-byte length + data (for fields < 255 bytes)
    fn write_variable_field(packet: &mut Vec<u8>, data: &[u8]) {
        let len = data.len().min(254) as u8;
        packet.push(len);
        packet.extend_from_slice(&data[..len as usize]);
    }

    /// Send all pending reports to PSKReporter, in datagrams of at most
    /// [`MAX_DATAGRAM_BYTES`] from one socket.
    ///
    /// Returns the number of reports sent. On the first failed send it
    /// returns the error and keeps every report not yet sent; reports in
    /// datagrams already sent are removed, so a retry never repeats them
    /// (PAN-285).
    pub async fn flush(&mut self) -> Result<usize> {
        if self.pending_reports.is_empty() {
            return Ok(0);
        }

        if self.config.reporter_callsign.is_empty() {
            return Err(DxError::from(DxErrorInner::Configuration(
                "Reporter callsign must be set for PSKReporter upload".to_string(),
            )));
        }

        let addr = format!("{}:{}", self.config.server, self.config.port);
        // Bind to 0.0.0.0:0 so the kernel can route the outbound packet to the
        // public PSKReporter server (a 127.0.0.1 source can't reach the internet).
        let socket = tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(DxError::from)?;

        // [sec I-14] Restrict the socket to its single peer. `connect` on a UDP
        // socket sets the default destination AND makes the kernel drop any
        // inbound datagram that does not originate from that peer, closing the
        // ephemeral-port exposure of the all-interfaces bind. This path is
        // send-only (we `send` then drop the socket; we never `recv`), so the
        // connect is purely a hardening measure with no behavioral cost.
        socket.connect(&addr).await.map_err(DxError::from)?;

        self.flush_to(&socket).await
    }

    /// The datagram loop behind [`Self::flush`]: a datagram's reports leave
    /// the queue only after its send returns `Ok`, and the first error stops
    /// the loop (PAN-285).
    async fn flush_to<S: DatagramSink>(&mut self, sink: &S) -> Result<usize> {
        let mut reports = 0usize;
        let mut datagrams = 0usize;
        while let Some((datagram, n)) = self.next_datagram()? {
            if let Err(e) = sink.send_datagram(&datagram).await {
                if reports > 0 {
                    info!(
                        "Sent {} reception reports to PSKReporter in {} datagrams before a send failed (delivery unconfirmed)",
                        reports, datagrams
                    );
                }
                return Err(DxError::from(DxErrorInner::Io(e)));
            }
            debug!(
                "PSKReporter datagram seq={}: {} reports, {} bytes",
                self.sequence_number,
                n,
                datagram.len()
            );
            self.mark_sent(n);
            reports += n;
            datagrams += 1;
        }
        // UDP is fire-and-forget: this only confirms the local socket writes
        // succeeded, not that PSKReporter's server received or accepted them.
        info!(
            "Sent {} reception reports to PSKReporter in {} datagrams (delivery unconfirmed)",
            reports, datagrams
        );
        Ok(reports)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_creation() {
        let config = PskReporterConfig::default();
        assert!(config.base_url.contains("pskreporter"));
        assert_eq!(config.timeout_seconds, 30);
        assert_eq!(config.rate_limit_ms, 2000);
    }

    #[test]
    fn test_client_creation() {
        let client = PskReporterClient::new();
        assert_eq!(client.config.software, "Pancetta-DX");
    }

    #[test]
    fn test_query_params_default() {
        let params = QueryParams::default();
        assert!(params.start_time < params.end_time);
        assert!(params.callsign.is_none());
    }

    #[test]
    fn test_mode_parsing() {
        let client = PskReporterClient::new();

        assert_eq!(client.parse_mode("FT8"), Some(Mode::FT8));
        assert_eq!(client.parse_mode("ft4"), Some(Mode::FT4));
        assert_eq!(client.parse_mode("PSK31"), Some(Mode::PSK31));
        assert_eq!(client.parse_mode("UNKNOWN"), None);
    }

    #[test]
    fn test_url_building() {
        let client = PskReporterClient::new();
        let params = QueryParams {
            start_time: DateTime::from_timestamp(1600000000, 0).unwrap(),
            end_time: DateTime::from_timestamp(1600003600, 0).unwrap(),
            callsign: Some("W1ABC".to_string()),
            min_frequency: Some(14_000_000),
            max_frequency: Some(14_350_000),
            ..Default::default()
        };

        let url = client.build_query_url(&params).unwrap();
        let url_str = url.as_str();

        assert!(url_str.contains("flowStartSeconds=1600000000"));
        assert!(url_str.contains("flowEndSeconds=1600003600"));
        assert!(url_str.contains("txCall=W1ABC"));
        assert!(url_str.contains("fLow=14000000"));
        assert!(url_str.contains("fHigh=14350000"));
    }

    #[tokio::test]
    async fn test_rate_limiting() {
        let mut client = PskReporterClient::new();
        client.config.rate_limit_ms = 100;

        let start = std::time::Instant::now();
        client.rate_limit().await.unwrap(); // First call should not delay
        let first_elapsed = start.elapsed();

        client.last_request = Some(Utc::now());
        client.rate_limit().await.unwrap(); // Second call should delay
        let second_elapsed = start.elapsed();

        assert!(first_elapsed.as_millis() < 50);
        assert!(second_elapsed.as_millis() >= 100);
    }

    #[test]
    fn test_spot_conversion() {
        let client = PskReporterClient::new();
        let psk_spot = PskReporterSpot {
            transmitter_call: "W1ABC".to_string(),
            frequency: 14_074_000,
            mode: "FT8".to_string(),
            receiver_call: "VE3XYZ".to_string(),
            flow_start_seconds: 1600000000,
            snr: Some(-5),
            transmitter_locator: Some("FN42".to_string()),
            receiver_locator: Some("FN03".to_string()),
        };

        let dx_spot = client.convert_spot(psk_spot).unwrap();

        assert_eq!(dx_spot.callsign, "W1ABC");
        assert_eq!(dx_spot.frequency, 14_074_000);
        assert_eq!(dx_spot.mode, Some(Mode::FT8));
        assert_eq!(dx_spot.spotter, "VE3XYZ");
        assert_eq!(dx_spot.grid_square, Some("FN42".to_string()));
        assert!(dx_spot.comment.is_some());
        assert!(dx_spot.comment.unwrap().contains("SNR: -5"));
    }

    // =========================================================================
    // PSKReporter Uploader tests
    // =========================================================================

    #[test]
    fn test_uploader_creation() {
        let config = PskReporterUploadConfig {
            reporter_callsign: "W1ABC".to_string(),
            reporter_grid: "FN42".to_string(),
            ..Default::default()
        };
        let uploader = PskReporterUploader::new(config);
        assert_eq!(uploader.pending_count(), 0);
    }

    #[test]
    fn test_uploader_add_report() {
        let config = PskReporterUploadConfig {
            reporter_callsign: "W1ABC".to_string(),
            reporter_grid: "FN42".to_string(),
            ..Default::default()
        };
        let mut uploader = PskReporterUploader::new(config);

        uploader.add_report(ReceptionReport {
            tx_callsign: "VE3XYZ".to_string(),
            frequency: 14_074_000,
            snr: Some(-12),
            mode: "FT8".to_string(),
            tx_grid: Some("FN03".to_string()),
            timestamp: 1600000000,
        });

        assert_eq!(uploader.pending_count(), 1);
    }

    /// Walks the IPFIX Sets following the 16-byte header and returns, per
    /// Template Set or Options Template Set encountered, the set ID and the
    /// field IDs (enterprise bit masked off) declared by its Template Record.
    /// Uses each Set's own declared length to advance — never scans raw
    /// bytes — so it can't be fooled by coincidental byte matches in the
    /// random export-time/session-id header or in variable-length data
    /// payloads.
    fn parse_template_field_ids(packet: &[u8]) -> Vec<(u16, Vec<u16>)> {
        let mut templates = Vec::new();
        let mut pos = 16; // fixed IPFIX header size
        while pos + 4 <= packet.len() {
            let set_id = u16::from_be_bytes([packet[pos], packet[pos + 1]]);
            let set_len = u16::from_be_bytes([packet[pos + 2], packet[pos + 3]]) as usize;
            assert!(
                set_len >= 4,
                "Set at {pos} has implausible length {set_len}"
            );
            let set_end = pos + set_len;
            assert!(set_end <= packet.len(), "Set at {pos} overruns packet");

            if set_id == 2 || set_id == 3 {
                // Template Record: template_id(2) + field_count(2)
                // [+ scope_field_count(2) for an Options Template], then per
                // field: field_specifier(2) + length(2) [+ enterprise_number(4)
                // if the enterprise bit is set].
                let mut p = pos + 4 + 2; // skip Set header (4) + Template ID (2)
                let field_count = u16::from_be_bytes([packet[p], packet[p + 1]]);
                p += 2;
                if set_id == 3 {
                    p += 2; // scope field count
                }
                let mut field_ids = Vec::new();
                for _ in 0..field_count {
                    let specifier = u16::from_be_bytes([packet[p], packet[p + 1]]);
                    p += 4; // specifier(2) + field_length(2)
                    if specifier & 0x8000 != 0 {
                        p += 4; // enterprise_number
                    }
                    field_ids.push(specifier & 0x7FFF);
                }
                templates.push((set_id, field_ids));
            }
            pos = set_end;
        }
        templates
    }

    /// Every set as (set ID, set bytes), walked by declared lengths. Asserts
    /// the sets end exactly at the datagram end.
    fn sets(datagram: &[u8]) -> Vec<(u16, &[u8])> {
        let mut out = Vec::new();
        let mut pos = 16;
        while pos < datagram.len() {
            let set_id = u16::from_be_bytes([datagram[pos], datagram[pos + 1]]);
            let set_len = u16::from_be_bytes([datagram[pos + 2], datagram[pos + 3]]) as usize;
            assert!(set_len >= 4 && pos + set_len <= datagram.len());
            out.push((set_id, &datagram[pos..pos + set_len]));
            pos += set_len;
        }
        assert_eq!(pos, datagram.len(), "sets must end at the datagram end");
        out
    }

    /// One decoded sender record, in Pancetta's layout.
    #[derive(Debug, PartialEq)]
    struct WireRecord {
        call: String,
        freq: u32,
        snr: i8,
        mode: String,
        info_source: u8,
        time: u32,
    }

    /// Smallest possible sender record: empty callsign and mode.
    const MIN_RECORD_BYTES: usize = 1 + 4 + 1 + 1 + 1 + 4;

    /// Walks the sets after the 16-byte header by their declared lengths and
    /// decodes every record in the 0x5003 data set. Stops when fewer bytes
    /// remain than the smallest possible record (so padding is never read as
    /// a record). Asserts the header length equals the datagram length.
    fn sender_records(datagram: &[u8]) -> Vec<WireRecord> {
        assert_eq!(
            u16::from_be_bytes([datagram[2], datagram[3]]) as usize,
            datagram.len(),
            "header length must equal the datagram length"
        );
        let mut records = Vec::new();
        for (set_id, set) in sets(datagram) {
            if set_id != SENDER_TEMPLATE_ID {
                continue;
            }
            let mut p = 4;
            while set.len() - p >= MIN_RECORD_BYTES {
                let call_len = set[p] as usize;
                let call = String::from_utf8(set[p + 1..p + 1 + call_len].to_vec()).unwrap();
                p += 1 + call_len;
                let freq = u32::from_be_bytes(set[p..p + 4].try_into().unwrap());
                p += 4;
                let snr = set[p] as i8;
                p += 1;
                let mode_len = set[p] as usize;
                let mode = String::from_utf8(set[p + 1..p + 1 + mode_len].to_vec()).unwrap();
                p += 1 + mode_len;
                let info_source = set[p];
                p += 1;
                let time = u32::from_be_bytes(set[p..p + 4].try_into().unwrap());
                p += 4;
                records.push(WireRecord {
                    call,
                    freq,
                    snr,
                    mode,
                    info_source,
                    time,
                });
            }
        }
        records
    }

    /// In-memory `DatagramSink`: records every datagram, and fails the send
    /// whose zero-based index is `fail_at`, and every later one.
    struct TestSink {
        sent: std::cell::RefCell<Vec<Vec<u8>>>,
        fail_at: Option<usize>,
    }

    impl TestSink {
        fn ok() -> Self {
            Self {
                sent: std::cell::RefCell::new(Vec::new()),
                fail_at: None,
            }
        }

        fn failing_at(k: usize) -> Self {
            Self {
                sent: std::cell::RefCell::new(Vec::new()),
                fail_at: Some(k),
            }
        }
    }

    impl DatagramSink for TestSink {
        async fn send_datagram(&self, datagram: &[u8]) -> std::io::Result<()> {
            let index = self.sent.borrow().len();
            if self.fail_at.is_some_and(|k| index >= k) {
                return Err(std::io::Error::other("simulated outage"));
            }
            self.sent.borrow_mut().push(datagram.to_vec());
            Ok(())
        }
    }

    fn report_n(i: usize) -> ReceptionReport {
        ReceptionReport {
            tx_callsign: format!("K{i:04}"),
            frequency: 14_074_000,
            snr: Some(-10),
            mode: "FT8".to_string(),
            tx_grid: None,
            timestamp: 1_600_000_000 + i as i64,
        }
    }

    fn uploader_with_cap(cap: usize) -> PskReporterUploader {
        PskReporterUploader::new(PskReporterUploadConfig {
            reporter_callsign: "W1ABC".to_string(),
            reporter_grid: "FN42".to_string(),
            max_pending_reports: cap,
            ..Default::default()
        })
    }

    fn calls_in(datagrams: &[Vec<u8>]) -> Vec<String> {
        datagrams
            .iter()
            .flat_map(|d| sender_records(d))
            .map(|r| r.call)
            .collect()
    }

    fn hex_bytes(hex: &str) -> Vec<u8> {
        hex.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    /// PAN-108: the receiver-info template (our station) and sender-info
    /// template (the spotted station) must declare PSKReporter's published
    /// enterprise field IDs, not the pre-fix values (which used the
    /// sender-template IDs 1/3/4/5 for what is actually the receiver-info
    /// template, and field ID 2 instead of 5 for frequency). PAN-285: the
    /// receiver descriptor is an Options Template Set (ID 3) and the sender
    /// template carries informationSource (11).
    #[test]
    fn test_field_ids_match_pskreporter_protocol() {
        let config = PskReporterUploadConfig {
            reporter_callsign: "W1ABC".to_string(),
            reporter_grid: "FN42".to_string(),
            antenna: "Dipole".to_string(),
            software: "Pancetta/0.1".to_string(),
            ..Default::default()
        };
        let mut uploader = PskReporterUploader::new(config);
        uploader.add_report(ReceptionReport {
            tx_callsign: "K1DEF".to_string(),
            frequency: 14_074_000,
            snr: Some(-5),
            mode: "FT8".to_string(),
            tx_grid: None,
            timestamp: 1_600_000_000,
        });

        let packet = uploader.next_datagram().unwrap().unwrap().0;
        let templates = parse_template_field_ids(&packet);

        assert_eq!(templates.len(), 2, "expected exactly two Template Sets");
        assert_eq!(
            templates[0],
            (3, vec![0x0002, 0x0004, 0x0008, 0x0009]),
            "receiver-info options template (our station) has the wrong set or field IDs"
        );
        assert_eq!(
            templates[1],
            (2, vec![0x0001, 0x0005, 0x0006, 0x000A, 0x000B, 0x0096]),
            "sender-info template (the spotted station) has the wrong set or field IDs"
        );
    }

    #[test]
    fn test_uploader_build_packet_structure() {
        let config = PskReporterUploadConfig {
            reporter_callsign: "W1ABC".to_string(),
            reporter_grid: "FN42".to_string(),
            antenna: "Dipole".to_string(),
            software: "Pancetta/0.1".to_string(),
            ..Default::default()
        };
        let mut uploader = PskReporterUploader::new(config);

        uploader.add_report(ReceptionReport {
            tx_callsign: "K1DEF".to_string(),
            frequency: 14_074_000,
            snr: Some(-5),
            mode: "FT8".to_string(),
            tx_grid: None,
            timestamp: 1600000000,
        });

        let packet = uploader.next_datagram().unwrap().unwrap().0;

        // Verify IPFIX header
        assert_eq!(packet[0], 0x00); // Version high byte
        assert_eq!(packet[1], 0x0A); // Version low byte = 10

        // Verify total length matches
        let total_len = u16::from_be_bytes([packet[2], packet[3]]) as usize;
        assert_eq!(total_len, packet.len());

        // Verify packet is reasonable size (header + descriptors + data)
        assert!(
            packet.len() > 16,
            "Packet should be larger than just header"
        );
        assert!(
            packet.len() <= MAX_DATAGRAM_BYTES,
            "Packet should fit in one datagram for a single report"
        );

        // Building a datagram neither removes reports nor advances the
        // sequence number; only a successful send does (PAN-285).
        assert_eq!(uploader.sequence_number, 0);
        assert_eq!(uploader.pending_count(), 1);
    }

    #[test]
    fn test_uploader_multiple_reports() {
        let config = PskReporterUploadConfig {
            reporter_callsign: "W1ABC".to_string(),
            reporter_grid: "FN42".to_string(),
            ..Default::default()
        };
        let mut uploader = PskReporterUploader::new(config);

        for i in 0..5 {
            uploader.add_report(ReceptionReport {
                tx_callsign: format!("K{}DEF", i),
                frequency: 14_074_000 + i as u64 * 100,
                snr: Some(-10 + i),
                mode: "FT8".to_string(),
                tx_grid: None,
                timestamp: 1600000000 + i as i64 * 15,
            });
        }

        assert_eq!(uploader.pending_count(), 5);

        let (packet, n) = uploader.next_datagram().unwrap().unwrap();
        // Multiple reports should produce a larger packet
        assert!(packet.len() > 100);
        assert_eq!(n, 5);
        assert_eq!(sender_records(&packet).len(), 5);
    }

    // --- PAN-285: bounded queue ---

    #[test]
    fn queue_holds_at_most_the_cap_and_drops_the_oldest() {
        let mut u = uploader_with_cap(2000);
        for i in 0..2500 {
            u.add_report(report_n(i));
        }
        assert_eq!(u.pending_count(), 2000);
        assert_eq!(u.dropped_count(), 500);
        assert_eq!(u.pending_reports.front().unwrap().tx_callsign, "K0500");
        assert_eq!(u.pending_reports.back().unwrap().tx_callsign, "K2499");
    }

    #[test]
    fn default_cap_is_2000() {
        assert_eq!(PskReporterUploadConfig::default().max_pending_reports, 2000);
        assert_eq!(DEFAULT_MAX_PENDING_REPORTS, 2000);
    }

    #[test]
    fn cap_of_zero_keeps_the_newest_report() {
        let mut u = uploader_with_cap(0);
        for i in 0..3 {
            u.add_report(report_n(i));
        }
        assert_eq!(u.pending_count(), 1);
        assert_eq!(u.dropped_count(), 2);
        assert_eq!(u.pending_reports.front().unwrap().tx_callsign, "K0002");
    }

    // --- PAN-285: chunked flush, removal after send, checked lengths ---

    #[tokio::test]
    async fn backlog_is_sent_in_datagrams_of_at_most_1400_bytes() {
        let mut u = uploader_with_cap(2000);
        for i in 0..2000 {
            u.add_report(report_n(i));
        }
        let sink = TestSink::ok();
        assert_eq!(u.flush_to(&sink).await.unwrap(), 2000);
        assert_eq!(u.pending_count(), 0);
        let sent = sink.sent.into_inner();
        assert!(
            sent.len() <= 2000usize.div_ceil(60),
            "{} datagrams",
            sent.len()
        );
        for d in &sent {
            assert!(
                d.len() <= MAX_DATAGRAM_BYTES,
                "datagram of {} bytes",
                d.len()
            );
            assert_eq!(u16::from_be_bytes([d[2], d[3]]) as usize, d.len());
        }
        assert_eq!(
            calls_in(&sent),
            (0..2000).map(|i| format!("K{i:04}")).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn a_failed_send_keeps_every_unsent_report_and_none_that_were_sent() {
        let mut u = uploader_with_cap(2000);
        for i in 0..200 {
            u.add_report(report_n(i));
        }
        let failing = TestSink::failing_at(2);
        assert!(u.flush_to(&failing).await.is_err());
        let first = calls_in(&failing.sent.into_inner());
        assert!(
            !first.is_empty(),
            "two datagrams went out before the failure"
        );
        assert_eq!(u.pending_count(), 200 - first.len());
        assert_eq!(
            u.pending_reports.front().unwrap().tx_callsign,
            format!("K{:04}", first.len())
        );
        let ok = TestSink::ok();
        assert_eq!(u.flush_to(&ok).await.unwrap(), 200 - first.len());
        let rest = calls_in(&ok.sent.into_inner());
        let all: Vec<String> = first.into_iter().chain(rest).collect();
        assert_eq!(
            all,
            (0..200).map(|i| format!("K{i:04}")).collect::<Vec<_>>()
        );
    }

    /// The ticket's scenario: two hours of FT8 (480 slots, 10 decodes each)
    /// with every 300 s flush failing, then the server comes back.
    #[tokio::test]
    async fn two_hour_outage_stays_within_the_cap_and_drains_when_the_server_returns() {
        let mut u = uploader_with_cap(2000);
        let down = TestSink::failing_at(0);
        let mut next = 0;
        for slot in 0..480 {
            for _ in 0..10 {
                u.add_report(report_n(next));
                next += 1;
            }
            assert!(u.pending_count() <= 2000);
            if slot % 20 == 19 {
                assert!(u.flush_to(&down).await.is_err());
            }
        }
        assert_eq!(u.pending_count(), 2000);
        assert_eq!(u.dropped_count(), 2800);
        let up = TestSink::ok();
        assert_eq!(u.flush_to(&up).await.unwrap(), 2000);
        assert_eq!(u.pending_count(), 0);
        let sent = up.sent.into_inner();
        assert!(sent.iter().all(|d| d.len() <= MAX_DATAGRAM_BYTES));
        assert_eq!(
            calls_in(&sent),
            (2800..4800).map(|i| format!("K{i:04}")).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn a_report_too_large_for_any_datagram_is_dropped_and_the_rest_are_sent() {
        let long = "X".repeat(254);
        let mut u = PskReporterUploader::new(PskReporterUploadConfig {
            reporter_callsign: long.clone(),
            reporter_grid: long.clone(),
            antenna: long.clone(),
            software: long.clone(),
            ..Default::default()
        });
        u.add_report(ReceptionReport {
            tx_callsign: long.clone(),
            mode: long.clone(),
            ..report_n(0)
        });
        u.add_report(report_n(1));
        let sink = TestSink::ok();
        assert_eq!(u.flush_to(&sink).await.unwrap(), 1);
        assert_eq!(u.pending_count(), 0);
        let sent = sink.sent.into_inner();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].len() <= MAX_DATAGRAM_BYTES);
        assert_eq!(sender_records(&sent[0])[0].call, "K0001");
    }

    #[test]
    fn a_length_over_16_bits_is_an_error_not_a_wrap() {
        let mut buf = vec![0u8; 4];
        assert!(patch_len(&mut buf, 2, 70_000).is_err());
        assert!(patch_len(&mut buf, 2, 1400).is_ok());
        assert_eq!(u16::from_be_bytes([buf[2], buf[3]]), 1400);
    }

    #[test]
    fn sequence_number_advances_once_per_sent_datagram() {
        let mut u = uploader_with_cap(2000);
        for i in 0..100 {
            u.add_report(report_n(i));
        }
        let (first, n) = u.next_datagram().unwrap().unwrap();
        assert_eq!(u32::from_be_bytes(first[8..12].try_into().unwrap()), 0);
        assert_eq!(u.pending_count(), 100, "building removes nothing");
        u.mark_sent(n);
        assert_eq!(u.pending_count(), 100 - n);
        let (second, _) = u.next_datagram().unwrap().unwrap();
        assert_eq!(u32::from_be_bytes(second[8..12].try_into().unwrap()), 1);
    }

    /// Real loopback UDP, real clock (MAN-506: never a paused clock around a
    /// socket read).
    #[tokio::test]
    async fn flush_sends_a_backlog_over_udp_in_datagrams_of_at_most_1400_bytes() {
        let rx = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut u = PskReporterUploader::new(PskReporterUploadConfig {
            reporter_callsign: "W1ABC".into(),
            reporter_grid: "FN42".into(),
            server: "127.0.0.1".into(),
            port: rx.local_addr().unwrap().port(),
            ..Default::default()
        });
        for i in 0..2000 {
            u.add_report(report_n(i));
        }
        assert_eq!(u.flush().await.unwrap(), 2000);
        let mut got = 0;
        let mut buf = vec![0u8; 65_536];
        while got < 2000 {
            let n = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv(&mut buf))
                .await
                .expect("datagram within 5 s")
                .unwrap();
            assert!(n <= MAX_DATAGRAM_BYTES);
            got += sender_records(&buf[..n]).len();
        }
        assert_eq!(got, 2000);
        assert_eq!(u.pending_count(), 0);
    }

    /// A local network failure (connect refused by the OS) leaves the queue
    /// intact. Linux-only: CI runs tests on ubuntu-latest, where an IPv6
    /// connect from the uploader's IPv4 socket fails with `os error 97`; the
    /// macOS errno for this case was not verified.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn flush_with_no_usable_route_keeps_the_queue() {
        let mut u = PskReporterUploader::new(PskReporterUploadConfig {
            reporter_callsign: "W1ABC".into(),
            server: "[::1]".into(),
            ..Default::default()
        });
        for i in 0..10 {
            u.add_report(report_n(i));
        }
        assert!(u.flush().await.is_err());
        assert_eq!(u.pending_count(), 10);
    }

    // --- PAN-285: wire layout from https://pskreporter.info/pskdev.html ---

    /// https://pskreporter.info/pskdev.html, "An example of a complete
    /// datagram" (read 2026-11-10): header, receiver options template (3
    /// fields, scope 1), sender template (5 fields), receiver record, two
    /// sender records, padded.
    const PSKDEV_EXAMPLE: &str = "\
00 0A 00 AC 47 95 32 72 00 00 00 01 00 00 00 00 \
00 03 00 24 99 92 00 03 00 01 80 02 FF FF 00 00 76 8F 80 04 FF FF 00 00 76 8F 80 08 FF FF 00 00 76 8F 00 00 \
00 02 00 2C 99 93 00 05 80 01 FF FF 00 00 76 8F 80 05 00 04 00 00 76 8F 80 0A FF FF 00 00 76 8F 80 0B 00 01 00 00 76 8F 00 96 00 04 \
99 92 00 20 04 4E 31 44 51 06 46 4E 34 32 68 6E 0D 48 6F 6D 65 62 72 65 77 20 76 35 2E 36 00 00 \
99 93 00 2C 04 4E 31 44 51 00 D6 B3 27 03 50 53 4B 01 47 95 32 54 06 4B 42 31 4D 42 58 00 D6 B4 CB 03 50 53 4B 01 47 95 32 68 00 00";

    #[test]
    fn rebuilds_the_pskdev_example_datagram_byte_for_byte() {
        let mut built = Vec::new();
        write_header(&mut built, 1_200_960_114, 1, 0);
        write_template_set(
            &mut built,
            SET_ID_OPTIONS_TEMPLATE,
            0x9992,
            Some(1),
            &[
                Field::pen(2, VARIABLE_LENGTH),
                Field::pen(4, VARIABLE_LENGTH),
                Field::pen(8, VARIABLE_LENGTH),
            ],
        )
        .unwrap();
        write_template_set(
            &mut built,
            SET_ID_TEMPLATE,
            0x9993,
            None,
            &[
                Field::pen(1, VARIABLE_LENGTH),
                Field::pen(5, 4),
                Field::pen(10, VARIABLE_LENGTH),
                Field::pen(11, 1),
                Field::iana(150, 4),
            ],
        )
        .unwrap();
        let receiver = begin_set(&mut built, 0x9992);
        PskReporterUploader::write_variable_field(&mut built, b"N1DQ");
        PskReporterUploader::write_variable_field(&mut built, b"FN42hn");
        PskReporterUploader::write_variable_field(&mut built, b"Homebrew v5.6");
        end_set(&mut built, receiver).unwrap();
        let senders = begin_set(&mut built, 0x9993);
        for (call, freq, time) in [
            (&b"N1DQ"[..], 14_070_567u32, 1_200_960_084u32),
            (&b"KB1MBX"[..], 14_070_987u32, 1_200_960_104u32),
        ] {
            PskReporterUploader::write_variable_field(&mut built, call);
            built.extend_from_slice(&freq.to_be_bytes());
            PskReporterUploader::write_variable_field(&mut built, b"PSK");
            built.push(INFORMATION_SOURCE_AUTOMATIC);
            built.extend_from_slice(&time.to_be_bytes());
        }
        end_set(&mut built, senders).unwrap();
        finish_datagram(&mut built).unwrap();

        let expected = hex_bytes(PSKDEV_EXAMPLE);
        assert_eq!(expected.len(), 172);
        assert_eq!(built, expected);
    }

    #[test]
    fn pancetta_datagram_follows_the_pskdev_layout_rules() {
        let mut u = PskReporterUploader::new(PskReporterUploadConfig {
            reporter_callsign: "W1ABC".to_string(),
            reporter_grid: "FN42".to_string(),
            antenna: "Dipole".to_string(),
            software: "Pancetta/0.1".to_string(),
            ..Default::default()
        });
        u.add_report(ReceptionReport {
            tx_callsign: "K1DEF".to_string(),
            frequency: 14_074_000,
            snr: Some(-5),
            mode: "FT4".to_string(),
            tx_grid: None,
            timestamp: 1_600_000_000,
        });
        let datagram = u.next_datagram().unwrap().unwrap().0;

        let all_sets = sets(&datagram);
        assert_eq!(all_sets.len(), 4);
        for (set_id, set) in &all_sets {
            assert_eq!(set.len() % 4, 0, "set {set_id:#06x} is not padded to 4");
        }

        // Receiver descriptor: Options Template Set, scope count 1, equal to
        // the page's four-field descriptor with our template ID.
        let (set_id, receiver_template) = all_sets[0];
        assert_eq!(set_id, SET_ID_OPTIONS_TEMPLATE);
        let mut expected_receiver = hex_bytes(
            "00 03 00 2C 99 92 00 04 00 01 80 02 FF FF 00 00 76 8F 80 04 FF FF 00 00 76 8F \
             80 08 FF FF 00 00 76 8F 80 09 FF FF 00 00 76 8F 00 00",
        );
        expected_receiver[4..6].copy_from_slice(&[0x50, 0x02]);
        assert_eq!(receiver_template, &expected_receiver[..]);
        assert_eq!(
            u16::from_be_bytes([receiver_template[8], receiver_template[9]]),
            1
        );

        let templates = parse_template_field_ids(&datagram);
        assert_eq!(templates[0], (3, vec![2, 4, 8, 9]));
        assert_eq!(templates[1], (2, vec![1, 5, 6, 10, 11, 150]));

        assert_eq!(
            sender_records(&datagram),
            vec![WireRecord {
                call: "K1DEF".to_string(),
                freq: 14_074_000,
                snr: -5,
                mode: "FT4".to_string(),
                info_source: 1,
                time: 1_600_000_000,
            }]
        );
        assert_eq!(datagram.len(), 172);
    }

    #[test]
    fn test_variable_field_encoding() {
        let mut buf = Vec::new();
        PskReporterUploader::write_variable_field(&mut buf, b"W1ABC");
        assert_eq!(buf[0], 5); // length
        assert_eq!(&buf[1..6], b"W1ABC");
    }

    #[tokio::test]
    async fn test_uploader_flush_empty() {
        let config = PskReporterUploadConfig {
            reporter_callsign: "W1ABC".to_string(),
            reporter_grid: "FN42".to_string(),
            ..Default::default()
        };
        let mut uploader = PskReporterUploader::new(config);

        // Flushing with no reports should return Ok(0)
        let result = uploader.flush().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_uploader_flush_no_callsign() {
        let config = PskReporterUploadConfig::default(); // Empty callsign
        let mut uploader = PskReporterUploader::new(config);
        uploader.add_report(ReceptionReport {
            tx_callsign: "K1DEF".to_string(),
            frequency: 14_074_000,
            snr: Some(-5),
            mode: "FT8".to_string(),
            tx_grid: None,
            timestamp: 1600000000,
        });

        // Should fail because reporter callsign is empty
        let result = uploader.flush().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn query_spots_over_budget_is_refused() {
        let (addr, h) = crate::test_support::serve_chunked(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json",
            b"",
            PSKREPORTER_MAX_BODY_BYTES + 1,
        );
        let mut client = PskReporterClient::with_config(PskReporterConfig {
            base_url: format!("http://{addr}/query"),
            rate_limit_ms: 0,
            ..Default::default()
        });
        let err = client
            .query_spots(QueryParams::default())
            .await
            .unwrap_err();
        assert!(
            matches!(err.kind(), DxErrorInner::ResponseTooLarge { limit } if *limit == PSKREPORTER_MAX_BODY_BYTES),
            "got {err:?}"
        );
        crate::test_support::served(h).await;
    }
}
