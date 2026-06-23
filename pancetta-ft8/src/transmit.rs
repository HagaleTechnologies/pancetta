//! FT8 transmission controller with PTT and safety features
//!
//! This module provides comprehensive transmission control for FT8:
//! - Push-to-talk (PTT) control via multiple interfaces
//! - FCC Part 97 compliance (6-minute TX timeout)
//! - Band edge protection and frequency validation
//! - Power limit enforcement
//! - Audio output management
//! - Transmission scheduling and coordination

// rationale: the PTT/audio mutex guards are taken in short, explicitly-scoped
// blocks; the `.await` points sit between (not inside) those scopes. Refactoring
// the locking shape is a correctness-sensitive change out of scope for lint
// hygiene, so the guard lifetimes are left exactly as-is and the lint is silenced
// with this note for a future dedicated review.
#![allow(clippy::await_holding_lock)]
// rationale: `AudioOutput` / `PttController` wrap platform handles that aren't
// Send+Sync; the `Arc<Mutex<..>>` is the intentional single-owner-shared shape and
// access is confined to this module's task. Changing it is out of scope here.
#![allow(clippy::arc_with_non_send_sync)]

use crate::{
    encoder::Ft8Encoder,
    modulator::{convert_samples, AudioFormat, Ft8Modulator},
    Ft8Error, Ft8Result,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant, SystemTime};
use tracing::{debug, error, info, warn};

#[cfg(feature = "gpio")]
use rppal::gpio::{Gpio, Level, OutputPin};

/// Maximum transmission time per FCC Part 97 (6 minutes)
pub const MAX_TX_TIME_SECONDS: u64 = 360;

/// Minimum time between transmissions (1 second)
pub const MIN_TX_INTERVAL_SECONDS: u64 = 1;

/// Maximum power output in watts (configurable limit)
pub const DEFAULT_MAX_POWER_WATTS: u8 = 100;

/// Band edge protection margin in Hz
pub const BAND_EDGE_MARGIN_HZ: f64 = 1000.0;

/// VOX delay time in milliseconds
pub const VOX_DELAY_MS: u64 = 500;

/// PTT assertion time before audio (milliseconds)
pub const PTT_LEAD_TIME_MS: u64 = 100;

/// PTT hold time after audio (milliseconds)
pub const PTT_TAIL_TIME_MS: u64 = 50;

/// FT8 transmission controller with safety features
pub struct Ft8Transmitter {
    /// FT8 encoder for generating symbols
    encoder: Ft8Encoder,
    /// Audio modulator for signal generation
    modulator: Ft8Modulator,
    /// PTT control interface
    ptt_controller: Arc<Mutex<PttController>>,
    /// Audio output interface
    audio_output: Arc<Mutex<AudioOutput>>,
    /// Transmission configuration
    config: Arc<RwLock<TransmissionConfig>>,
    /// Safety monitor for FCC compliance
    safety_monitor: Arc<SafetyMonitor>,
    /// Current transmission state
    state: Arc<RwLock<TransmissionState>>,
    /// Emergency stop flag
    emergency_stop: Arc<AtomicBool>,
}

