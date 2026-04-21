//! Core rig control implementation with connection management
//!
//! This module provides the main rig control interface with safe wrappers
//! around hamlib functionality, including connection management, error recovery,
//! and timeout handling.

use crate::bindings::*;
use crate::models::{Mode, ModeExt, RigCapabilities, RigModelType, Vfo};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio::time::sleep;
use tracing::{debug, error, info, instrument, warn};

/// Default timeout for rig operations in milliseconds
const DEFAULT_TIMEOUT_MS: u32 = 2000;

/// Maximum number of concurrent rig operations
const MAX_CONCURRENT_OPERATIONS: usize = 1;

/// Default retry count for failed operations
const DEFAULT_RETRY_COUNT: u32 = 3;

/// Rig connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Disconnected
    Disconnected,
    /// Connecting
    Connecting,
    /// Connected and ready
    Connected,
    /// Error state
    Error,
    /// Reconnecting after error
    Reconnecting,
}

/// PTT (Push-to-Talk) state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttState {
    /// PTT off (receive mode)
    Off,
    /// PTT on (transmit mode)
    On,
    /// PTT on via microphone
    OnMic,
    /// PTT on via data port
    OnData,
}

impl PttState {
    /// Convert to hamlib PTT constant
    pub fn to_hamlib(&self) -> u32 {
        match self {
            PttState::Off => RIG_PTT_OFF,
            PttState::On => RIG_PTT_ON,
            PttState::OnMic => RIG_PTT_ON_MIC,
            PttState::OnData => RIG_PTT_ON_DATA,
        }
    }

    /// Convert from hamlib PTT constant
    pub fn from_hamlib(ptt: u32) -> Self {
        match ptt {
            RIG_PTT_OFF => PttState::Off,
            RIG_PTT_ON => PttState::On,
            RIG_PTT_ON_MIC => PttState::OnMic,
            RIG_PTT_ON_DATA => PttState::OnData,
            _ => PttState::Off,
        }
    }
}

/// Rig configuration for connection
#[derive(Debug, Clone)]
pub struct RigConfig {
    /// Rig model
    pub model: RigModelType,
    /// Device path (e.g., "/dev/ttyUSB0", "COM3", "localhost:4532")
    pub device_path: String,
    /// Baud rate for serial connections
    pub baud_rate: Option<u32>,
    /// Connection timeout in milliseconds
    pub timeout_ms: u32,
    /// Number of retries for failed operations
    pub retry_count: u32,
    /// Enable automatic reconnection
    pub auto_reconnect: bool,
    /// Poll interval for status updates in milliseconds
    pub poll_interval_ms: u32,
    /// Additional hamlib parameters
    pub hamlib_params: HashMap<String, String>,
}

impl Default for RigConfig {
    fn default() -> Self {
        Self {
            model: RigModelType::Dummy,
            device_path: "/dev/ttyUSB0".to_string(),
            baud_rate: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            retry_count: DEFAULT_RETRY_COUNT,
            auto_reconnect: true,
            poll_interval_ms: 1000,
            hamlib_params: HashMap::new(),
        }
    }
}

/// Current rig status
#[derive(Debug, Clone)]
pub struct RigStatus {
    /// Connection state
    pub connection_state: ConnectionState,
    /// Current frequency in Hz
    pub frequency: Option<u64>,
    /// Current mode
    pub mode: Option<Mode>,
    /// Current passband width in Hz
    pub width: Option<i32>,
    /// Current VFO
    pub vfo: Option<Vfo>,
    /// PTT state
    pub ptt: Option<PttState>,
    /// Power level (0.0-1.0)
    pub power_level: Option<f32>,
    /// S-meter reading
    pub s_meter: Option<i32>,
    /// SWR reading
    pub swr: Option<f32>,
    /// Current memory channel
    pub memory_channel: Option<i32>,
    /// Last update timestamp
    pub last_update: Instant,
    /// Last error message
    pub last_error: Option<String>,
}

