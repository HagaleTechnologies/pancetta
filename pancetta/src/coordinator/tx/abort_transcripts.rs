//! Abort-transcript golden (PAN-322): drives the real TX worker through every
//! abort reason (shutdown, operator abort, remote disarm, supersede) at every
//! phase it can land in (before PTT, inside the PTT lead, during playback),
//! for a single frame and for a two-item bundle, and compares what the worker
//! sent on each channel with `tests/fixtures/tx_abort_transcripts.golden`.
//!
//! The golden was blessed from the code before the PAN-322 refactor, so any
//! change to abort behaviour fails this test until it is re-blessed on
//! purpose with
//! `PANCETTA_BLESS_TX_ABORT=1 cargo test -p pancetta --lib coordinator::tx::abort_transcripts`.
//!
//! This module uses only items that exist before PAN-322, so it can be
//! dropped onto the base commit to re-check the golden there.

use super::pan115_worker_gate_tests::{worker_harness_with_clock, Harness};
use crate::coordinator::active_tx_qso_key;
use crate::message_bus::{
    ComponentId, ComponentMessage, ControlMessage, MessageType, QsoMessage, RigControlMessage,
    TransmitRequestItem, TxOrigin,
};
use crossbeam_channel::Receiver;
use pancetta_agent::arm::{ArmTime, ConsentChannel, VerifiedArmGrant};
use pancetta_config::Config;
use pancetta_core::slot::SlotParity;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/tx_abort_transcripts.golden"
);
const BLESS_VAR: &str = "PANCETTA_BLESS_TX_ABORT";

