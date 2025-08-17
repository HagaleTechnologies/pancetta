//! Rigctld TCP client implementation
//!
//! This module provides a real implementation of the RigControl trait
//! that connects to rigctld (Hamlib's TCP daemon) for radio control.

use crate::models::{Mode, Vfo};
use crate::rig::{ConnectionState, PttState, RigControl, RigStatus};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info, instrument, warn};

/// Rigctld connection configuration
#[derive(Debug, Clone)]
pub struct RigctldConfig {
    /// Host address (default: "127.0.0.1")
    pub host: String,
    /// Port number (default: 4532)
    pub port: u16,
    /// Connection timeout in milliseconds
    pub timeout_ms: u64,
    /// Command timeout in milliseconds
    pub command_timeout_ms: u64,
    /// Retry count for failed commands
    pub retry_count: u32,
    /// Polling interval for status updates (milliseconds)
    pub poll_interval_ms: u64,
}

impl Default for RigctldConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 4532,
            timeout_ms: 5000,
            command_timeout_ms: 1000,
            retry_count: 3,
            poll_interval_ms: 500,
        }
    }
}

/// Rigctld client state
struct RigctldState {
    /// Current connection status
    connected: bool,
    /// Last frequency for caching
    last_frequency: u64,
    /// Last mode for caching
    last_mode: Mode,
    /// Last PTT state
    last_ptt: PttState,
    /// Last signal strength
    last_signal_strength: i32,
}

/// Rigctld TCP client
pub struct RigctldClient {
    /// Configuration
    config: RigctldConfig,
    /// TCP stream (when connected)
    stream: Arc<Mutex<Option<TcpStream>>>,
    /// Internal state
    state: Arc<RwLock<RigctldState>>,
}

impl RigctldClient {
    /// Create a new rigctld client
    pub fn new(config: RigctldConfig) -> Self {
        Self {
            config,
            stream: Arc::new(Mutex::new(None)),
            state: Arc::new(RwLock::new(RigctldState {
                connected: false,
                last_frequency: 0,
                last_mode: Mode::USB,
                last_ptt: PttState::Off,
                last_signal_strength: -120,
            })),
        }
    }

    /// Create with default configuration
    pub fn default() -> Self {
        Self::new(RigctldConfig::default())
    }

    /// Send a command and get response
    async fn send_command(&self, command: &str) -> Result<String> {
        let mut stream_guard = self.stream.lock().await;
        
        if let Some(stream) = stream_guard.as_mut() {
            // Send command
            let cmd_with_newline = format!("{}\n", command);
            debug!("Sending rigctld command: {}", command);
            
            stream.write_all(cmd_with_newline.as_bytes()).await?;
            stream.flush().await?;
            
            // Read response
            let mut response = String::new();
            let mut reader = BufReader::new(stream);
            
            // Read with timeout
            match timeout(
                Duration::from_millis(self.config.command_timeout_ms),
                reader.read_line(&mut response)
            ).await {
                Ok(Ok(_)) => {
                    response = response.trim().to_string();
                    
                    // Check for errors
                    if response.starts_with("RPRT") {
                        let code = response.split_whitespace().nth(1)
                            .and_then(|s| s.parse::<i32>().ok())
                            .unwrap_or(-1);
                        
                        if code != 0 {
                            return Err(anyhow!("Rigctld error code: {}", code));
                        }
                        
                        // Read actual data after RPRT 0
                        response.clear();
                        reader.read_line(&mut response).await?;
                        response = response.trim().to_string();
                    }
                    
                    debug!("Rigctld response: {}", response);
                    Ok(response)
                }
                Ok(Err(e)) => Err(anyhow!("Failed to read response: {}", e)),
                Err(_) => Err(anyhow!("Command timeout")),
            }
        } else {
            Err(anyhow!("Not connected to rigctld"))
        }
    }

