//! Live-session clock-skew monitor (PAN-114).
//!
//! A lightweight tokio task that runs [`ClockSkewMonitorCore`]'s schedule:
//! one SNTP probe to pool.ntp.org 30 s after start and then every 30 min
//! (plus a re-check after a detected clock step). The blocking query (DNS
//! resolution included, which no socket timeout bounds) runs on a detached
//! OS thread awaited under [`ClockMonitorSchedule::probe_deadline`], never on
//! tokio's blocking pool: a started `spawn_blocking` task cannot be aborted
//! and would hold runtime shutdown for as long as a stalled resolver does. It publishes the current warning through
//! [`ClockSkewShared`] (read by the TUI relay's 2 s `PipelineHealth` tick)
//! and emits each `clock.skew` / `clock.step` diagnostic both to the TUI
//! Diagnostics overlay and to `tracing`, because headless mode drops the TUI
//! bus receiver.
//!
//! Advisory only: it is not registered with the supervisor (a crash must not
//! trigger restart semantics) and it is never started under `--replay` or
//! `--wav` (see `ApplicationCoordinator::replay_mode`).

use crate::clock_skew::{
    ClockSkewMonitorCore, ClockSkewShared, ClockSkewTiming, SkewDiag, SNTP_TIMEOUT,
};
use crate::message_bus::{ComponentId, DiagnosticLevel, MessageBus};
use chrono::{DateTime, Utc};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Default [`ClockMonitorSchedule::probe_deadline`]: the SNTP send and
/// receive timeouts plus headroom for name resolution.
const CLOCK_PROBE_DEADLINE: Duration = Duration::from_secs(4 * SNTP_TIMEOUT.as_secs());

/// Timing of the monitor task. `Default` is the production schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClockMonitorSchedule {
    /// Scheduler tick; also the step-detection sampling period.
    pub tick: Duration,
    pub first_probe: Duration,
    pub interval: Duration,
    pub step_recheck: Duration,
    pub step_diag_min_gap: Duration,
    /// Upper bound on one probe, DNS resolution included. A probe still
    /// running at the deadline is reported as unreachable and left to finish
    /// on its detached thread; no new probe starts until it does.
    pub probe_deadline: Duration,
}

impl Default for ClockMonitorSchedule {
    fn default() -> Self {
        let timing = ClockSkewTiming::default();
        Self {
            tick: Duration::from_secs(1),
            first_probe: timing.first_probe,
            interval: timing.interval,
            step_recheck: timing.step_recheck,
            step_diag_min_gap: timing.step_diag_min_gap,
            probe_deadline: CLOCK_PROBE_DEADLINE,
        }
    }
}

impl ClockMonitorSchedule {
    fn timing(&self) -> ClockSkewTiming {
        ClockSkewTiming {
            first_probe: self.first_probe,
            interval: self.interval,
            step_recheck: self.step_recheck,
            step_diag_min_gap: self.step_diag_min_gap,
        }
    }
}

/// SNTP probe: the clock offset in seconds (positive = local clock behind).
pub(crate) type ClockProbe = Arc<dyn Fn() -> Result<f64, String> + Send + Sync>;

/// Wall-clock source (injected so tests can step it).
pub(crate) type WallClock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Spawn the monitor task. It exits on the first tick after `shutdown` is
/// set.
pub(crate) fn spawn_clock_skew_monitor(
    bus: MessageBus,
    shared: ClockSkewShared,
    shutdown: Arc<AtomicBool>,
    schedule: ClockMonitorSchedule,
    probe: ClockProbe,
    wall_now: WallClock,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let start = std::time::Instant::now();
        let mut last_tick = start;
        let mut core = ClockSkewMonitorCore::with_timing(wall_now(), schedule.timing());
        let in_flight = Arc::new(AtomicBool::new(false));
        let mut ticker = tokio::time::interval(schedule.tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            let now = std::time::Instant::now();
            let mono_since_start = now.duration_since(start);
            let mono_since_last = now.duration_since(last_tick);
            last_tick = now;

            let outcome = core.on_tick(mono_since_start, wall_now(), mono_since_last);
            shared.set_warning(core.warning());
            if let Some(diag) = outcome.step_diag {
                emit(&bus, &diag).await;
            }

            if outcome.probe_due {
                let result = run_probe(&probe, &in_flight, schedule.probe_deadline).await;
                let diag = core.on_probe(mono_since_start, result);
                shared.set_warning(core.warning());
                if let Some(diag) = diag {
                    emit(&bus, &diag).await;
                }
            }
        }
    })
}