const CHANNELS: [&str; 5] = ["hamlib", "autonomous", "qso", "tui", "audio"];
const HAMLIB: usize = 0;
const AUTONOMOUS: usize = 1;
const QSO: usize = 2;
const TUI: usize = 3;
const AUDIO: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shape {
    Single,
    Bundle,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    PrePtt,
    PreSlot,
    Playback,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reason {
    Shutdown,
    Operator,
    Disarm,
    Supersede,
}

#[derive(Clone, Copy, Debug)]
struct Cell {
    shape: Shape,
    phase: Phase,
    reason: Reason,
}

impl Cell {
    fn label(&self) -> String {
        format!("{:?}/{:?}/{:?}", self.shape, self.phase, self.reason)
    }

    fn frame_count(&self) -> usize {
        match self.shape {
            Shape::Single => 1,
            Shape::Bundle => 2,
        }
    }
}

/// The 20 cells, in golden order. Before PTT only shutdown and operator
/// abort apply: a disarm or supersede there is a different code path (the
/// pre-PTT gate and the queue), not an abort of a scheduled frame.
fn cells() -> Vec<Cell> {
    let mut out = Vec::new();
    for shape in [Shape::Single, Shape::Bundle] {
        for phase in [Phase::PrePtt, Phase::PreSlot, Phase::Playback] {
            let reasons: &[Reason] = match phase {
                Phase::PrePtt => &[Reason::Shutdown, Reason::Operator],
                _ => &[
                    Reason::Shutdown,
                    Reason::Operator,
                    Reason::Disarm,
                    Reason::Supersede,
                ],
            };
            for &reason in reasons {
                out.push(Cell {
                    shape,
                    phase,
                    reason,
                });
            }
        }
    }
    out
}

/// One message the worker sent: its bus id, channel index and rendering.
#[derive(Clone, Debug)]
struct Event {
    id: u64,
    channel: usize,
    line: String,
}

struct Recorded {
    cell: Cell,
    marker: u64,
    events: Vec<Event>,
}

fn describe(m: &mut MessageType) -> String {
    match m {
        MessageType::RigControl(RigControlMessage::SetPtt { state }) => format!("SetPtt({state})"),
        MessageType::TransmitComplete {
            success,
            qso_id,
            attempt_id,
            message_text,
            ..
        } => format!(
            "TransmitComplete(success={success}, {qso_id:?}, {attempt_id:?}, '{message_text}')"
        ),
        MessageType::QsoMessage(QsoMessage::TxFrameScheduled {
            qso_id, attempt_id, ..
        }) => format!("TxFrameScheduled({qso_id}, {attempt_id:?})"),
        MessageType::QsoMessage(QsoMessage::TxFrameNotSent {
            qso_id,
            message_text,
            attempt_id,
            replaced,
        }) => format!(
            "TxFrameNotSent({qso_id}, '{message_text}', {attempt_id:?}, replaced={})",
            replaced.is_some()
        ),
        MessageType::QsoMessage(QsoMessage::TxFrameSent {
            qso_id,
            message_text,
            ..
        }) => format!("TxFrameSent({qso_id}, '{message_text}')"),
        MessageType::TxQueueStatus { sending, queued } => format!(
            "TxQueueStatus(sending={:?}, queued={:?})",
            sending
                .as_ref()
                .map(|i| (i.text.clone(), i.freq_hz, i.deferred)),
            queued
                .iter()
                .map(|i| (i.text.clone(), i.freq_hz, i.deferred))
                .collect::<Vec<_>>()
        ),
        MessageType::DiagnosticEvent { text, level, .. } => {
            format!("Diagnostic({level:?}, '{text}')")
        }
        MessageType::AudioOutput {
            samples,
            flush_first,
            ..
        } => format!(
            "AudioOutput(flush_first={flush_first}, nonempty={})",
            !samples.is_empty()
        ),
        MessageType::AudioFlush { ack, .. } => {
            // As a live audio thread would.
            ack.complete(true);
            "AudioFlush".to_string()
        }
        other => {
            let d = format!("{other:?}");
            d.chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect()
        }
    }
}

/// The marker that splits "pre" from "post": a fresh id from the bus's
/// global counter. Every message created before it has a lower id.
fn marker_id() -> u64 {
    ComponentMessage::new(
        ComponentId::Qso,
        ComponentId::Qso,
        MessageType::Control(ControlMessage::Shutdown),
        Instant::now(),
    )
    .id
}

/// Drains the five receivers into one arrival-ordered event list.
struct Recorder {
    receivers: [Receiver<ComponentMessage>; 5],
    events: Vec<Event>,
}

impl Recorder {
    fn new(h: &Harness, audio_rx: Receiver<ComponentMessage>) -> Self {
        Self {
            receivers: [
                h.hamlib_rx.clone(),
                h.autonomous_rx.clone(),
                h.qso_rx.clone(),
                h.tui_rx.clone(),
                audio_rx,
            ],
            events: Vec::new(),
        }
    }

    fn drain(&mut self) {
        for (channel, rx) in self.receivers.iter().enumerate() {
            for mut message in rx.try_iter() {
                let line = describe(&mut message.message_type);
                self.events.push(Event {
                    id: message.id,
                    channel,
                    line,
                });
            }
        }
    }

    fn count(&self, channel: usize, prefix: &str, after: Option<u64>) -> usize {
        self.events
            .iter()
            .filter(|e| e.channel == channel && e.line.starts_with(prefix))
            .filter(|e| match after {
                Some(m) => e.id > m,
                None => true,
            })
            .count()
    }

    /// Drains every 5 ms until `done` holds or `limit` elapses; returns
    /// whether `done` held.
    async fn drain_until(&mut self, limit: Duration, done: impl Fn(&Self) -> bool) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            self.drain();
            if done(self) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

const SLOT_MS: i64 = 15_000;

fn ms_into_slot() -> i64 {
    chrono::Utc::now().timestamp_millis().rem_euclid(SLOT_MS)
}

/// Waits until the wall clock is 4 to 10 s into a 15 s slot, so a frame sent
/// now targets the next slot and waits out the full PTT lead.
async fn wait_for_send_window() {
    loop {
        let pos = ms_into_slot();
        if (4_000..10_000).contains(&pos) {
            return;
        }
        let wait = (4_050 - pos).rem_euclid(SLOT_MS).max(5);
        tokio::time::sleep(Duration::from_millis(wait as u64)).await;
    }
}

fn origin_for(cell: Cell) -> (ComponentId, TxOrigin, Option<String>) {
    if cell.reason == Reason::Disarm {
        (
            ComponentId::StationAgent,
            TxOrigin::Remote,
            Some("client-a".to_string()),
        )
    } else {
        (ComponentId::Qso, TxOrigin::Local, None)
    }
}

fn frame_for(cell: Cell) -> MessageType {
    let (_, origin, identity) = origin_for(cell);
    match cell.shape {
        Shape::Single => MessageType::TransmitRequest {
            attempt_id: Some(pancetta_qso::TxAttemptId(7)),
            message_text: "K1ABC W9XYZ FN20".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-a".to_string()),
            tx_parity: None,
            origin,
            remote_client_key_id: identity,
        },
        Shape::Bundle => MessageType::MultiTransmitRequest {
            items: [
                ("K1ABC W9XYZ FN20", 1000.0, "qso-a", 11),
                ("K2DEF W9XYZ FN20", 1800.0, "qso-b", 12),
            ]
            .into_iter()
            .map(|(text, freq, qso, attempt)| TransmitRequestItem {
                attempt_id: Some(pancetta_qso::TxAttemptId(attempt)),
                message_text: text.to_string(),
                frequency_offset: freq,
                qso_id: Some(qso.to_string()),
                origin,
                remote_client_key_id: identity.clone(),
            })
            .collect(),
            tx_parity: None,
            origin,
            remote_client_key_id: identity,
        },
    }
}

async fn run_cell(cell: Cell) -> Recorded {
    let base = ArmTime::now();
    let elapsed = Arc::new(AtomicI64::new(0));
    let clock_elapsed = elapsed.clone();
    let clock = Arc::new(move || {
        let ms = clock_elapsed.load(Ordering::SeqCst);
        ArmTime {
            unix_ms: base.unix_ms + ms,
            monotonic: base.monotonic + Duration::from_millis(ms as u64),
            suspend_aware_ms: base.suspend_aware_ms + ms,
        }
    });
    let mut cfg = Config::default();
    // The default 80 ms lead would make a pre-slot abort an 80 ms race.
    cfg.station.ptt_lead_ms = 2000;
    if cell.shape == Shape::Bundle {
        // Under the default 1 a two-item bundle takes the over-cap path.
        cfg.autonomous.max_concurrent_qsos = 2;
    }
    let h = worker_harness_with_clock(cfg, |_| {}, clock).await;
    let (_, audio_rx) = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Audio)
        .await
        .unwrap();
    {
        let mut arm = h.coordinator.remote_tx_arm.lock().unwrap();
        arm.set_channel_consent(ConsentChannel::StationAgent, true, base.unix_ms);
        arm.arm(
            VerifiedArmGrant {
                operator_callsign: "N0CALL".into(),
                ttl_ms: 900_000,
                armed_until_ms: base.unix_ms + 900_000,
                capability_exp_ms: base.unix_ms + 600_000,
                tx_enabled_until_ms: base.unix_ms + 600_000,
                effective_ttl_ms: 600_000,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: "pan322-abort".into(),
                client_key_id: "client-a".into(),
            },
            base,
        );
    }
    {
        let mut active = h.coordinator.active_tx_qsos.write().unwrap();
        active.insert(active_tx_qso_key("qso-a"));
        active.insert(active_tx_qso_key("qso-b"));
    }
    let mut rec = Recorder::new(&h, audio_rx);
    let frames = cell.frame_count();

    wait_for_send_window().await;
    let (source, _, _) = origin_for(cell);
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            source,
            ComponentId::Ft8Transmitter,
            frame_for(cell),
            Instant::now(),
        ))
        .await
        .unwrap();

    let label = cell.label();
    let reached = match cell.phase {
        Phase::PrePtt => {
            rec.drain_until(Duration::from_secs(10), |r| {
                r.count(QSO, "TxFrameScheduled", None) >= frames
            })
            .await
        }
        Phase::PreSlot => {
            rec.drain_until(Duration::from_secs(30), |r| {
                r.count(HAMLIB, "SetPtt(true)", None) >= 1
            })
            .await
        }
        Phase::Playback => {
            rec.drain_until(Duration::from_secs(35), |r| {
                r.count(TUI, "TxFrameLogged", None) >= frames
            })
            .await
        }
    };
    assert!(reached, "{label}: the action point was never reached");
    if cell.phase == Phase::PrePtt {
        assert_eq!(
            rec.count(HAMLIB, "SetPtt", None),
            0,
            "{label}: the pre-PTT action point must come before PTT"
        );
    }

    let marker = marker_id();
    match cell.reason {
        Reason::Shutdown => h.coordinator.shutdown_signal.store(true, Ordering::Release),
        Reason::Operator => h
            .coordinator
            .abort_current_tx
            .store(true, Ordering::Release),
        Reason::Disarm => elapsed.store(600_000, Ordering::SeqCst),
        Reason::Supersede => {
            let parity = SlotParity::of(chrono::Utc::now());
            h.coordinator
                .message_bus
                .send_message(ComponentMessage::new(
                    ComponentId::Qso,
                    ComponentId::Ft8Transmitter,
                    MessageType::TransmitRequest {
                        attempt_id: Some(pancetta_qso::TxAttemptId(8)),
                        message_text: "K1ABC W9XYZ -10".to_string(),
                        frequency_offset: 1600.0,
                        qso_id: Some("qso-a".to_string()),
                        tx_parity: Some(parity),
                        origin: TxOrigin::Local,
                        remote_client_key_id: None,
                    },
                    Instant::now(),
                ))
                .await
                .unwrap();
        }
    }

    if cell.reason == Reason::Supersede {
        // A playback supersede of a bundle re-airs the carried item and the
        // superseder together, so wait for both of their `TxFrameSent`s.
        let expected_sent = match (cell.shape, cell.phase) {
            (Shape::Bundle, Phase::Playback) => 2,
            _ => 1,
        };
        let sent = rec
            .drain_until(Duration::from_secs(50), |r| {
                r.count(QSO, "TxFrameSent", Some(marker)) >= expected_sent
            })
            .await;
        assert!(sent, "{label}: the superseder never aired");
    } else {
        rec.drain_until(Duration::from_secs(4), |_| false).await;
    }
    rec.drain_until(Duration::from_millis(300), |_| false).await;
    rec.drain();
    drop(h);
    Recorded {
        cell,
        marker,
        events: rec.events,
    }
}