    /// Send command with retry logic
    async fn send_command_with_retry(&self, command: &str) -> Result<String> {
        let mut last_error = None;
        
        for attempt in 0..self.config.retry_count {
            match self.send_command(command).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    warn!("Command failed (attempt {}): {}", attempt + 1, e);
                    last_error = Some(e);
                    
                    // Reconnect if disconnected
                    let state = self.state.read().await;
                    if !state.connected {
                        drop(state);
                        let _ = self.connect().await;
                    }
                }
            }
        }
        
        Err(last_error.unwrap_or_else(|| anyhow!("Command failed after retries")))
    }

    /// Parse frequency from response
    fn parse_frequency(response: &str) -> Result<u64> {
        response.trim()
            .parse::<u64>()
            .map_err(|e| anyhow!("Failed to parse frequency: {}", e))
    }

    /// Parse mode from response
    fn parse_mode(response: &str) -> Result<(Mode, i32)> {
        let parts: Vec<&str> = response.split_whitespace().collect();
        if parts.len() >= 2 {
            let mode = Self::string_to_mode(parts[0]);
            let passband = parts[1].parse::<i32>().unwrap_or(0);
            Ok((mode, passband))
        } else {
            Err(anyhow!("Invalid mode response format"))
        }
    }

    /// Convert string to Mode enum
    fn string_to_mode(s: &str) -> Mode {
        match s.to_uppercase().as_str() {
            "USB" => Mode::USB,
            "LSB" => Mode::LSB,
            "CW" => Mode::CW,
            "FM" => Mode::FM,
            "AM" => Mode::AM,
            "RTTY" => Mode::RTTY,
            "FT8" => Mode::FT8,
            "FT4" => Mode::FT4,
            "PSK31" => Mode::PSK31,
            "PACKET" => Mode::PACKET,
            _ => Mode::USB, // Default
        }
    }

    /// Convert Mode enum to string
    fn mode_to_string(mode: Mode) -> &'static str {
        match mode {
            Mode::USB => "USB",
            Mode::LSB => "LSB",
            Mode::CW => "CW",
            Mode::FM => "FM",
            Mode::AM => "AM",
            Mode::RTTY => "RTTY",
            Mode::FT8 => "PKTUSB", // FT8 uses USB data mode
            Mode::FT4 => "PKTUSB", // FT4 uses USB data mode
            Mode::PSK31 => "PKTUSB",
            Mode::PACKET => "PKTFM",
            _ => "USB",
        }
    }

    /// Convert VFO enum to rigctld string
    fn vfo_to_string(vfo: Vfo) -> &'static str {
        match vfo {
            Vfo::A => "VFOA",
            Vfo::B => "VFOB",
            Vfo::Current => "currVFO",
            _ => "currVFO",
        }
    }
}

#[async_trait]
impl RigControl for RigctldClient {
    #[instrument(skip(self))]
    async fn connect(&self) -> Result<()> {
        info!("Connecting to rigctld at {}:{}", self.config.host, self.config.port);
        
        // Try to connect with timeout
        let addr = format!("{}:{}", self.config.host, self.config.port);
        let connect_result = timeout(
            Duration::from_millis(self.config.timeout_ms),
            TcpStream::connect(&addr)
        ).await;
        
        match connect_result {
            Ok(Ok(stream)) => {
                // Set TCP keepalive
                stream.set_nodelay(true)?;
                
                // Store stream
                *self.stream.lock().await = Some(stream);
                
                // Update state
                let mut state = self.state.write().await;
                state.connected = true;
                
                info!("Successfully connected to rigctld");
                
                // Test connection with a simple command
                match timeout(
                    Duration::from_millis(1000),
                    self.send_command("\\dump_state")
                ).await {
                    Ok(Ok(_)) => {
                        info!("Rigctld connection verified");
                        Ok(())
                    }
                    Ok(Err(e)) => {
                        error!("Failed to verify connection: {}", e);
                        state.connected = false;
                        *self.stream.lock().await = None;
                        Err(anyhow!("Connection verification failed: {}", e))
                    }
                    Err(_) => {
                        error!("Connection verification timeout");
                        state.connected = false;
                        *self.stream.lock().await = None;
                        Err(anyhow!("Connection verification timeout"))
                    }
                }
            }
            Ok(Err(e)) => {
                error!("Failed to connect to rigctld: {}", e);
                Err(anyhow!("Connection failed: {}", e))
            }
            Err(_) => {
                error!("Connection timeout");
                Err(anyhow!("Connection timeout"))
            }
        }
    }

