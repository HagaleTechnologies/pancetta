use super::*;
use pancetta_core::time::SignedDurationExt as _;
use pancetta_core::time::UtcTimestamp;

/// Read the current global TX policy from the shared atomic.
pub(super) fn current_tx_policy(
    tx_policy: &std::sync::Arc<std::sync::atomic::AtomicU8>,
) -> pancetta_core::TxPolicy {
    pancetta_core::TxPolicy::from_u8(tx_policy.load(Ordering::Acquire))
}

/// PAN-19 round-7 review (Codex P1): `hamlib_loop_ready` is checked as an
/// ADDITIONAL, orthogonal condition alongside `restart_inhibit` -- not a
/// replacement for it. `restart_inhibit` (`tx_restart_inhibit`) reflects
/// the coordinator's restart-supervision bookkeeping (`TxInhibitGuard`),
/// which releases the instant `start_hamlib_component` RETURNS -- even on
/// a `LoopReadyOutcome::TimedOut`, which must stay non-bailing so a slow
/// rig never hard-fails startup (the HIGH-fix invariant). `hamlib_loop_ready`
/// instead reflects whether the message loop has genuinely confirmed it's
/// consuming commands, set independently by `start_hamlib_component`
/// (`coordinator/hamlib.rs`) and never touched by the restart-supervision
/// counter machinery at all -- so a `TimedOut` startup still blocks PTT
/// here even after `TxInhibitGuard` has already released.
///
/// PAN-19 round-12 review (Codex P1): "apply loop readiness to direct PTT
/// commands". This is now `pub(crate)` and is THE single shared gate every
/// PTT-on call site in the coordinator must route through -- not just the
/// automated TX worker's own `schedule_tx`/multi-TX call sites below.
/// Round 7 through 11 only ever wired this into the TX worker; the direct,
/// operator-triggered `TogglePtt` path (`coordinator/tui_relay.rs`) had its
/// own separate, duplicated `tx_restart_inhibit`-only check that never
/// learned about `hamlib_command_loop_ready` at all, so a manual PTT toggle
/// during a slow Hamlib startup (`LoopReadyOutcome::TimedOut`, restart
/// counter already released) could still queue a key-up the loop consumes
/// later -- unexpectedly keying the radio well after the operator's actual
/// keypress. Routing `TogglePtt` through this SAME function (rather than
/// re-deriving the same `restart_inhibit && hamlib_loop_ready` condition a
/// second time) is deliberate: a third PTT-on call site added later gets
/// this gate for free instead of silently omitting it again.
///
/// PAN-19 round-14 review (Codex P1): "keep TX muted until pending rig
/// state is delivered". `hamlib_command_loop_ready` correctly reflects
/// "the message loop can consume commands" -- but that's not the same as
/// "the rig's state is actually correct". If a prior generation's
/// `SetSplit`/`SetFrequency` failed its first delivery attempt (channel
/// momentarily full) it's requeued into `hamlib_pending_frequency`/
/// `hamlib_pending_split` (round 10) for the polling task's own ~500ms
/// retry -- but the loop IS ready to consume commands during that gap, so
/// without this check PTT-on could slip through and transmit with the
/// rig still holding its stale, pre-crash split/frequency configuration.
/// `hamlib_pending_frequency`/`hamlib_pending_split` are checked here (the
/// SAME single shared choke point, not a third condition scattered
/// elsewhere) and, like `restart_inhibit`/`hamlib_loop_ready` above, fail
/// CLOSED (treated as still-pending, i.e. still muted) on a poisoned
/// lock -- see [`has_undelivered_pending_hamlib_state`].
///
/// PAN-19 round-16 review (Codex P1): "keep restored rig state pending
/// through CAT application". Round 14's pending-slot check alone still had
/// a gap: `deliver_pending_hamlib_state` clears a pending slot as soon as
/// the command is successfully handed off onto the channel -- NOT once the
/// rig has actually accepted it. A PTT-on gated through here while the
/// message loop is still awaiting the underlying `set_frequency`/
/// `set_split_freq`/`set_split` CAT call would see an already-empty
/// pending slot and be permitted, then get queued behind that in-flight
/// command and key the rig regardless of whether the CAT call succeeds or
/// fails. `hamlib_command_in_flight` closes that window: the message
/// loop's arms bump it for the CAT call's exact duration (see
/// `HamlibCommandInFlightGuard` in `coordinator/hamlib.rs`), and any
/// nonzero count is treated the same as "pending state undelivered" here.
///
/// PAN-19 round-19 review (Codex P1): "count every pending command
/// handoff". `hamlib_command_in_flight` is now a count (`AtomicU32`), not
/// a boolean -- see that field's doc comment in `coordinator/mod.rs` for
/// why a boolean under-reported when two handoffs (frequency + split)
/// were outstanding at once.
///
/// PAN-115: two hardware-health inputs, checked right after the two
/// restart/readiness reasons (hardware faults explain more than CAT
/// bookkeeping; the operator's own `Disabled` stays last). Like PAN-5's
/// restart inhibit they are health-owned safety state ANDed in here, never
/// folded into `TxPolicy`, and both clear on their own once the link or
/// device recovers:
/// - `rig_conn_state` ([`super::super::hamlib::RigConnState`], written by the Hamlib
///   poll loop) mutes only on `PollingFailed` ("was connected, polls have
///   failed for 5 s"). `NotConnected` is overloaded (mock rig, rig control
///   disabled, not tried yet, initial connect failed), so it does NOT mute;
///   a real rig that never connects still reaches `PollingFailed` after 5 s.
///   The poll loop redials rigctld while disconnected, which is what clears
///   this state again (PTT itself used to be the only redial).
/// - `audio_output_alive` (written by the real audio thread's
///   `OutputWatchdog`) is `false` from an output stream error, or 2 s without
///   an output callback, until a (re)opened stream calls back again. It stays
///   `true` in no-device modes (`--no-audio`, stub, replay), which have no
///   output device to judge.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tx_hard_mute_reason(
    tx_policy: &std::sync::Arc<std::sync::atomic::AtomicU8>,
    restart_inhibit: &std::sync::Arc<std::sync::atomic::AtomicU32>,
    hamlib_loop_ready: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    hamlib_pending_frequency: &std::sync::Arc<
        std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, ComponentMessage>>,
    >,
    hamlib_pending_split: &std::sync::Arc<std::sync::Mutex<Option<ComponentMessage>>>,
    hamlib_command_in_flight: &std::sync::Arc<std::sync::atomic::AtomicU32>,
    rig_conn_state: &std::sync::Arc<std::sync::atomic::AtomicU8>,
    audio_output_alive: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    receive_gain: &std::sync::Arc<super::super::receive_overload::GainSafety>,

    data_mode: &pancetta_hamlib::DataModeReadiness,
) -> Option<&'static str> {
    if restart_inhibit.load(Ordering::Acquire) != 0 {
        Some("rig control is restarting")
    } else if !hamlib_loop_ready.load(Ordering::Acquire) {
        Some("Hamlib command loop is not yet ready")
    } else if super::super::hamlib::RigConnState::from_u8(rig_conn_state.load(Ordering::Acquire))
        == super::super::hamlib::RigConnState::PollingFailed
    {
        Some("rig is disconnected")
    } else if !audio_output_alive.load(Ordering::Acquire) {
        Some("audio output device is dead")
    } else if has_undelivered_pending_hamlib_state(hamlib_pending_frequency, hamlib_pending_split) {
        Some("pending rig frequency/split state has not been delivered yet")
    } else if hamlib_command_in_flight.load(Ordering::Acquire) > 0 {
        Some("a rig frequency/split command is still being applied")
    } else if receive_gain.blocked() {
        Some("receive gain change is in progress or unconfirmed")
    } else if !data_mode.is_ready() {
        Some("rig data mode is not ready")
    } else if current_tx_policy(tx_policy) == pancetta_core::TxPolicy::Disabled {
        Some("TX policy is Disabled")
    } else {
        None
    }
}

