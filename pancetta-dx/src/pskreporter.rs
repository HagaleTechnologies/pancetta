//! PSKReporter Client
//!
//! This module provides integration with PSKReporter for real-time
//! digital mode activity monitoring and spot collection.

// rationale: the crate-wide `DxError` is intentionally a flat (non-boxed) enum for
// ergonomic `?`; boxing it crate-wide to satisfy this lint is out of scope here.
#![allow(clippy::result_large_err)]

use crate::{Band, DxError, DxSpot, Mode, Result};
use chrono::{DateTime, Duration, Utc};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::time::{sleep, Duration as TokioDuration};
use tracing::{debug, error, info, warn};

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

        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(DxError::Network)?;

        if !response.status().is_success() {
            return Err(DxError::ExternalService(format!(
                "PSKReporter API error: HTTP {}",
                response.status()
            )));
        }

        let psk_response: PskReporterResponse = response
            .json()
            .await
            .map_err(|e| DxError::Parse(format!("Failed to parse PSKReporter response: {}", e)))?;

        if psk_response.status != "OK" {
            return Err(DxError::ExternalService(format!(
                "PSKReporter API error: {}",
                psk_response.error.unwrap_or("Unknown error".to_string())
            )));
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
        let mut url = Url::parse(&self.config.base_url)
            .map_err(|e| DxError::Configuration(format!("Invalid base URL: {}", e)))?;

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
            .ok_or_else(|| DxError::Parse("Invalid timestamp".to_string()))?;

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
#[derive(Debug, Clone)]
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
        }
    }
}