fn render(recorded: &[Recorded]) -> String {
    let mut out = String::new();
    out.push_str(
        "# TX-worker abort transcripts (PAN-322). One block per cell:\n\
         # [Shape/Phase/Reason], then `pre|post <channel> <event>` for every\n\
         # message the worker sent, relative to the abort action. Channels go\n\
         # in the order hamlib, autonomous, qso, tui, audio; each keeps its\n\
         # arrival order. Re-bless with:\n\
         #   PANCETTA_BLESS_TX_ABORT=1 cargo test -p pancetta --lib coordinator::tx::abort_transcripts\n",
    );
    for r in recorded {
        out.push('\n');
        out.push_str(&format!("[{}]\n", r.cell.label()));
        for (channel, name) in CHANNELS.iter().enumerate() {
            for e in r.events.iter().filter(|e| e.channel == channel) {
                let when = if e.id > r.marker { "post" } else { "pre" };
                out.push_str(&format!("{when} {name} {}\n", e.line));
            }
        }
    }
    out
}

fn assert_orders(recorded: &[Recorded]) {
    for r in recorded {
        let label = r.cell.label();
        let post: Vec<&Event> = r.events.iter().filter(|e| e.id > r.marker).collect();
        let first_ptt_off = post
            .iter()
            .filter(|e| e.channel == HAMLIB && e.line == "SetPtt(false)")
            .map(|e| e.id)
            .min();
        match r.cell.phase {
            Phase::PrePtt => assert!(
                !r.events
                    .iter()
                    .any(|e| e.channel == HAMLIB && e.line.starts_with("SetPtt")),
                "{label}: a pre-PTT abort must never key"
            ),
            Phase::PreSlot | Phase::Playback => assert!(
                first_ptt_off.is_some(),
                "{label}: an abort after key-down must unkey"
            ),
        }
        if matches!(r.cell.reason, Reason::Disarm | Reason::Supersede) {
            let off = first_ptt_off.expect("keyed cell has a PTT-off");
            for e in post.iter().filter(|e| {
                e.channel == AUTONOMOUS
                    || e.channel == QSO
                    || (e.channel == TUI && e.line.starts_with("Diagnostic"))
            }) {
                assert!(
                    off < e.id,
                    "{label}: PTT-off (id {off}) must precede {} {} (id {})",
                    CHANNELS[e.channel],
                    e.line,
                    e.id
                );
            }
        }
        if r.cell.phase == Phase::Playback && r.cell.reason == Reason::Disarm {
            let off = first_ptt_off.expect("keyed cell has a PTT-off");
            for e in post
                .iter()
                .filter(|e| e.channel == AUDIO && e.line == "AudioFlush")
            {
                assert!(
                    off < e.id,
                    "{label}: PTT-off (id {off}) must precede AudioFlush (id {})",
                    e.id
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abort_transcripts_match_golden() {
    let recorded = futures_util::future::join_all(cells().into_iter().map(run_cell)).await;
    assert_orders(&recorded);
    let rendered = render(&recorded);
    if std::env::var(BLESS_VAR).is_ok() {
        std::fs::write(GOLDEN, &rendered).expect("write tx_abort_transcripts.golden");
        return;
    }
    let expected = std::fs::read_to_string(GOLDEN).unwrap_or_default();
    if expected == rendered {
        return;
    }
    let want: Vec<&str> = expected.lines().collect();
    let got: Vec<&str> = rendered.lines().collect();
    let first = want
        .iter()
        .zip(got.iter())
        .position(|(w, g)| w != g)
        .unwrap_or(want.len().min(got.len()));
    let window = |v: &[&str]| {
        let lo = first.saturating_sub(3);
        let hi = (first + 4).min(v.len());
        v[lo.min(hi)..hi]
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{:>6}: {l}", lo + i + 1))
            .collect::<Vec<_>>()
            .join("\n")
    };
    panic!(
        "TX abort behaviour differs from tests/fixtures/tx_abort_transcripts.golden at line {}.\n\
         golden:\n{}\n\
         now:\n{}\n\
         If the change is intended, re-bless with:\n  \
         {BLESS_VAR}=1 cargo test -p pancetta --lib coordinator::tx::abort_transcripts",
        first + 1,
        window(&want),
        window(&got)
    );
}
