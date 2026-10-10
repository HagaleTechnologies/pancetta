//! System-clock skew measurement (PAN-114).
//!
//! One SNTP (RFC 4330) implementation shared by `pancetta doctor` and the
//! live-session clock-skew monitor (`coordinator/clock_monitor.rs`), the
//! thresholds both use, and [`ClockSkewMonitorCore`]: the pure state machine
//! that decides when to probe and which operator-facing diagnostic each
//! reading or clock step produces. The monitor task only runs the schedule
//! and the blocking UDP probe around it.

use chrono::{DateTime, Utc};
use pancetta_core::slot_clock::ClockStepDetector;
use pancetta_core::DiagnosticLevel;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// SNTP server (host:port) queried by doctor and the live monitor.
pub const NTP_SERVER: &str = "pool.ntp.org:123";

/// Host name shown in operator-facing texts.
pub const NTP_SERVER_HOST: &str = "pool.ntp.org";

/// Read/write timeout of one SNTP query.
pub const SNTP_TIMEOUT: Duration = Duration::from_secs(2);

/// Offset at or above which the clock is outside FT8's decode margin. The
/// live capture window spans DT ≈ −0.66…+0.36 s (`WINDOW_LEAD_SECS` and
/// `EXTENDED_WINDOW_LEAD_SECS` in `coordinator/mod.rs`), so a clock 0.3 s
/// off already starts losing late stations.
pub const CLOCK_SKEW_WARN_S: f64 = 0.3;

/// Offset at or above which FT8 decodes fail systematically (doctor FAIL).
pub const CLOCK_SKEW_FAIL_S: f64 = 1.0;

/// Spacing between live SNTP probes. The NTP pool's terms ask SNTP clients
/// for at most one query per 30 minutes (<https://ntppool.org/vendors>).
pub const CLOCK_PROBE_INTERVAL: Duration = Duration::from_secs(1800);

/// Delay before the first live probe after session start.
pub const CLOCK_FIRST_PROBE_DELAY: Duration = Duration::from_secs(30);

/// Delay from a detected clock step to the re-check probe (never sooner than
/// [`CLOCK_PROBE_INTERVAL`] after the previous probe).
pub const CLOCK_STEP_RECHECK_DELAY: Duration = Duration::from_secs(30);

/// Minimum spacing between two `clock.step` diagnostics.
pub const CLOCK_STEP_DIAG_MIN_GAP: Duration = Duration::from_secs(60);

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch (1970-01-01).
const NTP_UNIX_EPOCH_DELTA: f64 = 2_208_988_800.0;

/// Convert an 8-byte NTP timestamp (32.32 fixed point, seconds since 1900)
/// to Unix seconds as f64.
pub(crate) fn ntp_ts_to_unix_f64(b: &[u8]) -> f64 {
    let secs = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64;
    let frac = u32::from_be_bytes([b[4], b[5], b[6], b[7]]) as f64 / 4_294_967_296.0;
    secs + frac - NTP_UNIX_EPOCH_DELTA
}

/// RFC 4330 clock offset from the four timestamps:
/// T1 local send, T2 server receive, T3 server transmit, T4 local receive.
/// Positive = the local clock is BEHIND the server.
pub(crate) fn sntp_offset(t1: f64, t2: f64, t3: f64, t4: f64) -> f64 {
    ((t2 - t1) + (t3 - t4)) / 2.0
}

fn unix_now_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// One-shot SNTP (RFC 4330) query over UDP. Returns the clock offset in
/// seconds. Hand-rolled on purpose: a 48-byte packet is not worth a crate.
pub fn sntp_clock_offset(server: &str, timeout: Duration) -> anyhow::Result<f64> {
    use std::net::UdpSocket;
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(timeout))?;
    sock.set_write_timeout(Some(timeout))?;
    sock.connect(server)?;

    // 48-byte client request: LI=0, VN=4, Mode=3 (client) → first byte 0x23.
    let mut req = [0u8; 48];
    req[0] = 0x23;
    let t1 = unix_now_f64();
    sock.send(&req)?;
    let mut resp = [0u8; 48];
    let n = sock.recv(&mut resp)?;
    let t4 = unix_now_f64();
    anyhow::ensure!(n >= 48, "short SNTP response ({n} bytes)");
    let mode = resp[0] & 0x07;
    anyhow::ensure!(
        mode == 4 || mode == 5,
        "not an SNTP server response (mode {mode})"
    );
    let t2 = ntp_ts_to_unix_f64(&resp[32..40]); // Receive timestamp
    let t3 = ntp_ts_to_unix_f64(&resp[40..48]); // Transmit timestamp
    anyhow::ensure!(t3 > 0.0, "SNTP transmit timestamp is zero");
    Ok(sntp_offset(t1, t2, t3, t4))
}