/// `true` when a prior generation's `SetFrequency`/`SetSplit` is still
/// sitting undelivered in either pending slot (round 10's requeue-on-full
/// -channel mechanism), waiting for the polling task's next retry. Fails
/// CLOSED (`true`, i.e. "treat as still pending") on a poisoned lock --
/// unlike some other pending-state checks in this codebase that fail open,
/// this one gates a TX-safety-critical decision (avoiding an off-frequency
/// transmission), so an unknown state must never be treated as "safe to
/// key".
pub(super) fn has_undelivered_pending_hamlib_state(
    pending_frequency: &std::sync::Arc<
        std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, ComponentMessage>>,
    >,
    pending_split: &std::sync::Arc<std::sync::Mutex<Option<ComponentMessage>>>,
) -> bool {
    // PAN-35: any VFO with an undelivered pending command must still mute
    // TX -- not just "the shared slot happens to be occupied".
    pending_frequency
        .lock()
        .map(|slots| !slots.is_empty())
        .unwrap_or(true)
        || pending_split
            .lock()
            .map(|slot| slot.is_some())
            .unwrap_or(true)
}

/// PAN-118: frequency-aware sibling of [`tx_hard_mute_reason`]. That
/// function answers "may the station key at all right now" from global,
/// frequency-independent state; this one answers "may THIS transmission
/// key on THIS absolute RF frequency" from the configured band plan
/// (`rig.frequency.band_plan`; `edge_warnings` is the single master
/// switch for both the TUI edge warning and this hard refusal, so
/// `edge_warnings = false` remains the operator's documented escape
/// hatch). Per-request by construction (two bundle items can differ),
/// so it is NOT folded into `tx_hard_mute_reason` — whose one
/// frequency-less consumer (the QSO stall-evidence closure) must not
/// grow a frequency parameter. The boolean itself is
/// `pancetta_tui::app::tx_rf_out_of_band_plan`, the single source of
/// truth (region tables + `custom_bands` override) shared with the
/// TUI's entry-time warning. Refusals are rare; allocation cost is
/// fine. `pub` + re-exported from `coordinator` so the `coord_sim`
/// rig-simulation fixture mirrors the worker's gate with the REAL gate
/// functions, exactly as it already does for the coalescer, the arm
/// gate, and the liveness predicate.
///
/// `tx_rf_hz` is the LOWEST emitted tone (dial + audio offset, from
/// [`tx_rf_for_gate`]); `signal_span_hz` is how far above it the highest
/// emitted tone sits ([`tx_signal_span_hz`] for a protocol frame, `0.0`
/// for a single-tone tune or a raw manual-PTT carrier). The whole occupied
/// interval, low edge to high edge, must fit inside ONE allowed range of
/// the band plan (PR #446 Codex review: an FT8 frame whose base tone is
/// in-band can still put its top tone up to 43.75 Hz past an upper band
/// edge; PAN-208: a multi-range `custom_bands` entry must not be straddled
/// across the gap between two ranges —
/// `pancetta_tui::app::tx_interval_out_of_band_plan`).
pub fn tx_band_plan_mute_reason(
    tx_rf_hz: f64,
    signal_span_hz: f64,
    band_plan: &pancetta_config::BandPlanConfig,
) -> Option<String> {
    // Conservative integer edges computed from the exact f64 RF (PR #446
    // Codex review): the low edge rounds DOWN and the high edge rounds UP,
    // each from the unrounded base, so a fractional offset or span (FT4's
    // 62.4999 Hz) can never be checked inside the band while the real tone
    // sits a fraction of a Hz outside it. A non-finite RF fails closed; a
    // non-finite / negative span degrades to the base-tone-only check.
    if !tx_rf_hz.is_finite() {
        return Some(format!(
            "TX frequency {tx_rf_hz} Hz is not a valid RF frequency"
        ));
    }
    let span_hz = if signal_span_hz.is_finite() && signal_span_hz > 0.0 {
        signal_span_hz
    } else {
        0.0
    };
    let low_hz = tx_rf_hz.max(0.0).floor() as u64;
    let high_hz = (tx_rf_hz.max(0.0) + span_hz).ceil() as u64;
    let low_out = pancetta_tui::app::tx_rf_out_of_band_plan(low_hz, band_plan);
    // PAN-208: the whole occupied interval must fit inside ONE allowed range,
    // not just have each edge in some range.
    let interval_out = high_hz != low_hz
        && pancetta_tui::app::tx_interval_out_of_band_plan(low_hz, high_hz, band_plan);
    if low_out {
        Some(format!(
            "TX frequency {low_hz} Hz is outside the configured amateur band plan"
        ))
    } else if interval_out {
        Some(format!(
            "TX signal {low_hz}-{high_hz} Hz extends outside the configured \
             amateur band plan"
        ))
    } else {
        None
    }
}

