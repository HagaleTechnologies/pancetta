//! Advanced rig control features
//!
//! This module provides advanced functionality for band switching, memory channel
//! management, scanning operations, and real-time monitoring of rig parameters.

use crate::models::{Band, Mode, Vfo};
use crate::rig::{PttState, Rig, RigControl, RigStatus};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, RwLock};
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, instrument, warn};

/// Memory channel information
#[derive(Debug, Clone)]
pub struct MemoryChannel {
    /// Channel number
    pub channel: i32,
    /// Frequency in Hz
    pub frequency: u64,
    /// Mode
    pub mode: Mode,
    /// Passband width in Hz
    pub width: i32,
    /// Channel name/label
    pub name: Option<String>,
    /// CTCSS tone frequency (Hz)
    pub ctcss_tone: Option<f32>,
    /// DCS code
    pub dcs_code: Option<u16>,
    /// Repeater offset in Hz
    pub offset: Option<i64>,
    /// Last used timestamp
    pub last_used: Option<Instant>,
}

impl Default for MemoryChannel {
    fn default() -> Self {
        Self {
            channel: 0,
            frequency: 14_200_000,
            mode: Mode::USB,
            width: 2400,
            name: None,
            ctcss_tone: None,
            dcs_code: None,
            offset: None,
            last_used: None,
        }
    }
}

/// Scanning configuration
#[derive(Debug, Clone)]
pub struct ScanConfig {
    /// Scan type
    pub scan_type: ScanType,
    /// Scan speed (channels per second)
    pub speed: f32,
    /// Pause time on active channel (seconds)
    pub pause_time: f32,
    /// Resume after timeout (seconds)
    pub resume_timeout: f32,
    /// Squelch threshold for stopping scan
    pub squelch_threshold: i32,
    /// Include memory channels in scan
    pub include_memory: bool,
    /// Include VFO frequency range in scan
    pub include_vfo: bool,
    /// Custom frequency list for scanning
    pub custom_frequencies: Vec<u64>,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            scan_type: ScanType::Memory,
            speed: 5.0,
            pause_time: 2.0,
            resume_timeout: 5.0,
            squelch_threshold: -100,
            include_memory: true,
            include_vfo: false,
            custom_frequencies: Vec::new(),
        }
    }
}

/// Types of scanning
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanType {
    /// Scan memory channels
    Memory,
    /// Scan selected channels
    Selected,
    /// Priority scan
    Priority,
    /// Program scan (frequency range)
    Program,
    /// VFO scan
    Vfo,
    /// Custom frequency list
    Custom,
}

/// Band plan entry
#[derive(Debug, Clone)]
pub struct BandPlan {
    /// Band
    pub band: Band,
    /// Default frequency in Hz
    pub default_frequency: u64,
    /// Default mode for this band
    pub default_mode: Mode,
    /// Recommended passband width
    pub default_width: i32,
    /// Band edges (min, max) in Hz
    pub band_edges: (u64, u64),
    /// Common frequencies for this band
    pub common_frequencies: Vec<(u64, String)>,
}