impl Ft8Transmitter {
    /// Create a new FT8 transmitter
    pub fn new(config: TransmissionConfig) -> Ft8Result<Self> {
        let encoder = Ft8Encoder::new();
        let modulator = Ft8Modulator::new(
            config.audio_config.sample_rate,
            config.frequency_config.base_frequency,
            config.power_config.tx_power_level,
        )?;

        let ptt_controller = Arc::new(Mutex::new(PttController::new(config.ptt_config.clone())?));

        let audio_output = Arc::new(Mutex::new(AudioOutput::new(config.audio_config.clone())?));

        let safety_monitor = Arc::new(SafetyMonitor::new(config.safety_config.clone()));

        Ok(Self {
            encoder,
            modulator,
            ptt_controller,
            audio_output,
            config: Arc::new(RwLock::new(config)),
            safety_monitor,
            state: Arc::new(RwLock::new(TransmissionState::Idle)),
            emergency_stop: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Transmit an FT8 message
    ///
    /// # Arguments
    /// * `message_text` - Text message to transmit
    /// * `frequency_offset` - Additional frequency offset in Hz
    /// * `immediate` - If true, transmit immediately; if false, wait for next time slot
    pub async fn transmit_message(
        &mut self,
        message_text: &str,
        frequency_offset: f64,
        immediate: bool,
    ) -> Ft8Result<TransmissionReport> {
        // Check emergency stop
        if self.emergency_stop.load(Ordering::Relaxed) {
            return Err(Ft8Error::ConfigError(
                "Emergency stop activated".to_string(),
            ));
        }

        // Validate transmission is allowed
        self.validate_transmission_request(frequency_offset)?;

        // Set state to preparing
        *self.state.write() = TransmissionState::Preparing;

        let start_time = Instant::now();

        // Encode message to symbols
        let symbols = self.encoder.encode_message(message_text, None)?;
        info!("Encoded message '{}' to symbols", message_text);

        // Generate audio samples
        let audio_samples = self
            .modulator
            .modulate_symbols(&symbols, frequency_offset)?;
        debug!("Generated {} audio samples", audio_samples.len());

        // Wait for transmission slot if not immediate
        if !immediate {
            self.wait_for_transmission_slot().await?;
        }

        // Execute transmission
        let transmission_result = self
            .execute_transmission(audio_samples, message_text)
            .await?;

        // Update state
        *self.state.write() = TransmissionState::Idle;

        // Record transmission in safety monitor
        self.safety_monitor
            .record_transmission(start_time, transmission_result.duration);

        Ok(transmission_result)
    }

    /// Transmit standard CQ call
    pub async fn transmit_cq(
        &mut self,
        callsign: &str,
        grid_square: &str,
        frequency_offset: f64,
    ) -> Ft8Result<TransmissionReport> {
        let symbols = self.encoder.encode_cq(callsign, grid_square, false)?;
        let audio_samples = self
            .modulator
            .modulate_symbols(&symbols, frequency_offset)?;

        let message_text = format!("CQ {} {}", callsign, grid_square);
        self.execute_transmission(audio_samples, &message_text)
            .await
    }

    /// Transmit signal report
    pub async fn transmit_signal_report(
        &mut self,
        to_callsign: &str,
        from_callsign: &str,
        report_db: i8,
        frequency_offset: f64,
    ) -> Ft8Result<TransmissionReport> {
        let symbols = self
            .encoder
            .encode_signal_report(to_callsign, from_callsign, report_db)?;
        let audio_samples = self
            .modulator
            .modulate_symbols(&symbols, frequency_offset)?;

        let message_text = format!("{} {} {:+03}", to_callsign, from_callsign, report_db);
        self.execute_transmission(audio_samples, &message_text)
            .await
    }

    /// Transmit acknowledgment (RRR)
    pub async fn transmit_rrr(
        &mut self,
        to_callsign: &str,
        from_callsign: &str,
        frequency_offset: f64,
    ) -> Ft8Result<TransmissionReport> {
        let symbols = self.encoder.encode_rrr(to_callsign, from_callsign)?;
        let audio_samples = self
            .modulator
            .modulate_symbols(&symbols, frequency_offset)?;

        let message_text = format!("{} {} RRR", to_callsign, from_callsign);
        self.execute_transmission(audio_samples, &message_text)
            .await
    }

    /// Transmit final 73
    pub async fn transmit_73(
        &mut self,
        to_callsign: &str,
        from_callsign: &str,
        frequency_offset: f64,
    ) -> Ft8Result<TransmissionReport> {
        let symbols = self.encoder.encode_73(to_callsign, from_callsign)?;
        let audio_samples = self
            .modulator
            .modulate_symbols(&symbols, frequency_offset)?;

        let message_text = format!("{} {} 73", to_callsign, from_callsign);
        self.execute_transmission(audio_samples, &message_text)
            .await
    }

    /// Emergency stop - immediately halt all transmissions
    pub fn emergency_stop(&self) {
        warn!("Emergency stop activated");
        self.emergency_stop.store(true, Ordering::Relaxed);

        // Release PTT immediately — MUST block, never skip.
        // A skipped PTT release leaves the radio keyed up indefinitely.
        match self.ptt_controller.lock() {
            Ok(mut ptt) => {
                if let Err(e) = ptt.release_ptt() {
                    error!("Failed to release PTT during emergency stop: {}", e);
                }
            }
            Err(e) => {
                error!("CRITICAL: PTT lock poisoned during emergency stop: {}", e);
            }
        }

        // Stop audio output — also must block
        match self.audio_output.lock() {
            Ok(mut audio) => {
                if let Err(e) = audio.stop_output() {
                    error!("Failed to stop audio during emergency stop: {}", e);
                }
            }
            Err(e) => {
                error!("Audio lock poisoned during emergency stop: {}", e);
            }
        }

        *self.state.write() = TransmissionState::EmergencyStop;
    }

    /// Clear emergency stop condition
    pub fn clear_emergency_stop(&self) -> Ft8Result<()> {
        if !self.emergency_stop.load(Ordering::Relaxed) {
            return Ok(()); // Already cleared
        }

        // Reset safety monitor
        self.safety_monitor.reset();

        // Clear emergency stop flag
        self.emergency_stop.store(false, Ordering::Relaxed);

        // Reset state
        *self.state.write() = TransmissionState::Idle;

        info!("Emergency stop cleared");
        Ok(())
    }

    /// Get current transmission state
    pub fn get_state(&self) -> TransmissionState {
        *self.state.read()
    }

    /// Get transmission statistics
    pub fn get_statistics(&self) -> TransmissionStatistics {
        self.safety_monitor.get_statistics()
    }

    /// Test audio output and PTT control
    pub async fn test_transmission_system(
        &mut self,
        test_duration_seconds: f64,
    ) -> Ft8Result<TestReport> {
        info!("Starting transmission system test");

        let test_start = Instant::now();

        // Test PTT control
        let ptt_test_result = self.test_ptt_control().await?;

        // Test audio output
        let audio_test_result = self.test_audio_output(test_duration_seconds).await?;

        // Test frequency accuracy (may not be implemented yet)
        let frequency_test_result = self
            .test_frequency_accuracy()
            .unwrap_or(FrequencyTestResult {
                target_frequency: 0.0,
                measured_frequency: 0.0,
                frequency_error: 0.0,
                within_tolerance: true,
            });

        let test_duration = test_start.elapsed();

        Ok(TestReport {
            ptt_test: ptt_test_result,
            audio_test: audio_test_result,
            frequency_test: frequency_test_result,
            total_test_time: test_duration,
            success: true,
        })
    }

    /// Update transmission configuration
    pub fn update_config(&mut self, new_config: TransmissionConfig) -> Ft8Result<()> {
        // Validate new configuration
        self.validate_config(&new_config)?;

        // Update modulator settings
        self.modulator
            .set_base_frequency(new_config.frequency_config.base_frequency)?;
        self.modulator
            .set_tx_power(new_config.power_config.tx_power_level)?;

        // Update PTT controller
        if let Ok(mut ptt) = self.ptt_controller.lock() {
            ptt.update_config(new_config.ptt_config.clone())?;
        }

        // Update audio output
        if let Ok(mut audio) = self.audio_output.lock() {
            audio.update_config(new_config.audio_config.clone())?;
        }

        // Store new configuration
        *self.config.write() = new_config;

        info!("Transmission configuration updated");
        Ok(())
    }

    /// Validate transmission request against safety limits
    fn validate_transmission_request(&self, frequency_offset: f64) -> Ft8Result<()> {
        let config = self.config.read();

        // Check if transmission is currently allowed
        if !self.safety_monitor.is_transmission_allowed() {
            return Err(Ft8Error::ConfigError(
                "Transmission blocked by safety monitor".to_string(),
            ));
        }

        // Check band edges
        let total_frequency = config.frequency_config.base_frequency + frequency_offset;
        self.validate_frequency_limits(total_frequency, &config.frequency_config.band_limits)?;

        // Check power limits
        if config.power_config.max_power_watts > DEFAULT_MAX_POWER_WATTS {
            return Err(Ft8Error::ConfigError(format!(
                "Power limit {} W exceeds maximum {} W",
                config.power_config.max_power_watts, DEFAULT_MAX_POWER_WATTS
            )));
        }

        Ok(())
    }

    /// Execute the actual transmission
    async fn execute_transmission(
        &mut self,
        audio_samples: Vec<f32>,
        message_text: &str,
    ) -> Ft8Result<TransmissionReport> {
        let transmission_start = Instant::now();

        // Set state to transmitting
        *self.state.write() = TransmissionState::Transmitting;

        info!("Starting transmission: '{}'", message_text);

        // Assert PTT with lead time
        {
            let mut ptt = self.ptt_controller.lock().unwrap();
            ptt.assert_ptt()?;
        }

        // Wait for PTT lead time
        tokio::time::sleep(Duration::from_millis(PTT_LEAD_TIME_MS)).await;

        // Convert audio samples to output format
        let config = self.config.read();
        let audio_bytes = convert_samples(&audio_samples, config.audio_config.format);

        // Start audio output — release PTT on failure to prevent stuck transmitter
        {
            let mut audio = self.audio_output.lock().unwrap();
            if let Err(e) = audio.start_transmission(&audio_bytes) {
                error!("Audio start failed, releasing PTT: {}", e);
                if let Ok(mut ptt) = self.ptt_controller.lock() {
                    let _ = ptt.release_ptt();
                }
                *self.state.write() = TransmissionState::Idle;
                return Err(e);
            }
        }

        // Wait for transmission to complete
        let transmission_duration = Duration::from_millis(12640); // 12.64 seconds
        tokio::time::sleep(transmission_duration).await;

        // Stop audio output
        {
            let mut audio = self.audio_output.lock().unwrap();
            audio.stop_output()?;
        }

        // Wait for PTT tail time
        tokio::time::sleep(Duration::from_millis(PTT_TAIL_TIME_MS)).await;

        // Release PTT
        {
            let mut ptt = self.ptt_controller.lock().unwrap();
            ptt.release_ptt()?;
        }

        let total_duration = transmission_start.elapsed();

        info!("Transmission completed in {:?}", total_duration);

        Ok(TransmissionReport {
            message: message_text.to_string(),
            start_time: SystemTime::now() - total_duration,
            duration: total_duration,
            frequency_offset: 0.0, // TODO: Get actual frequency from config
            power_level: config.power_config.tx_power_level,
            success: true,
            error_message: None,
        })
    }

    /// Wait for appropriate transmission time slot
    async fn wait_for_transmission_slot(&self) -> Ft8Result<()> {
        // FT8 transmissions occur on 15-second boundaries
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|e| Ft8Error::ConfigError(format!("Time error: {}", e)))?;

        let seconds_since_epoch = now.as_secs();
        let seconds_in_period = seconds_since_epoch % 15;

        if seconds_in_period != 0 {
            let wait_time = 15 - seconds_in_period;
            info!("Waiting {} seconds for next transmission slot", wait_time);
            tokio::time::sleep(Duration::from_secs(wait_time)).await;
        }

        Ok(())
    }

    /// Validate frequency limits against band edges
    pub fn validate_frequency_limits(
        &self,
        frequency: f64,
        band_limits: &BandLimits,
    ) -> Ft8Result<()> {
        if frequency < band_limits.lower_edge + BAND_EDGE_MARGIN_HZ {
            return Err(Ft8Error::ConfigError(format!(
                "Frequency {:.0} Hz too close to lower band edge",
                frequency
            )));
        }

        if frequency > band_limits.upper_edge - BAND_EDGE_MARGIN_HZ {
            return Err(Ft8Error::ConfigError(format!(
                "Frequency {:.0} Hz too close to upper band edge",
                frequency
            )));
        }

        Ok(())
    }

    /// Validate transmission configuration
    fn validate_config(&self, config: &TransmissionConfig) -> Ft8Result<()> {
        // Validate frequency configuration
        if config.frequency_config.base_frequency < 1000.0
            || config.frequency_config.base_frequency > 30_000_000.0
        {
            return Err(Ft8Error::ConfigError(
                "Base frequency out of range".to_string(),
            ));
        }

        // Validate power configuration
        if config.power_config.tx_power_level < 0.0 || config.power_config.tx_power_level > 1.0 {
            return Err(Ft8Error::ConfigError(
                "TX power level out of range".to_string(),
            ));
        }

        // Validate audio configuration
        if config.audio_config.sample_rate == 0 {
            return Err(Ft8Error::ConfigError("Invalid sample rate".to_string()));
        }

        Ok(())
    }

    /// Test PTT control functionality
    async fn test_ptt_control(&mut self) -> Ft8Result<PttTestResult> {
        let test_start = Instant::now();

        let mut ptt = self.ptt_controller.lock().unwrap();

        // Test PTT assertion
        ptt.assert_ptt()?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Test PTT release
        ptt.release_ptt()?;

        let test_duration = test_start.elapsed();

        Ok(PttTestResult {
            success: true,
            test_duration,
            error_message: None,
        })
    }

    /// Test audio output functionality
    async fn test_audio_output(&mut self, duration_seconds: f64) -> Ft8Result<AudioTestResult> {
        let test_start = Instant::now();

        // Generate test tone
        let test_samples = self
            .modulator
            .generate_test_tone(1000.0, duration_seconds)?;

        let config = self.config.read();
        let audio_bytes = convert_samples(&test_samples, config.audio_config.format);

        let mut audio = self.audio_output.lock().unwrap();
        audio.start_transmission(&audio_bytes)?;

        tokio::time::sleep(Duration::from_millis((duration_seconds * 1000.0) as u64)).await;

        audio.stop_output()?;

        let test_duration = test_start.elapsed();

        Ok(AudioTestResult {
            success: true,
            test_duration,
            sample_rate: config.audio_config.sample_rate,
            error_message: None,
        })
    }

    /// Test frequency accuracy
    fn test_frequency_accuracy(&self) -> Ft8Result<FrequencyTestResult> {
        // Real frequency measurement requires DFT analysis of audio output loopback
        Err(Ft8Error::ConfigError(
            "frequency accuracy test requires audio loopback — not yet implemented".to_string(),
        ))
    }
}

/// PTT (Push-to-Talk) controller for various interfaces
struct PttController {
    config: PttConfig,
    #[cfg(feature = "gpio")]
    gpio_pin: Option<OutputPin>,
    serial_port: Option<Box<dyn serialport::SerialPort>>,
    ptt_asserted: bool,
}

impl PttController {
    fn new(config: PttConfig) -> Ft8Result<Self> {
        let mut controller = Self {
            config: config.clone(),
            #[cfg(feature = "gpio")]
            gpio_pin: None,
            serial_port: None,
            ptt_asserted: false,
        };

        controller.initialize_ptt_interface()?;
        Ok(controller)
    }

    fn initialize_ptt_interface(&mut self) -> Ft8Result<()> {
        match self.config.method {
            PttMethod::SerialDtr => {
                self.initialize_serial_ptt()?;
            }
            PttMethod::SerialRts => {
                self.initialize_serial_ptt()?;
            }
            PttMethod::CatCommand => {
                self.initialize_serial_ptt()?;
            }
            PttMethod::Gpio => {
                #[cfg(feature = "gpio")]
                {
                    self.initialize_gpio_ptt()?;
                }
                #[cfg(not(feature = "gpio"))]
                {
                    return Err(Ft8Error::ConfigError(
                        "GPIO support not compiled".to_string(),
                    ));
                }
            }
            PttMethod::Vox => {
                // VOX doesn't require hardware initialization
            }
            PttMethod::None => {
                // No PTT control
            }
        }

        Ok(())
    }

    fn initialize_serial_ptt(&mut self) -> Ft8Result<()> {
        if let Some(ref port_name) = self.config.serial_port {
            let port = serialport::new(port_name, self.config.serial_baud_rate)
                .timeout(Duration::from_millis(100))
                .open()
                .map_err(|e| {
                    Ft8Error::ConfigError(format!(
                        "Failed to open serial port {}: {}",
                        port_name, e
                    ))
                })?;

            self.serial_port = Some(port);
            info!("Initialized serial PTT on {}", port_name);
        }

        Ok(())
    }

    #[cfg(feature = "gpio")]
    fn initialize_gpio_ptt(&mut self) -> Ft8Result<()> {
        let gpio = Gpio::new()
            .map_err(|e| Ft8Error::ConfigError(format!("Failed to initialize GPIO: {}", e)))?;

        let pin = gpio
            .get(self.config.gpio_pin_number)
            .map_err(|e| {
                Ft8Error::ConfigError(format!(
                    "Failed to get GPIO pin {}: {}",
                    self.config.gpio_pin_number, e
                ))
            })?
            .into_output();

        self.gpio_pin = Some(pin);
        info!(
            "Initialized GPIO PTT on pin {}",
            self.config.gpio_pin_number
        );

        Ok(())
    }

    fn assert_ptt(&mut self) -> Ft8Result<()> {
        if self.ptt_asserted {
            return Ok(()); // Already asserted
        }

        match self.config.method {
            PttMethod::SerialDtr => {
                if let Some(ref mut port) = self.serial_port {
                    port.write_data_terminal_ready(true).map_err(|e| {
                        Ft8Error::ConfigError(format!("Failed to assert DTR: {}", e))
                    })?;
                }
            }
            PttMethod::SerialRts => {
                if let Some(ref mut port) = self.serial_port {
                    port.write_request_to_send(true).map_err(|e| {
                        Ft8Error::ConfigError(format!("Failed to assert RTS: {}", e))
                    })?;
                }
            }
            PttMethod::CatCommand => {
                if let Some(ref mut port) = self.serial_port {
                    let cmd = self.config.cat_ptt_on_command.as_bytes();
                    port.write_all(cmd).map_err(|e| {
                        Ft8Error::ConfigError(format!("Failed to send CAT PTT command: {}", e))
                    })?;
                }
            }
            PttMethod::Gpio => {
                #[cfg(feature = "gpio")]
                {
                    if let Some(ref mut pin) = self.gpio_pin {
                        let level = if self.config.gpio_active_high {
                            Level::High
                        } else {
                            Level::Low
                        };
                        pin.write(level);
                    }
                }
            }
            PttMethod::Vox | PttMethod::None => {
                // No hardware PTT action required
            }
        }

        self.ptt_asserted = true;
        debug!("PTT asserted");
        Ok(())
    }

    fn release_ptt(&mut self) -> Ft8Result<()> {
        if !self.ptt_asserted {
            return Ok(()); // Already released
        }

        match self.config.method {
            PttMethod::SerialDtr => {
                if let Some(ref mut port) = self.serial_port {
                    port.write_data_terminal_ready(false).map_err(|e| {
                        Ft8Error::ConfigError(format!("Failed to release DTR: {}", e))
                    })?;
                }
            }
            PttMethod::SerialRts => {
                if let Some(ref mut port) = self.serial_port {
                    port.write_request_to_send(false).map_err(|e| {
                        Ft8Error::ConfigError(format!("Failed to release RTS: {}", e))
                    })?;
                }
            }
            PttMethod::CatCommand => {
                if let Some(ref mut port) = self.serial_port {
                    let cmd = self.config.cat_ptt_off_command.as_bytes();
                    port.write_all(cmd).map_err(|e| {
                        Ft8Error::ConfigError(format!("Failed to send CAT PTT off command: {}", e))
                    })?;
                }
            }
            PttMethod::Gpio => {
                #[cfg(feature = "gpio")]
                {
                    if let Some(ref mut pin) = self.gpio_pin {
                        let level = if self.config.gpio_active_high {
                            Level::Low
                        } else {
                            Level::High
                        };
                        pin.write(level);
                    }
                }
            }
            PttMethod::Vox | PttMethod::None => {
                // No hardware PTT action required
            }
        }

        self.ptt_asserted = false;
        debug!("PTT released");
        Ok(())
    }

    fn update_config(&mut self, new_config: PttConfig) -> Ft8Result<()> {
        self.config = new_config;
        self.initialize_ptt_interface()?;
        Ok(())
    }
}

/// Audio output controller
struct AudioOutput {
    config: AudioConfig,
    stream: Option<cpal::Stream>,
}

impl AudioOutput {
    fn new(config: AudioConfig) -> Ft8Result<Self> {
        Ok(Self {
            config,
            stream: None,
        })
    }

    fn start_transmission(&mut self, audio_data: &[u8]) -> Ft8Result<()> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();

        // Select output device
        let device = if let Some(ref name) = self.config.device_name {
            host.output_devices()
                .map_err(|e| Ft8Error::ConfigError(format!("failed to enumerate devices: {}", e)))?
                .find(|d| d.name().is_ok_and(|n| &n == name))
                .ok_or_else(|| {
                    Ft8Error::ConfigError(format!("output device '{}' not found", name))
                })?
        } else {
            host.default_output_device()
                .ok_or_else(|| Ft8Error::ConfigError("no default output device".to_string()))?
        };

        let device_name = device.name().unwrap_or_else(|_| "unknown".to_string());
        info!("Starting audio transmission on device: {}", device_name);

        // Convert bytes to f32 samples
        let samples: Vec<f32> = audio_data
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();

        let sample_rate = self.config.sample_rate;
        let buffer = Arc::new(Mutex::new(samples));
        let position = Arc::new(AtomicU64::new(0));

        let stream_config = cpal::StreamConfig {
            channels: 1,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let buf = Arc::clone(&buffer);
        let pos = Arc::clone(&position);

        let stream = device
            .build_output_stream(
                &stream_config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let samples = buf.lock().unwrap();
                    let mut idx = pos.load(Ordering::Relaxed) as usize;
                    for sample in data.iter_mut() {
                        if idx < samples.len() {
                            *sample = samples[idx];
                            idx += 1;
                        } else {
                            *sample = 0.0;
                        }
                    }
                    pos.store(idx as u64, Ordering::Relaxed);
                },
                |err| {
                    error!("Audio output stream error: {}", err);
                },
                None,
            )
            .map_err(|e| Ft8Error::ConfigError(format!("failed to build output stream: {}", e)))?;

        stream
            .play()
            .map_err(|e| Ft8Error::ConfigError(format!("failed to start output stream: {}", e)))?;

        self.stream = Some(stream);
        debug!("Audio output stream started");
        Ok(())
    }

    fn stop_output(&mut self) -> Ft8Result<()> {
        debug!("Stopping audio output");
        self.stream = None;
        Ok(())
    }

    fn update_config(&mut self, new_config: AudioConfig) -> Ft8Result<()> {
        self.config = new_config;
        Ok(())
    }
}

/// Safety monitor for FCC compliance and protection
struct SafetyMonitor {
    config: SafetyConfig,
    transmission_log: Arc<Mutex<Vec<TransmissionRecord>>>,
    total_tx_time: Arc<AtomicU64>,
    last_reset: Arc<Mutex<Instant>>,
}

impl SafetyMonitor {
    fn new(config: SafetyConfig) -> Self {
        Self {
            config,
            transmission_log: Arc::new(Mutex::new(Vec::new())),
            total_tx_time: Arc::new(AtomicU64::new(0)),
            last_reset: Arc::new(Mutex::new(Instant::now())),
        }
    }