/// Clears the in-flight flag when the probe thread finishes, panics included.
struct InFlightGuard(Arc<AtomicBool>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Run `probe` on a detached OS thread and wait at most `deadline` for it.
///
/// Detached so that neither aborting the monitor task nor shutting the
/// runtime down ever waits on a hung resolver (process exit does not join
/// it). `in_flight` caps the leak at one thread: while an earlier probe is
/// still stuck, the next one fails fast instead of spawning another.
async fn run_probe(
    probe: &ClockProbe,
    in_flight: &Arc<AtomicBool>,
    deadline: Duration,
) -> Result<f64, String> {
    if in_flight.swap(true, Ordering::AcqRel) {
        return Err("previous probe still pending".to_string());
    }
    let guard = InFlightGuard(in_flight.clone());
    let (tx, rx) = tokio::sync::oneshot::channel();
    let p = probe.clone();
    let spawned = std::thread::Builder::new()
        .name("clock-skew-probe".to_string())
        .spawn(move || {
            let _guard = guard;
            let _ = tx.send(p());
        });
    if let Err(e) = spawned {
        // The closure (and with it the guard) was dropped: flag is clear.
        return Err(format!("probe thread failed to start: {e}"));
    }
    match tokio::time::timeout(deadline, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("probe thread panicked".to_string()),
        Err(_) => Err(format!("timed out after {} s", deadline.as_secs_f64())),
    }
}

/// Send `diag` to the Diagnostics overlay and log it, so headless runs (no
/// TUI receiver) still show it on the console and in the log file.
async fn emit(bus: &MessageBus, diag: &SkewDiag) {
    let prefix = if diag.target == "clock.step" {
        "clock step"
    } else {
        "clock skew"
    };
    match diag.level {
        DiagnosticLevel::Info => info!("{prefix}: {}", diag.text),
        _ => warn!("{prefix}: {}", diag.text),
    }
    super::tx::emit_diagnostic_full(
        bus,
        ComponentId::Coordinator,
        diag.target,
        diag.level,
        diag.text.clone(),
        None,
        None,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_bus::{ComponentMessage, MessageType};
    use crossbeam_channel::Receiver;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicI64, AtomicUsize};
    use std::sync::Mutex;
    use std::time::Instant;

    const WAIT: Duration = Duration::from_secs(3);

    fn test_schedule() -> ClockMonitorSchedule {
        ClockMonitorSchedule {
            tick: Duration::from_millis(10),
            first_probe: Duration::from_millis(20),
            interval: Duration::from_millis(150),
            step_recheck: Duration::from_millis(20),
            step_diag_min_gap: Duration::from_millis(50),
            probe_deadline: Duration::from_secs(1),
        }
    }

    /// Probe that replays `script` (repeating the last entry) and records
    /// when each call happened.
    fn scripted_probe(script: Vec<Result<f64, String>>) -> (ClockProbe, Arc<Mutex<Vec<Instant>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(VecDeque::from(script)));
        let calls_probe = calls.clone();
        let probe: ClockProbe = Arc::new(move || {
            calls_probe.lock().unwrap().push(Instant::now());
            let mut q = queue.lock().unwrap();
            if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                q.front().cloned().unwrap()
            }
        });
        (probe, calls)
    }

    /// Probe that blocks (polling every 5 ms, not spinning) until `release`
    /// is set, then reports +0.012 s. Counts calls.
    fn hung_probe() -> (ClockProbe, Arc<AtomicUsize>, Arc<AtomicBool>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));
        let (c, r) = (calls.clone(), release.clone());
        let probe: ClockProbe = Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            while !r.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(0.012)
        });
        (probe, calls, release)
    }

    /// Releases a [`hung_probe`] when dropped, so a failed assertion cannot
    /// leave its thread blocked.
    struct ReleaseOnDrop(Arc<AtomicBool>);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// `Utc::now()` plus a test-controlled offset in milliseconds.
    fn offset_wall_clock() -> (WallClock, Arc<AtomicI64>) {
        let offset_ms = Arc::new(AtomicI64::new(0));
        let off = offset_ms.clone();
        let wall: WallClock = Arc::new(move || {
            Utc::now() + chrono::Duration::milliseconds(off.load(Ordering::SeqCst))
        });
        (wall, offset_ms)
    }

    struct Diag {
        source: ComponentId,
        target: &'static str,
        level: DiagnosticLevel,
        text: String,
    }

    async fn next_diag(rx: &Receiver<ComponentMessage>) -> Diag {
        let deadline = Instant::now() + WAIT;
        loop {
            while let Ok(msg) = rx.try_recv() {
                if let MessageType::DiagnosticEvent {
                    target,
                    level,
                    text,
                    ..
                } = msg.message_type
                {
                    return Diag {
                        source: msg.source,
                        target,
                        level,
                        text,
                    };
                }
            }
            assert!(
                Instant::now() < deadline,
                "no DiagnosticEvent within {WAIT:?}"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    async fn wait_for_warning(shared: &ClockSkewShared, want: Option<f64>) {
        let deadline = Instant::now() + WAIT;
        while shared.warning() != want {
            assert!(
                Instant::now() < deadline,
                "shared warning stayed {:?}, wanted {want:?}",
                shared.warning()
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn monitor_raises_and_clears_the_shared_warning_and_emits_diagnostics() {
        let bus = MessageBus::new(16).unwrap();
        let (_tx, rx) = bus.create_channel(ComponentId::Tui).await.unwrap();
        let shared = ClockSkewShared::new();
        let shutdown = Arc::new(AtomicBool::new(false));
        let (probe, _calls) = scripted_probe(vec![Ok(0.012), Ok(0.42), Ok(0.004)]);
        let (wall, _offset) = offset_wall_clock();
        let handle = spawn_clock_skew_monitor(
            bus.clone(),
            shared.clone(),
            shutdown.clone(),
            test_schedule(),
            probe,
            wall,
        );

        let d = next_diag(&rx).await;
        assert_eq!(d.level, DiagnosticLevel::Info);
        assert_eq!(d.text, "clock check: offset +0.012 s vs pool.ntp.org — OK");
        assert_eq!(shared.warning(), None);
        assert_eq!(
            (d.source, d.target),
            (ComponentId::Coordinator, "clock.skew")
        );

        let d = next_diag(&rx).await;
        assert_eq!(d.level, DiagnosticLevel::Warn);
        assert!(
            d.text.starts_with(
                "system clock is 0.42 s SLOW vs pool.ntp.org — past FT8's ~0.3 s decode margin; enable NTP: "
            ),
            "{}",
            d.text
        );
        assert_eq!(
            (d.source, d.target),
            (ComponentId::Coordinator, "clock.skew")
        );
        wait_for_warning(&shared, Some(0.42)).await;

        let d = next_diag(&rx).await;
        assert_eq!(d.level, DiagnosticLevel::Info);
        assert_eq!(
            d.text,
            "clock skew cleared: offset +0.004 s vs pool.ntp.org"
        );
        assert_eq!(
            (d.source, d.target),
            (ComponentId::Coordinator, "clock.skew")
        );
        wait_for_warning(&shared, None).await;

        shutdown.store(true, Ordering::Release);
        tokio::time::timeout(WAIT, handle).await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn monitor_step_invalidates_warning_and_reports_clock_step() {
        let bus = MessageBus::new(16).unwrap();
        let (_tx, rx) = bus.create_channel(ComponentId::Tui).await.unwrap();
        let shared = ClockSkewShared::new();
        let shutdown = Arc::new(AtomicBool::new(false));
        let (probe, calls) = scripted_probe(vec![Ok(0.42), Ok(0.0)]);
        let (wall, offset_ms) = offset_wall_clock();
        let schedule = test_schedule();
        let handle = spawn_clock_skew_monitor(
            bus.clone(),
            shared.clone(),
            shutdown.clone(),
            schedule,
            probe,
            wall,
        );

        let d = next_diag(&rx).await;
        assert_eq!((d.target, d.level), ("clock.skew", DiagnosticLevel::Warn));
        wait_for_warning(&shared, Some(0.42)).await;
        assert_eq!(calls.lock().unwrap().len(), 1);

        let stepped_at = Instant::now();
        offset_ms.fetch_add(3_600_000, Ordering::SeqCst);

        let d = next_diag(&rx).await;
        assert_eq!(d.source, ComponentId::Coordinator);
        assert_eq!((d.target, d.level), ("clock.step", DiagnosticLevel::Warn));
        assert!(
            d.text
                .starts_with("system clock jumped +3600.0 s (sleep/wake or NTP step)"),
            "{}",
            d.text
        );
        wait_for_warning(&shared, None).await;

        let d = next_diag(&rx).await;
        assert_eq!((d.target, d.level), ("clock.skew", DiagnosticLevel::Info));
        assert_eq!(
            d.text,
            "clock skew cleared: offset +0.000 s vs pool.ntp.org"
        );
        let recheck_at = calls.lock().unwrap()[1];
        assert!(
            recheck_at.duration_since(stepped_at) >= schedule.step_recheck,
            "re-check ran {:?} after the step",
            recheck_at.duration_since(stepped_at)
        );
        assert_eq!(shared.warning(), None);

        shutdown.store(true, Ordering::Release);
        tokio::time::timeout(WAIT, handle).await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn monitor_exits_on_shutdown() {
        let bus = MessageBus::new(16).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let schedule = ClockMonitorSchedule {
            tick: Duration::from_millis(50),
            first_probe: Duration::from_secs(3600),
            ..test_schedule()
        };
        let (probe, calls) = scripted_probe(vec![Ok(0.0)]);
        let (wall, _offset) = offset_wall_clock();
        let handle = spawn_clock_skew_monitor(
            bus,
            ClockSkewShared::new(),
            shutdown.clone(),
            schedule,
            probe,
            wall,
        );
        tokio::time::sleep(schedule.tick).await;
        assert!(!handle.is_finished());

        shutdown.store(true, Ordering::Release);
        tokio::time::timeout(schedule.tick * 3, handle)
            .await
            .expect("monitor must exit within 3 ticks of shutdown")
            .unwrap();
        assert!(calls.lock().unwrap().is_empty());
    }

    /// A probe stuck in name resolution must time out, must not be stacked
    /// by later probes, and must not hold runtime shutdown (Codex P2 on
    /// #413: `spawn_blocking` work cannot be aborted).
    #[test]
    fn hung_probe_times_out_and_does_not_hold_runtime_shutdown() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (probe, calls, release) = hung_probe();
        let _release = ReleaseOnDrop(release.clone());
        let schedule = ClockMonitorSchedule {
            probe_deadline: Duration::from_millis(50),
            ..test_schedule()
        };
        let (wall, _offset) = offset_wall_clock();
        let shared = ClockSkewShared::new();
        let (bus, _tx, rx, handle) = rt.block_on(async {
            let bus = MessageBus::new(16).unwrap();
            let (tx, rx) = bus.create_channel(ComponentId::Tui).await.unwrap();
            let handle = spawn_clock_skew_monitor(
                bus.clone(),
                shared.clone(),
                Arc::new(AtomicBool::new(false)),
                schedule,
                probe,
                wall,
            );
            (bus, tx, rx, handle)
        });

        let d = rt.block_on(next_diag(&rx));
        assert_eq!((d.target, d.level), ("clock.skew", DiagnosticLevel::Info));
        assert_eq!(
            d.text,
            "could not reach pool.ntp.org (timed out after 0.05 s) — clock unverified"
        );

        // Later probes come due while the first is still stuck; they fail
        // fast instead of spawning another thread.
        rt.block_on(async { tokio::time::sleep(schedule.interval * 3).await });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(shared.warning(), None);

        drop(handle);
        drop(bus);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(rt);
            let _ = done_tx.send(());
        });
        let shut_down = done_rx.recv_timeout(WAIT);
        release.store(true, Ordering::SeqCst);
        shut_down.expect("runtime shutdown waited on the hung clock probe");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn probing_resumes_once_a_hung_probe_finishes() {
        let bus = MessageBus::new(16).unwrap();
        let (_tx, rx) = bus.create_channel(ComponentId::Tui).await.unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let (probe, calls, release) = hung_probe();
        let _release = ReleaseOnDrop(release.clone());
        let schedule = ClockMonitorSchedule {
            probe_deadline: Duration::from_millis(50),
            ..test_schedule()
        };
        let (wall, _offset) = offset_wall_clock();
        let handle = spawn_clock_skew_monitor(
            bus.clone(),
            ClockSkewShared::new(),
            shutdown.clone(),
            schedule,
            probe,
            wall,
        );

        let d = next_diag(&rx).await;
        assert!(
            d.text
                .starts_with("could not reach pool.ntp.org (timed out"),
            "{}",
            d.text
        );
        release.store(true, Ordering::SeqCst);

        let d = next_diag(&rx).await;
        assert_eq!(d.text, "clock check: offset +0.012 s vs pool.ntp.org — OK");
        assert!(calls.load(Ordering::SeqCst) >= 2);

        shutdown.store(true, Ordering::Release);
        tokio::time::timeout(WAIT, handle).await.unwrap().unwrap();
    }
}
