//! WSJT-X UDP companion-protocol support (GridTracker/JTAlert interop).
//!
//! See `docs/superpowers/specs/2026-07-13-wsjtx-udp-protocol-notes.md` for the
//! clean-room byte-level protocol reference this module implements against.
//!
//! Lifecycle mirrors `start_pskreporter_component`
//! (`pancetta/src/coordinator/psk_reporter.rs`): `enabled = false` (default)
//! ⇒ no socket is ever bound and a bus-drain task keeps the channel open so
//! the decoder fan-out never backs up; `enabled = true` ⇒ open the socket,
//! emit Heartbeat every 15s and change-driven Status, fan in FT8 decodes
//! (additive tap in `ft8.rs`) as Decode(2) into a 500-entry retention ring,
//! emit Clear(3) on band change, answer Replay(7) by re-emitting the ring
//! with `New=false`, and emit Close(6) on shutdown.

pub mod codec;

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use chrono::{DateTime, Timelike, Utc};
use tokio::time::{interval, sleep};
use tracing::{debug, info, warn};

use codec::{InMsg, OutMsg};
use pancetta_config::WsjtxUdpConfig;

use crate::message_bus::{ComponentId, MessageType};

/// Heartbeat cadence (protocol notes §1: every 15s, starting immediately at
/// boot — `tokio::time::interval`'s first tick fires immediately).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// Status sampling cadence (protocol notes §5: sample on a 1s timer, emit
/// only on change).
const STATUS_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// A decode within this many ms of "now" counts as `Decoding = true`.
const DECODING_WINDOW_MS: u64 = 2_000;

/// Inbound-poll timeout: bounds how long one `recv_from` wait can delay
/// servicing the Heartbeat/Status timers and the shutdown flag. Small enough
/// that the component stays responsive; large enough to avoid a busy loop.
const INBOUND_POLL_TIMEOUT: Duration = Duration::from_millis(200);

/// `Heartbeat.Version` string pancetta advertises. JTAlert reads this field
/// (⚑ protocol notes §7); a plausible WSJT-X-shaped version avoids
/// triggering any version-gated warning in companion apps.
const HEARTBEAT_VERSION: &str = "2.6.1";

/// Cap on the retained-decode ring used to answer Replay(7) (protocol notes
/// §5: "Retain decodes until you emit Clear") — matches the TUI's decode
/// channel bound so this never grows without limit across a long idle
/// session. Oldest entry evicted once a new decode would exceed this.
const DECODE_RING_CAPACITY: usize = 500;

