//! PAN-381 AC6/AC7/AC8: the listener task over real loopback sockets, and
//! the coordinator's start rules.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pancetta_config::{Config, OperatingMode};
use pancetta_core::DiagnosticLevel;
use tokio::net::UdpSocket;

use super::super::codec::{OutMsg, Writer};
use super::{run_decode_ingest, DropCounters, Dropped, IngestContext, IngestLimits};
use crate::coordinator::pipeline::{DecodeOrigin, RelayedDecode};
use crate::coordinator::ApplicationCoordinator;
use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};

const WSJTX: &str = "WSJT-X";

fn status(id: &str, dial: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(0xadbc_cbda);
    w.u32(2);
    w.u32(1);
    w.utf8(id);
    w.u64(dial);
    w.utf8("FT8");
    w.into_vec()
}

fn decode(id: &str, message: &str) -> Vec<u8> {
    OutMsg::Decode {
        new: true,
        time_ms: 39_435_000,
        snr: -5,
        delta_time: 0.2,
        delta_freq: 1302,
        mode: "~".into(),
        message: message.into(),
        low_confidence: false,
        off_air: false,
    }
    .encode(id)
}

fn request(kind: u32, id: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(0xadbc_cbda);
    w.u32(2);
    w.u32(kind);
    w.utf8(id);
    match kind {
        // HaltTx: AutoTxOnly = false (a full halt).
        8 => w.bool(false),
        // Reply: a full body naming a CQ.
        4 => {
            w.u32(39_435_000);
            w.i32(-5);
            w.u64(0.2f64.to_bits());
            w.u32(1302);
            w.utf8("~");
            w.utf8("CQ W1ABC FN31");
            w.bool(false);
            w.u8(0);
        }
        _ => {}
    }
    w.into_vec()
}