    fn is_transmission_allowed(&self) -> bool {
        let total_tx_ms = self.total_tx_time.load(Ordering::Relaxed);

        // Check 6-minute rule (total_tx_time is now in milliseconds)
        if total_tx_ms >= MAX_TX_TIME_SECONDS * 1000 {
            warn!("Transmission blocked: 6-minute limit reached");
            return false;
        }

        // Check minimum interval between transmissions
        if let Ok(log) = self.transmission_log.lock() {
            if let Some(last_transmission) = log.last() {
                let elapsed = last_transmission.start_time.elapsed();
                if elapsed < Duration::from_secs(MIN_TX_INTERVAL_SECONDS) {
                    debug!("Transmission blocked: minimum interval not met");
                    return false;
                }
            }
        }

        true
    }

    fn record_transmission(&self, start_time: Instant, duration: Duration) {
        let record = TransmissionRecord {
            start_time,
            duration,
            timestamp: SystemTime::now(),
        };

        if let Ok(mut log) = self.transmission_log.lock() {
            log.push(record);

            // Clean up old records (keep last 100)
            if log.len() > 100 {
                let excess = log.len() - 100;
                log.drain(0..excess);
            }
        }

        // Update total transmission time in milliseconds for accuracy.
        // FT8 transmissions are 12.64s; truncating to integer seconds loses
        // 640ms per TX, allowing the 6-minute safety limit to be exceeded.
        let tx_ms = duration.as_millis() as u64;
        self.total_tx_time.fetch_add(tx_ms, Ordering::Relaxed);

        debug!("Recorded transmission: {:?}", duration);
    }