/// PSKReporter uploader that batches and sends reception reports
pub struct PskReporterUploader {
    config: PskReporterUploadConfig,
    /// Pending reports to be uploaded
    pending_reports: Vec<ReceptionReport>,
    /// Random identifier for this session
    session_id: u32,
    /// Sequence number for packets
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
            pending_reports: Vec::new(),
            session_id,
            sequence_number: 0,
        }
    }

    /// Add a reception report to the upload queue
    pub fn add_report(&mut self, report: ReceptionReport) {
        self.pending_reports.push(report);
    }

    /// Get number of pending reports
    pub fn pending_count(&self) -> usize {
        self.pending_reports.len()
    }

    /// Build the binary UDP packet for PSKReporter
    ///
    /// PSKReporter uses an IPFIX-inspired binary format:
    /// - Header (16 bytes): version, length, export time, sequence, observation domain
    /// - Sender descriptor (template record describing sender info fields)
    /// - Receiver descriptor (template record describing reception report fields)
    /// - Sender data (our station info)
    /// - Receiver data (the spots we're reporting)
    fn build_packet(&mut self) -> Vec<u8> {
        let mut packet = Vec::with_capacity(1024);

        // We'll fill in the header length later
        let header_pos = packet.len();
        // IPFIX header: version=10, length=TBD, export_time, seq, observation_domain_id
        packet.extend_from_slice(&0x000Au16.to_be_bytes()); // Version 10
        packet.extend_from_slice(&0u16.to_be_bytes()); // Length placeholder
        let export_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;
        packet.extend_from_slice(&export_time.to_be_bytes());
        packet.extend_from_slice(&self.sequence_number.to_be_bytes());
        packet.extend_from_slice(&self.session_id.to_be_bytes());

        // --- Sender information descriptor (Template Set, ID=2) ---
        // Template Set header: Set ID=2, Length
        let sender_desc_start = packet.len();
        packet.extend_from_slice(&2u16.to_be_bytes()); // Set ID for template
        packet.extend_from_slice(&0u16.to_be_bytes()); // Length placeholder
                                                       // Template Record: ID=0x5002, field count=4
        packet.extend_from_slice(&0x5002u16.to_be_bytes()); // Template ID
        packet.extend_from_slice(&4u16.to_be_bytes()); // Field count
                                                       // Field: senderCallsign (ID=1, PEN=30351, variable length)
        packet.extend_from_slice(&(0x8001u16).to_be_bytes()); // Enterprise bit + ID 1
        packet.extend_from_slice(&0xFFFFu16.to_be_bytes()); // Variable length
        packet.extend_from_slice(&30351u32.to_be_bytes()); // IANA PEN for PSKReporter
                                                           // Field: senderLocator (ID=3, PEN=30351, variable length)
        packet.extend_from_slice(&(0x8003u16).to_be_bytes());
        packet.extend_from_slice(&0xFFFFu16.to_be_bytes());
        packet.extend_from_slice(&30351u32.to_be_bytes());
        // Field: decoderSoftware (ID=4, PEN=30351, variable length)
        packet.extend_from_slice(&(0x8004u16).to_be_bytes());
        packet.extend_from_slice(&0xFFFFu16.to_be_bytes());
        packet.extend_from_slice(&30351u32.to_be_bytes());
        // Field: antennaInformation (ID=5, PEN=30351, variable length)
        packet.extend_from_slice(&(0x8005u16).to_be_bytes());
        packet.extend_from_slice(&0xFFFFu16.to_be_bytes());
        packet.extend_from_slice(&30351u32.to_be_bytes());
        // Patch sender descriptor length
        let sender_desc_len = (packet.len() - sender_desc_start) as u16;
        packet[sender_desc_start + 2..sender_desc_start + 4]
            .copy_from_slice(&sender_desc_len.to_be_bytes());

        // --- Receiver information descriptor (Template Set, ID=2) ---
        let rx_desc_start = packet.len();
        packet.extend_from_slice(&2u16.to_be_bytes());
        packet.extend_from_slice(&0u16.to_be_bytes()); // Length placeholder
                                                       // Template Record: ID=0x5003, field count=5
        packet.extend_from_slice(&0x5003u16.to_be_bytes());
        packet.extend_from_slice(&5u16.to_be_bytes());
        // Field: senderCallsign (reused ID=1, PEN=30351, variable length)
        packet.extend_from_slice(&(0x8001u16).to_be_bytes());
        packet.extend_from_slice(&0xFFFFu16.to_be_bytes());
        packet.extend_from_slice(&30351u32.to_be_bytes());
        // Field: frequency (ID=2, PEN=30351, 4 bytes)
        packet.extend_from_slice(&(0x8002u16).to_be_bytes());
        packet.extend_from_slice(&4u16.to_be_bytes());
        packet.extend_from_slice(&30351u32.to_be_bytes());
        // Field: sNR (ID=6, PEN=30351, 1 byte)
        packet.extend_from_slice(&(0x8006u16).to_be_bytes());
        packet.extend_from_slice(&1u16.to_be_bytes());
        packet.extend_from_slice(&30351u32.to_be_bytes());
        // Field: mode (ID=10, PEN=30351, variable length)
        packet.extend_from_slice(&(0x800Au16).to_be_bytes());
        packet.extend_from_slice(&0xFFFFu16.to_be_bytes());
        packet.extend_from_slice(&30351u32.to_be_bytes());
        // Field: flowStartSeconds (ID=150, standard IPFIX, 4 bytes)
        packet.extend_from_slice(&150u16.to_be_bytes());
        packet.extend_from_slice(&4u16.to_be_bytes());
        // Patch receiver descriptor length
        let rx_desc_len = (packet.len() - rx_desc_start) as u16;
        packet[rx_desc_start + 2..rx_desc_start + 4].copy_from_slice(&rx_desc_len.to_be_bytes());

        // --- Sender data record (Data Set, ID=0x5002) ---
        let sender_data_start = packet.len();
        packet.extend_from_slice(&0x5002u16.to_be_bytes());
        packet.extend_from_slice(&0u16.to_be_bytes()); // Length placeholder
                                                       // Variable-length encoding: length byte + data
        Self::write_variable_field(&mut packet, self.config.reporter_callsign.as_bytes());
        Self::write_variable_field(&mut packet, self.config.reporter_grid.as_bytes());
        Self::write_variable_field(&mut packet, self.config.software.as_bytes());
        Self::write_variable_field(&mut packet, self.config.antenna.as_bytes());
        // Patch sender data length
        let sender_data_len = (packet.len() - sender_data_start) as u16;
        packet[sender_data_start + 2..sender_data_start + 4]
            .copy_from_slice(&sender_data_len.to_be_bytes());

        // --- Receiver data records (Data Set, ID=0x5003) ---
        let rx_data_start = packet.len();
        packet.extend_from_slice(&0x5003u16.to_be_bytes());
        packet.extend_from_slice(&0u16.to_be_bytes()); // Length placeholder

        for report in &self.pending_reports {
            // senderCallsign (variable)
            Self::write_variable_field(&mut packet, report.tx_callsign.as_bytes());
            // frequency (4 bytes, u32)
            packet.extend_from_slice(&(report.frequency as u32).to_be_bytes());
            // SNR (1 byte, signed)
            packet.push(report.snr.unwrap_or(0) as u8);
            // mode (variable)
            Self::write_variable_field(&mut packet, report.mode.as_bytes());
            // flowStartSeconds (4 bytes, u32)
            packet.extend_from_slice(&(report.timestamp as u32).to_be_bytes());
        }

        // Patch receiver data length
        let rx_data_len = (packet.len() - rx_data_start) as u16;
        packet[rx_data_start + 2..rx_data_start + 4].copy_from_slice(&rx_data_len.to_be_bytes());

        // Patch total packet length in header
        let total_len = packet.len() as u16;
        packet[header_pos + 2..header_pos + 4].copy_from_slice(&total_len.to_be_bytes());

        self.sequence_number += 1;
        packet
    }

    /// Write a variable-length field: 1-byte length + data (for fields < 255 bytes)
    fn write_variable_field(packet: &mut Vec<u8>, data: &[u8]) {
        let len = data.len().min(254) as u8;
        packet.push(len);
        packet.extend_from_slice(&data[..len as usize]);
    }

    /// Send all pending reports to PSKReporter
    ///
    /// Returns the number of reports sent, or an error.
    pub async fn flush(&mut self) -> Result<usize> {
        if self.pending_reports.is_empty() {
            return Ok(0);
        }

        if self.config.reporter_callsign.is_empty() {
            return Err(DxError::Configuration(
                "Reporter callsign must be set for PSKReporter upload".to_string(),
            ));
        }

        let count = self.pending_reports.len();
        let packet = self.build_packet();

        info!(
            "Uploading {} reception reports to PSKReporter ({} bytes)",
            count,
            packet.len()
        );

        let addr = format!("{}:{}", self.config.server, self.config.port);
        // Bind to 0.0.0.0:0 so the kernel can route the outbound packet to the
        // public PSKReporter server (a 127.0.0.1 source can't reach the internet).
        let socket = tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(DxError::Io)?;

        // [sec I-14] Restrict the socket to its single peer. `connect` on a UDP
        // socket sets the default destination AND makes the kernel drop any
        // inbound datagram that does not originate from that peer, closing the
        // ephemeral-port exposure of the all-interfaces bind. This path is
        // send-only (we `send` then drop the socket; we never `recv`), so the
        // connect is purely a hardening measure with no behavioral cost.
        socket.connect(&addr).await.map_err(DxError::Io)?;

        socket.send(&packet).await.map_err(DxError::Io)?;

        info!("Successfully uploaded {} reports to PSKReporter", count);
        self.pending_reports.clear();
        Ok(count)
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

        let packet = uploader.build_packet();

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
            packet.len() < 2000,
            "Packet should be under 2KB for single report"
        );

        // Verify pending reports were cleared by sequence increment
        assert_eq!(uploader.sequence_number, 1);
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

        let packet = uploader.build_packet();
        // Multiple reports should produce a larger packet
        assert!(packet.len() > 100);
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
}
