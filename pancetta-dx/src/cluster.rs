//! DX Cluster Client
//!
//! This module provides connectivity to DX cluster networks for real-time
//! DX spot monitoring and posting.

use crate::{Band, DxError, DxSpot, Mode, Result};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};
use url::Url;

/// DX Cluster configuration
#[derive(Debug, Clone)]
pub struct ClusterConfig {
    /// Cluster hostname
    pub hostname: String,
    /// Cluster port
    pub port: u16,
    /// Login callsign
    pub callsign: String,
    /// Connection timeout in seconds
    pub timeout_seconds: u64,
    /// Reconnect delay in seconds
    pub reconnect_delay_seconds: u64,
    /// Enable automatic reconnection
    pub auto_reconnect: bool,
    /// Filter settings
    pub filter_settings: ClusterFilter,
    /// Use WebSocket connection if available
    pub use_websocket: bool,
    /// WebSocket URL (if different from telnet)
    pub websocket_url: Option<String>,
}

/// DX Cluster filter settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterFilter {
    /// Bands to monitor
    pub bands: Vec<Band>,
    /// Modes to monitor
    pub modes: Vec<Mode>,
    /// DXCC entities to monitor
    pub dxcc_entities: Vec<u16>,
    /// Minimum frequency in Hz
    pub min_frequency: Option<u64>,
    /// Maximum frequency in Hz
    pub max_frequency: Option<u64>,
    /// Spotted callsign patterns (regex)
    pub callsign_patterns: Vec<String>,
    /// Exclude these callsigns/patterns
    pub exclude_patterns: Vec<String>,
    /// Minimum time between duplicate spots (seconds)
    pub duplicate_timeout: u64,
}

impl Default for ClusterFilter {
    fn default() -> Self {
        Self {
            bands: Vec::new(),         // Empty = all bands
            modes: Vec::new(),         // Empty = all modes
            dxcc_entities: Vec::new(), // Empty = all entities
            min_frequency: None,
            max_frequency: None,
            callsign_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            duplicate_timeout: 300, // 5 minutes
        }
    }
}

/// DX Cluster connection status
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    /// Disconnected
    Disconnected,
    /// Connecting
    Connecting,
    /// Connected and logged in
    Connected,
    /// Connection error
    Error(String),
}

/// Parsed DX spot from cluster
#[derive(Debug, Clone)]
pub struct ClusterSpot {
    /// Spotted callsign
    pub callsign: String,
    /// Frequency in kHz (as reported by cluster)
    pub frequency_khz: f64,
    /// Spotter callsign
    pub spotter: String,
    /// Spot time
    pub time: DateTime<Utc>,
    /// Spot comment
    pub comment: String,
    /// Raw spot line from cluster
    pub raw_line: String,
}

/// DX Cluster message types
#[derive(Debug, Clone)]
pub enum ClusterMessage {
    /// DX spot
    Spot(ClusterSpot),
    /// Announcement
    Announcement { text: String, time: DateTime<Utc> },
    /// WWV/WCY propagation data
    Propagation { text: String, time: DateTime<Utc> },
    /// System message
    System { text: String },
    /// Login prompt/response
    Login { text: String },
    /// Raw unparsed line
    Raw { text: String },
}

/// DX Cluster client
pub struct DxClusterClient {
    /// Configuration
    config: ClusterConfig,
    /// Connection status
    status: Arc<Mutex<ConnectionStatus>>,
    /// Message sender for outgoing commands
    command_sender: Option<mpsc::UnboundedSender<String>>,
    /// Spot receiver
    spot_receiver: Option<mpsc::UnboundedReceiver<DxSpot>>,
    /// Recently seen spots (for duplicate filtering)
    recent_spots: Arc<Mutex<HashMap<String, DateTime<Utc>>>>,
}