impl BandPlan {
    /// Create band plan for common amateur radio bands
    pub fn standard_bands() -> Vec<Self> {
        vec![
            BandPlan {
                band: Band::Band160m,
                default_frequency: 1_840_000,
                default_mode: Mode::LSB,
                default_width: 2400,
                band_edges: (1_800_000, 2_000_000),
                common_frequencies: vec![
                    (1_840_000, "CW".to_string()),
                    (1_910_000, "Digital".to_string()),
                    (1_995_000, "Phone".to_string()),
                ],
            },
            BandPlan {
                band: Band::Band80m,
                default_frequency: 3_700_000,
                default_mode: Mode::LSB,
                default_width: 2400,
                band_edges: (3_500_000, 4_000_000),
                common_frequencies: vec![
                    (3_500_000, "CW".to_string()),
                    (3_570_000, "Digital".to_string()),
                    (3_700_000, "Phone".to_string()),
                ],
            },
            BandPlan {
                band: Band::Band40m,
                default_frequency: 7_200_000,
                default_mode: Mode::LSB,
                default_width: 2400,
                band_edges: (7_000_000, 7_300_000),
                common_frequencies: vec![
                    (7_000_000, "CW".to_string()),
                    (7_070_000, "Digital".to_string()),
                    (7_200_000, "Phone".to_string()),
                ],
            },
            BandPlan {
                band: Band::Band20m,
                default_frequency: 14_200_000,
                default_mode: Mode::USB,
                default_width: 2400,
                band_edges: (14_000_000, 14_350_000),
                common_frequencies: vec![
                    (14_000_000, "CW".to_string()),
                    (14_070_000, "Digital".to_string()),
                    (14_074_000, "FT8".to_string()),
                    (14_200_000, "Phone".to_string()),
                ],
            },
            BandPlan {
                band: Band::Band15m,
                default_frequency: 21_200_000,
                default_mode: Mode::USB,
                default_width: 2400,
                band_edges: (21_000_000, 21_450_000),
                common_frequencies: vec![
                    (21_000_000, "CW".to_string()),
                    (21_070_000, "Digital".to_string()),
                    (21_074_000, "FT8".to_string()),
                    (21_200_000, "Phone".to_string()),
                ],
            },
            BandPlan {
                band: Band::Band10m,
                default_frequency: 28_200_000,
                default_mode: Mode::USB,
                default_width: 2400,
                band_edges: (28_000_000, 29_700_000),
                common_frequencies: vec![
                    (28_000_000, "CW".to_string()),
                    (28_070_000, "Digital".to_string()),
                    (28_074_000, "FT8".to_string()),
                    (28_200_000, "Phone".to_string()),
                    (29_600_000, "FM".to_string()),
                ],
            },
        ]
    }

    /// Get band plan for specific band
    pub fn for_band(band: Band) -> Option<Self> {
        Self::standard_bands()
            .into_iter()
            .find(|bp| bp.band == band)
    }
}

/// Real-time monitoring data
#[derive(Debug, Clone)]
pub struct MonitoringData {
    /// Timestamp
    pub timestamp: Instant,
    /// S-meter reading
    pub s_meter: Option<i32>,
    /// SWR reading
    pub swr: Option<f32>,
    /// Power output (0.0-1.0)
    pub power_output: Option<f32>,
    /// ALC level
    pub alc_level: Option<f32>,
    /// Current frequency
    pub frequency: Option<u64>,
    /// Current mode
    pub mode: Option<Mode>,
    /// PTT state
    pub ptt_active: bool,
    /// Squelch state
    pub squelch_open: bool,
}

/// Advanced rig control interface
#[async_trait]
pub trait AdvancedRigControl: RigControl {
    /// Switch to band with default settings
    async fn switch_to_band(&self, band: Band) -> Result<()>;

    /// Get band plan for current frequency
    async fn get_current_band_plan(&self) -> Result<Option<BandPlan>>;

    /// Save current settings to memory channel
    async fn save_to_memory(&self, channel: i32, name: Option<String>) -> Result<()>;

    /// Load settings from memory channel
    async fn load_from_memory(&self, channel: i32) -> Result<MemoryChannel>;

    /// Get all programmed memory channels
    async fn list_memory_channels(&self) -> Result<Vec<MemoryChannel>>;

    /// Clear memory channel
    async fn clear_memory_channel(&self, channel: i32) -> Result<()>;

    /// Start scanning with configuration
    async fn start_scan(&self, config: ScanConfig) -> Result<()>;

    /// Stop scanning
    async fn stop_scan(&self) -> Result<()>;

    /// Get current scan status
    async fn get_scan_status(&self) -> Result<ScanStatus>;

    /// Start real-time monitoring
    async fn start_monitoring(
        &self,
        interval_ms: u64,
    ) -> Result<broadcast::Receiver<MonitoringData>>;