impl super::ApplicationCoordinator {
    /// Start the WSJT-X UDP companion-protocol component.
    ///
    /// Lifecycle mirrors `start_pskreporter_component` exactly:
    /// `enabled = false` (default) ⇒ no socket is ever bound; a bus-drain
    /// task keeps the `ComponentId::WsjtxUdp` channel open so the decoder
    /// fan-out never backs up against a channel with no reader. `enabled =
    /// true` ⇒ snapshot config, open the socket, emit an immediate Status
    /// (GridTracker instance-validity rule — protocol notes §5: an instance
    /// is valid only after its first Status), then loop: draining the bus
    /// (mapping each decode to Decode(2), retaining it in a 500-entry ring,
    /// and emitting it), a 15s Heartbeat (+ Status right after each one)
    /// and a 1s change-driven Status sample (both also sample the dial
    /// frequency for Clear-on-band-change), and polling for inbound
    /// datagrams (answering Replay(7) by re-emitting the ring with
    /// `New=false` then a Status). Emits Close(6) on loop exit.
    pub(crate) async fn start_wsjtx_udp_component(&mut self) -> Result<()> {
        let config = self.config.read().await;
        if !config.network.wsjtx_udp.enabled {
            info!("WSJT-X UDP companion protocol disabled in configuration");
            drop(config);
            self.spawn_wsjtx_drain_task().await?;
            return Ok(());
        }

        let cfg = config.network.wsjtx_udp.clone();
        let station_ids = StationIds {
            de_call: config.station.callsign.clone(),
            de_grid: config.station.grid_square.clone(),
        };
        drop(config);

        let socket = match open_socket(&cfg) {
            Ok(s) => s,
            Err(e) => {
                // Fail-safe, not fail-closed-app: a bad destination/interface
                // shouldn't crash the whole station. Fall back to the same
                // drain-only shape as the disabled path so the channel still
                // has a reader.
                warn!("WSJT-X UDP: failed to open socket ({e}); component disabled for this run");
                self.spawn_wsjtx_drain_task().await?;
                return Ok(());
            }
        };

        let dest: SocketAddr = cfg
            .destination
            .parse()
            .context("invalid wsjtx_udp destination")?;

        info!(
            "Starting WSJT-X UDP companion component (destination: {}, id: \"{}\")",
            cfg.destination, cfg.instance_id
        );

        let (_wsjtx_tx, wsjtx_rx) = self
            .message_bus
            .create_channel(ComponentId::WsjtxUdp)
            .await?;

        let state = WsjtxState {
            operating_frequency_hz: self.operating_frequency_hz.clone(),
            active_protocol_mode: self.active_protocol_mode.clone(),
            tx_offset_hold_hz: self.tx_offset_hold_hz.clone(),
            tx_policy: self.tx_policy.clone(),
            ptt_active: self.ptt_active.clone(),
            last_decode_timestamp: self.last_decode_timestamp.clone(),
        };

        let instance_id = cfg.instance_id.clone();
        let shutdown = self.shutdown_signal.clone();

        let wsjtx_handle = tokio::spawn(async move {
            // GridTracker instance-validity rule (protocol notes §5): the
            // FIRST Status this session emits must precede any Decode —
            // send it synchronously here, before the loop (and therefore
            // before any future Decode fan-in drained from `wsjtx_rx`) ever
            // gets a chance to run.
            let initial_snapshot = state.sample();
            let initial_status = build_status(&initial_snapshot, &station_ids);
            let mut last_status = initial_status.encode(&instance_id);
            if let Err(e) = socket.send_to(&last_status, dest).await {
                warn!("WSJT-X UDP: initial Status send failed: {e}");
            }

            let mut heartbeat_timer = interval(HEARTBEAT_INTERVAL);
            let mut status_timer = interval(STATUS_SAMPLE_INTERVAL);
            let mut inbound_buf = [0u8; 2048];

            // Retention ring for Replay(7) support, seeded empty. Band
            // pre-seeded from the initial snapshot so the first sample never
            // spuriously fires a Clear (there's nothing to invalidate yet).
            let mut decode_ring: VecDeque<(OutMsg, StoredKey)> = VecDeque::new();
            let mut last_band = Some(band_of(initial_snapshot.dial_frequency_hz));

            'outer: while !shutdown.load(Ordering::Acquire) {
                // (c) Drain the bus channel: fan-in from the FT8 decoder
                // tap (ft8.rs, gated on `wsjtx_enabled`). Each decode is
                // mapped, retained in the ring, and emitted as Decode(2).
                loop {
                    match wsjtx_rx.try_recv() {
                        Ok(message) => {
                            if let MessageType::DecodedMessage(decoded) = message.message_type {
                                let mode = pancetta_config::OperatingMode::from_u8(
                                    state.active_protocol_mode.load(Ordering::Relaxed),
                                );
                                if let Some(out) = decode_to_msg(&decoded, mode) {
                                    if let Some(key) = stored_key(&out) {
                                        push_decode_ring(
                                            &mut decode_ring,
                                            (out.clone(), key),
                                            DECODE_RING_CAPACITY,
                                        );
                                    }
                                    if let Err(e) =
                                        socket.send_to(&out.encode(&instance_id), dest).await
                                    {
                                        warn!("WSJT-X UDP: decode send failed: {e}");
                                    }
                                }
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => break,
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            debug!("WSJT-X UDP channel disconnected");
                            break 'outer;
                        }
                    }
                }

                tokio::select! {
                    // (a) 15s Heartbeat, followed by a Status (protocol
                    // notes §5: "emit ... after each Heartbeat"). Fires
                    // immediately on the very first tick (interval's
                    // documented behavior), matching "start immediately at
                    // boot".
                    _ = heartbeat_timer.tick() => {
                        let hb = OutMsg::Heartbeat {
                            max_schema: 3,
                            version: HEARTBEAT_VERSION.to_string(),
                            revision: String::new(),
                        };
                        if let Err(e) = socket.send_to(&hb.encode(&instance_id), dest).await {
                            warn!("WSJT-X UDP: heartbeat send failed: {e}");
                        }
                        let snapshot = state.sample();
                        if let Some(band) = band_changed(snapshot.dial_frequency_hz, last_band) {
                            last_band = Some(band);
                            decode_ring.clear();
                            if let Err(e) = socket.send_to(&OutMsg::Clear.encode(&instance_id), dest).await {
                                warn!("WSJT-X UDP: Clear send failed: {e}");
                            }
                        }
                        let encoded = build_status(&snapshot, &station_ids).encode(&instance_id);
                        if let Err(e) = socket.send_to(&encoded, dest).await {
                            warn!("WSJT-X UDP: post-heartbeat status send failed: {e}");
                        }
                        last_status = encoded;
                    }
                    // (b) 1s Status sample, emitted only when the encoded
                    // bytes changed since the last send (protocol notes §5).
                    // Also the band-change sampler: a dial-frequency move
                    // into a new MHz bucket invalidates the retained ring.
                    _ = status_timer.tick() => {
                        let snapshot = state.sample();
                        if let Some(band) = band_changed(snapshot.dial_frequency_hz, last_band) {
                            last_band = Some(band);
                            decode_ring.clear();
                            if let Err(e) = socket.send_to(&OutMsg::Clear.encode(&instance_id), dest).await {
                                warn!("WSJT-X UDP: Clear send failed: {e}");
                            }
                        }
                        let encoded = build_status(&snapshot, &station_ids).encode(&instance_id);
                        if encoded != last_status {
                            if let Err(e) = socket.send_to(&encoded, dest).await {
                                warn!("WSJT-X UDP: status send failed: {e}");
                            }
                            last_status = encoded;
                        }
                    }
                    // (d) Inbound poll. Replay(7) is handled here: re-emit
                    // every retained decode as Decode(2) with New=false,
                    // then a Status (protocol notes §4 IN Replay). Every
                    // other inbound message type's dispatch (Reply/HaltTx/
                    // ...) arrives in a later task (Task 6); those datagrams
                    // are still read (to keep the socket buffer draining)
                    // and discarded at debug level. The timeout also
                    // throttles this branch so the loop never busy-spins.
                    recv = tokio::time::timeout(INBOUND_POLL_TIMEOUT, socket.recv_from(&mut inbound_buf)) => {
                        if let Ok(Ok((n, from))) = recv {
                            match InMsg::decode(&inbound_buf[..n]) {
                                Some((_id, InMsg::Replay)) => {
                                    for (out, _key) in &decode_ring {
                                        if let Some(replay) = as_replay(out) {
                                            if let Err(e) = socket
                                                .send_to(&replay.encode(&instance_id), dest)
                                                .await
                                            {
                                                warn!("WSJT-X UDP: replay decode send failed: {e}");
                                            }
                                        }
                                    }
                                    let encoded = build_status(&state.sample(), &station_ids).encode(&instance_id);
                                    if let Err(e) = socket.send_to(&encoded, dest).await {
                                        warn!("WSJT-X UDP: post-replay status send failed: {e}");
                                    }
                                    last_status = encoded;
                                }
                                Some(_) => {
                                    debug!(
                                        "WSJT-X UDP: {} inbound bytes from {} (dispatch not wired yet, Task 6)",
                                        n, from
                                    );
                                }
                                None => {
                                    debug!(
                                        "WSJT-X UDP: {} inbound bytes from {} (undecodable, ignored)",
                                        n, from
                                    );
                                }
                            }
                        }
                    }
                }
            }

            let close = OutMsg::Close.encode(&instance_id);
            if let Err(e) = socket.send_to(&close, dest).await {
                warn!("WSJT-X UDP: Close send failed: {e}");
            }
            info!("WSJT-X UDP component stopped");
            Ok(())
        });

        self.named_task_handles
            .push((ComponentId::WsjtxUdp, wsjtx_handle));
        info!("WSJT-X UDP component started");
        Ok(())
    }