impl Default for RigStatus {
    fn default() -> Self {
        Self {
            connection_state: ConnectionState::Disconnected,
            frequency: None,
            mode: None,
            width: None,
            vfo: None,
            ptt: None,
            power_level: None,
            s_meter: None,
            swr: None,
            memory_channel: None,
            last_update: Instant::now(),
            last_error: None,
        }
    }
}

/// Rig operation result with error recovery information
#[derive(Debug)]
pub struct RigOperationResult<T> {
    /// Operation result
    pub result: Result<T>,
    /// Number of retries attempted
    pub retries: u32,
    /// Operation duration
    pub duration: Duration,
    /// Whether operation required reconnection
    pub reconnected: bool,
}

/// Trait for rig control operations
#[async_trait]
pub trait RigControl: Send + Sync {
    /// Connect to the rig
    async fn connect(&self) -> Result<()>;

    /// Disconnect from the rig
    async fn disconnect(&self) -> Result<()>;

    /// Get current rig status
    async fn get_status(&self) -> Result<RigStatus>;

    /// Set frequency in Hz
    async fn set_frequency(&self, vfo: Vfo, freq: u64) -> Result<()>;

    /// Get frequency in Hz
    async fn get_frequency(&self, vfo: Vfo) -> Result<u64>;

    /// Set mode and passband
    async fn set_mode(&self, vfo: Vfo, mode: Mode, width: Option<i32>) -> Result<()>;

    /// Get mode and passband
    async fn get_mode(&self, vfo: Vfo) -> Result<(Mode, i32)>;

    /// Set VFO
    async fn set_vfo(&self, vfo: Vfo) -> Result<()>;

    /// Get current VFO
    async fn get_vfo(&self) -> Result<Vfo>;

    /// Set PTT state
    async fn set_ptt(&self, vfo: Vfo, ptt: PttState) -> Result<()>;

    /// Get PTT state
    async fn get_ptt(&self, vfo: Vfo) -> Result<PttState>;

    /// Set power level (0.0-1.0)
    async fn set_power_level(&self, level: f32) -> Result<()>;

    /// Get power level
    async fn get_power_level(&self) -> Result<f32>;

    /// Get S-meter reading
    async fn get_s_meter(&self) -> Result<i32>;

    /// Get SWR reading
    async fn get_swr(&self) -> Result<f32>;

    /// Set memory channel
    async fn set_memory_channel(&self, vfo: Vfo, channel: i32) -> Result<()>;

    /// Get memory channel
    async fn get_memory_channel(&self, vfo: Vfo) -> Result<i32>;

    /// Start/stop scanning
    async fn set_scan(&self, vfo: Vfo, enable: bool) -> Result<()>;

    /// Get rig information
    async fn get_info(&self) -> Result<String>;
}

/// Main rig control implementation
pub struct Rig {
    /// Rig configuration
    config: RwLock<RigConfig>,
    /// Rig capabilities
    capabilities: RigCapabilities,
    /// Hamlib rig handle
    handle: Mutex<Option<RigHandle>>,
    /// Current rig status
    status: RwLock<RigStatus>,
    /// Connection state
    connected: AtomicBool,
    /// Operation semaphore for serialization
    op_semaphore: Semaphore,
    /// Operation counter for debugging
    op_counter: AtomicU32,
}

// Safety: RigHandle is a pointer to C data that we ensure is only accessed
// from a single thread at a time via the Mutex and Semaphore
unsafe impl Send for Rig {}
unsafe impl Sync for Rig {}

impl Rig {
    /// Create new rig instance
    pub fn new(config: RigConfig) -> Self {
        let capabilities = config.model.capabilities();

        Self {
            config: RwLock::new(config),
            capabilities,
            handle: Mutex::new(None),
            status: RwLock::new(RigStatus::default()),
            connected: AtomicBool::new(false),
            op_semaphore: Semaphore::new(MAX_CONCURRENT_OPERATIONS),
            op_counter: AtomicU32::new(0),
        }
    }

