//! PAN-291 TX power and ALC readback while keyed, and the ALC overdrive
//! warning. While the rig is keyed (`swr_guard::poll_sees_keyed`, so single TX,
//! bundles, tune and manual PTT all count) the Hamlib poll loop reads the
//! rig's ALC and forward-power meters on the same 500 ms tick that reads SWR,
//! and sends one `TxMeterResponse` to the TUI per tick. Two consecutive keyed
//! ALC readings above `max_alc` raise one sticky, warn-only alert per keyed
//! episode. Nothing here blocks, delays or halts TX, and no reading is ever
//! invented: a meter the rig does not report stays `None`.

use std::time::{Duration, Instant};

use pancetta_config::PowerProtectionConfig;
use pancetta_core::tx_meter::{ForwardPower, TxMeterCapability, TxMeterReading};
use pancetta_hamlib::{TxMeter, TxMeterLevels};
use tracing::{debug, warn};

use super::swr_guard::to_tui;
use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};

/// Samples belonging to one acknowledged, played FT waveform.
#[derive(Debug, Default)]
pub(crate) struct TxDriveAttempt {
    auto: bool,
    band: Option<pancetta_core::Band>,
    confirmed: std::sync::atomic::AtomicBool,
    started: std::sync::atomic::AtomicBool,
    active: std::sync::atomic::AtomicBool,
    closed: std::sync::atomic::AtomicBool,
    invalidated: std::sync::atomic::AtomicBool,
    samples: std::sync::Mutex<Vec<TxMeterReading>>,
}

impl TxDriveAttempt {
    pub(crate) fn new(auto: bool, band: Option<pancetta_core::Band>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            auto,
            band,
            ..Self::default()
        })
    }
    pub(crate) fn confirm(&self) {
        self.confirmed
            .store(true, std::sync::atomic::Ordering::Release);
        self.active
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn started(&self) {
        self.started
            .store(true, std::sync::atomic::Ordering::Release);
    }
    fn suppress_warn_only(&self) -> bool {
        use std::sync::atomic::Ordering::Acquire;
        self.auto
            && self.confirmed.load(Acquire)
            && self.started.load(Acquire)
            && self.active.load(Acquire)
    }
    pub(crate) fn stop(&self) {
        self.active
            .store(false, std::sync::atomic::Ordering::Release);
    }
    /// Playback ended: later meter reads (the unkey tail) are not part of
    /// the burst. Unlike [`Self::stop`] this keeps the warn-only suppression
    /// until Hamlib acknowledges the PTT-off.
    pub(crate) fn close_sampling(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
    }
    /// Abnormal teardown (PTT watchdog): the burst yields no summary.
    pub(crate) fn invalidate(&self) {
        self.invalidated
            .store(true, std::sync::atomic::Ordering::Release);
        self.stop();
    }
    /// Key-down confirmed, playback started and neither stopped nor closed.
    pub(crate) fn sampling(&self) -> bool {
        use std::sync::atomic::Ordering::Acquire;
        self.confirmed.load(Acquire)
            && self.started.load(Acquire)
            && self.active.load(Acquire)
            && !self.closed.load(Acquire)
    }
    pub(crate) fn sample(&self, reading: TxMeterReading) {
        if self.sampling() {
            if let Ok(mut samples) = self.samples.lock() {
                samples.push(reading);
            }
        }
    }
    pub(crate) fn finish(&self) -> Vec<TxMeterReading> {
        self.summary().unwrap_or_default()
    }
    /// The burst's samples, or `None` when the attempt cannot train drive:
    /// key-down unconfirmed, playback never started, or invalidated. `None`
    /// is not "no ALC": the caller must not report it to the controller.
    pub(crate) fn summary(&self) -> Option<Vec<TxMeterReading>> {
        self.stop();
        if !self.confirmed.load(std::sync::atomic::Ordering::Acquire)
            || !self.started.load(std::sync::atomic::Ordering::Acquire)
            || self.invalidated.load(std::sync::atomic::Ordering::Acquire)
        {
            return None;
        }
        Some(self.samples.lock().map(|s| s.clone()).unwrap_or_default())
    }
}

/// Bound on the once-per-session `tx_meter_levels` capability probe.
pub(crate) const TX_METER_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// Consecutive keyed ALC readings above the limit that raise the warning.
pub(crate) const ALC_OVERDRIVE_RUN: u32 = 2;
const DIAGNOSTIC_TARGET: &str = "rig.alc";
const DEFAULT_MAX_ALC: f32 = pancetta_config::rig::DEFAULT_MAX_ALC;

/// Which meters this Hamlib session reads while keyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TxMeterPlan {
    /// Read the ALC meter.
    pub alc: bool,
    /// The forward-power meter to read, if any.
    pub forward_power: Option<TxMeter>,
}