    /// Stop real-time monitoring
    async fn stop_monitoring(&self) -> Result<()>;

    /// Get antenna switch position (if supported)
    async fn get_antenna(&self) -> Result<u32>;

    /// Set antenna switch position (if supported)
    async fn set_antenna(&self, antenna: u32) -> Result<()>;

    /// Get available antenna positions
    async fn list_antennas(&self) -> Result<Vec<u32>>;
}

/// Scan status information
#[derive(Debug, Clone)]
pub struct ScanStatus {
    /// Whether scanning is active
    pub active: bool,
    /// Current scan type
    pub scan_type: ScanType,
    /// Current channel or frequency being scanned
    pub current_position: String,
    /// Number of channels scanned
    pub channels_scanned: u32,
    /// Number of active signals found
    pub signals_found: u32,
    /// Scan start time
    pub start_time: Option<Instant>,
    /// Last activity time
    pub last_activity: Option<Instant>,
}

impl Default for ScanStatus {
    fn default() -> Self {
        Self {
            active: false,
            scan_type: ScanType::Memory,
            current_position: "None".to_string(),
            channels_scanned: 0,
            signals_found: 0,
            start_time: None,
            last_activity: None,
        }
    }
}

/// Implementation of advanced rig control
pub struct AdvancedRig {
    /// Base rig control
    rig: Arc<Rig>,
    /// Memory channels cache
    memory_channels: RwLock<HashMap<i32, MemoryChannel>>,
    /// Band plans
    band_plans: Vec<BandPlan>,
    /// Current scan status
    scan_status: RwLock<ScanStatus>,
    /// Monitoring broadcast channel
    monitoring_tx: RwLock<Option<broadcast::Sender<MonitoringData>>>,
    /// Monitoring task handle
    monitoring_handle: RwLock<Option<tokio::task::JoinHandle<()>>>,
    /// Scan task handle
    scan_handle: RwLock<Option<tokio::task::JoinHandle<()>>>,
}

impl AdvancedRig {
    /// Create new advanced rig control
    pub fn new(rig: Arc<Rig>) -> Self {
        Self {
            rig,
            memory_channels: RwLock::new(HashMap::new()),
            band_plans: BandPlan::standard_bands(),
            scan_status: RwLock::new(ScanStatus::default()),
            monitoring_tx: RwLock::new(None),
            monitoring_handle: RwLock::new(None),
            scan_handle: RwLock::new(None),
        }
    }

    /// Load memory channels from rig
    async fn load_memory_channels(&self) -> Result<()> {
        let capabilities = self.rig.capabilities();
        let max_channels = capabilities.memory_channels.unwrap_or(100);

        let mut channels = HashMap::new();

        for channel in 0..max_channels {
            // Try to load each memory channel
            match self.load_from_memory(channel as i32).await {
                Ok(mem_channel) => {
                    channels.insert(channel as i32, mem_channel);
                }
                Err(_) => {
                    // Channel is empty or not accessible
                    continue;
                }
            }
        }

        *self.memory_channels.write().await = channels;
        Ok(())
    }