    async fn disconnect(&self) -> Result<()> {
        info!("Disconnecting from rigctld");
        
        // Close stream
        *self.stream.lock().await = None;
        
        // Update state
        let mut state = self.state.write().await;
        state.connected = false;
        
        Ok(())
    }

    async fn get_status(&self) -> Result<RigStatus> {
        let state = self.state.read().await;
        
        Ok(RigStatus {
            connection_state: if state.connected {
                ConnectionState::Connected
            } else {
                ConnectionState::Disconnected
            },
            frequency: Some(state.last_frequency),
            mode: Some(state.last_mode),
            width: None,
            vfo: Some(Vfo::Current),
            ptt: Some(state.last_ptt),
            power_level: None,
            s_meter: Some(state.last_signal_strength),
            swr: Some(1.0), // TODO: Get real SWR
            memory_channel: None,
            last_update: std::time::Instant::now(),
            last_error: None,
        })
    }

    #[instrument(skip(self))]
    async fn set_frequency(&self, vfo: Vfo, frequency: u64) -> Result<()> {
        // Rigctld expects frequency in Hz
        let cmd = format!("\\set_freq {} {}", Self::vfo_to_string(vfo), frequency);
        self.send_command_with_retry(&cmd).await?;
        
        // Update cached value
        let mut state = self.state.write().await;
        state.last_frequency = frequency;
        
        Ok(())
    }

    async fn get_frequency(&self, vfo: Vfo) -> Result<u64> {
        let cmd = format!("\\get_freq {}", Self::vfo_to_string(vfo));
        let response = self.send_command_with_retry(&cmd).await?;
        let frequency = Self::parse_frequency(&response)?;
        
        // Update cached value
        let mut state = self.state.write().await;
        state.last_frequency = frequency;
        
        Ok(frequency)
    }

    #[instrument(skip(self))]
    async fn set_mode(&self, vfo: Vfo, mode: Mode, passband: Option<i32>) -> Result<()> {
        let pb = passband.unwrap_or(0);
        let cmd = format!("\\set_mode {} {} {}", 
            Self::vfo_to_string(vfo),
            Self::mode_to_string(mode),
            pb
        );
        
        self.send_command_with_retry(&cmd).await?;
        
        // Update cached value
        let mut state = self.state.write().await;
        state.last_mode = mode;
        
        Ok(())
    }

    async fn get_mode(&self, vfo: Vfo) -> Result<(Mode, i32)> {
        let cmd = format!("\\get_mode {}", Self::vfo_to_string(vfo));
        let response = self.send_command_with_retry(&cmd).await?;
        let (mode, passband) = Self::parse_mode(&response)?;
        
        // Update cached value
        let mut state = self.state.write().await;
        state.last_mode = mode;
        
        Ok((mode, passband))
    }

    #[instrument(skip(self))]
    async fn set_ptt(&self, vfo: Vfo, state: PttState) -> Result<()> {
        let ptt_value = match state {
            PttState::On | PttState::OnMic | PttState::OnData => "1",
            PttState::Off => "0",
        };
        
        let cmd = format!("\\set_ptt {} {}", Self::vfo_to_string(vfo), ptt_value);
        self.send_command_with_retry(&cmd).await?;
        
        // Update cached value
        let mut state_guard = self.state.write().await;
        state_guard.last_ptt = state;
        
        Ok(())
    }