/// PAN-118: distance in Hz from the lowest to the highest tone the
/// modulator emits for one `protocol` frame — `(num_tones - 1) *
/// tone_spacing`, exactly the upper bound `Ft8Modulator::
/// modulate_symbols_protocol` emits above its base frequency (FT8 43.75,
/// FT4 62.5, FT2 175). The band-plan gate adds it to the low edge to get
/// the occupied high edge.
pub fn tx_signal_span_hz(protocol: pancetta_ft8::Protocol) -> f64 {
    let params = pancetta_ft8::ProtocolParams::from_protocol(protocol);
    (params.num_tones as f64 - 1.0) * params.tone_spacing
}

/// PAN-118: absolute TX RF for gating: effective TX dial (split when
/// active, else RX dial — `pancetta_qso::qso_manager::effective_tx_dial`)
/// plus the request's absolute audio offset. `None` while the effective
/// TX dial reads 0 (no split set and RX dial 0) — the codebase's
/// "frequency not yet known" sentinel (TUI
/// 14.074 fallback, `--replay` seed, mock-rig dev modes) — in which case
/// the caller skips the band-plan check; every frequency-independent
/// hard-mute input still applies. Also the dial resolver for the TUI
/// relay's manual-PTT sibling gate (`tui_relay::ptt_band_plan_refusal`),
/// so the split-vs-RX-dial rule has exactly one implementation. `pub` +
/// re-exported for the same `coord_sim` reason as
/// `tx_band_plan_mute_reason` above.
pub fn tx_rf_for_gate(
    operating_frequency_hz: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    split_tx_frequency_hz: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    audio_offset_hz: f64,
) -> Option<f64> {
    let rx_dial = operating_frequency_hz.load(Ordering::Relaxed);
    let split = split_tx_frequency_hz.load(Ordering::Relaxed);
    // Resolve the effective TX dial FIRST (PR #446 Codex review): a known
    // split dial with the RX dial still 0 is a real TX frequency and must
    // be gated; only an effective dial of 0 means "not yet known".
    let tx_dial = pancetta_qso::qso_manager::effective_tx_dial(rx_dial, split);
    if tx_dial == 0 {
        return None;
    }
    // Exact f64 RF — NOT rounded here; `tx_band_plan_mute_reason` derives
    // conservative integer edges from it (PR #446 Codex review).
    Some(tx_dial as f64 + audio_offset_hz)
}

/// PAN-89: one TX hard-mute cause, display-only. Declared in
/// [`tx_hard_mute_reason`]'s priority order; [`TxMuteCause::as_reason`]
/// returns the gate's string for each, byte-for-byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxMuteCause {
    RigRestarting,
    HamlibLoopNotReady,
    RigDisconnected,
    AudioOutputDead,
    RigStatePending,
    RigCommandInFlight,
    ReceiveGainUnconfirmed,
    RigDataModeNotReady,
    PolicyDisabled,
}

impl TxMuteCause {
    /// The exact string [`tx_hard_mute_reason`] returns for this cause.
    pub(crate) fn as_reason(self) -> &'static str {
        match self {
            TxMuteCause::RigRestarting => "rig control is restarting",
            TxMuteCause::HamlibLoopNotReady => "Hamlib command loop is not yet ready",
            TxMuteCause::RigDisconnected => "rig is disconnected",
            TxMuteCause::AudioOutputDead => "audio output device is dead",
            TxMuteCause::RigStatePending => {
                "pending rig frequency/split state has not been delivered yet"
            }
            TxMuteCause::RigCommandInFlight => {
                "a rig frequency/split command is still being applied"
            }
            TxMuteCause::ReceiveGainUnconfirmed => {
                "receive gain change is in progress or unconfirmed"
            }
            TxMuteCause::RigDataModeNotReady => "rig data mode is not ready",
            TxMuteCause::PolicyDisabled => "TX policy is Disabled",
        }
    }
}

/// PAN-89: every active TX hard-mute cause, in [`tx_hard_mute_reason`]'s
/// priority order. Display-only (the station-readiness strip): it evaluates
/// every predicate instead of stopping at the first, and never gates TX
/// itself. Its first element always equals the gate's verdict (parity-tested
/// over every input combination), and it is empty exactly when the gate is
/// open.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tx_mute_causes(
    tx_policy: &std::sync::Arc<std::sync::atomic::AtomicU8>,
    restart_inhibit: &std::sync::Arc<std::sync::atomic::AtomicU32>,
    hamlib_loop_ready: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    hamlib_pending_frequency: &std::sync::Arc<
        std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, ComponentMessage>>,
    >,
    hamlib_pending_split: &std::sync::Arc<std::sync::Mutex<Option<ComponentMessage>>>,
    hamlib_command_in_flight: &std::sync::Arc<std::sync::atomic::AtomicU32>,
    rig_conn_state: &std::sync::Arc<std::sync::atomic::AtomicU8>,
    audio_output_alive: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    receive_gain: &std::sync::Arc<super::super::receive_overload::GainSafety>,

    data_mode: &pancetta_hamlib::DataModeReadiness,
) -> Vec<TxMuteCause> {
    let mut causes = Vec::new();
    if restart_inhibit.load(Ordering::Acquire) != 0 {
        causes.push(TxMuteCause::RigRestarting);
    }
    if !hamlib_loop_ready.load(Ordering::Acquire) {
        causes.push(TxMuteCause::HamlibLoopNotReady);
    }
    if super::super::hamlib::RigConnState::from_u8(rig_conn_state.load(Ordering::Acquire))
        == super::super::hamlib::RigConnState::PollingFailed
    {
        causes.push(TxMuteCause::RigDisconnected);
    }
    if !audio_output_alive.load(Ordering::Acquire) {
        causes.push(TxMuteCause::AudioOutputDead);
    }
    if has_undelivered_pending_hamlib_state(hamlib_pending_frequency, hamlib_pending_split) {
        causes.push(TxMuteCause::RigStatePending);
    }
    if hamlib_command_in_flight.load(Ordering::Acquire) > 0 {
        causes.push(TxMuteCause::RigCommandInFlight);
    }
    if receive_gain.blocked() {
        causes.push(TxMuteCause::ReceiveGainUnconfirmed);
    }
    if !data_mode.is_ready() {
        causes.push(TxMuteCause::RigDataModeNotReady);
    }
    if current_tx_policy(tx_policy) == pancetta_core::TxPolicy::Disabled {
        causes.push(TxMuteCause::PolicyDisabled);
    }
    causes
}

