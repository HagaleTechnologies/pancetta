//! Service-manager notifications are driven only by completed coordinator health passes.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Event {
    Ready,
    Watchdog,
    Stopping,
}
pub(super) type Sender = Arc<dyn Fn(Event) -> std::io::Result<()> + Send + Sync>;

#[derive(Default)]
struct State {
    ready: bool,
    stopping: bool,
    last_warning: Option<Instant>,
}

pub(super) struct ServiceNotify {
    pub startup: AtomicBool,
    pub audio: Arc<AtomicBool>,
    pub dsp: Arc<AtomicBool>,
    pub decoder: Arc<AtomicBool>,
    state: Mutex<State>,
    watchdog: Option<Duration>,
    send: Sender,
}

impl ServiceNotify {
    pub fn new() -> Self {
        #[cfg(target_os = "linux")]
        let watchdog = sd_notify::watchdog_enabled();
        #[cfg(not(target_os = "linux"))]
        let watchdog = None;
        Self::with_sender(watchdog, Arc::new(send))
    }
    pub(super) fn with_sender(watchdog: Option<Duration>, send: Sender) -> Self {
        Self {
            startup: AtomicBool::new(false),
            audio: Arc::new(AtomicBool::new(false)),
            dsp: Arc::new(AtomicBool::new(false)),
            decoder: Arc::new(AtomicBool::new(false)),
            state: Mutex::new(State::default()),
            watchdog,
            send,
        }
    }
    pub fn period(&self) -> Duration {
        self.watchdog
            .map(|d| (d / 2).max(Duration::from_nanos(1)))
            .unwrap_or(Duration::from_secs(5))
            .min(Duration::from_secs(5))
    }
    fn deliver(&self, state: &mut State, event: Event, now: Instant) -> bool {
        match (self.send)(event) {
            Ok(()) => true,
            Err(error) => {
                if state.last_warning.is_none_or(|last| {
                    now.saturating_duration_since(last) >= Duration::from_secs(60)
                }) {
                    tracing::warn!(%error, "Service notification failed");
                    state.last_warning = Some(now);
                }
                false
            }
        }
    }
    pub fn completed_pass(&self, rig_ready: bool, shutdown: &AtomicBool, now: Instant) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if shutdown.load(Ordering::Acquire) {
            self.stop_locked(&mut state, now);
            return;
        }
        if state.stopping {
            return;
        }
        if !state.ready {
            let initialized = self.startup.load(Ordering::Acquire)
                && rig_ready
                && [&self.audio, &self.dsp, &self.decoder]
                    .iter()
                    .all(|s| s.load(Ordering::Acquire));
            if initialized && self.deliver(&mut state, Event::Ready, now) {
                state.ready = true;
            }
        }
        // The health interval already enforces the cadence. A second elapsed-time
        // filter could skip an early-woken timer tick and double the heartbeat gap.
        if state.ready && self.watchdog.is_some() {
            self.deliver(&mut state, Event::Watchdog, now);
        }
    }
    fn stop_locked(&self, state: &mut State, now: Instant) {
        if !state.stopping {
            state.stopping = true;
            if state.ready {
                self.deliver(state, Event::Stopping, now);
            }
        }
    }
    pub fn stopping(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.stop_locked(&mut state, Instant::now());
    }
}

