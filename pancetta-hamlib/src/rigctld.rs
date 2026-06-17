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

/// A persistent connection to rigctld with a buffered reader.
/// The BufReader must be kept alive across commands so that
/// partially-read data isn't lost between calls.
struct RigctldConnection {
    reader: BufReader<tokio::io::ReadHalf<TcpStream>>,
    writer: tokio::io::WriteHalf<TcpStream>,
}

/// Rigctld TCP client
pub struct RigctldClient {
    /// Configuration
    config: RigctldConfig,
    /// Persistent connection (reader + writer)
    conn: Arc<Mutex<Option<RigctldConnection>>>,
    /// Internal state
    state: Arc<RwLock<RigctldState>>,
}

impl RigctldClient {
    /// Create a new rigctld client
    pub fn new(config: RigctldConfig) -> Self {
        Self {
            config,
            conn: Arc::new(Mutex::new(None)),
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
    // rationale: inherent `default()` is kept (callers use `Type::default()`);
    // switching to a `Default` impl would change the public API shape.
    #[allow(clippy::should_implement_trait)]
    pub fn default() -> Self {
        Self::new(RigctldConfig::default())
    }

    /// Send a command and get response
    async fn send_command(&self, command: &str) -> Result<String> {
        let mut conn_guard = self.conn.lock().await;

        if let Some(conn) = conn_guard.as_mut() {
            let cmd_with_newline = format!("{}\n", command);
            debug!("Sending rigctld command: {}", command);

            conn.writer.write_all(cmd_with_newline.as_bytes()).await?;
            conn.writer.flush().await?;

            // Read response line (using persistent BufReader so buffered data isn't lost)
            let mut response = String::new();
            match timeout(
                Duration::from_millis(self.config.command_timeout_ms),
                conn.reader.read_line(&mut response),
            )
            .await
            {
                Ok(Ok(0)) => {
                    *conn_guard = None;
                    let mut state = self.state.write().await;
                    state.connected = false;
                    Err(anyhow!("rigctld closed connection"))
                }
                Ok(Ok(_)) => {
                    response = response.trim().to_string();

                    // Check for RPRT error/success codes
                    if response.starts_with("RPRT") {
                        let code = response
                            .split_whitespace()
                            .nth(1)
                            .and_then(|s| s.parse::<i32>().ok())
                            .unwrap_or_else(|| {
                                tracing::warn!(
                                    "rigctld: malformed RPRT response '{}', \
                                     treating as error",
                                    response
                                );
                                -1
                            });

                        if code != 0 {
                            return Err(anyhow!("Rigctld error code: {}", code));
                        }

                        // RPRT 0 means success for set commands — no data follows
                        debug!("Rigctld response: RPRT 0 (OK)");
                        return Ok(String::new());
                    }

                    debug!("Rigctld response: {}", response);
                    Ok(response)
                }
                Ok(Err(e)) => {
                    *conn_guard = None;
                    let mut state = self.state.write().await;
                    state.connected = false;
                    Err(anyhow!("Failed to read response: {}", e))
                }
                Err(_) => {
                    *conn_guard = None;
                    let mut state = self.state.write().await;
                    state.connected = false;
                    Err(anyhow!("Command timeout"))
                }
            }
        } else {
            Err(anyhow!("Not connected to rigctld"))
        }
    }

    /// Read an additional line from the connection (for multi-line responses).
    /// Must be called while the connection is still valid.
    async fn read_extra_line(&self) -> Result<String> {
        let mut conn_guard = self.conn.lock().await;
        if let Some(conn) = conn_guard.as_mut() {
            let mut line = String::new();
            match timeout(
                Duration::from_millis(self.config.command_timeout_ms),
                conn.reader.read_line(&mut line),
            )
            .await
            {
                Ok(Ok(0)) => Err(anyhow!("rigctld closed connection (extra line)")),
                Ok(Ok(_)) => Ok(line.trim().to_string()),
                Ok(Err(e)) => Err(anyhow!("Failed to read extra line: {}", e)),
                Err(_) => Err(anyhow!("Timeout reading extra line")),
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
        response
            .trim()
            .parse::<u64>()
            .map_err(|e| anyhow!("Failed to parse frequency: {}", e))
    }

    /// Parse mode from response (mode + passband on one line, space-separated)
    #[allow(dead_code)]
    fn parse_mode(response: &str) -> Result<(Mode, i32)> {
        let parts: Vec<&str> = response.split_whitespace().collect();
        if parts.len() >= 2 {
            let mode = Self::string_to_mode(parts[0]);
            let passband = parts[1].parse::<i32>().unwrap_or_else(|_| {
                tracing::warn!(
                    "rigctld: could not parse passband from '{}', \
                     using 0 (disabled passband filter)",
                    parts[1]
                );
                0
            });
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

    /// First token of every rigctld command this client is permitted to send.
    ///
    /// SECURITY (I-12): `send_raw_command` is a public escape hatch, so an
    /// untrusted/buggy caller could otherwise reach *any* rigctld verb,
    /// including dangerous configuration ones (`\set_conf`, `q`/quit, reset,
    /// memory-clear, etc.). We constrain it to an allow-list keyed on the
    /// first whitespace-separated token. The set covers:
    ///   - every verb this client already issues internally (so nothing
    ///     that works today breaks):
    ///       short form: `f` (get_freq), `F` (set_freq), `m` (get_mode),
    ///                   `T` (set_ptt), `V` (set_vfo)
    ///       long form:  `\set_mode`, `\get_ptt`, `\get_level`, `\set_vfo`,
    ///                   `\get_vfo`, `\set_level`, `\set_mem`, `\get_mem`,
    ///                   `\set_func`, `\get_info`
    ///   - the standard short/long forms that pair with the above (so the
    ///     escape hatch can read what it can already set and vice versa):
    ///       `M` (set_mode), `t` (get_ptt), `v` (get_vfo)
    ///   - the antenna verbs the escape hatch was documented for:
    ///       `y` (get_ant), `Y` (set_ant), `\get_ant`, `\set_ant`
    /// Anything else (notably `\set_conf`, `q`, `Q`, `reset`, `\reset`,
    /// `\send_morse`, free text) is rejected without being sent.
    const ALLOWED_COMMAND_VERBS: &'static [&'static str] = &[
        // short-form verbs (internal + their read/write pair + antenna)
        "f",
        "F",
        "m",
        "M",
        "t",
        "T",
        "v",
        "V",
        "y",
        "Y",
        // long-form `\verb` commands (internal + antenna pair)
        "\\set_mode",
        "\\get_mode",
        "\\set_ptt",
        "\\get_ptt",
        "\\set_level",
        "\\get_level",
        "\\set_vfo",
        "\\get_vfo",
        "\\set_func",
        "\\get_func",
        "\\set_mem",
        "\\get_mem",
        "\\get_info",
        "\\set_ant",
        "\\get_ant",
    ];

    /// Send a raw command to rigctld and return the response string.
    ///
    /// This is a public escape hatch for commands that don't have a
    /// higher-level wrapper (e.g. antenna control via `y` / `Y`).
    ///
    /// The command must contain only printable ASCII characters (0x20..=0x7E).
    /// Embedded newlines (`\n`, `\r`) and non-printable bytes are rejected
    /// to prevent protocol injection. Additionally (I-12), the first token
    /// must be one of [`Self::ALLOWED_COMMAND_VERBS`]; any other verb
    /// (e.g. `\set_conf`, `q`, arbitrary text) is rejected without being
    /// sent to the rig.
    pub async fn send_raw_command(&self, cmd: &str) -> Result<String> {
        // Reject embedded newlines and non-printable ASCII to prevent injection
        if cmd.bytes().any(|b| b == b'\n' || b == b'\r') {
            return Err(anyhow!("raw command must not contain newline characters"));
        }
        if cmd.bytes().any(|b| !(0x20..=0x7E).contains(&b)) {
            return Err(anyhow!(
                "raw command must contain only printable ASCII characters (0x20-0x7E)"
            ));
        }
        // SECURITY (I-12): grammar allow-list keyed on the first token.
        if !Self::raw_command_allowed(cmd) {
            let verb = cmd.split_whitespace().next().unwrap_or("");
            return Err(anyhow!(
                "raw command verb '{}' is not in the rigctld allow-list",
                verb
            ));
        }
        self.send_command_with_retry(cmd).await
    }

    /// Returns `true` iff the first whitespace-separated token of `cmd` is in
    /// [`Self::ALLOWED_COMMAND_VERBS`]. Pure, side-effect-free helper so the
    /// I-12 allow-list can be unit-tested without a live rigctld connection.
    fn raw_command_allowed(cmd: &str) -> bool {
        let verb = cmd.split_whitespace().next().unwrap_or("");
        Self::ALLOWED_COMMAND_VERBS.contains(&verb)
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
        info!(
            "Connecting to rigctld at {}:{}",
            self.config.host, self.config.port
        );

        // Try to connect with timeout
        let addr = format!("{}:{}", self.config.host, self.config.port);
        let connect_result = timeout(
            Duration::from_millis(self.config.timeout_ms),
            TcpStream::connect(&addr),
        )
        .await;

        match connect_result {
            Ok(Ok(stream)) => {
                stream.set_nodelay(true)?;

                // Split into reader/writer and wrap reader in BufReader
                let (read_half, write_half) = tokio::io::split(stream);
                *self.conn.lock().await = Some(RigctldConnection {
                    reader: BufReader::new(read_half),
                    writer: write_half,
                });

                // Update state
                let mut state = self.state.write().await;
                state.connected = true;

                info!("Successfully connected to rigctld");

                // Verify with a simple frequency query
                drop(state); // release write lock before send_command
                match self.send_command("f").await {
                    Ok(resp) => {
                        info!("Rigctld connection verified (freq={})", resp);
                        Ok(())
                    }
                    Err(e) => {
                        error!("Failed to verify connection: {}", e);
                        let mut state = self.state.write().await;
                        state.connected = false;
                        *self.conn.lock().await = None;
                        Err(anyhow!("Connection verification failed: {}", e))
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

        // Close connection
        *self.conn.lock().await = None;

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
        // Use short-form rigctld command: "F <freq>" sets current VFO frequency
        // For non-current VFO, switch VFO first
        if !matches!(vfo, Vfo::Current | Vfo::A) {
            self.send_command_with_retry(&format!("V {}", Self::vfo_to_string(vfo)))
                .await?;
        }
        self.send_command_with_retry(&format!("F {}", frequency))
            .await?;

        // Restore to VFO A after operating on a non-current VFO
        if !matches!(vfo, Vfo::Current | Vfo::A) {
            let _ = self.send_command_with_retry("V VFOA").await;
        }

        // Update cached value
        let mut state = self.state.write().await;
        state.last_frequency = frequency;

        Ok(())
    }

    async fn get_frequency(&self, vfo: Vfo) -> Result<u64> {
        // Use short-form: "f" gets current VFO frequency
        if !matches!(vfo, Vfo::Current | Vfo::A) {
            self.send_command_with_retry(&format!("V {}", Self::vfo_to_string(vfo)))
                .await?;
        }
        let response = self.send_command_with_retry("f").await?;
        let frequency = Self::parse_frequency(&response)?;

        // Restore to VFO A after operating on a non-current VFO
        if !matches!(vfo, Vfo::Current | Vfo::A) {
            let _ = self.send_command_with_retry("V VFOA").await;
        }

        // Update cached value
        let mut state = self.state.write().await;
        state.last_frequency = frequency;

        Ok(frequency)
    }

    #[instrument(skip(self))]
    async fn set_mode(&self, vfo: Vfo, mode: Mode, passband: Option<i32>) -> Result<()> {
        // Switch VFO first if needed (same pattern as set_frequency)
        if !matches!(vfo, Vfo::Current | Vfo::A) {
            self.send_command_with_retry(&format!("V {}", Self::vfo_to_string(vfo)))
                .await?;
        }

        let pb = passband.unwrap_or(0);
        // rigctld \set_mode takes only (mode, passband), NOT (vfo, mode, passband)
        let cmd = format!("\\set_mode {} {}", Self::mode_to_string(mode), pb);

        self.send_command_with_retry(&cmd).await?;

        // Restore to VFO A after operating on a non-current VFO
        if !matches!(vfo, Vfo::Current | Vfo::A) {
            let _ = self.send_command_with_retry("V VFOA").await;
        }

        // Update cached value
        let mut state = self.state.write().await;
        state.last_mode = mode;

        Ok(())
    }

    async fn get_mode(&self, _vfo: Vfo) -> Result<(Mode, i32)> {
        // Use short-form "m" which returns two lines: mode\npassband\n
        // We must consume both lines to keep the BufReader in sync.
        let mode_str = self.send_command_with_retry("m").await?;
        let passband_str = self.read_extra_line().await.unwrap_or_default();
        let mode = Self::string_to_mode(&mode_str);
        let passband = passband_str.parse::<i32>().unwrap_or_else(|_| {
            tracing::warn!(
                "rigctld: could not parse passband from '{}', using 0",
                passband_str
            );
            0
        });

        // Update cached value
        let mut state = self.state.write().await;
        state.last_mode = mode;

        Ok((mode, passband))
    }

    #[instrument(skip(self))]
    async fn set_ptt(&self, _vfo: Vfo, state: PttState) -> Result<()> {
        let ptt_value = match state {
            PttState::On | PttState::OnMic | PttState::OnData => "1",
            PttState::Off => "0",
        };

        // Use rigctld's short-form command `T <0|1>` (no VFO arg). The
        // long-form `\set_ptt currVFO N` returns RPRT -1 (RIG_EINVAL) on
        // some hamlib drivers (incl. the FTdx10's) — short form works
        // reliably and matches tx_test's validated pattern.
        let cmd = format!("T {}", ptt_value);
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

        // Parse the integer reading (rigctld returns values like "-54").
        // Per hamlib convention, STRENGTH is dB relative to S9: 0 = S9,
        // -54 ≈ S0, +20 = S9 + 20 dB. On parse failure
        // return an error rather than the silent -120 fallback that would
        // be indistinguishable from a real "very weak signal" reading and
        // poison the SNR / band-noise displays in the TUI.
        let strength = response.trim().parse::<i32>().map_err(|e| {
            anyhow!(
                "rigctld: could not parse signal strength from '{}': {}",
                response.trim(),
                e
            )
        })?;

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
        let level = (watts / 100.0).clamp(0.0, 1.0);
        let cmd = format!("\\set_level RFPOWER {}", level);
        self.send_command_with_retry(&cmd).await?;
        Ok(())
    }

    async fn get_power_level(&self) -> Result<f32> {
        let response = self.send_command_with_retry("\\get_level RFPOWER").await?;

        // Parse as fraction (0.0 - 1.0) and convert to watts
        let level = response.trim().parse::<f32>().unwrap_or_else(|_| {
            tracing::warn!(
                "rigctld: could not parse power level from '{}', using 0.0",
                response.trim()
            );
            0.0
        });

        Ok(level * 100.0) // Convert to watts (assuming 100W max)
    }

    async fn get_swr(&self) -> Result<f32> {
        // Get SWR reading
        let response = self.send_command_with_retry("\\get_level SWR").await?;

        // Parse as SWR value. 1.0 is a perfect-match fallback that masks
        // a real high-SWR reading; warn so this is visible in logs.
        let swr = response.trim().parse::<f32>().unwrap_or_else(|_| {
            tracing::warn!(
                "rigctld: could not parse SWR from '{}', using 1.0",
                response.trim()
            );
            1.0
        });

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

        let channel = response
            .trim()
            .parse::<i32>()
            .map_err(|e| anyhow!("Failed to parse memory channel: {}", e))?;

        Ok(channel)
    }

    async fn set_scan(&self, vfo: Vfo, enable: bool) -> Result<()> {
        let scan_value = if enable { "1" } else { "0" };
        let cmd = format!(
            "\\set_func {} SCAN {}",
            Self::vfo_to_string(vfo),
            scan_value
        );
        self.send_command_with_retry(&cmd).await?;
        Ok(())
    }

    async fn get_info(&self) -> Result<String> {
        // NOTE: \dump_state returns dozens of lines and corrupts the BufReader
        // stream. Use the single-line \get_info command instead which returns
        // just the rig model/info string.
        let response = self.send_command_with_retry("\\get_info").await?;
        Ok(response)
    }
}

#[cfg(test)]
mod allow_list_tests {
    use super::*;

    /// Every command the client currently issues internally (plus the
    /// documented antenna escape-hatch verbs) must pass the I-12 allow-list,
    /// so tightening it breaks nothing that works today.
    #[test]
    fn accepts_all_internal_and_documented_commands() {
        let accepted = [
            // short-form internal callers
            "f",
            "F 14074000",
            "m",
            "T 1",
            "T 0",
            "V VFOA",
            // their read/write pairs
            "M PKTUSB 0",
            "t",
            "v",
            // long-form internal callers
            "\\set_mode PKTUSB 0",
            "\\get_ptt VFOA",
            "\\get_level STRENGTH",
            "\\get_level RFPOWER",
            "\\get_level SWR",
            "\\set_level RFPOWER 0.5",
            "\\set_vfo VFOA",
            "\\get_vfo",
            "\\set_func VFOA SCAN 1",
            "\\get_mem VFOA",
            "\\set_mem VFOA 3",
            "\\get_info",
            // documented antenna escape hatch
            "y",
            "Y 1",
            "\\get_ant",
            "\\set_ant 1",
        ];
        for cmd in accepted {
            assert!(
                RigctldClient::raw_command_allowed(cmd),
                "expected allow-list to accept {cmd:?}"
            );
        }
    }

    /// Unknown / dangerous verbs must be rejected without being sent.
    #[test]
    fn rejects_unknown_and_dangerous_commands() {
        let rejected = [
            "\\set_conf serial_speed 115200",
            "\\reset",
            "q",
            "Q",
            "reset",
            "\\send_morse hi",
            "\\dump_state",
            "rm -rf /",
            "arbitrary text",
            "",
            "   ",
            // an allowed verb only matches as the *first* token, not as an arg
            "echo f",
        ];
        for cmd in rejected {
            assert!(
                !RigctldClient::raw_command_allowed(cmd),
                "expected allow-list to reject {cmd:?}"
            );
        }
    }

    /// The async wrapper surfaces the rejection as an `Err` (and therefore
    /// never reaches `send_command_with_retry`). We use a client that is not
    /// connected; an allow-listed command would fail at the connection step,
    /// but a rejected command fails earlier with the allow-list message.
    #[tokio::test]
    async fn send_raw_command_rejects_disallowed_verb() {
        let client = RigctldClient::new(RigctldConfig::default());
        let err = client
            .send_raw_command("\\set_conf foo bar")
            .await
            .expect_err("disallowed verb must be rejected");
        assert!(
            err.to_string().contains("allow-list"),
            "unexpected error: {err}"
        );
    }
}