/// Whether a TX item belonging to `qso_id` is still allowed on the air, given
/// the shared active-QSO set. Thin wrapper over [`super::super::tx_qso_is_live`] that
/// takes the read lock. A poisoned lock fails *open* (returns `true`) — a stuck
/// lock should never silently mute legitimate TX; the worst case reverts to the
/// pre-fix behavior for one cycle, which the operator-facing emergency stop
/// (Shift+Q → cancel-all + Disabled) still covers.
pub(super) fn tx_qso_is_live(
    qso_id: Option<&str>,
    active: &std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
) -> bool {
    match active.read() {
        Ok(set) => super::super::tx_qso_is_live(qso_id, &set),
        Err(_) => true,
    }
}

/// Multi-TX key-time liveness check (Step 4b fast-path gate).
///
/// 2026-07-17 operator finding: a multi-TX bundle's waveform is summed
/// once, up front (Step 1), from whichever items survived Step 0b's
/// per-item liveness filter. The bundle then waits (Step 4) for the PTT
/// engage instant, and that wait CAN span an operator abort ('k') of one
/// of the bundled QSOs. Because the waveform is already a single summed
/// buffer at that point, it can't be partially re-filtered in place — so
/// the OLD key-time gate only checked "did EVERY item go stale" and, if
/// even one item was still live, transmitted the WHOLE pre-summed audio,
/// including the now-cancelled item's already-baked-in call.
/// Operator-observed symptom: killing a QSO to one DX station while a
/// second, still-live QSO shared the same bundle still transmitted a
/// call to the killed one.
///
/// This function answers only "did NOTHING go stale" (the common-case
/// fast path — reuse the already-built audio unchanged). When something
/// DID go stale, the caller re-encodes just the still-live subset via
/// [`encode_and_modulate_multi_tx`] and re-applies the already-computed
/// `TxSchedule` (timing doesn't depend on which items are in the bundle)
/// instead of either transmitting the stale audio or dropping the whole
/// bundle outright.
pub(super) fn multi_tx_bundle_still_fully_live(
    qso_ids: &[Option<String>],
    active_tx_qsos: &std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
) -> bool {
    qso_ids
        .iter()
        .all(|id| tx_qso_is_live(id.as_deref(), active_tx_qsos))
}

/// PAN-141: the freshness window for [`dx_parity_conflict`] — self-scaling
/// with the live protocol (`2 × active_slot_ns`: FT8 30s, FT4 15s, FT2
/// 6.4s), matching the incident-measured ~20-30s gap between a manual call's
/// parity latch and its actual key-up. Deliberately independent of
/// `CrossTimeState::a7_recent_calls`'s own 30s `max_age` — that value is
/// tuned for hb-048's decoder-correlation purpose, not TX-collision
/// avoidance, and `get()` doesn't filter by age on read (eviction is lazy,
/// on the next `record()`), so a caller must apply its own window regardless.
pub(super) fn dx_parity_freshness_window(slot_ns: i64) -> std::time::Duration {
    std::time::Duration::from_nanos(slot_ns.max(0) as u64 * 2)
}

/// PAN-146 (Codex P2 on PR #370, round 1): the band and slot grid the frame
/// about to key belongs to. An `a7_recent_calls` entry counts as DX-parity
/// evidence only if it was observed in this context — a parity heard on
/// another band says nothing about this one, and a parity computed on
/// another protocol's slot grid describes different slot boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DxParityContext {
    /// Live RX dial (Hz) from `operating_frequency_hz`; `0` = unknown.
    pub(super) dial_hz: u64,
    /// Slot period (ns) `required_parity` was computed on. Also sets the
    /// freshness window ([`dx_parity_freshness_window`]).
    pub(super) slot_ns: i64,
}

impl DxParityContext {
    /// Reads the live dial now — call at the gate, not earlier.
    pub(super) fn live(
        operating_frequency_hz: &std::sync::atomic::AtomicU64,
        slot_ns: i64,
    ) -> Self {
        Self {
            dial_hz: operating_frequency_hz.load(Ordering::Relaxed),
            slot_ns,
        }
    }

    /// Same slot grid, and same band per [`super::super::is_band_change`] — the rule
    /// the DSP band-change flush and the C9 QSO teardown use. The band check
    /// is skipped while either dial is the unknown-0 sentinel (as PAN-118's
    /// band-plan gate does): with no dial known there is no band switch to
    /// detect, so the PAN-141 hold keeps working on a station without CAT.
    fn observed_in(self, entry: &pancetta_qso::A7ExpectedCall) -> bool {
        entry.slot_ns == self.slot_ns
            && (entry.captured_dial_hz == 0
                || self.dial_hz == 0
                || !super::super::is_band_change(entry.captured_dial_hz, self.dial_hz))
    }
}

