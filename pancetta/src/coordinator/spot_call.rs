//! PAN-287: Space on a DX Hunter network spot (DX cluster / cqdx row never
//! heard locally) QSYs the rig to the spot's standard dial and calls at the
//! spot's audio offset, or at the frequency allocator's pick when the spot
//! carries no usable offset.
//!
//! The TUI resolves the dial and offset (`pancetta_tui::spot_call`); this
//! module admits and executes. Rules (see `docs/DECISIONS/tui.md`):
//!
//! - A dial change never happens while the rig is keyed: the final
//!   [`rig_is_keyed`] recheck and the `SetFrequency` enqueue run under
//!   `ptt_sync_gate`, so neither `PttGuard::new` nor the TUI's manual
//!   key-down (`tui_relay::manual_key_down`) can key in between. The
//!   autonomous operator is **never** locked while that gate is held (the
//!   Autonomous task holds the operator lock while its offset drain takes
//!   `ptt_sync_gate` — the opposite order would deadlock).
//! - A dial change is refused when the admission read finds an exchange in
//!   progress (a QSO whose partner has answered), and never happens without
//!   a Hamlib route. That read is a snapshot taken just before
//!   `ptt_sync_gate`, not serialized with the QSO task: a partner reply the
//!   QSO task applies after it and before [`qsy_send`] stores the new dial
//!   is still torn down by the `BandChanged`. The read cannot move under the
//!   gate, because `QsoManager::apply_tx_offset_switch` takes the manager's
//!   `qsos` lock before `ptt_sync_gate`.
//! - A call that needs a QSY while keyed is parked (latest wins) and runs
//!   after PTT-off. It is dropped after [`SPOT_QSY_WAIT_CAP`], on any
//!   operator frequency change or operating-mode switch, by a newer spot
//!   call, or when the TX policy, exchange state or rig route no longer allow
//!   it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::message_bus::{
    ComponentId, ComponentMessage, MessageBus, MessageType, QsoMessage, RigControlMessage,
};
use pancetta_tui::tui_runner::TuiMessage;

/// The autonomous operator as the Autonomous task publishes it.
pub(crate) type OperatorHandle = Arc<tokio::sync::Mutex<pancetta_qso::AutonomousOperator>>;

/// How long a parked spot call waits for PTT-off: two FT8 slots, enough to
/// finish one frame plus one more back-to-back frame.
pub(crate) const SPOT_QSY_WAIT_CAP: Duration = Duration::from_secs(30);

/// How often a parked spot call re-reads the keyed state.
const SPOT_QSY_POLL: Duration = Duration::from_millis(100);

/// Dial slack for "already there": absorbs rig readback rounding, far inside
/// the 100 Hz relevance gate.
const SAME_DIAL_SLACK_HZ: u64 = 10;

/// `true` while the rig is keyed or a key-down is being enqueued. `ptt_active`
/// is the TX worker's flag. Manual PTT and tune key through
/// `tx::send_ptt_command`, which sets bit 1 of `gain_tx_gate` once a key-down
/// is enqueued; Hamlib clears it on an acknowledged bus PTT-off. Bits 2+
/// count key-downs mid-enqueue. Bit 0 is the receive-gain transition, which
/// is not a key-down. Shared by the station agent's remote QSY refusal
/// (PAN-252) and the spot-call QSY (PAN-287).
pub(crate) fn rig_is_keyed(ptt_active: &AtomicBool, bus: &MessageBus) -> bool {
    ptt_active.load(Ordering::Acquire) || bus.gain_tx_gate.load(Ordering::Acquire) & !1 != 0
}

/// Partners of every active QSO whose exchange is in progress (the partner
/// has answered). `CallingCq` and `RespondingToCq` are not exchanges: a band
/// change ends them exactly like a band key does. A `Completed` QSO whose
/// closing RR73/73 is queued but not yet aired (PAN-280) still counts: a QSY
/// would air that frame on the new band. Other terminal QSOs are ignored.
pub(crate) async fn exchange_in_progress(manager: Option<pancetta_qso::QsoManager>) -> Vec<String> {
    use pancetta_qso::states::{ContestState, QsoState};
    let Some(manager) = manager else {
        return Vec::new();
    };
    let (active, pending_close_partners) =
        manager.active_qsos_and_pending_completion_partners().await;
    let mut partners: Vec<String> = active
        .into_iter()
        .filter_map(|(_, progress)| match progress.state {
            QsoState::WaitingForReport { their_callsign, .. }
            | QsoState::SendingReport { their_callsign, .. }
            | QsoState::WaitingForConfirmation { their_callsign, .. }
            | QsoState::SendingConfirmation { their_callsign, .. }
            | QsoState::Contest(ContestState::ExchangingInfo { their_callsign, .. }) => {
                Some(their_callsign)
            }
            _ => None,
        })
        .collect();
    partners.extend(pending_close_partners);
    partners
}

/// `true` when the rig is already on `target_hz` (within
/// [`SAME_DIAL_SLACK_HZ`]). A current dial of `0` is unknown and never
/// counts as there.
pub(crate) fn same_dial(current_hz: u64, target_hz: u64) -> bool {
    current_hz != 0 && current_hz.abs_diff(target_hz) <= SAME_DIAL_SLACK_HZ
}

/// What to do with a spot call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SpotAdmission {
    /// Run it now; `qsy` says whether the dial must move first.
    Proceed { qsy: bool },
    /// It needs a QSY and the rig is keyed: park it until PTT-off.
    Defer,
    /// Refuse with this status line. Nothing is retuned or keyed.
    Refuse(String),
}

/// Everything [`admit_spot_call`] decides on. Pure data.
#[derive(Debug, Clone)]
pub(crate) struct AdmissionInputs<'a> {
    pub callsign: &'a str,
    pub band: &'a str,
    pub dial_hz: u64,
    pub own_call: bool,
    pub policy: pancetta_core::TxPolicy,
    pub has_rig_route: bool,
    pub needs_qsy: bool,
    pub keyed: bool,
    pub exchanges: &'a [String],
    pub offset_known: bool,
    pub allocator_available: bool,
}