    /// Execute scanning loop
    async fn scan_loop(&self, config: ScanConfig) -> Result<()> {
        let mut scan_positions = Vec::new();

        // Build scan list based on configuration
        match config.scan_type {
            ScanType::Memory => {
                if config.include_memory {
                    let channels = self.memory_channels.read().await;
                    for &channel in channels.keys() {
                        scan_positions.push(ScanPosition::Memory(channel));
                    }
                }
            }
            ScanType::Custom => {
                for &freq in &config.custom_frequencies {
                    scan_positions.push(ScanPosition::Frequency(freq));
                }
            }
            ScanType::Program => {
                // For program scan, we'd need frequency range information
                // This is a simplified implementation
                let current_freq = self.rig.get_frequency(Vfo::Current).await?;
                let band = Band::from_frequency(current_freq);
                if let Some(band) = band {
                    let (start, end) = band.frequency_range();
                    let step = 25_000; // 25 kHz steps
                    let mut freq = start;
                    while freq <= end {
                        scan_positions.push(ScanPosition::Frequency(freq));
                        freq += step;
                    }
                }
            }
            _ => {
                return Err(anyhow!("Scan type {:?} not implemented", config.scan_type));
            }
        }

        if scan_positions.is_empty() {
            return Err(anyhow!("No positions to scan"));
        }

        info!("Starting scan with {} positions", scan_positions.len());

        let mut position_index = 0;
        let scan_interval = Duration::from_secs_f32(1.0 / config.speed);
        let mut interval_timer = interval(scan_interval);

        // Update scan status
        {
            let mut status = self.scan_status.write().await;
            status.active = true;
            status.scan_type = config.scan_type;
            status.start_time = Some(Instant::now());
            status.channels_scanned = 0;
            status.signals_found = 0;
        }

        loop {
            interval_timer.tick().await;

            // Check if scan should stop
            {
                let status = self.scan_status.read().await;
                if !status.active {
                    break;
                }
            }

            let position = &scan_positions[position_index];

            // Update current position
            {
                let mut status = self.scan_status.write().await;
                status.current_position = format!("{:?}", position);
                status.channels_scanned += 1;
            }

            // Tune to position
            match position {
                ScanPosition::Memory(channel) => {
                    if let Err(e) = self.rig.set_memory_channel(Vfo::Current, *channel).await {
                        warn!("Failed to set memory channel {}: {}", channel, e);
                    }
                }
                ScanPosition::Frequency(freq) => {
                    if let Err(e) = self.rig.set_frequency(Vfo::Current, *freq).await {
                        warn!("Failed to set frequency {}: {}", freq, e);
                    }
                }
            }

            // Brief pause to settle
            sleep(Duration::from_millis(50)).await;

            // Check for signal activity
            let signal_present = match self.rig.get_s_meter().await {
                Ok(s_meter) => s_meter > config.squelch_threshold,
                Err(_) => false,
            };

            if signal_present {
                info!("Signal detected at position {:?}", position);

                // Update scan status
                {
                    let mut status = self.scan_status.write().await;
                    status.signals_found += 1;
                    status.last_activity = Some(Instant::now());
                }

                // Pause on active signal
                sleep(Duration::from_secs_f32(config.pause_time)).await;

                // Check if signal is still present
                let still_present = match self.rig.get_s_meter().await {
                    Ok(s_meter) => s_meter > config.squelch_threshold,
                    Err(_) => false,
                };

                if still_present {
                    // Wait for resume timeout
                    sleep(Duration::from_secs_f32(config.resume_timeout)).await;
                }
            }

            // Move to next position
            position_index = (position_index + 1) % scan_positions.len();
        }

        // Update scan status
        {
            let mut status = self.scan_status.write().await;
            status.active = false;
            status.current_position = "Stopped".to_string();
        }

        info!("Scan stopped");
        Ok(())
    }

    /// Execute monitoring loop
    async fn monitoring_loop(
        &self,
        interval_ms: u64,
        tx: broadcast::Sender<MonitoringData>,
    ) -> Result<()> {
        let mut interval_timer = interval(Duration::from_millis(interval_ms));

        info!("Starting monitoring with {}ms interval", interval_ms);

        loop {
            interval_timer.tick().await;

            // Check if monitoring should stop
            if tx.receiver_count() == 0 {
                info!("No more monitoring receivers, stopping");
                break;
            }

            // Collect monitoring data
            let monitoring_data = MonitoringData {
                timestamp: Instant::now(),
                s_meter: self.rig.get_s_meter().await.ok(),
                swr: self.rig.get_swr().await.ok(),
                power_output: self.rig.get_power_level().await.ok(),
                alc_level: None, // Would need ALC level reading implementation
                frequency: self.rig.get_frequency(Vfo::Current).await.ok(),
                mode: self
                    .rig
                    .get_mode(Vfo::Current)
                    .await
                    .ok()
                    .map(|(mode, _)| mode),
                ptt_active: self
                    .rig
                    .get_ptt(Vfo::Current)
                    .await
                    .map(|ptt| ptt != PttState::Off)
                    .unwrap_or(false),
                squelch_open: false, // Would need squelch state reading
            };

            // Send monitoring data
            if let Err(e) = tx.send(monitoring_data) {
                debug!("Failed to send monitoring data: {}", e);
                break;
            }
        }

        info!("Monitoring stopped");
        Ok(())
    }
}

