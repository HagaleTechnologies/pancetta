//! PAN-381: the WSJT-X decode listener.
//!
//! With `[network.wsjtx_udp] accept_decodes = true`, pancetta binds a
//! receive-only loopback socket on `decode_listen` (default
//! `127.0.0.1:2238`) that a same-host WSJT-X's "UDP Server" setting points
//! at. It learns each WSJT-X instance's dial from its Status(1) datagrams
//! and turns every live Decode(2) into a [`RelayedDecode`] tagged
//! [`DecodeOrigin::WsjtxUdp`], which goes ONLY to the TUI relay channel
//! (`ft8_to_tui_tx`). There it is scored by the same `PriorityScorer` as a
//! native decode and shown in Band Activity and DX Hunter marked `EXT`.
//!
//! Display only, by construction: this module never puts a decode on the
//! message bus and never sends a datagram, so the QSO engine, the
//! autonomous operator, alerts, PSKReporter, the remote gateway and the
//! WSJT-X re-emit never see an external decode. The only bus traffic it
//! makes is the `remote.wsjtx` Diagnostics line through
//! `super::emit_diagnostic`.
//!
//! Clean room: field layouts follow
//! `docs/superpowers/specs/2026-07-13-wsjtx-udp-protocol-notes.md` only.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::TrySendError;
use pancetta_config::{OperatingMode, WsjtxUdpConfig};
use pancetta_core::slot::SlotParity;
use pancetta_core::DiagnosticLevel;
use tracing::{debug, info, warn};

use super::codec::InMsg;
use crate::coordinator::ft8::{sanitize_decoded_field, sanitize_decoded_message};
use crate::coordinator::pipeline::{DecodeOrigin, RelayedDecode};
use crate::message_bus::{ComponentId, MessageBus};

/// Idle receive timeout, so the shutdown flag and the summary timer are
/// checked even when WSJT-X is silent.
const RECV_POLL: Duration = Duration::from_millis(200);

/// Milliseconds in a day: a Decode's `Time` is a `QTime`, so anything at or
/// past this is not a time of day.
const MS_PER_DAY: u32 = 86_400_000;

/// FT8 and FT4 slot lengths, in milliseconds.
const FT8_SLOT_MS: u32 = 15_000;
const FT4_SLOT_MS: u32 = 7_500;

/// Range checks on a Decode's numeric fields (D5).
const SNR_RANGE: std::ops::RangeInclusive<i32> = -60..=60;
const MAX_ABS_DELTA_TIME_S: f64 = 30.0;
const MAX_DELTA_FREQ_HZ: u32 = 6_000;

/// The listener's flood and memory bounds (D7). Tests shrink them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IngestLimits {
    /// Forwarded decodes allowed per `window`.
    pub(crate) budget: u32,
    /// Fixed intake window the budget applies to.
    pub(crate) window: Duration,
    /// At most one drop-summary diagnostic per this interval.
    pub(crate) summary_interval: Duration,
    /// Sender Ids whose Status dial is remembered.
    pub(crate) max_senders: usize,
}

impl Default for IngestLimits {
    fn default() -> Self {
        Self {
            budget: 250,
            window: Duration::from_secs(15),
            summary_interval: Duration::from_secs(60),
            max_senders: 8,
        }
    }
}

/// Why a datagram produced no decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dropped {
    /// The source address is not loopback.
    NonLoopback,
    /// Not a WSJT-X datagram, or a Decode whose fields are out of range or
    /// whose message is empty.
    Malformed,
    /// Pancetta's own `instance_id` (an echo of its own broadcast).
    OwnEcho,
    /// A replay, an off-air (`.wav`) or low-confidence decode, or a mode
    /// other than FT8/FT4. Normal traffic; never summarised.
    Filtered,
    /// That WSJT-X has not sent a Status with a dial yet.
    NoStatus,
    /// Over the intake budget for the current window.
    OverBudget,
}