    /// Spawn the disabled-path drain task: creates the `ComponentId::WsjtxUdp`
    /// channel with no real reader beyond a discard loop, so a sender (the
    /// decoder fan-out, once wired) never fills the channel and floods
    /// "Channel full" warnings — same rationale as
    /// `start_pskreporter_component`'s disabled path (`psk_reporter.rs`).
    async fn spawn_wsjtx_drain_task(&mut self) -> Result<()> {
        let (_drain_tx, drain_rx) = self
            .message_bus
            .create_channel(ComponentId::WsjtxUdp)
            .await?;
        let shutdown = self.shutdown_signal.clone();
        let drain_handle = tokio::spawn(async move {
            while !shutdown.load(Ordering::Acquire) {
                loop {
                    match drain_rx.try_recv() {
                        Ok(_) => {}
                        Err(crossbeam_channel::TryRecvError::Empty) => break,
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            debug!("WSJT-X UDP drain channel disconnected");
                            return Ok(());
                        }
                    }
                }
                sleep(Duration::from_millis(100)).await;
            }
            Ok(())
        });
        self.named_task_handles
            .push((ComponentId::WsjtxUdp, drain_handle));
        Ok(())
    }
}

/// Shared atomics the component task samples into a [`StatusSnapshot`] on
/// every Heartbeat and every 1s Status tick. Cloned `Arc`s captured by the
/// spawned task at component-start time — the same pattern as every other
/// coordinator component (`psk_reporter.rs`'s `psk_operating_freq`, etc.).
struct WsjtxState {
    operating_frequency_hz: Arc<AtomicU64>,
    active_protocol_mode: Arc<AtomicU8>,
    tx_offset_hold_hz: Arc<AtomicU64>,
    tx_policy: Arc<AtomicU8>,
    ptt_active: Arc<AtomicBool>,
    last_decode_timestamp: Arc<AtomicU64>,
}

impl WsjtxState {
    /// Sample every atomic into a plain [`StatusSnapshot`] `build_status`
    /// can map onto the wire message. Kept as a thin, mostly-mechanical
    /// translation — the actual field-mapping logic (and its tests) lives
    /// in `build_status`.
    fn sample(&self) -> StatusSnapshot {
        let mode = pancetta_config::OperatingMode::from_u8(
            self.active_protocol_mode.load(Ordering::Relaxed),
        );
        let policy = pancetta_core::TxPolicy::from_u8(self.tx_policy.load(Ordering::Relaxed));
        let last_decode_ms = self.last_decode_timestamp.load(Ordering::Relaxed);
        let now_ms = super::now_epoch_ms();
        let decoding =
            last_decode_ms != 0 && now_ms.saturating_sub(last_decode_ms) <= DECODING_WINDOW_MS;

        StatusSnapshot {
            dial_frequency_hz: self.operating_frequency_hz.load(Ordering::Relaxed),
            mode: super::mode_str(mode),
            tx_df_hz: self.tx_offset_hold_hz.load(Ordering::Relaxed) as u32,
            tx_enabled: policy.allows_any_tx(),
            transmitting: self.ptt_active.load(Ordering::Relaxed),
            decoding,
        }
    }
}