/// Scan position types
#[derive(Debug, Clone)]
enum ScanPosition {
    Memory(i32),
    Frequency(u64),
}

// Delegate basic RigControl methods to the underlying rig
#[async_trait]
impl RigControl for AdvancedRig {
    async fn connect(&self) -> Result<()> {
        let result = self.rig.connect().await;
        if result.is_ok() {
            // Load memory channels after successful connection
            if let Err(e) = self.load_memory_channels().await {
                warn!("Failed to load memory channels: {}", e);
            }
        }
        result
    }

    async fn disconnect(&self) -> Result<()> {
        // Stop monitoring and scanning
        if let Err(e) = self.stop_monitoring().await {
            warn!("Error stopping monitoring: {}", e);
        }
        if let Err(e) = self.stop_scan().await {
            warn!("Error stopping scan: {}", e);
        }

        self.rig.disconnect().await
    }

    async fn get_status(&self) -> Result<RigStatus> {
        self.rig.get_status().await
    }

    async fn set_frequency(&self, vfo: Vfo, freq: u64) -> Result<()> {
        self.rig.set_frequency(vfo, freq).await
    }

    async fn get_frequency(&self, vfo: Vfo) -> Result<u64> {
        self.rig.get_frequency(vfo).await
    }

    async fn set_mode(&self, vfo: Vfo, mode: Mode, width: Option<i32>) -> Result<()> {
        self.rig.set_mode(vfo, mode, width).await
    }

    async fn get_mode(&self, vfo: Vfo) -> Result<(Mode, i32)> {
        self.rig.get_mode(vfo).await
    }

    async fn set_vfo(&self, vfo: Vfo) -> Result<()> {
        self.rig.set_vfo(vfo).await
    }

    async fn get_vfo(&self) -> Result<Vfo> {
        self.rig.get_vfo().await
    }

    async fn set_ptt(&self, vfo: Vfo, ptt: PttState) -> Result<()> {
        self.rig.set_ptt(vfo, ptt).await
    }

    async fn get_ptt(&self, vfo: Vfo) -> Result<PttState> {
        self.rig.get_ptt(vfo).await
    }

    async fn set_power_level(&self, level: f32) -> Result<()> {
        self.rig.set_power_level(level).await
    }

    async fn get_power_level(&self) -> Result<f32> {
        self.rig.get_power_level().await
    }

    async fn get_s_meter(&self) -> Result<i32> {
        self.rig.get_s_meter().await
    }

    async fn get_swr(&self) -> Result<f32> {
        self.rig.get_swr().await
    }

    async fn set_memory_channel(&self, vfo: Vfo, channel: i32) -> Result<()> {
        self.rig.set_memory_channel(vfo, channel).await
    }

    async fn get_memory_channel(&self, vfo: Vfo) -> Result<i32> {
        self.rig.get_memory_channel(vfo).await
    }

    async fn set_scan(&self, vfo: Vfo, enable: bool) -> Result<()> {
        self.rig.set_scan(vfo, enable).await
    }

    async fn get_info(&self) -> Result<String> {
        self.rig.get_info().await
    }
}

#[async_trait]
impl AdvancedRigControl for AdvancedRig {
    #[instrument(skip(self))]
    async fn switch_to_band(&self, band: Band) -> Result<()> {
        let band_plan =
            BandPlan::for_band(band).ok_or_else(|| anyhow!("No band plan for band {:?}", band))?;

        // Set frequency to default for this band
        self.set_frequency(Vfo::Current, band_plan.default_frequency)
            .await?;

        // Set mode to default for this band
        self.set_mode(
            Vfo::Current,
            band_plan.default_mode,
            Some(band_plan.default_width),
        )
        .await?;

        info!(
            "Switched to band {:?} at {} Hz",
            band, band_plan.default_frequency
        );
        Ok(())
    }