impl TxMeterPlan {
    /// Read only the meters the rig lists, preferring watts over the
    /// fraction. A failed or timed-out probe (`None`) tries ALC and watts:
    /// the runtime reads decide, as SWR does for `Unknown`.
    pub(crate) fn from_levels(levels: Option<TxMeterLevels>) -> Self {
        match levels {
            Some(levels) => Self {
                alc: levels.alc,
                forward_power: if levels.forward_power_watts {
                    Some(TxMeter::ForwardPowerWatts)
                } else if levels.forward_power_fraction {
                    Some(TxMeter::ForwardPowerFraction)
                } else {
                    None
                },
            },
            None => Self {
                alc: true,
                forward_power: Some(TxMeter::ForwardPowerWatts),
            },
        }
    }

    /// What the TUI should expect from this session.
    pub(crate) fn capability(&self, alc_limit: Option<f32>) -> TxMeterCapability {
        TxMeterCapability {
            forward_power: self.forward_power.is_some(),
            alc: self.alc,
            alc_limit,
        }
    }
}

/// Ask the backend once which TX meters it lists, bounded by
/// [`TX_METER_PROBE_TIMEOUT`]. An error or timeout gives the fallback plan.
pub(crate) async fn probe_tx_meters(
    rig: &(dyn pancetta_hamlib::RigControl + Send + Sync),
) -> TxMeterPlan {
    let levels = match tokio::time::timeout(TX_METER_PROBE_TIMEOUT, rig.tx_meter_levels()).await {
        Ok(Ok(levels)) => Some(levels),
        _ => None,
    };
    TxMeterPlan::from_levels(levels)
}

/// A usable ALC reading: finite and not negative.
fn valid_alc(alc: f32) -> bool {
    alc.is_finite() && alc >= 0.0
}

/// A usable forward-power reading: finite and not negative.
fn valid_forward_power(power: f32) -> bool {
    power.is_finite() && power >= 0.0
}

fn forward_power_from(meter: TxMeter, value: f32) -> Option<ForwardPower> {
    match meter {
        TxMeter::ForwardPowerWatts => Some(ForwardPower::Watts(value)),
        TxMeter::ForwardPowerFraction => Some(ForwardPower::FractionOfMax(value)),
        TxMeter::Alc => None,
    }
}

/// What the ALC guard wants done after one observation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum AlcAction {
    None,
    /// Two consecutive keyed readings above `max_alc`: warn once.
    ReportOverdrive {
        readings: [f32; 2],
        max_alc: f32,
    },
}

/// Pure ALC overdrive rule: counts consecutive valid keyed readings above
/// the limit within one keyed episode.
#[derive(Debug, Clone)]
pub(crate) struct AlcGuard {
    enabled: bool,
    max_alc: f32,
    /// Consecutive valid keyed readings above the limit in the current run.
    run: u32,
    /// The previous high reading in the current run, if the run is open.
    previous_high: Option<f32>,
    /// Already reported in this keyed episode.
    reported: bool,
    last_ptt_epoch: u64,
}

impl AlcGuard {
    pub(crate) fn new(protection: &PowerProtectionConfig) -> Self {
        let enabled = protection.alc_overdrive_warning;
        let configured = protection.max_alc;
        let max_alc = if configured.is_finite() && configured > 0.0 && configured < 1.0 {
            configured
        } else {
            if enabled {
                warn!(
                    target: "rig.alc",
                    "max_alc {} is outside 0.0..1.0; using {}", configured, DEFAULT_MAX_ALC
                );
            }
            DEFAULT_MAX_ALC
        };
        Self {
            enabled,
            max_alc,
            run: 0,
            previous_high: None,
            reported: false,
            last_ptt_epoch: 0,
        }
    }

    pub(crate) fn reset_episode(&mut self) {
        self.run = 0;
        self.previous_high = None;
        self.reported = false;
    }

    /// The limit the TUI should show, only while the warning is on.
    pub(crate) fn alc_limit(&self) -> Option<f32> {
        self.enabled.then_some(self.max_alc)
    }