/// Per-OS one-line instruction for turning on automatic time sync.
pub fn clock_fix_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "System Settings → General → Date & Time → 'Set time automatically'"
    } else if cfg!(target_os = "windows") {
        "run `w32tm /resync` in an admin prompt (or Settings → Time & Language → Sync now)"
    } else {
        "enable an NTP daemon: `sudo apt install chrony` (or systemd-timesyncd)"
    }
}

/// `"0.42 s SLOW"` / `"1.37 s FAST"`. A positive SNTP offset means the local
/// clock is behind UTC (SLOW).
pub fn describe_offset(offset_s: f64) -> String {
    let dir = if offset_s > 0.0 { "SLOW" } else { "FAST" };
    format!("{:.2} s {dir}", offset_s.abs())
}

/// Lock-free handoff of the current clock-skew warning from the monitor task
/// to the TUI relay thread. Stores f64 bits; NaN means "no warning".
#[derive(Debug, Clone)]
pub struct ClockSkewShared(Arc<AtomicU64>);

impl ClockSkewShared {
    /// A handle with no warning set.
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(f64::NAN.to_bits())))
    }

    /// Publish `Some(offset_s)` while warning, `None` otherwise.
    pub fn set_warning(&self, warning: Option<f64>) {
        let bits = warning.unwrap_or(f64::NAN).to_bits();
        self.0.store(bits, Ordering::Release);
    }

    /// The offset currently warned about, if any.
    pub fn warning(&self) -> Option<f64> {
        let v = f64::from_bits(self.0.load(Ordering::Acquire));
        (!v.is_nan()).then_some(v)
    }
}

impl Default for ClockSkewShared {
    fn default() -> Self {
        Self::new()
    }
}

/// Probe/step timing of [`ClockSkewMonitorCore`]. `Default` is the
/// production schedule; tests shrink it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSkewTiming {
    pub first_probe: Duration,
    pub interval: Duration,
    pub step_recheck: Duration,
    pub step_diag_min_gap: Duration,
}

impl Default for ClockSkewTiming {
    fn default() -> Self {
        Self {
            first_probe: CLOCK_FIRST_PROBE_DELAY,
            interval: CLOCK_PROBE_INTERVAL,
            step_recheck: CLOCK_STEP_RECHECK_DELAY,
            step_diag_min_gap: CLOCK_STEP_DIAG_MIN_GAP,
        }
    }
}

/// One operator-facing diagnostic produced by the monitor core.
#[derive(Debug, Clone, PartialEq)]
pub struct SkewDiag {
    /// `"clock.skew"` or `"clock.step"`.
    pub target: &'static str,
    pub level: DiagnosticLevel,
    pub text: String,
}

/// Result of one [`ClockSkewMonitorCore::on_tick`].
#[derive(Debug, Clone, PartialEq)]
pub struct TickOutcome {
    /// Run an SNTP probe now and feed it to [`ClockSkewMonitorCore::on_probe`].
    pub probe_due: bool,
    /// A `clock.step` diagnostic (rate-limited), if a step was detected.
    pub step_diag: Option<SkewDiag>,
}

/// Classification of the latest probe result.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Reading {
    /// No probe has completed yet.
    Pending,
    InSpec,
    Skewed(f64),
    Unreachable,
}

/// Pure clock-skew monitor state machine. Time is passed in: a monotonic
/// "time since start" for scheduling, plus a wall-clock read and the
/// monotonic time since the previous tick for step detection.
#[derive(Debug, Clone)]
pub struct ClockSkewMonitorCore {
    timing: ClockSkewTiming,
    detector: ClockStepDetector,
    reading: Reading,
    /// A clock step since the latest probe invalidated `reading`'s offset.
    invalidated_by_step: bool,
    last_probe_at: Option<Duration>,
    next_probe_at: Duration,
    last_step_diag_at: Option<Duration>,
}

impl ClockSkewMonitorCore {
    /// Production schedule ([`ClockSkewTiming::default`]).
    pub fn new(wall_now: DateTime<Utc>) -> Self {
        Self::with_timing(wall_now, ClockSkewTiming::default())
    }

    pub fn with_timing(wall_now: DateTime<Utc>, timing: ClockSkewTiming) -> Self {
        Self {
            timing,
            detector: ClockStepDetector::new(wall_now),
            reading: Reading::Pending,
            invalidated_by_step: false,
            last_probe_at: None,
            next_probe_at: timing.first_probe,
            last_step_diag_at: None,
        }
    }