/// The owner keeps this guard alive only while its initialized worker is alive.
pub(super) struct Initialized(Arc<AtomicBool>);
impl Initialized {
    pub fn after_start<E>(
        state: Arc<AtomicBool>,
        start: impl FnOnce() -> Result<(), E>,
    ) -> Result<Self, E> {
        state.store(false, Ordering::Release);
        start()?;
        state.store(true, Ordering::Release);
        Ok(Self(state))
    }
}
impl Drop for Initialized {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn send(event: Event) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use sd_notify::NotifyState;
        sd_notify::notify(&[match event {
            Event::Ready => NotifyState::Ready,
            Event::Watchdog => NotifyState::Watchdog,
            Event::Stopping => NotifyState::Stopping,
        }])
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = event;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(watchdog: Option<Duration>) -> (ServiceNotify, Arc<Mutex<Vec<Event>>>) {
        let events = Arc::new(Mutex::new(vec![]));
        let capture = events.clone();
        (
            ServiceNotify::with_sender(
                watchdog,
                Arc::new(move |event| {
                    capture.lock().unwrap().push(event);
                    Ok(())
                }),
            ),
            events,
        )
    }
    fn ready(notify: &ServiceNotify) {
        notify.startup.store(true, Ordering::Release);
        for flag in [&notify.audio, &notify.dsp, &notify.decoder] {
            flag.store(true, Ordering::Release);
        }
    }
    #[test]
    fn readiness_requires_every_initialized_component() {
        let (notify, events) = fixture(None);
        let shutdown = AtomicBool::new(false);
        let now = Instant::now();
        ready(&notify);
        for flag in [&notify.audio, &notify.dsp, &notify.decoder] {
            flag.store(false, Ordering::Release);
            notify.completed_pass(true, &shutdown, now);
            assert!(events.lock().unwrap().is_empty());
            flag.store(true, Ordering::Release);
        }
        notify.startup.store(false, Ordering::Release);
        notify.completed_pass(true, &shutdown, now);
        assert!(events.lock().unwrap().is_empty());
        notify.startup.store(true, Ordering::Release);
        notify.completed_pass(false, &shutdown, now);
        assert!(events.lock().unwrap().is_empty());
        notify.completed_pass(true, &shutdown, now);
        notify.completed_pass(true, &shutdown, now);
        assert_eq!(*events.lock().unwrap(), vec![Event::Ready]);
    }
    #[test]
    fn watchdog_follows_completed_health_passes() {
        let (notify, events) = fixture(Some(Duration::from_secs(6)));
        ready(&notify);
        let stop = AtomicBool::new(false);
        let now = Instant::now();
        notify.completed_pass(true, &stop, now);
        assert_eq!(notify.period(), Duration::from_secs(3));
        assert_eq!(*events.lock().unwrap(), vec![Event::Ready, Event::Watchdog]);
        notify.completed_pass(true, &stop, now + notify.period());
        assert_eq!(events.lock().unwrap().len(), 3);
    }
    #[test]
    fn shutdown_stops_notifications() {
        let (notify, events) = fixture(Some(Duration::from_secs(60)));
        ready(&notify);
        let stop = AtomicBool::new(false);
        notify.completed_pass(true, &stop, Instant::now());
        stop.store(true, Ordering::Release);
        notify.completed_pass(true, &stop, Instant::now());
        notify.stopping();
        notify.completed_pass(true, &stop, Instant::now());
        assert_eq!(
            *events.lock().unwrap(),
            vec![Event::Ready, Event::Watchdog, Event::Stopping]
        );
        let (pending, events) = fixture(None);
        ready(&pending);
        pending.completed_pass(true, &stop, Instant::now());
        assert!(events.lock().unwrap().is_empty());
    }
    #[test]
    fn failed_ready_is_retried() {
        let tries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let capture = tries.clone();
        let notify = ServiceNotify::with_sender(
            None,
            Arc::new(move |_| {
                if capture.fetch_add(1, Ordering::Relaxed) == 0 {
                    Err(std::io::Error::other("socket unavailable"))
                } else {
                    Ok(())
                }
            }),
        );
        ready(&notify);
        let stop = AtomicBool::new(false);
        notify.completed_pass(true, &stop, Instant::now());
        assert!(!notify.state.lock().unwrap().ready);
        notify.completed_pass(true, &stop, Instant::now());
        assert!(notify.state.lock().unwrap().ready);
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn sd_notify_datagrams_match_protocol() {
        use std::os::unix::net::UnixDatagram;
        use std::process::Command;
        const MODE: &str = "PAN335_NOTIFY_CHILD";
        if let Ok(mode) = std::env::var(MODE) {
            let notify = ServiceNotify::new();
            if mode == "mismatch" {
                assert!(notify.watchdog.is_none());
            }
            if mode == "enabled" {
                assert_eq!(notify.watchdog, Some(Duration::from_secs(60)));
            }
            ready(&notify);
            notify.completed_pass(true, &AtomicBool::new(false), Instant::now());
            notify.stopping();
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("notify.sock");
        let receiver = UnixDatagram::bind(&socket).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        for mode in ["enabled", "mismatch", "absent"] {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .arg("--exact")
                .arg("coordinator::service_notify::tests::sd_notify_datagrams_match_protocol")
                .env(MODE, mode)
                .env("WATCHDOG_USEC", "60000000")
                .env_remove("WATCHDOG_PID");
            if mode == "mismatch" {
                command.env("WATCHDOG_PID", "1");
            }
            if mode == "absent" {
                command.env_remove("NOTIFY_SOCKET");
            } else {
                command.env("NOTIFY_SOCKET", &socket);
            }
            assert!(command.status().unwrap().success());
            if mode != "absent" {
                let expected: &[&str] = if mode == "enabled" {
                    &["READY=1", "WATCHDOG=1", "STOPPING=1"]
                } else {
                    &["READY=1", "STOPPING=1"]
                };
                for expected in expected {
                    let mut bytes = [0; 256];
                    let size = receiver.recv(&mut bytes).unwrap();
                    assert_eq!(
                        std::str::from_utf8(&bytes[..size]).unwrap().trim(),
                        *expected
                    );
                }
            }
        }
    }
    #[test]
    fn no_systemd_is_noop() {
        // The isolated child removes NOTIFY_SOCKET and exercises the production adapter.
        #[cfg(target_os = "linux")]
        {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("coordinator::service_notify::tests::sd_notify_datagrams_match_protocol")
                .env("PAN335_NOTIFY_CHILD", "absent")
                .env_remove("NOTIFY_SOCKET")
                .env_remove("WATCHDOG_USEC")
                .env_remove("WATCHDOG_PID")
                .status()
                .unwrap();
            assert!(status.success());
        }
        #[cfg(not(target_os = "linux"))]
        assert!(send(Event::Ready).is_ok());
    }
}
