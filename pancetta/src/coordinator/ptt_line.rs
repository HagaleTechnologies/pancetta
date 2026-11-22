//! PAN-357: key the rig by asserting RTS on a separate serial port.
//!
//! With `rig.ptt.method = "serial_rts"` the Hamlib component wraps its rig in
//! [`LineKeyedRig`]. Every `set_ptt` the Hamlib loop makes (TX frames,
//! bundles, tune, manual PTT, the watchdog, teardown and the halts) drives the
//! RTS line of `rig.ptt.device` and never sends CAT `T 1`/`T 0`. Every other
//! [`RigControl`] method goes to the inner rig unchanged, so rigctld still sets
//! frequency, split and mode and reads the meters.
//!
//! The coordinator owns one [`PttLine`], opened once and held for the session:
//! reopening per frame would pulse RTS on Linux, which raises DTR and RTS when
//! a port opens. Each Hamlib start reconciles it with [`PttLine::configure`],
//! and shutdown lowers and latches it with [`PttLine::release_for_shutdown`]
//! independently of the Hamlib loop.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use pancetta_config::rig::{PttMethod, RigConfig};
use pancetta_hamlib::{
    DataModeReadiness, Mode, PttState, ReceiveGain, ReceiveGainCapabilities, RigControl, RigStatus,
    TxMeter, TxMeterLevels, Vfo,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// A modem-control line that can key the rig.
pub(crate) trait RtsLine: Send {
    /// Assert (`true`) or drop (`false`) RTS.
    fn write_rts(&mut self, asserted: bool) -> std::io::Result<()>;
}

impl RtsLine for Box<dyn serialport::SerialPort> {
    fn write_rts(&mut self, asserted: bool) -> std::io::Result<()> {
        self.write_request_to_send(asserted)
            .map_err(std::io::Error::from)
    }
}

/// Open `device` for RTS keying and drop RTS at once. Linux raises DTR and
/// RTS when a port opens, so the low write must come before anything else.
/// On unix the port also gets HUPCL, so the kernel drops RTS when the fd
/// closes however the process ends; a port that cannot take it is refused.
fn open_port_raw(device: &str) -> serialport::Result<Box<dyn serialport::SerialPort>> {
    let builder = serialport::new(device, 9600)
        .flow_control(serialport::FlowControl::None)
        .dtr_on_open(false)
        .timeout(Duration::from_secs(2));
    #[cfg(unix)]
    {
        use serialport::SerialPort as _;
        use std::os::unix::io::AsRawFd;
        let mut port = serialport::TTYPort::open(&builder)?;
        port.write_request_to_send(false)?;
        set_hang_up_on_close(port.as_raw_fd()).map_err(|e| {
            serialport::Error::new(
                serialport::ErrorKind::Io(e.kind()),
                format!("cannot set HUPCL (drop RTS on close): {e}"),
            )
        })?;
        Ok(Box::new(port))
    }
    #[cfg(not(unix))]
    {
        let mut port = builder.open()?;
        port.write_request_to_send(false)?;
        Ok(port)
    }
}

/// Set HUPCL on a tty, so the kernel drops the modem lines (RTS and DTR)
/// on the last close of the port. That unkeys the rig when the process dies
/// without running any Rust code (SIGKILL, abort, OOM kill); `cfmakeraw`
/// leaves HUPCL as the tty had it, and some drivers or udev rules clear it.
#[cfg(unix)]
pub(crate) fn set_hang_up_on_close(fd: std::os::unix::io::RawFd) -> std::io::Result<()> {
    // SAFETY: `fd` is an open tty the caller owns for the duration of the
    // call; `termios` is plain data that `tcgetattr` fills in completely.
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    termios.c_cflag |= libc::HUPCL;
    // SAFETY: as above; `termios` came from `tcgetattr` on the same fd.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Open a serial PTT port with RTS low. The baud rate does not matter for
/// modem lines. Errors read `cannot open serial PTT port <device>: <cause>`.
pub fn open_serial_rts_port(device: &str) -> Result<Box<dyn serialport::SerialPort>> {
    open_port_raw(device).with_context(|| format!("cannot open serial PTT port {device}"))
}

/// The PTT port the Hamlib component must key, or `None` when the rig keys
/// through CAT. Only a live rig (`rig_enabled`: interface on, not
/// `PANCETTA_MOCK_RIG`) with `serial_rts` and a non-blank device has a line.
pub(crate) fn line_target(rig: &RigConfig, rig_enabled: bool) -> Option<String> {
    if !rig_enabled || rig.ptt.method != PttMethod::SerialRts {
        return None;
    }
    rig.ptt
        .device
        .as_deref()
        .map(str::trim)
        .filter(|device| !device.is_empty())
        .map(str::to_string)
}

type Opener = Arc<dyn Fn(&str) -> Result<Box<dyn RtsLine>> + Send + Sync>;

#[derive(Default)]
struct LineState {
    device: Option<String>,
    handle: Option<Box<dyn RtsLine>>,
    shut: bool,
}

/// The coordinator's serial PTT line.
pub(crate) struct PttLine {
    opener: Opener,
    state: Mutex<LineState>,
    /// The line may be asserted: set before a key-down write, cleared only by
    /// a confirmed low write. Read lock-free by [`PttLine::is_asserted`].
    maybe_asserted: AtomicBool,
}

impl PttLine {
    /// The production line, opening real serial ports.
    pub(crate) fn serial() -> Self {
        Self::with_opener(Arc::new(|device: &str| {
            open_port_raw(device)
                .map(|port| Box::new(port) as Box<dyn RtsLine>)
                .map_err(anyhow::Error::from)
        }))
    }

    /// A line with a custom opener (tests use a fake line).
    pub(crate) fn with_opener(opener: Opener) -> Self {
        Self {
            opener,
            state: Mutex::new(LineState::default()),
            maybe_asserted: AtomicBool::new(false),
        }
    }

    /// Whether the line may be asserted right now.
    pub(crate) fn is_asserted(&self) -> bool {
        self.maybe_asserted.load(Ordering::Acquire)
    }

    /// Reconcile the line with this Hamlib start's target. Reuses an open
    /// handle for the same device, lowers and closes the old handle when the
    /// target changes or becomes `None`, refuses a device that resolves to
    /// the CAT port, and opens the new device with RTS low.
    pub(crate) async fn configure(
        self: &Arc<Self>,
        target: Option<String>,
        cat_port: &str,
    ) -> Result<()> {
        let line = self.clone();
        let cat_port = cat_port.trim().to_string();
        run_blocking(move || line.configure_blocking(target, &cat_port)).await
    }

    /// Assert (`true`) or drop (`false`) the line. A key-down is refused
    /// after shutdown, with no device, or on a poisoned lock; an unkey always
    /// tries to lower the line.
    pub(crate) async fn set(self: &Arc<Self>, asserted: bool) -> Result<()> {
        let line = self.clone();
        run_blocking(move || line.set_blocking(asserted)).await
    }

    /// Lower the line and latch it: every later key-down is refused, and an
    /// unkey still succeeds. A no-op when no line was ever configured.
    pub(crate) async fn release_for_shutdown(self: &Arc<Self>) -> Result<()> {
        let line = self.clone();
        run_blocking(move || line.release_blocking()).await
    }

    fn lock_for(&self, asserted: bool) -> Result<MutexGuard<'_, LineState>> {
        match self.state.lock() {
            Ok(guard) => Ok(guard),
            // Fail closed for TX, never block an unkey.
            Err(_) if asserted => Err(anyhow!(
                "serial PTT line state is poisoned; refusing to key"
            )),
            Err(poisoned) => Ok(poisoned.into_inner()),
        }
    }

    fn open_locked(&self, state: &mut LineState, device: &str) -> Result<()> {
        let mut handle = (self.opener)(device)
            .with_context(|| format!("cannot open serial PTT port {device}"))?;
        handle
            .write_rts(false)
            .with_context(|| format!("cannot open serial PTT port {device}"))?;
        self.maybe_asserted.store(false, Ordering::Release);
        state.handle = Some(handle);
        Ok(())
    }

    fn configure_blocking(&self, target: Option<String>, cat_port: &str) -> Result<()> {
        let mut state = self.lock_for(false)?;
        if state.device == target && state.handle.is_some() {
            return Ok(());
        }
        if state.device != target {
            if let Some(old) = state.device.clone() {
                if self.is_asserted() {
                    // Never switch away from a line that may still key the
                    // rig: keep it (get_ptt reports On, so the watchdog keeps
                    // unkeying) until a low write is confirmed.
                    self.lower_locked(&mut state, &old).with_context(|| {
                        format!(
                            "cannot switch the serial PTT port away from {old}: RTS may still \
                             be asserted there"
                        )
                    })?;
                } else if let Some(handle) = state.handle.as_mut() {
                    if let Err(e) = handle.write_rts(false) {
                        tracing::warn!(
                            "Serial PTT: could not lower RTS on {old} before closing it: {e}"
                        );
                    }
                }
            }
            state.handle = None;
            self.maybe_asserted.store(false, Ordering::Release);
            state.device = None;
        }
        if state.shut {
            // Released for shutdown: never open (and so never pulse) a port.
            return Ok(());
        }
        let Some(device) = target else {
            return Ok(());
        };
        if resolves_to_cat_port(&device, cat_port) {
            return Err(anyhow!(
                "rig.ptt.device = \"{device}\" resolves to the CAT port (rig.interface.port = \
                 \"{cat_port}\"). rigctld owns that port; serial_rts needs a separate PTT port"
            ));
        }
        state.device = Some(device.clone());
        self.open_locked(&mut state, &device)
    }

    /// Drive RTS low on `device`: through the open handle, or by reopening
    /// the port (which writes RTS low) when there is none or the write fails.
    fn lower_locked(&self, state: &mut LineState, device: &str) -> Result<()> {
        if let Some(handle) = state.handle.as_mut() {
            if handle.write_rts(false).is_ok() {
                self.maybe_asserted.store(false, Ordering::Release);
                return Ok(());
            }
            state.handle = None;
        }
        self.open_locked(state, device)
    }

    /// Whether a PTT device is configured (its open may still be pending).
    pub(crate) fn has_device(&self) -> bool {
        match self.state.lock() {
            Ok(state) => state.device.is_some(),
            Err(poisoned) => poisoned.into_inner().device.is_some(),
        }
    }

    fn set_blocking(&self, asserted: bool) -> Result<()> {
        let mut state = self.lock_for(asserted)?;
        if asserted && state.shut {
            return Err(anyhow!(
                "serial PTT line is released for shutdown; refusing to key"
            ));
        }
        let Some(device) = state.device.clone() else {
            if asserted {
                return Err(anyhow!("no serial PTT port is open; refusing to key"));
            }
            return Ok(());
        };
        if state.handle.is_none() {
            if !asserted && !self.is_asserted() {
                // Never asserted since the last confirmed low: nothing to drop.
                return Ok(());
            }
            // Reopening writes RTS low, which is the unkey itself.
            self.open_locked(&mut state, &device)?;
            if !asserted {
                return Ok(());
            }
        }
        if asserted {
            self.maybe_asserted.store(true, Ordering::Release);
        }
        let Some(handle) = state.handle.as_mut() else {
            return Err(anyhow!("serial PTT port {device} has no open handle"));
        };
        match handle.write_rts(asserted) {
            Ok(()) => {
                if !asserted {
                    self.maybe_asserted.store(false, Ordering::Release);
                }
                Ok(())
            }
            Err(e) => {
                // Drop the handle so the next call reopens the port; the line
                // stays "possibly asserted" until a low write is confirmed.
                state.handle = None;
                Err(anyhow!(
                    "serial PTT port {device}: cannot {} RTS: {e}",
                    if asserted { "assert" } else { "drop" }
                ))
            }
        }
    }

    fn release_blocking(&self) -> Result<()> {
        {
            let mut state = self.lock_for(false)?;
            state.shut = true;
        }
        self.set_blocking(false)
    }
}

impl Drop for PttLine {
    /// Lower RTS before the handle closes: a line dropped without
    /// `release_for_shutdown` (the coordinator unwinding after a panic) must
    /// never leave the rig keyed. Best effort; HUPCL backs it on unix.
    fn drop(&mut self) {
        let state = match self.state.get_mut() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(handle) = state.handle.as_mut() {
            if let Err(e) = handle.write_rts(false) {
                tracing::warn!("Serial PTT: could not lower RTS while closing the port: {e}");
            }
        }
    }
}

/// Whether `device` canonicalizes to the CAT port (catches
/// `/dev/serial/by-id` symlinks). Skipped when either path does not resolve,
/// for example `COM3` on Windows. `test-rig --ptt` uses it too.
pub fn resolves_to_cat_port(device: &str, cat_port: &str) -> bool {
    if device == cat_port {
        return true;
    }
    if cat_port.is_empty() {
        return false;
    }
    match (
        std::fs::canonicalize(device),
        std::fs::canonicalize(cat_port),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

async fn run_blocking<F>(body: F) -> Result<()>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    tokio::task::spawn_blocking(body)
        .await
        .map_err(|e| anyhow!("serial PTT task failed: {e}"))?
}

/// A rig keyed by a [`PttLine`] instead of CAT. `set_ptt` drives the line,
/// `get_ptt` reports `On` while the line may be asserted, and every other
/// method forwards to the inner rig with the same arguments and result.
pub(crate) struct LineKeyedRig {
    inner: Arc<dyn RigControl + Send + Sync>,
    line: Arc<PttLine>,
    data_mode: Option<DataModeReadiness>,
    /// Serializes a key-down with the data-mode CAT mutations below, as the
    /// inner `RigctldClient` serializes `T 1` with them under its own lock.
    key_gate: tokio::sync::Mutex<()>,
}

impl LineKeyedRig {
    pub(crate) fn new(inner: Arc<dyn RigControl + Send + Sync>, line: Arc<PttLine>) -> Self {
        Self {
            inner,
            line,
            data_mode: None,
            key_gate: tokio::sync::Mutex::new(()),
        }
    }

    /// The rig's data mode is managed (`rig.data_mode` is set). The inner
    /// `RigctldClient` refuses a mode or VFO change while it knows the rig is
    /// keyed, but under serial RTS it never sees the key-down. So, like it,
    /// this wrapper refuses a key-down while the data mode is not ready,
    /// refuses those CAT mutations while the line may be asserted, and drops
    /// the line before a (re)connect prepares the mode, as the client's own
    /// connect sends `T 0` first (PAN-357 review).
    pub(crate) fn with_data_mode(mut self, readiness: DataModeReadiness) -> Self {
        self.data_mode = Some(readiness);
        self
    }

    /// Under a managed data mode, hold the key gate and refuse while the
    /// line may be asserted. Without one, CAT mutations pass straight through.
    async fn unkeyed_mutation(&self) -> Result<Option<tokio::sync::MutexGuard<'_, ()>>> {
        if self.data_mode.is_none() {
            return Ok(None);
        }
        let gate = self.key_gate.lock().await;
        if self.line.is_asserted() {
            return Err(anyhow!("Cannot change rig data mode while keyed"));
        }
        Ok(Some(gate))
    }
}

// Keep in step with `pancetta-hamlib/src/rig.rs` (`trait RigControl`): every
// trait method, including the defaulted ones, must forward explicitly, or the
// wrapper silently answers with the trait default instead of the rig.
#[async_trait]
impl RigControl for LineKeyedRig {
    async fn receive_gain_capabilities(&self) -> Result<ReceiveGainCapabilities> {
        self.inner.receive_gain_capabilities().await
    }

    async fn get_receive_gain(&self) -> Result<ReceiveGain> {
        self.inner.get_receive_gain().await
    }

    async fn set_receive_gain(
        &self,
        gain: ReceiveGain,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        self.inner.set_receive_gain(gain, deadline).await
    }

    async fn connect(&self) -> Result<()> {
        if self.data_mode.is_none() {
            return self.inner.connect().await;
        }
        let _gate = self.key_gate.lock().await;
        // The client's connect sends `T 0` before it prepares the data mode;
        // the line is this rig's PTT, so drop it first.
        self.line.set(false).await?;
        self.inner.connect().await
    }

    async fn disconnect(&self) -> Result<()> {
        self.inner.disconnect().await
    }

    async fn get_status(&self) -> Result<RigStatus> {
        self.inner.get_status().await
    }

    async fn set_frequency(&self, vfo: Vfo, freq: u64) -> Result<()> {
        let _gate = self.unkeyed_mutation().await?;
        self.inner.set_frequency(vfo, freq).await
    }

    async fn get_frequency(&self, vfo: Vfo) -> Result<u64> {
        self.inner.get_frequency(vfo).await
    }

    async fn set_mode(&self, vfo: Vfo, mode: Mode, width: Option<i32>) -> Result<()> {
        self.inner.set_mode(vfo, mode, width).await
    }

    async fn get_mode(&self, vfo: Vfo) -> Result<(Mode, i32)> {
        self.inner.get_mode(vfo).await
    }

    async fn set_vfo(&self, vfo: Vfo) -> Result<()> {
        let _gate = self.unkeyed_mutation().await?;
        self.inner.set_vfo(vfo).await
    }

    async fn get_vfo(&self) -> Result<Vfo> {
        self.inner.get_vfo().await
    }

    async fn set_ptt(&self, _vfo: Vfo, ptt: PttState) -> Result<()> {
        if ptt == PttState::Off {
            // An unkey never waits for the gate.
            return self.line.set(false).await;
        }
        let _gate = self.key_gate.lock().await;
        if self.data_mode.as_ref().is_some_and(|mode| !mode.is_ready()) {
            return Err(anyhow!("rig data mode is not ready"));
        }
        self.line.set(true).await
    }

    async fn get_ptt(&self, vfo: Vfo) -> Result<PttState> {
        if self.line.is_asserted() {
            return Ok(PttState::On);
        }
        self.inner.get_ptt(vfo).await
    }

    fn connection_generation(&self) -> u64 {
        self.inner.connection_generation()
    }

    async fn get_attenuator(&self) -> Result<i32> {
        self.inner.get_attenuator().await
    }

    async fn set_attenuator(&self, db: i32) -> Result<()> {
        self.inner.set_attenuator(db).await
    }

    async fn supported_attenuator_steps(&self) -> Result<Vec<i32>> {
        self.inner.supported_attenuator_steps().await
    }

    async fn set_power_level(&self, level: f32) -> Result<()> {
        self.inner.set_power_level(level).await
    }

    async fn get_power_level(&self) -> Result<f32> {
        self.inner.get_power_level().await
    }

    async fn set_preamp(&self, preamp_db: i32) -> Result<()> {
        self.inner.set_preamp(preamp_db).await
    }

    async fn get_preamp(&self) -> Result<i32> {
        self.inner.get_preamp().await
    }

    async fn get_s_meter(&self) -> Result<i32> {
        self.inner.get_s_meter().await
    }

    async fn get_swr(&self) -> Result<f32> {
        self.inner.get_swr().await
    }

    async fn reports_swr(&self) -> Result<bool> {
        self.inner.reports_swr().await
    }

    async fn get_tx_meter(&self, meter: TxMeter) -> Result<f32> {
        self.inner.get_tx_meter(meter).await
    }

    async fn tx_meter_levels(&self) -> Result<TxMeterLevels> {
        self.inner.tx_meter_levels().await
    }

    async fn set_memory_channel(&self, vfo: Vfo, channel: i32) -> Result<()> {
        let _gate = self.unkeyed_mutation().await?;
        self.inner.set_memory_channel(vfo, channel).await
    }

    async fn get_memory_channel(&self, vfo: Vfo) -> Result<i32> {
        self.inner.get_memory_channel(vfo).await
    }

    async fn set_scan(&self, vfo: Vfo, enable: bool) -> Result<()> {
        self.inner.set_scan(vfo, enable).await
    }

    async fn get_info(&self) -> Result<String> {
        self.inner.get_info().await
    }

    async fn set_split(&self, enabled: bool, tx_vfo: Vfo) -> Result<()> {
        let _gate = self.unkeyed_mutation().await?;
        self.inner.set_split(enabled, tx_vfo).await
    }

    async fn get_split(&self) -> Result<(bool, Vfo)> {
        self.inner.get_split().await
    }

    async fn get_split_freq(&self) -> Result<u64> {
        self.inner.get_split_freq().await
    }

    async fn set_split_freq(&self, tx_freq: u64) -> Result<()> {
        let _gate = self.unkeyed_mutation().await?;
        self.inner.set_split_freq(tx_freq).await
    }
}

#[cfg(test)]
#[path = "ptt_line/tests/ptt_line_tests.rs"]
mod ptt_line_tests;

#[cfg(test)]
#[path = "ptt_line/tests/tx_frame_rts_tests.rs"]
mod tx_frame_rts_tests;
