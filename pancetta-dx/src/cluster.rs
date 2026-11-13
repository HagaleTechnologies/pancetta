//! DX Cluster Client
//!
//! This module provides connectivity to DX cluster networks for real-time
//! DX spot monitoring and posting.

use crate::{Band, DxError, DxErrorInner, DxSpot, Mode, Result};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, Mutex};
use tokio::time::Duration;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
};
use tracing::{debug, error, info, warn};
use url::Url;

/// Longest telnet line kept, line ending included. Spot lines are ~80 bytes.
const MAX_CLUSTER_LINE_BYTES: usize = 4096;
/// Spots queued for [`DxClusterClient::receive_spot`] before new ones are dropped.
const SPOT_QUEUE_CAPACITY: usize = 1024;
/// Outgoing commands queued for the writer task.
const COMMAND_QUEUE_CAPACITY: usize = 64;
/// Longest wait between two checks of the shutdown flag in
/// [`DxClusterClient::run_until_shutdown`], both while waiting for a spot and
/// while sleeping before a reconnect.
const SHUTDOWN_POLL: Duration = Duration::from_secs(1);
/// A session at least this long resets the reconnect delay to its base.
const STABLE_SESSION: Duration = Duration::from_secs(60);
/// The reconnect delay never grows past this many times its base.
const RECONNECT_CAP_FACTOR: u32 = 10;

/// Result of [`read_capped_line`].
#[derive(Debug, PartialEq, Eq)]
enum CappedLine {
    Eof,
    Line,
    TooLong,
}

/// Read one `\n`-terminated line into `buf`, holding at most `max` bytes
/// (line ending included). A longer line is consumed through its `\n` and
/// discarded, leaving `buf` empty, so its length never grows memory.
async fn read_capped_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<CappedLine> {
    buf.clear();
    let n = (&mut *reader)
        .take(max as u64)
        .read_until(b'\n', buf)
        .await?;
    if n == 0 {
        return Ok(CappedLine::Eof);
    }
    if buf.last() == Some(&b'\n') || n < max {
        return Ok(CappedLine::Line);
    }
    loop {
        buf.clear();
        let n = (&mut *reader)
            .take(max as u64)
            .read_until(b'\n', buf)
            .await?;
        if n == 0 || buf.last() == Some(&b'\n') {
            break;
        }
    }
    buf.clear();
    Ok(CappedLine::TooLong)
}

/// Capped exponential delay between DX-cluster reconnect attempts (PAN-288).
///
/// The first retry waits `base`, each later one doubles the wait up to
/// `base * RECONNECT_CAP_FACTOR`, and only a session that lasted
/// [`STABLE_SESSION`] brings it back to `base`, so a server that accepts and
/// closes at once is not retried at the minimum delay forever. Pure state:
/// it never sleeps.
#[derive(Debug, Clone)]
struct ReconnectBackoff {
    base: Duration,
    max: Duration,
    next: Duration,
}

impl ReconnectBackoff {
    fn with_base(base: Duration) -> Self {
        Self {
            base,
            max: base.saturating_mul(RECONNECT_CAP_FACTOR),
            next: base,
        }
    }

    /// Base delay from `reconnect_delay_seconds`, at least one second so a
    /// configured `0` cannot spin.
    fn from_config(cfg: &ClusterConfig) -> Self {
        Self::with_base(Duration::from_secs(cfg.reconnect_delay_seconds.max(1)))
    }

    /// The delay before the next attempt; the one after it is doubled, capped.
    fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(self.max);
        delay
    }

    /// Record how long the session that just ended lasted.
    fn session_ended(&mut self, lasted: Duration) {
        if lasted >= STABLE_SESSION {
            self.next = self.base;
        }
    }
}

/// Sleep `delay` in slices of at most [`SHUTDOWN_POLL`]. Returns `false` as
/// soon as `shutdown` is set, `true` once the whole delay has passed.
async fn sleep_unless_shutdown(delay: Duration, shutdown: &AtomicBool) -> bool {
    let mut remaining = delay;
    while !remaining.is_zero() {
        if shutdown.load(Ordering::Acquire) {
            return false;
        }
        let slice = remaining.min(SHUTDOWN_POLL);
        tokio::time::sleep(slice).await;
        remaining = remaining.saturating_sub(slice);
    }
    !shutdown.load(Ordering::Acquire)
}

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

impl ClusterFilter {
    /// Why a spot at `frequency_hz` with the derived `band` and parsed `mode`
    /// is excluded, or `None` when every configured restriction admits it
    /// (PAN-345). Frequency bounds are inclusive. An empty band/mode list
    /// admits any value, unknown included; a nonempty list excludes values it
    /// does not name, unknown included. Restrictions combine with AND.
    fn rejection(
        &self,
        frequency_hz: u64,
        band: Option<Band>,
        mode: Option<Mode>,
    ) -> Option<&'static str> {
        if self.min_frequency.is_some_and(|min| frequency_hz < min) {
            return Some("Frequency below minimum");
        }
        if self.max_frequency.is_some_and(|max| frequency_hz > max) {
            return Some("Frequency above maximum");
        }
        if !self.bands.is_empty() && !band.is_some_and(|b| self.bands.contains(&b)) {
            return Some("Band not in filter");
        }
        if !self.modes.is_empty() && !mode.is_some_and(|m| self.modes.contains(&m)) {
            return Some("Mode not in filter");
        }
        None
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
    command_sender: Option<mpsc::Sender<String>>,
    /// Spot receiver
    spot_receiver: Option<mpsc::Receiver<DxSpot>>,
    /// Recently seen spots (for duplicate filtering)
    recent_spots: Arc<Mutex<HashMap<String, DateTime<Utc>>>>,
    /// Spots dropped because the spot queue was full
    dropped_spots: Arc<AtomicU64>,
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
            dropped_spots: Arc::new(AtomicU64::new(0)),
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
            dropped_spots: Arc::new(AtomicU64::new(0)),
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