    /// Observe one poll tick. A missing reading neither counts nor resets;
    /// an unkeyed tick ends the episode and re-arms the report.
    pub(crate) fn observe(&mut self, keyed: bool, alc: Option<f32>) -> AlcAction {
        if !keyed {
            self.run = 0;
            self.previous_high = None;
            self.reported = false;
            return AlcAction::None;
        }
        if !self.enabled {
            return AlcAction::None;
        }
        let Some(alc) = alc.filter(|v| valid_alc(*v)) else {
            return AlcAction::None;
        };
        if alc <= self.max_alc {
            self.run = 0;
            self.previous_high = None;
            return AlcAction::None;
        }
        self.run = self.run.saturating_add(1);
        let previous = self.previous_high.replace(alc);
        match previous {
            Some(previous) if self.run >= ALC_OVERDRIVE_RUN && !self.reported => {
                self.reported = true;
                AlcAction::ReportOverdrive {
                    readings: [previous, alc],
                    max_alc: self.max_alc,
                }
            }
            _ => AlcAction::None,
        }
    }
}

/// The sticky strip text. Constant for a session so repeats count up.
pub(crate) fn overdrive_alert(max_alc: f32) -> String {
    format!("ALC overdrive: reduce audio drive (ALC above {max_alc:.2} while transmitting)")
}

/// The Shift+D and headless log detail.
pub(crate) fn overdrive_detail(readings: [f32; 2], max_alc: f32) -> String {
    format!(
        "ALC read {:.2} then {:.2} while transmitting, above max_alc {:.2}. \
         Press F7 to reduce TX audio drive by 1 dB for the next transmission. \
         If needed, lower the computer's sound-card output level or the rig's USB/DATA input level until \
         ALC stays at or below the limit. TX continues. Set [rig.power_control.protection] \
         alc_overdrive_warning = false to silence this warning",
        readings[0], readings[1], max_alc
    )
}

/// One poll tick of TX metering. Best-effort like every meter read: it never
/// affects poll_ok. Reads only while keyed.
pub(crate) async fn tx_meter_poll_step_with_dial(
    rig: &(dyn pancetta_hamlib::RigControl + Send + Sync),
    plan: &TxMeterPlan,
    guard: &mut AlcGuard,
    keyed: bool,
    bus: &MessageBus,
    rx_dial_hz: &std::sync::atomic::AtomicU64,
    split_tx_hz: &std::sync::atomic::AtomicU64,
) {
    let effective_band = || {
        pancetta_core::Band::from_frequency(pancetta_qso::qso_manager::effective_tx_dial(
            rx_dial_hz.load(std::sync::atomic::Ordering::Acquire),
            split_tx_hz.load(std::sync::atomic::Ordering::Acquire),
        ))
    };
    let band_before = effective_band();
    if !keyed {
        if let Ok(mut slot) = bus.active_tx_drive_attempt.lock() {
            if let Some(attempt) = slot.take() {
                attempt.stop();
            }
        }
    }
    let epoch = bus.gain_tx_epoch.load(std::sync::atomic::Ordering::Acquire);
    if epoch != guard.last_ptt_epoch {
        guard.reset_episode();
        guard.last_ptt_epoch = epoch;
    }
    // PAN-439: only an attempt whose waveform was already playing when the
    // reads began may take them; a read that started before playback is not
    // part of the burst even if playback starts while it runs.
    let episode = bus
        .active_tx_drive_attempt
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .filter(|attempt| attempt.sampling());
    let alc = if keyed {
        let started = Instant::now();
        let alc = if plan.alc {
            rig.get_tx_meter(TxMeter::Alc)
                .await
                .ok()
                .filter(|v| valid_alc(*v))
        } else {
            None
        };
        let forward_power = match plan.forward_power {
            Some(meter) => rig
                .get_tx_meter(meter)
                .await
                .ok()
                .filter(|v| valid_forward_power(*v))
                .and_then(|v| forward_power_from(meter, v)),
            None => None,
        };
        debug!(
            target: "rig.txmeter",
            elapsed_ms = started.elapsed().as_millis() as u64,
            ?alc,
            ?forward_power,
            "TX meter reads"
        );
        if alc.is_some() || forward_power.is_some() {
            if let Some(attempt) = episode {
                let current = bus
                    .active_tx_drive_attempt
                    .lock()
                    .ok()
                    .and_then(|slot| slot.clone());
                if current
                    .as_ref()
                    .is_some_and(|active| std::sync::Arc::ptr_eq(active, &attempt))
                    && band_before == effective_band()
                    && attempt.band == band_before
                {
                    attempt.sample(TxMeterReading { forward_power, alc });
                }
            }
            let _ = bus
                .send_message(ComponentMessage::new(
                    ComponentId::Hamlib,
                    ComponentId::Tui,
                    MessageType::RigControl(
                        crate::message_bus::RigControlMessage::TxMeterResponse(TxMeterReading {
                            forward_power,
                            alc,
                        }),
                    ),
                    Instant::now(),
                ))
                .await;
        }
        alc
    } else {
        None
    };
    let warning = guard.observe(keyed, alc);
    if !bus
        .active_tx_drive_attempt
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .as_ref()
        .is_some_and(|attempt| attempt.suppress_warn_only())
    {
        act(warning, bus).await;
    }
}