/// PAN-141: does the DX's most recently observed OWN-TX slot parity (from
/// [`pancetta_qso::CrossTimeState::a7_recent_calls`] — populated from every
/// decode this station makes, regardless of QSO relevance) conflict with
/// `required_parity`, the parity we are about to key into?
///
/// A "conflict" means: within the last `freshness_window`, `their_callsign`
/// was observed transmitting on `required_parity` itself. Since two stations
/// in QSO must transmit on OPPOSITE parities, a DX transmitting on the exact
/// slot we're about to use means they're mid-exchange with someone else in
/// that window — keying now would collide with that exchange, not reach
/// them. Returns `false` (proceed) on anything short of that positive fresh
/// signal: no entry for this callsign, an entry older than the freshness
/// window, or an unreadable (poisoned) table — this gate only ever blocks a
/// TX on POSITIVE fresh evidence of a collision, never on missing data,
/// mirroring `tx_qso_is_live`'s own fail-open posture on a poisoned lock.
///
/// Codex P2 on PR #370 (round 2, superseded by round 3): the lookup must
/// use the repo's established callsign-equivalence rules, not an exact
/// `HashMap` key match — `a7_recent_calls` is populated from on-air decodes
/// (whatever compound form the DX happened to sign with), while
/// `their_callsign` here comes from `QsoMetadata`, which can latch a
/// DIFFERENT compound form of the same station (round 2: also case, e.g. a
/// remote `callStation` command supplying lowercase; round 3: also compound
/// spelling, e.g. `EA8/G8BCG` latched but a later frame signs bare
/// `G8BCG`). Both are exactly what `pancetta_core::callsign::callsigns_match`
/// already exists to handle, so this scans every fresh matching entry (via
/// `entries_with_parity`, an existing public iterator, called once per
/// parity to cover the whole table with no exact-key lookup) using that
/// same shared matcher instead of hand-rolling a second, narrower
/// equivalence rule.
///
/// Local review gate, round 3: a compound and bare spelling of the same
/// station are separate table entries with independent timestamps and
/// parities — scanning only the REQUESTED parity's subset could match a
/// stale alias while a fresher entry (a different spelling, opposite
/// parity — i.e. NOT a conflict) exists for the same station. Selecting the
/// single NEWEST fresh matching entry across both parities first, then
/// comparing only its parity, uses the DX's actual latest known state
/// rather than whichever alias happens to match the parity being tested.
///
/// PAN-146: only entries observed in `context` (same band, same slot grid —
/// [`DxParityContext::observed_in`]) are candidates, and the context filter
/// runs before the newest-entry pick, so an out-of-context alias can neither
/// hold the frame nor hide an in-context entry.
pub(super) fn dx_parity_conflict(
    their_callsign: &str,
    required_parity: pancetta_core::slot::SlotParity,
    context: DxParityContext,
    cross_time_state: &pancetta_qso::CrossTimeState,
    now: std::time::SystemTime,
) -> bool {
    let freshness_window = dx_parity_freshness_window(context.slot_ns);
    let Ok(a7) = cross_time_state.a7_recent_calls.read() else {
        return false;
    };
    let newest = a7
        .entries_with_parity(0)
        .chain(a7.entries_with_parity(1))
        .filter(|entry| pancetta_core::callsign::callsigns_match(&entry.callsign, their_callsign))
        .filter(|entry| context.observed_in(entry))
        .filter(|entry| {
            now.duration_since(entry.decoded_at)
                .is_ok_and(|age| age <= freshness_window)
        })
        .max_by_key(|entry| entry.decoded_at);
    let Some(entry) = newest else {
        return false;
    };
    let observed = match entry.slot_parity {
        0 => pancetta_core::slot::SlotParity::Even,
        _ => pancetta_core::slot::SlotParity::Odd,
    };
    observed == required_parity
}

/// PAN-141: resolves `qso_id` to its established DX callsign via the
/// restart-safe `qso_manager_watch` handle (re-borrowed fresh here, never
/// cached across the pre-PTT wait — see PAN-72's `qso_manager_watch` amendment
/// for why a spawn-time-captured clone would go stale across a supervised Qso
/// restart). Fails open (`None`) at every step short of a resolved,
/// established partner: no `qso_id` (manual/tune sends never carry a target
/// to check), the Qso component not up yet, an unparseable id, the QSO not
/// found, or a QSO with no established `their_callsign` yet (e.g. a still-
/// unpartnered `CallingCq`).
///
/// PAN-148: this is the ONLY inherently-async half of the DX-parity check —
/// `manager.get_qso(id).await` is a real yield point; `their_callsign` is a
/// stable property of an established QSO once set, so resolving it here and
/// consulting it in a later, purely SYNCHRONOUS read (see
/// [`dx_parity_conflict`], over `a7_recent_calls` — a `std::sync::RwLock`,
/// not async) does not reintroduce staleness. Split out specifically so a
/// caller checking MULTIPLE items (`bundle_live_mask`) can resolve every
/// item's callsign first (the only awaits) and then read the actual
/// freshness-sensitive state for all of them back-to-back with zero
/// intervening awaits — see that function's doc comment for why a single
/// combined async-then-sync-per-item loop is NOT equivalent.
pub(super) async fn resolve_their_callsign(
    qso_id: Option<&str>,
    qso_manager_watch: &tokio::sync::watch::Receiver<Option<pancetta_qso::QsoManager>>,
) -> Option<String> {
    let id_str = qso_id?;
    let id = id_str.parse::<pancetta_qso::QsoId>().ok()?;
    let manager = qso_manager_watch.borrow().clone()?;
    let progress = manager.get_qso(id).await.ok()?;
    progress.metadata.their_callsign
}

/// PAN-141: resolves `qso_id` to its established DX callsign, then applies
/// [`dx_parity_conflict`]. See [`resolve_their_callsign`] for the fail-open
/// conditions.
///
/// PAN-146: the live dial is read AFTER the resolver's await, alongside the
/// clock, so a band switch landing during that await is seen.
pub(super) async fn dx_parity_conflict_for_qso(
    qso_id: Option<&str>,
    required_parity: pancetta_core::slot::SlotParity,
    operating_frequency_hz: &std::sync::atomic::AtomicU64,
    slot_ns: i64,
    qso_manager_watch: &tokio::sync::watch::Receiver<Option<pancetta_qso::QsoManager>>,
    cross_time_state: &pancetta_qso::CrossTimeState,
) -> bool {
    let Some(their_callsign) = resolve_their_callsign(qso_id, qso_manager_watch).await else {
        return false;
    };
    dx_parity_conflict(
        &their_callsign,
        required_parity,
        DxParityContext::live(operating_frequency_hz, slot_ns),
        cross_time_state,
        std::time::SystemTime::now(),
    )
}