/// What one datagram did.
// rationale: the decode is moved straight into the display channel; boxing
// it would add an allocation per decode to shrink a short-lived local.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub(crate) enum Ingested {
    /// A live decode for the display channel.
    Decode(RelayedDecode),
    /// A Status or Close updated the sender table.
    SenderUpdated,
    /// A message type the listener takes no action on.
    Ignored,
    /// Dropped, with the reason.
    Dropped(Dropped),
}

/// The pure half of the listener: source, codec, sender table, mapping and
/// budget. [`run_decode_ingest`] owns the socket and the channel.
#[derive(Debug)]
pub(crate) struct DecodeIngest {
    own_id: String,
    limits: IngestLimits,
    /// Sender Id → (last non-zero dial in Hz, when that Status arrived).
    senders: HashMap<String, (u64, Instant)>,
    window_start: Option<Instant>,
    window_count: u32,
}

impl DecodeIngest {
    pub(crate) fn new(own_id: impl Into<String>, limits: IngestLimits) -> Self {
        Self {
            own_id: own_id.into(),
            limits,
            senders: HashMap::new(),
            window_start: None,
            window_count: 0,
        }
    }

    /// Classify one datagram from `from`, received at `now`.
    pub(crate) fn accept(&mut self, bytes: &[u8], from: SocketAddr, now: Instant) -> Ingested {
        if !from.ip().to_canonical().is_loopback() {
            return Ingested::Dropped(Dropped::NonLoopback);
        }
        let Some((id, msg)) = InMsg::decode(bytes) else {
            return Ingested::Dropped(Dropped::Malformed);
        };
        if id == self.own_id {
            return Ingested::Dropped(Dropped::OwnEcho);
        }
        match msg {
            InMsg::Status { dial_frequency, .. } => {
                if dial_frequency == 0 {
                    self.senders.remove(&id);
                } else {
                    self.senders.insert(id, (dial_frequency, now));
                    while self.senders.len() > self.limits.max_senders {
                        let oldest = self
                            .senders
                            .iter()
                            .min_by_key(|(_, (_, at))| *at)
                            .map(|(k, _)| k.clone());
                        match oldest {
                            Some(k) => {
                                self.senders.remove(&k);
                            }
                            None => break,
                        }
                    }
                }
                Ingested::SenderUpdated
            }
            InMsg::Close => {
                self.senders.remove(&id);
                Ingested::SenderUpdated
            }
            InMsg::Decode {
                new,
                time_ms,
                snr,
                delta_time,
                delta_freq,
                mode,
                message,
                low_confidence,
                off_air,
            } => {
                if !new || off_air || low_confidence {
                    return Ingested::Dropped(Dropped::Filtered);
                }
                let (op_mode, slot_ms) = match mode.as_str() {
                    "~" => (OperatingMode::Ft8, FT8_SLOT_MS),
                    "+" => (OperatingMode::Ft4, FT4_SLOT_MS),
                    _ => return Ingested::Dropped(Dropped::Filtered),
                };
                if time_ms >= MS_PER_DAY
                    || !SNR_RANGE.contains(&snr)
                    || !delta_time.is_finite()
                    || delta_time.abs() > MAX_ABS_DELTA_TIME_S
                    || delta_freq > MAX_DELTA_FREQ_HZ
                {
                    return Ingested::Dropped(Dropped::Malformed);
                }
                let text = sanitize_decoded_field(message.trim()).trim().to_string();
                if text.is_empty() {
                    return Ingested::Dropped(Dropped::Malformed);
                }
                let Some(&(dial, _)) = self.senders.get(&id) else {
                    return Ingested::Dropped(Dropped::NoStatus);
                };
                if !self.take_budget(now) {
                    return Ingested::Dropped(Dropped::OverBudget);
                }
                Ingested::Decode(build_relayed(
                    &text, snr, delta_time, delta_freq, time_ms, slot_ms, op_mode, dial,
                ))
            }
            InMsg::Heartbeat { .. }
            | InMsg::Clear { .. }
            | InMsg::Reply { .. }
            | InMsg::Replay
            | InMsg::HaltTx { .. }
            | InMsg::Other(_) => Ingested::Ignored,
        }
    }