#[cfg(test)]
pub(crate) async fn tx_meter_poll_step(
    rig: &(dyn pancetta_hamlib::RigControl + Send + Sync),
    plan: &TxMeterPlan,
    guard: &mut AlcGuard,
    keyed: bool,
    bus: &MessageBus,
) {
    let unknown = std::sync::atomic::AtomicU64::new(0);
    tx_meter_poll_step_with_dial(rig, plan, guard, keyed, bus, &unknown, &unknown).await;
}

/// Carry out an ALC guard decision. Warn-only: it never touches TX.
pub(crate) async fn act(action: AlcAction, bus: &MessageBus) {
    let AlcAction::ReportOverdrive { readings, max_alc } = action else {
        return;
    };
    let detail = overdrive_detail(readings, max_alc);
    warn!(target: "rig.alc", "{}", detail);
    // Best-effort TUI feed: headless has no TUI channel.
    to_tui(
        bus,
        MessageType::Error {
            component_id: ComponentId::Hamlib,
            error_message: overdrive_alert(max_alc),
            error_code: None,
        },
    )
    .await;
    to_tui(
        bus,
        MessageType::DiagnosticEvent {
            target: DIAGNOSTIC_TARGET,
            level: pancetta_core::DiagnosticLevel::Warn,
            text: detail,
            qso_id: None,
            callsign: None,
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_bus::RigControlMessage;
    use anyhow::{anyhow, Result};
    use pancetta_hamlib::{MockRig, MockRigConfig, Mode, PttState, RigControl, RigStatus, Vfo};
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    fn guard_with(max_alc: f32) -> AlcGuard {
        AlcGuard::new(&PowerProtectionConfig {
            max_alc,
            ..Default::default()
        })
    }

    fn run(g: &mut AlcGuard, readings: &[Option<f32>]) -> Vec<AlcAction> {
        readings.iter().map(|r| g.observe(true, *r)).collect()
    }

    fn reports(actions: &[AlcAction]) -> usize {
        actions
            .iter()
            .filter(|a| matches!(a, AlcAction::ReportOverdrive { .. }))
            .count()
    }

    // ---- guard (pure) ----

    #[test]
    fn alc_guard_reports_on_the_second_consecutive_high_reading() {
        let mut g = guard_with(0.5);
        assert_eq!(g.observe(true, Some(0.8)), AlcAction::None);
        assert_eq!(
            g.observe(true, Some(0.9)),
            AlcAction::ReportOverdrive {
                readings: [0.8, 0.9],
                max_alc: 0.5
            }
        );
    }

    #[test]
    fn alc_guard_in_range_reading_resets_the_run() {
        let mut g = guard_with(0.5);
        assert_eq!(reports(&run(&mut g, &[Some(0.8), Some(0.2), Some(0.8)])), 0);
    }

    #[test]
    fn alc_guard_reading_at_the_limit_is_in_range() {
        let mut g = guard_with(0.5);
        assert_eq!(reports(&run(&mut g, &[Some(0.5), Some(0.5)])), 0);
        let mut g = guard_with(0.5);
        assert_eq!(reports(&run(&mut g, &[Some(0.8), Some(0.5), Some(0.8)])), 0);
    }

    #[test]
    fn alc_guard_missing_reading_neither_counts_nor_resets() {
        let mut g = guard_with(0.5);
        let actions = run(&mut g, &[Some(0.8), None, Some(0.8)]);
        assert_eq!(actions[0], AlcAction::None);
        assert_eq!(actions[1], AlcAction::None);
        assert_eq!(
            actions[2],
            AlcAction::ReportOverdrive {
                readings: [0.8, 0.8],
                max_alc: 0.5
            }
        );
        let mut g = guard_with(0.5);
        assert_eq!(reports(&run(&mut g, &[Some(0.8), None])), 0);
    }

    #[test]
    fn alc_guard_reports_once_per_keyed_episode_and_rearms_on_unkey() {
        let mut g = guard_with(0.5);
        assert_eq!(reports(&run(&mut g, &[Some(0.8); 5])), 1);
        assert_eq!(g.observe(false, None), AlcAction::None);
        let again = run(&mut g, &[Some(0.8), Some(0.8)]);
        assert_eq!(reports(&again), 1);
        assert!(matches!(again[1], AlcAction::ReportOverdrive { .. }));
    }

    #[test]
    fn alc_guard_disabled_never_reports() {
        let mut g = AlcGuard::new(&PowerProtectionConfig {
            alc_overdrive_warning: false,
            ..Default::default()
        });
        assert_eq!(g.alc_limit(), None);
        assert_eq!(reports(&run(&mut g, &[Some(0.99); 6])), 0);
    }

    #[test]
    fn alc_guard_invalid_limit_falls_back_to_default() {
        for bad in [f32::NAN, 1.5, 0.0, -0.2, f32::INFINITY] {
            let g = guard_with(bad);
            assert_eq!(g.alc_limit(), Some(0.5), "{bad}");
        }
        assert_eq!(guard_with(0.3).alc_limit(), Some(0.3));
    }

    #[test]
    fn overdrive_alert_text_is_exact() {
        assert_eq!(
            overdrive_alert(0.5),
            "ALC overdrive: reduce audio drive (ALC above 0.50 while transmitting)"
        );
        assert_eq!(
            overdrive_detail([0.81, 0.84], 0.5),
            "ALC read 0.81 then 0.84 while transmitting, above max_alc 0.50. Press F7 to \
             reduce TX audio drive by 1 dB for the next transmission. If needed, lower the \
             computer's sound-card output level or the rig's USB/DATA input level until ALC \
             stays at or below the limit. TX continues. Set \
             [rig.power_control.protection] alc_overdrive_warning = false to silence this \
             warning"
        );
    }

    // ---- plan and probe ----

    #[test]
    fn plan_prefers_watts_then_fraction() {
        let both = TxMeterLevels {
            alc: true,
            forward_power_watts: true,
            forward_power_fraction: true,
        };
        assert_eq!(
            TxMeterPlan::from_levels(Some(both)),
            TxMeterPlan {
                alc: true,
                forward_power: Some(TxMeter::ForwardPowerWatts)
            }
        );
        let fraction = TxMeterLevels {
            alc: false,
            forward_power_watts: false,
            forward_power_fraction: true,
        };
        assert_eq!(
            TxMeterPlan::from_levels(Some(fraction)),
            TxMeterPlan {
                alc: false,
                forward_power: Some(TxMeter::ForwardPowerFraction)
            }
        );
        let neither = TxMeterLevels {
            alc: true,
            ..Default::default()
        };
        assert_eq!(TxMeterPlan::from_levels(Some(neither)).forward_power, None);
        assert_eq!(
            TxMeterPlan::from_levels(Some(TxMeterLevels::default())).capability(Some(0.5)),
            TxMeterCapability {
                forward_power: false,
                alc: false,
                alc_limit: Some(0.5)
            }
        );
    }

    #[test]
    fn plan_after_a_failed_probe_tries_alc_and_watts() {
        assert_eq!(
            TxMeterPlan::from_levels(None),
            TxMeterPlan {
                alc: true,
                forward_power: Some(TxMeter::ForwardPowerWatts)
            }
        );
    }

    #[tokio::test]
    async fn probe_reads_the_rig_levels_and_a_failed_probe_falls_back() {
        let rig = MeterRig::new(Levels::Fixed(TxMeterLevels {
            alc: true,
            forward_power_watts: false,
            forward_power_fraction: true,
        }))
        .await;
        assert_eq!(
            probe_tx_meters(&rig).await,
            TxMeterPlan {
                alc: true,
                forward_power: Some(TxMeter::ForwardPowerFraction)
            }
        );
        let failing = MeterRig::new(Levels::Fail).await;
        assert_eq!(
            probe_tx_meters(&failing).await,
            TxMeterPlan::from_levels(None)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn probe_times_out_to_the_fallback_plan() {
        let rig = MeterRig::new(Levels::Hang).await;
        let started = tokio::time::Instant::now();
        assert_eq!(probe_tx_meters(&rig).await, TxMeterPlan::from_levels(None));
        assert!(started.elapsed() >= TX_METER_PROBE_TIMEOUT);
    }

    // ---- poll step (bus level) ----

    enum Levels {
        Fixed(TxMeterLevels),
        Fail,
        Hang,
    }

    /// Delegates required methods to `MockRig`, counts reads per meter, and
    /// answers each meter from a scripted queue (empty means an error).
    struct MeterRig {
        inner: MockRig,
        levels: Levels,
        scripts: Mutex<HashMap<TxMeter, VecDeque<Result<f32>>>>,
        reads: Mutex<HashMap<TxMeter, usize>>,
        /// Marks this attempt's playback as started during the next read.
        start_on_read: Mutex<Option<std::sync::Arc<TxDriveAttempt>>>,
    }

    impl MeterRig {
        async fn new(levels: Levels) -> Self {
            let inner = MockRig::new(MockRigConfig {
                connection_delay_ms: 0,
                operation_delay_ms: 0,
                ..Default::default()
            });
            inner.connect().await.unwrap();
            Self {
                inner,
                levels,
                scripts: Mutex::new(HashMap::new()),
                reads: Mutex::new(HashMap::new()),
                start_on_read: Mutex::new(None),
            }
        }

        fn script(&self, meter: TxMeter, values: Vec<Result<f32>>) {
            self.scripts
                .lock()
                .unwrap()
                .insert(meter, values.into_iter().collect());
        }

        fn reads(&self, meter: TxMeter) -> usize {
            self.reads.lock().unwrap().get(&meter).copied().unwrap_or(0)
        }
    }

    #[async_trait::async_trait]
    impl RigControl for MeterRig {
        async fn connect(&self) -> Result<()> {
            self.inner.connect().await
        }
        async fn disconnect(&self) -> Result<()> {
            self.inner.disconnect().await
        }
        async fn get_status(&self) -> Result<RigStatus> {
            self.inner.get_status().await
        }
        async fn set_frequency(&self, vfo: Vfo, freq: u64) -> Result<()> {
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
            self.inner.set_vfo(vfo).await
        }
        async fn get_vfo(&self) -> Result<Vfo> {
            self.inner.get_vfo().await
        }
        async fn set_ptt(&self, vfo: Vfo, ptt: PttState) -> Result<()> {
            self.inner.set_ptt(vfo, ptt).await
        }
        async fn get_ptt(&self, vfo: Vfo) -> Result<PttState> {
            self.inner.get_ptt(vfo).await
        }
        async fn set_power_level(&self, level: f32) -> Result<()> {
            self.inner.set_power_level(level).await
        }
        async fn get_power_level(&self) -> Result<f32> {
            self.inner.get_power_level().await
        }
        async fn get_s_meter(&self) -> Result<i32> {
            self.inner.get_s_meter().await
        }
        async fn get_swr(&self) -> Result<f32> {
            self.inner.get_swr().await
        }
        async fn set_memory_channel(&self, vfo: Vfo, channel: i32) -> Result<()> {
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
        async fn tx_meter_levels(&self) -> Result<TxMeterLevels> {
            match &self.levels {
                Levels::Fixed(levels) => Ok(*levels),
                Levels::Fail => Err(anyhow!("dump_caps failed")),
                Levels::Hang => std::future::pending().await,
            }
        }
        async fn get_tx_meter(&self, meter: TxMeter) -> Result<f32> {
            *self.reads.lock().unwrap().entry(meter).or_insert(0) += 1;
            if let Some(attempt) = self.start_on_read.lock().unwrap().take() {
                attempt.started();
            }
            self.scripts
                .lock()
                .unwrap()
                .get_mut(&meter)
                .and_then(|q| q.pop_front())
                .unwrap_or_else(|| Err(anyhow!("no scripted {meter:?} reading")))
        }
    }

    struct Inboxes {
        bus: MessageBus,
        qso: crossbeam_channel::Receiver<ComponentMessage>,
        tui: crossbeam_channel::Receiver<ComponentMessage>,
        hamlib: crossbeam_channel::Receiver<ComponentMessage>,
        gateway: crossbeam_channel::Receiver<ComponentMessage>,
    }

    async fn inboxes() -> Inboxes {
        let bus = MessageBus::new(16).unwrap();
        let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
        let (_, tui) = bus.create_channel(ComponentId::Tui).await.unwrap();
        let (_, hamlib) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        let (_, gateway) = bus
            .create_channel(ComponentId::RemoteGateway)
            .await
            .unwrap();
        Inboxes {
            bus,
            qso,
            tui,
            hamlib,
            gateway,
        }
    }

    fn drain(rx: &crossbeam_channel::Receiver<ComponentMessage>) -> Vec<MessageType> {
        rx.try_iter().map(|m| m.message_type).collect()
    }

    fn readings(messages: &[MessageType]) -> Vec<TxMeterReading> {
        messages
            .iter()
            .filter_map(|m| match m {
                MessageType::RigControl(RigControlMessage::TxMeterResponse(r)) => Some(*r),
                _ => None,
            })
            .collect()
    }

    fn overdrive_errors(messages: &[MessageType]) -> Vec<(String, Option<u32>)> {
        messages
            .iter()
            .filter_map(|m| match m {
                MessageType::Error {
                    component_id: ComponentId::Hamlib,
                    error_message,
                    error_code,
                } if error_message.starts_with("ALC overdrive") => {
                    Some((error_message.clone(), *error_code))
                }
                _ => None,
            })
            .collect()
    }

    const BOTH: TxMeterPlan = TxMeterPlan {
        alc: true,
        forward_power: Some(TxMeter::ForwardPowerWatts),
    };

    #[tokio::test]
    async fn step_keyed_reads_each_planned_meter_once_and_sends_one_reading() {
        let ib = inboxes().await;
        let rig = MeterRig::new(Levels::Fail).await;
        rig.script(TxMeter::Alc, vec![Ok(0.12)]);
        rig.script(TxMeter::ForwardPowerWatts, vec![Ok(48.0)]);
        let mut g = guard_with(0.5);
        tx_meter_poll_step(&rig, &BOTH, &mut g, true, &ib.bus).await;
        assert_eq!(rig.reads(TxMeter::Alc), 1);
        assert_eq!(rig.reads(TxMeter::ForwardPowerWatts), 1);
        assert_eq!(rig.reads(TxMeter::ForwardPowerFraction), 0);
        assert_eq!(
            readings(&drain(&ib.tui)),
            vec![TxMeterReading {
                forward_power: Some(ForwardPower::Watts(48.0)),
                alc: Some(0.12),
            }]
        );
    }

    #[tokio::test]
    async fn step_unkeyed_reads_no_meter() {
        let ib = inboxes().await;
        let rig = MeterRig::new(Levels::Fail).await;
        rig.script(TxMeter::Alc, vec![Ok(0.9)]);
        rig.script(TxMeter::ForwardPowerWatts, vec![Ok(48.0)]);
        let mut g = guard_with(0.5);
        tx_meter_poll_step(&rig, &BOTH, &mut g, false, &ib.bus).await;
        assert_eq!(rig.reads(TxMeter::Alc), 0);
        assert_eq!(rig.reads(TxMeter::ForwardPowerWatts), 0);
        assert!(drain(&ib.tui).is_empty());
    }

    #[tokio::test]
    async fn step_skips_meters_the_rig_does_not_report() {
        let ib = inboxes().await;
        let rig = MeterRig::new(Levels::Fail).await;
        rig.script(TxMeter::Alc, vec![Ok(0.2)]);
        let plan = TxMeterPlan {
            alc: true,
            forward_power: None,
        };
        let mut g = guard_with(0.5);
        tx_meter_poll_step(&rig, &plan, &mut g, true, &ib.bus).await;
        assert_eq!(rig.reads(TxMeter::Alc), 1);
        assert_eq!(rig.reads(TxMeter::ForwardPowerWatts), 0);
        assert_eq!(rig.reads(TxMeter::ForwardPowerFraction), 0);
        assert_eq!(
            readings(&drain(&ib.tui)),
            vec![TxMeterReading {
                forward_power: None,
                alc: Some(0.2),
            }]
        );
    }

    #[tokio::test]
    async fn step_drops_invalid_readings_and_keeps_the_other_meter() {
        let ib = inboxes().await;
        let rig = MeterRig::new(Levels::Fail).await;
        let mut g = guard_with(0.5);
        rig.script(TxMeter::Alc, vec![Ok(f32::NAN), Ok(0.2), Ok(0.2)]);
        rig.script(
            TxMeter::ForwardPowerWatts,
            vec![Ok(48.0), Ok(-1.0), Err(anyhow!("RPRT -11"))],
        );
        for _ in 0..3 {
            tx_meter_poll_step(&rig, &BOTH, &mut g, true, &ib.bus).await;
        }
        assert_eq!(
            readings(&drain(&ib.tui)),
            vec![
                TxMeterReading {
                    forward_power: Some(ForwardPower::Watts(48.0)),
                    alc: None,
                },
                TxMeterReading {
                    forward_power: None,
                    alc: Some(0.2),
                },
                TxMeterReading {
                    forward_power: None,
                    alc: Some(0.2),
                },
            ]
        );
    }

    #[tokio::test]
    async fn step_sends_nothing_when_no_reading_is_valid() {
        let ib = inboxes().await;
        let rig = MeterRig::new(Levels::Fail).await;
        rig.script(TxMeter::Alc, vec![Ok(f32::INFINITY)]);
        rig.script(TxMeter::ForwardPowerWatts, vec![Err(anyhow!("N/A"))]);
        let mut g = guard_with(0.5);
        tx_meter_poll_step(&rig, &BOTH, &mut g, true, &ib.bus).await;
        assert_eq!(rig.reads(TxMeter::Alc), 1);
        assert_eq!(rig.reads(TxMeter::ForwardPowerWatts), 1);
        assert!(drain(&ib.tui).is_empty());
    }

    #[tokio::test]
    async fn step_two_high_alc_ticks_raise_one_warn_only_alert() {
        let ib = inboxes().await;
        let rig = MeterRig::new(Levels::Fail).await;
        rig.script(TxMeter::Alc, vec![Ok(0.8), Ok(0.8), Ok(0.8)]);
        rig.script(
            TxMeter::ForwardPowerWatts,
            vec![Ok(48.0), Ok(48.0), Ok(48.0)],
        );
        let mut g = guard_with(0.5);
        for _ in 0..3 {
            tx_meter_poll_step(&rig, &BOTH, &mut g, true, &ib.bus).await;
        }
        let tui = drain(&ib.tui);
        assert_eq!(readings(&tui).len(), 3);
        assert_eq!(
            overdrive_errors(&tui),
            vec![(overdrive_alert(0.5), None)],
            "{tui:?}"
        );
        assert_eq!(
            overdrive_alert(0.5),
            "ALC overdrive: reduce audio drive (ALC above 0.50 while transmitting)"
        );
        let diagnostics: Vec<_> = tui
            .iter()
            .filter(|m| {
                matches!(
                    m,
                    MessageType::DiagnosticEvent {
                        target: "rig.alc",
                        level: pancetta_core::DiagnosticLevel::Warn,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(diagnostics.len(), 1);
        assert!(drain(&ib.hamlib).is_empty());
        assert!(drain(&ib.qso).is_empty());
        assert!(drain(&ib.gateway).is_empty());
    }

    #[tokio::test]
    async fn step_overdrive_rearms_after_unkey() {
        let ib = inboxes().await;
        let rig = MeterRig::new(Levels::Fail).await;
        rig.script(TxMeter::Alc, (0..4).map(|_| Ok(0.8)).collect());
        let plan = TxMeterPlan {
            alc: true,
            forward_power: None,
        };
        let mut g = guard_with(0.5);
        for keyed in [true, true, false, true, true] {
            tx_meter_poll_step(&rig, &plan, &mut g, keyed, &ib.bus).await;
        }
        let errors = overdrive_errors(&drain(&ib.tui));
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0], errors[1]);
    }

    #[tokio::test]
    async fn overdrive_without_a_tui_channel_completes() {
        let bus = MessageBus::new(16).unwrap();
        act(
            AlcAction::ReportOverdrive {
                readings: [0.8, 0.9],
                max_alc: 0.5,
            },
            &bus,
        )
        .await;
        let rig = MeterRig::new(Levels::Fail).await;
        rig.script(TxMeter::Alc, vec![Ok(0.8), Ok(0.8)]);
        let mut g = guard_with(0.5);
        for _ in 0..2 {
            tx_meter_poll_step(&rig, &BOTH, &mut g, true, &bus).await;
        }
    }

    // ---- cadence ----

    #[test]
    fn keyed_poll_interval_gives_two_readings_per_second() {
        assert!(super::super::hamlib::HAMLIB_POLL_INTERVAL <= Duration::from_millis(500));
    }
    #[test]
    fn rekey_rearms_without_an_idle_poll() {
        let protection = PowerProtectionConfig::default();
        let mut guard = AlcGuard::new(&protection);
        assert_eq!(guard.observe(true, Some(0.8)), AlcAction::None);
        assert!(matches!(
            guard.observe(true, Some(0.9)),
            AlcAction::ReportOverdrive { .. }
        ));
        guard.reset_episode();
        assert_eq!(guard.observe(true, Some(0.8)), AlcAction::None);
        assert!(matches!(
            guard.observe(true, Some(0.9)),
            AlcAction::ReportOverdrive { .. }
        ));
    }

    #[test]
    fn stale_poll_cannot_update_a_new_burst() {
        let band = pancetta_core::Band::from_frequency(14_074_000);
        let old = TxDriveAttempt::new(true, band);
        old.confirm();
        old.started();
        old.stop();
        old.sample(TxMeterReading {
            alc: Some(0.8),
            forward_power: None,
        });
        assert!(old.finish().is_empty());
        let new = TxDriveAttempt::new(true, band);
        new.confirm();
        new.started();
        new.sample(TxMeterReading {
            alc: Some(0.2),
            forward_power: None,
        });
        assert_eq!(new.finish().len(), 1);
    }

    // PAN-439: resolves to `tx_meter/tests/drive_attempt_tests.rs` and shares
    // this module's `MeterRig` fixture.
    #[path = "drive_attempt_tests.rs"]
    mod drive_attempt_tests;

    #[test]
    fn preplay_and_invalidated_bursts_do_not_train() {
        let band = pancetta_core::Band::from_frequency(14_074_000);
        let attempt = TxDriveAttempt::new(true, band);
        attempt.confirm();
        attempt.sample(TxMeterReading {
            alc: Some(0.8),
            forward_power: None,
        });
        assert!(attempt.finish().is_empty());
        attempt.started();
        attempt.sample(TxMeterReading {
            alc: Some(0.8),
            forward_power: None,
        });
        assert!(attempt.finish().is_empty());
    }
}