/// Plain snapshot of the atomics sampled into a Status message. Kept
/// separate from the atomics themselves so [`build_status`] — the mapping
/// from sampled values to wire fields — is a pure, easily-tested function
/// with no `Arc`/`Ordering` involved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StatusSnapshot {
    /// `operating_frequency_hz` (mod.rs) — the rig dial frequency in Hz.
    pub(crate) dial_frequency_hz: u64,
    /// `mode_str(OperatingMode::from_u8(active_protocol_mode))`.
    pub(crate) mode: &'static str,
    /// `tx_offset_hold_hz` — the operator-held TX audio offset in Hz.
    pub(crate) tx_df_hz: u32,
    /// `TxPolicy::from_u8(tx_policy).allows_any_tx()`.
    pub(crate) tx_enabled: bool,
    /// `ptt_active`.
    pub(crate) transmitting: bool,
    /// `true` when `last_decode_timestamp` is within the last 2s.
    pub(crate) decoding: bool,
}

/// Station identity fields sampled once from config at component start,
/// mirroring the PSKReporter pattern (`psk_reporter.rs`: callsign/grid
/// snapshotted before the config read-lock is dropped).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StationIds {
    /// `config.station.callsign`.
    pub(crate) de_call: String,
    /// `config.station.grid_square`.
    pub(crate) de_grid: String,
}

/// Map a [`StatusSnapshot`] + [`StationIds`] onto the wire `Status` message
/// (protocol notes §4 OUT table — all 21 fields). This is intentionally a
/// pure function: no atomics, no I/O, no coordinator state — so field
/// mapping is exhaustively unit-tested without a socket or running
/// coordinator.
///
/// Fields this task has no live source for yet (DXCall/Report/DXGrid —
/// tracked from an active QSO/decode in later tasks; TxMessage — the text
/// currently being transmitted) are emitted as unset (empty string; the
/// codec always writes the explicit `0x00000000` empty form, never the
/// `0xFFFFFFFF` null sentinel — both decode to `""` per protocol notes §3).
pub(crate) fn build_status(snapshot: &StatusSnapshot, ids: &StationIds) -> OutMsg {
    OutMsg::Status {
        dial_frequency: snapshot.dial_frequency_hz,
        mode: snapshot.mode.to_string(),
        dx_call: String::new(),
        report: String::new(),
        tx_mode: snapshot.mode.to_string(),
        tx_enabled: snapshot.tx_enabled,
        transmitting: snapshot.transmitting,
        decoding: snapshot.decoding,
        rx_df: 0,
        tx_df: snapshot.tx_df_hz,
        de_call: ids.de_call.clone(),
        de_grid: ids.de_grid.clone(),
        dx_grid: String::new(),
        tx_watchdog: false,
        sub_mode: String::new(),
        fast_mode: false,
        special_operation_mode: 0,
        frequency_tolerance: u32::MAX,
        tr_period: u32::MAX,
        configuration_name: "Default".to_string(),
        tx_message: String::new(),
    }
}

/// Minimal decode-identity triple used to match an echoed `Reply(4)`
/// against a retained decode (protocol notes §4 IN Reply: "Message + Time +
/// DeltaFrequency uniquely identify a decode in practice"). Task 7 wires
/// the actual match; this task only builds and retains it alongside each
/// ring entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredKey {
    /// Same value as the paired `OutMsg::Decode`'s `time_ms`.
    pub(crate) time_ms: u32,
    /// Same value as the paired `OutMsg::Decode`'s `delta_freq`.
    pub(crate) delta_freq: u32,
    /// Same value as the paired `OutMsg::Decode`'s `message`.
    pub(crate) message: String,
}

/// Map a live FT8 decode onto the wire `Decode` message (protocol notes §4
/// OUT table). Pure — no atomics, no I/O, `mode` passed in explicitly — so
/// field mapping is exhaustively unit-tested without a running component.
///
/// v1 emits `Decode` only when the station's active mode is FT8: `"~"` is
/// the only byte-verified mode glyph (protocol notes §8 flag 7 — FT4's
/// glyph is unconfirmed from non-GPL sources). Returns `None` for any other
/// active mode rather than guessing an unverified glyph; this must not
/// perturb decode behavior outside this fan-out (`mode=FT8` paths stay
/// byte-identical — the gate only decides whether a *wsjtx* message is
/// produced).
pub(crate) fn decode_to_msg(
    d: &pancetta_ft8::DecodedMessage,
    mode: pancetta_config::OperatingMode,
) -> Option<OutMsg> {
    if mode != pancetta_config::OperatingMode::Ft8 {
        return None;
    }
    Some(OutMsg::Decode {
        new: true,
        time_ms: time_ms_since_midnight_utc(d.timestamp),
        snr: d.snr_db.round() as i32,
        delta_time: d.time_offset,
        delta_freq: d.frequency_offset as u32,
        mode: "~".to_string(),
        message: d.text.clone(),
        low_confidence: false,
        off_air: false,
    })
}