    /// Spots dropped because the spot queue was full, since the client was created
    pub fn dropped_spot_count(&self) -> u64 {
        self.dropped_spots.load(Ordering::Relaxed)
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
        .map_err(|_| {
            DxError::from(DxErrorInner::ExternalService(
                "Connection timeout".to_string(),
            ))
        })?
        .map_err(|e| DxError::from(DxErrorInner::ExternalService(e.to_string())))?;

        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // Create channels for communication
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<String>(COMMAND_QUEUE_CAPACITY);
        let (spot_tx, spot_rx) = mpsc::channel::<DxSpot>(SPOT_QUEUE_CAPACITY);

        let login_cmd_tx = cmd_tx.clone();
        self.command_sender = Some(cmd_tx);
        self.spot_receiver = Some(spot_rx);

        let status = self.status.clone();
        let recent_spots = self.recent_spots.clone();
        let dropped_spots = self.dropped_spots.clone();
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
            let mut buf = Vec::with_capacity(256);
            let mut logged_in = false;
            let mut warned_long_line = false;

            loop {
                let read = match read_capped_line(&mut reader, &mut buf, MAX_CLUSTER_LINE_BYTES)
                    .await
                {
                    Ok(CappedLine::Eof) => Ok(None),
                    Ok(CappedLine::TooLong) => {
                        if !warned_long_line {
                            warned_long_line = true;
                            warn!(
                                "DX cluster sent a line longer than {MAX_CLUSTER_LINE_BYTES} bytes; discarding lines over that length"
                            );
                        } else {
                            debug!(
                                "Discarded a DX cluster line over {MAX_CLUSTER_LINE_BYTES} bytes"
                            );
                        }
                        continue;
                    }
                    // Same outcome as `read_line` for an in-cap line that
                    // is not UTF-8: the session ends with this error.
                    Ok(CappedLine::Line) => std::str::from_utf8(&buf).map(Some).map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "stream did not contain valid UTF-8",
                        )
                    }),
                    Err(e) => Err(e),
                };
                match read {
                    Ok(None) => {
                        warn!("DX cluster connection closed");
                        *status.lock().await = ConnectionStatus::Disconnected;
                        break;
                    }
                    Ok(Some(text)) => {
                        let line = text.trim();
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
                                // A repeated prompt needs no queued duplicates.
                                match login_cmd_tx.try_send(callsign.clone()) {
                                    Ok(()) => {}
                                    Err(TrySendError::Full(_)) => {
                                        debug!("Command queue full; dropping login reply");
                                    }
                                    Err(e) => {
                                        error!("Failed to send callsign for login: {}", e);
                                    }
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
                                        if !Self::queue_spot(&spot_tx, &dropped_spots, dx_spot) {
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
        let _url = Url::parse(ws_url).map_err(|e| {
            DxError::from(DxErrorInner::Configuration(format!(
                "Invalid WebSocket URL: {}",
                e
            )))
        })?;

        // One cluster line per message, so the line limit is the message limit.
        let ws_config = WebSocketConfig::default()
            .max_message_size(Some(MAX_CLUSTER_LINE_BYTES))
            .max_frame_size(Some(MAX_CLUSTER_LINE_BYTES));
        let (ws_stream, _response) = connect_async_with_config(ws_url, Some(ws_config), false)
            .await
            .map_err(|e| {
                DxError::from(DxErrorInner::ExternalService(format!(
                    "WebSocket connection failed: {}",
                    e
                )))
            })?;

        let (mut write, mut read) = ws_stream.split();

        // Create channels
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<String>(COMMAND_QUEUE_CAPACITY);
        let (spot_tx, spot_rx) = mpsc::channel::<DxSpot>(SPOT_QUEUE_CAPACITY);

        self.command_sender = Some(cmd_tx);
        self.spot_receiver = Some(spot_rx);

        let status = self.status.clone();
        let recent_spots = self.recent_spots.clone();
        let dropped_spots = self.dropped_spots.clone();
        let filter = self.config.filter_settings.clone();

        // Spawn writer task
        let writer_status = status.clone();
        tokio::spawn(async move {
            while let Some(command) = cmd_rx.recv().await {
                debug!("Sending WebSocket command: {}", command);

                if let Err(e) = write.send(Message::Text(command.into())).await {
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
                                if !Self::queue_spot(&spot_tx, &dropped_spots, dx_spot) {
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
            sender.try_send(self.config.callsign.clone()).map_err(|_| {
                DxError::from(DxErrorInner::ExternalService(
                    "Failed to send login command".to_string(),
                ))
            })?;
        }
        Ok(())
    }

    /// Send command to cluster
    pub async fn send_command(&self, command: &str) -> Result<()> {
        if let Some(sender) = &self.command_sender {
            sender.try_send(command.to_string()).map_err(|_| {
                DxError::from(DxErrorInner::ExternalService(
                    "Failed to send command".to_string(),
                ))
            })?;
            Ok(())
        } else {
            Err(DxError::from(DxErrorInner::ExternalService(
                "Not connected to cluster".to_string(),
            )))
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

    /// Connect, forward every spot to `on_spot`, and reconnect with capped
    /// backoff whenever the session ends, until `shutdown` is set (PAN-288).
    ///
    /// A session ends when [`Self::receive_spot`] returns `None`, which covers
    /// every way the reader task stops (EOF, I/O error, invalid UTF-8, panic).
    /// A failed connect is retried on the same schedule. The wait before a
    /// retry starts at `reconnect_delay_seconds` (at least 1 s), doubles after
    /// each retry up to ten times that, and returns to the base only after a
    /// session that lasted a minute. With `auto_reconnect` off, this returns
    /// after the first session ends or the first connect fails. `shutdown` is
    /// checked at least once a second.
    pub async fn run_until_shutdown<F, Fut>(&mut self, shutdown: &AtomicBool, on_spot: F)
    where
        F: FnMut(DxSpot) -> Fut + Send,
        Fut: Future<Output = ()> + Send,
    {
        let backoff = ReconnectBackoff::from_config(&self.config);
        self.run_with_backoff(backoff, shutdown, on_spot).await;
    }

    async fn run_with_backoff<F, Fut>(
        &mut self,
        mut backoff: ReconnectBackoff,
        shutdown: &AtomicBool,
        mut on_spot: F,
    ) where
        F: FnMut(DxSpot) -> Fut + Send,
        Fut: Future<Output = ()> + Send,
    {
        while !shutdown.load(Ordering::Acquire) {
            let connect_error = match self.connect().await {
                Ok(()) => {
                    info!("Connected to DX cluster");
                    if let Err(e) = self.login().await {
                        warn!("DX cluster login failed: {}. Continuing without.", e);
                    }
                    let started = Instant::now();
                    loop {
                        if shutdown.load(Ordering::Acquire) {
                            return;
                        }
                        match tokio::time::timeout(SHUTDOWN_POLL, self.receive_spot()).await {
                            Ok(Some(spot)) => on_spot(spot).await,
                            Ok(None) => break,
                            Err(_) => {}
                        }
                    }
                    backoff.session_ended(started.elapsed());
                    None
                }
                Err(e) => Some(e),
            };

            if !self.config.auto_reconnect {
                match connect_error {
                    Some(e) => warn!(
                        "Failed to connect to DX cluster {}:{}: {}; auto-reconnect is off",
                        self.config.hostname, self.config.port, e
                    ),
                    None => warn!("DX cluster connection lost; auto-reconnect is off"),
                }
                return;
            }

            let delay = backoff.next_delay();
            match connect_error {
                Some(e) => warn!(
                    "Failed to connect to DX cluster {}:{}: {}; retrying in {:?}",
                    self.config.hostname, self.config.port, e, delay
                ),
                None => warn!("DX cluster connection lost; reconnecting in {:?}", delay),
            }
            if !sleep_unless_shutdown(delay, shutdown).await {
                return;
            }
        }
    }

    /// Disconnect from cluster
    pub async fn disconnect(&mut self) {
        if let Some(sender) = &self.command_sender {
            let _ = sender.try_send("BYE".to_string());
        }

        self.command_sender = None;
        self.spot_receiver = None;
        *self.status.lock().await = ConnectionStatus::Disconnected;

        info!("Disconnected from DX cluster");
    }

    /// Queue a spot for [`Self::receive_spot`] without waiting. When the queue
    /// is full the new spot is dropped and counted, so the reader keeps
    /// draining the connection. Returns `false` once the receiver is gone.
    fn queue_spot(spot_tx: &mpsc::Sender<DxSpot>, dropped_spots: &AtomicU64, spot: DxSpot) -> bool {
        match spot_tx.try_send(spot) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                let n = dropped_spots.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 || n.is_multiple_of(1000) {
                    warn!(
                        "DX cluster spot queue full ({SPOT_QUEUE_CAPACITY}); dropped {n} new spot(s) so far"
                    );
                }
                true
            }
            Err(TrySendError::Closed(_)) => {
                warn!("Spot receiver dropped");
                false
            }
        }
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
        // A `…Z` token holding non-ASCII text is not a cluster time; the line
        // is skipped rather than given a made-up time (PAN-288).
        if time_str.ends_with('Z') && !time_str.is_ascii() {
            return None;
        }
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

    /// Parse cluster time format (HHMMZ). Only four ASCII digits before the
    /// `Z` are accepted, checked before any slicing, so a multibyte character
    /// cannot land a slice mid-character (PAN-288).
    fn parse_cluster_time(time_str: &str) -> Option<DateTime<Utc>> {
        let digits = time_str.strip_suffix('Z')?.as_bytes();
        if digits.len() != 4 || !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }

        let pair = |hi: u8, lo: u8| u32::from(hi - b'0') * 10 + u32::from(lo - b'0');
        let hour = pair(digits[0], digits[1]);
        let minute = pair(digits[2], digits[3]);

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

    /// Convert cluster spot to DxSpot (PAN-345).
    ///
    /// Applies the configured frequency, band and mode allowlists BEFORE the
    /// duplicate check, so a rejected spot never records a dedup key that
    /// would suppress a later accepted spot for the same callsign/frequency.
    /// Band comes from the shared [`Band::from_frequency`] table; an empty
    /// allowlist admits everything (unknown included), a nonempty one admits
    /// only listed values (unknown excluded). A zero `duplicate_timeout`
    /// disables duplicate suppression.
    async fn convert_cluster_spot(
        cluster_spot: ClusterSpot,
        filter: &ClusterFilter,
        recent_spots: &Arc<Mutex<HashMap<String, DateTime<Utc>>>>,
    ) -> Result<DxSpot> {
        // A malformed frequency must not masquerade as valid metadata.
        if !cluster_spot.frequency_khz.is_finite() || cluster_spot.frequency_khz <= 0.0 {
            return Err(DxError::from(DxErrorInner::Configuration(
                "Invalid spot frequency".to_string(),
            )));
        }

        // Convert frequency to Hz, rounded: `2048.14 * 1000.0` is
        // 2048139.9999999998, which truncation would carry as 2048139 Hz and
        // judge against inclusive limits and band edges one hertz low.
        let frequency = (cluster_spot.frequency_khz * 1000.0).round() as u64;
        let band = Band::from_frequency(frequency);
        // Try to extract mode from comment
        let mode = Self::extract_mode_from_comment(&cluster_spot.comment);

        if let Some(reason) = filter.rejection(frequency, band, mode) {
            return Err(DxError::from(DxErrorInner::Configuration(
                reason.to_string(),
            )));
        }

        // Check for duplicates (accepted spots only)
        if filter.duplicate_timeout > 0 {
            let spot_key = format!(
                "{}:{}",
                cluster_spot.callsign, cluster_spot.frequency_khz as u64
            );
            let now = Utc::now();
            let mut recent = recent_spots.lock().await;
            if let Some(last_time) = recent.get(&spot_key) {
                let elapsed = now.signed_duration_since(*last_time);
                if elapsed.num_seconds() < filter.duplicate_timeout as i64 {
                    return Err(DxError::from(DxErrorInner::Configuration(
                        "Duplicate spot filtered".to_string(),
                    )));
                }
            }
            recent.insert(spot_key, cluster_spot.time);
            // Prune entries older than the dedup window. Without this the map
            // grows unboundedly when a (compromised/MITM) cluster streams
            // unique spot lines forever; it also clears stale keys that have
            // aged past the window so they can't linger and mis-dedup.
            Self::prune_recent_spots(&mut recent, now, filter.duplicate_timeout);
        }

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

    /// A real telnet spot line timed "now", so an accepted spot's dedup key
    /// stays inside the window instead of being pruned as an old spot.
    fn pan345_line(freq_khz: &str, call: &str, comment: &str) -> String {
        let time = Utc::now().format("%H%MZ");
        format!("DX de W1ABC:     {freq_khz}  {call}       {comment}     {time}")
    }

    /// Parse `line` with the real parser, then run the real converter.
    async fn pan345_convert(
        line: &str,
        filter: &ClusterFilter,
        recent: &Arc<Mutex<HashMap<String, DateTime<Utc>>>>,
    ) -> Result<DxSpot> {
        let spot = DxClusterClient::parse_dx_spot(line).expect("a parseable spot line");
        DxClusterClient::convert_cluster_spot(spot, filter, recent).await
    }

    fn pan345_recent() -> Arc<Mutex<HashMap<String, DateTime<Utc>>>> {
        Arc::new(Mutex::new(HashMap::new()))
    }

    /// AC3 (PAN-345), table-driven through the real parser and converter:
    /// a band-only `["20m"]` allowlist admits a 14074.0 kHz spot and
    /// excludes a 7074.0 kHz spot; independent band and mode allowlists
    /// combine with AND; an empty list admits unknown values while a
    /// nonempty list excludes unknown values for that dimension.
    #[tokio::test]
    async fn cluster_filter_allowlists() {
        /// (bands, modes, freq kHz, comment, admitted?)
        type Case<'a> = (&'a [Band], &'a [Mode], &'a str, &'a str, bool);
        let cases: &[Case] = &[
            // Empty lists admit everything, unknown included.
            (&[], &[], "14074.0", "FT8 -10 dB", true),
            (&[], &[], "14074.0", "CW 25 WPM", true),
            (&[], &[], "14074.0", "no mode here", true),
            (&[], &[], "1000.0", "FT8 out of every band", true),
            // Mode allowlist.
            (&[], &[Mode::FT8], "14074.0", "FT8 -10 dB", true),
            (&[], &[Mode::FT8], "14074.0", "CW 25 WPM", false),
            (&[], &[Mode::FT8], "14074.0", "no mode here", false),
            (&[], &[Mode::FT8, Mode::CW], "14025.0", "CW 25 WPM", true),
            // Band allowlist.
            (&[Band::Band20m], &[], "14074.0", "FT8 -10 dB", true),
            (&[Band::Band20m], &[], "7074.0", "FT8 -10 dB", false),
            (
                &[Band::Band20m],
                &[],
                "1000.0",
                "FT8 out of every band",
                false,
            ),
            (&[Band::Band20m, Band::Band40m], &[], "7074.0", "FT8", true),
            // Both combine with AND.
            (
                &[Band::Band20m],
                &[Mode::FT8],
                "14074.0",
                "FT8 -10 dB",
                true,
            ),
            (
                &[Band::Band20m],
                &[Mode::FT8],
                "14074.0",
                "CW 25 WPM",
                false,
            ),
            (
                &[Band::Band20m],
                &[Mode::FT8],
                "7074.0",
                "FT8 -10 dB",
                false,
            ),
            (&[Band::Band20m], &[Mode::FT8], "7025.0", "CW 25 WPM", false),
        ];
        for (i, (bands, modes, freq, comment, admitted)) in cases.iter().enumerate() {
            let filter = ClusterFilter {
                bands: bands.to_vec(),
                modes: modes.to_vec(),
                ..Default::default()
            };
            let call = format!("JA{i}XYZ");
            let got = pan345_convert(
                &pan345_line(freq, &call, comment),
                &filter,
                &pan345_recent(),
            )
            .await;
            assert_eq!(
                got.is_ok(),
                *admitted,
                "case {i}: bands {bands:?} modes {modes:?} {freq} kHz {comment:?} -> {got:?}"
            );
        }

        // An admitted CW spot stays CW and an unknown mode stays None.
        let recent = pan345_recent();
        let cw = pan345_convert(
            &pan345_line("14025.0", "K1CW", "CW 25 WPM"),
            &ClusterFilter::default(),
            &recent,
        )
        .await
        .unwrap();
        assert_eq!(cw.mode, Some(Mode::CW));
        let unknown = pan345_convert(
            &pan345_line("14025.0", "K1UNK", "loud"),
            &ClusterFilter::default(),
            &recent,
        )
        .await
        .unwrap();
        assert_eq!(unknown.mode, None);
    }

    #[tokio::test]
    async fn cluster_filter_band_edges() {
        let only_20m = ClusterFilter {
            bands: vec![Band::Band20m],
            duplicate_timeout: 0,
            ..Default::default()
        };
        // The shared table's 20m edges are inclusive: 14000.0 and 14350.0 kHz.
        for (freq, admitted) in [
            ("13999.9", false),
            ("14000.0", true),
            ("14074.0", true),
            ("14350.0", true),
            ("14350.1", false),
        ] {
            let got = pan345_convert(
                &pan345_line(freq, "JA1XYZ", "FT8"),
                &only_20m,
                &pan345_recent(),
            )
            .await;
            assert_eq!(got.is_ok(), admitted, "{freq} kHz -> {got:?}");
            if let Ok(spot) = got {
                assert_eq!(Band::from_frequency(spot.frequency), Some(Band::Band20m));
            }
        }
        // 14074.0 kHz carries 14,074,000 Hz, which the shared table calls 20m;
        // a frequency outside every band derives no band.
        let spot = pan345_convert(
            &pan345_line("14074.0", "JA1XYZ", "FT8"),
            &ClusterFilter::default(),
            &pan345_recent(),
        )
        .await
        .unwrap();
        assert_eq!(spot.frequency, 14_074_000);
        assert_eq!(Band::from_frequency(spot.frequency), Some(Band::Band20m));
        assert_eq!(Band::from_frequency(1_000_000), None);

        // Malformed frequencies are refused, never converted.
        for freq in [0.0, -14074.0, f64::NAN, f64::INFINITY] {
            let spot = ClusterSpot {
                callsign: "JA1XYZ".to_string(),
                frequency_khz: freq,
                spotter: "W1ABC".to_string(),
                time: Utc::now(),
                comment: "FT8".to_string(),
                raw_line: String::new(),
            };
            let got = DxClusterClient::convert_cluster_spot(
                spot,
                &ClusterFilter::default(),
                &pan345_recent(),
            )
            .await;
            assert!(got.is_err(), "{freq} kHz must be refused: {got:?}");
        }
    }

    #[tokio::test]
    async fn cluster_filter_frequency_limits() {
        let limits = ClusterFilter {
            min_frequency: Some(14_000_000),
            max_frequency: Some(14_350_000),
            duplicate_timeout: 0,
            ..Default::default()
        };
        for (freq, admitted) in [
            ("13999.0", false),
            ("14000.0", true),
            ("14350.0", true),
            ("14351.0", false),
        ] {
            let got = pan345_convert(
                &pan345_line(freq, "JA1XYZ", "FT8"),
                &limits,
                &pan345_recent(),
            )
            .await;
            assert_eq!(got.is_ok(), admitted, "{freq} kHz -> {got:?}");
        }

        // A two-decimal kHz value lands on its exact hertz, so an inclusive
        // limit at that hertz admits it.
        let exact = ClusterFilter {
            min_frequency: Some(2_048_140),
            max_frequency: Some(2_048_140),
            duplicate_timeout: 0,
            ..Default::default()
        };
        let spot = pan345_convert(
            &pan345_line("2048.14", "JA1XYZ", "FT8"),
            &exact,
            &pan345_recent(),
        )
        .await
        .expect("2048.14 kHz is exactly 2,048,140 Hz");
        assert_eq!(spot.frequency, 2_048_140);
    }

    #[tokio::test]
    async fn rejected_spot_does_not_poison_dedup() {
        let ft8_only = ClusterFilter {
            modes: vec![Mode::FT8],
            duplicate_timeout: 600,
            ..Default::default()
        };
        let recent = pan345_recent();
        let cw = pan345_convert(
            &pan345_line("14074.0", "JA1XYZ", "CW 25 WPM"),
            &ft8_only,
            &recent,
        )
        .await;
        assert!(cw.is_err(), "CW spot must be filtered: {cw:?}");
        assert!(
            recent.lock().await.is_empty(),
            "a rejected spot records no dedup key"
        );

        let ft8 = pan345_convert(
            &pan345_line("14074.0", "JA1XYZ", "FT8 -10 dB"),
            &ft8_only,
            &recent,
        )
        .await
        .expect("the following FT8 spot is accepted");
        assert_eq!(ft8.mode, Some(Mode::FT8));
    }

    #[tokio::test]
    async fn accepted_spots_obey_duplicate_window() {
        let line = pan345_line("14074.0", "JA1XYZ", "FT8 -10 dB");

        let windowed = ClusterFilter {
            duplicate_timeout: 600,
            ..Default::default()
        };
        let recent = pan345_recent();
        assert!(pan345_convert(&line, &windowed, &recent).await.is_ok());
        assert!(
            pan345_convert(&line, &windowed, &recent).await.is_err(),
            "a repeat inside the window is suppressed"
        );

        let disabled = ClusterFilter {
            duplicate_timeout: 0,
            ..Default::default()
        };
        let recent = pan345_recent();
        for _ in 0..3 {
            assert!(
                pan345_convert(&line, &disabled, &recent).await.is_ok(),
                "a zero window disables suppression"
            );
        }
        assert!(recent.lock().await.is_empty());
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

    async fn capped(reader: &mut BufReader<&[u8]>, buf: &mut Vec<u8>) -> CappedLine {
        read_capped_line(reader, buf, 16).await.unwrap()
    }

    #[tokio::test]
    async fn capped_line_keeps_lines_that_fit_including_crlf() {
        let bytes = b"short\r\nexactly16bytes\r\n";
        let mut reader = BufReader::new(&bytes[..]);
        let mut buf = Vec::new();
        assert_eq!(capped(&mut reader, &mut buf).await, CappedLine::Line);
        assert_eq!(buf, b"short\r\n");
        assert_eq!(capped(&mut reader, &mut buf).await, CappedLine::Line);
        assert_eq!(buf, b"exactly16bytes\r\n");
        assert_eq!(capped(&mut reader, &mut buf).await, CappedLine::Eof);
    }

    #[tokio::test]
    async fn capped_line_discards_an_over_length_line_and_resyncs() {
        let bytes = b"seventeen_bytes\r\nafter\r\n";
        let mut reader = BufReader::new(&bytes[..]);
        let mut buf = Vec::new();
        assert_eq!(capped(&mut reader, &mut buf).await, CappedLine::TooLong);
        assert!(buf.is_empty());
        assert_eq!(capped(&mut reader, &mut buf).await, CappedLine::Line);
        assert_eq!(buf, b"after\r\n");
    }

    #[tokio::test]
    async fn capped_line_never_grows_past_max() {
        let mut bytes = vec![b'x'; 1024 * 1024];
        bytes.extend_from_slice(b"\r\nnext\r\n");
        let mut reader = BufReader::new(&bytes[..]);
        let mut buf = Vec::new();
        assert_eq!(capped(&mut reader, &mut buf).await, CappedLine::TooLong);
        assert!(buf.capacity() <= 64, "capacity {}", buf.capacity());
        assert_eq!(capped(&mut reader, &mut buf).await, CappedLine::Line);
        assert_eq!(buf, b"next\r\n");
        assert!(buf.capacity() <= 64, "capacity {}", buf.capacity());
    }

    #[tokio::test]
    async fn capped_line_returns_a_final_line_without_newline() {
        let bytes = b"tail";
        let mut reader = BufReader::new(&bytes[..]);
        let mut buf = Vec::new();
        assert_eq!(capped(&mut reader, &mut buf).await, CappedLine::Line);
        assert_eq!(buf, b"tail");
        assert_eq!(capped(&mut reader, &mut buf).await, CappedLine::Eof);
    }

    /// Client config pointed at a local mock cluster.
    fn local_config(port: u16) -> ClusterConfig {
        ClusterConfig {
            hostname: "127.0.0.1".to_string(),
            port,
            callsign: "N0CALL".to_string(),
            timeout_seconds: 5,
            reconnect_delay_seconds: 1,
            auto_reconnect: false,
            filter_settings: ClusterFilter::default(),
            use_websocket: false,
            websocket_url: None,
        }
    }

    /// Local plaintext "cluster" on a std thread: accepts one connection,
    /// writes `Welcome\r\n` so the reader logs in, runs `script`, then holds
    /// the socket open until the returned sender is dropped (test end).
    fn serve_cluster<F>(script: F) -> (u16, std::sync::mpsc::Sender<()>)
    where
        F: FnOnce(&mut std::net::TcpStream) + Send + 'static,
    {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = stream.write_all(b"Welcome\r\n");
            script(&mut stream);
            let _ = hold_rx.recv();
        });
        (port, hold_tx)
    }

    fn spot_line(call: &str, freq_khz: u32) -> String {
        format!("DX de W1AW:     {freq_khz}.0  {call}        FT8 test     1234Z\r\n")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn telnet_reader_skips_an_over_length_line_and_keeps_reading() {
        let (port, _hold) = serve_cluster(|stream| {
            use std::io::Write;
            let prefix = b"DX de W1AW:     14074.0  GIANT        FT8 ";
            if stream.write_all(prefix).is_err() {
                return;
            }
            let chunk = vec![b'x'; 64 * 1024];
            let mut written = prefix.len();
            while written < 8 * 1024 * 1024 {
                if stream.write_all(&chunk).is_err() {
                    return;
                }
                written += chunk.len();
            }
            let _ = stream.write_all(b" 1234Z\r\n");
            let _ = stream.write_all(spot_line("K1ABC", 14074).as_bytes());
        });

        let mut client = DxClusterClient::with_config(local_config(port));
        client.connect().await.unwrap();

        let first = tokio::time::timeout(Duration::from_secs(5), client.receive_spot())
            .await
            .expect("a spot within 5 s")
            .expect("spot channel open");
        assert_eq!(first.callsign, "K1ABC");

        let next = tokio::time::timeout(Duration::from_millis(300), client.receive_spot()).await;
        if let Ok(Some(spot)) = next {
            assert_ne!(spot.callsign, "GIANT");
        }
        assert_eq!(client.status().await, ConnectionStatus::Connected);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn telnet_spot_queue_is_bounded_and_drops_new_spots() {
        let total = SPOT_QUEUE_CAPACITY + 200;
        let (port, _hold) = serve_cluster(move |stream| {
            use std::io::Write;
            let mut lines = String::new();
            for i in 0..total {
                lines.push_str(&spot_line(&format!("K{i:04}X"), 14_000 + i as u32));
            }
            let _ = stream.write_all(lines.as_bytes());
        });

        let mut client = DxClusterClient::with_config(local_config(port));
        client.connect().await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), async {
            while client.dropped_spot_count() < 200 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("200 spots dropped within 5 s");
        assert_eq!(client.dropped_spot_count(), 200);

        let mut received = Vec::new();
        while let Ok(Some(spot)) =
            tokio::time::timeout(Duration::from_millis(300), client.receive_spot()).await
        {
            received.push(spot.callsign);
        }
        assert_eq!(received.len(), SPOT_QUEUE_CAPACITY);
        assert_eq!(received[0], "K0000X");
        assert_eq!(client.dropped_spot_count(), 200);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_command_fails_fast_when_the_command_queue_is_full() {
        // The mock never reads, so the writer task stalls once the socket
        // buffers fill.
        let (port, _hold) = serve_cluster(|_| {});

        let mut client = DxClusterClient::with_config(local_config(port));
        client.connect().await.unwrap();

        let command = "x".repeat(64 * 1024);
        let outcome = tokio::time::timeout(Duration::from_secs(10), async {
            for _ in 0..2_000 {
                if let Err(e) = client.send_command(&command).await {
                    return Some(e);
                }
                tokio::task::yield_now().await;
            }
            None
        })
        .await
        .expect("send_command loop finished within 10 s");

        let err = outcome.expect("a send_command call failed once the queue was full");
        assert!(
            err.to_string().contains("Failed to send command"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn websocket_branch_refuses_messages_over_the_line_limit() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let line = spot_line("K1ABC", 14074);
            let _ = ws.send(Message::Text(line.trim_end().into())).await;
            // Wait until the test has seen the spot, so `connect()` has
            // already stored `Connected` before the large message arrives.
            let _ = go_rx.await;
            let _ = ws.send(Message::Text("x".repeat(1024 * 1024).into())).await;
            std::future::pending::<()>().await;
        });

        let mut config = local_config(addr.port());
        config.use_websocket = true;
        config.websocket_url = Some(format!("ws://{addr}"));
        let mut client = DxClusterClient::with_config(config);
        client.connect().await.unwrap();

        let spot = tokio::time::timeout(Duration::from_secs(5), client.receive_spot())
            .await
            .expect("a spot within 5 s")
            .expect("spot channel open");
        assert_eq!(spot.callsign, "K1ABC");
        let _ = go_tx.send(());

        let status = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let ConnectionStatus::Error(e) = client.status().await {
                    return e;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("status became Error within 5 s");
        assert!(
            status.contains("Message too long"),
            "unexpected error: {status}"
        );
    }

    // ---- PAN-288: non-ASCII time tokens ----

    #[test]
    fn time_token_with_multibyte_chars_returns_none() {
        use chrono::Timelike;
        // Each of the first three is 5 bytes ending in Z; before the fix they
        // panicked slicing mid-character.
        for token in ["€1Z", "1é1Z", "😀Z", "ééZ", "+123Z"] {
            assert!(
                DxClusterClient::parse_cluster_time(token).is_none(),
                "{token:?} should not parse"
            );
        }
        for (token, hour, minute) in [("1234Z", 12, 34), ("0000Z", 0, 0), ("2359Z", 23, 59)] {
            let time = DxClusterClient::parse_cluster_time(token)
                .unwrap_or_else(|| panic!("{token:?} should parse"));
            assert_eq!((time.hour(), time.minute()), (hour, minute), "{token:?}");
        }
        for token in ["2560Z", "1260Z", "1234"] {
            assert!(
                DxClusterClient::parse_cluster_time(token).is_none(),
                "{token:?} should not parse"
            );
        }
    }

    #[test]
    fn dx_spot_with_non_ascii_time_token_is_not_a_spot() {
        let line = "DX de W1AW:     14074.0  K1ABC        FT8 test     €1Z";
        assert!(DxClusterClient::parse_dx_spot(line).is_none());
        assert!(!matches!(
            DxClusterClient::parse_cluster_line(line),
            Some(ClusterMessage::Spot(_))
        ));

        // An ASCII-malformed time keeps the lenient "now" fallback.
        let lenient = "DX de W1AW:     14074.0  K1ABC        FT8 test     2560Z";
        let spot = DxClusterClient::parse_dx_spot(lenient).expect("lenient line still a spot");
        assert_eq!(spot.callsign, "K1ABC");
        assert!(
            Utc::now()
                .signed_duration_since(spot.time)
                .num_seconds()
                .abs()
                < 5,
            "fallback time should be now, got {}",
            spot.time
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn telnet_reader_survives_a_non_ascii_time_token() {
        let (port, _hold) = serve_cluster(|stream| {
            use std::io::Write;
            let _ = stream
                .write_all("DX de W1AW:     14074.0  BAD1X        FT8 test     €1Z\r\n".as_bytes());
            let _ = stream.write_all(spot_line("K1ABC", 14074).as_bytes());
        });

        let mut client = DxClusterClient::with_config(local_config(port));
        client.connect().await.unwrap();

        let spot = tokio::time::timeout(Duration::from_secs(5), client.receive_spot())
            .await
            .expect("a spot within 5 s")
            .expect("spot channel open: the reader did not stop");
        assert_eq!(spot.callsign, "K1ABC");
    }

    // ---- PAN-288: reconnect with capped backoff ----

    #[test]
    fn reconnect_backoff_doubles_to_ten_times_base() {
        let mut backoff = ReconnectBackoff::with_base(Duration::from_secs(30));
        let delays: Vec<u64> = (0..6).map(|_| backoff.next_delay().as_secs()).collect();
        assert_eq!(delays, vec![30, 60, 120, 240, 300, 300]);
    }

    #[test]
    fn reconnect_backoff_resets_only_after_a_stable_session() {
        let mut backoff = ReconnectBackoff::with_base(Duration::from_secs(30));
        assert_eq!(backoff.next_delay(), Duration::from_secs(30));
        backoff.session_ended(Duration::from_secs(59));
        assert_eq!(backoff.next_delay(), Duration::from_secs(60));
        backoff.session_ended(Duration::from_secs(59));
        assert_eq!(backoff.next_delay(), Duration::from_secs(120));
        backoff.session_ended(Duration::from_secs(60));
        assert_eq!(backoff.next_delay(), Duration::from_secs(30));
    }

    #[test]
    fn reconnect_backoff_from_config_clamps_zero_delay() {
        let mut config = local_config(1);
        config.reconnect_delay_seconds = 0;
        let mut backoff = ReconnectBackoff::from_config(&config);
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
        assert_eq!(backoff.next_delay(), Duration::from_secs(2));
    }

    /// Local "cluster" that accepts every connection: each one is counted,
    /// sent `Welcome\r\n`, and handed to `script` with its 1-based number on
    /// its own thread. The connection closes when `script` returns.
    fn serve_cluster_repeatedly<F>(script: F) -> (u16, Arc<std::sync::atomic::AtomicUsize>)
    where
        F: Fn(usize, &mut std::net::TcpStream) + Send + Sync + 'static,
    {
        use std::io::Write;
        use std::sync::atomic::AtomicUsize;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let counter = accepts.clone();
        let script = Arc::new(script);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                let script = script.clone();
                std::thread::spawn(move || {
                    let _ = stream.write_all(b"Welcome\r\n");
                    script(n, &mut stream);
                });
            }
        });
        (port, accepts)
    }

    fn reconnecting_config(port: u16) -> ClusterConfig {
        let mut config = local_config(port);
        config.auto_reconnect = true;
        config
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_reconnects_after_the_cluster_closes_and_forwards_the_next_spot() {
        let (port, accepts) = serve_cluster_repeatedly(|n, stream| {
            use std::io::Write;
            if n >= 2 {
                let _ = stream.write_all(spot_line("K1ABC", 14074).as_bytes());
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let (spot_tx, mut spot_rx) = mpsc::channel::<String>(8);

        let task = {
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                let mut client = DxClusterClient::with_config(reconnecting_config(port));
                let backoff = ReconnectBackoff::with_base(Duration::from_millis(50));
                client
                    .run_with_backoff(backoff, &shutdown, |spot| {
                        let spot_tx = spot_tx.clone();
                        async move {
                            let _ = spot_tx.send(spot.callsign).await;
                        }
                    })
                    .await;
            })
        };

        let call = tokio::time::timeout(Duration::from_secs(5), spot_rx.recv())
            .await
            .expect("a spot within 5 s")
            .expect("spot forwarded");
        assert_eq!(call, "K1ABC");
        assert_eq!(accepts.load(Ordering::SeqCst), 2);

        shutdown.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("run returned after shutdown")
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_does_not_spin_while_the_cluster_keeps_closing() {
        let (port, accepts) = serve_cluster_repeatedly(|_, _| {});
        let shutdown = Arc::new(AtomicBool::new(false));

        let task = {
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                let mut client = DxClusterClient::with_config(reconnecting_config(port));
                let backoff = ReconnectBackoff::with_base(Duration::from_millis(50));
                client
                    .run_with_backoff(backoff, &shutdown, |_| async {})
                    .await;
            })
        };

        tokio::time::sleep(Duration::from_millis(1500)).await;
        let n = accepts.load(Ordering::SeqCst);
        // 50, 100, 200, 400, 500, 500 ms waits: about 6 sessions in 1.5 s.
        // The old loop made 1 accept and millions of empty turns instead.
        assert!(
            (2..=10).contains(&n),
            "expected 2..=10 reconnects in 1.5 s, got {n}"
        );
        assert!(!task.is_finished(), "run must keep retrying");

        shutdown.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("run returned after shutdown")
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_keeps_retrying_when_the_first_connect_fails_and_stops_on_shutdown() {
        // A port that was bound and released: nothing listens on it.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let shutdown = Arc::new(AtomicBool::new(false));

        let task = {
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                let mut client = DxClusterClient::with_config(reconnecting_config(port));
                let backoff = ReconnectBackoff::with_base(Duration::from_millis(50));
                client
                    .run_with_backoff(backoff, &shutdown, |_| async {})
                    .await;
            })
        };

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !task.is_finished(),
            "a failed first connect must be retried, not end the run"
        );

        shutdown.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("run returned within 2 s of shutdown")
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_without_auto_reconnect_returns_after_the_session_ends() {
        let (port, accepts) = serve_cluster_repeatedly(|_, _| {});
        let shutdown = AtomicBool::new(false);
        let mut client = DxClusterClient::with_config(local_config(port));
        assert!(!client.config().auto_reconnect);

        tokio::time::timeout(
            Duration::from_secs(2),
            client.run_until_shutdown(&shutdown, |_| async {}),
        )
        .await
        .expect("run returned within 2 s");
        assert_eq!(accepts.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod tls_security_tests {
    use super::*;
    mod fixture {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../test-support/tls.rs"
        ));
    }
    #[tokio::test]
    async fn tls_untrusted_cluster() {
        fixture::rejects_untrusted("wss", |url| async move {
            DxClusterClient::new().connect_websocket(&url).await
        })
        .await;
    }
}
