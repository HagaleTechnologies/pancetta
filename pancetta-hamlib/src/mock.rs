//! Mock rig implementation for testing and development
//!
//! This module provides a complete mock implementation of a transceiver
//! that simulates realistic behavior without requiring actual hardware.

use crate::models::{Band, Mode, RigCapabilities, Vfo};
use crate::rig::{ConnectionState, PttState, RigControl, RigStatus};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, info, instrument};

/// Mock rig configuration
#[derive(Debug, Clone)]
pub struct MockRigConfig {
    /// Simulated connection delay in milliseconds
    pub connection_delay_ms: u64,
    /// Simulated operation delay in milliseconds
    pub operation_delay_ms: u64,
    /// Failure rate (0.0 to 1.0)
    pub failure_rate: f32,
    /// Whether to simulate S-meter readings
    pub simulate_s_meter: bool,
    /// S-meter noise level
    pub s_meter_noise: i32,
    /// Whether to simulate SWR readings
    pub simulate_swr: bool,
    /// Base SWR value
    pub base_swr: f32,
    /// Supported frequency range
    pub frequency_range: (u64, u64),
    /// Memory channel count
    pub memory_channels: u32,
}

impl Default for MockRigConfig {
    fn default() -> Self {
        Self {
            connection_delay_ms: 100,
            operation_delay_ms: 10,
            failure_rate: 0.0,
            simulate_s_meter: true,
            s_meter_noise: 10,
            simulate_swr: true,
            base_swr: 1.2,
            frequency_range: (1_800_000, 450_000_000),
            memory_channels: 100,
        }
    }
}

/// Mock rig state
#[derive(Debug, Clone)]
struct MockRigState {
    /// Connection state
    connected: bool,
    /// Current frequency for each VFO
    frequencies: HashMap<Vfo, u64>,
    /// Current mode for each VFO
    modes: HashMap<Vfo, (Mode, i32)>,
    /// Current VFO
    current_vfo: Vfo,
    /// PTT state for each VFO
    ptt_states: HashMap<Vfo, PttState>,
    /// Power level (0.0-1.0)
    power_level: f32,
    /// Memory channels
    memory_channels: HashMap<i32, (u64, Mode, i32)>,
    /// Current memory channel for each VFO
    current_memory: HashMap<Vfo, i32>,
    /// Antenna position
    #[allow(dead_code)]
    antenna: u32,
    /// Scanning state
    scanning: bool,
    /// Last operation time
    last_operation: Instant,
}

impl Default for MockRigState {
    fn default() -> Self {
        let mut frequencies = HashMap::new();
        frequencies.insert(Vfo::A, 14_200_000);
        frequencies.insert(Vfo::B, 14_074_000);
        frequencies.insert(Vfo::Current, 14_200_000);

        let mut modes = HashMap::new();
        modes.insert(Vfo::A, (Mode::USB, 2400));
        modes.insert(Vfo::B, (Mode::FT8, 3000));
        modes.insert(Vfo::Current, (Mode::USB, 2400));

        let mut ptt_states = HashMap::new();
        ptt_states.insert(Vfo::A, PttState::Off);
        ptt_states.insert(Vfo::B, PttState::Off);
        ptt_states.insert(Vfo::Current, PttState::Off);

        Self {
            connected: false,
            frequencies,
            modes,
            current_vfo: Vfo::A,
            ptt_states,
            power_level: 0.5,
            memory_channels: HashMap::new(),
            current_memory: HashMap::new(),
            antenna: 1,
            scanning: false,
            last_operation: Instant::now(),
        }
    }
}

/// Mock rig implementation
pub struct MockRig {
    /// Mock configuration
    config: MockRigConfig,
    /// Rig capabilities
    capabilities: RigCapabilities,
    /// Current state
    state: RwLock<MockRigState>,
    /// Operation counter for simulation
    operation_count: AtomicU32,
    /// Random seed for consistent behavior
    random_seed: AtomicU32,
}