    /// Count one forwarded decode against the fixed intake window.
    fn take_budget(&mut self, now: Instant) -> bool {
        let in_window = self
            .window_start
            .is_some_and(|start| now.saturating_duration_since(start) < self.limits.window);
        if !in_window {
            self.window_start = Some(now);
            self.window_count = 0;
        }
        if self.window_count >= self.limits.budget {
            return false;
        }
        self.window_count += 1;
        true
    }
}

/// Build the display decode exactly as the native path shapes one: the
/// native constructor, the native sanitizer, and a free-text guard (D6).
#[allow(clippy::too_many_arguments)]
fn build_relayed(
    text: &str,
    snr: i32,
    delta_time: f64,
    delta_freq: u32,
    time_ms: u32,
    slot_ms: u32,
    mode: OperatingMode,
    dial: u64,
) -> RelayedDecode {
    let mut decoded =
        pancetta_ft8::DecodedMessage::from_ft8lib(text, delta_freq as f32, snr as f32, 0);
    decoded.time_offset = delta_time;
    decoded.slot_parity = Some(slot_parity_from_qtime(time_ms, slot_ms));
    decoded.captured_dial_hz = Some(dial);
    sanitize_decoded_message(&mut decoded);

    // Free text ("TNX BOB 73 GL") parses with a word as the from-call. Keep
    // the text for Band Activity, but give it no callsigns or grid so it
    // creates no DX Hunter row. An unresolved `<...>` is left alone: DX
    // Hunter already ignores it.
    let implausible_from = decoded
        .message
        .from_callsign
        .as_deref()
        .map(|call| call.trim().to_ascii_uppercase())
        .is_some_and(|call| {
            pancetta_core::callsign::resolve_hash_render(&call)
                .is_some_and(|resolved| !pancetta_core::callsign::is_plausible_callsign(resolved))
        });
    if implausible_from {
        decoded.message.from_callsign = None;
        decoded.message.to_callsign = None;
        decoded.message.grid_square = None;
    }

    RelayedDecode {
        message: decoded,
        dial_hz: dial,
        origin: DecodeOrigin::WsjtxUdp { mode },
    }
}

/// Slot parity from a Decode's `Time` (ms since UTC midnight). A day holds
/// an even number of FT8 and FT4 slots, so this matches the parity of the
/// absolute slot regardless of date. The time is rounded to the nearest
/// slot start, because WSJT-X renders an FT4 slot start such as 12:00:07.5
/// at whole-second resolution (`120007`), which plain division would put in
/// the previous slot.
pub(crate) fn slot_parity_from_qtime(time_ms: u32, slot_ms: u32) -> SlotParity {
    let slot_ms = slot_ms.max(1);
    if (time_ms.saturating_add(slot_ms / 2) / slot_ms).is_multiple_of(2) {
        SlotParity::Even
    } else {
        SlotParity::Odd
    }
}

/// Drops worth telling the operator about, since the last summary.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct DropCounters {
    non_loopback: u64,
    malformed: u64,
    no_status: u64,
    over_budget: u64,
    queue_full: u64,
}

impl DropCounters {
    /// Count one drop. `OwnEcho` and `Filtered` are normal traffic and are
    /// never summarised.
    pub(crate) fn record(&mut self, reason: Dropped) {
        match reason {
            Dropped::NonLoopback => self.non_loopback += 1,
            Dropped::Malformed => self.malformed += 1,
            Dropped::NoStatus => self.no_status += 1,
            Dropped::OverBudget => self.over_budget += 1,
            Dropped::OwnEcho | Dropped::Filtered => {}
        }
    }

    /// Count one decode the display channel could not take.
    pub(crate) fn record_queue_full(&mut self) {
        self.queue_full += 1;
    }