/// PAN-148: reasons the Step 4d final gate can deny a single-TX key.
/// Distinct variants only so the denial's log/diagnostic text can name the
/// actual reason, mirroring Step 4b/4b-parity's separate messages for the
/// same two conditions re-evaluated here at the true last instant.
/// PAN-117 adds the third: the frame's elapsed time in its target slot
/// exceeded `tx_late_max_ms` by the time the gate ran — keying it would
/// put an undecodable fragment on the air.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FinalGateDenial {
    StaleQso,
    DxParityConflict,
    TooLateToDecode { elapsed_ms: u64, late_max_ms: u64 },
}

/// PAN-141 (Step 4b) / PAN-148 (Step 4d): resolves every id's DX callsign —
/// the batch form of [`resolve_their_callsign`] for a multi-TX bundle.
/// Callers running this for the Step 4d final gate MUST call it BEFORE
/// Step 3's final audio-trim (see that call site's comment: round-2 review,
/// Codex P1 — awaiting this lookup AFTER the trim risked keying `SetPtt`
/// against a now-stale alignment cursor if `QsoManager`'s internal lock
/// were ever contended).
pub(super) async fn resolve_callsigns(
    ids: &[Option<String>],
    qso_manager_watch: &tokio::sync::watch::Receiver<Option<pancetta_qso::QsoManager>>,
) -> Vec<Option<String>> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(resolve_their_callsign(id.as_deref(), qso_manager_watch).await);
    }
    out
}

/// PAN-148 Step 4d: the single-TX path's final, atomic pre-PTT gate —
/// SYNCHRONOUS, using ONLY state read at THIS call — never a value cached
/// from Step 4b/4b-parity earlier in the cycle, which can have gone stale
/// during Step 4c's pivot/remodulation or Step 5's status-announce awaits
/// (`send_tx_status`/`send_tx_queue_status`). The caller MUST NOT `.await`
/// anything else between this call returning and the `SetPtt{true}` send it
/// gates — see the call site's own comment.
///
/// `their_callsign` must be resolved SEPARATELY and EARLY, via
/// [`resolve_their_callsign`] called before Step 3's final audio-trim — see
/// that function's doc comment. Round-1 review (Codex P1) found an earlier
/// version checking liveness before awaiting the DX-parity lookup inline
/// here (a cancellation landing during that await went uncaught); round-2
/// review (Codex P1) found that simply reordering those two checks still
/// left an unbounded `QsoManager`-lock-contention wait sitting AFTER Step
/// 3's timing-sensitive trim, risking a stale audio-alignment cursor at key
/// time. Resolving the callsign early and passing it in here — leaving this
/// function with NO internal `.await` at all — fixes both: liveness and the
/// actual freshness read (`dx_parity_conflict`) are the true last reads
/// before `SetPtt`, and nothing async can suspend between Step 3's trim and
/// the key.
///
/// PAN-117's slot-timing re-check is deliberately NOT here: it reads the
/// clock, so it runs as its own [`key_time_gate_denial`] call after every
/// other synchronous pre-PTT gate (this one, the hard mute, the remote arm
/// recheck), immediately before the `SetPtt{true}` send — PR #450 Codex
/// review: a clock read taken here could go stale while this function's
/// `RwLock` reads and the later gates ran.
pub(super) fn final_single_tx_gate_denial(
    qso_id: Option<&str>,
    required_parity: pancetta_core::slot::SlotParity,
    context: DxParityContext,
    active_tx_qsos: &std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    their_callsign: Option<&str>,
    cross_time_state: &pancetta_qso::CrossTimeState,
) -> Option<FinalGateDenial> {
    if !tx_qso_is_live(qso_id, active_tx_qsos) {
        return Some(FinalGateDenial::StaleQso);
    }
    let conflict = their_callsign.is_some_and(|callsign| {
        dx_parity_conflict(
            callsign,
            required_parity,
            context,
            cross_time_state,
            std::time::SystemTime::now(),
        )
    });
    if conflict {
        return Some(FinalGateDenial::DxParityConflict);
    }
    None
}

/// PAN-145: compare raw QsoId epochs after `PttGuard::new` has set
/// `ptt_active` under the shared gate. Later offset switches now refuse, so
/// these reads need no async lock or repeated DSP. None is manual/test TX.
pub(super) fn relocation_epoch_stale(
    epochs: &pancetta_qso::qso_manager::TxRelocationEpochs,
    snapshots: &[(Option<String>, u64)],
) -> bool {
    snapshots
        .iter()
        .any(|(id, before)| id.as_deref().is_some_and(|id| epochs.epoch(id) != *before))
}

/// PAN-117: the slot-timing half of the Step 4d final gate, shared by both
/// worker arms. `now` must be the caller's TRUE last clock read — taken
/// after every other synchronous pre-PTT gate, immediately before the
/// `SetPtt{true}` send — and if more than `late_max_ms` of `target_slot`
/// has elapsed by then the frame is denied with `TooLateToDecode`: keyed
/// that late it is mathematically undecodable for any receiver, so it is
/// dropped rather than keyed (the QSO engine's existing retry timers own
/// the re-send).
pub(super) fn key_time_gate_denial(
    now: UtcTimestamp,
    target_slot: UtcTimestamp,
    late_max_ms: u64,
) -> Option<FinalGateDenial> {
    key_time_too_late(now, target_slot, late_max_ms).then(|| FinalGateDenial::TooLateToDecode {
        elapsed_ms: (now - target_slot).as_millis_i64().max(0) as u64,
        late_max_ms,
    })
}

/// Log + diagnostic text for a single-TX Step 4d denial, `(log, diagnostic)`.
pub(super) fn final_gate_denial_text(
    denial: FinalGateDenial,
    qso_id: Option<&str>,
    required_parity: pancetta_core::slot::SlotParity,
    message_text: &str,
) -> (String, String) {
    match denial {
        FinalGateDenial::StaleQso => (
            format!(
                "dropping stale TX at final pre-PTT check for ended QSO {}: '{message_text}'",
                qso_id.unwrap_or("?")
            ),
            format!("dropping stale TX at final pre-PTT check for ended QSO: '{message_text}'"),
        ),
        FinalGateDenial::DxParityConflict => (
            format!(
                "holding TX at final pre-PTT check for QSO {}: DX observed transmitting on the colliding parity {required_parity:?}: '{message_text}'",
                qso_id.unwrap_or("?")
            ),
            format!(
                "holding TX at final pre-PTT check: DX observed transmitting on the colliding parity {required_parity:?}: '{message_text}'"
            ),
        ),
        FinalGateDenial::TooLateToDecode {
            elapsed_ms,
            late_max_ms,
        } => (
            format!(
                "dropping stale TX at final pre-PTT check for QSO {}: {elapsed_ms}ms into the slot exceeds tx_late_max_ms={late_max_ms}ms — no receiver could decode it: '{message_text}'",
                qso_id.unwrap_or("?")
            ),
            format!(
                "dropping stale TX at final pre-PTT check: {elapsed_ms}ms into the slot exceeds tx_late_max_ms={late_max_ms}ms — no receiver could decode it: '{message_text}'"
            ),
        ),
    }
}