    async fn get_current_band_plan(&self) -> Result<Option<BandPlan>> {
        let freq = self.get_frequency(Vfo::Current).await?;
        let band = Band::from_frequency(freq);
        Ok(band.and_then(BandPlan::for_band))
    }

    #[instrument(skip(self))]
    async fn save_to_memory(&self, channel: i32, name: Option<String>) -> Result<()> {
        // Get current settings
        let frequency = self.get_frequency(Vfo::Current).await?;
        let (mode, width) = self.get_mode(Vfo::Current).await?;

        // Create memory channel entry
        let memory_channel = MemoryChannel {
            channel,
            frequency,
            mode,
            width,
            name,
            ctcss_tone: None,
            dcs_code: None,
            offset: None,
            last_used: Some(Instant::now()),
        };

        // Save to rig (simplified - real implementation would use rig_set_channel)
        // For now, we'll just store in our cache
        self.memory_channels
            .write()
            .await
            .insert(channel, memory_channel.clone());

        info!("Saved current settings to memory channel {}", channel);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn load_from_memory(&self, channel: i32) -> Result<MemoryChannel> {
        // Try to load from cache first
        if let Some(memory_channel) = self.memory_channels.read().await.get(&channel).cloned() {
            // Load settings to rig
            self.set_frequency(Vfo::Current, memory_channel.frequency)
                .await?;
            self.set_mode(
                Vfo::Current,
                memory_channel.mode,
                Some(memory_channel.width),
            )
            .await?;

            info!("Loaded memory channel {}", channel);
            return Ok(memory_channel);
        }

        // Try to load from rig
        self.set_memory_channel(Vfo::Current, channel).await?;

        // Get current settings (which should now be from the memory channel)
        let frequency = self.get_frequency(Vfo::Current).await?;
        let (mode, width) = self.get_mode(Vfo::Current).await?;

        let memory_channel = MemoryChannel {
            channel,
            frequency,
            mode,
            width,
            name: None,
            ctcss_tone: None,
            dcs_code: None,
            offset: None,
            last_used: Some(Instant::now()),
        };

        // Cache it
        self.memory_channels
            .write()
            .await
            .insert(channel, memory_channel.clone());

        Ok(memory_channel)
    }

    async fn list_memory_channels(&self) -> Result<Vec<MemoryChannel>> {
        let channels = self.memory_channels.read().await;
        let mut channel_list: Vec<_> = channels.values().cloned().collect();
        channel_list.sort_by_key(|ch| ch.channel);
        Ok(channel_list)
    }

    #[instrument(skip(self))]
    async fn clear_memory_channel(&self, channel: i32) -> Result<()> {
        // Remove from cache
        self.memory_channels.write().await.remove(&channel);

        // Clear from rig (simplified - real implementation would use rig_clear_channel)
        info!("Cleared memory channel {}", channel);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn start_scan(&self, config: ScanConfig) -> Result<()> {
        // Stop any existing scan
        self.stop_scan().await?;

        // Clone the necessary components for the scan task
        let rig_clone = Arc::clone(&self.rig);

        // Create scan task
        let handle = tokio::spawn(async move {
            // Create a simplified AdvancedRig for the scan task
            // We don't need all the components for scanning
            let temp_rig = AdvancedRig::new(rig_clone);

            if let Err(e) = temp_rig.scan_loop(config).await {
                error!("Scan loop error: {}", e);
            }
        });

        *self.scan_handle.write().await = Some(handle);
        info!("Started scanning");
        Ok(())
    }

    #[instrument(skip(self))]
    async fn stop_scan(&self) -> Result<()> {
        // Signal scan to stop
        {
            let mut status = self.scan_status.write().await;
            status.active = false;
        }

        // Wait for scan task to complete
        if let Some(handle) = self.scan_handle.write().await.take() {
            if let Err(e) = handle.await {
                warn!("Error waiting for scan task: {}", e);
            }
        }

        info!("Stopped scanning");
        Ok(())
    }

    async fn get_scan_status(&self) -> Result<ScanStatus> {
        Ok(self.scan_status.read().await.clone())
    }

    #[instrument(skip(self))]
    async fn start_monitoring(
        &self,
        interval_ms: u64,
    ) -> Result<broadcast::Receiver<MonitoringData>> {
        // Stop any existing monitoring
        self.stop_monitoring().await?;

        // Create broadcast channel
        let (tx, rx) = broadcast::channel(1000);

        // Store sender
        *self.monitoring_tx.write().await = Some(tx.clone());

        // Start monitoring task
        let rig_clone = Arc::clone(&self.rig);
        let handle = tokio::spawn(async move {
            let temp_rig = AdvancedRig::new(rig_clone);

            if let Err(e) = temp_rig.monitoring_loop(interval_ms, tx).await {
                error!("Monitoring loop error: {}", e);
            }
        });

        *self.monitoring_handle.write().await = Some(handle);

        info!("Started monitoring with {}ms interval", interval_ms);
        Ok(rx)
    }

    #[instrument(skip(self))]
    async fn stop_monitoring(&self) -> Result<()> {
        // Drop the sender to signal monitoring to stop
        *self.monitoring_tx.write().await = None;

        // Wait for monitoring task to complete
        if let Some(handle) = self.monitoring_handle.write().await.take() {
            if let Err(e) = handle.await {
                warn!("Error waiting for monitoring task: {}", e);
            }
        }

        info!("Stopped monitoring");
        Ok(())
    }

    async fn get_antenna(&self) -> Result<u32> {
        // This would be implemented using rig_get_level with RIG_LEVEL_ANT
        // For now, return default antenna 1
        Ok(1)
    }

    async fn set_antenna(&self, antenna: u32) -> Result<()> {
        // This would be implemented using rig_set_level with RIG_LEVEL_ANT
        info!("Set antenna to position {}", antenna);
        Ok(())
    }

    async fn list_antennas(&self) -> Result<Vec<u32>> {
        // Return available antenna positions based on rig capabilities
        Ok(vec![1, 2])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::RigModelType;
    use crate::rig::RigConfig;

    #[tokio::test]
    async fn test_band_plan_creation() {
        let band_plans = BandPlan::standard_bands();
        assert!(!band_plans.is_empty());

        let band_20m = BandPlan::for_band(Band::Band20m);
        assert!(band_20m.is_some());

        let plan = band_20m.unwrap();
        assert_eq!(plan.band, Band::Band20m);
        assert_eq!(plan.default_mode, Mode::USB);
        assert!(plan.default_frequency >= 14_000_000);
        assert!(plan.default_frequency <= 14_350_000);
    }

    #[tokio::test]
    async fn test_memory_channel() {
        let channel = MemoryChannel {
            channel: 1,
            frequency: 14_200_000,
            mode: Mode::USB,
            width: 2400,
            name: Some("20m USB".to_string()),
            ..Default::default()
        };

        assert_eq!(channel.channel, 1);
        assert_eq!(channel.frequency, 14_200_000);
        assert_eq!(channel.mode, Mode::USB);
        assert_eq!(channel.name, Some("20m USB".to_string()));
    }

    #[tokio::test]
    async fn test_scan_config() {
        let config = ScanConfig {
            scan_type: ScanType::Memory,
            speed: 10.0,
            pause_time: 1.0,
            custom_frequencies: vec![14_200_000, 21_200_000, 28_200_000],
            ..Default::default()
        };

        assert_eq!(config.scan_type, ScanType::Memory);
        assert_eq!(config.speed, 10.0);
        assert_eq!(config.custom_frequencies.len(), 3);
    }
}