    /// Monotonic time (since start) at which the next probe is due.
    #[cfg(test)]
    fn next_probe_at(&self) -> Duration {
        self.next_probe_at
    }

    /// One scheduler tick. `mono_since_last` is monotonic time since the
    /// previous tick (or construction).
    pub fn on_tick(
        &mut self,
        mono_since_start: Duration,
        wall_now: DateTime<Utc>,
        mono_since_last: Duration,
    ) -> TickOutcome {
        let mut step_diag = None;
        if let Some(skew) = self.detector.observe(wall_now, mono_since_last) {
            // The latest reading predates the step, so it no longer describes
            // the clock. Re-check soon, but never sooner than the pool's
            // 30-minute SNTP spacing after the previous probe.
            self.invalidated_by_step = true;
            let earliest = self
                .last_probe_at
                .map(|t| t + self.timing.interval)
                .unwrap_or(self.timing.first_probe);
            self.next_probe_at = earliest.max(mono_since_start + self.timing.step_recheck);

            let diag_allowed = self.last_step_diag_at.is_none_or(|t| {
                mono_since_start.saturating_sub(t) >= self.timing.step_diag_min_gap
            });
            if diag_allowed {
                self.last_step_diag_at = Some(mono_since_start);
                let secs = match skew.num_nanoseconds() {
                    Some(ns) => ns as f64 / 1e9,
                    None => skew.num_milliseconds() as f64 / 1e3,
                };
                step_diag = Some(SkewDiag {
                    target: "clock.step",
                    level: DiagnosticLevel::Warn,
                    text: format!(
                        "system clock jumped {secs:+.1} s (sleep/wake or NTP step) — slot timing re-anchored to UTC"
                    ),
                });
            }
        }
        TickOutcome {
            probe_due: mono_since_start >= self.next_probe_at,
            step_diag,
        }
    }

    /// Feed one probe result taken at `mono_since_start`. Returns a
    /// `clock.skew` diagnostic only when the reported state changes.
    pub fn on_probe(
        &mut self,
        mono_since_start: Duration,
        result: Result<f64, String>,
    ) -> Option<SkewDiag> {
        self.last_probe_at = Some(mono_since_start);
        self.next_probe_at = mono_since_start + self.timing.interval;
        let was_invalidated = std::mem::replace(&mut self.invalidated_by_step, false);
        let prev = self.reading;

        let (next, diag) = match result {
            Ok(offset) if offset.abs() >= CLOCK_SKEW_WARN_S => {
                let raise = !matches!(prev, Reading::Skewed(_)) || was_invalidated;
                let diag = raise.then(|| SkewDiag {
                    target: "clock.skew",
                    level: DiagnosticLevel::Warn,
                    text: format!(
                        "system clock is {} vs {NTP_SERVER_HOST} — past FT8's ~0.3 s decode margin; enable NTP: {}",
                        describe_offset(offset),
                        clock_fix_hint()
                    ),
                });
                (Reading::Skewed(offset), diag)
            }
            Ok(offset) => {
                let text = match prev {
                    Reading::Skewed(_) => Some(format!(
                        "clock skew cleared: offset {offset:+.3} s vs {NTP_SERVER_HOST}"
                    )),
                    Reading::Pending | Reading::Unreachable => Some(format!(
                        "clock check: offset {offset:+.3} s vs {NTP_SERVER_HOST} — OK"
                    )),
                    Reading::InSpec => None,
                };
                let diag = text.map(|text| SkewDiag {
                    target: "clock.skew",
                    level: DiagnosticLevel::Info,
                    text,
                });
                (Reading::InSpec, diag)
            }
            Err(e) => {
                let diag = (prev != Reading::Unreachable).then(|| SkewDiag {
                    target: "clock.skew",
                    level: DiagnosticLevel::Info,
                    text: format!("could not reach {NTP_SERVER_HOST} ({e}) — clock unverified"),
                });
                (Reading::Unreachable, diag)
            }
        };
        self.reading = next;
        diag
    }