/// Whether a **remote-originated** TX request from `channel` is permitted to
/// key PTT right now, per the shared arm gate.
///
/// This is the safety gate for `TxOrigin::Remote` requests only — the worker
/// never calls it for `TxOrigin::Local` (local TX is byte-identical). It reads
/// the shared [`ArmState`](pancetta_agent::arm::ArmState) and returns
/// `arm.tx_permitted(now(), channel)` — `channel`'s own consent bit, never
/// another channel's. A diagnostic entry point: production gates go through
/// [`remote_tx_permitted_for`], which derives the channel from the frame.
///
/// **Fail-CLOSED on a poisoned lock** — the OPPOSITE of [`tx_qso_is_live`]'s
/// fail-open. This is a safety gate: if the arm mutex is poisoned we can no
/// longer prove the station consented, so remote TX is DENIED. `now` is the
/// one clock read (`ArmState` itself is pure); it runs only AFTER the lock is
/// acquired, so a lock wait cannot make the sample stale. Production passes
/// [`ArmTime::now`](pancetta_agent::arm::ArmTime::now).
pub fn remote_tx_permitted(
    arm: &std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    now: impl FnOnce() -> pancetta_agent::arm::ArmTime,
    channel: pancetta_agent::arm::ConsentChannel,
) -> bool {
    match arm.lock() {
        Ok(state) => state.tx_permitted(now(), channel),
        Err(_) => false,
    }
}

/// The remote-TX channel a frame originated on, derived from its bound client
/// identity: every station-agent producer binds `Some(peer)`
/// (`tx_kind_to_qso_message`), while the WSJT-X UDP bridge — which has no
/// client identity concept — binds `None`. Selects which channel's own
/// consent bit the TX-time gate requires.
pub(super) fn consent_channel_for(
    remote_client_key_id: Option<&str>,
) -> pancetta_agent::arm::ConsentChannel {
    match remote_client_key_id {
        Some(_) => pancetta_agent::arm::ConsentChannel::StationAgent,
        None => pancetta_agent::arm::ConsentChannel::WsjtxUdp,
    }
}

/// Like [`remote_tx_permitted`], but additionally binds the check to a
/// specific client (PAN-91).
///
/// The plain boolean arm gate above only asks "is *some* client currently
/// armed?" — it has no way to tell whether the frame in hand is the one that
/// specific client actually requested. Without this, a peer without TX-arm
/// eligibility could get a `TxRequest`-originated QSO queued while the
/// controller slot is free (creating the QSO is deliberately never gated —
/// only transmission is), and if a DIFFERENT, legitimate peer later takes
/// control and arms, the worker would authorize the pending frame from the
/// FIRST peer, because the old gate only checked "is the shared `ArmState`
/// armed", never "armed by the same client this frame is bound to".
///
/// `remote_client_key_id` also names the ORIGINATING channel, whose own
/// current consent is required (PAN-98; one channel's consent never permits
/// another channel's TX):
///
/// - `None` — the WSJT-X UDP bridge (no client identity concept): requires
///   the WSJT-X UDP channel's consent ([`ConsentChannel::WsjtxUdp`]) plus the
///   rest of the boolean arm gate, with no identity binding.
/// - `Some(id)` — the station-agent channel: requires the station-agent
///   channel's consent ([`ConsentChannel::StationAgent`]) plus the rest of the
///   boolean arm gate, AND that `id` is the client
///   `ArmState::armed_client_key_id()` currently reports — a mismatch
///   (including "armed, but by someone else") denies fail-closed.
///
/// [`ConsentChannel::WsjtxUdp`]: pancetta_agent::arm::ConsentChannel::WsjtxUdp
/// [`ConsentChannel::StationAgent`]: pancetta_agent::arm::ConsentChannel::StationAgent
pub fn remote_tx_permitted_for(
    arm: &std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    now: impl FnOnce() -> pancetta_agent::arm::ArmTime,
    remote_client_key_id: Option<&str>,
) -> bool {
    match arm.lock() {
        Ok(state) => {
            if !state.tx_permitted(now(), consent_channel_for(remote_client_key_id)) {
                return false;
            }
            match remote_client_key_id {
                None => true,
                Some(id) => state.armed_client_key_id() == Some(id),
            }
        }
        Err(_) => false,
    }
}

/// PAN-134: authorization for all four safety-critical sends. Evaluated
/// immediately before the enqueue: for audio inside MessageBus's read lock by
/// `send_message_if`, after the lock wait; for PTT-on by
/// `send_rig_command_if`, with no lock (PAN-202).
/// Never check this and then use plain `send_message`: that restores the gap.
pub(super) fn tx_send_authorized(
    origin: crate::message_bus::TxOrigin,
    arm: &std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    remote_client_key_id: Option<&str>,
) -> bool {
    origin != crate::message_bus::TxOrigin::Remote
        || remote_tx_permitted_for(arm, pancetta_agent::arm::ArmTime::now, remote_client_key_id)
}