/// `QTime` wire value for a decode: milliseconds since midnight UTC of
/// `t` (protocol notes §3 QTime / §4 Decode `Time`).
fn time_ms_since_midnight_utc(t: SystemTime) -> u32 {
    let datetime: DateTime<Utc> = t.into();
    let time = datetime.time();
    time.num_seconds_from_midnight() * 1_000 + time.nanosecond() / 1_000_000
}

/// Extract the `StoredKey` for a just-built `Decode` message. `None` for
/// any other `OutMsg` variant (defensive — only ever called right after
/// [`decode_to_msg`] returns `Some`).
fn stored_key(out: &OutMsg) -> Option<StoredKey> {
    match out {
        OutMsg::Decode {
            time_ms,
            delta_freq,
            message,
            ..
        } => Some(StoredKey {
            time_ms: *time_ms,
            delta_freq: *delta_freq,
            message: message.clone(),
        }),
        _ => None,
    }
}

/// Push a new decode into the retention ring, evicting the oldest entry(s)
/// once the ring exceeds `cap`. Pure — the ring and cap are passed in
/// explicitly — so eviction behavior is unit-tested without a running
/// component/socket.
fn push_decode_ring(
    ring: &mut VecDeque<(OutMsg, StoredKey)>,
    entry: (OutMsg, StoredKey),
    cap: usize,
) {
    ring.push_back(entry);
    while ring.len() > cap {
        ring.pop_front();
    }
}

/// Integer-MHz-bucket approximation of "band", sufficient for detecting
/// the kind of frequency change that invalidates a band-activity window
/// (brief: "integer MHz bucket is sufficient" — a full band-plan lookup
/// isn't needed here).
fn band_of(dial_hz: u64) -> u64 {
    dial_hz / 1_000_000
}

/// Decide whether the sampled dial frequency has moved to a new band
/// bucket relative to `last_band`. Returns `Some(new_band)` when a
/// Clear-on-band-change should fire (and `last_band` should be updated to
/// it), `None` when the band is unchanged. Pure — no I/O — so band-change
/// detection is unit-tested without a socket.
fn band_changed(dial_hz: u64, last_band: Option<u64>) -> Option<u64> {
    let band = band_of(dial_hz);
    if last_band == Some(band) {
        None
    } else {
        Some(band)
    }
}

/// Rebuild a retained `Decode` for Replay(7): every field verbatim except
/// `New`, forced to `false` (protocol notes §4 IN Replay: "re-emitting
/// every decode still in the band-activity window as Decode(2) with
/// New=false"). `None` for any non-`Decode` `OutMsg` (defensive — the ring
/// only ever holds `Decode` entries).
fn as_replay(out: &OutMsg) -> Option<OutMsg> {
    match out.clone() {
        OutMsg::Decode {
            time_ms,
            snr,
            delta_time,
            delta_freq,
            mode,
            message,
            low_confidence,
            off_air,
            ..
        } => Some(OutMsg::Decode {
            new: false,
            time_ms,
            snr,
            delta_time,
            delta_freq,
            mode,
            message,
            low_confidence,
            off_air,
        }),
        _ => None,
    }
}

/// Multicast socket options to apply when the destination is a multicast
/// group. `None` when `dest` is unicast — no socket options are needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MulticastOptions {
    /// `IP_MULTICAST_TTL` to set (GridTracker uses 3; config default 3).
    pub(crate) ttl: u32,
    /// `IP_MULTICAST_IF` to set, when `multicast_interface` is configured.
    /// `None` leaves the OS default outgoing interface (WSJT-X's classic
    /// loopback-only cross-host failure mode — protocol notes §1).
    pub(crate) interface: Option<Ipv4Addr>,
}

/// Detect whether `dest` is a multicast destination and, if so, derive the
/// socket options `open_socket` must apply (protocol notes §1/§6: TTL ≥ 1,
/// `IP_MULTICAST_IF` set to the LAN NIC for cross-host reach). Pure and
/// side-effect-free — no socket is touched — so this is unit-tested without
/// an actual send.
pub(crate) fn multicast_options(
    dest: &SocketAddr,
    cfg: &WsjtxUdpConfig,
) -> Option<MulticastOptions> {
    let SocketAddr::V4(v4) = dest else {
        // IPv6 multicast destinations are out of scope for v1 (design spec
        // non-goals: "IPv6"); treat as non-multicast (no socket options).
        return None;
    };
    if !v4.ip().is_multicast() {
        return None;
    }
    let interface = if cfg.multicast_interface.is_empty() {
        None
    } else {
        cfg.multicast_interface.parse::<Ipv4Addr>().ok()
    };
    Some(MulticastOptions {
        ttl: cfg.multicast_ttl,
        interface,
    })
}