impl MockRig {
    /// Create new mock rig
    pub fn new(config: MockRigConfig) -> Self {
        // Create realistic capabilities for a modern transceiver
        let capabilities = RigCapabilities {
            modes: vec![
                Mode::LSB,
                Mode::USB,
                Mode::CW,
                Mode::FM,
                Mode::AM,
                Mode::RTTY,
                Mode::PACKET,
                Mode::FT8,
                Mode::FT4,
            ],
            frequency_ranges: vec![config.frequency_range],
            has_dual_vfo: true,
            has_memory: true,
            memory_channels: Some(config.memory_channels),
            has_scanning: true,
            has_smeter: config.simulate_s_meter,
            has_swr: config.simulate_swr,
            has_power_control: true,
            has_antenna_switch: true,
            has_if_shift: true,
            has_noise_reduction: true,
            bands: vec![
                Band::Band160m,
                Band::Band80m,
                Band::Band60m,
                Band::Band40m,
                Band::Band30m,
                Band::Band20m,
                Band::Band17m,
                Band::Band15m,
                Band::Band12m,
                Band::Band10m,
                Band::Band6m,
                Band::Band2m,
                Band::Band70cm,
            ],
            default_baud_rate: 38400,
            default_timeout: 2000,
        };

        // Initialize some memory channels
        let mut state = MockRigState::default();
        state
            .memory_channels
            .insert(1, (14_200_000, Mode::USB, 2400));
        state
            .memory_channels
            .insert(2, (14_074_000, Mode::FT8, 3000));
        state
            .memory_channels
            .insert(3, (7_200_000, Mode::LSB, 2400));
        state
            .memory_channels
            .insert(4, (7_074_000, Mode::FT8, 3000));
        state
            .memory_channels
            .insert(5, (21_200_000, Mode::USB, 2400));

        Self {
            config,
            capabilities,
            state: RwLock::new(state),
            operation_count: AtomicU32::new(0),
            random_seed: AtomicU32::new(12345),
        }
    }

    /// Create mock rig with default configuration
    pub fn default() -> Self {
        Self::new(MockRigConfig::default())
    }

    /// Simulate operation delay
    async fn simulate_delay(&self) {
        if self.config.operation_delay_ms > 0 {
            sleep(Duration::from_millis(self.config.operation_delay_ms)).await;
        }
    }

    /// Simulate potential failure
    fn simulate_failure(&self, operation: &str) -> Result<()> {
        if self.config.failure_rate <= 0.0 {
            return Ok(());
        }

        let count = self.operation_count.fetch_add(1, Ordering::Relaxed);
        let seed = self.random_seed.load(Ordering::Relaxed);

        // Simple pseudo-random number generator
        let random = ((count.wrapping_mul(1103515245).wrapping_add(seed)) >> 16) as f32 / 65536.0;

        if random < self.config.failure_rate {
            return Err(anyhow!("Simulated failure for operation: {}", operation));
        }

        Ok(())
    }

    /// Simulate S-meter reading based on frequency and other factors
    fn simulate_s_meter(&self, frequency: u64) -> i32 {
        if !self.config.simulate_s_meter {
            return -120; // No signal
        }

        let count = self.operation_count.load(Ordering::Relaxed);
        let seed = self.random_seed.load(Ordering::Relaxed);

        // Base signal level varies by band
        let base_level = match Band::from_frequency(frequency) {
            Some(Band::Band20m) => -50, // Good propagation
            Some(Band::Band40m) => -60, // Medium propagation
            Some(Band::Band80m) => -70, // Poor propagation during day
            Some(Band::Band15m) => -80, // Variable propagation
            Some(Band::Band10m) => -90, // Poor propagation
            _ => -100,                  // Other bands
        };

        // Add noise and variation
        let noise = ((count.wrapping_mul(1103515245).wrapping_add(seed)) as i32
            % (self.config.s_meter_noise * 2))
            - self.config.s_meter_noise;

        // Occasional strong signals
        let strong_signal = if (count % 100) == 0 { 30 } else { 0 };

        base_level + noise + strong_signal
    }

    /// Simulate SWR reading based on frequency and other factors
    fn simulate_swr(&self, frequency: u64) -> f32 {
        if !self.config.simulate_swr {
            return 1.0;
        }

        let count = self.operation_count.load(Ordering::Relaxed);

        // SWR varies slightly with frequency
        let freq_variation = ((frequency as f32 / 1_000_000.0) % 10.0) * 0.01;

        // Add some random variation
        let random_variation = ((count % 100) as f32 / 1000.0) - 0.05;

        (self.config.base_swr + freq_variation + random_variation)
            .max(1.0)
            .min(3.0)
    }