/// Pure admission for a spot call. Refusals use the exact operator-facing
/// lines; a call that needs no QSY never waits for PTT-off and is never
/// blocked by an exchange in progress.
pub(crate) fn admit_spot_call(i: &AdmissionInputs<'_>) -> SpotAdmission {
    let call = i.callsign;
    if i.own_call {
        return SpotAdmission::Refuse(format!("Refusing to call our own station ({call})"));
    }
    if !i.policy.allows_initiation() {
        return SpotAdmission::Refuse(format!(
            "Can't call {call} — TX policy is {} (press g for Full)",
            i.policy.label()
        ));
    }
    if i.needs_qsy && !i.has_rig_route {
        return SpotAdmission::Refuse(format!(
            "Can't QSY to {} for {call} — no rig control (tune to {:.3} MHz, then press Space)",
            i.band,
            i.dial_hz as f64 / 1e6
        ));
    }
    if i.needs_qsy && !i.exchanges.is_empty() {
        return SpotAdmission::Refuse(match i.exchanges {
            [one] => format!(
                "Can't QSY to {} for {call} — QSO with {one} in progress (finish it or abort with k)",
                i.band
            ),
            many => format!(
                "Can't QSY to {} for {call} — {} QSOs in progress (finish or abort them with k)",
                i.band,
                many.len()
            ),
        });
    }
    if !i.offset_known && !i.allocator_available {
        return SpotAdmission::Refuse(allocator_not_ready(call));
    }
    if i.needs_qsy && i.keyed {
        return SpotAdmission::Defer;
    }
    SpotAdmission::Proceed { qsy: i.needs_qsy }
}

fn allocator_not_ready(call: &str) -> String {
    format!("Can't call {call} — TX offset allocator not ready, try again")
}

/// Handles shared by every pancetta-commanded QSY from the TUI relay.
#[derive(Clone)]
pub(crate) struct QsyCtx {
    pub bus: MessageBus,
    pub operating_freq_hz: Arc<AtomicU64>,
    pub last_freq_command: Arc<std::sync::Mutex<Option<(u64, Instant)>>>,
    pub split_tx_hz: Arc<AtomicU64>,
    pub tui_msg_tx: crossbeam_channel::Sender<TuiMessage>,
    pub operator_watch: watch::Receiver<Option<OperatorHandle>>,
}

impl QsyCtx {
    fn status(&self, text: String) {
        let _ = self.tui_msg_tx.send(TuiMessage::StatusUpdate {
            component: "TX".to_string(),
            status: text,
        });
    }
}

/// Command the rig to `frequency`: store the dial, stamp the C9 anchor, send
/// `BandChanged` and clear split on a band change, then forward
/// `RigControlMessage::SetFrequency`. Returns the previous dial only if the
/// retune was enqueued. A refused enqueue restores the dial and C9 anchor;
/// the preceding band teardown and split clear are not undone. Never locks
/// the autonomous operator, so it is safe under `ptt_sync_gate`.
pub(crate) async fn qsy_send(
    ctx: &QsyCtx,
    source: ComponentId,
    vfo: u8,
    frequency: u64,
) -> Option<u64> {
    // C9 — band change mid-QSO: tear down active QSOs so no
    // stale keep-call keeps TXing on the new band. Capture the
    // *old* dial frequency BEFORE we overwrite the atomic, then
    // decide whether this dial move is a genuine band change.
    let old_freq_hz = ctx.operating_freq_hz.load(Ordering::Relaxed);
    ctx.operating_freq_hz.store(frequency, Ordering::Relaxed);
    // Stamp the C9 dedup anchor: pancetta commanded this
    // freq, so the hamlib poll loop suppresses its own
    // teardown (here and during the rig settle window).
    let previous_anchor = if let Ok(mut anchor) = ctx.last_freq_command.lock() {
        anchor.replace((frequency, Instant::now()))
    } else {
        None
    };
    if super::is_band_change(old_freq_hz, frequency) {
        info!(
            target: "operator.override",
            "Band change {} Hz -> {} Hz — tearing down active QSOs",
            old_freq_hz, frequency
        );
        let teardown = ComponentMessage::new(
            source,
            ComponentId::Qso,
            MessageType::QsoMessage(QsoMessage::BandChanged {
                previous_hz: old_freq_hz,
                new_hz: frequency,
            }),
            Instant::now(),
        );
        if let Err(e) = ctx.bus.send_message(teardown).await {
            warn!("Band change: failed to send teardown: {}", e);
        }
        // A band change invalidates any split TX freq.
        if ctx.split_tx_hz.load(Ordering::Relaxed) != 0 {
            let store =
                super::forward_split_change(&ctx.bus, source, &ctx.split_tx_hz, false, 0).await;
            // Push authoritative split state to the TUI chip.
            let _ = ctx
                .tui_msg_tx
                .send(TuiMessage::SplitUpdate { tx_hz: store });
        }
    }
    // Forward to hamlib if available
    let msg = ComponentMessage::new(
        source,
        ComponentId::Hamlib,
        MessageType::RigControl(RigControlMessage::SetFrequency { vfo, frequency }),
        Instant::now(),
    );
    let delivered = ctx.bus.send_message_checked(msg).await.unwrap_or_else(|e| {
        debug!("Failed to forward SetFrequency to hamlib: {}", e);
        false
    });
    if !delivered {
        ctx.operating_freq_hz.store(old_freq_hz, Ordering::Relaxed);
        if let Ok(mut anchor) = ctx.last_freq_command.lock() {
            *anchor = previous_anchor;
        }
        return None;
    }
    // A13 (review L4-24): no SetMode here. With `[rig] data_mode` set, the
    // rigctld backend reapplies the configured mode after every commanded
    // frequency change (PAN-276), so this QSY sets the mode too.
    Some(old_freq_hz)
}

/// Clear the allocator's old-dial data (spectral snapshot, decode history,
/// CQ offset). A no-op while no operator is published. Locks the operator:
/// never call it while holding `ptt_sync_gate`.
pub(crate) async fn invalidate_allocator_band_state(ctx: &QsyCtx) {
    let operator = ctx.operator_watch.borrow().clone();
    if let Some(operator) = operator {
        operator.lock().await.invalidate_band_scoped_state();
    }
}

/// The operator's QSY (band keys, Shift+F): [`qsy_send`], then
/// [`invalidate_allocator_band_state`] when the dial moved. Sibling of the
/// autonomous `ChangeBand` block in `coordinator/autonomous.rs`, which
/// invalidates through the hop itself and also relays split to the gateway.
pub(crate) async fn qsy_to(ctx: &QsyCtx, source: ComponentId, vfo: u8, frequency: u64) -> u64 {
    let Some(old) = qsy_send(ctx, source, vfo, frequency).await else {
        ctx.status("Can't QSY — rig command was not delivered, try again".to_string());
        return ctx.operating_freq_hz.load(Ordering::Relaxed);
    };
    if old != frequency {
        invalidate_allocator_band_state(ctx).await;
    }
    old
}

/// The allocator's TX offset for a spot call with no usable DX offset,
/// avoiding every active QSO's offset. `None` while no operator is published.
pub(crate) async fn resolve_allocator_offset(
    operator_watch: &watch::Receiver<Option<OperatorHandle>>,
    active_offsets: &std::sync::RwLock<HashMap<String, f64>>,
) -> Option<f64> {
    let operator = operator_watch.borrow().clone()?;
    let avoid: Vec<f64> = active_offsets
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .copied()
        .collect();
    let op = operator.lock().await;
    Some(op.allocate_smart_frequency_avoiding(None, None, None, &avoid))
}