    /// The summary line, or `None` when nothing actionable was dropped.
    /// Info when the only reason is "before WSJT-X sent a Status" (normal for
    /// a few seconds after start), Warn otherwise.
    pub(crate) fn summary(&self, limits: &IngestLimits) -> Option<(DiagnosticLevel, String)> {
        let total = self.non_loopback
            + self.malformed
            + self.no_status
            + self.over_budget
            + self.queue_full;
        if total == 0 {
            return None;
        }
        let mut parts = Vec::new();
        if self.non_loopback > 0 {
            parts.push(format!("{} from non-loopback sources", self.non_loopback));
        }
        if self.malformed > 0 {
            parts.push(format!("{} malformed", self.malformed));
        }
        if self.no_status > 0 {
            parts.push(format!("{} before WSJT-X sent a Status", self.no_status));
        }
        if self.over_budget > 0 {
            parts.push(format!(
                "{} over the {}-per-{} limit",
                self.over_budget,
                limits.budget,
                fmt_duration(limits.window)
            ));
        }
        if self.queue_full > 0 {
            parts.push(format!("{} with the display queue full", self.queue_full));
        }
        let level = if self.no_status == total {
            DiagnosticLevel::Info
        } else {
            DiagnosticLevel::Warn
        };
        let noun = if total == 1 { "datagram" } else { "datagrams" };
        Some((
            level,
            format!(
                "WSJT-X decode listener dropped {total} {noun} in the last {}: {}",
                fmt_duration(limits.summary_interval),
                parts.join(", ")
            ),
        ))
    }
}

/// `60 s` for whole seconds, `100 ms` otherwise.
fn fmt_duration(d: Duration) -> String {
    if d.subsec_millis() == 0 && d.as_secs() > 0 {
        format!("{} s", d.as_secs())
    } else {
        format!("{} ms", d.as_millis())
    }
}

/// Everything the listener task owns.
pub(crate) struct IngestContext {
    pub(crate) socket: tokio::net::UdpSocket,
    pub(crate) tui_tx: crossbeam_channel::Sender<RelayedDecode>,
    pub(crate) message_bus: MessageBus,
    pub(crate) shutdown: Arc<AtomicBool>,
    pub(crate) own_instance_id: String,
    pub(crate) limits: IngestLimits,
}

/// The listener task. Receive-only: it never sends a datagram. A full or
/// closed display channel drops the decode instead of blocking, because the
/// native decoder's send on that channel blocks. It never returns early, so
/// the supervisor never sees `WsjtxUdp` fail because of WSJT-X traffic.
pub(crate) async fn run_decode_ingest(ctx: IngestContext) -> Result<()> {
    let mut ingest = DecodeIngest::new(ctx.own_instance_id.clone(), ctx.limits);
    let mut counters = DropCounters::default();
    let mut last_summary = Instant::now();
    let mut buf = [0u8; 2048];

    while !ctx.shutdown.load(Ordering::Acquire) {
        match tokio::time::timeout(RECV_POLL, ctx.socket.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => match ingest.accept(&buf[..n], from, Instant::now()) {
                Ingested::Decode(decode) => match ctx.tui_tx.try_send(decode) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                        counters.record_queue_full();
                    }
                },
                Ingested::Dropped(reason) => {
                    debug!("WSJT-X decode listener: dropped a datagram from {from}: {reason:?}");
                    counters.record(reason);
                }
                Ingested::SenderUpdated | Ingested::Ignored => {}
            },
            Ok(Err(e)) => {
                // Windows reports an ICMP port-unreachable as a receive
                // error; nothing here is fatal. Back off briefly so a
                // persistent error cannot spin.
                debug!("WSJT-X decode listener: receive error: {e}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(_) => {}
        }
        if last_summary.elapsed() >= ctx.limits.summary_interval {
            if let Some((level, text)) = counters.summary(&ctx.limits) {
                super::emit_diagnostic(&ctx.message_bus, level, text).await;
            }
            counters = DropCounters::default();
            last_summary = Instant::now();
        }
    }
    Ok(())
}

/// What `start_wsjtx_decode_ingest` should do with this config and run mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IngestStart {
    /// `accept_decodes = false`.
    Off,
    /// `--replay`: live WSJT-X traffic would mix with replayed decodes.
    Replay,
    /// Headless: the display channel has only a debug drain.
    Headless,
    /// The address is unusable even though validation was bypassed.
    Refused(String),
    /// Bind this loopback address.
    Bind(SocketAddr),
}