    /// Get rig capabilities
    pub fn capabilities(&self) -> &RigCapabilities {
        &self.capabilities
    }

    /// Update configuration
    pub async fn update_config(&self, config: RigConfig) -> Result<()> {
        let was_connected = self.connected.load(Ordering::Relaxed);

        if was_connected {
            self.disconnect().await?;
        }

        *self.config.write() = config;

        if was_connected {
            self.connect().await?;
        }

        Ok(())
    }

    /// Get current configuration
    pub fn get_config(&self) -> RigConfig {
        self.config.read().clone()
    }

    /// Execute rig operation with retry and error recovery
    async fn execute_operation<T, F>(&self, operation: F) -> RigOperationResult<T>
    where
        F: Fn() -> Result<T>,
        T: Send,
    {
        let start_time = Instant::now();
        let op_id = self.op_counter.fetch_add(1, Ordering::Relaxed);
        let config = self.config.read().clone();
        let mut retries = 0;
        let mut reconnected = false;

        debug!(
            "Starting rig operation {} with timeout {}ms",
            op_id, config.timeout_ms
        );

        // Acquire operation semaphore
        let _permit = self.op_semaphore.acquire().await.unwrap();

        loop {
            // Check if we're connected
            if !self.connected.load(Ordering::Relaxed) {
                if config.auto_reconnect {
                    warn!(
                        "Rig not connected, attempting reconnection for operation {}",
                        op_id
                    );
                    if let Err(e) = self.connect().await {
                        error!("Failed to reconnect for operation {}: {}", op_id, e);
                        return RigOperationResult {
                            result: Err(anyhow!(
                                "Rig not connected and reconnection failed: {}",
                                e
                            )),
                            retries,
                            duration: start_time.elapsed(),
                            reconnected,
                        };
                    }
                    reconnected = true;
                } else {
                    return RigOperationResult {
                        result: Err(anyhow!("Rig not connected")),
                        retries,
                        duration: start_time.elapsed(),
                        reconnected,
                    };
                }
            }

            // Execute operation (synchronously for now to avoid lifetime issues)
            let result = operation();

            match result {
                Ok(value) => {
                    debug!(
                        "Operation {} completed successfully after {} retries",
                        op_id, retries
                    );
                    return RigOperationResult {
                        result: Ok(value),
                        retries,
                        duration: start_time.elapsed(),
                        reconnected,
                    };
                }
                Err(e) => {
                    warn!(
                        "Operation {} failed (attempt {}): {}",
                        op_id,
                        retries + 1,
                        e
                    );

                    retries += 1;
                    if retries >= config.retry_count {
                        error!("Operation {} failed after {} retries", op_id, retries);

                        // Mark as disconnected on persistent failure
                        self.connected.store(false, Ordering::Relaxed);
                        self.update_connection_state(ConnectionState::Error, Some(e.to_string()))
                            .await;

                        return RigOperationResult {
                            result: Err(e),
                            retries,
                            duration: start_time.elapsed(),
                            reconnected,
                        };
                    }

                    // Brief delay before retry
                    sleep(Duration::from_millis(100 * retries as u64)).await;
                }
            }
        }
    }

    /// Update connection state and status
    async fn update_connection_state(&self, state: ConnectionState, error: Option<String>) {
        let mut status = self.status.write();
        status.connection_state = state;
        status.last_update = Instant::now();
        if let Some(err) = error {
            status.last_error = Some(err);
        }
    }