    /// Update last operation time and increment operation counter
    fn update_operation_time(&self) {
        self.operation_count.fetch_add(1, Ordering::Relaxed);
        let mut state = self.state.write();
        state.last_operation = Instant::now();
    }

    /// Get rig capabilities
    pub fn capabilities(&self) -> &RigCapabilities {
        &self.capabilities
    }

    /// Update mock configuration
    pub fn update_config(&mut self, config: MockRigConfig) {
        self.config = config;
    }

    /// Set random seed for consistent behavior in tests
    pub fn set_random_seed(&self, seed: u32) {
        self.random_seed.store(seed, Ordering::Relaxed);
    }

    /// Reset operation counter
    pub fn reset_operation_count(&self) {
        self.operation_count.store(0, Ordering::Relaxed);
    }

    /// Get current operation count
    pub fn get_operation_count(&self) -> u32 {
        self.operation_count.load(Ordering::Relaxed)
    }

    /// Validate frequency range
    fn validate_frequency(&self, frequency: u64) -> Result<()> {
        let (min_freq, max_freq) = self.config.frequency_range;
        if frequency < min_freq || frequency > max_freq {
            return Err(anyhow!(
                "Frequency {} Hz is outside valid range ({}-{} Hz)",
                frequency,
                min_freq,
                max_freq
            ));
        }
        Ok(())
    }