/// Open the WSJT-X-role UDP socket: bind an ephemeral local port, apply
/// multicast options when the configured destination is a multicast group,
/// and set non-blocking so the tokio-wrapped socket can be polled in the
/// component's select loop (protocol notes §1: one socket, both send and
/// `recv_from`, no `connect()` — multicast destinations aren't reply
/// sources).
pub(crate) fn open_socket(cfg: &WsjtxUdpConfig) -> Result<tokio::net::UdpSocket> {
    let dest: SocketAddr = cfg
        .destination
        .parse()
        .with_context(|| format!("invalid wsjtx_udp destination: {}", cfg.destination))?;

    // `set_multicast_if_v4` isn't exposed by `std::net::UdpSocket` — only by
    // socket2's `Socket` wrapper — so the socket is built with socket2 and
    // converted to `std::net::UdpSocket` (then to tokio's) once configured.
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .context("failed to create wsjtx_udp UDP socket")?;
    let bind_addr: SocketAddr = "0.0.0.0:0".parse().expect("valid bind literal");
    socket
        .bind(&bind_addr.into())
        .context("failed to bind wsjtx_udp UDP socket")?;

    if let Some(opts) = multicast_options(&dest, cfg) {
        socket
            .set_multicast_ttl_v4(opts.ttl)
            .context("failed to set wsjtx_udp multicast TTL")?;
        if let Some(iface) = opts.interface {
            socket
                .set_multicast_if_v4(&iface)
                .context("failed to set wsjtx_udp multicast interface")?;
        }
    }

    socket
        .set_nonblocking(true)
        .context("failed to set wsjtx_udp socket non-blocking")?;

    let std_socket: std::net::UdpSocket = socket.into();
    tokio::net::UdpSocket::from_std(std_socket).context("failed to wrap wsjtx_udp socket for tokio")
}

#[cfg(test)]
mod socket_tests {
    use super::*;

    #[test]
    fn multicast_destination_detected_and_options_derived() {
        let dest: SocketAddr = "224.0.0.73:2237".parse().unwrap();
        let cfg = WsjtxUdpConfig {
            multicast_interface: "192.168.1.5".to_string(),
            multicast_ttl: 3,
            ..WsjtxUdpConfig::default()
        };
        let opts = multicast_options(&dest, &cfg).expect("224.0.0.73 is multicast");
        assert_eq!(opts.ttl, 3);
        assert_eq!(opts.interface, Some(Ipv4Addr::new(192, 168, 1, 5)));
    }

    #[test]
    fn multicast_with_empty_interface_leaves_interface_unset() {
        let dest: SocketAddr = "239.255.0.1:2237".parse().unwrap();
        let cfg = WsjtxUdpConfig::default(); // multicast_interface = ""
        let opts = multicast_options(&dest, &cfg).expect("239.255.0.1 is multicast");
        assert_eq!(opts.interface, None);
        assert_eq!(opts.ttl, cfg.multicast_ttl);
    }

    #[test]
    fn unicast_destination_has_no_multicast_options() {
        let dest: SocketAddr = "127.0.0.1:2237".parse().unwrap();
        let cfg = WsjtxUdpConfig::default();
        assert_eq!(multicast_options(&dest, &cfg), None);

        let dest: SocketAddr = "192.168.1.20:2237".parse().unwrap();
        assert_eq!(multicast_options(&dest, &cfg), None);
    }

    #[tokio::test]
    async fn open_socket_binds_ephemeral_port_for_unicast_destination() {
        let cfg = WsjtxUdpConfig {
            destination: "127.0.0.1:2237".to_string(),
            ..WsjtxUdpConfig::default()
        };
        let socket = open_socket(&cfg).expect("unicast open_socket should succeed");
        // Ephemeral bind: local port is nonzero and not the destination port
        // (we never bind 2237 ourselves — protocol notes §1).
        assert_ne!(socket.local_addr().unwrap().port(), 0);
    }