    async fn get_ptt(&self, vfo: Vfo) -> Result<PttState> {
        let cmd = format!("\\get_ptt {}", Self::vfo_to_string(vfo));
        let response = self.send_command_with_retry(&cmd).await?;
        
        let ptt = match response.trim() {
            "1" => PttState::On,
            _ => PttState::Off,
        };
        
        // Update cached value
        let mut state = self.state.write().await;
        state.last_ptt = ptt;
        
        Ok(ptt)
    }

    async fn get_s_meter(&self) -> Result<i32> {
        // Get signal strength
        let response = self.send_command_with_retry("\\get_level STRENGTH").await?;
        
        // Parse as dBm (rigctld returns values like "-54")
        let strength = response.trim()
            .parse::<i32>()
            .unwrap_or(-120);
        
        // Update cached value
        let mut state = self.state.write().await;
        state.last_signal_strength = strength;
        
        Ok(strength)
    }

    async fn set_vfo(&self, vfo: Vfo) -> Result<()> {
        let cmd = format!("\\set_vfo {}", Self::vfo_to_string(vfo));
        self.send_command_with_retry(&cmd).await?;
        Ok(())
    }

    async fn get_vfo(&self) -> Result<Vfo> {
        let response = self.send_command_with_retry("\\get_vfo").await?;
        
        let vfo = match response.trim() {
            "VFOA" => Vfo::A,
            "VFOB" => Vfo::B,
            _ => Vfo::Current,
        };
        
        Ok(vfo)
    }

    async fn set_power_level(&self, watts: f32) -> Result<()> {
        // Convert watts to percentage (0.0 - 1.0)
        // Assuming 100W max for now (should be configurable)
        let level = (watts / 100.0).min(1.0).max(0.0);
        let cmd = format!("\\set_level RFPOWER {}", level);
        self.send_command_with_retry(&cmd).await?;
        Ok(())
    }

    async fn get_power_level(&self) -> Result<f32> {
        let response = self.send_command_with_retry("\\get_level RFPOWER").await?;
        
        // Parse as fraction (0.0 - 1.0) and convert to watts
        let level = response.trim()
            .parse::<f32>()
            .unwrap_or(0.0);
        
        Ok(level * 100.0) // Convert to watts (assuming 100W max)
    }

    async fn get_swr(&self) -> Result<f32> {
        // Get SWR reading
        let response = self.send_command_with_retry("\\get_level SWR").await?;
        
        // Parse as SWR value
        let swr = response.trim()
            .parse::<f32>()
            .unwrap_or(1.0);
        
        Ok(swr)
    }

    async fn set_memory_channel(&self, vfo: Vfo, channel: i32) -> Result<()> {
        let cmd = format!("\\set_mem {} {}", Self::vfo_to_string(vfo), channel);
        self.send_command_with_retry(&cmd).await?;
        Ok(())
    }

    async fn get_memory_channel(&self, vfo: Vfo) -> Result<i32> {
        let cmd = format!("\\get_mem {}", Self::vfo_to_string(vfo));
        let response = self.send_command_with_retry(&cmd).await?;
        
        let channel = response.trim()
            .parse::<i32>()
            .map_err(|e| anyhow!("Failed to parse memory channel: {}", e))?;
        
        Ok(channel)
    }

    async fn set_scan(&self, vfo: Vfo, enable: bool) -> Result<()> {
        let scan_value = if enable { "1" } else { "0" };
        let cmd = format!("\\set_func {} SCAN {}", Self::vfo_to_string(vfo), scan_value);
        self.send_command_with_retry(&cmd).await?;
        Ok(())
    }

    async fn get_info(&self) -> Result<String> {
        // Get rig info from rigctld
        let response = self.send_command_with_retry("\\dump_state").await?;
        Ok(response)
    }
}