/// Poll a crossbeam receiver from async code until `within` passes.
async fn recv_within<T>(rx: &crossbeam_channel::Receiver<T>, within: Duration) -> Option<T> {
    let deadline = Instant::now() + within;
    loop {
        if let Ok(v) = rx.try_recv() {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn diagnostic_of(msg: ComponentMessage) -> (&'static str, DiagnosticLevel, String) {
    match msg.message_type {
        MessageType::DiagnosticEvent {
            target,
            level,
            text,
            ..
        } => (target, level, text),
        other => panic!("expected a DiagnosticEvent, got {other:?}"),
    }
}

struct TaskHarness {
    addr: SocketAddr,
    tui_rx: crossbeam_channel::Receiver<RelayedDecode>,
    diag_rx: crossbeam_channel::Receiver<ComponentMessage>,
    shutdown: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    wsjtx: UdpSocket,
}

impl TaskHarness {
    async fn start(capacity: usize) -> Self {
        let limits = IngestLimits {
            summary_interval: Duration::from_millis(100),
            ..IngestLimits::default()
        };
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let (tui_tx, tui_rx) = crossbeam_channel::bounded(capacity);
        let bus = MessageBus::new(64).unwrap();
        let (_diag_tx, diag_rx) = bus.create_channel(ComponentId::Tui).await.unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = tokio::spawn(run_decode_ingest(IngestContext {
            socket,
            tui_tx,
            message_bus: bus,
            shutdown: shutdown.clone(),
            own_instance_id: "WSJT-X - pancetta".into(),
            limits,
        }));
        let wsjtx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        Self {
            addr,
            tui_rx,
            diag_rx,
            shutdown,
            handle,
            wsjtx,
        }
    }

    async fn send(&self, bytes: &[u8]) {
        self.wsjtx.send_to(bytes, self.addr).await.unwrap();
    }

    /// Set the shutdown flag; the task must return `Ok(())` within 1 s.
    async fn stop(self) {
        self.shutdown.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(1), self.handle)
            .await
            .expect("listener exits within 1 s of shutdown")
            .unwrap()
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_then_decode_reaches_the_display_channel() {
    let h = TaskHarness::start(16).await;
    h.send(&status(WSJTX, 14_074_000)).await;
    h.send(&decode(WSJTX, "CQ DX JA1ABC PM95")).await;
    let d = recv_within(&h.tui_rx, Duration::from_secs(2))
        .await
        .expect("decode delivered within 2 s");
    assert_eq!(
        d.origin,
        DecodeOrigin::WsjtxUdp {
            mode: OperatingMode::Ft8
        }
    );
    assert_eq!(d.message.text, "CQ DX JA1ABC PM95");
    assert_eq!(d.dial_hz, 14_074_000);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listener_never_sends_a_datagram() {
    let h = TaskHarness::start(16).await;
    h.send(&status(WSJTX, 14_074_000)).await;
    h.send(&decode(WSJTX, "CQ K1ABC FN42")).await;
    for kind in [0u32, 3, 4, 7, 8] {
        h.send(&request(kind, WSJTX)).await;
    }
    let mut buf = [0u8; 2048];
    let got = tokio::time::timeout(Duration::from_millis(500), h.wsjtx.recv_from(&mut buf)).await;
    assert!(got.is_err(), "the listener sent a datagram: {got:?}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_display_channel_drops_without_blocking() {
    let h = TaskHarness::start(1).await;
    h.send(&status(WSJTX, 14_074_000)).await;
    for _ in 0..5 {
        h.send(&decode(WSJTX, "CQ K1ABC FN42")).await;
    }
    // The four that did not fit are summarised as a Warn.
    let (target, level, text) = diagnostic_of(
        recv_within(&h.diag_rx, Duration::from_secs(2))
            .await
            .expect("queue-full summary"),
    );
    assert_eq!(target, "remote.wsjtx");
    assert_eq!(level, DiagnosticLevel::Warn);
    assert_eq!(
        text,
        "WSJT-X decode listener dropped 4 datagrams in the last 100 ms: 4 with the display queue full"
    );
    assert_eq!(h.tui_rx.len(), 1, "only the first decode fit");
    // Still responsive: it exits promptly with the channel still full.
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn summary_diagnostic_text_and_level() {
    // Exact texts with the production limits.
    let limits = IngestLimits::default();
    let mut c = DropCounters::default();
    for _ in 0..4 {
        c.record(Dropped::NoStatus);
    }
    c.record(Dropped::OwnEcho);
    c.record(Dropped::Filtered);
    assert_eq!(
        c.summary(&limits),
        Some((
            DiagnosticLevel::Info,
            "WSJT-X decode listener dropped 4 datagrams in the last 60 s: 4 before WSJT-X sent a Status"
                .to_string()
        ))
    );
    let mut c = DropCounters::default();
    for _ in 0..2 {
        c.record(Dropped::NonLoopback);
    }
    for _ in 0..298 {
        c.record(Dropped::OverBudget);
    }
    assert_eq!(
        c.summary(&limits),
        Some((
            DiagnosticLevel::Warn,
            "WSJT-X decode listener dropped 300 datagrams in the last 60 s: 2 from non-loopback sources, 298 over the 250-per-15 s limit"
                .to_string()
        ))
    );
    // Parts keep their order; any reason besides NoStatus makes it Warn.
    let mut c = DropCounters::default();
    c.record_queue_full();
    c.record(Dropped::NoStatus);
    c.record(Dropped::Malformed);
    let (level, text) = c.summary(&limits).unwrap();
    assert_eq!(level, DiagnosticLevel::Warn);
    assert_eq!(
        text,
        "WSJT-X decode listener dropped 3 datagrams in the last 60 s: 1 malformed, 1 before WSJT-X sent a Status, 1 with the display queue full"
    );
    // Echo and filtered traffic never produce a line.
    let mut c = DropCounters::default();
    c.record(Dropped::OwnEcho);
    c.record(Dropped::Filtered);
    assert_eq!(c.summary(&limits), None);

    // Over the wire, with a 100 ms interval: a decode before any Status is
    // Info; a malformed datagram is Warn; filtered traffic says nothing.
    let h = TaskHarness::start(16).await;
    h.send(&decode(WSJTX, "CQ K1ABC FN42")).await;
    let (target, level, text) = diagnostic_of(
        recv_within(&h.diag_rx, Duration::from_secs(2))
            .await
            .expect("no-status summary"),
    );
    assert_eq!(target, "remote.wsjtx");
    assert_eq!(level, DiagnosticLevel::Info);
    assert_eq!(
        text,
        "WSJT-X decode listener dropped 1 datagram in the last 100 ms: 1 before WSJT-X sent a Status"
    );
    h.send(b"not wsjt-x").await;
    let (_, level, text) = diagnostic_of(
        recv_within(&h.diag_rx, Duration::from_secs(2))
            .await
            .expect("malformed summary"),
    );
    assert_eq!(level, DiagnosticLevel::Warn);
    assert_eq!(
        text,
        "WSJT-X decode listener dropped 1 datagram in the last 100 ms: 1 malformed"
    );
    let replay = OutMsg::Decode {
        new: false,
        time_ms: 0,
        snr: 0,
        delta_time: 0.0,
        delta_freq: 0,
        mode: "~".into(),
        message: "CQ K1ABC FN42".into(),
        low_confidence: false,
        off_air: false,
    }
    .encode(WSJTX);
    h.send(&replay).await;
    h.send(&decode("WSJT-X - pancetta", "CQ K1ABC FN42")).await;
    assert!(
        recv_within(&h.diag_rx, Duration::from_millis(400))
            .await
            .is_none(),
        "filtered and echoed datagrams produce no summary"
    );
    h.stop().await;
}

// ---------------------------------------------------------------------------
// Coordinator-level start rules.
// ---------------------------------------------------------------------------

struct Station {
    coordinator: ApplicationCoordinator,
    _scratch: tempfile::TempDir,
}

async fn station(
    edit: impl FnOnce(&mut Config),
    headless: bool,
    replay: Option<PathBuf>,
) -> Station {
    let scratch = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.station.callsign = "K5ARH".into();
    config.network.wsjtx_udp.accept_decodes = true;
    config.network.wsjtx_udp.decode_listen = "127.0.0.1:0".into();
    edit(&mut config);
    let mut coordinator = ApplicationCoordinator::new(
        config,
        None,
        true,
        headless,
        None,
        replay,
        None,
        1500.0,
        Arc::new(AtomicBool::new(false)),
        Vec::new(),
        scratch.path().join("config.toml"),
        true,
        Some(scratch.path().to_path_buf()),
    )
    .await
    .unwrap();
    coordinator.init_decode_handles();
    Station {
        coordinator,
        _scratch: scratch,
    }
}

async fn stop(mut c: ApplicationCoordinator) {
    c.shutdown_signal.store(true, Ordering::Release);
    for (_, task) in c.named_task_handles.drain(..) {
        task.await.unwrap().unwrap();
    }
}

/// A free loopback port nobody holds right now.
fn free_loopback_addr() -> SocketAddr {
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    probe.local_addr().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_decodes_reach_only_the_display_channel() {
    let Station {
        mut coordinator,
        _scratch,
    } = station(|_| {}, false, None).await;
    let tui_decodes = coordinator.take_tui_decode_receiver_for_test();
    let mut quiet = Vec::new();
    for id in [
        ComponentId::Qso,
        ComponentId::Autonomous,
        ComponentId::PskReporter,
        ComponentId::RemoteGateway,
        ComponentId::WsjtxUdp,
        ComponentId::Ft8Transmitter,
        ComponentId::Hamlib,
    ] {
        let (_tx, rx) = coordinator.message_bus.create_channel(id).await.unwrap();
        quiet.push((id, rx));
    }
    let (_tui_tx, tui_bus) = coordinator
        .message_bus
        .create_channel(ComponentId::Tui)
        .await
        .unwrap();
    let policy_before = coordinator.tx_policy.load(Ordering::SeqCst);

    let addr = coordinator
        .start_wsjtx_decode_ingest()
        .await
        .unwrap()
        .expect("listener bound");
    let wsjtx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for bytes in [
        status(WSJTX, 14_074_000),
        decode(WSJTX, "CQ DX JA1ABC PM95"),
        decode(WSJTX, "K5ARH W1ABC FN31"),
        request(8, WSJTX),
        request(4, WSJTX),
    ] {
        wsjtx.send_to(&bytes, addr).await.unwrap();
    }

    let first = recv_within(&tui_decodes, Duration::from_secs(2))
        .await
        .expect("CQ decode on the display channel");
    let second = recv_within(&tui_decodes, Duration::from_secs(2))
        .await
        .expect("directed decode on the display channel");
    assert_eq!(first.message.text, "CQ DX JA1ABC PM95");
    assert_eq!(second.message.text, "K5ARH W1ABC FN31");
    for d in [&first, &second] {
        assert!(matches!(d.origin, DecodeOrigin::WsjtxUdp { .. }));
    }

    tokio::time::sleep(Duration::from_millis(1500)).await;
    for (id, rx) in &quiet {
        let leaked: Vec<_> = rx.try_iter().map(|m| m.message_type).collect();
        assert!(leaked.is_empty(), "{id:?} received {leaked:?}");
    }
    let mut tui_messages = 0;
    for msg in tui_bus.try_iter() {
        let (target, _, _) = diagnostic_of(msg);
        assert_eq!(target, "remote.wsjtx");
        tui_messages += 1;
    }
    assert!(tui_messages >= 1, "the bind line reaches Diagnostics");
    assert!(!coordinator.abort_current_tx.load(Ordering::SeqCst));
    assert_eq!(coordinator.tx_policy.load(Ordering::SeqCst), policy_before);
    stop(coordinator).await;
}

#[test]
fn ingest_source_names_no_bus_or_tx_path() {
    let source = include_str!("../../ingest.rs");
    let forbidden = [
        "MessageType",
        "send_message",
        "TransmitRequest",
        "StartQso",
        "QsoMessage",
        "abort_current_tx",
        "tx_policy",
        "send_to(",
    ];
    for (n, line) in source.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for token in forbidden {
            assert!(
                !line.contains(token),
                "ingest.rs:{} names `{token}`: {line}",
                n + 1
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingest_off_by_default_binds_nothing() {
    assert!(!Config::default().network.wsjtx_udp.accept_decodes);
    let free = free_loopback_addr();
    let Station {
        mut coordinator,
        _scratch,
    } = station(
        |c| {
            c.network.wsjtx_udp.accept_decodes = false;
            c.network.wsjtx_udp.decode_listen = free.to_string();
        },
        false,
        None,
    )
    .await;
    let tasks_before = coordinator.named_task_handles.len();
    assert_eq!(coordinator.start_wsjtx_decode_ingest().await.unwrap(), None);
    assert_eq!(coordinator.named_task_handles.len(), tasks_before);
    // The configured port is still free.
    let _held = UdpSocket::bind(free).await.expect("port left unbound");
    stop(coordinator).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_and_headless_skip_the_listener() {
    for (headless, replay) in [
        (false, Some(PathBuf::from("/some/capture/dir"))),
        (true, None),
    ] {
        let free = free_loopback_addr();
        let Station {
            mut coordinator,
            _scratch,
        } = station(
            |c| c.network.wsjtx_udp.decode_listen = free.to_string(),
            headless,
            replay,
        )
        .await;
        let tasks_before = coordinator.named_task_handles.len();
        assert_eq!(coordinator.start_wsjtx_decode_ingest().await.unwrap(), None);
        assert_eq!(coordinator.named_task_handles.len(), tasks_before);
        let _held = UdpSocket::bind(free).await.expect("port left unbound");
        stop(coordinator).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_loopback_listen_is_refused_even_unvalidated() {
    for listen in ["0.0.0.0:0", "192.0.2.1:2238", "localhost:2238", "garbage"] {
        let Station {
            mut coordinator,
            _scratch,
        } = station(
            |c| c.network.wsjtx_udp.decode_listen = listen.to_string(),
            false,
            None,
        )
        .await;
        let (_tx, tui_bus) = coordinator
            .message_bus
            .create_channel(ComponentId::Tui)
            .await
            .unwrap();
        let tasks_before = coordinator.named_task_handles.len();
        assert_eq!(
            coordinator.start_wsjtx_decode_ingest().await.unwrap(),
            None,
            "{listen}"
        );
        assert_eq!(coordinator.named_task_handles.len(), tasks_before);
        let (_, level, text) = diagnostic_of(tui_bus.try_recv().expect("refusal diagnostic"));
        assert_eq!(level, DiagnosticLevel::Warn);
        assert!(
            text.starts_with(&format!("WSJT-X decode listener could not bind {listen} (")),
            "{text}"
        );
        stop(coordinator).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_port_warns_and_continues() {
    let holder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let held = holder.local_addr().unwrap();
    let Station {
        mut coordinator,
        _scratch,
    } = station(
        |c| c.network.wsjtx_udp.decode_listen = held.to_string(),
        false,
        None,
    )
    .await;
    let (_tx, tui_bus) = coordinator
        .message_bus
        .create_channel(ComponentId::Tui)
        .await
        .unwrap();
    let tasks_before = coordinator.named_task_handles.len();
    assert_eq!(coordinator.start_wsjtx_decode_ingest().await.unwrap(), None);
    let (target, level, text) = diagnostic_of(tui_bus.try_recv().expect("bind-failure line"));
    assert_eq!(target, "remote.wsjtx");
    assert_eq!(level, DiagnosticLevel::Warn);
    assert!(
        text.starts_with(&format!("WSJT-X decode listener could not bind {held} (")),
        "{text}"
    );
    assert!(
        text.ends_with("); WSJT-X decodes are off for this run"),
        "{text}"
    );
    assert_eq!(coordinator.named_task_handles.len(), tasks_before);
    drop(holder);
    stop(coordinator).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listener_runs_without_companion_broadcast() {
    let Station {
        mut coordinator,
        _scratch,
    } = station(|c| c.network.wsjtx_udp.enabled = false, false, None).await;
    let (_tx, tui_bus) = coordinator
        .message_bus
        .create_channel(ComponentId::Tui)
        .await
        .unwrap();
    let tui_decodes = coordinator.take_tui_decode_receiver_for_test();
    let addr = coordinator
        .start_wsjtx_decode_ingest()
        .await
        .unwrap()
        .expect("bound with enabled = false");
    assert!(addr.ip().is_loopback());
    assert_ne!(addr.port(), 0);
    let (_, level, text) = diagnostic_of(tui_bus.try_recv().expect("bind line"));
    assert_eq!(level, DiagnosticLevel::Info);
    assert_eq!(
        text,
        format!(
            "WSJT-X decode listener on {addr}: set WSJT-X's UDP Server to this address; its decodes are shown and scored, never transmitted on"
        )
    );
    let wsjtx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    wsjtx
        .send_to(&status(WSJTX, 7_074_000), addr)
        .await
        .unwrap();
    wsjtx
        .send_to(&decode(WSJTX, "CQ K1ABC FN42"), addr)
        .await
        .unwrap();
    let d = recv_within(&tui_decodes, Duration::from_secs(2))
        .await
        .expect("decode delivered");
    assert_eq!(d.dial_hz, 7_074_000);
    stop(coordinator).await;
}

// ---------------------------------------------------------------------------
// PAN-434: the listener is its own supervised component.
// ---------------------------------------------------------------------------

const LISTENER_DEGRADED: &str = "WSJT-X decode listener stopped -- no WSJT-X decodes until restart";

/// Abort every supervised task, wait until each has finished, then run one
/// supervisor pass.
async fn fail_every_task_and_supervise(coordinator: &mut ApplicationCoordinator) {
    for (_, task) in &coordinator.named_task_handles {
        task.abort();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !coordinator
        .named_task_handles
        .iter()
        .all(|(_, task)| task.is_finished())
    {
        assert!(Instant::now() < deadline, "aborted tasks did not finish");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    coordinator.check_task_handles().await;
}

/// What one supervisor pass sent to the TUI.
struct SupervisorReports {
    /// `supervisor` Diagnostics rows.
    diagnostics: Vec<(DiagnosticLevel, String)>,
    /// Status-line texts.
    status_lines: Vec<String>,
    /// Every message, as debug text.
    every_message: Vec<String>,
}

fn supervisor_reports(
    tui_bus: &crossbeam_channel::Receiver<ComponentMessage>,
) -> SupervisorReports {
    let mut diagnostics = Vec::new();
    let mut status_lines = Vec::new();
    let mut every_message = Vec::new();
    for msg in tui_bus.try_iter() {
        every_message.push(format!("{msg:?}"));
        match msg.message_type {
            MessageType::DiagnosticEvent {
                target: "supervisor",
                level,
                text,
                ..
            } => diagnostics.push((level, text)),
            MessageType::StatusUpdate { text, .. } => status_lines.push(text),
            _ => {}
        }
    }
    SupervisorReports {
        diagnostics,
        status_lines,
        every_message,
    }
}

/// PAN-434 ticket scenario 3: with `accept_decodes = true`, a failed decode
/// listener is reported as the decode listener, not the companion broadcast.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listener_failure_names_the_decode_listener() {
    let Station {
        mut coordinator,
        _scratch,
    } = station(|_| {}, false, None).await;
    let (_tx, tui_bus) = coordinator
        .message_bus
        .create_channel(ComponentId::Tui)
        .await
        .unwrap();
    coordinator
        .start_wsjtx_decode_ingest()
        .await
        .unwrap()
        .expect("listener bound");
    assert_eq!(
        coordinator.named_task_handles.last().map(|(id, _)| *id),
        Some(ComponentId::WsjtxDecodeListener)
    );
    // The bind line is a `remote.wsjtx` Info event, not a supervisor report.
    let _ = tui_bus.try_iter().count();

    fail_every_task_and_supervise(&mut coordinator).await;
    let SupervisorReports {
        diagnostics,
        status_lines,
        every_message,
    } = supervisor_reports(&tui_bus);
    assert_eq!(
        diagnostics,
        vec![(
            DiagnosticLevel::Error,
            format!("WsjtxDecodeListener {LISTENER_DEGRADED}")
        )]
    );
    assert_eq!(
        status_lines,
        vec![format!("WsjtxDecodeListener: {LISTENER_DEGRADED}")]
    );
    assert!(!every_message.is_empty());
    for message in &every_message {
        assert!(!message.contains("WsjtxUdp"), "{message}");
    }
    let map = coordinator.component_status.read().await;
    assert_eq!(
        crate::coordinator::health::degraded_summary(&map),
        Some(format!("WsjtxDecodeListener: {LISTENER_DEGRADED}"))
    );
}

/// PAN-434: a companion failure and a listener failure in the same
/// supervisor pass are reported apart. Under one shared id the second was
/// never reported, because the supervisor keys its status on the id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn companion_and_listener_failures_are_reported_apart() {
    let Station {
        mut coordinator,
        _scratch,
    } = station(|_| {}, false, None).await;
    let (_tx, tui_bus) = coordinator
        .message_bus
        .create_channel(ComponentId::Tui)
        .await
        .unwrap();
    // A stand-in for the companion socket's task, registered first as the
    // coordinator does. Aborted rather than panicked, so the log stays clean.
    coordinator.named_task_handles.push((
        ComponentId::WsjtxUdp,
        tokio::spawn(std::future::pending::<anyhow::Result<()>>()),
    ));
    coordinator
        .start_wsjtx_decode_ingest()
        .await
        .unwrap()
        .expect("listener bound");
    let _ = tui_bus.try_iter().count();

    fail_every_task_and_supervise(&mut coordinator).await;
    let mut texts: Vec<String> = supervisor_reports(&tui_bus)
        .diagnostics
        .into_iter()
        .map(|(_, text)| text)
        .collect();
    texts.sort();
    assert_eq!(
        texts,
        vec![
            format!("WsjtxDecodeListener {LISTENER_DEGRADED}"),
            "WsjtxUdp Component failed".to_owned(),
        ]
    );
    let map = coordinator.component_status.read().await;
    for (id, error) in [
        (ComponentId::WsjtxDecodeListener, LISTENER_DEGRADED),
        (ComponentId::WsjtxUdp, "Component failed"),
    ] {
        let status = map.get(&id).expect("status entry");
        assert_eq!(
            status.state,
            crate::coordinator::ComponentState::Failed(error.to_owned()),
            "{id}"
        );
        assert_eq!(status.error_count, 1, "{id}");
    }
}