    /// Validate memory channel
    fn validate_memory_channel(&self, channel: i32) -> Result<()> {
        if channel < 0 || channel >= self.config.memory_channels as i32 {
            return Err(anyhow!(
                "Memory channel {} is outside valid range (0-{})",
                channel,
                self.config.memory_channels - 1
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl RigControl for MockRig {
    #[instrument(skip(self))]
    async fn connect(&self) -> Result<()> {
        self.simulate_failure("connect")?;

        info!("Mock rig connecting...");

        // Simulate connection delay
        if self.config.connection_delay_ms > 0 {
            sleep(Duration::from_millis(self.config.connection_delay_ms)).await;
        }

        {
            let mut state = self.state.write();
            state.connected = true;
        }

        self.update_operation_time();
        info!("Mock rig connected successfully");
        Ok(())
    }

    #[instrument(skip(self))]
    async fn disconnect(&self) -> Result<()> {
        info!("Mock rig disconnecting...");

        {
            let mut state = self.state.write();
            state.connected = false;
        }

        self.update_operation_time();
        info!("Mock rig disconnected");
        Ok(())
    }

    async fn get_status(&self) -> Result<RigStatus> {
        let state = self.state.read();

        let connection_state = if state.connected {
            ConnectionState::Connected
        } else {
            ConnectionState::Disconnected
        };

        Ok(RigStatus {
            connection_state,
            frequency: state.frequencies.get(&state.current_vfo).copied(),
            mode: state.modes.get(&state.current_vfo).map(|(mode, _)| *mode),
            width: state.modes.get(&state.current_vfo).map(|(_, width)| *width),
            vfo: Some(state.current_vfo),
            ptt: state.ptt_states.get(&state.current_vfo).copied(),
            power_level: Some(state.power_level),
            s_meter: state
                .frequencies
                .get(&state.current_vfo)
                .map(|&freq| self.simulate_s_meter(freq)),
            swr: state
                .frequencies
                .get(&state.current_vfo)
                .map(|&freq| self.simulate_swr(freq)),
            memory_channel: state.current_memory.get(&state.current_vfo).copied(),
            last_update: state.last_operation,
            last_error: None,
        })
    }

    #[instrument(skip(self))]
    async fn set_frequency(&self, vfo: Vfo, freq: u64) -> Result<()> {
        self.simulate_failure("set_frequency")?;
        self.simulate_delay().await;
        self.validate_frequency(freq)?;

        {
            let state = self.state.read();
            if !state.connected {
                return Err(anyhow!("Mock rig not connected"));
            }
        }

        {
            let mut state = self.state.write();
            let current_vfo = state.current_vfo;
            state.frequencies.insert(vfo, freq);
            if vfo == Vfo::Current {
                state.frequencies.insert(current_vfo, freq);
            }
        }

        self.update_operation_time();
        debug!("Mock rig set frequency to {} Hz on VFO {:?}", freq, vfo);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_frequency(&self, vfo: Vfo) -> Result<u64> {
        self.simulate_failure("get_frequency")?;
        self.simulate_delay().await;

        let frequency = {
            let state = self.state.read();
            if !state.connected {
                return Err(anyhow!("Mock rig not connected"));
            }

            let actual_vfo = if vfo == Vfo::Current {
                state.current_vfo
            } else {
                vfo
            };

            state
                .frequencies
                .get(&actual_vfo)
                .copied()
                .ok_or_else(|| anyhow!("No frequency set for VFO {:?}", actual_vfo))?
        };

        self.update_operation_time();
        debug!(
            "Mock rig get frequency: {} Hz from VFO {:?}",
            frequency, vfo
        );
        Ok(frequency)
    }

    #[instrument(skip(self))]
    async fn set_mode(&self, vfo: Vfo, mode: Mode, width: Option<i32>) -> Result<()> {
        self.simulate_failure("set_mode")?;
        self.simulate_delay().await;

        {
            let state = self.state.read();
            if !state.connected {
                return Err(anyhow!("Mock rig not connected"));
            }
        }

        use crate::models::ModeExt;
        let actual_width = width.unwrap_or_else(|| mode.default_width().unwrap_or(2400));

        {
            let mut state = self.state.write();
            let current_vfo = state.current_vfo;
            state.modes.insert(vfo, (mode, actual_width));
            if vfo == Vfo::Current {
                state.modes.insert(current_vfo, (mode, actual_width));
            }
        }

        self.update_operation_time();
        debug!(
            "Mock rig set mode to {:?} with width {} Hz on VFO {:?}",
            mode, actual_width, vfo
        );
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_mode(&self, vfo: Vfo) -> Result<(Mode, i32)> {
        self.simulate_failure("get_mode")?;
        self.simulate_delay().await;

        let (mode, width) = {
            let state = self.state.read();
            if !state.connected {
                return Err(anyhow!("Mock rig not connected"));
            }

            let actual_vfo = if vfo == Vfo::Current {
                state.current_vfo
            } else {
                vfo
            };

            state
                .modes
                .get(&actual_vfo)
                .copied()
                .ok_or_else(|| anyhow!("No mode set for VFO {:?}", actual_vfo))?
        };

        self.update_operation_time();
        debug!(
            "Mock rig get mode: {:?} with width {} Hz from VFO {:?}",
            mode, width, vfo
        );
        Ok((mode, width))
    }

    #[instrument(skip(self))]
    async fn set_vfo(&self, vfo: Vfo) -> Result<()> {
        self.simulate_failure("set_vfo")?;
        self.simulate_delay().await;

        {
            let state = self.state.read();
            if !state.connected {
                return Err(anyhow!("Mock rig not connected"));
            }
        }

        if vfo == Vfo::Current {
            return Err(anyhow!("Cannot set VFO to Current"));
        }

        {
            let mut state = self.state.write();
            state.current_vfo = vfo;
        }

        self.update_operation_time();
        debug!("Mock rig set VFO to {:?}", vfo);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_vfo(&self) -> Result<Vfo> {
        self.simulate_failure("get_vfo")?;
        self.simulate_delay().await;

        let vfo = {
            let state = self.state.read();
            if !state.connected {
                return Err(anyhow!("Mock rig not connected"));
            }
            state.current_vfo
        };

        self.update_operation_time();
        debug!("Mock rig get VFO: {:?}", vfo);
        Ok(vfo)
    }

    #[instrument(skip(self))]
    async fn set_ptt(&self, vfo: Vfo, ptt: PttState) -> Result<()> {
        self.simulate_failure("set_ptt")?;
        self.simulate_delay().await;

        {
            let state = self.state.read();
            if !state.connected {
                return Err(anyhow!("Mock rig not connected"));
            }
        }

        {
            let mut state = self.state.write();
            let current_vfo = state.current_vfo;
            state.ptt_states.insert(vfo, ptt);
            if vfo == Vfo::Current {
                state.ptt_states.insert(current_vfo, ptt);
            }
        }

        self.update_operation_time();
        debug!("Mock rig set PTT to {:?} on VFO {:?}", ptt, vfo);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_ptt(&self, vfo: Vfo) -> Result<PttState> {
        self.simulate_failure("get_ptt")?;
        self.simulate_delay().await;

        let ptt = {
            let state = self.state.read();
            if !state.connected {
                return Err(anyhow!("Mock rig not connected"));
            }

            let actual_vfo = if vfo == Vfo::Current {
                state.current_vfo
            } else {
                vfo
            };

            state
                .ptt_states
                .get(&actual_vfo)
                .copied()
                .unwrap_or(PttState::Off)
        };

        self.update_operation_time();
        debug!("Mock rig get PTT: {:?} from VFO {:?}", ptt, vfo);
        Ok(ptt)
    }

    #[instrument(skip(self))]
    async fn set_power_level(&self, level: f32) -> Result<()> {
        self.simulate_failure("set_power_level")?;
        self.simulate_delay().await;

        if !(0.0..=1.0).contains(&level) {
            return Err(anyhow!("Power level must be between 0.0 and 1.0"));
        }

        {
            let state = self.state.read();
            if !state.connected {
                return Err(anyhow!("Mock rig not connected"));
            }
        }

        {
            let mut state = self.state.write();
            state.power_level = level;
        }

        self.update_operation_time();
        debug!("Mock rig set power level to {:.1}%", level * 100.0);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_power_level(&self) -> Result<f32> {
        self.simulate_failure("get_power_level")?;
        self.simulate_delay().await;

        let level = {
            let state = self.state.read();
            if !state.connected {
                return Err(anyhow!("Mock rig not connected"));
            }
            state.power_level
        };

        self.update_operation_time();
        debug!("Mock rig get power level: {:.1}%", level * 100.0);
        Ok(level)
    }

    #[instrument(skip(self))]
    async fn get_s_meter(&self) -> Result<i32> {
        self.simulate_failure("get_s_meter")?;
        self.simulate_delay().await;

        let frequency = {
            let state = self.state.read();
            if !state.connected {
                return Err(anyhow!("Mock rig not connected"));
            }
            state
                .frequencies
                .get(&state.current_vfo)
                .copied()
                .unwrap_or(14_200_000)
        };

        let s_meter = self.simulate_s_meter(frequency);
        self.update_operation_time();
        debug!("Mock rig get S-meter: {} dBm", s_meter);
        Ok(s_meter)
    }

    #[instrument(skip(self))]
    async fn get_swr(&self) -> Result<f32> {
        self.simulate_failure("get_swr")?;
        self.simulate_delay().await;

        let frequency = {
            let state = self.state.read();
            if !state.connected {
                return Err(anyhow!("Mock rig not connected"));
            }
            state
                .frequencies
                .get(&state.current_vfo)
                .copied()
                .unwrap_or(14_200_000)
        };

        let swr = self.simulate_swr(frequency);
        self.update_operation_time();
        debug!("Mock rig get SWR: {:.2}:1", swr);
        Ok(swr)
    }

    #[instrument(skip(self))]
    async fn set_memory_channel(&self, vfo: Vfo, channel: i32) -> Result<()> {
        self.simulate_failure("set_memory_channel")?;
        self.simulate_delay().await;
        self.validate_memory_channel(channel)?;

        {
            let state = self.state.read();
            if !state.connected {
                return Err(anyhow!("Mock rig not connected"));
            }
        }

        // Load memory channel settings
        let (frequency, mode, width) = {
            let state = self.state.read();
            state
                .memory_channels
                .get(&channel)
                .copied()
                .ok_or_else(|| anyhow!("Memory channel {} is empty", channel))?
        };

        // Apply memory channel settings
        {
            let mut state = self.state.write();
            let current_vfo = state.current_vfo;
            state.frequencies.insert(vfo, frequency);
            state.modes.insert(vfo, (mode, width));
            state.current_memory.insert(vfo, channel);

            if vfo == Vfo::Current {
                state.frequencies.insert(current_vfo, frequency);
                state.modes.insert(current_vfo, (mode, width));
                state.current_memory.insert(current_vfo, channel);
            }
        }

        self.update_operation_time();
        debug!("Mock rig set memory channel {} on VFO {:?}", channel, vfo);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_memory_channel(&self, vfo: Vfo) -> Result<i32> {
        self.simulate_failure("get_memory_channel")?;
        self.simulate_delay().await;

        let channel = {
            let state = self.state.read();
            if !state.connected {
                return Err(anyhow!("Mock rig not connected"));
            }

            let actual_vfo = if vfo == Vfo::Current {
                state.current_vfo
            } else {
                vfo
            };

            state
                .current_memory
                .get(&actual_vfo)
                .copied()
                .ok_or_else(|| anyhow!("No memory channel selected for VFO {:?}", actual_vfo))?
        };

        self.update_operation_time();
        debug!(
            "Mock rig get memory channel: {} from VFO {:?}",
            channel, vfo
        );
        Ok(channel)
    }

    #[instrument(skip(self))]
    async fn set_scan(&self, vfo: Vfo, enable: bool) -> Result<()> {
        self.simulate_failure("set_scan")?;
        self.simulate_delay().await;

        {
            let state = self.state.read();
            if !state.connected {
                return Err(anyhow!("Mock rig not connected"));
            }
        }

        {
            let mut state = self.state.write();
            state.scanning = enable;
        }

        self.update_operation_time();
        debug!("Mock rig set scan: {} on VFO {:?}", enable, vfo);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_info(&self) -> Result<String> {
        self.simulate_failure("get_info")?;
        self.simulate_delay().await;

        {
            let state = self.state.read();
            if !state.connected {
                return Err(anyhow!("Mock rig not connected"));
            }
        }

        let info = format!(
            "Mock Transceiver v1.0\nFreq: {:.3} MHz - {:.3} MHz\nMemory: {} channels\nOperations: {}",
            self.config.frequency_range.0 as f64 / 1_000_000.0,
            self.config.frequency_range.1 as f64 / 1_000_000.0,
            self.config.memory_channels,
            self.get_operation_count()
        );

        self.update_operation_time();
        debug!("Mock rig get info");
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_rig_connection() {
        let rig = MockRig::default();

        // Test connection
        let result = rig.connect().await;
        assert!(result.is_ok());

        let status = rig.get_status().await.unwrap();
        assert_eq!(status.connection_state, ConnectionState::Connected);

        // Test disconnection
        let result = rig.disconnect().await;
        assert!(result.is_ok());

        let status = rig.get_status().await.unwrap();
        assert_eq!(status.connection_state, ConnectionState::Disconnected);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_rig_frequency_control() {
        let rig = MockRig::default();
        rig.connect().await.unwrap();

        // Test setting frequency
        let test_freq = 14_200_000;
        rig.set_frequency(Vfo::A, test_freq).await.unwrap();

        let freq = rig.get_frequency(Vfo::A).await.unwrap();
        assert_eq!(freq, test_freq);

        // Test invalid frequency
        let invalid_freq = 1_000_000_000; // 1 GHz - outside range
        let result = rig.set_frequency(Vfo::A, invalid_freq).await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_rig_mode_control() {
        let rig = MockRig::default();
        rig.connect().await.unwrap();

        // Test setting mode
        rig.set_mode(Vfo::A, Mode::CW, Some(500)).await.unwrap();

        let (mode, width) = rig.get_mode(Vfo::A).await.unwrap();
        assert_eq!(mode, Mode::CW);
        assert_eq!(width, 500);

        // Test mode with default width
        rig.set_mode(Vfo::A, Mode::USB, None).await.unwrap();
        let (mode, width) = rig.get_mode(Vfo::A).await.unwrap();
        assert_eq!(mode, Mode::USB);
        assert_eq!(width, 2400); // Default USB width
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_rig_vfo_control() {
        let rig = MockRig::default();
        rig.connect().await.unwrap();

        // Test VFO switching
        rig.set_vfo(Vfo::B).await.unwrap();
        let vfo = rig.get_vfo().await.unwrap();
        assert_eq!(vfo, Vfo::B);

        // Test setting frequency on specific VFO
        rig.set_frequency(Vfo::B, 21_200_000).await.unwrap();
        let freq = rig.get_frequency(Vfo::B).await.unwrap();
        assert_eq!(freq, 21_200_000);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_rig_memory_channels() {
        let rig = MockRig::default();
        rig.connect().await.unwrap();

        // Test loading memory channel
        rig.set_memory_channel(Vfo::A, 1).await.unwrap();
        let channel = rig.get_memory_channel(Vfo::A).await.unwrap();
        assert_eq!(channel, 1);

        // Verify frequency was loaded from memory
        let freq = rig.get_frequency(Vfo::A).await.unwrap();
        assert_eq!(freq, 14_200_000); // From pre-loaded memory

        // Test invalid memory channel
        let result = rig.set_memory_channel(Vfo::A, 999).await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_rig_ptt_control() {
        let rig = MockRig::default();
        rig.connect().await.unwrap();

        // Test PTT on
        rig.set_ptt(Vfo::A, PttState::On).await.unwrap();
        let ptt = rig.get_ptt(Vfo::A).await.unwrap();
        assert_eq!(ptt, PttState::On);

        // Test PTT off
        rig.set_ptt(Vfo::A, PttState::Off).await.unwrap();
        let ptt = rig.get_ptt(Vfo::A).await.unwrap();
        assert_eq!(ptt, PttState::Off);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_rig_power_control() {
        let rig = MockRig::default();
        rig.connect().await.unwrap();

        // Test setting power level
        rig.set_power_level(0.75).await.unwrap();
        let power = rig.get_power_level().await.unwrap();
        assert!((power - 0.75).abs() < 0.01);

        // Test invalid power level
        let result = rig.set_power_level(1.5).await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_rig_monitoring() {
        let rig = MockRig::default();
        rig.connect().await.unwrap();
        rig.set_frequency(Vfo::A, 14_200_000).await.unwrap();

        // Test S-meter reading
        let s_meter = rig.get_s_meter().await.unwrap();
        assert!(s_meter >= -120);
        assert!(s_meter <= 60);

        // Test SWR reading
        let swr = rig.get_swr().await.unwrap();
        assert!(swr >= 1.0);
        assert!(swr <= 3.0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_rig_operation_delay() {
        let mut config = MockRigConfig::default();
        config.operation_delay_ms = 50;
        let rig = MockRig::new(config);
        rig.connect().await.unwrap();

        let start = Instant::now();
        rig.get_frequency(Vfo::A).await.unwrap();
        let duration = start.elapsed();

        // Should take at least the configured delay
        assert!(duration >= Duration::from_millis(50));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_rig_failure_simulation() {
        let mut config = MockRigConfig::default();
        config.failure_rate = 1.0; // 100% failure rate
        let rig = MockRig::new(config);

        // Connection should fail
        let result = rig.connect().await;
        assert!(result.is_err());

        // Reset for partial testing
        let mut config = MockRigConfig::default();
        config.failure_rate = 0.0; // 0% failure rate
        let rig = MockRig::new(config);
        rig.connect().await.unwrap();

        // Operations should succeed
        let result = rig.get_frequency(Vfo::A).await;
        assert!(result.is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_rig_info() {
        let rig = MockRig::default();
        rig.connect().await.unwrap();

        let info = rig.get_info().await.unwrap();
        assert!(info.contains("Mock Transceiver"));
        assert!(info.contains("MHz"));
        assert!(info.contains("channels"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_rig_operation_count() {
        let rig = MockRig::default();
        rig.connect().await.unwrap();

        let initial_count = rig.get_operation_count();

        // Perform some operations
        rig.get_frequency(Vfo::A).await.unwrap();
        rig.get_mode(Vfo::A).await.unwrap();
        rig.get_s_meter().await.unwrap();

        let final_count = rig.get_operation_count();
        assert!(final_count > initial_count);

        // Reset count
        rig.reset_operation_count();
        assert_eq!(rig.get_operation_count(), 0);
    }
}