/// One-time snapshot of the remote-TX arm's permission state, taken under a
/// single lock acquisition, so a BATCH of entries (e.g. a coalesced
/// backlog) can be evaluated against one consistent view instead of each
/// entry re-locking `ArmState` separately (round-3 review, Codex P1).
/// Per-entry re-locking left a TOCTOU window: if control transfers A→B→A
/// between two entries' individual lock acquisitions within the same
/// batch, both entries can pass their own check yet the batch's later
/// single-identity fold only ever records one of them, silently
/// authorizing the other under a mismatched identity.
#[derive(Clone)]
pub(super) struct ArmSnapshot {
    /// The boolean arm gate for a station-agent-originated frame (`Some`
    /// client identity) — reads the station-agent channel's consent.
    pub(super) station_agent_permitted: bool,
    /// The boolean arm gate for a WSJT-X-UDP-originated frame (`None` client
    /// identity) — reads the WSJT-X UDP channel's consent.
    pub(super) wsjtx_permitted: bool,
    pub(super) armed_client_key_id: Option<String>,
    /// Round-10 review (Codex P2): captured under the SAME lock as the
    /// other two fields, so audit attribution for a denial this snapshot
    /// decided can use this frozen value instead of re-locking the LIVE
    /// `ArmState` later — the denial reporting this snapshot feeds is now
    /// a detached `tokio::spawn`'d task, which can run arbitrarily later,
    /// by which point a different operator (or the same one re-arming)
    /// may have taken the arm. Re-locking at that point would attribute a
    /// durable `TxDenied` record to whoever happens to be armed when the
    /// task finally runs, not whoever was (or wasn't) armed when the
    /// denial was actually decided.
    pub(super) operator_callsign: Option<String>,
    /// The wall-clock instant (unix ms) the permission was decided at, taken
    /// with the lock held — the timestamp for any audit this snapshot feeds.
    pub(super) decided_at_unix_ms: i64,
}

impl ArmSnapshot {
    /// `now` is sampled only after the lock is acquired (see
    /// [`remote_tx_permitted`]).
    pub(super) fn take(
        arm: &std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
        now: impl FnOnce() -> pancetta_agent::arm::ArmTime,
    ) -> Self {
        match arm.lock() {
            Ok(state) => {
                let now = now();
                Self {
                    station_agent_permitted: state
                        .tx_permitted(now, pancetta_agent::arm::ConsentChannel::StationAgent),
                    wsjtx_permitted: state
                        .tx_permitted(now, pancetta_agent::arm::ConsentChannel::WsjtxUdp),
                    armed_client_key_id: state.armed_client_key_id().map(str::to_string),
                    operator_callsign: state.operator_callsign().map(str::to_string),
                    decided_at_unix_ms: now.unix_ms,
                }
            }
            Err(_) => Self {
                station_agent_permitted: false,
                wsjtx_permitted: false,
                armed_client_key_id: None,
                operator_callsign: None,
                decided_at_unix_ms: now().unix_ms,
            },
        }
    }

    /// The boolean arm gate (no identity binding) for the channel
    /// `remote_client_key_id` originated on — see [`consent_channel_for`].
    fn channel_permitted(&self, remote_client_key_id: Option<&str>) -> bool {
        match consent_channel_for(remote_client_key_id) {
            pancetta_agent::arm::ConsentChannel::StationAgent => self.station_agent_permitted,
            pancetta_agent::arm::ConsentChannel::WsjtxUdp => self.wsjtx_permitted,
        }
    }

    /// Mirrors [`remote_tx_permitted_for`]'s logic against this frozen
    /// snapshot instead of a fresh lock.
    pub(super) fn permits(&self, remote_client_key_id: Option<&str>) -> bool {
        if !self.channel_permitted(remote_client_key_id) {
            return false;
        }
        match remote_client_key_id {
            None => true,
            Some(id) => self.armed_client_key_id.as_deref() == Some(id),
        }
    }

    /// Round-3 review (Codex P2): true only when the frame would otherwise
    /// be permitted (arm is up, heartbeat/TTL/consent/kill all clear) but
    /// is bound to a DIFFERENT client than the one currently armed — as
    /// opposed to every other denial cause (nobody armed, heartbeat lost,
    /// TTL expired, local consent/kill), which is not an identity problem
    /// at all and must not be reported as one.
    pub(super) fn is_identity_mismatch(&self, remote_client_key_id: Option<&str>) -> bool {
        self.channel_permitted(remote_client_key_id)
            && remote_client_key_id
                .is_some_and(|id| self.armed_client_key_id.as_deref() != Some(id))
    }

    /// Round-10 review (Codex P2): mirrors [`tx_denied_operator_attribution`]'s
    /// logic against this frozen snapshot instead of a fresh lock — see
    /// this struct's `operator_callsign` field doc for why that matters
    /// for detached denial reporting.
    ///
    /// Round-13 review (Codex P1): deliberately does NOT reuse
    /// `is_identity_mismatch` — that predicate is gated on `tx_permitted`
    /// (it exists to pick DENIAL-REASON WORDING: don't say "bound to a
    /// different client" when the real cause is heartbeat/TTL/kill).
    /// Attribution is a different question — "did A's frame get denied
    /// while it's actually B who is bound here" — and must hold
    /// independent of WHY the frame was denied: if nobody is currently
    /// armed AND the frame is bound to a specific client, that client's
    /// identity still differs from "nobody", so this must still refuse to
    /// attribute the denial to whoever the (unrelated) armed_client_key_id
    /// happens to be.
    pub(super) fn operator_attribution(
        &self,
        remote_client_key_id: Option<&str>,
    ) -> Option<String> {
        let identity_differs =
            remote_client_key_id.is_some_and(|id| self.armed_client_key_id.as_deref() != Some(id));
        if identity_differs {
            None
        } else {
            self.operator_callsign.clone()
        }
    }
}

/// Attribute a `TxDenied` audit record's `operator_callsign`, never to the
/// CURRENTLY-armed operator when the denied frame is bound to a DIFFERENT
/// client (PAN-91 identity mismatch) — that would misattribute an
/// unauthorized request as if it came from whoever else happens to be
/// armed, which is actively misleading in a durable security audit record.
/// Falls back to the pre-PAN-91 attribution (whoever is armed, if anyone)
/// for every other denial reason (not armed, heartbeat lost, TTL expired,
/// local consent/kill) and for every frame with no bound identity at all.
pub(super) fn tx_denied_operator_attribution(
    remote_tx_arm: &std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    remote_client_key_id: Option<&str>,
) -> Option<String> {
    remote_tx_arm.lock().ok().and_then(|s| {
        let mismatched_identity =
            remote_client_key_id.is_some_and(|id| s.armed_client_key_id() != Some(id));
        if mismatched_identity {
            None
        } else {
            s.operator_callsign().map(str::to_string)
        }
    })
}