/// A resolved network-spot call, as the TUI sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpotCall {
    pub callsign: String,
    pub band: String,
    pub dial_hz: u64,
    pub audio_offset_hz: Option<u64>,
}

/// Everything a spot call reads and the parked-call state.
#[derive(Clone)]
pub(crate) struct SpotCallCtx {
    pub qsy: QsyCtx,
    pub ptt_active: Arc<AtomicBool>,
    pub tx_policy: Arc<AtomicU8>,
    pub our_callsign: String,
    pub qso_manager_watch: watch::Receiver<Option<pancetta_qso::QsoManager>>,
    pub active_tx_offsets: Arc<std::sync::RwLock<HashMap<String, f64>>>,
    pub ptt_sync_gate: Arc<tokio::sync::Mutex<()>>,
    /// Held by the relay's `SetFrequency` arm and by spot-call execution, so
    /// a band key cannot interleave between a QSY and its `StartQso`, and
    /// across the relay's operating-mode switch, so a parked call cannot run
    /// between that switch and its cancel.
    pub spot_qsy_lock: Arc<tokio::sync::Mutex<()>>,
    /// Bumped by every new spot call, every operator frequency change and
    /// every successful operating-mode switch; a parked call whose
    /// generation is stale exits quietly.
    pub generation: Arc<AtomicU64>,
    /// Callsign of the parked call, if any, so a cancel line can name it.
    pub parked_call: Arc<std::sync::Mutex<Option<String>>>,
    /// How long a parked call waits for PTT-off: [`SPOT_QSY_WAIT_CAP`] in
    /// production; tests shorten it (the crate has no tokio `test-util`).
    pub wait_cap: Duration,
}

impl SpotCallCtx {
    fn keyed(&self) -> bool {
        rig_is_keyed(&self.ptt_active, &self.qsy.bus)
    }

    fn take_parked(&self) -> Option<String> {
        self.parked_call
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    async fn admit(&self, call: &SpotCall, keyed: bool) -> SpotAdmission {
        let needs_qsy = !same_dial(
            self.qsy.operating_freq_hz.load(Ordering::Relaxed),
            call.dial_hz,
        );
        let exchanges = if needs_qsy {
            let manager = self.qso_manager_watch.borrow().clone();
            exchange_in_progress(manager).await
        } else {
            Vec::new()
        };
        admit_spot_call(&AdmissionInputs {
            callsign: &call.callsign,
            band: &call.band,
            dial_hz: call.dial_hz,
            own_call: pancetta_qso::exchange::callsigns_match(&call.callsign, &self.our_callsign),
            policy: pancetta_core::TxPolicy::from_u8(self.tx_policy.load(Ordering::Acquire)),
            has_rig_route: self.qsy.bus.has_rig_route(),
            needs_qsy,
            keyed,
            exchanges: &exchanges,
            offset_known: call.audio_offset_hz.is_some(),
            allocator_available: self.qsy.operator_watch.borrow().is_some(),
        })
    }
}

/// Outcome of one execution attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecOutcome {
    /// Finished: called, refused, or superseded.
    Done,
    /// The rig keyed again under `ptt_sync_gate`; keep waiting.
    Defer,
}

/// Entry point for `TuiCommand::CallNetworkSpot`. A newer spot call always
/// supersedes a parked one.
pub(crate) async fn handle_spot_call(ctx: &SpotCallCtx, call: SpotCall) {
    {
        let _lock = ctx.spot_qsy_lock.lock().await;
        ctx.generation.fetch_add(1, Ordering::SeqCst);
        ctx.take_parked();
    }
    if pancetta_qso::exchange::callsigns_match(&call.callsign, &ctx.our_callsign) {
        warn!(
            target: "qso.security",
            "Refusing to call our own station {}", call.callsign
        );
        crate::coordinator::tx::emit_diagnostic_full(
            &ctx.qsy.bus,
            ComponentId::Qso,
            "qso.security",
            pancetta_core::DiagnosticLevel::Warn,
            format!("Refusing to call our own station {}", call.callsign),
            None,
            Some(&call.callsign),
        )
        .await;
    }
    match ctx.admit(&call, ctx.keyed()).await {
        SpotAdmission::Refuse(line) => {
            info!(target: "tx.policy", "Spot call refused: {}", line);
            ctx.qsy.status(line);
        }
        SpotAdmission::Defer => park(ctx, call).await,
        SpotAdmission::Proceed { .. } => {
            if execute_spot_call(ctx, call.clone(), None).await == ExecOutcome::Defer {
                park(ctx, call).await;
            }
        }
    }
}

/// Park `call` until PTT-off (latest wins) and spawn its waiter.
async fn park(ctx: &SpotCallCtx, call: SpotCall) {
    let (gen, dial_at_park) = {
        let _lock = ctx.spot_qsy_lock.lock().await;
        let gen = ctx.generation.fetch_add(1, Ordering::SeqCst) + 1;
        *ctx.parked_call.lock().unwrap_or_else(|e| e.into_inner()) = Some(call.callsign.clone());
        (gen, ctx.qsy.operating_freq_hz.load(Ordering::Relaxed))
    };
    info!(target: "operator.override", "Spot call to {} parked until PTT-off", call.callsign);
    ctx.qsy.status(format!(
        "Queued: QSY to {} {:.3} MHz after this frame, then call {}",
        call.band,
        call.dial_hz as f64 / 1e6,
        call.callsign
    ));
    tokio::spawn(wait_then_execute(ctx.clone(), call, gen, dial_at_park));
}