    #[tokio::test]
    async fn open_socket_applies_multicast_options_without_sending() {
        let cfg = WsjtxUdpConfig {
            destination: "224.0.0.73:2237".to_string(),
            multicast_ttl: 3,
            ..WsjtxUdpConfig::default()
        };
        // No actual datagram is sent — this only exercises socket-option
        // setup (set_multicast_ttl_v4 / set_nonblocking) on a real bound
        // socket, confirming it doesn't error for a multicast destination.
        let socket = open_socket(&cfg).expect("multicast open_socket should succeed");
        assert_ne!(socket.local_addr().unwrap().port(), 0);
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use pancetta_core::TxPolicy;

    fn snapshot(tx_enabled: bool) -> StatusSnapshot {
        StatusSnapshot {
            dial_frequency_hz: 14_074_000,
            mode: "FT8",
            tx_df_hz: 1500,
            tx_enabled,
            transmitting: false,
            decoding: true,
        }
    }

    fn ids() -> StationIds {
        StationIds {
            de_call: "K5ARH".to_string(),
            de_grid: "EM12".to_string(),
        }
    }

    #[test]
    fn maps_dial_mode_txdf_de_call_de_grid() {
        let status = build_status(&snapshot(true), &ids());
        let OutMsg::Status {
            dial_frequency,
            mode,
            tx_df,
            de_call,
            de_grid,
            ..
        } = status
        else {
            panic!("expected OutMsg::Status");
        };
        assert_eq!(dial_frequency, 14_074_000);
        assert_eq!(mode, "FT8");
        assert_eq!(tx_df, 1500);
        assert_eq!(de_call, "K5ARH");
        assert_eq!(de_grid, "EM12");
    }

    #[test]
    fn tx_enabled_true_for_full_and_respond_only() {
        for policy in [TxPolicy::Full, TxPolicy::RespondOnly] {
            let s = StatusSnapshot {
                tx_enabled: policy.allows_any_tx(),
                ..snapshot(false)
            };
            let OutMsg::Status { tx_enabled, .. } = build_status(&s, &ids()) else {
                panic!("expected OutMsg::Status");
            };
            assert!(tx_enabled, "{policy:?} should map to TxEnabled=true");
        }
    }

    #[test]
    fn tx_enabled_false_for_disabled_policy() {
        let s = StatusSnapshot {
            tx_enabled: TxPolicy::Disabled.allows_any_tx(),
            ..snapshot(true)
        };
        let OutMsg::Status { tx_enabled, .. } = build_status(&s, &ids()) else {
            panic!("expected OutMsg::Status");
        };
        assert!(!tx_enabled);
    }

    #[test]
    fn unset_strings_are_null_i_e_empty() {
        let status = build_status(&snapshot(true), &ids());
        let OutMsg::Status {
            dx_call,
            report,
            dx_grid,
            sub_mode,
            tx_message,
            ..
        } = status
        else {
            panic!("expected OutMsg::Status");
        };
        assert_eq!(dx_call, "");
        assert_eq!(report, "");
        assert_eq!(dx_grid, "");
        assert_eq!(sub_mode, "");
        assert_eq!(tx_message, "");
    }

    #[test]
    fn frequency_tolerance_and_tr_period_are_sentinel_unset() {
        let status = build_status(&snapshot(true), &ids());
        let OutMsg::Status {
            frequency_tolerance,
            tr_period,
            ..
        } = status
        else {
            panic!("expected OutMsg::Status");
        };
        assert_eq!(frequency_tolerance, u32::MAX);
        assert_eq!(tr_period, u32::MAX);
    }

    #[test]
    fn special_operation_mode_is_always_zero() {
        let status = build_status(&snapshot(true), &ids());
        let OutMsg::Status {
            special_operation_mode,
            ..
        } = status
        else {
            panic!("expected OutMsg::Status");
        };
        assert_eq!(special_operation_mode, 0);
    }
}

#[cfg(test)]
mod decode_tests {
    use super::*;
    use chrono::TimeZone;
    use pancetta_config::OperatingMode;
    use pancetta_ft8::{DecodedMessage, Ft8Message};

    /// Build a `DecodedMessage` fixture with explicit `text`/`timestamp`
    /// overrides (the constructor derives `text` from an empty `Ft8Message`
    /// and stamps `timestamp = SystemTime::now()`, neither of which is
    /// deterministic enough for these tests).
    fn decoded(
        snr_db: f32,
        frequency_offset: f64,
        time_offset: f64,
        text: &str,
        timestamp: SystemTime,
    ) -> DecodedMessage {
        let mut d = DecodedMessage::new(
            Ft8Message::default(),
            snr_db,
            1.0,
            frequency_offset,
            time_offset,
        );
        d.text = text.to_string();
        d.timestamp = timestamp;
        d
    }

    fn known_timestamp() -> SystemTime {
        // 2026-07-15 12:34:56.789 UTC — mid-day, non-trivial ms remainder,
        // so both the seconds and sub-second parts of `time_ms_since_
        // midnight_utc` are exercised.
        Utc.with_ymd_and_hms(2026, 7, 15, 12, 34, 56)
            .unwrap()
            .checked_add_signed(chrono::Duration::milliseconds(789))
            .unwrap()
            .into()
    }

    #[test]
    fn ft8_mode_emits_decode_with_tilde_glyph_and_text() {
        let d = decoded(-5.0, 1302.0, 0.2, "CQ K1ABC FN42", known_timestamp());
        let out = decode_to_msg(&d, OperatingMode::Ft8).expect("FT8 mode must emit a Decode");
        let OutMsg::Decode {
            new,
            mode,
            message,
            delta_time,
            low_confidence,
            off_air,
            ..
        } = out
        else {
            panic!("expected OutMsg::Decode");
        };
        assert!(new, "live decodes must be New=true");
        assert_eq!(
            mode, "~",
            "protocol notes §8: only \"~\" is byte-verified for FT8"
        );
        assert_eq!(message, "CQ K1ABC FN42");
        assert_eq!(delta_time, 0.2);
        assert!(!low_confidence);
        assert!(!off_air);
    }

    #[test]
    fn non_ft8_active_mode_emits_nothing() {
        let d = decoded(-5.0, 1302.0, 0.2, "CQ K1ABC FN42", known_timestamp());
        // v1 only has a byte-verified glyph for FT8 (protocol notes §8 flag
        // 7) — FT4/FT2 must not guess one.
        assert!(decode_to_msg(&d, OperatingMode::Ft4).is_none());
        assert!(decode_to_msg(&d, OperatingMode::Ft2).is_none());
    }