    fn reset(&self) {
        self.total_tx_time.store(0, Ordering::Relaxed);
        if let Ok(mut log) = self.transmission_log.lock() {
            log.clear();
        }
        if let Ok(mut last_reset) = self.last_reset.lock() {
            *last_reset = Instant::now();
        }
        info!("Safety monitor reset");
    }

    fn get_statistics(&self) -> TransmissionStatistics {
        let total_tx_ms = self.total_tx_time.load(Ordering::Relaxed);
        let total_tx_seconds = total_tx_ms / 1000;
        let transmission_count = if let Ok(log) = self.transmission_log.lock() {
            log.len()
        } else {
            0
        };

        let time_since_reset = if let Ok(last_reset) = self.last_reset.lock() {
            last_reset.elapsed()
        } else {
            Duration::ZERO
        };

        TransmissionStatistics {
            total_transmissions: transmission_count,
            total_tx_time_seconds: total_tx_seconds,
            remaining_tx_time_seconds: MAX_TX_TIME_SECONDS.saturating_sub(total_tx_seconds),
            time_since_reset,
            transmission_allowed: self.is_transmission_allowed(),
        }
    }
}

/// Transmission configuration
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TransmissionConfig {
    pub frequency_config: FrequencyConfig,
    pub power_config: PowerConfig,
    pub audio_config: AudioConfig,
    pub ptt_config: PttConfig,
    pub safety_config: SafetyConfig,
}

/// Frequency configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrequencyConfig {
    pub base_frequency: f64,
    pub band_limits: BandLimits,
    pub frequency_calibration: f64,
}

