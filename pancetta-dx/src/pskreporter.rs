//! PSKReporter Client
//!
//! This module provides integration with PSKReporter for real-time
//! digital mode activity monitoring and spot collection.

use crate::{Band, Mode, DxSpot, DxError, Result};
use chrono::{DateTime, Utc, Duration};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::time::{sleep, Duration as TokioDuration};
use tracing::{debug, info, warn, error};

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
    /// Receiver locator (grid square)
    #[serde(rename = "receiverLocator")]
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
    /// Statistics
    #[serde(rename = "statistics")]
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
        
        let response = self.client.get(url)
            .send()
            .await
            .map_err(|e| DxError::Network(e))?;
        
        if !response.status().is_success() {
            return Err(DxError::ExternalService(
                format!("PSKReporter API error: HTTP {}", response.status())
            ));
        }
        
        let psk_response: PskReporterResponse = response.json()
            .await
            .map_err(|e| DxError::Parse(format!("Failed to parse PSKReporter response: {}", e)))?;
        
        if psk_response.status != "OK" {
            return Err(DxError::ExternalService(
                format!("PSKReporter API error: {}", 
                       psk_response.error.unwrap_or("Unknown error".to_string()))
            ));
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
        
        info!("Starting PSKReporter real-time monitoring (poll interval: {}s)", poll_interval_seconds);
        
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
                },
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
            distance_km: None, // Will be calculated by DX Hunter
            bearing_degrees: None, // Will be calculated by DX Hunter
            rarity_score: None, // Will be calculated by DX Hunter
        })
    }
    
    /// Parse mode string from PSKReporter
    fn parse_mode(&self, mode_str: &str) -> Option<Mode> {
        match mode_str.to_uppercase().as_str() {
            "FT8" => Some(Mode::FT8),
            "FT4" => Some(Mode::FT4),
            "JT65" => None, // JT65 not in unified Mode
            "JT9" => None, // JT9 not in unified Mode
            "MSK144" => None, // MSK144 not in unified Mode,
            "JS8" => Some(Mode::JS8),
            "PSK31" => Some(Mode::PSK31),
            "PSK63" => Some(Mode::PSK63),
            "RTTY" => Some(Mode::RTTY),
            "OLIVIA" => None, // OLIVIA not in unified Mode
            "CONTESTIA" => None, // CONTESTIA not in unified Mode
            "THOR" => None, // THOR not in unified Mode
            "DOMINO" => None, // DOMINO not in unified Mode
            "HELL" => None, // HELL not in unified Mode,
            "MFSK" => None, // MFSK not in unified Mode,
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
                let sleep_duration = (min_interval - elapsed).to_std()
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
}