/// QSY (when needed) and open the call. `parked` is the parked call's
/// generation and the dial it was parked on; a stale generation returns
/// quietly, and a dial that moved while parked (an autonomous hop, a remote
/// or front-panel retune) cancels the call rather than undo that move.
async fn execute_spot_call(
    ctx: &SpotCallCtx,
    call: SpotCall,
    parked: Option<(u64, u64)>,
) -> ExecOutcome {
    let _lock = ctx.spot_qsy_lock.lock().await;
    let expected_gen = parked.map(|(gen, _)| gen);
    if let Some((gen, dial_at_park)) = parked {
        if ctx.generation.load(Ordering::SeqCst) != gen {
            return ExecOutcome::Done;
        }
        let now = ctx.qsy.operating_freq_hz.load(Ordering::Relaxed);
        if now.abs_diff(dial_at_park) > SAME_DIAL_SLACK_HZ {
            ctx.take_parked();
            info!(
                target: "operator.override",
                "Parked spot call to {} cancelled: dial moved {} -> {} Hz while parked",
                call.callsign, dial_at_park, now
            );
            ctx.qsy.status(format!(
                "Queued call to {} cancelled — frequency changed",
                call.callsign
            ));
            return ExecOutcome::Done;
        }
    }
    // Re-admit: policy, rig route, exchanges and allocator may have changed
    // while parked. The keyed state is rechecked under `ptt_sync_gate` below.
    let qsy = match ctx.admit(&call, false).await {
        SpotAdmission::Refuse(line) => {
            if expected_gen.is_some() {
                ctx.take_parked();
            }
            info!(target: "tx.policy", "Spot call refused: {}", line);
            ctx.qsy.status(line);
            return ExecOutcome::Done;
        }
        SpotAdmission::Proceed { qsy } => qsy,
        // `keyed == false` above, so admission cannot defer.
        SpotAdmission::Defer => true,
    };
    if qsy {
        {
            // Every key-down passes this gate before its PTT-on enqueue
            // (`PttGuard::new` stores `ptt_active` under it;
            // `tui_relay::manual_key_down` holds it across the enqueue), so no
            // PTT-on can be enqueued between this recheck and `SetFrequency`.
            // Never read the QSO manager here: `apply_tx_offset_switch` takes
            // its `qsos` lock before this gate.
            let _gate = ctx.ptt_sync_gate.lock().await;
            if ctx.keyed() {
                return ExecOutcome::Defer;
            }
            if qsy_send(&ctx.qsy, ComponentId::Tui, 0, call.dial_hz)
                .await
                .is_none()
            {
                if expected_gen.is_some() {
                    ctx.take_parked();
                }
                ctx.qsy.status(format!(
                    "Can't call {} — rig command was not delivered, try again",
                    call.callsign
                ));
                return ExecOutcome::Done;
            }
        }
        // The operator lock comes only after `ptt_sync_gate` is released.
        invalidate_allocator_band_state(&ctx.qsy).await;
        let _ = ctx.qsy.tui_msg_tx.send(TuiMessage::SpotQsy {
            dial_hz: call.dial_hz,
        });
        info!(
            target: "operator.override",
            "Spot QSY to {} Hz for {}", call.dial_hz, call.callsign
        );
        ctx.qsy.status(format!(
            "QSY {} {:.3} MHz — calling {}",
            call.band,
            call.dial_hz as f64 / 1e6,
            call.callsign
        ));
    }
    if expected_gen.is_some() {
        ctx.take_parked();
    }
    let (frequency, dx_offset_unknown) = match call.audio_offset_hz {
        Some(offset) => (offset, false),
        None => {
            match resolve_allocator_offset(&ctx.qsy.operator_watch, &ctx.active_tx_offsets).await {
                Some(hz) => (hz.round() as u64, true),
                None => {
                    // The QSY stands: the operator asked to go there.
                    ctx.qsy.status(allocator_not_ready(&call.callsign));
                    return ExecOutcome::Done;
                }
            }
        }
    };
    info!(
        "Spot call: {} at {} Hz (dx_offset_unknown={})",
        call.callsign, frequency, dx_offset_unknown
    );
    let msg = ComponentMessage::new(
        ComponentId::Tui,
        ComponentId::Qso,
        MessageType::QsoMessage(QsoMessage::StartQso {
            callsign: call.callsign,
            frequency,
            dx_parity: None,
            remote_origin: false,
            remote_client_key_id: None,
            dx_offset_unknown,
        }),
        Instant::now(),
    );
    if let Err(e) = ctx.qsy.bus.send_message(msg).await {
        warn!("Failed to forward spot call: {}", e);
    }
    ExecOutcome::Done
}

/// Poll for PTT-off every [`SPOT_QSY_POLL`], then execute; give up after
/// [`SPOT_QSY_WAIT_CAP`] or as soon as the generation moves on.
async fn wait_then_execute(ctx: SpotCallCtx, call: SpotCall, gen: u64, dial_at_park: u64) {
    let deadline = tokio::time::Instant::now() + ctx.wait_cap;
    loop {
        tokio::time::sleep(SPOT_QSY_POLL).await;
        if ctx.generation.load(Ordering::SeqCst) != gen {
            return;
        }
        if !ctx.keyed()
            && execute_spot_call(&ctx, call.clone(), Some((gen, dial_at_park))).await
                == ExecOutcome::Done
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            let _lock = ctx.spot_qsy_lock.lock().await;
            if ctx.generation.load(Ordering::SeqCst) == gen {
                ctx.take_parked();
                warn!(
                    target: "operator.override",
                    "Dropping parked spot call to {}: still keyed after {:?}",
                    call.callsign, ctx.wait_cap
                );
                ctx.qsy.status(format!(
                    "Dropped queued call to {} — still transmitting after {} s",
                    call.callsign,
                    SPOT_QSY_WAIT_CAP.as_secs()
                ));
            }
            return;
        }
    }
}

/// Any operator frequency change (band key, Shift+F) cancels a parked spot
/// call. Call with `spot_qsy_lock` held.
pub(crate) fn note_operator_frequency_change(ctx: &SpotCallCtx) {
    ctx.generation.fetch_add(1, Ordering::SeqCst);
    if let Some(call) = ctx.take_parked() {
        ctx.qsy.status(format!(
            "Queued call to {call} cancelled — frequency changed"
        ));
    }
}

/// Run the operating-mode switch `switch` (Shift+M) under `spot_qsy_lock`,
/// so a parked spot call cannot execute between the switch and its cancel.
/// When the switch succeeds, a parked call is cancelled: the TUI resolved its
/// dial for the old mode, so running it would retune to that dial and call
/// there in the new mode. A refused switch leaves the parked call alone.
/// Returns the cancelled call's callsign; report it with
/// [`report_mode_change_cancel`] after the switch's own status lines.
pub(crate) async fn switch_mode_cancelling_parked<E>(
    ctx: &SpotCallCtx,
    switch: impl FnOnce() -> Result<(), E>,
) -> Result<Option<String>, E> {
    let _lock = ctx.spot_qsy_lock.lock().await;
    switch()?;
    ctx.generation.fetch_add(1, Ordering::SeqCst);
    let cancelled = ctx.take_parked();
    if let Some(call) = &cancelled {
        info!(
            target: "operator.override",
            "Parked spot call to {} cancelled: operating mode changed", call
        );
    }
    Ok(cancelled)
}