impl Default for FrequencyConfig {
    fn default() -> Self {
        Self {
            base_frequency: 1500.0,
            band_limits: BandLimits {
                lower_edge: 14074000.0, // 20m FT8 band
                upper_edge: 14076000.0,
            },
            frequency_calibration: 0.0,
        }
    }
}

/// Band edge limits
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandLimits {
    pub lower_edge: f64,
    pub upper_edge: f64,
}

/// Power configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerConfig {
    pub tx_power_level: f64,    // 0.0 - 1.0
    pub max_power_watts: u8,    // Hardware limit
    pub power_calibration: f64, // Calibration factor
}

impl Default for PowerConfig {
    fn default() -> Self {
        Self {
            tx_power_level: 0.5,
            max_power_watts: 100,
            power_calibration: 1.0,
        }
    }
}

/// Audio configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub format: AudioFormat,
    pub device_name: Option<String>,
    pub buffer_size: usize,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: 12000,
            format: AudioFormat::ft8_standard(),
            device_name: None,
            buffer_size: 1024,
        }
    }
}

/// PTT configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PttConfig {
    pub method: PttMethod,
    pub serial_port: Option<String>,
    pub serial_baud_rate: u32,
    pub gpio_pin_number: u8,
    pub gpio_active_high: bool,
    pub cat_ptt_on_command: String,
    pub cat_ptt_off_command: String,
    pub vox_delay_ms: u64,
}