    #[test]
    fn snr_rounds_to_nearest_i32() {
        let cases: [(f32, i32); 4] = [(-4.6, -5), (13.4, 13), (2.5, 3), (-2.5, -3)];
        for (snr_db, expected) in cases {
            let d = decoded(snr_db, 0.0, 0.0, "x", known_timestamp());
            let OutMsg::Decode { snr, .. } = decode_to_msg(&d, OperatingMode::Ft8).unwrap() else {
                panic!("expected OutMsg::Decode");
            };
            assert_eq!(snr, expected, "snr_db {snr_db} should round to {expected}");
        }
    }

    #[test]
    fn time_ms_is_milliseconds_since_utc_midnight() {
        let d = decoded(0.0, 0.0, 0.0, "x", known_timestamp());
        let OutMsg::Decode { time_ms, .. } = decode_to_msg(&d, OperatingMode::Ft8).unwrap() else {
            panic!("expected OutMsg::Decode");
        };
        // 12:34:56.789 UTC -> ((12*3600 + 34*60 + 56) * 1000) + 789
        assert_eq!(time_ms, 45_296_789);
    }

    #[test]
    fn delta_freq_truncates_frequency_offset_to_u32() {
        let d = decoded(0.0, 1302.9, 0.0, "x", known_timestamp());
        let OutMsg::Decode { delta_freq, .. } = decode_to_msg(&d, OperatingMode::Ft8).unwrap()
        else {
            panic!("expected OutMsg::Decode");
        };
        assert_eq!(delta_freq, 1302);
    }

    #[test]
    fn stored_key_matches_the_decode_it_was_built_from() {
        let d = decoded(-5.0, 1302.0, 0.2, "CQ K1ABC FN42", known_timestamp());
        let out = decode_to_msg(&d, OperatingMode::Ft8).unwrap();
        let key = stored_key(&out).expect("Decode variant must yield a StoredKey");
        let OutMsg::Decode {
            time_ms,
            delta_freq,
            message,
            ..
        } = &out
        else {
            unreachable!()
        };
        assert_eq!(key.time_ms, *time_ms);
        assert_eq!(key.delta_freq, *delta_freq);
        assert_eq!(&key.message, message);
    }

    #[test]
    fn stored_key_is_none_for_non_decode_messages() {
        assert!(stored_key(&OutMsg::Clear).is_none());
    }

    #[test]
    fn push_decode_ring_evicts_oldest_past_capacity() {
        let mut ring: VecDeque<(OutMsg, StoredKey)> = VecDeque::new();
        for i in 0..5u32 {
            let entry = (
                OutMsg::Clear,
                StoredKey {
                    time_ms: i,
                    delta_freq: 0,
                    message: i.to_string(),
                },
            );
            push_decode_ring(&mut ring, entry, 3);
        }
        assert_eq!(ring.len(), 3);
        let kept: Vec<u32> = ring.iter().map(|(_, k)| k.time_ms).collect();
        assert_eq!(
            kept,
            vec![2, 3, 4],
            "oldest entries evicted, insertion order preserved"
        );
    }

    #[test]
    fn push_decode_ring_caps_at_the_real_500_entry_capacity() {
        let mut ring: VecDeque<(OutMsg, StoredKey)> = VecDeque::new();
        for i in 0..(DECODE_RING_CAPACITY as u32 + 10) {
            let entry = (
                OutMsg::Clear,
                StoredKey {
                    time_ms: i,
                    delta_freq: 0,
                    message: String::new(),
                },
            );
            push_decode_ring(&mut ring, entry, DECODE_RING_CAPACITY);
        }
        assert_eq!(ring.len(), DECODE_RING_CAPACITY);
    }

    #[test]
    fn band_changed_none_when_same_mhz_bucket() {
        // 14.074 MHz and 14.078 MHz are both bucket 14.
        assert_eq!(band_changed(14_078_000, Some(14)), None);
    }

    #[test]
    fn band_changed_some_when_mhz_bucket_differs() {
        // 14.074 MHz (bucket 14) -> 7.074 MHz (bucket 7).
        assert_eq!(band_changed(7_074_000, Some(14)), Some(7));
    }

    #[test]
    fn band_changed_some_on_first_sample_with_no_prior_band() {
        assert_eq!(band_changed(14_074_000, None), Some(14));
    }

    #[test]
    fn as_replay_forces_new_false_and_keeps_other_fields() {
        let d = decoded(-5.0, 1302.0, 0.2, "CQ K1ABC FN42", known_timestamp());
        let live = decode_to_msg(&d, OperatingMode::Ft8).unwrap();
        let replay = as_replay(&live).expect("Decode variant must replay");
        let OutMsg::Decode {
            new: live_new,
            message: live_message,
            ..
        } = &live
        else {
            unreachable!()
        };
        let OutMsg::Decode {
            new: replay_new,
            message: replay_message,
            ..
        } = &replay
        else {
            unreachable!()
        };
        assert!(*live_new);
        assert!(!*replay_new, "Replay(7) responses must carry New=false");
        assert_eq!(live_message, replay_message);
    }

    #[test]
    fn as_replay_is_none_for_non_decode_messages() {
        assert!(as_replay(&OutMsg::Clear).is_none());
    }
}