/// Decide whether to start the listener. Refuses an unparseable or
/// non-loopback `decode_listen` even when config validation was bypassed.
pub(crate) fn ingest_start(cfg: &WsjtxUdpConfig, replay: bool, headless: bool) -> IngestStart {
    if !cfg.accept_decodes {
        return IngestStart::Off;
    }
    if replay {
        return IngestStart::Replay;
    }
    if headless {
        return IngestStart::Headless;
    }
    match cfg.decode_listen.parse::<SocketAddr>() {
        Err(_) => IngestStart::Refused("not an address:port".to_string()),
        Ok(addr) if !addr.ip().to_canonical().is_loopback() => {
            IngestStart::Refused("not a loopback address".to_string())
        }
        Ok(addr) => IngestStart::Bind(addr),
    }
}

fn bind_failure_text(listen: &str, reason: &str) -> String {
    format!(
        "WSJT-X decode listener could not bind {listen} ({reason}); WSJT-X decodes are off for this run"
    )
}

impl super::super::ApplicationCoordinator {
    /// PAN-381: start the WSJT-X decode listener when
    /// `[network.wsjtx_udp] accept_decodes = true`. Independent of
    /// `enabled`. Returns the bound address, or `Ok(None)` when nothing was
    /// bound (off, `--replay`, headless, refused address, bind failure); a
    /// bind failure is reported in Diagnostics and the station keeps running.
    pub(crate) async fn start_wsjtx_decode_ingest(&mut self) -> Result<Option<SocketAddr>> {
        let config = self.config.read().await;
        let cfg = config.network.wsjtx_udp.clone();
        drop(config);

        let addr = match ingest_start(&cfg, self.replay_mode(), self.headless) {
            IngestStart::Off => return Ok(None),
            IngestStart::Replay => {
                info!("Replay mode: WSJT-X decode listener not started");
                return Ok(None);
            }
            IngestStart::Headless => {
                info!(
                    "accept_decodes is on but pancetta is headless; WSJT-X decodes only reach \
                     the TUI, so the decode listener is not started"
                );
                return Ok(None);
            }
            IngestStart::Refused(reason) => {
                let text = bind_failure_text(&cfg.decode_listen, &reason);
                warn!("{text}");
                super::emit_diagnostic(&self.message_bus, DiagnosticLevel::Warn, text).await;
                return Ok(None);
            }
            IngestStart::Bind(addr) => addr,
        };

        let socket = match tokio::net::UdpSocket::bind(addr).await {
            Ok(socket) => socket,
            Err(e) => {
                let text = bind_failure_text(&cfg.decode_listen, &e.to_string());
                warn!("{text}");
                super::emit_diagnostic(&self.message_bus, DiagnosticLevel::Warn, text).await;
                return Ok(None);
            }
        };
        let local = socket.local_addr()?;
        let tui_tx = self.decode_handles()?.ft8_to_tui_tx.clone();

        let text = format!(
            "WSJT-X decode listener on {local}: set WSJT-X's UDP Server to this address; \
             its decodes are shown and scored, never transmitted on"
        );
        info!("{text}");
        super::emit_diagnostic(&self.message_bus, DiagnosticLevel::Info, text).await;

        let ctx = IngestContext {
            socket,
            tui_tx,
            message_bus: self.message_bus.clone(),
            shutdown: self.shutdown_signal.clone(),
            own_instance_id: cfg.instance_id.clone(),
            limits: IngestLimits::default(),
        };
        let handle = tokio::spawn(run_decode_ingest(ctx));
        self.named_task_handles
            .push((ComponentId::WsjtxUdp, handle));
        Ok(Some(local))
    }
}

#[cfg(test)]
#[path = "ingest/tests/config_reload_tests.rs"]
mod config_reload_tests;

#[cfg(test)]
#[path = "ingest/tests/decode_ingest_tests.rs"]
mod decode_ingest_tests;

#[cfg(test)]
#[path = "ingest/tests/listener_task_tests.rs"]
mod listener_task_tests;