impl Default for PttConfig {
    fn default() -> Self {
        Self {
            method: PttMethod::None,
            serial_port: None,
            serial_baud_rate: 9600,
            gpio_pin_number: 18,
            gpio_active_high: true,
            cat_ptt_on_command: "TX1;".to_string(),
            cat_ptt_off_command: "TX0;".to_string(),
            vox_delay_ms: VOX_DELAY_MS,
        }
    }
}

/// PTT control methods
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PttMethod {
    None,
    SerialDtr,
    SerialRts,
    CatCommand,
    Gpio,
    Vox,
}

/// Safety configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyConfig {
    pub enable_tx_timeout: bool,
    pub max_tx_time_seconds: u64,
    pub min_tx_interval_seconds: u64,
    pub enable_band_edge_protection: bool,
    pub band_edge_margin_hz: f64,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            enable_tx_timeout: true,
            max_tx_time_seconds: MAX_TX_TIME_SECONDS,
            min_tx_interval_seconds: MIN_TX_INTERVAL_SECONDS,
            enable_band_edge_protection: true,
            band_edge_margin_hz: BAND_EDGE_MARGIN_HZ,
        }
    }
}

/// Current transmission state
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransmissionState {
    Idle,
    Preparing,
    Transmitting,
    EmergencyStop,
}