    /// Initialize hamlib handle
    fn init_hamlib_handle(&self) -> Result<RigHandle> {
        let config = self.config.read();
        let model_id = config.model.hamlib_id();

        unsafe {
            // Initialize hamlib with debug level 0 (errors only)
            rig_init(0);

            // Create rig handle
            let raw_handle = rig_init_rig(model_id);
            if raw_handle.is_null() {
                return Err(anyhow!("Failed to initialize rig model {}", model_id));
            }
            let handle = RigHandle::new(raw_handle);

            // Set device path
            let device_cstr = string_to_c_str(&config.device_path)
                .map_err(|e| anyhow!("Invalid device path: {}", e))?;
            let result = rig_set_conf(handle.as_ptr(), 1, device_cstr.as_ptr()); // TOKEN_PATHNAME = 1
            if !is_hamlib_success(result) {
                rig_cleanup(handle.as_ptr());
                return Err(anyhow!(
                    "Failed to set device path: {}",
                    hamlib_error_message(result)
                ));
            }

            // Set baud rate if specified
            if let Some(baud_rate) = config
                .baud_rate
                .or(Some(config.model.capabilities().default_baud_rate))
            {
                let baud_str = baud_rate.to_string();
                let baud_cstr =
                    string_to_c_str(&baud_str).map_err(|e| anyhow!("Invalid baud rate: {}", e))?;
                let result = rig_set_conf(handle.as_ptr(), 2, baud_cstr.as_ptr()); // TOKEN_SERIAL_SPEED = 2
                if !is_hamlib_success(result) {
                    warn!(
                        "Failed to set baud rate {}: {}",
                        baud_rate,
                        hamlib_error_message(result)
                    );
                }
            }

            // Set timeout
            let result = rig_set_timeout(handle.as_ptr(), config.timeout_ms as i32);
            if !is_hamlib_success(result) {
                warn!("Failed to set timeout: {}", hamlib_error_message(result));
            }

            // Set additional parameters
            for (key, value) in &config.hamlib_params {
                if let Ok(_key_cstr) = string_to_c_str(key) {
                    if let Ok(_value_cstr) = string_to_c_str(value) {
                        // This is simplified - in practice you'd need to map parameter names to token IDs
                        debug!("Setting hamlib parameter {}: {}", key, value);
                    }
                }
            }

            Ok(handle)
        }
    }
}

#[async_trait]
impl RigControl for Rig {
    #[instrument(skip(self))]
    async fn connect(&self) -> Result<()> {
        if self.connected.load(Ordering::Relaxed) {
            return Ok(());
        }

        self.update_connection_state(ConnectionState::Connecting, None)
            .await;

        // Initialize handle
        let handle = self.init_hamlib_handle()?;

        // Open connection directly (not through execute_operation, which would
        // recurse back into connect() when it sees we're not connected)
        let open_result = unsafe {
            let result = rig_open(handle.as_ptr());
            if is_hamlib_success(result) {
                Ok(())
            } else {
                Err(anyhow!(
                    "Failed to open rig: {}",
                    hamlib_error_message(result)
                ))
            }
        };

        match open_result {
            Ok(()) => {
                *self.handle.lock() = Some(handle);
                self.connected.store(true, Ordering::Relaxed);
                self.update_connection_state(ConnectionState::Connected, None)
                    .await;
                info!("Successfully connected to rig");
                Ok(())
            }
            Err(e) => {
                unsafe {
                    rig_cleanup(handle.as_ptr());
                }
                self.update_connection_state(ConnectionState::Error, Some(e.to_string()))
                    .await;
                Err(e)
            }
        }
    }

    #[instrument(skip(self))]
    async fn disconnect(&self) -> Result<()> {
        if !self.connected.load(Ordering::Relaxed) {
            return Ok(());
        }

        let handle = {
            let mut handle_guard = self.handle.lock();
            handle_guard.take()
        };

        if let Some(handle) = handle {
            let handle_for_restore = RigHandle::new(handle.as_ptr());
            let result = self
                .execute_operation(move || unsafe {
                    let close_result = rig_close(handle.as_ptr());
                    let cleanup_result = rig_cleanup(handle.as_ptr());

                    if !is_hamlib_success(close_result) {
                        warn!("Error closing rig: {}", hamlib_error_message(close_result));
                    }
                    if !is_hamlib_success(cleanup_result) {
                        warn!(
                            "Error cleaning up rig: {}",
                            hamlib_error_message(cleanup_result)
                        );
                    }

                    Ok(())
                })
                .await;

            if let Err(e) = result.result {
                warn!("Error during disconnect: {}", e);
                // Restore the handle so it isn't leaked
                let mut handle_guard = self.handle.lock();
                *handle_guard = Some(handle_for_restore);
                return Err(e);
            }
        }

        self.connected.store(false, Ordering::Relaxed);
        self.update_connection_state(ConnectionState::Disconnected, None)
            .await;
        info!("Disconnected from rig");
        Ok(())
    }