    /// The offset to warn about: the latest reading when it is at or past
    /// [`CLOCK_SKEW_WARN_S`] and no clock step has happened since.
    pub fn warning(&self) -> Option<f64> {
        match self.reading {
            Reading::Skewed(offset) if !self.invalidated_by_step => Some(offset),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    /// Drives a core with consistent wall and monotonic clocks; `jump_ms`
    /// moves only the wall clock.
    struct Sim {
        core: ClockSkewMonitorCore,
        base: DateTime<Utc>,
        mono_ms: i64,
        wall_offset_ms: i64,
    }

    impl Sim {
        fn new() -> Self {
            let base = Utc.with_ymd_and_hms(2026, 10, 10, 19, 0, 0).unwrap();
            Self {
                core: ClockSkewMonitorCore::new(base),
                base,
                mono_ms: 0,
                wall_offset_ms: 0,
            }
        }

        fn tick_at(&mut self, mono_s: u64) -> TickOutcome {
            let target_ms = (mono_s * 1000) as i64;
            let since_last = Duration::from_millis((target_ms - self.mono_ms) as u64);
            self.mono_ms = target_ms;
            let wall = self.base + chrono::Duration::milliseconds(target_ms + self.wall_offset_ms);
            self.core.on_tick(secs(mono_s), wall, since_last)
        }

        fn jump_ms(&mut self, ms: i64) {
            self.wall_offset_ms += ms;
        }

        fn probe_at(&mut self, mono_s: u64, r: Result<f64, String>) -> Option<SkewDiag> {
            self.tick_at(mono_s);
            self.core.on_probe(secs(mono_s), r)
        }
    }

    #[test]
    fn ntp_timestamp_conversion_handles_epoch_and_fraction() {
        // NTP epoch is 1900-01-01; Unix is 1970-01-01; delta 2_208_988_800 s.
        let unix_zero: [u8; 8] = [0x83, 0xAA, 0x7E, 0x80, 0, 0, 0, 0]; // 2_208_988_800.0
        assert_eq!(ntp_ts_to_unix_f64(&unix_zero), 0.0);
        // +1 second and a half-fraction (0x8000_0000 / 2^32 = 0.5) → 1.5.
        let one_and_half: [u8; 8] = [0x83, 0xAA, 0x7E, 0x81, 0x80, 0, 0, 0];
        assert!((ntp_ts_to_unix_f64(&one_and_half) - 1.5).abs() < 1e-6);
    }

    #[test]
    fn sntp_offset_recovers_skew_independent_of_symmetric_delay() {
        // Local clock 5 s slow, 200 ms symmetric round trip:
        // T1=100.0 (local send), T2=T3=105.1 (server), T4=100.2 (local recv).
        let offset = sntp_offset(100.0, 105.1, 105.1, 100.2);
        assert!((offset - 5.0).abs() < 1e-9);
        // Zero skew, only delay → offset 0.
        assert!(sntp_offset(100.0, 100.1, 100.1, 100.2).abs() < 1e-9);
    }

    #[test]
    fn positive_offset_is_slow_negative_is_fast() {
        assert_eq!(describe_offset(0.42), "0.42 s SLOW");
        assert_eq!(describe_offset(-1.37), "1.37 s FAST");
    }

    #[test]
    fn shared_warning_round_trips_and_clears() {
        let shared = ClockSkewShared::new();
        assert_eq!(shared.warning(), None);
        let relay = shared.clone();
        shared.set_warning(Some(0.42));
        assert_eq!(relay.warning(), Some(0.42));
        shared.set_warning(None);
        assert_eq!(relay.warning(), None);
    }

    #[test]
    fn first_probe_is_due_after_thirty_seconds() {
        let mut sim = Sim::new();
        assert!(!sim.tick_at(29).probe_due);
        assert!(sim.tick_at(30).probe_due);
    }

    #[test]
    fn in_spec_first_reading_emits_one_info_and_no_warning() {
        let mut sim = Sim::new();
        let diag = sim.probe_at(30, Ok(0.012)).expect("first reading reports");
        assert_eq!(diag.target, "clock.skew");
        assert_eq!(diag.level, DiagnosticLevel::Info);
        assert_eq!(
            diag.text,
            "clock check: offset +0.012 s vs pool.ntp.org — OK"
        );
        assert_eq!(sim.core.warning(), None);
    }

    #[test]
    fn skewed_reading_raises_the_warning_with_fix_text() {
        let mut sim = Sim::new();
        let diag = sim.probe_at(30, Ok(0.42)).expect("skew reports");
        assert_eq!(diag.target, "clock.skew");
        assert_eq!(diag.level, DiagnosticLevel::Warn);
        assert_eq!(
            diag.text,
            format!(
                "system clock is 0.42 s SLOW vs pool.ntp.org — past FT8's ~0.3 s decode margin; enable NTP: {}",
                clock_fix_hint()
            )
        );
        assert_eq!(sim.core.warning(), Some(0.42));
    }

    #[test]
    fn threshold_is_inclusive_at_point_three() {
        for offset in [0.3, -0.3] {
            let mut sim = Sim::new();
            sim.probe_at(30, Ok(offset));
            assert_eq!(sim.core.warning(), Some(offset), "offset {offset}");
        }
        let mut sim = Sim::new();
        sim.probe_at(30, Ok(0.299));
        assert_eq!(sim.core.warning(), None);
    }

    #[test]
    fn back_in_spec_reading_clears_with_info() {
        let mut sim = Sim::new();
        sim.probe_at(30, Ok(0.42));
        let diag = sim.probe_at(1830, Ok(0.004)).expect("clear reports");
        assert_eq!(diag.target, "clock.skew");
        assert_eq!(diag.level, DiagnosticLevel::Info);
        assert_eq!(
            diag.text,
            "clock skew cleared: offset +0.004 s vs pool.ntp.org"
        );
        assert_eq!(sim.core.warning(), None);
    }

    #[test]
    fn unchanged_state_emits_nothing() {
        let mut sim = Sim::new();
        assert!(sim.probe_at(30, Ok(0.012)).is_some());
        assert_eq!(sim.probe_at(1830, Ok(-0.020)), None);
    }

    #[test]
    fn unreachable_reports_once_and_keeps_thirty_minute_cadence() {
        let mut sim = Sim::new();
        let diag = sim
            .probe_at(30, Err("timed out".to_string()))
            .expect("first failure reports");
        assert_eq!(diag.target, "clock.skew");
        assert_eq!(diag.level, DiagnosticLevel::Info);
        assert_eq!(
            diag.text,
            "could not reach pool.ntp.org (timed out) — clock unverified"
        );
        assert_eq!(sim.core.next_probe_at(), secs(1830));
        assert!(!sim.tick_at(1829).probe_due);
        assert_eq!(sim.probe_at(1830, Err("timed out".to_string())), None);
        assert_eq!(sim.core.next_probe_at(), secs(3630));
        assert!(!sim.tick_at(3629).probe_due);
        assert!(sim.tick_at(3630).probe_due);
        assert_eq!(sim.core.warning(), None);
    }

    #[test]
    fn probes_are_spaced_thirty_minutes() {
        let mut sim = Sim::new();
        sim.probe_at(100, Ok(0.0));
        assert!(!sim.tick_at(1899).probe_due);
        assert!(sim.tick_at(1900).probe_due);
        assert_eq!(sim.core.next_probe_at(), secs(1900));
    }

    #[test]
    fn clock_step_clears_warning_and_schedules_recheck_without_beating_the_pool_limit() {
        let mut sim = Sim::new();
        sim.probe_at(100, Ok(0.42));
        assert_eq!(sim.core.warning(), Some(0.42));
        sim.jump_ms(3_604_300);
        let out = sim.tick_at(200);
        assert_eq!(sim.core.warning(), None);
        let diag = out.step_diag.expect("step reports");
        assert_eq!(diag.target, "clock.step");
        assert_eq!(diag.level, DiagnosticLevel::Warn);
        assert_eq!(
            diag.text,
            "system clock jumped +3604.3 s (sleep/wake or NTP step) — slot timing re-anchored to UTC"
        );
        assert!(!out.probe_due);
        assert_eq!(sim.core.next_probe_at(), secs(1900));
        assert!(!sim.tick_at(1899).probe_due);
        assert!(sim.tick_at(1900).probe_due);
    }

    #[test]
    fn clock_step_long_after_last_probe_rechecks_in_thirty_seconds() {
        let mut sim = Sim::new();
        sim.probe_at(100, Ok(0.42));
        sim.jump_ms(-600_000);
        let out = sim.tick_at(4000);
        assert!(out.step_diag.is_some());
        assert_eq!(sim.core.warning(), None);
        assert_eq!(sim.core.next_probe_at(), secs(4030));
        assert!(!sim.tick_at(4029).probe_due);
        assert!(sim.tick_at(4030).probe_due);
    }

    #[test]
    fn clock_step_diagnostics_are_limited_to_one_per_minute() {
        let mut sim = Sim::new();
        sim.probe_at(100, Ok(0.0));
        sim.jump_ms(5_000);
        assert!(sim.tick_at(200).step_diag.is_some());
        sim.jump_ms(5_000);
        assert!(sim.tick_at(230).step_diag.is_none());
        sim.jump_ms(5_000);
        assert!(sim.tick_at(261).step_diag.is_some());
    }
}