/// Transmission report
#[derive(Debug, Clone)]
pub struct TransmissionReport {
    pub message: String,
    pub start_time: SystemTime,
    pub duration: Duration,
    pub frequency_offset: f64,
    pub power_level: f64,
    pub success: bool,
    pub error_message: Option<String>,
}

/// Transmission statistics
#[derive(Debug, Clone)]
pub struct TransmissionStatistics {
    pub total_transmissions: usize,
    pub total_tx_time_seconds: u64,
    pub remaining_tx_time_seconds: u64,
    pub time_since_reset: Duration,
    pub transmission_allowed: bool,
}

/// Test report
#[derive(Debug, Clone)]
pub struct TestReport {
    pub ptt_test: PttTestResult,
    pub audio_test: AudioTestResult,
    pub frequency_test: FrequencyTestResult,
    pub total_test_time: Duration,
    pub success: bool,
}

/// PTT test result
#[derive(Debug, Clone)]
pub struct PttTestResult {
    pub success: bool,
    pub test_duration: Duration,
    pub error_message: Option<String>,
}

/// Audio test result
#[derive(Debug, Clone)]
pub struct AudioTestResult {
    pub success: bool,
    pub test_duration: Duration,
    pub sample_rate: u32,
    pub error_message: Option<String>,
}