    async fn get_status(&self) -> Result<RigStatus> {
        Ok(self.status.read().clone())
    }

    #[instrument(skip(self))]
    async fn set_frequency(&self, vfo: Vfo, freq: u64) -> Result<()> {
        let handle = {
            let handle_guard = self.handle.lock();
            handle_guard.clone()
        };

        let handle = handle.ok_or_else(|| anyhow!("Rig not connected"))?;
        let vfo_hamlib = vfo.to_hamlib();
        let freq_hamlib = freq as Frequency;

        let result = self
            .execute_operation(move || unsafe {
                let result = rig_set_freq(handle.as_ptr(), vfo_hamlib, freq_hamlib);
                if is_hamlib_success(result) {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "Failed to set frequency: {}",
                        hamlib_error_message(result)
                    ))
                }
            })
            .await;

        if result.result.is_ok() {
            let mut status = self.status.write();
            status.frequency = Some(freq);
            status.last_update = Instant::now();
        }

        result.result
    }

    #[instrument(skip(self))]
    async fn get_frequency(&self, vfo: Vfo) -> Result<u64> {
        let handle = {
            let handle_guard = self.handle.lock();
            handle_guard.clone()
        };

        let handle = handle.ok_or_else(|| anyhow!("Rig not connected"))?;
        let vfo_hamlib = vfo.to_hamlib();

        let result = self
            .execute_operation(move || unsafe {
                let mut freq: Frequency = 0;
                let result = rig_get_freq(handle.as_ptr(), vfo_hamlib, &mut freq);
                if is_hamlib_success(result) {
                    Ok(freq as u64)
                } else {
                    Err(anyhow!(
                        "Failed to get frequency: {}",
                        hamlib_error_message(result)
                    ))
                }
            })
            .await;

        if let Ok(freq) = &result.result {
            let mut status = self.status.write();
            status.frequency = Some(*freq);
            status.last_update = Instant::now();
        }

        result.result
    }

    #[instrument(skip(self))]
    async fn set_mode(&self, vfo: Vfo, mode: Mode, width: Option<i32>) -> Result<()> {
        let handle = {
            let handle_guard = self.handle.lock();
            handle_guard.clone()
        };

        let handle = handle.ok_or_else(|| anyhow!("Rig not connected"))?;
        let vfo_hamlib = vfo.to_hamlib();
        let mode_hamlib = mode.to_hamlib();
        let width_hamlib = width.unwrap_or_else(|| mode.default_width().unwrap_or(0)) as i64;

        let result = self
            .execute_operation(move || unsafe {
                let result = rig_set_mode(handle.as_ptr(), vfo_hamlib, mode_hamlib, width_hamlib);
                if is_hamlib_success(result) {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "Failed to set mode: {}",
                        hamlib_error_message(result)
                    ))
                }
            })
            .await;

        if result.result.is_ok() {
            let mut status = self.status.write();
            status.mode = Some(mode);
            status.width = Some(width_hamlib as i32);
            status.last_update = Instant::now();
        }

        result.result
    }

    #[instrument(skip(self))]
    async fn get_mode(&self, vfo: Vfo) -> Result<(Mode, i32)> {
        let handle = {
            let handle_guard = self.handle.lock();
            handle_guard.clone()
        };

        let handle = handle.ok_or_else(|| anyhow!("Rig not connected"))?;
        let vfo_hamlib = vfo.to_hamlib();

        let result = self
            .execute_operation(move || unsafe {
                let mut mode: u32 = 0;
                let mut width: i64 = 0;
                let result = rig_get_mode(handle.as_ptr(), vfo_hamlib, &mut mode, &mut width);
                if is_hamlib_success(result) {
                    Ok((Mode::from_hamlib(mode), width as i32))
                } else {
                    Err(anyhow!(
                        "Failed to get mode: {}",
                        hamlib_error_message(result)
                    ))
                }
            })
            .await;

        if let Ok((mode, width)) = &result.result {
            let mut status = self.status.write();
            status.mode = Some(*mode);
            status.width = Some(*width);
            status.last_update = Instant::now();
        }

        result.result
    }

    #[instrument(skip(self))]
    async fn set_vfo(&self, vfo: Vfo) -> Result<()> {
        let handle = {
            let handle_guard = self.handle.lock();
            handle_guard.clone()
        };

        let handle = handle.ok_or_else(|| anyhow!("Rig not connected"))?;
        let vfo_hamlib = vfo.to_hamlib();

        let result = self
            .execute_operation(move || unsafe {
                let result = rig_set_vfo(handle.as_ptr(), vfo_hamlib);
                if is_hamlib_success(result) {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "Failed to set VFO: {}",
                        hamlib_error_message(result)
                    ))
                }
            })
            .await;

        if result.result.is_ok() {
            let mut status = self.status.write();
            status.vfo = Some(vfo);
            status.last_update = Instant::now();
        }

        result.result
    }

    #[instrument(skip(self))]
    async fn get_vfo(&self) -> Result<Vfo> {
        let handle = {
            let handle_guard = self.handle.lock();
            handle_guard.clone()
        };

        let handle = handle.ok_or_else(|| anyhow!("Rig not connected"))?;

        let result = self
            .execute_operation(move || unsafe {
                let mut vfo: u32 = 0;
                let result = rig_get_vfo(handle.as_ptr(), &mut vfo);
                if is_hamlib_success(result) {
                    Ok(Vfo::from_hamlib(vfo))
                } else {
                    Err(anyhow!(
                        "Failed to get VFO: {}",
                        hamlib_error_message(result)
                    ))
                }
            })
            .await;

        if let Ok(vfo) = &result.result {
            let mut status = self.status.write();
            status.vfo = Some(*vfo);
            status.last_update = Instant::now();
        }

        result.result
    }

    #[instrument(skip(self))]
    async fn set_ptt(&self, vfo: Vfo, ptt: PttState) -> Result<()> {
        let handle = {
            let handle_guard = self.handle.lock();
            handle_guard.clone()
        };

        let handle = handle.ok_or_else(|| anyhow!("Rig not connected"))?;
        let vfo_hamlib = vfo.to_hamlib();
        let ptt_hamlib = ptt.to_hamlib();

        let result = self
            .execute_operation(move || unsafe {
                let result = rig_set_ptt(handle.as_ptr(), vfo_hamlib, ptt_hamlib);
                if is_hamlib_success(result) {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "Failed to set PTT: {}",
                        hamlib_error_message(result)
                    ))
                }
            })
            .await;

        if result.result.is_ok() {
            let mut status = self.status.write();
            status.ptt = Some(ptt);
            status.last_update = Instant::now();
        }

        result.result
    }

    #[instrument(skip(self))]
    async fn get_ptt(&self, vfo: Vfo) -> Result<PttState> {
        let handle = {
            let handle_guard = self.handle.lock();
            handle_guard.clone()
        };

        let handle = handle.ok_or_else(|| anyhow!("Rig not connected"))?;
        let vfo_hamlib = vfo.to_hamlib();

        let result = self
            .execute_operation(move || unsafe {
                let mut ptt: u32 = 0;
                let result = rig_get_ptt(handle.as_ptr(), vfo_hamlib, &mut ptt);
                if is_hamlib_success(result) {
                    Ok(PttState::from_hamlib(ptt))
                } else {
                    Err(anyhow!(
                        "Failed to get PTT: {}",
                        hamlib_error_message(result)
                    ))
                }
            })
            .await;

        if let Ok(ptt) = &result.result {
            let mut status = self.status.write();
            status.ptt = Some(*ptt);
            status.last_update = Instant::now();
        }

        result.result
    }

    #[instrument(skip(self))]
    async fn set_power_level(&self, level: f32) -> Result<()> {
        if !(0.0..=1.0).contains(&level) {
            return Err(anyhow!("Power level must be between 0.0 and 1.0"));
        }

        let handle = {
            let handle_guard = self.handle.lock();
            handle_guard.clone()
        };

        let handle = handle.ok_or_else(|| anyhow!("Rig not connected"))?;

        let result = self
            .execute_operation(move || {
                unsafe {
                    // Convert to hamlib power level format
                    let power_val = level as PowerLevel;
                    let result = rig_set_level(
                        handle.as_ptr(),
                        RIG_VFO_CURR,
                        RIG_LEVEL_RFPOWER,
                        std::ptr::addr_of!(power_val) as *const c_void,
                    );
                    if is_hamlib_success(result) {
                        Ok(())
                    } else {
                        Err(anyhow!(
                            "Failed to set power level: {}",
                            hamlib_error_message(result)
                        ))
                    }
                }
            })
            .await;

        if result.result.is_ok() {
            let mut status = self.status.write();
            status.power_level = Some(level);
            status.last_update = Instant::now();
        }

        result.result
    }

    #[instrument(skip(self))]
    async fn get_power_level(&self) -> Result<f32> {
        let handle = {
            let handle_guard = self.handle.lock();
            handle_guard.clone()
        };

        let handle = handle.ok_or_else(|| anyhow!("Rig not connected"))?;

        let result = self
            .execute_operation(move || unsafe {
                let mut power_val: PowerLevel = 0.0;
                let result = rig_get_level(
                    handle.as_ptr(),
                    RIG_VFO_CURR,
                    RIG_LEVEL_RFPOWER,
                    std::ptr::addr_of_mut!(power_val) as *mut _ as _,
                );
                if is_hamlib_success(result) {
                    Ok(power_val)
                } else {
                    Err(anyhow!(
                        "Failed to get power level: {}",
                        hamlib_error_message(result)
                    ))
                }
            })
            .await;

        if let Ok(power) = &result.result {
            let mut status = self.status.write();
            status.power_level = Some(*power);
            status.last_update = Instant::now();
        }

        result.result
    }

    #[instrument(skip(self))]
    async fn get_s_meter(&self) -> Result<i32> {
        let handle = {
            let handle_guard = self.handle.lock();
            handle_guard.clone()
        };

        let handle = handle.ok_or_else(|| anyhow!("Rig not connected"))?;

        let result = self
            .execute_operation(move || unsafe {
                let mut s_val: SMeter = 0;
                let result = rig_get_level(
                    handle.as_ptr(),
                    RIG_VFO_CURR,
                    RIG_LEVEL_STRENGTH,
                    std::ptr::addr_of_mut!(s_val) as *mut _ as _,
                );
                if is_hamlib_success(result) {
                    Ok(s_val)
                } else {
                    Err(anyhow!(
                        "Failed to get S-meter: {}",
                        hamlib_error_message(result)
                    ))
                }
            })
            .await;

        if let Ok(s_meter) = &result.result {
            let mut status = self.status.write();
            status.s_meter = Some(*s_meter);
            status.last_update = Instant::now();
        }

        result.result
    }

    #[instrument(skip(self))]
    async fn get_swr(&self) -> Result<f32> {
        let handle = {
            let handle_guard = self.handle.lock();
            handle_guard.clone()
        };

        let handle = handle.ok_or_else(|| anyhow!("Rig not connected"))?;

        let result = self
            .execute_operation(move || unsafe {
                let mut swr_val: SwrReading = 0.0;
                let result = rig_get_level(
                    handle.as_ptr(),
                    RIG_VFO_CURR,
                    RIG_LEVEL_SWR,
                    std::ptr::addr_of_mut!(swr_val) as *mut _ as _,
                );
                if is_hamlib_success(result) {
                    Ok(swr_val)
                } else {
                    Err(anyhow!(
                        "Failed to get SWR: {}",
                        hamlib_error_message(result)
                    ))
                }
            })
            .await;

        if let Ok(swr) = &result.result {
            let mut status = self.status.write();
            status.swr = Some(*swr);
            status.last_update = Instant::now();
        }

        result.result
    }

    #[instrument(skip(self))]
    async fn set_memory_channel(&self, vfo: Vfo, channel: i32) -> Result<()> {
        let handle = {
            let handle_guard = self.handle.lock();
            handle_guard.clone()
        };

        let handle = handle.ok_or_else(|| anyhow!("Rig not connected"))?;
        let vfo_hamlib = vfo.to_hamlib();

        let result = self
            .execute_operation(move || unsafe {
                let result = rig_set_mem(handle.as_ptr(), vfo_hamlib, channel);
                if is_hamlib_success(result) {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "Failed to set memory channel: {}",
                        hamlib_error_message(result)
                    ))
                }
            })
            .await;

        if result.result.is_ok() {
            let mut status = self.status.write();
            status.memory_channel = Some(channel);
            status.last_update = Instant::now();
        }

        result.result
    }

    #[instrument(skip(self))]
    async fn get_memory_channel(&self, vfo: Vfo) -> Result<i32> {
        let handle = {
            let handle_guard = self.handle.lock();
            handle_guard.clone()
        };

        let handle = handle.ok_or_else(|| anyhow!("Rig not connected"))?;
        let vfo_hamlib = vfo.to_hamlib();

        let result = self
            .execute_operation(move || unsafe {
                let mut channel: i32 = 0;
                let result = rig_get_mem(handle.as_ptr(), vfo_hamlib, &mut channel);
                if is_hamlib_success(result) {
                    Ok(channel)
                } else {
                    Err(anyhow!(
                        "Failed to get memory channel: {}",
                        hamlib_error_message(result)
                    ))
                }
            })
            .await;

        if let Ok(channel) = &result.result {
            let mut status = self.status.write();
            status.memory_channel = Some(*channel);
            status.last_update = Instant::now();
        }

        result.result
    }

    #[instrument(skip(self))]
    async fn set_scan(&self, vfo: Vfo, enable: bool) -> Result<()> {
        let handle = {
            let handle_guard = self.handle.lock();
            handle_guard.clone()
        };

        let handle = handle.ok_or_else(|| anyhow!("Rig not connected"))?;
        let vfo_hamlib = vfo.to_hamlib();
        let scan_type = if enable { RIG_SCAN_MEM } else { RIG_SCAN_STOP };

        let result = self
            .execute_operation(move || unsafe {
                let result = rig_scan(handle.as_ptr(), vfo_hamlib, scan_type, 0);
                if is_hamlib_success(result) {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "Failed to set scan: {}",
                        hamlib_error_message(result)
                    ))
                }
            })
            .await;

        result.result
    }

    #[instrument(skip(self))]
    async fn get_info(&self) -> Result<String> {
        let handle = {
            let handle_guard = self.handle.lock();
            handle_guard.clone()
        };

        let handle = handle.ok_or_else(|| anyhow!("Rig not connected"))?;

        let result = self
            .execute_operation(move || unsafe {
                let info_ptr = rig_get_info(handle.as_ptr());
                if info_ptr.is_null() {
                    Err(anyhow!("Failed to get rig info"))
                } else {
                    c_str_to_string(info_ptr)
                        .map_err(|e| anyhow!("Failed to convert info string: {}", e))
                }
            })
            .await;

        result.result
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        // Cleanup on drop - this runs in the destructor
        if let Some(handle) = self.handle.lock().take() {
            unsafe {
                rig_close(handle.as_ptr());
                rig_cleanup(handle.as_ptr());
            }
        }
    }
}