impl DxClusterClient {
    /// Create new DX cluster client
    pub fn new() -> Self {
        Self {
            config: ClusterConfig {
                hostname: "dxc.nc7j.com".to_string(),
                port: 23,
                callsign: "ANONYMOUS".to_string(),
                timeout_seconds: 30,
                reconnect_delay_seconds: 30,
                auto_reconnect: true,
                filter_settings: ClusterFilter::default(),
                use_websocket: false,
                websocket_url: None,
            },
            status: Arc::new(Mutex::new(ConnectionStatus::Disconnected)),
            command_sender: None,
            spot_receiver: None,
            recent_spots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create client with custom configuration
    pub fn with_config(config: ClusterConfig) -> Self {
        Self {
            config,
            status: Arc::new(Mutex::new(ConnectionStatus::Disconnected)),
            command_sender: None,
            spot_receiver: None,
            recent_spots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Update configuration
    pub fn update_config(&mut self, config: ClusterConfig) {
        self.config = config;
    }

    /// Get current configuration
    pub fn config(&self) -> &ClusterConfig {
        &self.config
    }

    /// Get connection status
    pub async fn status(&self) -> ConnectionStatus {
        self.status.lock().await.clone()
    }

    /// Connect to DX cluster
    pub async fn connect(&mut self) -> Result<()> {
        *self.status.lock().await = ConnectionStatus::Connecting;

        info!(
            "Connecting to DX cluster: {}:{}",
            self.config.hostname, self.config.port
        );

        if self.config.use_websocket {
            if let Some(ws_url) = self.config.websocket_url.clone() {
                self.connect_websocket(&ws_url).await
            } else {
                self.connect_telnet().await
            }
        } else {
            self.connect_telnet().await
        }
    }

    /// Connect via Telnet
    async fn connect_telnet(&mut self) -> Result<()> {
        let addr = format!("{}:{}", self.config.hostname, self.config.port);

        let stream = tokio::time::timeout(
            Duration::from_secs(self.config.timeout_seconds),
            TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| DxError::ExternalService("Connection timeout".to_string()))?
        .map_err(|e| DxError::ExternalService(e.to_string()))?;

        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // Create channels for communication
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<String>();
        let (spot_tx, spot_rx) = mpsc::unbounded_channel::<DxSpot>();

        let login_cmd_tx = cmd_tx.clone();
        self.command_sender = Some(cmd_tx);
        self.spot_receiver = Some(spot_rx);

        let status = self.status.clone();
        let recent_spots = self.recent_spots.clone();
        let filter = self.config.filter_settings.clone();
        let callsign = self.config.callsign.clone();

        // Spawn writer task
        let writer_status = status.clone();
        tokio::spawn(async move {
            while let Some(command) = cmd_rx.recv().await {
                debug!("Sending cluster command: {}", command);

                if let Err(e) = writer
                    .write_all(format!("{}\r\n", command).as_bytes())
                    .await
                {
                    error!("Failed to send command to cluster: {}", e);
                    *writer_status.lock().await = ConnectionStatus::Error(e.to_string());
                    break;
                }
            }
        });

        // Spawn reader task
        tokio::spawn(async move {
            let mut line = String::new();
            let mut logged_in = false;

            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        warn!("DX cluster connection closed");
                        *status.lock().await = ConnectionStatus::Disconnected;
                        break;
                    }
                    Ok(_) => {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }

                        debug!("Cluster: {}", line);

                        // Handle login
                        if !logged_in {
                            if line.contains("call:")
                                || line.contains("callsign:")
                                || line.contains("Please enter your call")
                            {
                                info!("Login prompt detected — sending callsign");
                                if let Err(e) = login_cmd_tx.send(callsign.clone()) {
                                    error!("Failed to send callsign for login: {}", e);
                                }
                            } else if line.contains("Hello") || line.contains("Welcome") {
                                logged_in = true;
                                *status.lock().await = ConnectionStatus::Connected;
                                info!("Successfully logged into DX cluster");
                            }
                            continue;
                        }

                        // Parse cluster messages
                        if let Some(message) = Self::parse_cluster_line(line) {
                            match message {
                                ClusterMessage::Spot(cluster_spot) => {
                                    if let Ok(dx_spot) = Self::convert_cluster_spot(
                                        cluster_spot,
                                        &filter,
                                        &recent_spots,
                                    )
                                    .await
                                    {
                                        if spot_tx.send(dx_spot).is_err() {
                                            warn!("Spot receiver dropped");
                                            break;
                                        }
                                    }
                                }
                                _ => {
                                    // Handle other message types as needed
                                    debug!("Received cluster message: {:?}", message);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Error reading from cluster: {}", e);
                        *status.lock().await = ConnectionStatus::Error(e.to_string());
                        break;
                    }
                }
            }
        });

        *self.status.lock().await = ConnectionStatus::Connected;
        Ok(())
    }

    /// Connect via WebSocket
    async fn connect_websocket(&mut self, ws_url: &str) -> Result<()> {
        // Validate URL first
        let _url = Url::parse(ws_url)
            .map_err(|e| DxError::Configuration(format!("Invalid WebSocket URL: {}", e)))?;

        let (ws_stream, _response) = connect_async(ws_url)
            .await
            .map_err(|e| DxError::ExternalService(format!("WebSocket connection failed: {}", e)))?;

        let (mut write, mut read) = ws_stream.split();

        // Create channels
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<String>();
        let (spot_tx, spot_rx) = mpsc::unbounded_channel::<DxSpot>();

        self.command_sender = Some(cmd_tx);
        self.spot_receiver = Some(spot_rx);

        let status = self.status.clone();
        let recent_spots = self.recent_spots.clone();
        let filter = self.config.filter_settings.clone();

        // Spawn writer task
        let writer_status = status.clone();
        tokio::spawn(async move {
            while let Some(command) = cmd_rx.recv().await {
                debug!("Sending WebSocket command: {}", command);

                if let Err(e) = write.send(Message::Text(command)).await {
                    error!("Failed to send WebSocket message: {}", e);
                    *writer_status.lock().await = ConnectionStatus::Error(e.to_string());
                    break;
                }
            }
        });

        // Spawn reader task
        tokio::spawn(async move {
            while let Some(message) = read.next().await {
                match message {
                    Ok(Message::Text(text)) => {
                        debug!("WebSocket: {}", text);

                        if let Some(ClusterMessage::Spot(cluster_spot)) =
                            Self::parse_cluster_line(&text)
                        {
                            if let Ok(dx_spot) =
                                Self::convert_cluster_spot(cluster_spot, &filter, &recent_spots)
                                    .await
                            {
                                if spot_tx.send(dx_spot).is_err() {
                                    warn!("Spot receiver dropped");
                                    break;
                                }
                            }
                        }
                    }
                    Ok(Message::Close(_)) => {
                        info!("WebSocket connection closed");
                        *status.lock().await = ConnectionStatus::Disconnected;
                        break;
                    }
                    Err(e) => {
                        error!("WebSocket error: {}", e);
                        *status.lock().await = ConnectionStatus::Error(e.to_string());
                        break;
                    }
                    _ => {}
                }
            }
        });

        *self.status.lock().await = ConnectionStatus::Connected;
        Ok(())
    }

    /// Login to cluster
    pub async fn login(&self) -> Result<()> {
        if let Some(sender) = &self.command_sender {
            sender.send(self.config.callsign.clone()).map_err(|_| {
                DxError::ExternalService("Failed to send login command".to_string())
            })?;
        }
        Ok(())
    }

    /// Send command to cluster
    pub async fn send_command(&self, command: &str) -> Result<()> {
        if let Some(sender) = &self.command_sender {
            sender
                .send(command.to_string())
                .map_err(|_| DxError::ExternalService("Failed to send command".to_string()))?;
            Ok(())
        } else {
            Err(DxError::ExternalService(
                "Not connected to cluster".to_string(),
            ))
        }
    }

    /// Post a DX spot
    pub async fn post_spot(&self, spot: &DxSpot) -> Result<()> {
        let freq_khz = spot.frequency as f64 / 1000.0;
        let command = format!(
            "DX {:.1} {} {}",
            freq_khz,
            spot.callsign,
            spot.comment.as_deref().unwrap_or("")
        );

        self.send_command(&command).await
    }

    /// Set cluster filters
    pub async fn set_filters(&self) -> Result<()> {
        let filter = &self.config.filter_settings;

        // Set band filters
        if !filter.bands.is_empty() {
            let bands_str = filter
                .bands
                .iter()
                .map(|b| b.to_string())
                .collect::<Vec<_>>()
                .join(",");
            self.send_command(&format!("SET/FILTER BAND {}", bands_str))
                .await?;
        }

        // Set frequency filters
        if let (Some(min_freq), Some(max_freq)) = (filter.min_frequency, filter.max_frequency) {
            let min_khz = min_freq as f64 / 1000.0;
            let max_khz = max_freq as f64 / 1000.0;
            self.send_command(&format!("SET/FILTER FREQ {}-{}", min_khz, max_khz))
                .await?;
        }

        Ok(())
    }

    /// Receive spots
    pub async fn receive_spot(&mut self) -> Option<DxSpot> {
        if let Some(receiver) = &mut self.spot_receiver {
            receiver.recv().await
        } else {
            None
        }
    }

    /// Monitor spots with callback
    pub async fn monitor_spots<F>(&mut self, mut callback: F) -> Result<()>
    where
        F: FnMut(DxSpot) -> Result<()>,
    {
        info!("Starting DX cluster spot monitoring");

        while let Some(spot) = self.receive_spot().await {
            if let Err(e) = callback(spot) {
                error!("Error in spot callback: {}", e);
            }
        }

        Ok(())
    }

    /// Disconnect from cluster
    pub async fn disconnect(&mut self) {
        if let Some(sender) = &self.command_sender {
            let _ = sender.send("BYE".to_string());
        }

        self.command_sender = None;
        self.spot_receiver = None;
        *self.status.lock().await = ConnectionStatus::Disconnected;

        info!("Disconnected from DX cluster");
    }

    /// Parse cluster line into message
    fn parse_cluster_line(line: &str) -> Option<ClusterMessage> {
        let line = line.trim();

        // DX spot format: DX de W1ABC:     14074.0  JA1XYZ       FT8 from Tokyo     1234Z
        if line.starts_with("DX de ") {
            if let Some(spot) = Self::parse_dx_spot(line) {
                return Some(ClusterMessage::Spot(spot));
            }
        }

        // Announcements
        if line.starts_with("To ALL de ") || line.starts_with("WWV de ") {
            return Some(ClusterMessage::Announcement {
                text: line.to_string(),
                time: Utc::now(),
            });
        }

        // System messages
        if line.starts_with("Login: ") || line.contains("Welcome") || line.contains("Hello") {
            return Some(ClusterMessage::Login {
                text: line.to_string(),
            });
        }

        // Default to raw message
        Some(ClusterMessage::Raw {
            text: line.to_string(),
        })
    }

    /// Sanitize text taken raw from the plaintext (non-TLS) telnet cluster feed
    /// before it is stored/forwarded to the TUI, logs, or ADIF.
    ///
    /// [sec I-15] A compromised or MITM'd cluster could inject ANSI escape
    /// sequences, control characters, or oversized strings. This strips every
    /// control character below `0x20` (including the `0x1b` ESC that begins ANSI
    /// sequences and `\t`/`\r`/`\n`) and the `0x7f` DEL, keeps the normal space
    /// and all printable characters, and caps the result at 200 characters.
    fn sanitize_spot_text(s: &str) -> String {
        s.chars()
            .filter(|&c| c == ' ' || (!c.is_control() && c != '\u{7f}'))
            .take(200)
            .collect()
    }

    /// Parse DX spot line
    fn parse_dx_spot(line: &str) -> Option<ClusterSpot> {
        // Expected format: DX de W1ABC:     14074.0  JA1XYZ       FT8 from Tokyo     1234Z
        let parts: Vec<&str> = line.split_whitespace().collect();

        if parts.len() < 5 {
            return None;
        }

        // Extract spotter (remove "de" and ":")
        let spotter = parts.get(2)?.trim_end_matches(':');

        // Extract frequency
        let frequency_khz: f64 = parts.get(3)?.parse().ok()?;

        // Extract spotted callsign
        let callsign = parts.get(4)?.to_string();

        // Extract comment (everything after callsign)
        let comment_start = line.find(callsign.as_str())? + callsign.len();
        let comment = line[comment_start..].trim();

        // Extract time (last token ending with Z)
        let time_str = parts.last()?;
        let time = if time_str.ends_with('Z') {
            // Parse time format like 1234Z
            Self::parse_cluster_time(time_str).unwrap_or_else(Utc::now)
        } else {
            Utc::now()
        };

        // [sec I-15] Sanitize every field sourced from the untrusted telnet line
        // (strip control/ANSI chars, cap length) before it is stored/forwarded.
        Some(ClusterSpot {
            callsign: Self::sanitize_spot_text(&callsign.to_uppercase()),
            frequency_khz,
            spotter: Self::sanitize_spot_text(&spotter.to_uppercase()),
            time,
            comment: Self::sanitize_spot_text(comment),
            raw_line: line.to_string(),
        })
    }

    /// Parse cluster time format (HHMMZ)
    fn parse_cluster_time(time_str: &str) -> Option<DateTime<Utc>> {
        if !time_str.ends_with('Z') || time_str.len() != 5 {
            return None;
        }

        let time_digits = &time_str[0..4];
        let hour: u32 = time_digits[0..2].parse().ok()?;
        let minute: u32 = time_digits[2..4].parse().ok()?;

        if hour > 23 || minute > 59 {
            return None;
        }

        let now = Utc::now();
        let today = now.date_naive();

        let time = today.and_hms_opt(hour, minute, 0)?.and_utc();

        // If the time is in the future (next hour), assume it's from yesterday
        if time > now + chrono::Duration::hours(1) {
            Some(time - chrono::Duration::days(1))
        } else {
            Some(time)
        }
    }

    /// Drop dedup entries whose timestamp is older than `duplicate_timeout`
    /// seconds relative to `now`. Pure/synchronous so it can be unit-tested and
    /// reused; keeps `recent_spots` bounded by the dedup window rather than by
    /// the (unbounded) number of distinct spots ever seen.
    fn prune_recent_spots(
        recent: &mut HashMap<String, DateTime<Utc>>,
        now: DateTime<Utc>,
        duplicate_timeout: u64,
    ) {
        recent.retain(|_, &mut t| {
            now.signed_duration_since(t).num_seconds() < duplicate_timeout as i64
        });
    }

    /// Convert cluster spot to DxSpot
    async fn convert_cluster_spot(
        cluster_spot: ClusterSpot,
        filter: &ClusterFilter,
        recent_spots: &Arc<Mutex<HashMap<String, DateTime<Utc>>>>,
    ) -> Result<DxSpot> {
        // Check for duplicates
        let spot_key = format!(
            "{}:{}",
            cluster_spot.callsign, cluster_spot.frequency_khz as u64
        );
        {
            let now = Utc::now();
            let mut recent = recent_spots.lock().await;
            if let Some(last_time) = recent.get(&spot_key) {
                let elapsed = now.signed_duration_since(*last_time);
                if elapsed.num_seconds() < filter.duplicate_timeout as i64 {
                    return Err(DxError::Configuration(
                        "Duplicate spot filtered".to_string(),
                    ));
                }
            }
            recent.insert(spot_key, cluster_spot.time);
            // Prune entries older than the dedup window. Without this the map
            // grows unboundedly when a (compromised/MITM) cluster streams
            // unique spot lines forever; it also clears stale keys that have
            // aged past the window so they can't linger and mis-dedup.
            Self::prune_recent_spots(&mut recent, now, filter.duplicate_timeout);
        }

        // Convert frequency to Hz
        let frequency = (cluster_spot.frequency_khz * 1000.0) as u64;

        // Apply frequency filters
        if let Some(min_freq) = filter.min_frequency {
            if frequency < min_freq {
                return Err(DxError::Configuration(
                    "Frequency below minimum".to_string(),
                ));
            }
        }

        if let Some(max_freq) = filter.max_frequency {
            if frequency > max_freq {
                return Err(DxError::Configuration(
                    "Frequency above maximum".to_string(),
                ));
            }
        }

        // Try to extract mode from comment
        let mode = Self::extract_mode_from_comment(&cluster_spot.comment);

        Ok(DxSpot {
            callsign: cluster_spot.callsign,
            frequency,
            mode,
            spotter: cluster_spot.spotter,
            time: cluster_spot.time,
            comment: Some(cluster_spot.comment),
            dxcc_entity: None,     // Will be filled by DX Hunter
            grid_square: None,     // Could be extracted from comment
            distance_km: None,     // Will be calculated
            bearing_degrees: None, // Will be calculated
            rarity_score: None,    // Will be calculated
        })
    }

    /// Extract mode from spot comment
    // rationale: the JT65/JT9/MSK144 arms each return `None` with a comment
    // explaining the mode is intentionally unmapped; keeping them distinct
    // documents the recognized-but-unmapped modes rather than collapsing.
    #[allow(clippy::if_same_then_else)]
    fn extract_mode_from_comment(comment: &str) -> Option<Mode> {
        let comment_upper = comment.to_uppercase();

        if comment_upper.contains("FT8") {
            Some(Mode::FT8)
        } else if comment_upper.contains("FT4") {
            Some(Mode::FT4)
        } else if comment_upper.contains("CW") {
            Some(Mode::CW)
        } else if comment_upper.contains("SSB") {
            Some(Mode::USB)
        } else if comment_upper.contains("RTTY") {
            Some(Mode::RTTY)
        } else if comment_upper.contains("PSK31") {
            Some(Mode::PSK31)
        } else if comment_upper.contains("JT65") {
            None // JT65 not in unified Mode
        } else if comment_upper.contains("JT9") {
            None // JT9 not in unified Mode
        } else if comment_upper.contains("MSK144") {
            None // MSK144 not in unified Mode
        } else {
            None
        }
    }
}

impl Default for DxClusterClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let client = DxClusterClient::new();
        assert_eq!(client.config.hostname, "dxc.nc7j.com");
        assert_eq!(client.config.port, 23);
    }

    #[test]
    fn test_prune_recent_spots_evicts_old_keeps_fresh() {
        let now = Utc::now();
        let timeout: u64 = 300; // 5 minutes
        let mut recent: HashMap<String, DateTime<Utc>> = HashMap::new();
        // Older than the window -> should be evicted.
        recent.insert(
            "OLD:14074".to_string(),
            now - chrono::Duration::seconds(timeout as i64 + 1),
        );
        // Inside the window -> should be kept.
        recent.insert(
            "FRESH:14074".to_string(),
            now - chrono::Duration::seconds(10),
        );

        DxClusterClient::prune_recent_spots(&mut recent, now, timeout);

        assert!(
            !recent.contains_key("OLD:14074"),
            "stale entry should be pruned"
        );
        assert!(
            recent.contains_key("FRESH:14074"),
            "fresh entry should be kept"
        );
        assert_eq!(recent.len(), 1);
    }

    #[test]
    fn test_dx_spot_parsing() {
        let line = "DX de W1ABC:     14074.0  JA1XYZ       FT8 from Tokyo     1234Z";
        let spot = DxClusterClient::parse_dx_spot(line).unwrap();

        assert_eq!(spot.spotter, "W1ABC");
        assert_eq!(spot.frequency_khz, 14074.0);
        assert_eq!(spot.callsign, "JA1XYZ");
        assert!(spot.comment.contains("FT8"));
        assert!(spot.comment.contains("Tokyo"));
    }

    #[test]
    fn test_sanitize_spot_text_strips_ansi_and_control() {
        // [sec I-15] ANSI escape + NUL/control chars are removed; printable text kept.
        let dirty = "\x1b[31mhi\x00\x07 there\x7f";
        let clean = DxClusterClient::sanitize_spot_text(dirty);
        assert_eq!(clean, "[31mhi there");
        assert!(!clean.contains('\x1b'));
        assert!(!clean.contains('\x00'));
        assert!(!clean.contains('\x7f'));
    }

    #[test]
    fn test_sanitize_spot_text_truncates_to_200() {
        let long = "A".repeat(500);
        let clean = DxClusterClient::sanitize_spot_text(&long);
        assert_eq!(clean.chars().count(), 200);
    }

    #[test]
    fn test_sanitize_spot_text_passes_normal_through() {
        let normal = "FT8 from Tokyo -15 dB";
        assert_eq!(DxClusterClient::sanitize_spot_text(normal), normal);
    }

    #[test]
    fn test_dx_spot_parsing_sanitizes_comment() {
        // A MITM'd cluster injects an ANSI escape + control char into the comment.
        let line = "DX de W1ABC:     14074.0  JA1XYZ       \x1b[31mFT8\x00 Tokyo     1234Z";
        let spot = DxClusterClient::parse_dx_spot(line).unwrap();
        assert!(!spot.comment.contains('\x1b'));
        assert!(!spot.comment.contains('\x00'));
        assert!(spot.comment.contains("FT8"));
        assert!(spot.comment.contains("Tokyo"));
    }

    #[test]
    fn test_time_parsing() {
        let time = DxClusterClient::parse_cluster_time("1234Z").unwrap();
        assert_eq!(chrono::Timelike::hour(&time.time()), 12);
        assert_eq!(chrono::Timelike::minute(&time.time()), 34);

        assert!(DxClusterClient::parse_cluster_time("2560Z").is_none()); // Invalid hour
        assert!(DxClusterClient::parse_cluster_time("1234").is_none()); // Missing Z
    }

    #[test]
    fn test_mode_extraction() {
        assert_eq!(
            DxClusterClient::extract_mode_from_comment("FT8 from Tokyo"),
            Some(Mode::FT8)
        );
        assert_eq!(
            DxClusterClient::extract_mode_from_comment("CW QRP"),
            Some(Mode::CW)
        );
        assert_eq!(
            DxClusterClient::extract_mode_from_comment("SSB contest"),
            Some(Mode::USB)
        );
        assert_eq!(
            DxClusterClient::extract_mode_from_comment("Just saying hello"),
            None
        );
    }

    #[test]
    fn test_cluster_filter_default() {
        let filter = ClusterFilter::default();
        assert!(filter.bands.is_empty());
        assert!(filter.modes.is_empty());
        assert_eq!(filter.duplicate_timeout, 300);
    }

    #[test]
    fn test_message_parsing() {
        // Test DX spot
        let dx_line = "DX de W1ABC:     14074.0  JA1XYZ       FT8 from Tokyo     1234Z";
        if let Some(ClusterMessage::Spot(spot)) = DxClusterClient::parse_cluster_line(dx_line) {
            assert_eq!(spot.callsign, "JA1XYZ");
        } else {
            panic!("Failed to parse DX spot");
        }

        // Test announcement
        let ann_line = "To ALL de W1ABC: Contest starts now!";
        if let Some(ClusterMessage::Announcement { text, .. }) =
            DxClusterClient::parse_cluster_line(ann_line)
        {
            assert!(text.contains("Contest"));
        } else {
            panic!("Failed to parse announcement");
        }

        // Test login
        let login_line = "Login: W1ABC";
        if let Some(ClusterMessage::Login { text }) =
            DxClusterClient::parse_cluster_line(login_line)
        {
            assert!(text.contains("Login"));
        } else {
            panic!("Failed to parse login");
        }
    }
}