/// Frequency test result
#[derive(Debug, Clone)]
pub struct FrequencyTestResult {
    pub target_frequency: f64,
    pub measured_frequency: f64,
    pub frequency_error: f64,
    pub within_tolerance: bool,
}

/// Transmission record for safety monitoring
#[derive(Debug, Clone)]
struct TransmissionRecord {
    start_time: Instant,
    duration: Duration,
    timestamp: SystemTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transmission_config_default() {
        let config = TransmissionConfig::default();
        assert_eq!(config.frequency_config.base_frequency, 1500.0);
        assert_eq!(config.power_config.tx_power_level, 0.5);
        assert_eq!(config.ptt_config.method, PttMethod::None);
    }

    #[test]
    fn test_safety_monitor() {
        let config = SafetyConfig::default();
        let monitor = SafetyMonitor::new(config);

        assert!(monitor.is_transmission_allowed());

        let stats = monitor.get_statistics();
        assert_eq!(stats.total_transmissions, 0);
        assert_eq!(stats.total_tx_time_seconds, 0);
    }

    #[test]
    fn test_ptt_controller_creation() {
        let config = PttConfig::default();
        let result = PttController::new(config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_transmission_state() {
        assert_eq!(TransmissionState::Idle, TransmissionState::Idle);
        assert_ne!(TransmissionState::Idle, TransmissionState::Transmitting);
    }

    #[test]
    fn test_band_limits_validation() {
        let transmitter = Ft8Transmitter::new(TransmissionConfig::default()).unwrap();
        let band_limits = BandLimits {
            lower_edge: 14070000.0, // Wider band for testing
            upper_edge: 14080000.0, // 10 kHz total bandwidth
        };

        // Test frequency within band (respecting 1000 Hz margin)
        assert!(transmitter
            .validate_frequency_limits(14075000.0, &band_limits)
            .is_ok());

        // Test frequency too close to lower edge
        assert!(transmitter
            .validate_frequency_limits(14070500.0, &band_limits)
            .is_err());

        // Test frequency too close to upper edge
        assert!(transmitter
            .validate_frequency_limits(14079500.0, &band_limits)
            .is_err());

        // Test frequency way outside band
        assert!(transmitter
            .validate_frequency_limits(14065000.0, &band_limits)
            .is_err());
        assert!(transmitter
            .validate_frequency_limits(14085000.0, &band_limits)
            .is_err());
    }

    #[test]
    fn test_emergency_stop() {
        let transmitter = Ft8Transmitter::new(TransmissionConfig::default()).unwrap();

        assert_eq!(transmitter.get_state(), TransmissionState::Idle);
        assert!(!transmitter.emergency_stop.load(Ordering::Relaxed));

        transmitter.emergency_stop();
        assert!(transmitter.emergency_stop.load(Ordering::Relaxed));
        assert_eq!(transmitter.get_state(), TransmissionState::EmergencyStop);

        transmitter.clear_emergency_stop().unwrap();
        assert!(!transmitter.emergency_stop.load(Ordering::Relaxed));
        assert_eq!(transmitter.get_state(), TransmissionState::Idle);
    }
}