/// Status line for a parked spot call a mode switch cancelled.
pub(crate) fn report_mode_change_cancel(ctx: &SpotCallCtx, call: &str) {
    ctx.qsy.status(format!(
        "Queued call to {call} cancelled — operating mode changed"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::Receiver;

    const CALL: &str = "ZL1ABC";

    struct Harness {
        ctx: SpotCallCtx,
        tui_rx: crossbeam_channel::Receiver<TuiMessage>,
        qso_rx: Receiver<ComponentMessage>,
        hamlib_rx: Option<Receiver<ComponentMessage>>,
        op_tx: watch::Sender<Option<OperatorHandle>>,
        _qso_tx: watch::Sender<Option<pancetta_qso::QsoManager>>,
    }

    async fn harness(
        rig: bool,
        dial_hz: u64,
        manager: Option<pancetta_qso::QsoManager>,
    ) -> Harness {
        let bus = MessageBus::new(64).unwrap();
        let (_, qso_rx) = bus.create_channel(ComponentId::Qso).await.unwrap();
        let hamlib_rx = if rig {
            Some(bus.create_channel(ComponentId::Hamlib).await.unwrap().1)
        } else {
            None
        };
        let (tui_tx, tui_rx) = crossbeam_channel::unbounded();
        let (op_tx, op_rx) = watch::channel(None);
        let (qso_tx, qso_watch) = watch::channel(manager);
        let ctx = SpotCallCtx {
            qsy: QsyCtx {
                bus,
                operating_freq_hz: Arc::new(AtomicU64::new(dial_hz)),
                last_freq_command: Arc::new(std::sync::Mutex::new(None)),
                split_tx_hz: Arc::new(AtomicU64::new(0)),
                tui_msg_tx: tui_tx,
                operator_watch: op_rx,
            },
            ptt_active: Arc::new(AtomicBool::new(false)),
            tx_policy: Arc::new(AtomicU8::new(pancetta_core::TxPolicy::Full.as_u8())),
            our_callsign: "W1AW".into(),
            qso_manager_watch: qso_watch,
            active_tx_offsets: Arc::new(std::sync::RwLock::new(HashMap::new())),
            ptt_sync_gate: Arc::new(tokio::sync::Mutex::new(())),
            spot_qsy_lock: Arc::new(tokio::sync::Mutex::new(())),
            generation: Arc::new(AtomicU64::new(0)),
            parked_call: Arc::new(std::sync::Mutex::new(None)),
            wait_cap: Duration::from_millis(600),
        };
        Harness {
            ctx,
            tui_rx,
            qso_rx,
            hamlib_rx,
            op_tx,
            _qso_tx: qso_tx,
        }
    }

    /// A Hold-mode operator parked at `parked_hz`.
    fn hold_operator(parked_hz: u64) -> OperatorHandle {
        let mut op = pancetta_qso::AutonomousOperator::new(
            pancetta_qso::AutonomousConfig::default(),
            "W1AW".into(),
            Some("FN31".into()),
        );
        op.set_tx_freq_mode_source(Arc::new(AtomicU8::new(
            pancetta_core::TxFreqMode::Hold.as_u8(),
        )));
        op.set_tx_offset_hold_source(Arc::new(AtomicU64::new(parked_hz)));
        Arc::new(tokio::sync::Mutex::new(op))
    }

    fn spot(offset: Option<u64>) -> SpotCall {
        SpotCall {
            callsign: CALL.into(),
            band: "20M".into(),
            dial_hz: 14_074_000,
            audio_offset_hz: offset,
        }
    }

    fn statuses(h: &Harness) -> Vec<String> {
        h.tui_rx
            .try_iter()
            .filter_map(|m| match m {
                TuiMessage::StatusUpdate { status, .. } => Some(status),
                _ => None,
            })
            .collect()
    }

    fn drain(rx: &Receiver<ComponentMessage>) -> Vec<ComponentMessage> {
        rx.try_iter().collect()
    }

    fn set_freqs(msgs: &[ComponentMessage]) -> Vec<u64> {
        msgs.iter()
            .filter_map(|m| match m.message_type {
                MessageType::RigControl(RigControlMessage::SetFrequency { frequency, .. }) => {
                    Some(frequency)
                }
                _ => None,
            })
            .collect()
    }

    /// `(frequency, dx_offset_unknown)` of every StartQso.
    fn start_qsos(msgs: &[ComponentMessage]) -> Vec<(u64, bool)> {
        msgs.iter()
            .filter_map(|m| match &m.message_type {
                MessageType::QsoMessage(QsoMessage::StartQso {
                    frequency,
                    dx_offset_unknown,
                    dx_parity: None,
                    remote_origin: false,
                    ..
                }) => Some((*frequency, *dx_offset_unknown)),
                _ => None,
            })
            .collect()
    }

    fn is_band_changed(m: &ComponentMessage) -> bool {
        matches!(
            m.message_type,
            MessageType::QsoMessage(QsoMessage::BandChanged { .. })
        )
    }

    // ---- rig_is_keyed -------------------------------------------------

    #[test]
    fn rig_is_keyed_reads_ptt_and_key_down_bits_not_receive_gain() {
        let bus = MessageBus::new(8).unwrap();
        let ptt = AtomicBool::new(false);
        assert!(!rig_is_keyed(&ptt, &bus));
        ptt.store(true, Ordering::Release);
        assert!(rig_is_keyed(&ptt, &bus));
        ptt.store(false, Ordering::Release);
        bus.gain_tx_gate.store(1, Ordering::Release);
        assert!(
            !rig_is_keyed(&ptt, &bus),
            "bit 0 is the receive-gain transition"
        );
        bus.gain_tx_gate.store(2, Ordering::Release);
        assert!(rig_is_keyed(&ptt, &bus), "bit 1 is manual PTT");
        bus.gain_tx_gate.store(4, Ordering::Release);
        assert!(
            rig_is_keyed(&ptt, &bus),
            "bits 2+ are key-downs mid-enqueue"
        );
    }

    // ---- exchange_in_progress -----------------------------------------

    fn manager() -> pancetta_qso::QsoManager {
        pancetta_qso::QsoManager::new(pancetta_qso::QsoManagerConfig {
            our_callsign: "W1AW".into(),
            ..Default::default()
        })
    }

    async fn open_exchange(m: &pancetta_qso::QsoManager, call: &str, hz: f64) {
        m.respond_to_caller(
            call.into(),
            hz,
            None,
            pancetta_core::ResponseStep::Report,
            Some(-10.0),
            None,
            None,
            false,
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn exchange_in_progress_ignores_cq_and_unanswered_calls() {
        let m = manager();
        assert!(exchange_in_progress(None).await.is_empty());
        m.start_cq(1500.0, None, false, None).await.unwrap();
        m.respond_to_cq_manual("K2AAA".into(), 900.0, None)
            .await
            .unwrap();
        assert!(exchange_in_progress(Some(m)).await.is_empty());
    }

    #[tokio::test]
    async fn exchange_in_progress_names_each_partner_that_answered() {
        let m = manager();
        open_exchange(&m, "K1XYZ", 1200.0).await;
        assert_eq!(exchange_in_progress(Some(m.clone())).await, vec!["K1XYZ"]);
        open_exchange(&m, "K3QRP", 1800.0).await;
        let mut both = exchange_in_progress(Some(m.clone())).await;
        both.sort();
        assert_eq!(both, vec!["K1XYZ", "K3QRP"]);
        // Terminal QSOs are ignored.
        for (id, _) in m.get_active_qsos().await {
            m.cancel_qso(id).await.unwrap();
        }
        assert!(exchange_in_progress(Some(m)).await.is_empty());
    }

    /// Validate finding: a QSO that completed but whose closing 73 has not
    /// aired (PAN-280) is `Completed`, yet a QSY would air that 73 on the
    /// new band. It must block a dial-changing spot call.
    #[tokio::test]
    async fn exchange_in_progress_names_partner_whose_close_has_not_aired() {
        let m = manager();
        m.respond_to_caller(
            "K1XYZ".into(),
            1200.0,
            None,
            pancetta_core::ResponseStep::SeventyThree,
            Some(-10.0),
            None,
            None,
            false,
            None,
        )
        .await
        .unwrap();
        assert!(
            m.get_active_qsos().await.is_empty(),
            "the QSO is Completed, waiting on its 73"
        );
        let exchanges = exchange_in_progress(Some(m.clone())).await;
        assert_eq!(exchanges, vec!["K1XYZ"]);
        let refusal = admit_spot_call(&inputs(&exchanges));
        assert!(
            matches!(refusal, SpotAdmission::Refuse(_)),
            "a pending close refuses the QSY: {refusal:?}"
        );
    }

    // ---- admit_spot_call ----------------------------------------------

    fn inputs<'a>(exchanges: &'a [String]) -> AdmissionInputs<'a> {
        AdmissionInputs {
            callsign: CALL,
            band: "20M",
            dial_hz: 14_074_000,
            own_call: false,
            policy: pancetta_core::TxPolicy::Full,
            has_rig_route: true,
            needs_qsy: true,
            keyed: false,
            exchanges,
            offset_known: true,
            allocator_available: true,
        }
    }

    #[test]
    fn admit_spot_call_table() {
        let none: Vec<String> = Vec::new();
        let one = vec!["K1XYZ".to_string()];
        let two = vec!["K1XYZ".to_string(), "K3QRP".to_string()];
        let refuse = |s: &str| SpotAdmission::Refuse(s.to_string());

        let mut i = inputs(&none);
        i.own_call = true;
        assert_eq!(
            admit_spot_call(&i),
            refuse("Refusing to call our own station (ZL1ABC)")
        );

        let mut i = inputs(&none);
        i.policy = pancetta_core::TxPolicy::RespondOnly;
        assert_eq!(
            admit_spot_call(&i),
            refuse("Can't call ZL1ABC — TX policy is RESPOND-ONLY (press g for Full)")
        );

        let mut i = inputs(&none);
        i.has_rig_route = false;
        assert_eq!(
            admit_spot_call(&i),
            refuse("Can't QSY to 20M for ZL1ABC — no rig control (tune to 14.074 MHz, then press Space)")
        );

        assert_eq!(
            admit_spot_call(&inputs(&one)),
            refuse("Can't QSY to 20M for ZL1ABC — QSO with K1XYZ in progress (finish it or abort with k)")
        );
        assert_eq!(
            admit_spot_call(&inputs(&two)),
            refuse(
                "Can't QSY to 20M for ZL1ABC — 2 QSOs in progress (finish or abort them with k)"
            )
        );

        let mut i = inputs(&none);
        i.offset_known = false;
        i.allocator_available = false;
        assert_eq!(
            admit_spot_call(&i),
            refuse("Can't call ZL1ABC — TX offset allocator not ready, try again")
        );

        let mut i = inputs(&none);
        i.keyed = true;
        assert_eq!(admit_spot_call(&i), SpotAdmission::Defer);

        // No QSY means no wait, and an exchange does not block.
        let mut i = inputs(&one);
        i.needs_qsy = false;
        i.keyed = true;
        assert_eq!(admit_spot_call(&i), SpotAdmission::Proceed { qsy: false });

        let mut i = inputs(&none);
        i.needs_qsy = false;
        i.has_rig_route = false;
        assert_eq!(admit_spot_call(&i), SpotAdmission::Proceed { qsy: false });

        assert_eq!(
            admit_spot_call(&inputs(&none)),
            SpotAdmission::Proceed { qsy: true }
        );
    }

    #[test]
    fn same_dial_allows_readback_rounding_and_treats_zero_as_unknown() {
        assert!(same_dial(14_074_000, 14_074_000));
        assert!(same_dial(14_074_010, 14_074_000));
        assert!(!same_dial(14_074_011, 14_074_000));
        assert!(!same_dial(7_074_000, 14_074_000));
        assert!(!same_dial(0, 14_074_000));
    }

    // ---- qsy_to -------------------------------------------------------

    #[tokio::test]
    async fn qsy_to_cross_band_sends_band_changed_before_set_frequency_and_invalidates() {
        let h = harness(true, 7_074_000, None).await;
        h.ctx.qsy.split_tx_hz.store(7_076_000, Ordering::Release);
        let op = hold_operator(1600);
        op.lock()
            .await
            .update_spectral(pancetta_qso::frequency::SpectralSnapshot {
                power_bins: vec![0.0f32; 140],
                freq_min_hz: 200.0,
                freq_max_hz: 3000.0,
            });
        assert!(op.lock().await.placement_snapshot(10).is_some());
        h.op_tx.send_replace(Some(op.clone()));

        let old = qsy_to(&h.ctx.qsy, ComponentId::Tui, 0, 14_074_000).await;
        assert_eq!(old, 7_074_000);
        assert_eq!(
            h.ctx.qsy.operating_freq_hz.load(Ordering::Acquire),
            14_074_000
        );
        assert_eq!(
            h.ctx.qsy.last_freq_command.lock().unwrap().map(|(f, _)| f),
            Some(14_074_000)
        );
        let qso = drain(&h.qso_rx);
        let rig = drain(h.hamlib_rx.as_ref().unwrap());
        let band_changed = qso
            .iter()
            .find(|m| is_band_changed(m))
            .expect("BandChanged");
        let set_freq = rig
            .iter()
            .find(|m| {
                matches!(
                    m.message_type,
                    MessageType::RigControl(RigControlMessage::SetFrequency { .. })
                )
            })
            .expect("SetFrequency");
        assert!(band_changed.timestamp <= set_freq.timestamp);
        assert_eq!(
            h.ctx.qsy.split_tx_hz.load(Ordering::Acquire),
            0,
            "split cleared"
        );
        assert!(
            op.lock().await.placement_snapshot(10).is_none(),
            "invalidated"
        );
    }

    #[tokio::test]
    async fn qsy_to_same_band_move_invalidates_without_band_changed() {
        let h = harness(true, 14_074_000, None).await;
        let op = hold_operator(1600);
        op.lock()
            .await
            .update_spectral(pancetta_qso::frequency::SpectralSnapshot {
                power_bins: vec![0.0f32; 140],
                freq_min_hz: 200.0,
                freq_max_hz: 3000.0,
            });
        h.op_tx.send_replace(Some(op.clone()));
        qsy_to(&h.ctx.qsy, ComponentId::Tui, 0, 14_080_000).await;
        assert!(!drain(&h.qso_rx).iter().any(is_band_changed));
        assert_eq!(
            set_freqs(&drain(h.hamlib_rx.as_ref().unwrap())),
            vec![14_080_000]
        );
        assert!(op.lock().await.placement_snapshot(10).is_none());
    }

    #[tokio::test]
    async fn resolve_allocator_offset_uses_the_published_operator() {
        let h = harness(true, 14_074_000, None).await;
        assert_eq!(
            resolve_allocator_offset(&h.ctx.qsy.operator_watch, &h.ctx.active_tx_offsets).await,
            None
        );
        h.op_tx.send_replace(Some(hold_operator(1600)));
        assert_eq!(
            resolve_allocator_offset(&h.ctx.qsy.operator_watch, &h.ctx.active_tx_offsets).await,
            Some(1600.0)
        );
    }

    // ---- Gherkin scenarios --------------------------------------------

    /// Scenario 1: Space on a network spot moves the rig, then calls at the
    /// spot's offset.
    #[tokio::test]
    async fn scenario_1_spot_on_another_band_qsys_then_calls_at_spot_offset() {
        let h = harness(true, 7_074_000, None).await;
        handle_spot_call(&h.ctx, spot(Some(1210))).await;
        let qso = drain(&h.qso_rx);
        assert!(
            is_band_changed(&qso[0]),
            "BandChanged reaches the QSO channel first"
        );
        assert_eq!(start_qsos(&qso), vec![(1210, false)]);
        assert_eq!(
            set_freqs(&drain(h.hamlib_rx.as_ref().unwrap())),
            vec![14_074_000]
        );
        let tui: Vec<TuiMessage> = h.tui_rx.try_iter().collect();
        assert!(tui.iter().any(|m| matches!(
            m,
            TuiMessage::SpotQsy {
                dial_hz: 14_074_000
            }
        )));
        assert!(tui.iter().any(|m| matches!(m,
            TuiMessage::StatusUpdate { status, .. } if status == "QSY 20M 14.074 MHz — calling ZL1ABC")));
    }

    #[tokio::test]
    async fn spot_on_current_dial_calls_without_qsy() {
        let h = harness(true, 14_074_000, None).await;
        handle_spot_call(&h.ctx, spot(Some(1210))).await;
        assert_eq!(start_qsos(&drain(&h.qso_rx)), vec![(1210, false)]);
        assert!(set_freqs(&drain(h.hamlib_rx.as_ref().unwrap())).is_empty());
        assert!(statuses(&h).is_empty());
    }

    /// Scenario 2: a spot with no offset QSYs and takes the allocator's pick.
    #[tokio::test]
    async fn scenario_2_dial_only_spot_calls_at_allocator_offset() {
        let h = harness(true, 7_074_000, None).await;
        h.op_tx.send_replace(Some(hold_operator(1600)));
        handle_spot_call(&h.ctx, spot(None)).await;
        assert_eq!(
            set_freqs(&drain(h.hamlib_rx.as_ref().unwrap())),
            vec![14_074_000]
        );
        assert_eq!(start_qsos(&drain(&h.qso_rx)), vec![(1600, true)]);
    }

    #[tokio::test]
    async fn full_rig_channel_refuses_spot_call_and_allows_retry() {
        let h = harness(true, 7_074_000, None).await;
        let filler = || {
            ComponentMessage::new(
                ComponentId::Tui,
                ComponentId::Hamlib,
                MessageType::RigControl(RigControlMessage::SetFrequency {
                    vfo: 0,
                    frequency: 7_074_000,
                }),
                Instant::now(),
            )
        };
        while h.ctx.qsy.bus.send_message_checked(filler()).await.unwrap() {}
        handle_spot_call(&h.ctx, spot(Some(1210))).await;
        assert!(
            start_qsos(&drain(&h.qso_rx)).is_empty(),
            "no call without retune delivery"
        );
        assert_eq!(
            h.ctx.qsy.operating_freq_hz.load(Ordering::Acquire),
            7_074_000
        );
        assert!(h.ctx.qsy.last_freq_command.lock().unwrap().is_none());
        let tui: Vec<_> = h.tui_rx.try_iter().collect();
        assert!(!tui.iter().any(|m| matches!(m, TuiMessage::SpotQsy { .. })));
        assert!(tui
            .iter()
            .any(|m| matches!(m, TuiMessage::StatusUpdate { status, .. }
            if status.contains("rig command was not delivered"))));
        drain(h.hamlib_rx.as_ref().unwrap());
        handle_spot_call(&h.ctx, spot(Some(1210))).await;
        assert_eq!(
            set_freqs(&drain(h.hamlib_rx.as_ref().unwrap())),
            vec![14_074_000]
        );
        assert_eq!(start_qsos(&drain(&h.qso_rx)), vec![(1210, false)]);
    }

    #[tokio::test]
    async fn no_rig_route_refuses_and_sends_nothing() {
        let h = harness(false, 7_074_000, None).await;
        handle_spot_call(&h.ctx, spot(Some(1210))).await;
        assert!(drain(&h.qso_rx).is_empty());
        assert_eq!(
            statuses(&h),
            vec!["Can't QSY to 20M for ZL1ABC — no rig control (tune to 14.074 MHz, then press Space)"]
        );
    }

    /// Scenario 3: a frame is airing, so the call is queued and the QSY
    /// happens after the frame.
    #[tokio::test]
    async fn scenario_3_keyed_parks_the_call_and_qsys_after_ptt_off() {
        let h = harness(true, 7_074_000, None).await;
        h.ctx.ptt_active.store(true, Ordering::Release);
        handle_spot_call(&h.ctx, spot(Some(1210))).await;
        assert_eq!(
            statuses(&h),
            vec!["Queued: QSY to 20M 14.074 MHz after this frame, then call ZL1ABC"]
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(drain(&h.qso_rx).is_empty(), "nothing while keyed");
        assert!(drain(h.hamlib_rx.as_ref().unwrap()).is_empty());
        h.ctx.ptt_active.store(false, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let qso = drain(&h.qso_rx);
        assert!(is_band_changed(&qso[0]));
        assert_eq!(start_qsos(&qso), vec![(1210, false)]);
        assert_eq!(
            set_freqs(&drain(h.hamlib_rx.as_ref().unwrap())),
            vec![14_074_000]
        );
        assert!(h.ctx.parked_call.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn keyed_with_no_qsy_needed_calls_immediately() {
        let h = harness(true, 14_074_000, None).await;
        h.ctx.ptt_active.store(true, Ordering::Release);
        handle_spot_call(&h.ctx, spot(Some(1210))).await;
        assert_eq!(start_qsos(&drain(&h.qso_rx)), vec![(1210, false)]);
    }

    // ---- deferral -----------------------------------------------------

    #[tokio::test]
    async fn parked_call_times_out_while_still_keyed() {
        let h = harness(true, 7_074_000, None).await;
        h.ctx.ptt_active.store(true, Ordering::Release);
        handle_spot_call(&h.ctx, spot(Some(1210))).await;
        tokio::time::sleep(h.ctx.wait_cap + Duration::from_millis(400)).await;
        assert!(drain(&h.qso_rx).is_empty());
        assert!(drain(h.hamlib_rx.as_ref().unwrap()).is_empty());
        let s = statuses(&h);
        assert_eq!(
            s.last().unwrap(),
            "Dropped queued call to ZL1ABC — still transmitting after 30 s"
        );
        // Unkeying later does nothing.
        h.ctx.ptt_active.store(false, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(drain(&h.qso_rx).is_empty());
    }

    #[tokio::test]
    async fn newer_spot_call_replaces_the_parked_one() {
        let h = harness(true, 7_074_000, None).await;
        h.ctx.ptt_active.store(true, Ordering::Release);
        handle_spot_call(&h.ctx, spot(Some(1210))).await;
        let mut b = spot(Some(900));
        b.callsign = "VK9XX".into();
        handle_spot_call(&h.ctx, b).await;
        h.ctx.ptt_active.store(false, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(400)).await;
        let qso = drain(&h.qso_rx);
        assert_eq!(
            start_qsos(&qso),
            vec![(900, false)],
            "only the newer call runs"
        );
        assert_eq!(
            set_freqs(&drain(h.hamlib_rx.as_ref().unwrap())),
            vec![14_074_000]
        );
    }

    #[tokio::test]
    async fn operator_frequency_change_cancels_the_parked_call() {
        let h = harness(true, 7_074_000, None).await;
        h.ctx.ptt_active.store(true, Ordering::Release);
        handle_spot_call(&h.ctx, spot(Some(1210))).await;
        let _ = statuses(&h);
        {
            let _lock = h.ctx.spot_qsy_lock.lock().await;
            note_operator_frequency_change(&h.ctx);
        }
        assert_eq!(
            statuses(&h),
            vec!["Queued call to ZL1ABC cancelled — frequency changed"]
        );
        h.ctx.ptt_active.store(false, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(drain(&h.qso_rx).is_empty());
        assert!(drain(h.hamlib_rx.as_ref().unwrap()).is_empty());
    }

    /// Shift+M while a call is parked: the call's dial was resolved for the
    /// old mode, so it must not run after the switch.
    #[tokio::test]
    async fn operating_mode_switch_cancels_the_parked_call() {
        let h = harness(true, 7_074_000, None).await;
        h.ctx.ptt_active.store(true, Ordering::Release);
        handle_spot_call(&h.ctx, spot(Some(1210))).await;
        let _ = statuses(&h);
        let switched = switch_mode_cancelling_parked(&h.ctx, || {
            assert!(
                h.ctx.spot_qsy_lock.try_lock().is_err(),
                "the switch runs under spot_qsy_lock"
            );
            Ok::<(), ()>(())
        })
        .await;
        assert_eq!(switched, Ok(Some(CALL.to_string())));
        assert!(statuses(&h).is_empty(), "the caller reports the cancel");
        report_mode_change_cancel(&h.ctx, CALL);
        assert_eq!(
            statuses(&h),
            vec!["Queued call to ZL1ABC cancelled — operating mode changed"]
        );
        h.ctx.ptt_active.store(false, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(drain(&h.qso_rx).is_empty());
        assert!(drain(h.hamlib_rx.as_ref().unwrap()).is_empty());
    }

    /// A refused mode switch (a QSO is active, a lock is busy) leaves the
    /// parked call to run at PTT-off.
    #[tokio::test]
    async fn refused_mode_switch_keeps_the_parked_call() {
        let h = harness(true, 7_074_000, None).await;
        h.ctx.ptt_active.store(true, Ordering::Release);
        handle_spot_call(&h.ctx, spot(Some(1210))).await;
        let switched = switch_mode_cancelling_parked(&h.ctx, || Err::<(), _>("refused")).await;
        assert_eq!(switched, Err("refused"));
        h.ctx.ptt_active.store(false, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            set_freqs(&drain(h.hamlib_rx.as_ref().unwrap())),
            vec![14_074_000]
        );
        assert_eq!(start_qsos(&drain(&h.qso_rx)), vec![(1210, false)]);
    }

    /// A dial move nobody routed through the relay (an autonomous hop, a
    /// remote or front-panel retune) while the call is parked cancels it at
    /// PTT-off instead of undoing that move.
    #[tokio::test]
    async fn dial_moved_while_parked_cancels_the_call() {
        let h = harness(true, 7_074_000, None).await;
        h.ctx.ptt_active.store(true, Ordering::Release);
        handle_spot_call(&h.ctx, spot(Some(1210))).await;
        let _ = statuses(&h);
        h.ctx
            .qsy
            .operating_freq_hz
            .store(3_573_000, Ordering::Relaxed);
        h.ctx.ptt_active.store(false, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            statuses(&h),
            vec!["Queued call to ZL1ABC cancelled — frequency changed"]
        );
        assert!(drain(&h.qso_rx).is_empty());
        assert!(drain(h.hamlib_rx.as_ref().unwrap()).is_empty());
    }

    #[tokio::test]
    async fn policy_change_while_parked_drops_the_call() {
        let h = harness(true, 7_074_000, None).await;
        h.ctx.ptt_active.store(true, Ordering::Release);
        handle_spot_call(&h.ctx, spot(Some(1210))).await;
        let _ = statuses(&h);
        h.ctx
            .tx_policy
            .store(pancetta_core::TxPolicy::Disabled.as_u8(), Ordering::Release);
        h.ctx.ptt_active.store(false, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(drain(&h.qso_rx).is_empty());
        assert!(drain(h.hamlib_rx.as_ref().unwrap()).is_empty());
        assert_eq!(
            statuses(&h),
            vec!["Can't call ZL1ABC — TX policy is DISABLED (press g for Full)"]
        );
    }

    #[tokio::test]
    async fn exchange_started_while_parked_drops_the_call() {
        let m = manager();
        let h = harness(true, 7_074_000, Some(m.clone())).await;
        h.ctx.ptt_active.store(true, Ordering::Release);
        handle_spot_call(&h.ctx, spot(Some(1210))).await;
        let _ = statuses(&h);
        open_exchange(&m, "K1XYZ", 1200.0).await;
        h.ctx.ptt_active.store(false, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(drain(h.hamlib_rx.as_ref().unwrap()).is_empty());
        assert_eq!(
            statuses(&h),
            vec!["Can't QSY to 20M for ZL1ABC — QSO with K1XYZ in progress (finish it or abort with k)"]
        );
    }
}
