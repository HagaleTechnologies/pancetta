//! QSO state machine and management
//!
//! This module provides the core QSO management functionality including
//! state transitions, timeout handling, and QSO lifecycle management.

use crate::async_database::QsoDatabase;
use crate::states::*;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, RwLock};
use tokio::time::{interval, Duration as TokioDuration, Interval};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Usable FT8 audio passband for our TX offset (Hz). Matches the collision
/// detector's clamp range in `autonomous.rs`.
pub const TX_OFFSET_MIN_HZ: f64 = 300.0;
/// Upper bound of the usable FT8 audio passband for our TX offset (Hz).
pub const TX_OFFSET_MAX_HZ: f64 = 2700.0;

/// Widest audio offset an **already-active** QSO may legitimately sit on (Hz),
/// and therefore the defensive clamp band for
/// [`QsoManager::apply_tx_offset_switch`].
///
/// Deliberately WIDER than [`TX_OFFSET_MIN_HZ`]/[`TX_OFFSET_MAX_HZ`], which
/// govern where we autonomously *pick a fresh* offset — not where an
/// in-progress QSO is allowed to be. Answering a CQ ties our reply to wherever
/// we decoded the DX, unclamped, for any DX up to ~2900 Hz (see the identical
/// reasoning on `DRIFT_CANDIDATE_MIN_HZ`/`MAX_HZ` in the frequency-drift
/// candidate gate). Clamping an offset action to the narrower *pick* band
/// would therefore be actively wrong for `OffsetAction::Revert`, whose
/// `target_hz` is a `last_known_good_offset_hz` that may itself be such an
/// unclamped reply offset: reverting a QSO to 2700 when it demonstrably worked
/// at 2850 defeats the point of reverting at all.
///
/// The real constraint is the modulator's transmittable envelope
/// (`pancetta_ft8::modulator::MAX_FREQUENCY_DEVIATION` = 3100.0, which covers
/// a 2900 Hz base plus the widest FT2 tone spread).
pub const ACTIVE_QSO_TX_OFFSET_MIN_HZ: f64 = 200.0;
/// Upper bound of [`ACTIVE_QSO_TX_OFFSET_MIN_HZ`]'s band. See its doc comment.
pub const ACTIVE_QSO_TX_OFFSET_MAX_HZ: f64 = 2900.0;

/// Below this much movement (Hz), an "offset switch" relocates nothing.
///
/// The pre-existing float-noise tolerance
/// [`QsoManager::apply_tx_offset_switch`]'s `partner_freq` latch already used
/// (borrowed in turn from `compute_manual_tx_offset`'s `tx_off != dx_freq`
/// test), promoted to a named constant so the no-op REFUSAL added for PAN-79 /
/// PAN-72 round 3 and that latch can never disagree about what counts as a
/// real move. Well below the allocator's `min_separation_hz` (75 Hz default),
/// so it can never reject a genuine relocation.
const TX_OFFSET_NOOP_TOLERANCE_HZ: f64 = 1.0;

/// SM-F6: bounded resend cap for AUTONOMOUS QSOs re-armed by
/// [`QsoManager::rearm_manual_calls_at`] (`RespondingToCq`/`SendingReport`
/// only). This is deliberately NOT `manual_call_max_calls` (25 calls, an
/// operator-supervised, long-running bound) -- an Auto QSO is unattended TX
/// and must stay conservative. `call_count` starts at 1 (the opening send),
/// so a cap of 2 allows exactly ONE resend. Combined with the one-slot
/// (`rearm_slot_millis`, 15s in FT8) cadence, that resend lands around the
/// ~15s mark under FT8 -- and earlier still under the shorter FT4/FT2 slots --
/// safely inside the existing 30s `report_timeout` (see `check_timeouts_at`'s
/// "Phase 5" Auto branch) -- we are NOT extending that 30s outer bound, only
/// making use of the window with an actual mid-window resend instead of dead
/// silence. No new config surface.
///
/// PAN-72 round 14 (Codex round 14 on PR #350, thread on `tui_relay.rs:3116`):
/// hoisted out of `rearm_manual_calls_at`'s local scope so
/// `QsoManager::apply_tx_offset_switch`'s own one-shot retransmission path
/// (the `cq_retransmit` block) can reference the SAME value when deciding
/// whether an Auto QSO in `RespondingToCq`/`SendingReport` is actually AT or
/// PAST the cap -- see that block's doc comment.
const AUTO_RESEND_MAX_CALLS: u32 = 2;

/// Hound calling region: Hounds call the Fox in 1000–2700 Hz, where a WSJT-X
/// Fox listens (PAN-259).
const HOUND_CALL_MIN_HZ: f64 = 1000.0;
const HOUND_CALL_MAX_HZ: f64 = 2700.0;
/// Hound response envelope: a Fox transmits in 300–900 Hz, and when it sends
/// us a report we answer at the Fox's own offset. The envelope only admits
/// that offset; it is never a pool to pick another frequency from (PAN-259).
const HOUND_RESPONSE_MIN_HZ: f64 = 300.0;
const HOUND_RESPONSE_MAX_HZ: f64 = 900.0;

/// Audio-offset boundaries for FT8 Hound (DXpedition chaser) mode.
///
/// Carried inside [`QsoManagerConfig`] so callers (e.g. the coordinator) can
/// wire in operator-configured ranges from `pancetta-config::HoundConfig`
/// without introducing a cross-crate dependency (pancetta-qso is a lower crate
/// and must not depend on pancetta-config).
///
/// The defaults match the module-level `HOUND_*` constants (calling
/// 1000/2700, response envelope 300/900).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoundRegions {
    /// Calling-region minimum audio offset (Hz). Default 1000.
    pub call_min_hz: f64,
    /// Calling-region maximum audio offset (Hz). Default 2700.
    pub call_max_hz: f64,
    /// Fox response-envelope minimum audio offset (Hz). Default 300.
    pub response_min_hz: f64,
    /// Fox response-envelope maximum audio offset (Hz). Default 900.
    pub response_max_hz: f64,
}

impl Default for HoundRegions {
    fn default() -> Self {
        Self {
            call_min_hz: HOUND_CALL_MIN_HZ,
            call_max_hz: HOUND_CALL_MAX_HZ,
            response_min_hz: HOUND_RESPONSE_MIN_HZ,
            response_max_hz: HOUND_RESPONSE_MAX_HZ,
        }
    }
}

/// PAN-17: is `callsign` representable in *any* FT8 wire format the encoder
/// supports? Mirrors `pancetta_ft8::encoder`'s i3=4 nonstandard-callsign
/// "exact" field constraint (a 58-bit base-38 pack: at most 11 characters
/// from `" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/"`) without pulling in a
/// runtime dependency on pancetta-ft8 (which pancetta-qso only depends on
/// optionally, for the `sim-hifi` harness). `pack28`'s standard-callsign
/// charset is a strict subset of this one, so a callsign failing this check
/// cannot be encoded by EITHER path — it genuinely can never be
/// transmitted, which is what lets [`QsoManager::check_timeouts_at`] retire
/// a QSO stuck on one immediately instead of waiting on the generic
/// timeout/watchdog timers (see the PAN-17 check there for why).
fn callsign_is_wire_representable(callsign: &str) -> bool {
    const HASH_CALL_CHARSET: &str = " 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/";
    let upper = callsign.to_ascii_uppercase();
    upper.len() <= 11 && upper.chars().all(|c| HASH_CALL_CHARSET.contains(c))
}

/// PAN-17 round 3 (Codex re-review of #248, finding 1): is `callsign`
/// *plausibly* representable via `pack28` (the i3=1 standard path — the
/// ONLY path that can carry a numeric report/ack; the i3=4 nonstandard-
/// callsign path's 2-bit report field only has room for
/// blank/RRR/RR73/73, never a real dB value — see
/// `pancetta_ft8::encoder::try_encode_nonstandard`'s doc)?
///
/// A loose mirror of `pack28`'s real acceptance shape — length 3-6 after
/// stripping a bare `/P` or `/R` suffix, and no OTHER `/` component —
/// without depending on pancetta-ft8 at runtime (see
/// `callsign_is_wire_representable`'s doc for why pancetta-qso can't call
/// `pack28` directly). Deliberately conservative in the SAFE direction:
/// this can return `true` for a callsign that would actually fail
/// `pack28`'s finer digit-position rules (e.g. `"8G81PA"` — 6 chars, no
/// other `/`, but position 3 is a digit where `pack28` requires a letter)
/// — an accepted false positive: it only costs speed (the QSO falls back
/// to the slower pre-existing generic timeout instead of this fast
/// watchdog check), never wrongly retires a QSO with a genuinely standard
/// callsign. It must NEVER return `false` for a callsign `pack28` would
/// actually accept.
fn callsign_is_plausibly_pack28_standard(callsign: &str) -> bool {
    let upper = callsign.to_ascii_uppercase();
    let base = upper
        .strip_suffix("/P")
        .or_else(|| upper.strip_suffix("/R"))
        .unwrap_or(upper.as_str());
    // Any OTHER '/' (a compound prefix, or a portable suffix other than a
    // bare /P or /R — pack28's ONLY two special-cased suffixes) is
    // definitely not pack28-representable.
    !base.contains('/') && (3..=6).contains(&base.len())
}

/// PAN-17 round 3 (Codex re-review of #248, finding 1): the DX callsign a
/// QSO's outgoing message would embed, ONLY for the rungs whose associated
/// message is a genuine numeric dB report/ack (`SignalReport`/`ReportAck`)
/// — never just a token (RRR/RR73/73) or a droppable grid. `None` for every
/// other state (nothing report-shaped to check).
///
/// `SendingReport`/`WaitingForReport` always transmit or re-send our own
/// numeric `SignalReport`/`ReportAck`. `WaitingForConfirmation` is
/// deliberately excluded (PAN-27 finding 3, round-4 review): the message it
/// currently has queued is always `FinalConfirmation` (RR73-class,
/// representable via i3=4 regardless of callsign shape) — a numeric
/// report/ack only re-enters the picture via a state REGRESSION back to
/// `SendingReport`, which changes `progress.state`'s variant itself, so the
/// `SendingReport` arm above already catches it once that happens. Checking
/// WaitingForConfirmation here as well falsely retired QSOs on the
/// skip-rung `RespondingToCq + ReportAck → WaitingForConfirmation` path,
/// which was about to complete cleanly with RR73. `SendingConfirmation` is
/// likewise excluded — it only ever sends RR73/73.
fn report_stage_partner_callsign(state: &QsoState) -> Option<&str> {
    match state {
        QsoState::SendingReport { their_callsign, .. }
        | QsoState::WaitingForReport { their_callsign, .. } => Some(their_callsign.as_str()),
        _ => None,
    }
}

/// Pick an audio offset in `[lo, hi]` deterministically from `seed` (e.g. a
/// callsign), spreading distinct seeds across the region so concurrent Hound
/// QSOs don't stack on one offset. Deterministic: same seed → same offset.
pub(crate) fn hound_offset_for(seed: &str, lo: f64, hi: f64) -> f64 {
    // Simple stable hash (FNV-1a style) → fraction of the range, snapped to 5 Hz.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in seed.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let span = (hi - lo).max(0.0);
    if span == 0.0 {
        return lo;
    }
    let frac = (h % 10_000) as f64 / 10_000.0; // [0,1)
    let raw = lo + frac * span;
    // snap to 5 Hz, clamp inside [lo, hi]
    ((raw / 5.0).round() * 5.0).clamp(lo, hi)
}

/// PAN-259: whether a Hound drops this auto-sequenced reply. The only reply a
/// Hound never sends is the terminal 73 after the Fox's RR73.
pub fn hound_suppresses_reply(reply: Option<&MessageType>) -> bool {
    matches!(reply, Some(MessageType::SeventyThree { .. }))
}

/// PAN-259: the offset a Hound answers a Fox report at. It is exactly the
/// Fox's decoded audio offset, never a rounded or hashed pick, and it is
/// admitted only when it is finite, inside the configured Hound response
/// envelope and inside the transmittable TX range. `None` means the report is
/// ignored: no state advance and no R-report anywhere else.
pub(crate) fn hound_report_offset(fox_hz: f64, regions: &HoundRegions) -> Option<f64> {
    let (lo, hi) = (regions.response_min_hz, regions.response_max_hz);
    (fox_hz.is_finite()
        && lo.is_finite()
        && hi.is_finite()
        && (lo..=hi).contains(&fox_hz)
        && (ACTIVE_QSO_TX_OFFSET_MIN_HZ..=ACTIVE_QSO_TX_OFFSET_MAX_HZ).contains(&fox_hz))
    .then_some(fox_hz)
}

/// Minimum TX audio separation (Hz) between concurrent QSOs to avoid
/// self-interference. At ≥75 Hz two FT8 signals (each ~50 Hz wide) do not
/// overlap; this matches criterion #7 in `SmartFrequencyAllocator`.
pub const MIN_TX_SEPARATION_HZ: f64 = 75.0;

/// Tight frequency-match tolerance (Hz) for INITIAL / ambiguous matching
/// (`CallingCq`, `Idle`, and any non-matching message), used by
/// [`QsoManager::is_message_relevant`], [`QsoManager::classify_relevance`],
/// and [`QsoManager::maybe_confirm_frequency_drift_at`]. Frequency tolerance
/// tightened from 50 Hz → 15 Hz to reduce cross-QSO message bleed-through in
/// multi-QSO mode: FT8 frame-to-frame drift is typically < 6 Hz on a stable
/// transceiver, so 15 Hz covers normal operation while shrinking the window
/// an attacker can exploit. (Security review 2026-04-29 C-1.)
///
/// These three sites must agree on the same tolerances — this constant (and
/// [`ESTABLISHED_FREQ_TOLERANCE_HZ`]) is the single shared definition so a
/// future tolerance change can't silently desync one site from the others
/// (PAN-15 item 6).
const FREQ_TOLERANCE_HZ: f64 = 15.0;

/// Wide frequency-match tolerance (Hz) once a QSO is ESTABLISHED (we know the
/// contra callsign and are past `CallingCq`/`Idle`) — an actively-answering
/// DX that has drifted beyond [`FREQ_TOLERANCE_HZ`] is not dropped. See
/// [`FREQ_TOLERANCE_HZ`] for the shared-definition rationale (PAN-15 item 6).
const ESTABLISHED_FREQ_TOLERANCE_HZ: f64 = 100.0;

/// How long the offset a QSO just moved away from
/// ([`crate::states::QsoMetadata::pre_switch_offset`]) stays a valid RX
/// baseline, and stays the offset a forward advance is credited to — two
/// transmit slots of the ACTIVE protocol.
///
/// PAN-72 (Codex round 4 on PR #350, finding 3). The frame that TRIGGERS a
/// relocation is emitted on the old offset by the very
/// [`QsoManager::rearm_manual_calls_at`] pass that trips the stall threshold,
/// and the coordinator's once-per-slot drain commits the move only afterwards —
/// so the last pre-switch transmission is at most one slot old when the QSO
/// changes offset, its answer occupies the next slot, and that answer is
/// decoded and processed by the end of it. Two slots covers exactly that
/// round trip.
///
/// Round 5, finding 2: the window does NOT by itself separate a reply to the
/// pre-switch frame from a reply to a post-switch one. That reasoning holds
/// only for a stall-triggered switch; an operator-forced `u` nudge is not
/// coupled to a triggering resend, so our first POST-switch transmission can go
/// out immediately and be answered comfortably inside this grace. The offset a
/// reply actually decoded at is what disambiguates the two — see the
/// `dx_frame_advanced` crediting block in `process_message_for_qso`. The
/// window's job is only to bound how long the vacated offset stays worth
/// considering at all.
///
/// Round 8 redesign (fix 1): originally a hardcoded `chrono::Duration::seconds(30)`
/// constant — "two FT8 slots" only by coincidence, since FT4's slot is 7.5 s
/// and FT2's is 3.2 s. Now derived from the SAME `active_slot_ns` atomic
/// [`rearm_slot_millis_from_ns`] already converts, so the grace window scales
/// with whichever protocol is actually running. A public free function
/// (mirroring [`rearm_slot_millis_from_ns`]) so the coordinator can derive
/// the identical value from its own copy of the `active_slot_ns` atomic —
/// one source of truth, two call sites.
pub fn pre_switch_offset_grace_from_slot_ns(slot_ns: i64) -> chrono::Duration {
    chrono::Duration::milliseconds(2 * rearm_slot_millis_from_ns(slot_ns))
}

/// The offset this QSO moved away from, if it did so recently enough for a
/// reply to the last frame we sent there to still be arriving — see
/// [`pre_switch_offset_grace_from_slot_ns`] and
/// [`crate::states::QsoMetadata::pre_switch_offset`].
fn live_pre_switch_offset_at(
    metadata: &crate::states::QsoMetadata,
    now: DateTime<Utc>,
    grace: chrono::Duration,
) -> Option<crate::states::PreSwitchOffset> {
    metadata
        .pre_switch_offset
        .filter(|pre| now.signed_duration_since(pre.left_at) <= grace)
}

/// FIX B (one-QSO-per-(callsign,band) close idempotency,
/// docs/qso-tx-deep-review-2026-07-18.md): how long after a manual QSO
/// COMPLETES a subsequent close-step (`Rr73`/`SeventyThree`) reply for the
/// same (callsign, band) is treated as "the operator is still trying to
/// close out the same contact" (re-key the existing QSO's last frame) vs. a
/// genuinely new contact (open a fresh QSO). See
/// [`QsoManager::find_recently_completed_manual_qso_for`]. Shared with the
/// coordinator's drop-stale-TX purge delay
/// (`pancetta::coordinator::qso`'s `completed_tx_grace`, formerly a separate
/// hardcoded 45s literal) so the two windows can never drift apart: it would
/// be incoherent for the QSO engine to still treat a completed QSO as
/// "reworkable" after the coordinator has already purged its TX liveness, or
/// vice versa.
pub const COMPLETED_QSO_REWORK_GRACE: chrono::Duration = chrono::Duration::seconds(45);

/// The rearm/stall cadence used when no slot-length source was ever injected:
/// FT8's historical 15-second transmit slot. See
/// [`QsoManager::set_active_slot_ns_source`].
pub const DEFAULT_SLOT_NS: i64 = 15_000_000_000;

/// The late-start TX window used when no station config was injected:
/// `[station] tx_late_max_ms`'s documented default (3000 ms, see
/// `pancetta-config`'s `default_tx_late_max_ms`). The coordinator injects
/// the operator's real value via [`QsoManager::set_tx_late_max_ms`].
pub const DEFAULT_TX_LATE_MAX_MS: u64 = 3000;

/// One transmit slot in milliseconds, converted from the coordinator's
/// `active_slot_ns` atomic (PAN-72; Codex round 7 on PR #350, finding 5, plus
/// the round-7 re-review's staleness fix).
///
/// [`QsoManager::rearm_manual_calls_at`] re-emits at most one frame per slot
/// and — since PAN-72 — counts silent cycles against
/// [`crate::states::QsoMetadata::stall_cycles`]. The re-send cadence is a
/// *slot* quantity, but the stall count is not (PAN-128): a QSO transmits
/// only on its own parity, so only the first emission feeding each
/// own-parity TX slot counts — one count per real TX opportunity, every
/// other slot. `TimeoutConfig::qso_stall_switch_after` is documented in
/// those own TX slots. The gate was a hardcoded 15 s, which is only FT8's
/// period — under FT4 a threshold of 4 took ~60 s (8 real slots) instead
/// of 4, and under FT2 far worse, delaying adaptive recovery until the
/// ordinary QSO watchdog had already won.
///
/// The input is the LIVE slot length the coordinator derives from the active
/// protocol (`pancetta_ft8::Protocol::slot_ns()`, i.e. `cycle_duration * 1e9`:
/// FT8 → 15e9, FT4 → 7.5e9, FT2 → 3.2e9) and rewrites on every runtime mode
/// switch. Deriving it from a per-mode lookup table keyed on the *cloned*
/// `config.active_mode` string was the bug this replaced: see
/// [`QsoManager::set_active_slot_ns_source`].
///
/// A non-positive or sub-millisecond value (never produced in practice) falls
/// back to [`DEFAULT_SLOT_NS`]'s 15 s rather than to a shorter cadence, so a
/// bogus atomic can never make the stall detector fire *faster* than FT8.
pub fn rearm_slot_millis_from_ns(slot_ns: i64) -> i64 {
    let millis = slot_ns / 1_000_000;
    if millis <= 0 {
        DEFAULT_SLOT_NS / 1_000_000
    } else {
        millis
    }
}

/// The TX worker's base coalesce-collection window at FT8 (its
/// `COALESCE_COLLECT_WINDOW_MS`), mode-scaled like there. Mirrored for
/// [`keep_call_acceptance_cap_ms`]; `pancetta-qso` cannot depend on the
/// `pancetta` crate that owns the original.
const TX_WORKER_COALESCE_COLLECT_WINDOW_MS: u64 = 800;

/// The TX worker's fixed (unscaled) coalesce safety margin (its
/// `COALESCE_CAP_SAFETY_MARGIN_MS`). Mirrored for
/// [`keep_call_acceptance_cap_ms`].
const TX_WORKER_COALESCE_CAP_SAFETY_MARGIN_MS: u64 = 500;

/// Mirror of the TX worker's `current_slot_acceptance_cap_ms` (PAN-117),
/// the cap `schedule_tx` decides current-slot acceptance against for a
/// single `TransmitRequest` — which every keep-call re-send is: the
/// mode-scaled `tx_late_max_ms`, minus the mode-scaled base coalesce
/// window, minus the fixed safety margin, saturating at 0. Each term is
/// scaled and rounded separately, exactly as the worker's
/// `tx_late_max_ms_effective` / `coalesce_collect_window_ms` do, from the
/// live slot length (FT8 15 s → ratio 1, FT4 1/2, FT2 ~0.213) instead of a
/// second protocol source. FT8 at the 3000 ms default → 1700 ms; FT4 →
/// 600 ms; FT2 → 0 ms.
fn keep_call_acceptance_cap_ms(slot_millis: i64, tx_late_max_ms: u64) -> i64 {
    let ratio = slot_millis as f64 / 15_000.0;
    let scale = |ms: u64| ((ms as f64) * ratio).round() as u64;
    scale(tx_late_max_ms)
        .saturating_sub(scale(TX_WORKER_COALESCE_COLLECT_WINDOW_MS))
        .saturating_sub(TX_WORKER_COALESCE_CAP_SAFETY_MARGIN_MS) as i64
}

/// The own-parity TX slot a keep-call rearm emission at `now` feeds
/// (PAN-128), derived with the TX scheduler's own current-slot
/// acceptance rule (`pancetta::coordinator::tx::schedule_tx`): the
/// CURRENT slot when its parity matches the QSO's latched
/// `QsoMetadata::tx_parity` and `now` is still inside the worker's
/// queue-time acceptance window ([`keep_call_acceptance_cap_ms`]);
/// otherwise the first own-parity slot STRICTLY after `now`.
///
/// The result is PROVISIONAL (PAN-223): the TX worker decides the real
/// target slot from the instant it dequeues the request, and reports it
/// (`QsoMessage::TxFrameScheduled`) once the schedule is final;
/// [`QsoManager::record_scheduled_tx_slot`] then re-keys the attempt to that
/// slot. An emission inside the acceptance cap that reaches the worker after
/// it, or waits in its queue, is deferred to the next own slot, which no
/// prediction made at emission time can know.
///
/// A QSO is parity-constrained and only keys on slots matching its latched
/// `QsoMetadata::tx_parity` — every OTHER protocol slot (every 30 s under
/// FT8) — while `rearm_manual_calls_at` re-emits once per protocol slot.
/// The stall counter therefore cannot count emissions (it would run at
/// twice the real on-air cadence and trip
/// `TimeoutConfig::qso_stall_switch_after` after roughly half the
/// unanswered transmissions the operator configured); it counts the TX
/// slots those emissions feed, via this anchor:
///
/// - `tx_parity = Some(p)`, current slot viable (parity match + inside the
///   late window): the current slot's start — the same slot
///   `schedule_tx` would key the frame in.
/// - `tx_parity = Some(p)`, otherwise:
///   `pancetta_core::slot::next_slot_with_parity_with_period(now, p, slot_ns)`
///   — the period-aware form the parity latch and the TX scheduler's
///   `resolve_required_parity` already use.
/// - `tx_parity = None` (never left unresolved by `QsoManager`'s own
///   creation paths — a defensive fallback): every slot is its own TX slot,
///   so the next slot of either parity, matching per-slot counting.
///
/// The current-slot acceptance is what keeps one real TX opportunity at one
/// count (PAN-128 PR review): a rearm in the off-parity slot before own
/// slot S and a rearm inside S's late-start window are BOTH scheduled into
/// S — the scheduler keys the in-window request in the current slot — so
/// they must share S as their ledger key. Attributing the in-window emission
/// to S's successor instead gave the two emissions different keys and both
/// incremented, booking a slot's count a full TX period before it could air
/// and firing the switch one transmission before the configured threshold.
/// Past the window the worker defers the frame to the next own slot, so the
/// strictly-after anchor applies there — an own-slot emission too late to
/// key shares its successor's slot with that successor's own off-parity
/// predecessor. The window is the TX worker's QUEUE-TIME acceptance cap
/// (PAN-117's `current_slot_acceptance_cap_ms`: the mode-scaled
/// `tx_late_max_ms` minus the mode-scaled base coalesce window minus the
/// fixed safety margin — 1700 ms for FT8 at the 3000 ms default), which is
/// what `schedule_tx` is actually called with for a keep-call
/// `TransmitRequest`. It should match exactly, in both directions, so the
/// provisional key is already right whenever the worker dequeues the
/// emission promptly and the re-key changes nothing: a WIDER
/// ledger window tags an emission the worker defers (say 2.0 s into own
/// slot S) with S, while its off-parity successor feeding the same deferred
/// slot gets S's successor, so the first emission after a reset or advance
/// double-counts one TX opportunity (Codex round 2 on PR #477); a NARROWER
/// one splits the in-window pair the same way.
///
/// Known approximation (documented in the PAN-128 plan): a runtime mode
/// switch or the one-time `tx_parity_provisional` correction changes the
/// values this returns, which can add at most one extra count at the
/// boundary.
fn keep_call_tx_slot(
    now: DateTime<Utc>,
    slot_millis: i64,
    tx_late_max_ms: u64,
    tx_parity: Option<pancetta_core::slot::SlotParity>,
) -> DateTime<Utc> {
    let slot_ns = slot_millis * 1_000_000;
    match tx_parity {
        Some(p) => {
            // `schedule_tx`'s rule: the current slot is the target iff its
            // parity matches AND we haven't burned past the queue-time
            // acceptance cap the worker passes it.
            let cur_start = pancetta_core::slot::current_slot_start_with_period(now, slot_ns);
            if pancetta_core::slot::SlotParity::of_with_period(cur_start, slot_ns) == p {
                let mstr_in_cur_slot = (now - cur_start).num_milliseconds().max(0);
                if mstr_in_cur_slot <= keep_call_acceptance_cap_ms(slot_millis, tx_late_max_ms) {
                    return cur_start;
                }
            }
            pancetta_core::slot::next_slot_with_parity_with_period(now, p, slot_ns)
        }
        None => pancetta_core::slot::next_slot_start_with_period(now, Duration::zero(), slot_ns),
    }
}

/// Nudge a candidate TX audio offset away from already-occupied offsets so
/// concurrent QSOs don't stack. Returns `candidate` unchanged if it is within
/// `[lo, hi]` AND at least `min_sep` Hz from every offset in `occupied`.
/// Otherwise searches outward from `candidate` in 25 Hz steps for the nearest
/// in-range offset that is `>= min_sep` from all `occupied`; if none exists in
/// range, returns `candidate.clamp(lo, hi)`. Deterministic (no RNG) — for tests
/// and reproducibility.
pub fn deconflict_offset(candidate: f64, occupied: &[f64], min_sep: f64, lo: f64, hi: f64) -> f64 {
    let clear = |off: f64| occupied.iter().all(|o| (off - o).abs() >= min_sep);
    let c = candidate.clamp(lo, hi);
    if clear(c) {
        return c;
    }
    // Search outward in 25 Hz steps: candidate±25, ±50, ... preferring the
    // closer side; only accept in-range offsets that clear all occupied.
    let step = 25.0;
    let max_steps = (((hi - lo) / step).ceil() as i64).max(1);
    for k in 1..=max_steps {
        let d = k as f64 * step;
        for cand in [c - d, c + d] {
            if cand >= lo && cand <= hi && clear(cand) {
                return cand;
            }
        }
    }
    c // no clear slot in range — fall back to the clamped candidate
}

/// The dial the station actually transmits on: the split TX dial when split is
/// active (`split_tx_hz != 0`), otherwise the RX dial. Used to stamp the logged
/// RF frequency of a completed QSO (dial + audio offset).
pub fn effective_tx_dial(rx_dial_hz: u64, split_tx_hz: u64) -> u64 {
    if split_tx_hz != 0 {
        split_tx_hz
    } else {
        rx_dial_hz
    }
}

/// QSO management errors
#[derive(Debug, Error)]
pub enum QsoManagerError {
    #[error("QSO not found: {qso_id}")]
    QsoNotFound { qso_id: QsoId },

    #[error("Invalid state transition from {from:?} to {to:?}")]
    InvalidTransition { from: QsoState, to: QsoState },

    #[error("QSO already exists for callsign {callsign} on frequency {frequency}")]
    DuplicateQso { callsign: String, frequency: f64 },

    #[error("Invalid callsign format: {callsign}")]
    InvalidCallsign { callsign: String },

    #[error("QSO timeout: {reason}")]
    Timeout { reason: String },

    #[error("Configuration error: {message}")]
    Configuration { message: String },

    #[error("Database error: {source}")]
    Database { source: anyhow::Error },

    #[error("{message}")]
    Internal { message: String },

    /// PAN-72 (Codex round 1 on PR #350, finding 3): a queued TX-offset
    /// action was drained after the QSO went terminal. Completed QSOs are
    /// retained in the map (and deliberately kept in the coordinator's active
    /// snapshots for a 45-second trailing-73 grace window), so the lookup
    /// still succeeds — but nothing may be mutated. Expected, not a fault.
    #[error("QSO {qso_id} is no longer active — TX-offset action discarded")]
    QsoNotActive { qso_id: QsoId },

    /// PAN-72 (Codex round 1 on PR #350, finding 8): a queued TX-offset
    /// action was drained after the QSO made forward progress, so the offset
    /// it wanted to move away from is the one that just demonstrably worked.
    /// Expected (the drain runs only once per 15-second slot), not a fault.
    #[error(
        "TX-offset action for QSO {qso_id} is stale \
         (raised at advance generation {raised_at}, QSO is now at {current})"
    )]
    OffsetActionStale {
        qso_id: QsoId,
        raised_at: u32,
        current: u32,
    },

    /// PAN-79 / PAN-72 (Codex round 3 on PR #350, finding 2): the resolved
    /// offset is the one the QSO is already on, so there is nothing to
    /// relocate. The allocator returns `avoid_hz` unchanged when every
    /// candidate is excluded (its documented "no valid relocation" signal) and
    /// the drain passes that straight through; committing it as a move would
    /// clear valid accumulated `stall_cycles` evidence and announce a
    /// `TxOffsetApplied` for a move that never happened. Expected, not a fault.
    #[error(
        "TX-offset action for QSO {qso_id} resolves to its current offset \
         ({offset_hz:.0} Hz) — nothing to relocate"
    )]
    OffsetActionNoOp { qso_id: QsoId, offset_hz: f64 },

    /// PAN-72 (Codex round 4 on PR #350, finding 2): the resolved offset falls
    /// outside the Hound region this QSO is procedurally pinned to *right now*.
    /// The coordinator resolves against a `get_qso` snapshot taken before the
    /// allocator runs, and the Fox's first report performs the mandatory
    /// calling-region → response-region QSY; if that lands in between, the
    /// resolved (low, calling-region) offset would undo the QSY and put our
    /// R+report where the Fox is not listening. Revalidated under the same
    /// write lock that performs the mutation, so check and commit are atomic.
    /// Expected, not a fault — the stall detector re-raises against the new
    /// region on its own.
    #[error(
        "TX-offset action for QSO {qso_id} resolves to {offset_hz:.0} Hz, \
         outside the Hound region it is now pinned to ({min_hz:.0}-{max_hz:.0} Hz)"
    )]
    OffsetActionOutsideHoundRegion {
        qso_id: QsoId,
        offset_hz: f64,
        min_hz: f64,
        max_hz: f64,
    },

    /// PAN-72 (Codex round 10, thread on `autonomous.rs:988`): the operator
    /// switched TX-frequency mode to Hold AFTER a queued Switch/Revert was
    /// raised but BEFORE this commit's write lock was acquired. The caller's
    /// own Hold check (batch-level and per-request, in
    /// `drain_pending_qso_offset_requests`) runs before the `.await` for the
    /// lock, leaving a window in which the mode can flip underneath it.
    /// Re-checked here, inside the same locked section that would otherwise
    /// commit, so the two can never disagree. Expected, not a fault — Hold's
    /// promise is that autonomous offset changes are suppressed, and this is
    /// exactly that promise being kept a moment later than the caller alone
    /// could guarantee.
    #[error("TX-offset action for QSO {qso_id} refused — TX-frequency mode is Hold")]
    OffsetActionHeld { qso_id: QsoId },

    /// PAN-140 (Codex P1 on PR #369): a PTT keyed for the current slot
    /// AFTER the caller's own pre-commit check (`drain_pending_qso_offset_requests`'s
    /// top-of-loop guard) but BEFORE this method's write lock was acquired —
    /// the exact same shape of window `OffsetActionHeld` above closes for
    /// Hold mode, here closed for PTT. Re-checked here, inside the same
    /// locked section that would otherwise commit, so a frequency move can
    /// never land mid-transmission regardless of how the caller's own check
    /// raced. Expected, not a fault — the caller re-queues the action for
    /// its next drain tick rather than discarding it.
    #[error(
        "TX-offset action for QSO {qso_id} refused — a PTT is already keyed for the current slot"
    )]
    OffsetActionPttInFlight { qso_id: QsoId },

    /// PAN-75: the rearm that raised this stall action was later reported
    /// unsent, and rolling it back left `stall_cycles` below
    /// `qso_stall_switch_after`. The threshold the action depended on is no
    /// longer met, so committing it would move the QSO on the strength of a
    /// frame that never aired. Expected, not a fault — the detector re-raises
    /// once the streak rebuilds from frames that do air.
    #[error(
        "TX-offset action for QSO {qso_id} is stale — the stall attempt that \
         raised it was rolled back as unsent"
    )]
    OffsetActionStallRolledBack { qso_id: QsoId },

    /// PAN-182: the frame whose emission raised this stall action has not
    /// been reported by the TX worker yet — neither aired
    /// ([`QsoManager::confirm_stall_trigger_aired`]) nor unsent (the PAN-75
    /// rollback). Committing now could move the QSO on the strength of a
    /// frame that never aired, and a later not-sent report could no longer
    /// undo the move. Expected, not a fault — the caller re-queues the action
    /// for its next drain tick rather than discarding it.
    #[error(
        "TX-offset action for QSO {qso_id} deferred — the stall attempt that \
         raised it has not been reported aired or unsent yet"
    )]
    OffsetActionTriggerOutcomePending { qso_id: QsoId },

    /// PAN-182: the stall action's trigger frame went unreported for
    /// `STALL_TRIGGER_HOLD_SLOTS` slots (a frame dropped with no report, e.g.
    /// on shutdown). The action is dropped rather than committed on unknown
    /// evidence; the streak is untouched, so the detector's next raise arms a
    /// fresh hold. Expected, not a fault.
    #[error(
        "TX-offset action for QSO {qso_id} dropped — the stall attempt that \
         raised it was never reported aired or unsent"
    )]
    OffsetActionTriggerOutcomeExpired { qso_id: QsoId },

    /// PAN-131 (Codex round 13 on PR #350, P2): an operator `u` nudge was
    /// queued while its QSO sat in a nudge-eligible state, but the DX moved
    /// the QSO on before the once-per-slot drain committed it — into a state
    /// neither `rearm_manual_calls_at`'s cadence nor the commit's own one-shot
    /// retransmission sends anything from (see [`operator_nudge_retransmits`]).
    /// Committing would move the offset with nothing ever sent on it. Expected,
    /// not a fault — the DX's progress supersedes the nudge, exactly as a DX
    /// advance supersedes a stall action through `OffsetActionStale`.
    #[error(
        "operator TX-offset nudge for QSO {qso_id} superseded — the QSO \
         advanced to a state no retransmission covers"
    )]
    OffsetActionNudgeSuperseded { qso_id: QsoId },
}

impl QsoManagerError {
    /// PAN-72: is this an *expected* refusal of a queued TX-offset action
    /// (the QSO finished, or advanced, between the action being raised and
    /// the once-per-slot drain committing it, or the allocator found no
    /// candidate to relocate to at all) rather than a real fault? The
    /// coordinator's drain logs these at `debug!`/`info!` instead of `warn!`.
    pub fn is_expected_offset_action_refusal(&self) -> bool {
        matches!(
            self,
            QsoManagerError::QsoNotFound { .. }
                | QsoManagerError::QsoNotActive { .. }
                | QsoManagerError::OffsetActionStale { .. }
                | QsoManagerError::OffsetActionNoOp { .. }
                | QsoManagerError::OffsetActionOutsideHoundRegion { .. }
                | QsoManagerError::OffsetActionHeld { .. }
                | QsoManagerError::OffsetActionPttInFlight { .. }
                | QsoManagerError::OffsetActionStallRolledBack { .. }
                | QsoManagerError::OffsetActionTriggerOutcomePending { .. }
                | QsoManagerError::OffsetActionTriggerOutcomeExpired { .. }
                | QsoManagerError::OffsetActionNudgeSuperseded { .. }
        )
    }
}

/// The Hz window a TX-offset action for this QSO is allowed to resolve inside,
/// or `None` for the general allocation range.
///
/// PAN-72 (Codex round 2 on PR #350, finding 1; hoisted here in round 4,
/// finding 2). A **Hound**'s TX offset is procedurally pinned, not free: while
/// calling the Fox we must sit in the low calling region, and once the Fox has
/// answered and we have QSY'd we must sit in the response region — that is
/// where the Fox is listening, and nowhere else. The generic allocator knows
/// nothing about either region, so a stall switch resolved through it could
/// move a post-QSY Hound back down into the calling region and guarantee the
/// QSO dies.
///
/// [`QsoMetadata::hound_qsyed`](crate::states::QsoMetadata::hound_qsyed) is the
/// discriminator rather than the QSO's state, because it is exactly the flag
/// the QSY itself sets (`process_message_for_qso`'s Hound block): before it
/// flips we are calling in the calling region, after it flips we are pinned to
/// the Fox's own offset (PAN-259). Non-Hound QSOs are unconstrained.
///
/// Lives in this crate, not in the coordinator, so the constraint the
/// coordinator *resolves* against and the one
/// [`QsoManager::apply_tx_offset_switch`] *revalidates* at commit time are one
/// definition rather than two that can drift apart.
///
/// The window is sanitized rather than trusted: the bounds ultimately come from
/// operator TOML (`[hound]`), so a non-finite or inverted window degrades to
/// "no constraint" instead of filtering every candidate away (or, at the commit
/// end, refusing every action).
pub fn hound_switch_range_hz(
    progress: &crate::states::QsoProgress,
    config: &QsoManagerConfig,
) -> Option<(f64, f64)> {
    hound_switch_range_for(&progress.metadata, config)
}

/// [`hound_switch_range_hz`] against a bare metadata borrow — what
/// [`QsoManager::apply_tx_offset_switch`] has in hand while it holds the write
/// lock on the QSO it is about to mutate.
fn hound_switch_range_for(
    metadata: &crate::states::QsoMetadata,
    config: &QsoManagerConfig,
) -> Option<(f64, f64)> {
    if metadata.fox {
        // PAN-386: a Fox-role stream owns one 60 Hz slot in 300-900 Hz for
        // its whole life, so its switch window is that single point. A stall
        // switch, revert or `u` nudge to any other offset is refused at
        // commit, exactly as a QSY'd Hound's is.
        let slot = metadata.frequency;
        return slot.is_finite().then_some((slot, slot));
    }
    if !metadata.hound {
        return None;
    }
    let (lo, hi) = if metadata.hound_qsyed {
        // PAN-259: once the Fox has sent us a report we answer at the Fox's
        // own offset and nowhere else, so the post-report window is a single
        // point. Every stall switch, revert or `u` nudge then resolves to the
        // allocator's exhaustion tail (`avoid_hz` unchanged) and is refused as
        // a no-op at commit, while the report-driven move itself stays in
        // `process_message_for_qso`.
        let fox_hz = metadata.partner_freq.unwrap_or(metadata.frequency);
        (fox_hz, fox_hz)
    } else {
        (config.hound.call_min_hz, config.hound.call_max_hz)
    };
    (lo.is_finite() && hi.is_finite() && lo <= hi).then_some((lo, hi))
}

/// QSO manager configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QsoManagerConfig {
    /// Our station callsign
    pub our_callsign: String,

    /// Our grid square
    pub our_grid: Option<GridSquare>,

    /// Timeout settings
    pub timeouts: TimeoutConfig,

    /// Contest mode settings
    pub contest_mode: Option<ContestConfig>,

    /// PAN-50 contest-scoring context: the validated `[contest]` TOML
    /// section converted by the coordinator at startup. `None` (default)
    /// disables contest scoring entirely. Unrelated to the dormant
    /// `contest_mode` serial-exchange scaffolding above, which is NOT
    /// reused (see the PAN-50 plan's implementation approach).
    #[serde(default)]
    pub contest_scoring: Option<crate::contest::scoring::ContestScoringContext>,

    /// Automatic sequencing configuration
    pub auto_sequence: AutoSequenceConfig,

    /// Duplicate checking settings
    pub duplicate_checking: DuplicateCheckConfig,

    /// Hound (DXpedition chaser) audio-offset regions.
    ///
    /// Controls the calling region (low) and the post-QSY response region (high).
    /// Defaults to 300–900 Hz (call) / 1000–2700 Hz (response), matching the
    /// WSJT-X Fox/Hound conventions.  Populate from `pancetta_config::HoundConfig`
    /// in the coordinator to honour operator-set ranges.
    #[serde(default)]
    pub hound: HoundRegions,

    /// Station-wide active operating mode string (`"FT8"`, `"FT4"`, or
    /// `"FT2"`), stamped into every [`QsoMetadata::mode`] this manager
    /// creates and thus into the ADIF `MODE` field of logged QSOs. FT8 is a
    /// station-global mode (not per-decode); the coordinator populates this
    /// from `[rig].mode`. Defaults to `"FT8"` so the legacy path is
    /// byte-identical.
    #[serde(default = "default_active_mode")]
    pub active_mode: String,
}

/// Default value for [`QsoManagerConfig::active_mode`]: `"FT8"`.
fn default_active_mode() -> String {
    "FT8".to_string()
}

/// Timeout configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutConfig {
    /// Timeout for CQ calls (seconds)
    pub cq_timeout: u64,

    /// Timeout for waiting for report (seconds)
    pub report_timeout: u64,

    /// Timeout for waiting for confirmation (seconds)
    pub confirmation_timeout: u64,

    /// Maximum QSO duration (seconds)
    pub max_qso_duration: u64,

    /// Cleanup interval for completed QSOs (seconds)
    pub cleanup_interval: u64,

    /// Manual keep-calling watchdog: stop calling after this many minutes
    /// have elapsed since the first manual call, regardless of call count.
    /// Whichever of this and `manual_call_max_calls` fires first ends the
    /// manual call attempt. Default: 5 minutes.
    pub manual_call_watchdog_minutes: u64,

    /// Manual keep-calling watchdog: stop after transmitting this many
    /// calls to the DX. Whichever of this and `manual_call_watchdog_minutes`
    /// fires first ends the manual call attempt. Default: 25 — high enough that
    /// the 5-minute time watchdog is the governing limit (operator wants ~5 min
    /// of calling, not the old ~2.5 min the 10-call cap produced); it remains a
    /// safety backstop. Applies only to the INITIAL call states.
    pub manual_call_max_calls: u32,

    /// Repetitive-TX watchdog (seconds). If a QSO sits in the SAME active TX
    /// state this long — i.e. we have been re-sending the same message without
    /// the DX advancing us — the QSO is retired as a timeout. Bounds "stuck
    /// sending the same thing" for BOTH manual and auto QSOs, independent of
    /// (and tighter than) the manual keep-call watchdog. Default: 300 s.
    /// (Raised from 120 s on operator request — we were giving up on calling a
    /// non-answering DX too quickly. The 5-min keep-call watchdog now governs.)
    #[serde(default = "default_repetitive_tx_timeout_secs")]
    pub repetitive_tx_timeout_secs: u64,

    /// Consecutive stalled cycles (see [`crate::states::QsoMetadata::
    /// stall_cycles`]) before an in-progress QSO's TX offset is switched (or
    /// reverted to its last known-good offset) — Auto TX-freq mode only.
    /// Counted in the QSO's own-parity transmit slots — one every other
    /// protocol slot (every 30 s under FT8, every 15 s under FT4), i.e. one
    /// per real transmit opportunity (PAN-128). Default 4 (PAN-72). Distinct
    /// from `AutonomousConfig::cq_no_response_switch_after` (a different
    /// struct, governs the self-CQ-hunting case before any QSO exists),
    /// though the two are exposed under the same `[autonomous]` TOML section
    /// by the coordinator (see `pancetta-config`) since they're one logical
    /// "adaptive TX behavior" knob from the operator's point of view.
    #[serde(default = "default_qso_stall_switch_after")]
    pub qso_stall_switch_after: u32,
}

/// Default for [`TimeoutConfig::repetitive_tx_timeout_secs`] (5 minutes).
fn default_repetitive_tx_timeout_secs() -> u64 {
    300
}

/// Default for [`TimeoutConfig::qso_stall_switch_after`] (PAN-72).
fn default_qso_stall_switch_after() -> u32 {
    4
}

/// Contest configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContestConfig {
    /// Contest name
    pub contest_name: String,

    /// Contest category
    pub category: String,

    /// Starting serial number
    pub starting_serial: SerialNumber,

    /// Enable contest mode
    pub enabled: bool,
}

/// Automatic sequencing configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoSequenceConfig {
    /// Enable automatic sequencing
    pub enabled: bool,

    /// Automatically respond to CQ calls
    pub auto_respond_cq: bool,

    /// Automatically send reports
    pub auto_send_reports: bool,

    /// Automatically send confirmations
    pub auto_send_confirmations: bool,

    /// Delay between automatic actions (milliseconds)
    pub action_delay_ms: u64,
}

/// Duplicate checking configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateCheckConfig {
    /// Enable duplicate checking
    pub enabled: bool,

    /// Check duplicates within this time window (hours)
    pub time_window_hours: u32,

    /// When true, scope duplicates to the same band, regardless of audio offset
    pub check_frequency: bool,

    /// Reserved compatibility field; check_frequency controls band scoping.
    pub check_band: bool,
}

/// What an in-progress QSO's stall-detection (PAN-72) wants done about its
/// TX offset. Emitted by `QsoManager` as `QsoEvent::TxOffsetActionNeeded`;
/// resolved and committed by the coordinator (see
/// `docs/superpowers/specs/2026-09-04-pan-72-adaptive-tx-offset-design.md`)
/// since only `AutonomousOperator` may call the smart allocator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OffsetAction {
    /// We're on the last known-good offset (or none is recorded yet) — find
    /// a new one, avoiding this one. The coordinator's drain avoids the QSO's
    /// live `metadata.frequency` whenever the QSO still exists; this value is
    /// only the fallback (PAN-130).
    Switch { avoid_hz: f64 },
    /// We stalled again on a previously-switched offset — go back to the one
    /// that was last confirmed working, no allocator call needed.
    Revert { target_hz: f64 },
}

/// Who asked for a TX-offset relocation — and, for the stall detector, the
/// staleness token that guards its request at commit time.
///
/// PAN-72. Two separate consumers read this, which is why the provenance is
/// modelled explicitly rather than inferred from the presence of a token:
///
/// - [`QsoManager::apply_tx_offset_switch`]'s staleness guard (round 1, finding
///   8) discards a `StallDetected` request whose
///   [`crate::states::QsoMetadata::advance_generation`] has moved since it was
///   raised. An `OperatorForced` nudge carries no token and is never
///   generation-checked, but a DX advance out of the nudge-eligible states
///   still supersedes it at commit time (guard 2a, PAN-131);
/// - the known-good crediting site in `process_message_for_qso` (round 6,
///   finding 2) needs to know whether anything actually indicted the offset we
///   left. A stall switch happens BECAUSE the old offset stopped working; an
///   operator nudge implies nothing about it — since PAN-83 only as the
///   fallback when no aired frame answers it. See
///   [`crate::states::PreSwitchOffset::operator_forced`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OffsetRelocationOrigin {
    /// Raised by the silence-driven stall detector, carrying the QSO's
    /// `advance_generation` as it stood under the same write lock that raised
    /// the action.
    StallDetected { raised_at_generation: u32 },
    /// Forced by the operator's `u` nudge.
    OperatorForced,
}

impl OffsetRelocationOrigin {
    /// The staleness token to validate at commit time, if this origin has one.
    pub fn raised_at_generation(&self) -> Option<u32> {
        match self {
            Self::StallDetected {
                raised_at_generation,
            } => Some(*raised_at_generation),
            Self::OperatorForced => None,
        }
    }

    /// `true` for the operator's `u` nudge.
    pub fn is_operator_forced(&self) -> bool {
        matches!(self, Self::OperatorForced)
    }
}

/// PAN-72 (Codex round 1 on PR #350, finding 8): one queued TX-offset action
/// plus the origin that guards and explains it at commit time.
///
/// This is the element type of the coordinator's `pending_qso_offset_requests`
/// mailbox. It lives here, next to [`OffsetAction`], because
/// [`QsoManager::apply_tx_offset_switch`] is what actually consumes the
/// origin — the coordinator only carries it across the once-per-slot gap
/// between the QSO event firing and the autonomous tick draining it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OffsetActionRequest {
    pub qso_id: QsoId,
    pub action: OffsetAction,
    /// What asked for this relocation — see [`OffsetRelocationOrigin`].
    pub origin: OffsetRelocationOrigin,
}

impl OffsetActionRequest {
    /// A stall-detected action, guarded by the QSO's advance generation.
    pub fn stall_detected(qso_id: QsoId, action: OffsetAction, raised_at_generation: u32) -> Self {
        Self {
            qso_id,
            action,
            origin: OffsetRelocationOrigin::StallDetected {
                raised_at_generation,
            },
        }
    }

    /// An operator-forced action (the `u` nudge) — never generation-checked;
    /// see [`QsoManager::apply_tx_offset_switch`]'s guard 2a.
    pub fn operator_forced(qso_id: QsoId, action: OffsetAction) -> Self {
        Self {
            qso_id,
            action,
            origin: OffsetRelocationOrigin::OperatorForced,
        }
    }
}

/// PAN-131 (Codex round 13 on PR #350, P2): does `progress`'s CURRENT state
/// still get a frame sent at a new offset if an operator `u` nudge commits
/// against it now?
///
/// [`QsoManager::apply_tx_offset_switch`]'s guard 2a reads this at commit time,
/// because an `OperatorForced` origin carries no generation token and guard 2
/// cannot see a DX advance that happened after the keypress. Each `true` arm
/// names the mechanism that retransmits after the commit:
///
/// - `CallingCq` (any initiation): `rearm_manual_calls_at`'s keep-calling
///   cadence for a Manual CQ; the commit's own one-shot `cq_retransmit` for an
///   Auto self-CQ (round 10, Fix A), which that cadence never re-sends.
/// - `WaitingForReport` (Manual only): `rearm_manual_calls_at` re-sends the
///   latched report (SM-F4). An Auto self-CQ that a caller answered also sits
///   here, but neither mechanism sends anything for it, so it is `false`.
/// - `RespondingToCq` / `SendingReport` (any initiation):
///   `rearm_manual_calls_at` below its call cap, and `cq_retransmit` for an
///   Auto QSO at or past `AUTO_RESEND_MAX_CALLS` (round 14).
///
/// No cap logic, matching keypress-time selection: a Manual QSO past
/// `manual_call_max_calls` is watchdog-retirement territory. Every later state
/// (`WaitingForConfirmation`, `SendingConfirmation`, contest exchange, …) falls
/// to both mechanisms' `_` arm.
///
/// Must stay equal to the selection-time union in
/// `pancetta/src/coordinator/tui_relay.rs` (`qso_state_is_rearm_eligible` ∪
/// `auto_calling_cq_ids`). If the two drift, the commit refuses a nudge the
/// TUI accepted — the safe direction.
pub fn operator_nudge_retransmits(progress: &QsoProgress) -> bool {
    let is_manual = progress.metadata.initiated_by == CallInitiation::Manual;
    match &progress.state {
        QsoState::CallingCq { .. } => true,
        QsoState::WaitingForReport { .. } => is_manual,
        QsoState::RespondingToCq { .. } | QsoState::SendingReport { .. } => true,
        _ => false,
    }
}

/// QSO event notifications
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QsoEvent {
    /// QSO state changed
    StateChanged {
        qso_id: QsoId,
        old_state: QsoState,
        new_state: QsoState,
        timestamp: DateTime<Utc>,
    },

    /// Message received
    MessageReceived { qso_id: QsoId, message: QsoMessage },

    /// Message should be sent
    MessageToSend {
        qso_id: QsoId,
        message: MessageType,
        frequency: f64,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
        /// SECURITY: `true` iff the emitting QSO's `QsoMetadata.remote_origin`
        /// is set (the QSO was initiated by a remote operator). The coordinator
        /// forwards this as `TxOrigin::Remote` so the frame is armed-TX gated.
        /// `false` for every Local / TUI / autonomous QSO.
        remote_origin: bool,
        /// Mirrors `QsoMetadata.remote_client_key_id` — the station-agent peer
        /// this QSO is bound to, iff `remote_origin`. The coordinator's arm
        /// gate compares this against the CURRENTLY-armed client so a
        /// different peer taking control later can never authorize a frame it
        /// never requested.
        remote_client_key_id: Option<String>,
        /// PAN-183: this emission's attempt id, so a failed frame rolls back
        /// exactly its own accounting. For a keep-call rearm, an operator
        /// re-send or the offset-switch one-shot retransmit it names the
        /// ledger attempt the emission recorded. Opening calls and advances
        /// record none and get a fresh id that matches no attempt, so their
        /// failure rolls nothing back, never a newer same-text attempt. The
        /// engine always sets it; `Option` matches the bus fields, where
        /// frames from outside the engine carry `None`.
        attempt_id: Option<TxAttemptId>,
    },

    /// QSO completed
    QsoCompleted {
        qso_id: QsoId,
        metadata: QsoMetadata,
        /// Full state-transition + message timeline as of completion (Layer
        /// 2 timeline persistence — see
        /// docs/observability-diagnostics-plan.md). Captured at the
        /// emission site from the live in-memory `QsoProgress` before it is
        /// eventually dropped (`cleanup_completed_qsos` removes terminal
        /// QSOs from the active map ~1h later, discarding these fields if
        /// nothing persisted them first). Consumers that don't need the
        /// timeline can destructure with `..`.
        state_history: Vec<StateTransition>,
        messages: Vec<QsoMessage>,
    },

    /// QSO failed
    QsoFailed {
        qso_id: QsoId,
        reason: QsoFailureReason,
        /// Operational state immediately before the QSO entered `Failed`.
        /// Unlike `state_history`, this is populated even when a newly opened
        /// QSO times out before its first sent/received transition.
        last_state: QsoState,
        metadata: QsoMetadata,
        /// See `QsoCompleted::state_history`.
        state_history: Vec<StateTransition>,
        messages: Vec<QsoMessage>,
    },

    /// Duplicate QSO detected
    DuplicateDetected {
        qso_id: QsoId,
        original_qso_id: QsoId,
        callsign: String,
    },

    /// A decoded frame was refused on sender-verification grounds.
    MessageRejected {
        qso_id: QsoId,
        reason: RejectionReason,
        from_callsign: Option<String>,
        to_callsign: Option<String>,
    },

    /// PAN-72: an in-progress QSO's TX offset needs an autonomous action
    /// (Auto TX-freq mode only — see `QsoManager::rearm_manual_calls_at`).
    /// The coordinator resolves `OffsetAction::Switch` via the smart
    /// allocator and commits either variant via
    /// `QsoManager::apply_tx_offset_switch`.
    ///
    /// `raised_at_generation` is the QSO's
    /// [`crate::states::QsoMetadata::advance_generation`] at emission time —
    /// carried through the coordinator's mailbox and re-checked at commit
    /// time so an action raised before a DX advance that has since landed is
    /// discarded rather than moving the QSO off the offset that just worked.
    TxOffsetActionNeeded {
        qso_id: QsoId,
        action: OffsetAction,
        raised_at_generation: u32,
    },

    /// PAN-72: a TX-offset action was actually committed by
    /// [`QsoManager::apply_tx_offset_switch`] (post-clamp value). Purely an
    /// announcement — nothing in `pancetta-qso` consumes it. The coordinator
    /// uses it to rebuild the `ActiveQsosSnapshot` the TUI banner and the
    /// remote gateway render from, which is otherwise rebuilt only on a state
    /// transition; a stalled exchange may produce none for a long time, so
    /// without this the displayed offset would lag through multiple
    /// switch/revert cycles (Codex round 1 on PR #350, finding 7).
    TxOffsetApplied { qso_id: QsoId, offset_hz: f64 },
}

/// Routing verdict plus an optional security classification.
struct Relevance {
    relevant: bool,
    reason: Option<RejectionReason>,
}

impl Default for QsoManagerConfig {
    fn default() -> Self {
        Self {
            our_callsign: "NOCALL".to_string(),
            our_grid: None,
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            contest_scoring: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
            hound: HoundRegions::default(),
            active_mode: default_active_mode(),
        }
    }
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            cq_timeout: 30,
            report_timeout: 30,
            confirmation_timeout: 30,
            max_qso_duration: 300,
            cleanup_interval: 60,
            manual_call_watchdog_minutes: 5,
            manual_call_max_calls: 25,
            repetitive_tx_timeout_secs: default_repetitive_tx_timeout_secs(),
            qso_stall_switch_after: default_qso_stall_switch_after(),
        }
    }
}

impl Default for AutoSequenceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_respond_cq: false,
            auto_send_reports: false,
            auto_send_confirmations: false,
            action_delay_ms: 1000,
        }
    }
}

impl Default for DuplicateCheckConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            time_window_hours: 24,
            check_frequency: true,
            check_band: false,
        }
    }
}

/// Predicate for "would a Remote frame bound to this client (if any) be
/// permitted to key PTT right now?" — see [`QsoManager::remote_tx_permitted`]'s
/// doc comment.
type RemoteTxPermittedSource = Arc<dyn Fn(Option<&str>) -> bool + Send + Sync>;

// A frame may wait behind an in-flight TX and a same-parity slot (~3 slots).
// Every unaired frame now reports its own attempt (PAN-279), so retention is
// only a backstop for a genuinely lost report.
const UNSENT_ATTEMPT_RETENTION_SLOTS: i64 = 4;
const MAX_UNSENT_ATTEMPTS_PER_QSO: usize = 8;
// Same tolerance the TX worker's pivot check uses for "same offset".
const UNSENT_ATTEMPT_OFFSET_EPSILON_HZ: f64 = 0.5;
// PAN-182: how long a stall action waits on its trigger frame's outcome
// before the hold expires. Matches the ledger's retention: past it, a
// not-sent report could no longer find the attempt to roll back anyway.
const STALL_TRIGGER_HOLD_SLOTS: i64 = 4;

fn stall_trigger_hold_expired(
    emitted_at: DateTime<Utc>,
    now: DateTime<Utc>,
    slot_millis: i64,
) -> bool {
    now - emitted_at >= Duration::milliseconds(STALL_TRIGGER_HOLD_SLOTS * slot_millis)
}

// The pivot-safe "same frame" test shared by the not-sent rollback
// (`KeepCallAttempt::carries`) and the aired release
// (`PendingStallTrigger::aired_as`). A `None` binding is a wildcard.
fn emission_carries(
    frequency_hz: f64,
    remote_origin: bool,
    remote_client_key_id: &Option<String>,
    payload: &UnsentAttemptPayload,
) -> bool {
    (frequency_hz - payload.frequency_hz).abs() < UNSENT_ATTEMPT_OFFSET_EPSILON_HZ
        && payload
            .remote_binding
            .as_ref()
            .is_none_or(|(origin, key_id)| {
                remote_origin == *origin && remote_client_key_id == key_id
            })
}

/// PAN-183: identifies one QSO-engine emission and, for a keep-call rearm,
/// re-send or offset-switch retransmit, the ledger attempt it recorded.
/// Carried on its `MessageToSend` and the TX request built from it, and
/// echoed back on the not-sent report so the rollback reverses that attempt
/// and no other. Never reused within a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TxAttemptId(pub u64);

#[derive(Debug, Clone)]
struct KeepCallAttempt {
    attempt_id: TxAttemptId,
    emitted_at: DateTime<Utc>,
    raw_text: String,
    prev_last_call_at: Option<DateTime<Utc>>,
    // This attempt holds its `tx_slot`'s stall increment (PAN-128): exactly
    // one surviving attempt per own-parity TX slot carries `true`, so
    // `stall_cycles` rises once per own TX slot, not once per emission.
    bumped_stall: bool,
    // The own-parity TX slot this emission feeds: the rearm's prediction
    // (`keep_call_tx_slot`) until the TX worker reports the slot it
    // scheduled the frame into, then that slot (PAN-223,
    // `record_scheduled_tx_slot`); `None` for attempts that never count
    // toward the stall streak and
    // never inherit one either (`resend_last_tx`, the offset-switch
    // retransmit, and anything a stall reset cleared — the reset retires
    // the slot key with the increment).
    tx_slot: Option<DateTime<Utc>>,
    recorded_sent: bool,
    // What the emission carried besides its text. A TX pivot can replace a
    // frame with one that differs only in offset or remote binding.
    frequency_hz: f64,
    remote_origin: bool,
    remote_client_key_id: Option<String>,
}

impl KeepCallAttempt {
    fn carries(&self, payload: &UnsentAttemptPayload) -> bool {
        emission_carries(
            self.frequency_hz,
            self.remote_origin,
            &self.remote_client_key_id,
            payload,
        )
    }
}

// PAN-182: the keep-call emission whose outcome a raised stall action is
// waiting on. `apply_tx_offset_switch` refuses a stall-origin commit while
// one is recorded; it is released when the TX worker reports the frame
// aired, re-pointed or cleared by the not-sent rollback, and cleared by
// every stall reset.
#[derive(Debug, Clone)]
struct PendingStallTrigger {
    attempt_id: TxAttemptId,
    emitted_at: DateTime<Utc>,
    // The own-parity TX slot the trigger feeds; no frame that aired before
    // it started can be the trigger. Follows its attempt's re-key to the
    // TX worker's scheduled slot (PAN-223).
    tx_slot: Option<DateTime<Utc>>,
    raw_text: String,
    frequency_hz: f64,
    remote_origin: bool,
    remote_client_key_id: Option<String>,
}

impl PendingStallTrigger {
    fn from_attempt(attempt: &KeepCallAttempt) -> Self {
        Self {
            attempt_id: attempt.attempt_id,
            emitted_at: attempt.emitted_at,
            tx_slot: attempt.tx_slot,
            raw_text: attempt.raw_text.clone(),
            frequency_hz: attempt.frequency_hz,
            remote_origin: attempt.remote_origin,
            remote_client_key_id: attempt.remote_client_key_id.clone(),
        }
    }

    // PAN-183: by identity. A same-text sibling stamped at the same instant
    // is a different attempt.
    fn is_attempt(&self, attempt: &KeepCallAttempt) -> bool {
        self.attempt_id == attempt.attempt_id
    }

    // Matches on the aired frame's offset and binding, not text alone: a
    // pivot can air a same-text frame elsewhere while the trigger's own
    // not-sent report is still on its way. Keep-calling repeats the same
    // text on the same offset every slot, so a frame reported aired before
    // the trigger's own slot started is an earlier emission's, not the
    // trigger's.
    fn aired_as(
        &self,
        message_text: &str,
        aired: &UnsentAttemptPayload,
        aired_at: DateTime<Utc>,
    ) -> bool {
        self.raw_text == message_text
            && self.tx_slot.is_none_or(|slot| aired_at >= slot)
            && emission_carries(
                self.frequency_hz,
                self.remote_origin,
                &self.remote_client_key_id,
                aired,
            )
    }
}

#[derive(Debug, Default)]
struct KeepCallLedger {
    // Never rolled back: keeps pacing independent of TX accounting.
    last_emitted_at: Option<DateTime<Utc>>,
    attempts: VecDeque<KeepCallAttempt>,
    // A rollback took `stall_cycles` back below the switch threshold, so any
    // stall action already raised is stale. Cleared when an action is raised
    // again or the streak resets.
    stall_action_revoked: bool,
    // PAN-182: see `PendingStallTrigger`.
    pending_stall_trigger: Option<PendingStallTrigger>,
    // PAN-83: the most recently reported aired frame for this QSO, in report
    // order — airing order, since the TX worker reports as frames play out and
    // `DurableInbox` yields in send order. Frame history, not stall evidence:
    // no stall reset clears it, and `answered_frame_offset_hz` reads it.
    last_aired: Option<AiredFrame>,
}

// PAN-83: a frame the TX worker reported aired for a QSO — where it went out
// and when its playback finished (`TxFrameSent::aired_at`).
#[derive(Debug, Clone, Copy, PartialEq)]
struct AiredFrame {
    offset_hz: f64,
    aired_at: DateTime<Utc>,
}

/// PAN-83: the TX offset of the frame a reply answered, when the aired
/// reports pin it down.
///
/// A QSO alternates slots, so a reply the DX sent in slot R answers what it
/// decoded from us in slot R-1. `reply_parity` is the decoded frame's own slot
/// parity; R is the latest slot of that parity starting at or before
/// `received_at`, which holds whether the decode was processed inside R or
/// just after it. The newest aired frame is the answered one only if it
/// finished airing in `[R - slot, R + slot)`: not before R-1 began (an older
/// frame), and not after R+1 began (our next frame, which cannot finish before
/// R+1 + ~12.6 s; a late-started R-1 frame can finish just after R begins).
/// `None` when there is no parity, no record, or no frame in that window —
/// the R-1 frame never aired or its report has not arrived — and the caller
/// falls back to the relocation's provenance.
fn answered_frame_offset_hz(
    last_aired: Option<AiredFrame>,
    received_at: DateTime<Utc>,
    reply_parity: Option<pancetta_core::slot::SlotParity>,
    slot_ns: i64,
) -> Option<f64> {
    let reply_parity = reply_parity?;
    let frame = last_aired?;
    let slot = Duration::nanoseconds(slot_ns);
    let cur = pancetta_core::slot::current_slot_start_with_period(received_at, slot_ns);
    let reply_slot =
        if pancetta_core::slot::SlotParity::of_with_period(cur, slot_ns) == reply_parity {
            cur
        } else {
            cur - slot
        };
    (frame.aired_at >= reply_slot - slot && frame.aired_at < reply_slot + slot)
        .then_some(frame.offset_hz)
}

/// Whether a failed TX could be matched to a recent, active keep-call emission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsentAttemptRollback {
    /// One emission was reversed; includes the remaining call count.
    RolledBack { call_count: u32 },
    /// No matching emission remains within the retention window.
    NoMatchingAttempt,
    /// Missing or terminal QSOs have no live bookkeeping to restore.
    QsoNotActive,
}

/// PAN-223: what [`QsoManager::record_scheduled_tx_slot`] did with the TX
/// worker's report of the slot it scheduled a frame into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduledTxSlot {
    /// The QSO is unknown or no longer active.
    QsoNotActive,
    /// No surviving ledger attempt has this id (an opening call or advance,
    /// which record none, or an attempt already rolled back or aged out).
    /// Never falls back to content matching (PAN-183).
    NoMatchingAttempt,
    /// The attempt records no slot (`resend_last_tx`, the offset-switch
    /// retransmit, or a key a stall reset retired): never keyed, never counts.
    NoSlotKey,
    /// The rearm's prediction already named the worker's slot.
    Unchanged,
    /// Re-keyed; the attempt held no count and gains none.
    RekeyedUncounted,
    /// Re-keyed; its count moved to a slot no other attempt counted.
    CountMoved,
    /// Re-keyed uncounted; a surviving attempt still keyed to the old slot
    /// took that slot's count (the PAN-128 heir rule).
    CountLeftWithSibling,
    /// Another attempt already counted the worker's slot: this attempt's
    /// count was the duplicate and is gone.
    DuplicateCountRemoved { stall_cycles: u32 },
}

/// PAN-280: a pending completion [`QsoManager`] reverted, announced after the
/// caller's `qsos` guard drops.
struct RevertedCompletion {
    qso_id: QsoId,
    to: RevertedTo,
    old_state: QsoState,
    new_state: QsoState,
    metadata: QsoMetadata,
    state_history: Vec<StateTransition>,
    messages: Vec<QsoMessage>,
}

/// One QSO [`QsoManager`]'s locked supersede retired (PAN-280), announced
/// after the caller's `qsos` guard drops.
struct SupersededQso {
    qso_id: QsoId,
    old_state: QsoState,
    new_state: QsoState,
    metadata: QsoMetadata,
    state_history: Vec<StateTransition>,
    messages: Vec<QsoMessage>,
}

/// What a manual open or re-call actually did (PAN-138). The frame-emitting
/// outcomes carry the emitted `MessageToSend`'s attempt id (PAN-280), so a
/// caller can match that frame's later TX report to this call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualCallOutcome {
    /// A new QSO was created and its first frame emitted.
    Opened(TxAttemptId),
    /// An existing QSO's latest outbound frame was re-emitted and charged.
    Resent(TxAttemptId),
    /// An existing QSO moved to a later step and that step's frame was emitted.
    Advanced(TxAttemptId),
    /// Nothing emitted: the remote client was not TX-permitted when re-checked
    /// under the `qsos` write guard.
    NotPermitted,
    /// Nothing emitted: the existing QSO had no prior `Sent` frame, or was
    /// gone by the time the guard was taken.
    NothingToResend,
}

impl ManualCallOutcome {
    /// Whether a `MessageToSend` was emitted.
    pub fn emitted_frame(self) -> bool {
        self.attempt_id().is_some()
    }

    /// The emitted `MessageToSend`'s attempt id (PAN-280), or `None` when
    /// nothing was emitted.
    pub fn attempt_id(self) -> Option<TxAttemptId> {
        match self {
            Self::Opened(id) | Self::Resent(id) | Self::Advanced(id) => Some(id),
            Self::NotPermitted | Self::NothingToResend => None,
        }
    }
}

/// PAN-280: how long a completion waits for its closing frame's TX outcome
/// before it is treated as not aired: `max(6 slots, 30 s)` of the live slot
/// length (FT8 90 s, FT4 45 s, FT2 30 s). The worst case from queue to
/// `TxFrameSent`, computed from the TX worker's constants, is about 58 s on
/// FT8.
const PENDING_COMPLETION_EXPIRY_SLOTS: i64 = 6;
const PENDING_COMPLETION_EXPIRY_FLOOR_MS: i64 = 30_000;

/// PAN-280: what a pending completion puts back when its closing frame never
/// airs.
#[derive(Debug, Clone)]
enum CompletionRestore {
    /// Opened directly at the close: there is no earlier state, so the QSO
    /// retires as `Failed { CloseNotAired }`.
    RetireOpen,
    /// Advanced an existing QSO: put back what the completion changed.
    Revert {
        state: QsoState,
        reports: SignalReports,
        end_time: Option<DateTime<Utc>>,
        completed_rf_frequency_hz: Option<f64>,
        /// The `Sent` record the completion appended: `(timestamp, raw_text)`.
        sent_record: Option<(DateTime<Utc>, String)>,
    },
}

/// PAN-280: a completion that queued a closing frame, held until the TX
/// worker reports that frame aired. Kept in its own map, not on the PAN-75
/// ledger entry, because the rearm prunes ledger entries of non-active QSOs
/// and a pending completion's QSO is `Completed`.
#[derive(Debug, Clone)]
struct PendingCompletion {
    /// The closing frame's attempt, plus each FIX B re-key charged while
    /// pending. Any one airing commits; the last one reported not sent
    /// reverts.
    attempt_ids: Vec<TxAttemptId>,
    raw_text: String,
    frequency_hz: f64,
    remote_origin: bool,
    remote_client_key_id: Option<String>,
    queued_at: DateTime<Utc>,
    /// The `Completed` state this record created; commit and revert act only
    /// while the QSO still holds exactly this state.
    completed_state: QsoState,
    restore: CompletionRestore,
    /// The `QsoCompleted` payload, built at queue time exactly as before.
    metadata: QsoMetadata,
    state_history: Vec<StateTransition>,
    messages: Vec<QsoMessage>,
}

/// PAN-280: what [`QsoManager::confirm_completion_aired`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionAired {
    /// The held `QsoCompleted` was emitted.
    Committed,
    /// No pending completion matched this frame.
    NotPending,
}

/// PAN-280: where a reverted completion left its QSO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevertedTo {
    /// An advanced QSO is back in its pre-close state.
    Reopened,
    /// A QSO opened at the close is `Failed { CloseNotAired }`.
    Retired,
}

/// PAN-280: what [`QsoManager::resolve_unsent_completion`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsentCompletion {
    /// No pending completion holds this attempt (or the report had no id).
    NotPending,
    /// The attempt was dropped from the pending set; another can still air.
    StillPending,
    /// The last pending attempt did not air; nothing was logged.
    Reverted(RevertedTo),
}

/// The payload of a frame a TX pivot replaced (PAN-75). Text alone cannot
/// tell that frame apart from the fresher same-text frame that aired in its
/// place when the pivot changed only the offset or the remote binding.
/// PAN-182 reuses it for the frame that aired, to release a held stall
/// action only for the frame it was waiting on.
#[derive(Debug, Clone, PartialEq)]
pub struct UnsentAttemptPayload {
    pub frequency_hz: f64,
    /// `(remote_origin, remote_client_key_id)`, or `None` when the reporter
    /// does not supply the frame's binding (a multi-TX supersede that kept
    /// the bundle's authorization).
    pub remote_binding: Option<(bool, Option<String>)>,
}

/// Per-QSO relocation counters shared with the TX worker (PAN-145).
///
/// `apply_tx_offset_switch` bumps its raw QsoId key under `ptt_sync_gate`,
/// before committing the offset. The worker snapshots before reading intent
/// and compares after acquiring that gate. This detects the PAN-143 round-4
/// race even while the coordinator's asynchronous intent mirror lags.
/// Other paths that move a QSO's TX offset without the gate (for example the
/// drift relatch and the Hound QSY) do not bump it.
/// The map lock is never held across an await.
#[derive(Default)]
pub struct TxRelocationEpochs {
    counter: AtomicU64,
    per_qso: std::sync::Mutex<HashMap<String, u64>>,
}

impl TxRelocationEpochs {
    /// Invalidate this QSO's earlier TX snapshots while holding `ptt_sync_gate`.
    pub fn bump(&self, qso_id: &str) {
        let mut epochs = self.per_qso.lock().expect("TX relocation epochs poisoned");
        let next = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        epochs.insert(qso_id.to_owned(), next);
    }

    /// An untouched QSO starts at zero; raw QsoId strings are never normalized.
    pub fn epoch(&self, qso_id: &str) -> u64 {
        self.per_qso
            .lock()
            .expect("TX relocation epochs poisoned")
            .get(qso_id)
            .copied()
            .unwrap_or(0)
    }
}

// Historical UTC timestamps remain untouched. A state identity includes its
// variant as well as its original start, so transitions discard only state grace.
type WatchdogStateIdentity = (std::mem::Discriminant<QsoState>, DateTime<Utc>);

struct WatchdogAnchors {
    state: Option<(WatchdogStateIdentity, DateTime<Utc>)>,
    first_call: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

fn watchdog_state_identity(state: &QsoState, now: DateTime<Utc>) -> Option<WatchdogStateIdentity> {
    state
        .state_duration(now)
        .map(|age| (std::mem::discriminant(state), now - age))
}

/// QSO manager implementation
pub struct QsoManager {
    utc_clock: pancetta_core::time::UtcClock,
    /// Configuration
    config: QsoManagerConfig,

    /// Active QSOs by ID
    qsos: Arc<RwLock<HashMap<QsoId, QsoProgress>>>,
    /// PAN-296: timeout-only anchors, shared with the production loop's clone.
    /// Lock order is qsos, then watchdog_anchors. Never hold this across await.
    watchdog_anchors: Arc<std::sync::Mutex<HashMap<QsoId, WatchdogAnchors>>>,

    /// PAN-75 accounting only. Lock after `qsos`, never across an await.
    keep_call_ledger: Arc<std::sync::Mutex<HashMap<QsoId, KeepCallLedger>>>,

    /// PAN-280: completions held until their closing frame airs. Lock after
    /// `qsos`, never across an await, never together with `keep_call_ledger`.
    pending_completions: Arc<std::sync::Mutex<HashMap<QsoId, PendingCompletion>>>,

    /// PAN-280: when a QSO's pending completion was reverted to its pre-close
    /// state. `check_timeouts_at` exempts it from every timeout for one
    /// [`COMPLETED_QSO_REWORK_GRACE`]. Same lock rules as above.
    completion_reverted_at: Arc<std::sync::Mutex<HashMap<QsoId, DateTime<Utc>>>>,

    /// PAN-183: allocates [`TxAttemptId`]s, shared across clones like the
    /// ledger so ids stay unique per process.
    attempt_id_counter: Arc<AtomicU64>,

    /// QSOs by callsign for duplicate checking
    qsos_by_callsign: Arc<RwLock<HashMap<String, Vec<QsoId>>>>,

    /// Event broadcaster
    event_sender: broadcast::Sender<QsoEvent>,

    /// Next contest serial number
    next_serial: Arc<RwLock<SerialNumber>>,

    /// Cleanup interval timer
    cleanup_interval: Arc<RwLock<Option<Interval>>>,

    /// Optional database for persistent duplicate checking
    database: Option<Arc<QsoDatabase>>,
    require_database: bool,

    /// Rig dial frequency in Hz, shared from the coordinator's hamlib poll
    /// (0 if unknown / no rig). `metadata.frequency` holds the *audio offset*;
    /// the logged RF frequency of a completed QSO is `dial + offset` (WSJT-X
    /// convention). Used only when stamping completed-QSO metadata so the ADIF
    /// records a real FREQ/BAND instead of the bare offset.
    dial_frequency_hz: Arc<AtomicU64>,

    /// Rig split-TX dial in Hz (0 = simplex), shared from the coordinator.
    /// When nonzero, completed-QSO RF is stamped against this TX dial instead
    /// of `dial_frequency_hz` (the RX dial). Defaults to a private `0` atomic
    /// so callers that never inject a source keep simplex (RX==TX) behavior.
    split_tx_frequency_hz: Arc<AtomicU64>,

    /// Operator TX-frequency mode (`pancetta_core::TxFreqMode` as `u8`), shared
    /// from the coordinator. The silence-driven stall detector (`stall_cycles`
    /// counted in `rearm_manual_calls_at`, emitting
    /// `QsoEvent::TxOffsetActionNeeded` at threshold) only fires in `Auto`
    /// mode; in the default `Hold` mode the operator's picked offset is sticky
    /// and never moved autonomously. Defaults to a private `Hold` atomic so
    /// unit tests and any caller that never injects a source keep the
    /// hold-the-frequency behavior.
    tx_freq_mode: Arc<std::sync::atomic::AtomicU8>,

    /// PAN-140 (Codex P1 on PR #369): whether a PTT is currently keyed,
    /// shared from the coordinator. `apply_tx_offset_switch` re-checks this
    /// immediately before committing a frequency move — the same
    /// closest-to-the-mutation pattern as `tx_freq_mode` above — so a stall/
    /// nudge action can never commit a mid-flight frequency change out from
    /// under a transmission that keyed between the caller's own check and
    /// this method's write-lock acquisition. Defaults to `false` (matches
    /// this method's behavior before PAN-140 existed) so unit tests and any
    /// caller that never injects a source are unaffected.
    ptt_active: Arc<std::sync::atomic::AtomicBool>,

    /// PAN-143 (follow-up to PAN-140's Codex round 2, itself parented under
    /// PAN-134's "check-then-act with no shared lock" root cause): a
    /// recheck of `tx_freq_mode`/`ptt_active` immediately before this
    /// method's own mutation only NARROWS the race against the TX worker's
    /// `ptt_active.store` (`PttGuard::new`) and the TUI-relay task's
    /// `tx_freq_mode.store` (three sites — the `f`-key toggle and both `o`-
    /// modal branches) — it cannot CLOSE it, because none of those three
    /// writers share a lock with this method's own `qsos` write lock. This
    /// gate is that shared lock: `apply_tx_offset_switch` takes it (after
    /// already holding `qsos.write()`, never before — see the design spec
    /// for the lock-ordering argument) and holds it from the guard-5/
    /// guard-6 recheck through the frequency mutation AND the resulting
    /// `TxOffsetApplied`/one-shot-retransmit `MessageToSend` emission —
    /// releasing it any earlier (round 3, Codex P1) would let the TX
    /// worker key PTT for an already-queued, now-stale request in the gap
    /// between "frequency committed" and "new intent published",
    /// reopening the exact class of race this exists to close, just moved
    /// later in the function. The three writers above take it around
    /// their own store. Because the critical section now spans this
    /// method's own `.await` points (the event emissions), this MUST be
    /// an async-aware `tokio::sync::Mutex`, not a blocking
    /// `std::sync::Mutex` — holding a blocking lock across an `.await`
    /// would be a real hazard, not just a lint. `PttGuard::new` and the
    /// TUI-relay handlers become `async fn`/`.await` this store
    /// accordingly. `tokio::sync::Mutex::lock` is infallible (no
    /// poisoning to recover from, unlike `std::sync::Mutex`). Defaults to
    /// a private, fresh mutex so unit tests and any caller that never
    /// injects a source are unaffected (uncontended lock, same behavior
    /// as before this field existed). See `docs/superpowers/specs/
    /// 2026-09-12-pan-143-ptt-shared-synchronization-design.md`.
    ptt_sync_gate: Arc<tokio::sync::Mutex<()>>,

    /// PAN-145: shared with the worker; private until explicitly injected.
    relocation_epochs: Arc<TxRelocationEpochs>,

    /// Global operator TX policy (`pancetta_core::TxPolicy` as `u8`), shared
    /// from the coordinator.
    ///
    /// PAN-72 (Codex round 2 on PR #350, finding 5). The silence-driven stall
    /// detector treats each per-slot re-send in `rearm_manual_calls_at` as
    /// evidence that the DX heard us and did not answer. Under
    /// `TxPolicy::Disabled` that inference is simply false: the coordinator's
    /// `tx_hard_mute_reason` blocks the transmission outright, so the DX's
    /// silence says nothing at all — yet the keep-calling loop still re-arms
    /// every slot. Left uncounted... it was counted, and four muted cycles in
    /// `TxFreqMode::Auto` moved an established QSO off a known-good offset
    /// nothing had actually transmitted on.
    ///
    /// Defaults to a private `Full` atomic so unit tests and any caller that
    /// never injects a source keep the pre-existing behavior (TX assumed
    /// live). `Full` and `RespondOnly` both `allows_any_tx()`; only `Disabled`
    /// suppresses the count.
    ///
    /// This covers the operator-visible mute only. The TX worker's other
    /// hard-mute causes (`tx_hard_mute_reason`: a Hamlib supervisor restart,
    /// which AGENTS.md documents as spanning multiple slots, the command loop
    /// not yet ready, an undelivered pending frequency/split command, an
    /// in-flight rig command, plus PAN-115's lost rig CAT link and dead audio
    /// output) are covered separately by `hamlib_hard_muted` below — the
    /// PAN-79 follow-up, landed as Codex round 10 on PR #350 — wired in
    /// `start_qso_component` from the same atomics the TX worker's own
    /// pre-PTT gate reads.
    tx_policy: Arc<std::sync::atomic::AtomicU8>,

    /// "Would a **remote-origin** frame be permitted to key PTT right now?" —
    /// the coordinator's station-agent armed-TX gate, read as a predicate.
    ///
    /// PAN-72 (Codex round 7 on PR #350, finding 3). `tx_policy` above closed
    /// only ONE of the two independent gates a rearmed frame must pass. A QSO
    /// with [`crate::states::QsoMetadata::remote_origin`] set emits every frame
    /// as `TxOrigin::Remote`, and the TX worker additionally checks
    /// `pancetta::coordinator::tx::remote_tx_permitted` (the
    /// `pancetta_agent::arm::ArmState` gate) immediately before PTT. When an
    /// arm expires — or a remote operator explicitly disarms — while global
    /// `TxPolicy` stays `Full`, every rearmed frame is dropped there, yet the
    /// stall detector kept counting the resulting silence as "the DX ignored
    /// us" and moved the QSO off a known-good offset after the threshold.
    ///
    /// **This is not a TX gate and must never become one.** It is read-only
    /// evidence about whether a frame reached the air, consulted solely to
    /// decide whether a rearm cycle counts as stall evidence; the real
    /// fail-CLOSED gate stays exactly where it is, in the TX worker (see
    /// AGENTS.md's "armed-TX gate fails CLOSED" invariant). Nothing here can
    /// authorize a transmission.
    ///
    /// Consulted only for QSOs that are actually `remote_origin`; a Local /
    /// TUI / autonomous QSO never reads it. Defaults to a closure returning
    /// `true` ("assume the frame went out"), so unit tests and any caller that
    /// never injects a source keep the pre-existing behavior — the same
    /// convention `tx_policy`'s private `Full` default uses.
    ///
    /// PAN-91 review follow-up: takes the QSO's own `remote_client_key_id` so
    /// the evidence predicate matches the TX worker's actual identity-bound
    /// gate (`remote_tx_permitted_for`) — a QSO bound to client A must not
    /// read as "reached the air" just because client B happens to be armed.
    /// `None` (a pre-PAN-91 remote_origin QSO with no bound identity) falls
    /// back to the boolean-only check, exactly like the TX worker's own gate.
    remote_tx_permitted: RemoteTxPermittedSource,

    /// "Is a rearmed frame currently blocked by a Hamlib-specific hard mute?"
    /// — `pancetta::coordinator::tx::tx_hard_mute_reason(...).is_some()`, read
    /// as a predicate.
    ///
    /// PAN-72 (Codex round 10, thread on `qso_manager.rs:6335`). The combined
    /// "frame reaches the air" check in [`Self::rearm_manual_calls_at`]
    /// already accounts for the global `tx_policy` mute (round 2) and the
    /// remote-arm gate (round 7, `remote_tx_permitted` above), but neither of
    /// those sees the TX worker's OWN pre-PTT Hamlib checks: a Hamlib
    /// supervisor restart, the command loop not yet ready, an undelivered
    /// pending frequency/split command, or an in-flight Hamlib command. A
    /// "rearmed" frame never reaches the air during any of these, yet
    /// `stall_cycles` was still incrementing as if the DX had ignored a real
    /// transmission — after `qso_stall_switch_after` such cycles this could
    /// relocate a QSO that was never actually unreachable, only temporarily
    /// blocked by rig-control state.
    ///
    /// **This is not a TX gate and must never become one** — same convention
    /// as `remote_tx_permitted` above: read-only evidence about whether a
    /// frame reached the air, consulted solely to decide whether a rearm
    /// cycle counts as stall evidence. The real fail-CLOSED gate stays
    /// exactly where it is, in the TX worker.
    ///
    /// Defaults to a closure returning `false` ("not muted"), so unit tests
    /// and any caller that never injects a source keep the pre-existing
    /// (mode=FT8-byte-identical) behavior.
    hamlib_hard_muted: Arc<dyn Fn() -> bool + Send + Sync>,

    /// Live length of one transmit slot in NANOSECONDS for the station's
    /// active protocol (FT8 → 15e9, FT4 → 7.5e9, FT2 → 3.2e9), shared from the
    /// coordinator's `active_slot_ns`.
    ///
    /// PAN-72 (PR #350 round-7 re-review). This is an `Arc` atomic and not a
    /// `config` field for a specific, load-bearing reason:
    /// [`QsoManager::start`] spawns `timeout_check_loop` — the loop that runs
    /// the whole rearm/stall cadence — on a `self.clone()`, and [`Clone`]
    /// copies `config` BY VALUE while every shared knob here is `Arc`-shared.
    /// The coordinator's Shift+M handler (`QsoMessage::SetOperatingMode` →
    /// [`QsoManager::set_active_mode`]) mutates only its own task-owned
    /// binding, so a config-derived cadence never saw a runtime mode switch at
    /// all: switching INTO FT4/FT2 left stall detection under-firing, and
    /// switching FT4 → FT8 left the loop stuck on a 7.5 s cadence for a QSO
    /// that was by then genuinely FT8 — a deviation from FT8's historical
    /// behavior in violation of AGENTS.md's byte-identity invariant.
    ///
    /// Defaults to a private [`DEFAULT_SLOT_NS`] (FT8) atomic, so unit tests
    /// and any caller that never injects a source keep exactly the historical
    /// 15 s cadence.
    active_slot_ns: Arc<std::sync::atomic::AtomicI64>,

    /// Shared late-start limit: worker and timeout clones observe reloads together.
    tx_late_max_ms: Arc<std::sync::atomic::AtomicU64>,
}

/// Outcome of the half-duplex parity-admission check for a *new* QSO.
///
/// FT8 is half-duplex on a 15 s slot grid: while we transmit in one window we
/// are deaf to the band. To keep the *opposite* window always free for hearing
/// responses, **every concurrent active QSO must transmit on the same parity**
/// (the "TX side"). A new QSO whose desired `tx_parity` matches the current
/// side (or any QSO when we are idle) is [`TxAdmission::Admit`]ted and runs
/// concurrently; a cross-parity request is [`TxAdmission::Queue`]d until all
/// current-side QSOs finish and a clean window flip is possible. We never
/// preempt an in-flight QSO to change sides. See [`admit_new_qso`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxAdmission {
    /// Start the QSO now — either we are idle (it adopts its own side) or its
    /// desired parity matches the current TX side (concurrent, same window).
    Admit,
    /// Defer the QSO — it wants the opposite window from the one we are
    /// currently committed to. Hold it until the current-side QSOs complete.
    Queue,
}

/// Pure half-duplex admission decision (see [`TxAdmission`]).
///
/// * `current_side` — the parity all currently-active QSOs transmit on, or
///   `None` when we are idle (no committed side).
/// * `desired` — the parity this new QSO wants to transmit on (`None` means the
///   caller hasn't pinned a parity, e.g. a CQ that lets the scheduler choose —
///   such a request never conflicts and is always admitted, adopting whatever
///   side is live).
///
/// Rules: idle → admit (adopt the side); unpinned desire → admit (flexible);
/// same side → admit (concurrent, same window); cross-side → queue.
pub fn admit_new_qso(
    current_side: Option<pancetta_core::slot::SlotParity>,
    desired: Option<pancetta_core::slot::SlotParity>,
) -> TxAdmission {
    match (current_side, desired) {
        // Idle: the new QSO defines the side.
        (None, _) => TxAdmission::Admit,
        // Caller didn't pin a parity: it can ride the live side.
        (Some(_), None) => TxAdmission::Admit,
        // Committed side and a pinned desire: admit iff they match.
        (Some(side), Some(want)) => {
            if side == want {
                TxAdmission::Admit
            } else {
                TxAdmission::Queue
            }
        }
    }
}

/// Outcome of the concurrent-QSO capacity check for an open that would add one
/// more ACTIVE QSO (PAN-78). Sibling of [`TxAdmission`]: parity decides which
/// window a new QSO may use; capacity decides whether there is room at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityAdmission {
    /// Room for one more active QSO.
    Admit,
    /// `active` QSOs already occupy all `cap` slots — refuse the open.
    AtCapacity { active: usize, cap: usize },
}

/// Pure capacity decision (see [`CapacityAdmission`]): admit iff
/// `active < cap`. `cap` is used as given (no floor) so callers reproduce
/// `maybe_answer_caller`'s historical `active >= cap` refusal exactly.
pub fn admit_within_capacity(active: usize, cap: usize) -> CapacityAdmission {
    if active < cap {
        CapacityAdmission::Admit
    } else {
        CapacityAdmission::AtCapacity { active, cap }
    }
}

/// PAN-137: whether a QSO or queued call belongs to a station-agent client
/// other than the current controller. Local and unbound WSJT-X QSOs stay.
pub fn bound_to_other_remote_client(
    remote_origin: bool,
    remote_client_key_id: Option<&str>,
    controller_key_id: &str,
) -> bool {
    remote_origin && remote_client_key_id.is_some_and(|k| k != controller_key_id)
}

impl QsoManager {
    /// Current true RF frequency for an audio-offset frequency, using the
    /// live rig dial (+ split TX dial if active) -- the same `effective_tx_dial`
    /// pattern used to stamp a Completed QSO's `metadata.frequency` (PAN-25).
    /// Falls back to the bare offset when no dial source is available
    /// (`dial == 0`, e.g. unit tests / no rig), matching that same stamping
    /// fallback exactly, so a completed QSO whose frequency was never
    /// dial-adjusted still compares consistently against a same-session
    /// incoming (also un-adjusted) frequency.
    fn current_rf_frequency(&self, audio_offset_hz: f64) -> f64 {
        let rx_dial = self.dial_frequency_hz.load(Ordering::Relaxed);
        let split = self.split_tx_frequency_hz.load(Ordering::Relaxed);
        let dial = effective_tx_dial(rx_dial, split);
        if dial > 0 {
            audio_offset_hz + dial as f64
        } else {
            audio_offset_hz
        }
    }

    /// The parity our currently-active QSOs are committed to transmitting on,
    /// or `None` when idle.
    ///
    /// Half-duplex discipline (see [`admit_new_qso`]) guarantees every active
    /// QSO shares one TX parity, so the first active QSO carrying a latched
    /// `tx_parity` defines the side. QSOs with `tx_parity == None` (parity left
    /// to the scheduler) don't pin a side and are skipped. Returns `None` when
    /// no active QSO pins a parity (we are free to adopt either window).
    pub async fn current_tx_side(&self) -> Option<pancetta_core::slot::SlotParity> {
        let qsos = self.qsos.read().await;
        qsos.values()
            .filter(|p| p.state.is_active())
            .find_map(|p| p.metadata.tx_parity)
    }

    /// Count of currently-active (non-terminal) QSOs.
    pub async fn active_qso_count(&self) -> usize {
        self.qsos
            .read()
            .await
            .values()
            .filter(|p| p.state.is_active())
            .count()
    }

    /// Count of currently-active (non-terminal) QSOs that are **not** in the
    /// `CallingCq` state — i.e. caller-answer / in-exchange QSOs only.
    ///
    /// Used by the Fox-mode path in `maybe_answer_caller`: the Fox's own CQ
    /// QSO is `CallingCq` and must not eat a Hound-answer slot.  With default
    /// `fox_max_streams = 5`, this lets 5 Hounds be worked simultaneously
    /// while the CQ keeps running (CQ + 5 answers = 6 streams ≤
    /// `MAX_RETAINED_TX_STREAMS = 8`).
    pub async fn active_caller_qso_count(&self) -> usize {
        self.qsos
            .read()
            .await
            .values()
            .filter(|p| {
                p.state.is_active() && !matches!(p.state, crate::states::QsoState::CallingCq { .. })
            })
            .count()
    }

    /// PAN-78: whether a MANUAL open of `callsign` at `step`, on `frequency`'s
    /// band, would ADD an active QSO, as opposed to resolving to one that
    /// already exists. Pass the same `frequency` the open call will receive
    /// (the computed TX offset):
    /// - `SeventyThree` never adds: it advances an active QSO, re-keys a
    ///   recently completed one, or opens directly at `Completed` (not active);
    /// - any active QSO with this callsign on this band → no: FIX 1 continues a
    ///   manual one, FIX 3 ([`Self::supersede_active_qsos_for`]) replaces any
    ///   other one-for-one;
    /// - `Rr73` with a manual QSO completed within
    ///   [`COMPLETED_QSO_REWORK_GRACE`] → no: FIX B re-keys it;
    /// - otherwise → yes.
    ///
    /// Must stay in lockstep with [`Self::respond_to_cq_with`] /
    /// [`Self::respond_to_caller`] (pinned by the
    /// `manual_open_predicate_matches_real_open_effect` test). Read-only: never
    /// takes a write lock.
    pub async fn manual_open_adds_active_qso(
        &self,
        callsign: &str,
        frequency: f64,
        step: pancetta_core::ResponseStep,
    ) -> bool {
        if step == pancetta_core::ResponseStep::SeventyThree {
            return false;
        }
        let want_band = crate::utils::frequency_to_band(frequency);
        let key = callsign.to_uppercase();
        let ids = self
            .qsos_by_callsign
            .read()
            .await
            .get(&key)
            .cloned()
            .unwrap_or_default();
        let has_active_same_band = {
            let qsos = self.qsos.read().await;
            ids.iter().any(|id| {
                qsos.get(id).is_some_and(|p| {
                    p.state.is_active()
                        && crate::utils::frequency_to_band(p.metadata.frequency) == want_band
                })
            })
        };
        if has_active_same_band {
            return false;
        }
        // FIX B only exists on `respond_to_caller`'s non-Grid path; Grid
        // (ladder rank 0) is routed through `respond_to_cq_with`, which has
        // no completed-QSO re-key, so the rank gate excludes it here too.
        if Self::step_ladder_rank(step) >= Some(2)
            && self
                .find_recently_completed_manual_qso_for(
                    callsign,
                    frequency,
                    COMPLETED_QSO_REWORK_GRACE,
                )
                .await
                .is_some()
        {
            return false;
        }
        true
    }

    /// PAN-78: whether [`Self::engage_hound`] for `fox_call` would ADD an
    /// active QSO. `engage_hound` is a manual `respond_to_cq_with` at the
    /// Fox's deterministic low calling offset, so a repeat engage of a Fox we
    /// are already working continues (FIX 1) or replaces (FIX 3) that QSO and
    /// adds nothing; this is [`Self::manual_open_adds_active_qso`] at that
    /// offset and the `Grid` step.
    pub async fn engage_hound_adds_active_qso(&self, fox_call: &str) -> bool {
        self.manual_open_adds_active_qso(
            fox_call,
            self.hound_calling_offset(fox_call),
            pancetta_core::ResponseStep::Grid,
        )
        .await
    }

    /// The low calling offset [`Self::engage_hound`] opens a Hound QSO at.
    fn hound_calling_offset(&self, fox_call: &str) -> f64 {
        hound_offset_for(
            fox_call,
            self.config.hound.call_min_hz,
            self.config.hound.call_max_hz,
        )
    }

    /// Whether any *active* (non-terminal) QSO already exists with `callsign`
    /// (compound-call-aware via [`crate::exchange::callsigns_match`], so
    /// `EA8/G8BCG` and `G8BCG` count as the same station). Used by the
    /// always-answer-callers path to avoid opening a duplicate QSO with a
    /// station an exchange is already in progress with.
    pub async fn has_active_qso_with(&self, callsign: &str) -> bool {
        let qsos = self.qsos.read().await;
        qsos.values().any(|p| {
            p.state.is_active()
                && p.metadata
                    .their_callsign
                    .as_deref()
                    .is_some_and(|c| crate::exchange::callsigns_match(c, callsign))
        })
    }

    /// True if we have an ACTIVE QSO with `callsign`, OR a recently-COMPLETED
    /// one on the SAME band (within `within`). Used to suppress the
    /// always-answer-callers path from opening a second QSO for a station we
    /// just finished working — the bounded auto-resend-73 path already
    /// handles a DX that didn't copy our 73. Explicit operator re-work
    /// bypasses this gate entirely (it uses the `StartQso` →
    /// `respond_to_cq_manual` path, not `maybe_answer_caller`).
    ///
    /// Compound-call-aware: `EA8/G8BCG` and `G8BCG` are the same station.
    ///
    /// PAN-25 round 2 (Codex): `frequency` (the incoming message's audio
    /// offset) band-scopes the completed-QSO arm the same way
    /// `find_qsos_for_message`/`find_recently_completed_manual_qso_for_at`
    /// already do — without it, a station worked on 20m stayed "reserved"
    /// against this preflight for a fresh direct call on 40m too.
    pub async fn has_active_or_recent_qso_with(
        &self,
        callsign: &str,
        frequency: f64,
        within: std::time::Duration,
    ) -> bool {
        let qsos = self.qsos.read().await;
        let now = self.now_utc();
        let window_secs = within.as_secs() as i64;
        let want_band = crate::utils::frequency_to_band(self.current_rf_frequency(frequency));
        qsos.values().any(|p| {
            let call_match = p
                .metadata
                .their_callsign
                .as_deref()
                .is_some_and(|c| crate::exchange::callsigns_match(c, callsign));
            if !call_match {
                return false;
            }
            if p.state.is_active() {
                return true;
            }
            // Recently completed, on the same band? (`completed_at` lives in
            // QsoState::Completed { .. }.)
            if let QsoState::Completed { completed_at, .. } = &p.state {
                return Self::completed_band(p) == want_band
                    && now.signed_duration_since(*completed_at).num_seconds() <= window_secs;
            }
            false
        })
    }

    fn now_utc(&self) -> DateTime<Utc> {
        (self.utc_clock)()
    }

    #[cfg(test)]
    pub fn with_clock_for_test(
        config: QsoManagerConfig,
        clock: pancetta_core::time::UtcClock,
    ) -> Self {
        let mut manager = Self::new(config);
        manager.utc_clock = clock;
        manager
    }

    /// Create a new QSO manager using the system UTC clock.
    pub fn new(config: QsoManagerConfig) -> Self {
        let (event_sender, _) = broadcast::channel(1000);
        let next_serial = config
            .contest_mode
            .as_ref()
            .map(|c| c.starting_serial)
            .unwrap_or(1);

        Self {
            utc_clock: pancetta_core::time::system_utc_clock(),
            config,
            qsos: Arc::new(RwLock::new(HashMap::new())),
            watchdog_anchors: Arc::new(std::sync::Mutex::new(HashMap::new())),
            keep_call_ledger: Arc::new(std::sync::Mutex::new(HashMap::new())),
            pending_completions: Arc::new(std::sync::Mutex::new(HashMap::new())),
            completion_reverted_at: Arc::new(std::sync::Mutex::new(HashMap::new())),
            attempt_id_counter: Arc::new(AtomicU64::new(0)),
            qsos_by_callsign: Arc::new(RwLock::new(HashMap::new())),
            event_sender,
            next_serial: Arc::new(RwLock::new(next_serial)),
            cleanup_interval: Arc::new(RwLock::new(None)),
            database: None,
            require_database: false,
            dial_frequency_hz: Arc::new(AtomicU64::new(0)),
            split_tx_frequency_hz: Arc::new(AtomicU64::new(0)),
            // Default Hold: the silence-driven stall detector stays off unless
            // the coordinator injects a shared mode atomic and the operator
            // switches to Auto.
            tx_freq_mode: Arc::new(std::sync::atomic::AtomicU8::new(
                pancetta_core::TxFreqMode::Hold.as_u8(),
            )),
            // Default false: with no injected source, behave exactly as this
            // method did before PAN-140 existed (see the field's doc comment).
            ptt_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            // Default a fresh, private, uncontended mutex: with no injected
            // source, behave exactly as this method did before PAN-143
            // existed (see the field's doc comment).
            ptt_sync_gate: Arc::new(tokio::sync::Mutex::new(())),
            relocation_epochs: Arc::new(TxRelocationEpochs::default()),
            // Default Full: with no injected source, assume TX is live — the
            // pre-existing behavior (see the field's doc comment).
            tx_policy: Arc::new(std::sync::atomic::AtomicU8::new(
                pancetta_core::TxPolicy::Full.as_u8(),
            )),
            // Default "permitted": with no injected source, assume a remote
            // frame reached the air — the pre-existing behavior (see the
            // field's doc comment).
            remote_tx_permitted: Arc::new(|_| true),
            // Default "not muted": with no injected source, assume no
            // Hamlib-specific hard mute is in effect — the pre-existing
            // behavior (see the field's doc comment).
            hamlib_hard_muted: Arc::new(|| false),
            // Default FT8 slot: with no injected source the rearm/stall
            // cadence is the historical 15 s (see the field's doc comment).
            active_slot_ns: Arc::new(std::sync::atomic::AtomicI64::new(DEFAULT_SLOT_NS)),
            // Default late-start window: the documented operator default
            // (see the field's doc comment).
            tx_late_max_ms: Arc::new(std::sync::atomic::AtomicU64::new(DEFAULT_TX_LATE_MAX_MS)),
        }
    }

    /// Share the coordinator's rig dial-frequency source so completed QSOs log
    /// the true RF frequency (dial + audio offset) instead of the bare offset.
    /// Pass the same `Arc<AtomicU64>` the hamlib poll loop updates; if never
    /// called, completed metadata keeps the offset value (e.g. unit tests).
    pub fn set_dial_frequency_source(&mut self, source: Arc<AtomicU64>) {
        self.dial_frequency_hz = source;
    }

    /// Share the coordinator's split-TX dial source so completed QSOs log the
    /// real TX RF during split operation. Pass the same `Arc<AtomicU64>` the
    /// TUI SetSplit relay updates (0 = simplex). If never called, the manager
    /// keeps its private `0` (RX==TX).
    pub fn set_split_tx_frequency_source(&mut self, source: Arc<AtomicU64>) {
        self.split_tx_frequency_hz = source;
    }

    /// Share the coordinator's TX-frequency-mode atomic so the silence-driven
    /// stall detector (`stall_cycles` → `QsoEvent::TxOffsetActionNeeded`)
    /// respects the operator's Hold/Auto choice at runtime. Pass the same
    /// `Arc<AtomicU8>` the TUI toggle updates (encoded via
    /// [`pancetta_core::TxFreqMode::as_u8`]). If never called, the manager keeps
    /// its private `Hold` default (no autonomous frequency changes).
    pub fn set_tx_freq_mode_source(&mut self, source: Arc<std::sync::atomic::AtomicU8>) {
        self.tx_freq_mode = source;
    }

    /// Share the coordinator's PTT-active atomic (PAN-140) so
    /// `apply_tx_offset_switch` can refuse to commit a frequency move while a
    /// PTT is already keyed for the current slot. Pass the same
    /// `Arc<AtomicBool>` the TX worker sets true/false around each key. If
    /// never called, the manager keeps its private `false` default (see the
    /// field's doc comment — matches pre-PAN-140 behavior).
    pub fn set_ptt_active_source(&mut self, source: Arc<std::sync::atomic::AtomicBool>) {
        self.ptt_active = source;
    }

    /// Share the coordinator's PTT/offset-switch synchronization gate
    /// (PAN-143) so `apply_tx_offset_switch`'s recheck-through-publish
    /// span is genuinely atomic against the TX worker's `ptt_active`
    /// store and the TUI-relay task's `tx_freq_mode` stores, instead of
    /// merely narrowing the race between them. Pass the same
    /// `Arc<tokio::sync::Mutex<()>>` those sites take. If never called,
    /// the manager keeps its own private, always-uncontended mutex (see
    /// the field's doc comment — matches pre-PAN-143 behavior, since
    /// nothing else could be racing a lock nobody else holds a reference
    /// to).
    pub fn set_ptt_sync_gate_source(&mut self, source: Arc<tokio::sync::Mutex<()>>) {
        self.ptt_sync_gate = source;
    }

    /// Share the coordinator's relocation counters with its TX worker so a
    /// gate wait cannot hide an offset change behind the async intent mirror.
    pub fn set_relocation_epoch_source(&mut self, source: Arc<TxRelocationEpochs>) {
        self.relocation_epochs = source;
    }

    /// Share the coordinator's global TX-policy atomic (encoded via
    /// [`pancetta_core::TxPolicy::as_u8`]) so the silence-driven stall detector
    /// can tell "the DX ignored us" apart from "we never actually transmitted".
    ///
    /// PAN-72 (Codex round 2 on PR #350, finding 5) — see the `tx_policy`
    /// field's doc comment. Mirrors [`Self::set_tx_freq_mode_source`]: pass the
    /// same `Arc<AtomicU8>` the TUI's policy toggle writes. If never called,
    /// the manager keeps its private `Full` default and behaves exactly as
    /// before.
    pub fn set_tx_policy_source(&mut self, source: Arc<std::sync::atomic::AtomicU8>) {
        self.tx_policy = source;
    }

    /// Share the coordinator's station-agent armed-TX predicate so the stall
    /// detector can tell "the DX ignored us" apart from "the arm gate dropped
    /// every frame we rearmed" on a REMOTE-origin QSO.
    ///
    /// PAN-72 (Codex round 7 on PR #350, finding 3) — see the
    /// `remote_tx_permitted` field's doc comment, especially the note that
    /// this is read-only evidence and never a TX gate. Pass a closure that
    /// evaluates `pancetta::coordinator::tx::remote_tx_permitted` against the
    /// live `ArmState` (which fails CLOSED on a poisoned lock, so a poisoned
    /// arm also stops the silence being counted — the conservative direction).
    /// If never called, the manager keeps its private "permitted" default and
    /// behaves exactly as before.
    pub fn set_remote_tx_permitted_source(&mut self, source: RemoteTxPermittedSource) {
        self.remote_tx_permitted = source;
    }

    /// Share a predicate for "is a rearmed frame currently blocked by a
    /// Hamlib-specific hard mute?" so the stall detector can tell "the DX
    /// ignored us" apart from "the TX worker's own pre-PTT Hamlib checks
    /// dropped every frame we rearmed".
    ///
    /// PAN-72 (Codex round 10, thread on `qso_manager.rs:6335`) — see the
    /// `hamlib_hard_muted` field's doc comment, especially the note that this
    /// is read-only evidence and never a TX gate. Pass a closure that
    /// evaluates `pancetta::coordinator::tx::tx_hard_mute_reason(...).
    /// is_some()` against the SAME atomics/handles the TX worker's own
    /// pre-PTT check already reads. If never called, the manager keeps its
    /// private "not muted" default and behaves exactly as before.
    pub fn set_hamlib_hard_muted_source(&mut self, source: Arc<dyn Fn() -> bool + Send + Sync>) {
        self.hamlib_hard_muted = source;
    }

    /// Share the coordinator's `active_slot_ns` atomic so the keep-calling
    /// rearm gate and the `stall_cycles` counter it drives run on the ACTIVE
    /// protocol's transmit slot (FT8 → 15e9 ns, FT4 → 7.5e9, FT2 → 3.2e9).
    ///
    /// PAN-72 (PR #350 round-7 re-review) — see the `active_slot_ns` field's
    /// doc comment for the staleness bug this closes. Pass the same
    /// `Arc<AtomicI64>` the coordinator derives at startup from `[rig].mode`
    /// and rewrites in `try_switch_operating_mode` on every runtime Shift+M
    /// switch; because it is shared (not copied into `config`), the clone
    /// [`Self::start`] spawns the rearm loop on observes later switches too.
    ///
    /// Must be called BEFORE [`Self::start`], like the other `set_*_source`
    /// injectors. If never called, the manager keeps its private
    /// [`DEFAULT_SLOT_NS`] (FT8) atomic and behaves exactly as before.
    pub fn set_active_slot_ns_source(&mut self, source: Arc<std::sync::atomic::AtomicI64>) {
        self.active_slot_ns = source;
    }

    /// Set the station's late-start TX window (`[station] tx_late_max_ms`,
    /// in milliseconds) that `keep_call_tx_slot`'s current-slot acceptance
    /// mirrors from the TX scheduler (`schedule_tx`).
    ///
    /// PAN-128 PR review — see the `tx_late_max_ms` field's doc comment for
    /// why the stall ledger needs the scheduler's own window. Pass the same
    /// `cfg.station.tx_late_max_ms` used by the TX worker; if never called,
    /// the manager keeps [`DEFAULT_TX_LATE_MAX_MS`] (3000, the documented default).
    pub fn set_tx_late_max_ms(&mut self, tx_late_max_ms: u64) {
        self.tx_late_max_ms
            .store(tx_late_max_ms, std::sync::atomic::Ordering::Release);
    }

    /// Bind the station's live late-start limit before starting background work.
    pub fn set_tx_late_max_ms_source(&mut self, source: Arc<std::sync::atomic::AtomicU64>) {
        self.tx_late_max_ms = source;
    }

    /// One transmit slot in milliseconds for the live active protocol — the
    /// rearm/stall cadence. See [`rearm_slot_millis_from_ns`].
    fn rearm_slot_millis(&self) -> i64 {
        rearm_slot_millis_from_ns(
            self.active_slot_ns
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// The live [`pre_switch_offset_grace_from_slot_ns`] window for the
    /// currently active protocol (PAN-72 round-8 redesign, fix 1) — reads the
    /// same `active_slot_ns` atomic [`Self::rearm_slot_millis`] does.
    fn pre_switch_offset_grace(&self) -> chrono::Duration {
        pre_switch_offset_grace_from_slot_ns(
            self.active_slot_ns
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Create a new QSO manager with a database for persistent duplicate checking
    pub fn with_database(config: QsoManagerConfig, database: Arc<QsoDatabase>) -> Self {
        let mut manager = Self::new(config);
        manager.database = Some(database);
        manager
    }

    /// Require persistent history for autonomous admission. Set before admission-capable clones.
    pub fn require_persistent_history(&mut self) {
        self.require_database = true;
    }

    /// Attach the prepared index without replacing subscriptions or the event broadcaster.
    pub fn attach_database(&mut self, database: Arc<QsoDatabase>) {
        self.database = Some(database);
    }

    /// Get the configuration
    pub fn config(&self) -> &QsoManagerConfig {
        &self.config
    }

    /// Update the station-wide active mode string stamped into every
    /// [`QsoMetadata::mode`] this manager creates from now on. Only affects
    /// QSOs opened AFTER this call — anything already in progress keeps its
    /// already-stamped mode. Called from the coordinator's QSO task when the
    /// operator switches mode live (Shift+M); the caller (coordinator) is
    /// responsible for having already confirmed no QSO is active before the
    /// switch (see `try_switch_operating_mode`), so this setter itself does
    /// no gating.
    pub fn set_active_mode(&mut self, mode: String) {
        self.config.active_mode = mode;
    }

    /// Start the QSO manager
    pub async fn start(&self) -> Result<(), QsoManagerError> {
        info!("Starting QSO manager for {}", self.config.our_callsign);

        // Start cleanup timer
        let cleanup_duration = TokioDuration::from_secs(self.config.timeouts.cleanup_interval);
        let interval_timer = interval(cleanup_duration);
        *self.cleanup_interval.write().await = Some(interval_timer);

        // Start background tasks
        let manager = self.clone();
        tokio::spawn(async move {
            manager.cleanup_loop().await;
        });

        let manager = self.clone();
        tokio::spawn(async move {
            manager.timeout_check_loop().await;
        });

        Ok(())
    }

    /// Subscribe to QSO events
    pub fn subscribe(&self) -> broadcast::Receiver<QsoEvent> {
        self.event_sender.subscribe()
    }

    /// Resolve a CQ opening's TX parity to a CONCRETE value, latching it once.
    ///
    /// docs/archive/qso-engine-bugs.md BUG 1: calling CQ transmitted on EVERY 15s
    /// window instead of alternating. Root cause: `start_cq`/`start_cq_manual`
    /// accepted `tx_parity: None` (the caller's "Auto, no fixed preference"
    /// case — the DEFAULT for both the manual and autonomous CQ paths) and
    /// stored `None` directly into `QsoMetadata.tx_parity`. Every subsequent
    /// emission (the opening CQ AND every per-slot keep-call rearm) then
    /// re-asked the TX scheduler's `resolve_required_parity(None, Auto, now,
    /// …)` to pick "the nearest next slot" FRESH each time — which is just
    /// "whichever window is next," not a fixed side. Over consecutive calls
    /// that alternates Even/Odd/Even/Odd… i.e. TRANSMITS ON BOTH PARITIES,
    /// so the opposite (reply) window is never free and we never hear anyone
    /// answering our own CQ.
    ///
    /// Fix: resolve `None` to a CONCRETE parity exactly ONCE, here, at QSO
    /// creation, using the same "nearest next slot of either parity" rule the
    /// TX scheduler's `Auto` mode uses (mirrored here with only
    /// `pancetta_core::slot` primitives, since this crate has no dependency
    /// on the coordinator's `TxSelfParity` type) — then that ONE resolved
    /// value is stored in `QsoMetadata.tx_parity` and reused, unchanged, by
    /// the opening CQ and every rearm for the life of the QSO. A caller that
    /// already supplied a fixed preference (`Some(Even)`/`Some(Odd)`, e.g. a
    /// station configured for a specific side) is untouched — this only
    /// resolves the previously-ambiguous `None` case.
    fn latch_cq_parity_if_none(
        tx_parity: Option<pancetta_core::slot::SlotParity>,
        now: DateTime<Utc>,
    ) -> Option<pancetta_core::slot::SlotParity> {
        use pancetta_core::slot::{next_slot_with_parity, SlotParity};
        Some(tx_parity.unwrap_or_else(|| {
            let next_even = next_slot_with_parity(now, SlotParity::Even);
            let next_odd = next_slot_with_parity(now, SlotParity::Odd);
            if next_even <= next_odd {
                SlotParity::Even
            } else {
                SlotParity::Odd
            }
        }))
    }

    /// Start a new CQ call
    /// Start a CQ call.
    ///
    /// `tx_parity` is the parity we want our CQ to land on, if the caller has
    /// a fixed preference. `None` (no preference — the Auto-config default)
    /// is resolved to a CONCRETE parity ONCE at creation
    /// ([`Self::latch_cq_parity_if_none`]) and held for the life of the QSO —
    /// it does NOT mean "let the scheduler re-pick every slot" (that was BUG
    /// 1: re-picking "nearest slot" every call alternates both parities,
    /// i.e. transmits every window and never listens).
    pub async fn start_cq(
        &self,
        frequency: f64,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
        remote_origin: bool,
        remote_client_key_id: Option<String>,
    ) -> Result<QsoId, QsoManagerError> {
        self.start_cq_with_id(
            Uuid::new_v4(),
            frequency,
            tx_parity,
            remote_origin,
            remote_client_key_id,
        )
        .await
    }

    /// PAN-38 round 2 (Codex): same as [`Self::start_cq`], but lets the
    /// caller supply `qso_id` up front instead of having this function
    /// generate it internally. The autonomous coordinator uses this to
    /// register the qso_id<->cq_attempt_id association (`AutonomousCqOpened`)
    /// BEFORE this call's own `MessageToSend` becomes visible to the
    /// independently-scheduled event-forwarding task — otherwise a
    /// same-instant downstream failure's `TransmitComplete` could reach the
    /// autonomous task before the association does, silently dropping the
    /// rollback (and then registering a stale, never-cleaned-up entry when
    /// the late `AutonomousCqOpened` finally arrives).
    pub async fn start_cq_with_id(
        &self,
        qso_id: QsoId,
        frequency: f64,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
        remote_origin: bool,
        remote_client_key_id: Option<String>,
    ) -> Result<QsoId, QsoManagerError> {
        if self.config.our_callsign == "NOCALL" || self.config.our_callsign == "N0CALL" {
            return Err(QsoManagerError::Configuration {
                message: format!(
                    "Cannot transmit with placeholder callsign '{}'. Configure your callsign first.",
                    self.config.our_callsign
                ),
            });
        }
        let now = self.now_utc();
        // BUG 1 fix (docs/archive/qso-engine-bugs.md): latch a concrete parity ONCE at
        // QSO creation when the caller has no fixed preference (`None`, the
        // Auto-config default) — see `latch_cq_parity_if_none` doc.
        let tx_parity = Self::latch_cq_parity_if_none(tx_parity, now);

        let state = QsoState::CallingCq {
            frequency,
            started_at: now,
            call_count: 1,
        };

        let metadata = QsoMetadata {
            qso_id,
            our_callsign: self.config.our_callsign.clone(),
            their_callsign: None,
            frequency,
            completed_rf_frequency_hz: None,
            mode: self.config.active_mode.clone(),
            first_answer_at: None,
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares {
                ours: self.config.our_grid.clone(),
                theirs: None,
            },
            their_state: None,
            contest_info: None,
            tags: HashMap::new(),
            adif_field_types: HashMap::new(),
            notes: None,
            tx_parity,
            // Calling CQ is not a manual keep-calling QSO; it has its own
            // CallingCq timeout and call_count in the state itself.
            initiated_by: CallInitiation::Auto,
            // We called CQ → CQer role (drives the role-aware display ladder).
            role: QsoRole::Cqer,
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound: false,
            partner_freq: None,
            pending_freq_drift: None,
            hound_qsyed: false,
            fox: false,
            remote_origin,
            remote_client_key_id: remote_client_key_id.clone(),
            // CQ-path latch: resolved from our own preference, not an
            // observed DX parity — always self-consistent, never provisional.
            tx_parity_provisional: false,
        };

        let progress = QsoProgress {
            state: state.clone(),
            state_history: vec![],
            messages: vec![],
            metadata,
        };

        self.qsos.write().await.insert(qso_id, progress);

        // Emit CQ message
        let message = MessageType::Cq {
            callsign: self.config.our_callsign.clone(),
            grid: self.config.our_grid.clone(),
        };

        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
            remote_origin,
            remote_client_key_id,
            attempt_id: Some(self.next_attempt_id()),
        })
        .await;

        info!("Started CQ on {:.1} Hz: {}", frequency, qso_id);

        Ok(qso_id)
    }

    /// Start a **manual** (operator-initiated) CQ call as a real QSO.
    ///
    /// This is the engine half of the TUI `c` (StartCq) key. Unlike
    /// [`Self::start_cq`] (which marks the QSO [`CallInitiation::Auto`] for
    /// autonomous CQ), this marks it [`CallInitiation::Manual`] so that:
    ///
    /// 1. **We keep calling CQ every slot** until a station answers or the
    ///    CQ watchdog fires — [`Self::rearm_manual_calls_at`] re-emits a
    ///    `Cq` `MessageToSend` for a manual `CallingCq` QSO once per FT8
    ///    slot, bounded by `manual_call_max_calls` /
    ///    `manual_call_watchdog_minutes` (see [`Self::check_timeouts_at`]).
    /// 2. **When a caller answers, the exchange auto-sequences to
    ///    Completed + logs** — the auto-reply emitter in
    ///    [`Self::process_message`] is gated on `CallInitiation::Manual`, so
    ///    a manual CQer (us) automatically replies with our report → RR73 as
    ///    the caller's CqResponse → ReportAck arrive, exactly like the
    ///    operator-driven Callers path.
    ///
    /// We emit a `StateChanged` (Idle → CallingCq) so the coordinator's
    /// drop-stale-TX gate keys this QSO into `active_tx_qsos` (otherwise the
    /// TX worker would refuse to key PTT for it), and emit the first `Cq`
    /// `MessageToSend` immediately. `last_call_at` is stamped `now` so the
    /// per-slot rearm does not double-send the first CQ within the opening
    /// slot.
    ///
    /// `tx_parity` is the parity we want our CQ to land on, if fixed; `None`
    /// (no preference) is resolved to a CONCRETE parity ONCE here
    /// ([`Self::latch_cq_parity_if_none`]) and held for the life of the QSO —
    /// see that function's doc for the BUG 1 (transmit-every-window) history.
    /// (Calling CQ, we choose our own slot parity — there is no DX parity to
    /// oppose.)
    pub async fn start_cq_manual(
        &self,
        frequency: f64,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
        remote_origin: bool,
        remote_client_key_id: Option<String>,
    ) -> Result<QsoId, QsoManagerError> {
        if self.config.our_callsign == "NOCALL" || self.config.our_callsign == "N0CALL" {
            return Err(QsoManagerError::Configuration {
                message: format!(
                    "Cannot transmit with placeholder callsign '{}'. Configure your callsign first.",
                    self.config.our_callsign
                ),
            });
        }
        let qso_id = Uuid::new_v4();
        let now = self.now_utc();
        // BUG 1 fix (docs/archive/qso-engine-bugs.md): latch a concrete parity ONCE at
        // QSO creation when the caller has no fixed preference (`None`, the
        // Auto-config default) — see `latch_cq_parity_if_none` doc.
        let tx_parity = Self::latch_cq_parity_if_none(tx_parity, now);

        let state = QsoState::CallingCq {
            frequency,
            started_at: now,
            call_count: 1,
        };

        let message = MessageType::Cq {
            callsign: self.config.our_callsign.clone(),
            grid: self.config.our_grid.clone(),
        };

        let raw_text = self.render_sent_text(&message);
        let mut metadata = QsoMetadata {
            qso_id,
            our_callsign: self.config.our_callsign.clone(),
            their_callsign: None,
            frequency,
            completed_rf_frequency_hz: None,
            mode: self.config.active_mode.clone(),
            first_answer_at: None,
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares {
                ours: self.config.our_grid.clone(),
                theirs: None,
            },
            their_state: None,
            contest_info: None,
            tags: HashMap::new(),
            adif_field_types: HashMap::new(),
            notes: None,
            tx_parity,
            // Operator pressed `c`: this is a MANUAL CQ. The manual
            // keep-calling watchdog re-arms our CQ every slot, and the
            // CallInitiation::Manual gate turns on the auto-reply emitter so
            // an answering station drives the exchange to completion.
            initiated_by: CallInitiation::Manual,
            // We called CQ → CQer role (drives the role-aware display ladder).
            role: QsoRole::Cqer,
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound: false,
            partner_freq: None,
            pending_freq_drift: None,
            hound_qsyed: false,
            fox: false,
            // `false` for operator-pressed `c` (local); `true` for a remote
            // operator's `startCq` routed via the station agent.
            remote_origin,
            remote_client_key_id: remote_client_key_id.clone(),
            // CQ-path latch: resolved from our own preference, not an
            // observed DX parity — always self-consistent, never provisional.
            tx_parity_provisional: false,
        };
        self.enroll_manual_contest(&mut metadata);

        let progress = QsoProgress {
            state: state.clone(),
            state_history: vec![],
            // Record the opening CQ as a Sent message so the TUI last-TX line
            // and `resend_last_tx` see it.
            messages: vec![QsoMessage {
                timestamp: now,
                direction: MessageDirection::Sent,
                message_type: message.clone(),
                raw_text,
                signal_strength: None,
                frequency,
            }],
            metadata,
        };

        self.qsos.write().await.insert(qso_id, progress);

        // Emit a state change (Idle → CallingCq) so the coordinator's
        // drop-stale-TX gate keys this QSO into `active_tx_qsos`; without it
        // the TX worker would drop our CQ as "stale TX for an ended QSO".
        self.emit_state_change(qso_id, QsoState::Idle, state).await;

        // Emit the first CQ. Subsequent slots are owned by the per-slot
        // manual keep-call rearm (`rearm_manual_calls_at`).
        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
            remote_origin,
            remote_client_key_id,
            attempt_id: Some(self.next_attempt_id()),
        })
        .await;

        info!("Started manual CQ on {:.1} Hz: {}", frequency, qso_id);

        Ok(qso_id)
    }

    /// Respond to a CQ call (autonomous/internal path).
    ///
    /// `dx_parity` is the slot parity of the DX station's CQ, used to
    /// derive our `tx_parity` (opposite of theirs). May be `None` if
    /// the CQ came from a DX cluster spot rather than an on-air decode.
    ///
    /// This is the autonomous path: the self-duplicate gate applies and
    /// there is no manual keep-calling. For operator-initiated manual
    /// calls use [`Self::respond_to_cq_manual`] (or
    /// [`Self::respond_to_cq_with`] with [`CallInitiation::Manual`]).
    pub async fn respond_to_cq(
        &self,
        target_callsign: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<QsoId, QsoManagerError> {
        self.respond_to_cq_with(
            target_callsign,
            frequency,
            dx_parity,
            CallInitiation::Auto,
            None,  // auto path always Tx=Rx; partner_freq not needed
            false, // autonomous is a LOCAL initiation, never remote
            None,
        )
        .await
    }

    /// Respond to a CQ call as an operator-initiated **manual** call.
    ///
    /// Bypasses the self-duplicate gate (the operator explicitly chose to
    /// call this station, e.g. to re-work it) and marks the QSO so the
    /// manual keep-calling watchdog re-arms a call every TX slot until the
    /// DX answers or the watchdog fires.
    pub async fn respond_to_cq_manual(
        &self,
        target_callsign: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<QsoId, QsoManagerError> {
        self.respond_to_cq_with(
            target_callsign,
            frequency,
            dx_parity,
            CallInitiation::Manual,
            None,  // partner_freq computed by coordinator (T3); None = Tx=Rx fallback
            false, // TUI/DX-hunter manual call is LOCAL, never remote
            None,
        )
        .await
    }

    /// Enroll new manual attempts only. Exchange activation is separate
    /// from scoring identity: SC's Field Day exchange is not implemented.
    fn enroll_manual_contest(&self, metadata: &mut QsoMetadata) {
        if metadata.initiated_by != CallInitiation::Manual {
            return;
        }
        if let Some(context) = self.config.contest_scoring.as_ref() {
            if context.in_period(metadata.start_time) {
                crate::contest::ledger::attach(metadata, context);
            }
        }
    }

    fn grid_contest_engaged(metadata: &QsoMetadata) -> bool {
        metadata.contest_info.as_ref().is_some_and(|info| {
            info.scoring
                .as_ref()
                .is_none_or(|stamp| stamp.rules_id == crate::contest::scoring::KSQP_FT_2026)
        })
    }

    /// Engage a contest profile on an existing QSO. Stamps
    /// `metadata.contest_info` so `process_message_with_parity`'s
    /// reclassification step (PAN-49) knows to try this QSO's engaged
    /// profile's exchange-shape matcher against otherwise-`NonStandard`
    /// decodes, and so ADIF logging stamps `CONTEST_ID` from the profile's
    /// `adif_contest_id` when it has one (adif.rs) — the profile id itself
    /// never reaches the wire (PAN-52).
    ///
    /// No operator UI calls this yet — a later plan wires the "enter this
    /// contest?" modal to it (docs/superpowers/specs/
    /// 2026-08-30-contest-mode-design.md §4).
    pub async fn engage_contest_profile(
        &self,
        qso_id: QsoId,
        profile: crate::contest::profile::ContestProfile,
    ) -> Result<(), QsoManagerError> {
        let mut qsos = self.qsos.write().await;
        let progress = qsos
            .get_mut(&qso_id)
            .ok_or(QsoManagerError::QsoNotFound { qso_id })?;
        progress.metadata.contest_info = Some(ContestInfo {
            contest_name: profile.id,
            adif_contest_id: profile.adif_contest_id.clone(),
            category: String::new(),
            serials: ContestSerials {
                sent: None,
                received: None,
            },
            points: 0,
            multiplier: None,
            // PAN-50: engagement stamps exchange identity only; a scoring
            // instance stamp is attached separately when a validated
            // `[contest]` context owns the attempt.
            scoring: progress
                .metadata
                .contest_info
                .as_ref()
                .and_then(|info| info.scoring.clone()),
        });
        Ok(())
    }

    /// Stable IDs of completed session contacts expected in both durable logs.
    /// The caller performs all I/O after this short read lock is released.
    pub async fn completed_contest_contact_ids(&self) -> Vec<String> {
        self.qsos
            .read()
            .await
            .values()
            .filter(|p| matches!(p.state, QsoState::Completed { .. }))
            .filter_map(|p| {
                p.metadata
                    .tags
                    .get(crate::contest::ledger::CONTACT_ID)
                    .cloned()
            })
            .collect()
    }

    /// Open a manual **Hound** QSO to work a Fox (DXpedition) station.
    ///
    /// Thin wrapper over [`respond_to_cq_with`](Self::respond_to_cq_with) that
    /// additionally sets `metadata.hound = true` and
    /// `metadata.partner_freq = Some(fox_freq)` on the created QSO.
    ///
    /// The Hound procedure (WSJT-X DXpedition mode) splits TX and RX offsets:
    /// - **Our TX offset** (`metadata.frequency`) is placed in the **calling
    ///   region** `[300, 900]` Hz, derived deterministically from `fox_call` so
    ///   concurrent Hound QSOs spread across the pile-up (avoids
    ///   `Math.random`-style non-determinism and keeps tests reproducible).
    /// - **Fox's RX offset** (`metadata.partner_freq`) is `Some(fox_freq)` —
    ///   where we *hear* the Fox. The relevance gate keys on this when set, so
    ///   the Fox's reply on its own offset is correctly routed to this QSO.
    ///
    /// `fox_grid`, if provided, is the Fox's *own* grid square (for
    /// display/logging metadata); it does **not** affect the TX message content
    /// (our opening `<Fox> <us> <grid>` uses *our* grid from station config,
    /// exactly as the normal Caller path does).
    ///
    /// Task 5 (QSY-on-report) mutates `metadata.frequency` upward when the Fox
    /// answers; this constructor only covers the open-and-call-low phase.
    ///
    /// Existing callers of [`respond_to_cq_with`] are unchanged (their
    /// `hound`/`partner_freq` defaults stay `false`/`None`).
    pub async fn engage_hound(
        &self,
        fox_call: &str,
        fox_freq: f64,
        fox_grid: Option<&str>,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<QsoId, QsoManagerError> {
        // Compute our deterministic low calling offset for this Fox callsign,
        // using the operator-configured (or default) call region bounds.
        let low_offset = self.hound_calling_offset(fox_call);

        // Store the Fox's grid on `their_callsign`'s QSO (for logging); the TX
        // message itself uses *our* grid (station config), which respond_to_cq_with
        // already handles via `self.config.our_grid`.
        // fox_grid is accepted for future metadata use (Task 5 / ADIF tagging);
        // we don't thread it into the opening message (same as how the normal path
        // handles the partner's grid — it's not in the CqResponse frame).
        let _ = fox_grid; // used in future tasks (ADIF tag, logging)

        // Open the QSO as a manual Caller at our LOW calling offset. This
        // bypasses the self-duplicate gate (operator explicitly chose this Fox),
        // sets role=Caller, initiated_by=Manual, emits the opening CqResponse
        // (`<Fox> <us> <our_grid>`), and latches tx_parity=dx_parity.opposite().
        // Pass `partner_freq = Some(fox_freq)` directly so the relevance gate is
        // set atomically at construction (no post-insertion mutation needed).
        let qso_id = self
            .respond_to_cq_with(
                fox_call.to_string(),
                low_offset,
                dx_parity,
                CallInitiation::Manual,
                Some(fox_freq), // Fox's RX offset; routes the Fox's reply via partner_freq
                false,          // Shift+H hound engage is a LOCAL operator action
                None,
            )
            .await?;

        // Stamp the hound=true flag (partner_freq is now set via the ctor param).
        self.stamp_hound_flag(qso_id).await;

        info!(
            "Hound: engaging Fox {} on partner_freq={:.1} Hz, calling low @ {:.1} Hz: {}",
            fox_call, fox_freq, low_offset, qso_id
        );

        Ok(qso_id)
    }

    /// Atomically stamp `metadata.hound = true` on an already-created QSO and
    /// clear any `pending_freq_drift` candidate that accumulated in the
    /// window between construction (`respond_to_cq_with`, which releases the
    /// write lock after insertion) and this call.
    ///
    /// `metadata.hound` is the sole discriminator `maybe_confirm_frequency_drift_at`
    /// uses to skip genuine Hound/Fox QSOs (Hound has its own one-shot QSY
    /// mechanism and the Fox's RX offset is protocol-fixed for the QSO's
    /// life). Before this QSO is stamped `hound=true`, a decode landing in
    /// that window sees `hound=false` + `partner_freq=Some` and is eligible
    /// to record a drift candidate — it can't relatch alone (two sightings at
    /// least 5 seconds apart are required), but without clearing it here the
    /// candidate is never cleared afterwards either: once `hound=true`, the
    /// skip at the top of `maybe_confirm_frequency_drift_at` `continue`s
    /// before reaching the in-tolerance reset, so a stale `pending_freq_drift`
    /// would persist in serialized metadata for the QSO's life (PAN-15 item 2).
    async fn stamp_hound_flag(&self, qso_id: QsoId) {
        // PAN-142 (Codex P2 on PR #371, round 5): a decode landing in the
        // documented construction-window gap (between `respond_to_cq_with`/
        // `engage_hound` creating the QSO and this stamp running) can have
        // `maybe_confirm_frequency_drift_at` note a `pending_freq_drift`
        // candidate — round 1's fix emits a StateChanged for that. This
        // stamp then clears the candidate below with no refresh of its own,
        // so a stale "[DRIFT …]" indicator could sit in the TUI snapshot
        // until an unrelated QSO event happens to refresh it (genuine Hound
        // QSOs are skipped by the drift-confirm mechanism entirely from this
        // point on, so nothing else would ever clear it). Same fix shape as
        // round 1: emit only when there was actually a candidate to clear.
        let mut qsos = self.qsos.write().await;
        if let Some(progress) = qsos.get_mut(&qso_id) {
            progress.metadata.hound = true;
            // NOTE: do NOT insert "HOUND" into metadata.tags — that would
            // produce a bare `<HOUND:4>true` ADIF field, which is not a
            // valid ADIF name (must be `APP_`-prefixed per the ADIF spec)
            // and can trip LoTW. The human-readable COMMENT "HOUND" and the
            // machine-readable `APP_PANCETTA_HOUND` field are both written
            // by `AdifProcessor::qso_to_adif` from `metadata.hound` directly.
            if progress.metadata.pending_freq_drift.take().is_some() {
                let state = progress.state.clone();
                self.emit_state_change(qso_id, state.clone(), state).await;
            }
        }
    }

    /// PAN-386: mark an active, non-Hound QSO as a Fox-role stream.
    ///
    /// Fox mode calls this right after it opens the Fox CQ or answers a
    /// Hound, next to `record_fox_admitted`, the same way `stamp_hound_flag`
    /// marks a Hound after creation. A Fox stream is pinned to its slot (see
    /// `hound_switch_range_for`) and never drift-relatched, so a pending drift
    /// candidate is cleared here, with a state refresh only when one existed.
    /// Returns `false` for an unknown or Hound QSO, or one that is neither
    /// active nor `Completed`.
    pub async fn mark_fox_role(&self, qso_id: QsoId) -> bool {
        let mut qsos = self.qsos.write().await;
        let Some(progress) = qsos.get_mut(&qso_id) else {
            return false;
        };
        // A Hound answered at its RR73 step opens already `Completed` with
        // the closing RR73 still to air; it is a Fox stream too, so its slot
        // stays held through the completion grace.
        let open_or_closing =
            progress.state.is_active() || matches!(progress.state, QsoState::Completed { .. });
        if progress.metadata.hound || !open_or_closing {
            return false;
        }
        progress.metadata.fox = true;
        if progress.metadata.pending_freq_drift.take().is_some() {
            let state = progress.state.clone();
            self.emit_state_change(qso_id, state.clone(), state).await;
        }
        true
    }

    /// PAN-15 item 3: returns `Some(separation)` when a candidate relatched
    /// `partner_freq` has drifted within [`MIN_TX_SEPARATION_HZ`] of our own
    /// TX offset — the caller should warn, since we may now be keying
    /// directly on top of the station we're trying to hear. Returns `None`
    /// when the separation is still adequate. Pure so the boundary can be
    /// unit-tested without a `QsoManager` instance.
    fn tx_separation_warning(new_partner_freq: f64, our_tx_freq: f64) -> Option<f64> {
        let separation = (new_partner_freq - our_tx_freq).abs();
        (separation < MIN_TX_SEPARATION_HZ).then_some(separation)
    }

    /// Respond to a CQ call, explicitly choosing the initiation mode.
    ///
    /// [`CallInitiation::Auto`] preserves the historical behavior
    /// (duplicate gate enforced, no keep-calling). [`CallInitiation::Manual`]
    /// bypasses the duplicate gate and enables manual keep-calling.
    ///
    /// `partner_freq` — when `Some(f)`, the DX's RX audio offset differs from
    /// our TX offset (`frequency`). `metadata.partner_freq` is set to `f` so the
    /// relevance gate routes the DX's replies (which arrive at *their* audio
    /// offset) to this QSO. Pass `None` for the normal Tx=Rx case (no partner
    /// routing needed). This is the same mechanism `engage_hound` uses.
    #[allow(clippy::too_many_arguments)]
    pub async fn respond_to_cq_with(
        &self,
        target_callsign: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
        initiated_by: CallInitiation,
        partner_freq: Option<f64>,
        remote_origin: bool,
        remote_client_key_id: Option<String>,
    ) -> Result<QsoId, QsoManagerError> {
        self.respond_to_cq_with_outcome(
            target_callsign,
            frequency,
            dx_parity,
            initiated_by,
            partner_freq,
            remote_origin,
            remote_client_key_id,
        )
        .await
        .map(|(qso_id, _)| qso_id)
    }

    #[allow(clippy::too_many_arguments)]
    async fn respond_to_cq_with_outcome(
        &self,
        target_callsign: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
        initiated_by: CallInitiation,
        partner_freq: Option<f64>,
        remote_origin: bool,
        remote_client_key_id: Option<String>,
    ) -> Result<(QsoId, ManualCallOutcome), QsoManagerError> {
        if self.config.our_callsign == "NOCALL" || self.config.our_callsign == "N0CALL" {
            return Err(QsoManagerError::Configuration {
                message: format!(
                    "Cannot transmit with placeholder callsign '{}'. Configure your callsign first.",
                    self.config.our_callsign
                ),
            });
        }
        // PAN-23: refuse FT8's literal unresolved-hash placeholder "<...>"
        // outright — shared by `respond_to_cq` (autonomous),
        // `respond_to_cq_manual` (StartQso/DX-Hunter), and `engage_hound`.
        // It carries no identity information and can never be encoded into
        // a transmittable message (see `callsign_is_wire_representable`),
        // so a QSO opened for it is guaranteed to fail later at encode
        // time. Defense in depth: the TUI already filters "<...>" out of
        // every station list an operator can select from (PAN-16/PAN-23);
        // this guards any other path.
        if target_callsign == "<...>" {
            return Err(QsoManagerError::InvalidCallsign {
                callsign: target_callsign,
            });
        }
        // Check for duplicate — but only for autonomous calls. A manual
        // call is an explicit operator decision to work (or re-work) this
        // station, so the self-duplicate gate must not block it.
        if initiated_by == CallInitiation::Auto
            && self.check_duplicate(&target_callsign, frequency).await?
        {
            return Err(QsoManagerError::DuplicateQso {
                callsign: target_callsign,
                frequency,
            });
        }

        // FIX 1: re-calling a station we are ALREADY actively working CONTINUES
        // that QSO instead of superseding it / spawning a duplicate. The
        // on-air failure mode this guards against: an operator mashing Space on
        // one DX previously created a brand-new QSO each press (each
        // superseding the last from the grid step), flooding the single TX
        // worker with stale frames and surfacing the intentional supersede as a
        // scary "QSO … failed: superseded". Now a re-call of an active manual
        // QSO on the same band is an idempotent keep-call: re-send the existing
        // QSO's CURRENT outbound message and return its id. State is untouched
        // (we do NOT reset to RespondingToCq/grid). Only when there is NO active
        // QSO do we fall through to create one (and the genuine
        // re-call-after-terminal case still supersedes any leftover).
        if initiated_by == CallInitiation::Manual {
            if let Some(existing_id) = self
                .find_active_manual_qso_for(&target_callsign, frequency)
                .await
            {
                info!(
                    "Re-call of {} on {:.1} Hz — continuing existing QSO {} (idempotent keep-call, no new QSO)",
                    target_callsign, frequency, existing_id
                );
                let outcome = self
                    .resend_existing_if_permitted(existing_id, remote_origin, remote_client_key_id)
                    .await;
                return Ok((existing_id, outcome));
            }
        }

        // FIX 3: supersede any existing active QSO with this callsign on the
        // same band before creating the new one. With FIX 1 above this should
        // now only ever fire for the genuine case (the older QSO already went
        // terminal but its mapping/record lingered before cleanup). Operator
        // policy: "if there are two exchanges on the same band from the same
        // callsign, use the state of whichever is more recent." We retire the
        // older one (→ Failed{Superseded}, mapping removed) so exactly one QSO
        // per (callsign, band) remains active.
        self.supersede_active_qsos_for(&target_callsign, frequency)
            .await;

        let qso_id = Uuid::new_v4();
        let now = self.now_utc();
        // When `dx_parity` is `None` (e.g. answering a DX-cluster/DX-Hunter
        // spot that has never actually been decoded live, so there is no
        // observed slot parity to oppose), fall back to the same "nearest
        // next slot" latch the CQ paths use instead of leaving `tx_parity`
        // permanently `None` (which would make the TX scheduler re-resolve
        // "nearest next slot" independently every subsequent slot and
        // alternate parity — the exact failure `latch_cq_parity_if_none` was
        // built to prevent). Mark the latch `tx_parity_provisional` so the
        // first real decode from this partner can refine it to the true
        // opposite-of-DX parity (see `process_message_for_qso`).
        let (tx_parity, tx_parity_provisional) = match dx_parity {
            Some(p) => (Some(p.opposite()), false),
            None => (Self::latch_cq_parity_if_none(None, now), true),
        };

        let state = QsoState::RespondingToCq {
            target_callsign: target_callsign.clone(),
            frequency,
            started_at: now,
        };

        let mut metadata = QsoMetadata {
            qso_id,
            our_callsign: self.config.our_callsign.clone(),
            their_callsign: Some(target_callsign.clone()),
            frequency,
            completed_rf_frequency_hz: None,
            mode: self.config.active_mode.clone(),
            first_answer_at: None,
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares {
                ours: self.config.our_grid.clone(),
                theirs: None,
            },
            their_state: None,
            contest_info: None,
            tags: HashMap::new(),
            adif_field_types: HashMap::new(),
            notes: None,
            tx_parity,
            initiated_by,
            // We answered the DX's CQ → Caller role.
            role: QsoRole::Caller,
            // The first call is emitted immediately below (the CqResponse
            // MessageToSend), so the count starts at 1.
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound: false,
            // When our TX offset != the DX's RX offset (Hold mode / de-conflict),
            // the caller supplies `partner_freq = Some(dx_freq)` so the relevance
            // gate routes the DX's reply (which arrives at their own audio offset)
            // to this QSO. `None` = Tx=Rx (legacy behavior, unchanged).
            partner_freq,
            pending_freq_drift: None,
            hound_qsyed: false,
            fox: false,
            remote_origin,
            remote_client_key_id: remote_client_key_id.clone(),
            tx_parity_provisional,
        };
        self.enroll_manual_contest(&mut metadata);

        // Send response message
        let message = MessageType::CqResponse {
            calling_station: target_callsign.clone(),
            responding_station: self.config.our_callsign.clone(),
            grid: self.config.our_grid.clone(),
        };

        // Record the initial outbound call as a Sent message so it can be
        // re-sent later (see `resend_last_tx`) and surfaced to the UI. The
        // raw_text is the rendered FT8 text so the TUI "TX:" line shows what
        // we sent (UX audit Batch 2 — was String::new() → blank line).
        let raw_text = self.render_sent_text(&message);
        let progress = QsoProgress {
            state: state.clone(),
            state_history: vec![],
            messages: vec![QsoMessage {
                timestamp: now,
                direction: MessageDirection::Sent,
                message_type: message.clone(),
                raw_text,
                signal_strength: None,
                frequency,
            }],
            metadata,
        };

        self.qsos.write().await.insert(qso_id, progress);
        self.add_callsign_mapping(&target_callsign, qso_id).await;

        // Announce the QSO's birth into its initial active state BEFORE the
        // first MessageToSend. The coordinator's `active_tx_qsos` populater
        // inserts a qso_id only on `StateChanged` into an active state; the TX
        // worker's drop-stale-TX gate (Step 4b) then drops any TransmitRequest
        // whose qso_id is absent. Both the StateChanged and the MessageToSend
        // are consumed by the SAME serial event loop in the coordinator, so
        // emitting StateChanged first guarantees the insert is ordered ahead of
        // the TransmitRequest the MessageToSend produces — otherwise the very
        // first scheduled call is silently dropped and PTT never keys (the
        // operator-reported "scheduled QSO never keys PTT, but manual/tune
        // do" bug).
        self.emit_state_change(qso_id, QsoState::Idle, state).await;

        let attempt_id = self.next_attempt_id();
        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
            remote_origin,
            remote_client_key_id,
            attempt_id: Some(attempt_id),
        })
        .await;

        info!(
            "Responding to CQ from {} on {:.1} Hz: {}",
            target_callsign, frequency, qso_id
        );

        Ok((qso_id, ManualCallOutcome::Opened(attempt_id)))
    }

    /// Respond to a station **calling us**, opening the exchange at an
    /// operator-chosen [`ResponseStep`] instead of always sending our grid.
    ///
    /// This is the engine half of the TUI "Callers" panel. The operator picks
    /// a caller and pancetta classifies what they sent (their CQ/grid →
    /// `Grid`, their report → `ReportAck`, etc.), with a manual override. We
    /// reuse all of [`respond_to_cq_with`](Self::respond_to_cq_with)'s manual
    /// machinery — self-duplicate-gate bypass, superseding a same-call QSO,
    /// latching `tx_parity = dx_parity.opposite()`, and per-slot keep-calling
    /// under the manual watchdog — but set the *initial* [`QsoState`] and emit
    /// the *first* [`MessageType`] according to `step`:
    ///
    /// | step         | initial state          | first message      |
    /// |--------------|------------------------|--------------------|
    /// | `Grid`       | `RespondingToCq`       | `CqResponse` (grid)|
    /// | `Report`     | `SendingReport`        | `SignalReport`     |
    /// | `ReportAck`  | `SendingReport`        | `ReportAck`        |
    /// | `Rr73`       | `WaitingForConfirmation` | `FinalConfirmation` |
    /// | `SeventyThree` | `Completed`          | `SeventyThree` (+ QsoCompleted) |
    ///
    /// `our_snr_of_them` is our measurement of the caller's signal; it
    /// produces the report we send (rounded, clamped to −30..50, defaulting to
    /// −15 if absent). `their_report` is the report they sent us, if known —
    /// used to populate the `their_report` field of the `SendingReport` /
    /// `WaitingForConfirmation` state for `ReportAck`/`Rr73` opens.
    ///
    /// `partner_freq` — when `Some(f)`, our TX offset (`frequency`) differs from
    /// the DX's RX offset `f`. Set by the coordinator when the operator holds a
    /// custom TX offset or when de-confliction nudges us away from the DX's freq.
    /// `None` = Tx=Rx (legacy behavior, unchanged).
    ///
    /// The `Grid` step is exactly equivalent to `respond_to_cq_manual`, so the
    /// DX-Hunter path (which still uses `StartQso` → `respond_to_cq_manual`) is
    /// unaffected.
    // arg count is intrinsic to the QSO-open signature
    #[allow(clippy::too_many_arguments)]
    pub async fn respond_to_caller(
        &self,
        target: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
        step: pancetta_core::ResponseStep,
        our_snr_of_them: Option<f32>,
        their_report: Option<i8>,
        partner_freq: Option<f64>,
        remote_origin: bool,
        remote_client_key_id: Option<String>,
    ) -> Result<QsoId, QsoManagerError> {
        self.respond_to_caller_with_outcome(
            target,
            frequency,
            dx_parity,
            step,
            our_snr_of_them,
            their_report,
            partner_freq,
            remote_origin,
            remote_client_key_id,
        )
        .await
        .map(|(qso_id, _)| qso_id)
    }

    /// [`Self::respond_to_caller`], also reporting whether a frame was emitted (PAN-138).
    #[allow(clippy::too_many_arguments)]
    pub async fn respond_to_caller_with_outcome(
        &self,
        target: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
        step: pancetta_core::ResponseStep,
        our_snr_of_them: Option<f32>,
        their_report: Option<i8>,
        partner_freq: Option<f64>,
        remote_origin: bool,
        remote_client_key_id: Option<String>,
    ) -> Result<(QsoId, ManualCallOutcome), QsoManagerError> {
        use pancetta_core::ResponseStep;

        // PAN-23: refuse FT8's literal unresolved-hash placeholder "<...>"
        // up front, before branching on `step`. This is the exact failure
        // mode observed on-air (production logs 2026-08-20..22): an
        // operator selected "<...>" from the TUI Callers panel at step
        // ReportAck, pancetta queued/started the QSO, and the eventual
        // encode attempt failed with "callsign '<...>' cannot be
        // represented in any FT8 message format". The `step == Grid` branch
        // below would also be caught by the identical guard in
        // `respond_to_cq_with`, but checking here first (a) covers every
        // other step (Report/ReportAck/Rr73/SeventyThree) that bypasses
        // `respond_to_cq_with` entirely, and (b) fails fast before any
        // logging/queueing work happens. Defense in depth: the TUI already
        // filters "<...>" out of the Callers list an operator can select
        // from (PAN-16/PAN-23); this guards any other path — present or
        // future — that might route a `RespondToCaller` command here.
        if target == "<...>" {
            return Err(QsoManagerError::InvalidCallsign { callsign: target });
        }

        // Grid is exactly the historical manual-call behavior; route through
        // the existing path so there is a single source of truth for it.
        if step == ResponseStep::Grid {
            return self
                .respond_to_cq_with_outcome(
                    target,
                    frequency,
                    dx_parity,
                    CallInitiation::Manual,
                    partner_freq,
                    remote_origin,
                    remote_client_key_id,
                )
                .await;
        }

        if self.config.our_callsign == "NOCALL" || self.config.our_callsign == "N0CALL" {
            return Err(QsoManagerError::Configuration {
                message: format!(
                    "Cannot transmit with placeholder callsign '{}'. Configure your callsign first.",
                    self.config.our_callsign
                ),
            });
        }

        // Our report of their signal (the report WE send), same formula the
        // auto-sequencer uses in `MessageExchange::generate_response`, but
        // defaulting to -15 when we have no measurement.
        let our_report: SignalReport = our_snr_of_them
            .map(|s| (s.round() as i8).clamp(-30, 50))
            .unwrap_or(-15);
        // Their report of us (only meaningful for ReportAck/Rr73 opens).
        let their_report_val: SignalReport = their_report.unwrap_or(-15);

        // FIX 1: if we already have an ACTIVE manual QSO with this caller on
        // this band, CONTINUE it instead of superseding/duplicating. Mashing a
        // context reply on a station already in progress must keep ONE QSO per
        // (callsign, band).
        //   - If the requested step is AHEAD of the existing QSO's current
        //     ladder stage (e.g. the DX now sent RR73 → SeventyThree while we
        //     were in SendingReport), advance the EXISTING QSO to emit that
        //     step.
        //   - If it matches (or is behind) the current stage, just re-send the
        //     existing QSO's current outbound (idempotent keep-call).
        // Either way we return the existing id and never create a second QSO.
        if let Some(existing_id) = self.find_active_manual_qso_for(&target, frequency).await {
            let existing_rank = {
                let qsos = self.qsos.read().await;
                qsos.get(&existing_id)
                    .and_then(|p| Self::ladder_rank(&p.state))
            };
            let requested_rank = Self::step_ladder_rank(step);
            match (existing_rank, requested_rank) {
                (Some(cur), Some(req)) if req > cur => {
                    info!(
                        "Context reply to {} at step {:?} — advancing existing QSO {} \
                         (ahead of its current stage)",
                        target, step, existing_id
                    );
                    // Round 13 / PAN-138: advancing can complete and log a
                    // contact. Check permission inside the advance's write
                    // guard before changing identity or exchange state.
                    let outcome = self
                        .advance_existing_qso_to_step(
                            existing_id,
                            &target,
                            frequency,
                            step,
                            our_report,
                            their_report_val,
                            remote_origin,
                            remote_client_key_id,
                        )
                        .await?;
                    return Ok((existing_id, outcome));
                }
                _ => {
                    info!(
                        "Context reply to {} at step {:?} — re-sending existing QSO {} \
                         current outbound (idempotent keep-call)",
                        target, step, existing_id
                    );
                    let outcome = self
                        .resend_existing_if_permitted(
                            existing_id,
                            remote_origin,
                            remote_client_key_id,
                        )
                        .await;
                    return Ok((existing_id, outcome));
                }
            }
        }

        // FIX B (one-QSO-per-(callsign,band) close idempotency,
        // docs/qso-tx-deep-review-2026-07-18.md): no ACTIVE QSO with this
        // caller (FIX 1 above didn't fire) — but if the requested step is a
        // CLOSE step (Rr73/SeventyThree) and we JUST completed a manual QSO
        // with this exact station on this band, within the grace window, this
        // is the operator mashing "send RR73"/"send 73" again after the QSO
        // already finished (observed on-air: 3 SeventyThree presses in 8s
        // produced 4 duplicate ADIF log entries for the same contact). Re-key
        // the EXISTING completed QSO's last frame instead of opening a new
        // one — never spawn a sibling `QsoId`, never emit a second
        // `QsoCompleted`. Grid/Report/ReportAck (ladder rank < 2) fall
        // through to the normal new-QSO path below: a fresh call at those
        // steps within the grace window is the deliberate legitimate-rework
        // case (e.g. working the same station again on a different exchange).
        if Self::step_ladder_rank(step) >= Some(2) {
            if let Some(existing_id) = self
                .find_recently_completed_manual_qso_for(
                    &target,
                    frequency,
                    COMPLETED_QSO_REWORK_GRACE,
                )
                .await
            {
                info!(
                    "Context reply to {} at step {:?} — DX already worked within the \
                     grace window, re-sending existing QSO {}'s last frame",
                    target, step, existing_id
                );
                let outcome = self
                    .resend_existing_if_permitted(existing_id, remote_origin, remote_client_key_id)
                    .await;
                return Ok((existing_id, outcome));
            }
        }

        // Round-16 review (Codex P1): this identity-bound permission check
        // (round-15's fix) MUST run before `supersede_active_qsos_for`
        // below, not after — an autonomous same-call/band QSO is invisible
        // to the manual-QSO lookups above (FIX 1), so a denied close-step
        // request could otherwise reach here, supersede (cancel) that
        // ongoing autonomous exchange, and ONLY THEN get refused itself —
        // destroying a real QSO in progress and replacing it with nothing.
        //
        // Round-15 review (Codex P1): unlike the EXISTING-QSO ladder-advance
        // fix (round 13), a brand-new QSO opened directly at a close step
        // (`SeventyThree`) is created ALREADY `Completed` — the state-build
        // match below sets that unconditionally, and completing immediately
        // emits `QsoCompleted` (ADIF/logbook entry) further down. Doing that
        // for a client the TX layer will deny is worse than the existing-QSO
        // case: the resulting QSO is already terminal, so it can never be
        // retried once that client becomes authorized, and the false log
        // entry cannot be un-logged. Refuse outright rather than create
        // anything — a later, actually-authorized call creates it fresh.
        //
        // PAN-139 (Codex round 23 on PR #362): the same refusal now covers
        // BOTH close steps (`step_ladder_rank >= 2`, the identical predicate
        // FIX B uses above), not just `SeventyThree`. A denied `Rr73` open
        // used to mutate unconditionally: the TX arm gate correctly dropped
        // the unauthorized `FinalConfirmation` frame at key-time, but the
        // QSO was already stranded in `WaitingForConfirmation` with a
        // phantom `Sent` record for a frame that never aired — a state the
        // rearm loop deliberately never retries (`rearm_manual_calls_at`
        // has no keep-call concept past RR73; the DX's own next frame drives
        // that stage), so `confirmation_timeout` retired it as a silently
        // lost QSO. Refusing outright lets a later authorized call open it
        // fresh, and the deferred admission queue already re-queues and
        // retries the refusal within its TTL. `Report`/`ReportAck` opens are
        // NOT covered: their states are rearm-eligible and
        // `frame_reaches_the_air` already refuses to compound a denial, so
        // they self-correct.
        //
        // PAN-280: the check now runs under the SAME `qsos` write guard as the
        // supersede and the insert below (no `qsos` release between them), so
        // no other `qsos` holder can run between the check and the insert.
        // Everything the open needs is built before that guard is taken; the
        // guard's only await is `qsos_by_callsign`, taken in the existing
        // `qsos` -> `qsos_by_callsign` order. A clock-based arm expiry right
        // after the check is still caught only by the TX worker at key time.
        let qso_id = Uuid::new_v4();
        let now = self.now_utc();
        // See the matching comment in `respond_to_cq_with`: a `None`
        // `dx_parity` (no observed decode for this caller yet) falls back to
        // the "nearest next slot" latch and is marked provisional so the
        // first real decode from this partner can refine it.
        let (tx_parity, tx_parity_provisional) = match dx_parity {
            Some(p) => (Some(p.opposite()), false),
            None => (Self::latch_cq_parity_if_none(None, now), true),
        };

        // Build (initial_state, first_message) for the chosen step.
        let (state, message): (QsoState, MessageType) = match step {
            ResponseStep::Grid => unreachable!("Grid handled above"),
            ResponseStep::Report => (
                QsoState::SendingReport {
                    their_callsign: target.clone(),
                    their_report: None,
                    our_report,
                    frequency,
                    started_at: now,
                },
                MessageType::SignalReport {
                    to_station: target.clone(),
                    from_station: self.config.our_callsign.clone(),
                    report: our_report,
                },
            ),
            ResponseStep::ReportAck => (
                QsoState::SendingReport {
                    their_callsign: target.clone(),
                    their_report: Some(their_report_val),
                    our_report,
                    frequency,
                    started_at: now,
                },
                MessageType::ReportAck {
                    to_station: target.clone(),
                    from_station: self.config.our_callsign.clone(),
                    report: our_report,
                },
            ),
            ResponseStep::Rr73 => (
                QsoState::WaitingForConfirmation {
                    their_callsign: target.clone(),
                    their_report: their_report_val,
                    our_report,
                    frequency,
                    grid_square: None,
                    started_at: now,
                },
                MessageType::FinalConfirmation {
                    to_station: target.clone(),
                    from_station: self.config.our_callsign.clone(),
                },
            ),
            ResponseStep::SeventyThree => (
                QsoState::Completed {
                    their_callsign: target.clone(),
                    their_report: their_report_val,
                    our_report,
                    frequency,
                    grid_square: None,
                    completed_at: now,
                    duration_seconds: 0,
                },
                MessageType::SeventyThree {
                    to_station: target.clone(),
                    from_station: self.config.our_callsign.clone(),
                },
            ),
        };

        let mut metadata = QsoMetadata {
            qso_id,
            our_callsign: self.config.our_callsign.clone(),
            their_callsign: Some(target.clone()),
            frequency,
            completed_rf_frequency_hz: None,
            mode: self.config.active_mode.clone(),
            first_answer_at: None,
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares {
                ours: self.config.our_grid.clone(),
                theirs: None,
            },
            their_state: None,
            contest_info: None,
            tags: HashMap::new(),
            adif_field_types: HashMap::new(),
            notes: None,
            tx_parity,
            initiated_by: CallInitiation::Manual,
            // Replying to a station calling us → Caller role.
            role: QsoRole::Caller,
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound: false,
            // When our TX offset != the DX's RX offset (Hold mode / de-conflict),
            // the caller supplies `partner_freq = Some(dx_freq)` so the relevance
            // gate routes the DX's reply (which arrives at their audio offset) to
            // this QSO. `None` = Tx=Rx (legacy behavior, unchanged).
            partner_freq,
            pending_freq_drift: None,
            hound_qsyed: false,
            fox: false,
            remote_origin,
            remote_client_key_id: remote_client_key_id.clone(),
            tx_parity_provisional,
        };
        self.enroll_manual_contest(&mut metadata);

        // If we are opening at the close (SeventyThree → Completed), stamp the
        // completion reports/end-time so the logged record is well-formed.
        let is_completed_open = matches!(state, QsoState::Completed { .. });
        if is_completed_open {
            metadata.reports = SignalReports {
                sent: Some(our_report),
                received: Some(their_report_val),
            };
            metadata.end_time = Some(now);
        }

        let raw_text = self.render_sent_text(&message);

        // If we are opening at the close, the logged record carries the real
        // on-air RF frequency (dial + offset), mirroring the completion path
        // in `process_message_for_qso`. Only the event's clone gets
        // `frequency` overwritten with the RF value -- the STORED entry's
        // `frequency` must stay the audio offset (PAN-25 round 2: see
        // `QsoMetadata::frequency`'s doc comment for why). The stored entry
        // gets `completed_rf_frequency_hz` instead, for the recent-completed
        // suppression's band check. PAN-280: stamped before the insert, under
        // the same guard, instead of by a second write lock afterwards.
        let mut stored_metadata = metadata.clone();
        if is_completed_open {
            let rx_dial = self.dial_frequency_hz.load(Ordering::Relaxed);
            let split = self.split_tx_frequency_hz.load(Ordering::Relaxed);
            let dial = effective_tx_dial(rx_dial, split);
            if dial > 0 {
                let rf_frequency = metadata.frequency + dial as f64;
                metadata.frequency = rf_frequency;
                stored_metadata.completed_rf_frequency_hz = Some(rf_frequency);
            }
        }
        let closing_text = raw_text.clone();
        let progress = QsoProgress {
            state: state.clone(),
            state_history: vec![],
            messages: vec![QsoMessage {
                timestamp: now,
                direction: MessageDirection::Sent,
                message_type: message.clone(),
                raw_text,
                signal_strength: None,
                frequency,
            }],
            metadata: stored_metadata,
        };
        // Captured before the move into the map below so the held
        // QsoCompleted for an open at the close still has it (Layer 2
        // timeline persistence).
        let opening_state_history = progress.state_history.clone();
        let opening_messages = progress.messages.clone();

        // PAN-280: check, supersede and insert under ONE `qsos` write guard.
        // Round-16 / PAN-139 ordering kept: the permission check runs before
        // any supersede mutation, so a denied open mutates nothing.
        let new_band = crate::utils::frequency_to_band(frequency);
        let (superseded, attempt_id) = {
            let mut qsos = self.qsos.write().await;
            if Self::step_ladder_rank(step) >= Some(2)
                && remote_origin
                && !(self.remote_tx_permitted)(remote_client_key_id.as_deref())
            {
                return Err(QsoManagerError::Internal {
                    message: format!(
                        "refusing to open a brand-new QSO with {target} at close step {step:?}: \
                         the bound remote client is not currently TX-permitted"
                    ),
                });
            }
            // Manual: supersede any same-call QSO on this band, then insert
            // the new one (no duplicate gate — the operator explicitly chose
            // this caller). With FIX 1 above this only fires when no ACTIVE
            // QSO remains (e.g. a lingering terminal record), so it should
            // rarely trigger now.
            let candidate_ids = self
                .qsos_by_callsign
                .read()
                .await
                .get(&target.to_uppercase())
                .cloned()
                .unwrap_or_default();
            let superseded =
                Self::supersede_active_locked(&mut qsos, &candidate_ids, &new_band, self.now_utc());
            qsos.insert(qso_id, progress);
            let attempt_id = self.next_attempt_id();
            // PAN-280: an open at the close holds its QsoCompleted until the
            // 73 airs. Stored under the same guard as the insert and before
            // the MessageToSend below, so its TX report always finds it.
            if is_completed_open {
                self.hold_completion(
                    qso_id,
                    PendingCompletion {
                        attempt_ids: vec![attempt_id],
                        raw_text: closing_text,
                        frequency_hz: frequency,
                        remote_origin,
                        remote_client_key_id: remote_client_key_id.clone(),
                        queued_at: now,
                        completed_state: state.clone(),
                        restore: CompletionRestore::RetireOpen,
                        metadata,
                        state_history: opening_state_history,
                        messages: opening_messages,
                    },
                );
            }
            (superseded, attempt_id)
        };
        self.emit_superseded(&target, &new_band, superseded).await;
        self.add_callsign_mapping(&target, qso_id).await;

        // See the matching comment in `respond_to_cq_with`: emit the initial
        // StateChanged BEFORE the first MessageToSend so the coordinator's
        // `active_tx_qsos` set has this qso_id inserted before the first
        // scheduled TransmitRequest reaches the Step 4b PTT gate. A
        // `Completed` open (SeventyThree) is not an active state, but emitting
        // the StateChanged is still correct/harmless — the gate's post-
        // completion grace window keeps the final-73 TransmitRequest live (and
        // the QsoCompleted event below drives the grace insert+removal anyway).
        self.emit_state_change(qso_id, QsoState::Idle, state.clone())
            .await;

        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
            remote_origin,
            remote_client_key_id,
            attempt_id: Some(attempt_id),
        })
        .await;

        info!(
            "Responding to caller {} on {:.1} Hz at step {:?}: {}",
            target, frequency, step, qso_id
        );

        // PAN-280: an open at the close no longer logs here. Its QsoCompleted,
        // with the RF-stamped metadata built above, is held until the 73's
        // `TxFrameSent` (`confirm_completion_aired`).

        Ok((qso_id, ManualCallOutcome::Opened(attempt_id)))
    }

    /// Two-strike confirm-before-relatch at the current time. See
    /// [`Self::maybe_confirm_frequency_drift_at`].
    async fn maybe_confirm_frequency_drift(&self, message_type: &MessageType, frequency: f64) {
        self.maybe_confirm_frequency_drift_at(message_type, frequency, self.now_utc())
            .await;
    }

    /// Two-strike confirm-before-relatch at an explicit time (for testability,
    /// mirroring [`Self::check_timeouts_at`]'s pattern): track a pending off-latch
    /// frequency candidate per QSO, and only relatch once a SECOND message from the
    /// same identified partner repeats at the same new frequency AT LEAST 5 real
    /// seconds after the candidate was FIRST noted. Ordinary Tx=Rx QSOs relatch both
    /// `metadata.frequency` and the active state's embedded `frequency`; non-Hound
    /// split-TX QSOs relatch only `metadata.partner_freq`, preserving our deliberate
    /// TX offset. Runs BEFORE the existing
    /// `find_qsos_for_message`/`is_message_relevant` routing, which stays completely
    /// unmodified — see
    /// `docs/superpowers/specs/2026-07-27-qso-frequency-relatch-v2-design.md`.
    ///
    /// A single off-frequency identity-matching message can't be safely distinguished
    /// from a spoofed frame claiming the partner's callsign (FT8 decoded text carries
    /// no cryptographic identity) — see
    /// `adversarial_3party.rs::b10_partner_call_used_by_other_station_discarded`. Only
    /// a REPEATED match at the same new frequency is trusted — and "repeated" must mean
    /// a genuinely separate transmission, not a second decode-pipeline copy of the same
    /// one. The hb-091 scoped fast-path decodes the same audio window twice and forwards
    /// both copies here (this pre-pass runs before the dedup that
    /// `is_message_relevant` used to provide), landing within milliseconds of each
    /// other, so a bare "matches an existing candidate" check is satisfiable by ONE
    /// physical transmission. The 5s floor is comfortably below FT4's 7.5s slot period
    /// (the shortest slot this app supports), so two genuinely separate transmissions
    /// always clear it while two decode-pipeline copies of one transmission never do.
    ///
    /// A same-frequency sighting that arrives again WITHIN the 5s gap does NOT reset
    /// the pending candidate's timestamp — it is presumed a duplicate delivery of the
    /// same transmission, and the ORIGINAL timestamp is preserved untouched. If the
    /// timestamp were reset on every near-duplicate, repeated fast deliveries (spoofed
    /// or an artifact of the decode pipeline) could push confirmation out indefinitely;
    /// a real DX repeating naturally will eventually land >=5s after the ORIGINAL
    /// sighting and correctly confirm.
    async fn maybe_confirm_frequency_drift_at(
        &self,
        message_type: &MessageType,
        frequency: f64,
        now: DateTime<Utc>,
    ) {
        // Reuses the module-level `ESTABLISHED_FREQ_TOLERANCE_HZ`/
        // `FREQ_TOLERANCE_HZ` shared with `is_message_relevant`/
        // `classify_relevance` (PAN-15 item 6 hoisted all three sites onto
        // one definition — previously each redeclared its own local copy of
        // the same values). `DRIFT_CONFIRM_TOLERANCE_HZ` keeps its own name
        // here since it means something distinct ("counts as the same spot"
        // for two-strike confirmation) even though the value matches
        // `FREQ_TOLERANCE_HZ`.
        const DRIFT_CONFIRM_TOLERANCE_HZ: f64 = FREQ_TOLERANCE_HZ;
        // Confirming sighting must land >=5 real seconds after the ORIGINAL candidate
        // timestamp — comfortably below FT4's 7.5s slot period, so two genuinely
        // separate transmissions always clear it while two decode-pipeline copies of
        // the SAME transmission (milliseconds apart) never do.
        const DRIFT_CONFIRM_MIN_GAP: chrono::Duration = chrono::Duration::seconds(5);
        // Wide RX-plausibility sanity bound for candidate ELIGIBILITY -- deliberately
        // NOT the same as TX_OFFSET_MIN_HZ/MAX_HZ (which govern where we autonomously
        // PICK a fresh TX offset, not where we're willing to consider a real decoded
        // signal a real DX). Responding to a CQ already ties our reply to wherever we
        // decoded it, unclamped, for any DX up to ~2900 Hz -- this must preserve that,
        // not narrow it. The constraint that actually matters here is the modulator's
        // transmittable envelope (`pancetta_ft8::modulator::MAX_FREQUENCY_DEVIATION` =
        // 3100.0, verified to cover a 2900 Hz base plus the widest FT2 tone spread) --
        // NOT `frequency.rs`/`autonomous.rs`'s own convention, which is actually
        // 200-2800 (narrower on the top end) and governs a different concern (where
        // the autonomous allocator prefers to place a fresh CQ/answer).
        const DRIFT_CANDIDATE_MIN_HZ: f64 = 200.0;
        const DRIFT_CANDIDATE_MAX_HZ: f64 = 2900.0;

        let Some(sender) = message_type.sender_callsign() else {
            return;
        };
        if !message_type.is_addressed_to(&self.config.our_callsign) {
            return;
        }

        let mut qsos = self.qsos.write().await;
        for (&qso_id, progress) in qsos.iter_mut() {
            if !progress.state.is_active() {
                continue;
            }
            if progress.metadata.hound || progress.metadata.fox {
                // PAN-386: a Fox-role stream is pinned to its slot, so its TX
                // offset and `partner_freq` never relatch either.
                // Genuine Hound/Fox only. The Fox's RX offset is protocol-fixed for
                // the QSO's life and Hound has its own one-shot QSY mechanism.
                // `partner_freq` alone is not a Hound discriminator: ordinary manual
                // QSOs also set it after an offset hold, collision nudge, or clamp.
                continue;
            }
            let Some(their_callsign) = progress.state.their_callsign().map(|s| s.to_string())
            else {
                continue; // Pre-establishment (CallingCq/Idle) — not this mechanism's scope.
            };
            if !Self::is_partner(sender, &their_callsign) {
                continue;
            }
            let Some(qso_freq) = progress.state.frequency() else {
                continue;
            };
            // Where we expect to hear the DX. This is deliberately identical to
            // is_message_relevant's baseline so both gates agree on their location.
            let split_tx = progress.metadata.partner_freq;
            let match_freq = split_tx.unwrap_or(qso_freq);
            let distance = (match_freq - frequency).abs();

            if distance <= ESTABLISHED_FREQ_TOLERANCE_HZ {
                // PAN-142 (Codex P1 on PR #371): only emit when there was
                // actually a candidate to clear. This branch is the ordinary
                // in-tolerance path every normal exchange takes, so emitting
                // unconditionally here would flood the event stream with a
                // same-state StateChanged on nearly every decoded frame.
                if progress.metadata.pending_freq_drift.take().is_some() {
                    let state = progress.state.clone();
                    self.emit_state_change(qso_id, state.clone(), state).await;
                }
                continue;
            }

            if !(DRIFT_CANDIDATE_MIN_HZ..=DRIFT_CANDIDATE_MAX_HZ).contains(&frequency) {
                continue; // Out-of-band decode — don't let noise reset a real candidate.
            }

            let confirmed = progress.metadata.pending_freq_drift.is_some_and(|(f, t)| {
                (f - frequency).abs() <= DRIFT_CONFIRM_TOLERANCE_HZ
                    && now - t >= DRIFT_CONFIRM_MIN_GAP
            });

            if confirmed {
                progress.metadata.pending_freq_drift = None;
                if split_tx.is_some() {
                    progress.metadata.partner_freq = Some(frequency);
                    info!(
                        target: "qso.freq_gate",
                        partner = %their_callsign,
                        old_partner_freq = match_freq,
                        new_partner_freq = frequency,
                        our_tx_freq = qso_freq,
                        "confirmed partner drift on a split-TX QSO (2 consistent sightings, \
                         >=5s apart) — relatching partner_freq; our TX offset is unchanged"
                    );
                    // PAN-15 item 3: the relatched partner_freq is bounded only
                    // by the RX-plausibility window above — it's never checked
                    // against our own TX offset (`metadata.frequency`) or other
                    // active TX offsets. A DX that drifts onto (or within
                    // MIN_TX_SEPARATION_HZ of) our latched TX offset would have
                    // us keying directly on top of the station we're trying to
                    // hear, silently defeating whatever collision nudge/clamp
                    // separated them originally. No re-deconfliction is
                    // performed (stretch goal); this is the minimum-bar warn.
                    if let Some(tx_separation) =
                        Self::tx_separation_warning(frequency, progress.metadata.frequency)
                    {
                        warn!(
                            target: "qso.freq_gate",
                            partner = %their_callsign,
                            new_partner_freq = frequency,
                            our_tx_freq = progress.metadata.frequency,
                            separation_hz = tx_separation,
                            "relatched partner_freq drifted within MIN_TX_SEPARATION_HZ of our \
                             own TX offset — we may now be keying on top of the station we're \
                             trying to hear; no re-deconfliction performed"
                        );
                    }
                    // The QSO state itself is deliberately untouched here (only
                    // where we HEAR the DX moved; our TX offset did not), so
                    // old_state == new_state. We still emit, because the
                    // coordinator refreshes its scoped/AP decoder hint
                    // (`active_qso_freq_hz`) only in its `StateChanged` handler,
                    // re-reading `metadata.partner_freq` there. Without this the
                    // decoder stays centred on the obsolete partner offset while
                    // the relevance gate has already moved to the new one —
                    // a confirmation arriving on a frame that leaves the state
                    // unchanged (e.g. a repeated `SignalReport` in
                    // `SendingReport`) has no other transition to piggyback on,
                    // so weak replies at the relatched frequency would lose
                    // narrow-band/AP recovery until some unrelated transition.
                    // The handler is idempotent for a same-state event: it
                    // re-inserts into `active_tx_qsos`/`active_tx_offsets` (both
                    // keyed on the unchanged TX offset), recomputes AP context,
                    // and re-pushes the TUI snapshot. It triggers no TX.
                    let state = progress.state.clone();
                    self.emit_state_change(qso_id, state.clone(), state).await;
                } else {
                    let old_state = progress.state.clone();
                    progress.metadata.frequency = frequency;
                    match &mut progress.state {
                        QsoState::RespondingToCq {
                            frequency: state_freq,
                            ..
                        }
                        | QsoState::WaitingForReport {
                            frequency: state_freq,
                            ..
                        }
                        | QsoState::SendingReport {
                            frequency: state_freq,
                            ..
                        }
                        | QsoState::WaitingForConfirmation {
                            frequency: state_freq,
                            ..
                        }
                        | QsoState::SendingConfirmation {
                            frequency: state_freq,
                            ..
                        }
                        | QsoState::Contest(ContestState::ExchangingInfo {
                            frequency: state_freq,
                            ..
                        })
                        | QsoState::Contest(ContestState::ContestCompleted {
                            frequency: state_freq,
                            ..
                        }) => {
                            *state_freq = frequency;
                        }
                        _ => {
                            debug_assert!(
                                false,
                                "confirmed drift relatch has no frequency-field mutation arm for \
                                 this QsoState variant — metadata.frequency and the state's own \
                                 frequency field are now desynced"
                            );
                        }
                    }
                    info!(
                        target: "qso.freq_gate",
                        partner = %their_callsign,
                        old_freq = qso_freq,
                        new_freq = frequency,
                        "confirmed frequency drift (2 consistent sightings, >=5s apart) — \
                         relatching QSO"
                    );
                    self.emit_state_change(qso_id, old_state, progress.state.clone())
                        .await;
                }
            } else {
                // Only start (or leave untouched) a pending candidate — see the
                // "duplicate delivery" doc comment above for why an existing
                // candidate's timestamp is never bumped forward here.
                if progress
                    .metadata
                    .pending_freq_drift
                    .is_none_or(|(f, _)| (f - frequency).abs() > DRIFT_CONFIRM_TOLERANCE_HZ)
                {
                    progress.metadata.pending_freq_drift = Some((frequency, now));
                    debug!(
                        target: "qso.freq_gate",
                        partner = %their_callsign,
                        candidate_freq = frequency,
                        latched_freq = match_freq,
                        "identity-verified message outside tolerance — noting drift candidate \
                         (needs 1 more confirming sighting >=5s later)"
                    );
                    // PAN-142 (Codex P1 on PR #371): a new/changed candidate
                    // is the ONE event the operator-visible drift indicator
                    // exists to surface — without this, `pending_freq_drift`
                    // is set here but nothing re-triggers the TUI snapshot
                    // that carries it (this mechanism's own state-machine
                    // state is deliberately untouched, so no OTHER event
                    // naturally fires), and the indicator built for exactly
                    // this "am I stuck or about to self-heal" moment would
                    // never actually render it. Mirrors the confirmed-split-TX
                    // branch's own same-state emit above.
                    let state = progress.state.clone();
                    self.emit_state_change(qso_id, state.clone(), state).await;
                }
            }
        }
    }

    /// Process an incoming message.
    ///
    /// Does not carry a decoded slot parity — the first-decode provisional-
    /// parity refinement (see `process_message_for_qso`) is a no-op on this
    /// path. Callers that have a real observed slot parity for the decode
    /// (the live coordinator decode path) should use
    /// [`Self::process_message_with_parity`] instead so a provisionally-
    /// latched QSO parity can be refined.
    pub async fn process_message(
        &self,
        message_type: MessageType,
        raw_text: String,
        frequency: f64,
        signal_strength: Option<f32>,
    ) -> Result<(), QsoManagerError> {
        self.process_message_with_parity(message_type, raw_text, frequency, signal_strength, None)
            .await
    }

    /// Process an incoming message that carries a known decoded slot parity.
    ///
    /// Identical to [`Self::process_message`] except `observed_slot_parity` is
    /// threaded through to `process_message_for_qso`, where it drives the
    /// first-decode provisional-parity refinement: a QSO whose `tx_parity`
    /// was latched without ever having observed the DX's real slot parity
    /// (see `QsoMetadata::tx_parity_provisional`) gets that latch corrected
    /// to the true opposite-of-DX parity the first time a verified frame from
    /// the latched partner arrives. Pass `None` when the slot parity of this
    /// decode is not tracked (byte-identical to `process_message`).
    pub async fn process_message_with_parity(
        &self,
        message_type: MessageType,
        raw_text: String,
        frequency: f64,
        signal_strength: Option<f32>,
        observed_slot_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<(), QsoManagerError> {
        let timestamp = self.now_utc();

        // PAN-49: an otherwise-unclassifiable decode may be a contest ack
        // (e.g. "R"+grid) that `MessageExchange::parse_message` has no
        // pattern for. Only reinterpret it when the incoming frame's sender
        // matches the PARTNER of a QSO that is itself contest-engaged
        // (`engage_contest_profile`) — scoped per-QSO, not "any QSO
        // anywhere is engaged" (final review Finding 2: that global gate let
        // a frame from an unrelated third party get reclassified and routed
        // to someone else's engaged QSO). This keeps every QSO whose partner
        // did not send this frame — including every non-contest QSO —
        // byte-identical to today; Finding 1's sender-gated grid latch is
        // the remaining defense-in-depth layer once a frame does reach the
        // transition arm.
        let message_type = match &message_type {
            MessageType::NonStandard { .. } => {
                match crate::contest::matcher::match_grid_with_r_ack(&raw_text) {
                    Some(m) => {
                        let any_engaged = {
                            let qsos = self.qsos.read().await;
                            qsos.values().any(|p| {
                                p.state.is_active()
                                    && Self::grid_contest_engaged(&p.metadata)
                                    && p.state.their_callsign().is_some_and(|c| {
                                        crate::exchange::callsigns_match(c, &m.from_station)
                                    })
                            })
                        };
                        if any_engaged {
                            MessageType::ContestReply {
                                to_station: m.to_station,
                                from_station: m.from_station,
                                grid: m.grid,
                                is_ack: true,
                            }
                        } else {
                            message_type
                        }
                    }
                    None => message_type,
                }
            }
            _ => message_type,
        };

        self.maybe_confirm_frequency_drift(&message_type, frequency)
            .await;

        // Find relevant QSO(s)
        let qso_ids = self.find_qsos_for_message(&message_type, frequency).await;

        for qso_id in qso_ids {
            let message = QsoMessage {
                timestamp,
                direction: MessageDirection::Received,
                message_type: message_type.clone(),
                raw_text: raw_text.clone(),
                signal_strength,
                frequency,
            };

            self.process_message_for_qso(qso_id, message, observed_slot_parity)
                .await?;
        }

        Ok(())
    }

    /// Get QSO status
    pub async fn get_qso(&self, qso_id: QsoId) -> Result<QsoProgress, QsoManagerError> {
        let qsos = self.qsos.read().await;
        qsos.get(&qso_id)
            .cloned()
            .ok_or(QsoManagerError::QsoNotFound { qso_id })
    }

    /// Get all active QSOs
    pub async fn get_active_qsos(&self) -> Vec<(QsoId, QsoProgress)> {
        let qsos = self.qsos.read().await;
        qsos.iter()
            .filter(|(_, progress)| progress.state.is_active())
            .map(|(id, progress)| (*id, progress.clone()))
            .collect()
    }

    /// PAN-287: every active QSO, plus the partners of every QSO whose
    /// completion queued a closing frame that has not aired or been settled
    /// yet (PAN-280). Such a QSO is `Completed`, so it is not in the active
    /// list, but its 73 is still TX-live. Both come from one `qsos` read
    /// guard: holds, commits and reverts all run under the write guard, so a
    /// QSO moving between the two sets is never missed by both. A record
    /// whose QSO no longer holds the state it created is skipped, matching
    /// commit and revert.
    pub async fn active_qsos_and_pending_completion_partners(
        &self,
    ) -> (Vec<(QsoId, QsoProgress)>, Vec<String>) {
        // Lock order matches `hold_completion`: `qsos`, then the pending map.
        let qsos = self.qsos.read().await;
        let active = qsos
            .iter()
            .filter(|(_, progress)| progress.state.is_active())
            .map(|(id, progress)| (*id, progress.clone()))
            .collect();
        let pending = self
            .pending_completions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let partners = pending
            .iter()
            .filter_map(|(id, record)| {
                let progress = qsos.get(id)?;
                if progress.state != record.completed_state {
                    return None;
                }
                progress.metadata.their_callsign.clone()
            })
            .collect();
        (active, partners)
    }

    /// Selected Fox identities for the next decode window, under one short read.
    pub async fn active_hound_foxes(&self) -> Vec<String> {
        self.qsos
            .read()
            .await
            .values()
            .filter(|p| p.state.is_active() && p.metadata.hound)
            .filter_map(|p| p.metadata.their_callsign.clone())
            .collect()
    }

    /// Return the TX audio offsets (Hz) of all currently-active (non-terminal)
    /// QSOs. Used by the coordinator at QSO-open time to de-conflict a new QSO's
    /// TX offset against live concurrent streams so no two QSOs transmit on the
    /// same (or near) audio frequency.
    pub async fn active_tx_offsets(&self) -> Vec<f64> {
        let qsos = self.qsos.read().await;
        qsos.values()
            .filter(|p| p.state.is_active())
            .map(|p| p.metadata.frequency)
            .collect()
    }

    /// PAN-386: active TX offsets split into `(fox_role, others)`, with the
    /// same `is_active()` filter as [`Self::active_tx_offsets`]. Fox slot
    /// selection (`fox_slot_offset`) spaces Fox streams 60 Hz apart and keeps
    /// 75 Hz from every other stream.
    ///
    /// A Fox-role QSO that completed within [`COMPLETED_QSO_REWORK_GRACE`]
    /// still holds its slot: its closing RR73 may not have aired yet (the
    /// PAN-280 hold), and a new Hound answered on that slot would collide
    /// with it in the same bundle.
    pub async fn active_tx_offsets_by_fox_role(&self) -> (Vec<f64>, Vec<f64>) {
        let qsos = self.qsos.read().await;
        let now = self.now_utc();
        let mut fox = Vec::new();
        let mut others = Vec::new();
        let holds_slot = |p: &&QsoProgress| {
            p.state.is_active()
                || (p.metadata.fox
                    && matches!(
                        p.state,
                        QsoState::Completed { completed_at, .. }
                            if now - completed_at < COMPLETED_QSO_REWORK_GRACE
                    ))
        };
        for p in qsos.values().filter(holds_slot) {
            if p.metadata.fox {
                fox.push(p.metadata.frequency);
            } else {
                others.push(p.metadata.frequency);
            }
        }
        (fox, others)
    }

    /// Cancel a QSO
    pub async fn cancel_qso(&self, qso_id: QsoId) -> Result<(), QsoManagerError> {
        let mut qsos = self.qsos.write().await;
        if let Some(mut progress) = qsos.remove(&qso_id) {
            let old_state = progress.state.clone();
            progress.state = QsoState::Failed {
                reason: QsoFailureReason::UserCancelled,
                failed_at: self.now_utc(),
                last_state: Box::new(old_state.clone()),
            };
            // Capture metadata before it's consumed below (Batch 4, SM-F5).
            let metadata = progress.metadata.clone();
            // Layer 2 timeline persistence: capture the full timeline before
            // `progress` is dropped at the end of this block — this is the
            // "QSO leaves the active map" discard site.
            let state_history = progress.state_history.clone();
            let messages = progress.messages.clone();

            self.emit_state_change(qso_id, old_state.clone(), progress.state.clone())
                .await;
            // SM-F5: this producer previously emitted only StateChanged →
            // Failed, leaving `QsoEvent::QsoFailed` dead — the coordinator's
            // priority-scoring failure backoff (`record_failure`) subscribes
            // to QsoFailed specifically, so a cancelled QSO never counted
            // against a station's priority score. The coordinator's
            // active_tx_qsos removal is a HashSet::remove on both this event
            // and the StateChanged above, so emitting both is idempotent.
            self.emit_event(QsoEvent::QsoFailed {
                qso_id,
                reason: QsoFailureReason::UserCancelled,
                last_state: old_state,
                metadata,
                state_history,
                messages,
            })
            .await;

            // Remove from callsign mapping
            if let Some(callsign) = progress.metadata.their_callsign.as_ref() {
                self.remove_callsign_mapping(callsign, qso_id).await;
            }

            info!("Cancelled QSO: {}", qso_id);
        }

        Ok(())
    }

    /// Fail a QSO with an explicit `reason`, transitioning it to
    /// `QsoState::Failed` and emitting both `QsoEvent::StateChanged` and
    /// `QsoEvent::QsoFailed` — the same producer pattern `cancel_qso` (fixed
    /// at `UserCancelled`) and the supersede/timeout retirement paths each
    /// use inline, generalized here to an arbitrary reason so callers
    /// outside this module (the coordinator's task supervisor, specifically)
    /// can retire a QSO through the manager's real state machine instead of
    /// constructing a `QsoEvent` by hand. Currently used for
    /// `QsoFailureReason::SupervisorRestart`: when the Qso component's task
    /// is restarted after a panic, every QSO still active at that moment is
    /// surfaced this way rather than silently vanishing from the map.
    pub async fn fail_qso(
        &self,
        qso_id: QsoId,
        reason: QsoFailureReason,
    ) -> Result<(), QsoManagerError> {
        let mut qsos = self.qsos.write().await;
        if let Some(mut progress) = qsos.remove(&qso_id) {
            let old_state = progress.state.clone();
            progress.state = QsoState::Failed {
                reason: reason.clone(),
                failed_at: self.now_utc(),
                last_state: Box::new(old_state.clone()),
            };
            let metadata = progress.metadata.clone();
            let state_history = progress.state_history.clone();
            let messages = progress.messages.clone();

            self.emit_state_change(qso_id, old_state.clone(), progress.state.clone())
                .await;
            self.emit_event(QsoEvent::QsoFailed {
                qso_id,
                reason,
                last_state: old_state,
                metadata,
                state_history,
                messages,
            })
            .await;

            if let Some(callsign) = progress.metadata.their_callsign.as_ref() {
                self.remove_callsign_mapping(callsign, qso_id).await;
            }

            info!("Failed QSO {}: {:?}", qso_id, progress.state);
        }

        Ok(())
    }

    /// PAN-137: end active QSOs bound to another station-agent client, freeing
    /// their TX window and capacity slots. Selection and removal share one
    /// write lock so a QSO already rebound to the new controller is kept.
    /// Emit the normal failure events after releasing that lock.
    pub async fn retire_qsos_bound_to_other_remote_clients(
        &self,
        controller_key_id: &str,
    ) -> Vec<QsoId> {
        let removed: Vec<(QsoId, QsoProgress)> = {
            let mut qsos = self.qsos.write().await;
            let ids: Vec<QsoId> = qsos
                .iter()
                .filter(|(_, p)| {
                    p.state.is_active()
                        && bound_to_other_remote_client(
                            p.metadata.remote_origin,
                            p.metadata.remote_client_key_id.as_deref(),
                            controller_key_id,
                        )
                })
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| qsos.remove(&id).map(|p| (id, p)))
                .collect()
        };
        let mut retired = Vec::with_capacity(removed.len());
        for (qso_id, mut progress) in removed {
            let old_state = progress.state.clone();
            let reason = QsoFailureReason::ControlTransferred;
            progress.state = QsoState::Failed {
                reason: reason.clone(),
                failed_at: self.now_utc(),
                last_state: Box::new(old_state.clone()),
            };
            self.emit_state_change(qso_id, old_state.clone(), progress.state.clone())
                .await;
            self.emit_event(QsoEvent::QsoFailed {
                qso_id,
                reason,
                last_state: old_state,
                metadata: progress.metadata.clone(),
                state_history: progress.state_history.clone(),
                messages: progress.messages.clone(),
            })
            .await;
            if let Some(callsign) = progress.metadata.their_callsign.as_ref() {
                self.remove_callsign_mapping(callsign, qso_id).await;
            }
            info!("Retired QSO {qso_id}: remote control passed to {controller_key_id}");
            retired.push(qso_id);
        }
        retired
    }

    /// Emit a MessageToSend event for a QSO.
    ///
    /// Reads `tx_parity` from the QSO metadata so that every emission
    /// carries the value latched at QSO start, regardless of when this
    /// method is called.  Used by the auto_sequencer internally and
    /// exposed as `pub` so integration tests can drive additional
    /// MessageToSend events without going through the auto_sequencer.
    pub async fn send_message(&self, qso_id: QsoId, message: MessageType, frequency: f64) {
        self.send_attempt_message(qso_id, message, frequency, self.next_attempt_id())
            .await;
    }

    /// [`Self::send_message`] under a given attempt id: the keep-call ledger
    /// attempt the emission recorded, or a fresh unrecorded one (PAN-183).
    async fn send_attempt_message(
        &self,
        qso_id: QsoId,
        message: MessageType,
        frequency: f64,
        attempt_id: TxAttemptId,
    ) {
        let (tx_parity, remote_origin, remote_client_key_id) = self
            .qsos
            .read()
            .await
            .get(&qso_id)
            .map(|p| {
                (
                    p.metadata.tx_parity,
                    p.metadata.remote_origin,
                    p.metadata.remote_client_key_id.clone(),
                )
            })
            .unwrap_or((None, false, None));
        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
            remote_origin,
            remote_client_key_id,
            attempt_id: Some(attempt_id),
        })
        .await;
    }

    /// PAN-138: re-key an EXISTING QSO on behalf of a (possibly remote)
    /// caller. The permission check, the identity rebind (round 3) and the
    /// attempt's bookkeeping share ONE `qsos` write guard with no await
    /// between them, like `rearm_manual_calls_at`'s `frame_reaches_the_air`.
    /// A denied caller rebinds nothing (round 20) and charges nothing
    /// (round 10). `send_message` re-reads the binding when emitting.
    async fn resend_existing_if_permitted(
        &self,
        qso_id: QsoId,
        remote_origin: bool,
        remote_client_key_id: Option<String>,
    ) -> ManualCallOutcome {
        self.resend_existing_where(
            qso_id,
            |_| true,
            Self::last_sent_frame,
            remote_origin,
            remote_client_key_id,
        )
        .await
    }

    /// [`Self::resend_existing_if_permitted`], refusing with `NothingToResend`
    /// when `eligible` rejects the QSO or `frame` builds nothing to send.
    /// `eligible`, `frame`, the permission check and the charge share one
    /// `qsos` write guard (PAN-138, PAN-275).
    async fn resend_existing_where(
        &self,
        qso_id: QsoId,
        eligible: impl FnOnce(&QsoProgress) -> bool,
        frame: impl FnOnce(&QsoProgress) -> Option<MessageType>,
        remote_origin: bool,
        remote_client_key_id: Option<String>,
    ) -> ManualCallOutcome {
        let now = self.now_utc();
        let (message, frequency, attempt_id) = {
            let mut qsos = self.qsos.write().await;
            let Some(progress) = qsos.get_mut(&qso_id) else {
                return ManualCallOutcome::NothingToResend;
            };
            if !eligible(progress) {
                return ManualCallOutcome::NothingToResend;
            }
            if remote_origin && !(self.remote_tx_permitted)(remote_client_key_id.as_deref()) {
                return ManualCallOutcome::NotPermitted;
            }
            progress.metadata.remote_origin = remote_origin;
            progress.metadata.remote_client_key_id = remote_client_key_id;
            let Some(message) = frame(progress) else {
                return ManualCallOutcome::NothingToResend;
            };
            self.charge_frame_locked(qso_id, progress, now, message)
        };
        self.send_attempt_message(qso_id, message, frequency, attempt_id)
            .await;
        ManualCallOutcome::Resent(attempt_id)
    }

    /// PAN-275: send a 73 for a COMPLETED manual QSO by id, for the
    /// coordinator's bounded auto-73. It never opens a QSO, never changes a
    /// state and never emits `QsoCompleted`, however long ago the QSO completed.
    /// It builds the 73 rather than replaying the QSO's last sent frame: a QSO
    /// that completed on a plain incoming 73 sent none, so its last sent frame
    /// is a report or the opening call (Codex P2 on PR #588).
    /// Returns `NothingToResend` unless the QSO exists, is `Completed`, was
    /// `Manual`, is not a Hound, and completed on the band that `frequency` (the
    /// decode's audio offset) maps to now. Like
    /// `resend_existing_if_permitted`, except for the frame it sends, the
    /// eligibility check, the remote permission re-check, the identity rebind
    /// and the charge share one `qsos` write guard.
    pub async fn resend_completed_close(
        &self,
        qso_id: QsoId,
        frequency: f64,
        remote_origin: bool,
        remote_client_key_id: Option<String>,
    ) -> ManualCallOutcome {
        let want_band = crate::utils::frequency_to_band(self.current_rf_frequency(frequency));
        self.resend_existing_where(
            qso_id,
            |p| Self::completed_close_rekeyable(p, &want_band),
            |p| self.closing_73_frame(p),
            remote_origin,
            remote_client_key_id,
        )
        .await
    }

    /// Read-only preview of [`Self::resend_completed_close`]'s eligibility
    /// rule (it does not check remote permission).
    /// The coordinator asks it before registering the QSO's TX liveness, so
    /// a re-key the engine is certain to refuse (another band, not a
    /// completed manual QSO) never keeps the QSO live for the TX worker.
    /// `resend_completed_close` re-checks under its own write guard.
    pub async fn completed_close_rekeyable_now(&self, qso_id: QsoId, frequency: f64) -> bool {
        let want_band = crate::utils::frequency_to_band(self.current_rf_frequency(frequency));
        self.qsos
            .read()
            .await
            .get(&qso_id)
            .is_some_and(|p| Self::completed_close_rekeyable(p, &want_band))
    }

    /// The 73 [`Self::resend_completed_close`] sends: our callsign to the
    /// QSO's station, addressed like the exchange's own 73 (the state's call,
    /// not the logged call a C18 compound upgrade may have lengthened).
    /// `None` when the QSO names no station.
    fn closing_73_frame(&self, p: &QsoProgress) -> Option<MessageType> {
        let their = p
            .state
            .their_callsign()
            .or(p.metadata.their_callsign.as_deref())?;
        Some(MessageType::SeventyThree {
            to_station: their.to_string(),
            from_station: self.config.our_callsign.clone(),
        })
    }

    /// The PAN-275 rule: a COMPLETED, manual, non-Hound QSO worked on
    /// `want_band`.
    fn completed_close_rekeyable(p: &QsoProgress, want_band: &str) -> bool {
        matches!(p.state, QsoState::Completed { .. })
            && p.metadata.initiated_by == CallInitiation::Manual
            && !p.metadata.hound
            && !p.metadata.fox
            && Self::completed_band(p) == want_band
    }

    /// The band a COMPLETED QSO was worked on: its completion-time RF
    /// frequency when stamped, else its stored `metadata.frequency` (PAN-25).
    fn completed_band(p: &QsoProgress) -> String {
        crate::utils::frequency_to_band(
            p.metadata
                .completed_rf_frequency_hz
                .unwrap_or(p.metadata.frequency),
        )
    }

    /// Test seam (PAN-275): move a completed QSO's completion time, and its
    /// `end_time` when set, back by `by`, so tests in other crates can exercise
    /// the windows measured from `completed_at` without sleeping. Returns `false`
    /// and changes nothing unless the QSO exists and is `Completed`. Not part of
    /// the supported API; hidden from rustdoc like `pancetta_qso::sim`.
    #[doc(hidden)]
    pub async fn backdate_completion_for_test(&self, qso_id: QsoId, by: chrono::Duration) -> bool {
        let mut qsos = self.qsos.write().await;
        let Some(progress) = qsos.get_mut(&qso_id) else {
            return false;
        };
        let QsoState::Completed { completed_at, .. } = &mut progress.state else {
            return false;
        };
        *completed_at -= by;
        if let Some(end) = progress.metadata.end_time.as_mut() {
            *end -= by;
        }
        true
    }

    /// Read `qso_id`'s CURRENT remote-identity binding rather than trusting a
    /// value captured before the caller released the `qsos` write lock — a
    /// concurrent [`Self::resend_existing_if_permitted`] can change it in that
    /// window, and acting on a stale snapshot afterward can misattribute a
    /// reply's origin/client to the wrong controller (Codex P1, PR #362
    /// round 17). Falls back to `snapshot` only if the QSO itself is gone by
    /// the time this runs.
    async fn current_remote_binding(
        &self,
        qso_id: QsoId,
        snapshot: (bool, Option<String>),
    ) -> (bool, Option<String>) {
        self.qsos
            .read()
            .await
            .get(&qso_id)
            .map(|p| {
                (
                    p.metadata.remote_origin,
                    p.metadata.remote_client_key_id.clone(),
                )
            })
            .unwrap_or(snapshot)
    }

    // Called under the qsos write lock at either stall-reset site. Those
    // contributions are already gone; delayed failure must not subtract
    // them from a later streak. `tx_slot` is cleared too: a reset must
    // also retire the slot key, or the PAN-128 heir rule in
    // `roll_back_replaced_attempt_at` could hand a post-reset count to a
    // pre-reset attempt that feeds the same slot (its increment was
    // deliberately cleared here). Call-count and timeline eligibility
    // remain.
    fn clear_recorded_stall_increments(&self, qso_id: QsoId) {
        let mut ledger = self
            .keep_call_ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = ledger.get_mut(&qso_id) {
            for attempt in &mut entry.attempts {
                attempt.bumped_stall = false;
                attempt.tx_slot = None;
            }
            entry.stall_action_revoked = false;
            entry.pending_stall_trigger = None;
        }
    }

    fn next_attempt_id(&self) -> TxAttemptId {
        TxAttemptId(self.attempt_id_counter.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn record_keep_call_attempt(
        &self,
        ledger: &mut HashMap<QsoId, KeepCallLedger>,
        qso_id: QsoId,
        attempt: KeepCallAttempt,
    ) {
        let cutoff = attempt.emitted_at
            - Duration::milliseconds(UNSENT_ATTEMPT_RETENTION_SLOTS * self.rearm_slot_millis());
        let entry = ledger.entry(qso_id).or_default();
        entry.last_emitted_at = Some(attempt.emitted_at);
        entry.attempts.push_back(attempt);
        entry
            .attempts
            .retain(|attempt| attempt.emitted_at >= cutoff);
        while entry.attempts.len() > MAX_UNSENT_ATTEMPTS_PER_QSO {
            entry.attempts.pop_front();
        }
    }

    /// Reverse a recent keep-call emission that the TX worker reported as unsent.
    /// No events are emitted; pacing still uses the last emission (PAN-75).
    /// `replaced` is set only for the frame a TX pivot replaced, and
    /// `attempt_id` whenever the reporter knows the frame's ledger attempt
    /// (PAN-183); see [`Self::roll_back_replaced_attempt_at`].
    pub async fn roll_back_unsent_attempt(
        &self,
        qso_id: QsoId,
        message_text: &str,
        replaced: Option<&UnsentAttemptPayload>,
        attempt_id: Option<TxAttemptId>,
    ) -> UnsentAttemptRollback {
        self.roll_back_replaced_attempt_at(
            qso_id,
            message_text,
            replaced,
            attempt_id,
            self.now_utc(),
        )
        .await
    }

    /// Deterministic-clock rollback. Matches newest text first, within four slots.
    /// Opening calls and manual-regression re-sends have no ledger entry.
    pub async fn roll_back_unsent_attempt_at(
        &self,
        qso_id: QsoId,
        message_text: &str,
        now: DateTime<Utc>,
    ) -> UnsentAttemptRollback {
        self.roll_back_replaced_attempt_at(qso_id, message_text, None, None, now)
            .await
    }

    /// Like [`Self::roll_back_unsent_attempt_at`], but with `replaced` it only
    /// matches the newest same-text emission that also carried that payload.
    /// A pivot that changed only the offset or the remote binding leaves two
    /// same-text emissions, and the fresher one aired. When no emission
    /// carries the payload (the replaced frame was an opening call or an
    /// advance, which have no ledger entry) nothing is rolled back: every
    /// same-text entry left may be the frame that aired.
    ///
    /// PAN-183: with `attempt_id` it matches that attempt alone and ignores
    /// text and payload, so an older failed frame never takes a newer
    /// same-text attempt's entry (an operator re-send, or a differently bound
    /// rearm). An id with no surviving attempt is a no-op; it never falls back
    /// to content matching. Content matching applies only to reports without
    /// an id.
    pub async fn roll_back_replaced_attempt_at(
        &self,
        qso_id: QsoId,
        message_text: &str,
        replaced: Option<&UnsentAttemptPayload>,
        attempt_id: Option<TxAttemptId>,
        now: DateTime<Utc>,
    ) -> UnsentAttemptRollback {
        let mut qsos = self.qsos.write().await;
        let Some(progress) = qsos.get_mut(&qso_id).filter(|p| p.state.is_active()) else {
            return UnsentAttemptRollback::QsoNotActive;
        };
        let mut ledger = self
            .keep_call_ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(entry) = ledger.get_mut(&qso_id) else {
            return UnsentAttemptRollback::NoMatchingAttempt;
        };
        let cutoff =
            now - Duration::milliseconds(UNSENT_ATTEMPT_RETENTION_SLOTS * self.rearm_slot_millis());
        entry
            .attempts
            .retain(|attempt| attempt.emitted_at >= cutoff);
        let found = match attempt_id {
            Some(id) => entry
                .attempts
                .iter()
                .rposition(|attempt| attempt.attempt_id == id),
            None => entry.attempts.iter().rposition(|attempt| {
                attempt.raw_text == message_text && replaced.is_none_or(|p| attempt.carries(p))
            }),
        };
        let Some(index) = found else {
            return UnsentAttemptRollback::NoMatchingAttempt;
        };
        let Some(attempt) = entry.attempts.remove(index) else {
            return UnsentAttemptRollback::NoMatchingAttempt;
        };
        if let Some(next) = entry.attempts.get_mut(index) {
            // Do not relink across a forward-advance stamp.
            if next.prev_last_call_at == Some(attempt.emitted_at) {
                next.prev_last_call_at = attempt.prev_last_call_at;
            }
        }
        progress.metadata.call_count = progress.metadata.call_count.saturating_sub(1);
        if progress.metadata.last_call_at == Some(attempt.emitted_at) {
            progress.metadata.last_call_at = attempt.prev_last_call_at;
        }
        // PAN-182: the frame a held stall action was waiting on never aired.
        // Without an heir (below) nothing is left to wait on; the streak
        // decrement and `stall_action_revoked` decide the action instead.
        let trigger_rolled_back = entry
            .pending_stall_trigger
            .as_ref()
            .is_some_and(|trigger| trigger.is_attempt(&attempt));
        if trigger_rolled_back {
            entry.pending_stall_trigger = None;
        }
        if attempt.bumped_stall {
            // PAN-128 (the "heir" rule): a counted attempt and an uncounted
            // twin can feed the SAME own-parity TX slot (two rearms one
            // protocol slot apart). If the counted one is rolled back while
            // a same-slot sibling survives, the sibling inherits the slot's
            // increment and nothing is decremented — the slot still has a
            // frame that can air, so it keeps its count. Only with no
            // surviving sibling does the existing decrement (and the
            // `stall_action_revoked` path below) run.
            let heir = attempt
                .tx_slot
                .and_then(|slot| entry.attempts.iter().rposition(|a| a.tx_slot == Some(slot)));
            match heir {
                Some(index) => {
                    entry.attempts[index].bumped_stall = true;
                    // PAN-182: the heir still carries the slot's count and
                    // can still air, so a held action now waits on it.
                    if trigger_rolled_back {
                        entry.pending_stall_trigger =
                            Some(PendingStallTrigger::from_attempt(&entry.attempts[index]));
                    }
                }
                None => {
                    self.release_one_stall_count(entry, &mut progress.metadata, trigger_rolled_back)
                }
            }
        }
        if attempt.recorded_sent {
            if let Some(index) = progress.messages.iter().rposition(|m| {
                m.direction == MessageDirection::Sent
                    && m.timestamp == attempt.emitted_at
                    && m.raw_text == attempt.raw_text
            }) {
                progress.messages.remove(index);
            }
        }
        UnsentAttemptRollback::RolledBack {
            call_count: progress.metadata.call_count,
        }
    }

    // One own-parity TX slot lost its only counted attempt (a no-heir
    // rollback, or a PAN-223 re-key that found the attempt's count a
    // duplicate): take the count off the streak, revoke a raised action the
    // streak no longer justifies, and re-point a hold the released attempt
    // carried. The caller has already cleared the released attempt's
    // `bumped_stall` (or removed it) and, when it was the trigger, its
    // `pending_stall_trigger`.
    fn release_one_stall_count(
        &self,
        entry: &mut KeepCallLedger,
        metadata: &mut QsoMetadata,
        released_trigger: bool,
    ) {
        let threshold = self.config.timeouts.qso_stall_switch_after;
        let was_at_threshold = metadata.stall_cycles >= threshold;
        metadata.stall_cycles = metadata.stall_cycles.saturating_sub(1);
        if was_at_threshold && metadata.stall_cycles < threshold {
            entry.stall_action_revoked = true;
        }
        // PAN-182 (PR #507 review): with no heir the streak lost one count,
        // but a NEWER counted attempt can keep it at or above the threshold
        // — the trigger was peeled by a payload-qualified rollback after a
        // later own-slot emission already pushed the streak past it. That
        // newer attempt is now the decisive count and its outcome is just as
        // unresolved as the trigger's was, so the hold moves to it instead of
        // vanishing; otherwise the next drain commits before its frame airs
        // (the PAN-182 race again).
        if released_trigger && metadata.stall_cycles >= threshold {
            if let Some(newest) = entry.attempts.iter().rposition(|a| a.bumped_stall) {
                entry.pending_stall_trigger =
                    Some(PendingStallTrigger::from_attempt(&entry.attempts[newest]));
            }
        }
    }

    /// PAN-223: key a keep-call attempt by the slot the TX worker actually
    /// scheduled its frame into (`TxSchedule::target_slot`), replacing the
    /// rearm's `keep_call_tx_slot` prediction. A re-key moves or removes a slot
    /// count, never adds one:
    ///
    /// - an uncounted attempt is re-keyed and stays uncounted; the next rearm
    ///   feeding the new slot counts it, as it would have;
    /// - a counted attempt whose old slot still has a surviving attempt hands
    ///   that slot's count to the newest such sibling (the PAN-128 heir rule);
    /// - otherwise, if another attempt already counts the worker's slot, this
    ///   attempt's count was the duplicate and is released exactly as a
    ///   no-heir rollback releases one (decrement, revoke, hold re-point);
    /// - otherwise the count moves with the attempt.
    ///
    /// A held stall trigger follows its attempt. Ids are authoritative: an id
    /// with no surviving attempt is a no-op. Repeated or late reports converge
    /// in any order.
    pub async fn record_scheduled_tx_slot(
        &self,
        qso_id: QsoId,
        attempt_id: TxAttemptId,
        target_slot: DateTime<Utc>,
    ) -> ScheduledTxSlot {
        let mut qsos = self.qsos.write().await;
        let Some(progress) = qsos.get_mut(&qso_id).filter(|p| p.state.is_active()) else {
            return ScheduledTxSlot::QsoNotActive;
        };
        let mut ledger = self
            .keep_call_ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(entry) = ledger.get_mut(&qso_id) else {
            return ScheduledTxSlot::NoMatchingAttempt;
        };
        let Some(index) = entry
            .attempts
            .iter()
            .position(|attempt| attempt.attempt_id == attempt_id)
        else {
            return ScheduledTxSlot::NoMatchingAttempt;
        };
        let Some(old_slot) = entry.attempts[index].tx_slot else {
            return ScheduledTxSlot::NoSlotKey;
        };
        if old_slot == target_slot {
            return ScheduledTxSlot::Unchanged;
        }
        entry.attempts[index].tx_slot = Some(target_slot);
        let is_trigger = entry
            .pending_stall_trigger
            .as_ref()
            .is_some_and(|trigger| trigger.is_attempt(&entry.attempts[index]));
        if is_trigger {
            if let Some(trigger) = entry.pending_stall_trigger.as_mut() {
                trigger.tx_slot = Some(target_slot);
            }
        }
        if !entry.attempts[index].bumped_stall {
            return ScheduledTxSlot::RekeyedUncounted;
        }
        let heir = entry
            .attempts
            .iter()
            .enumerate()
            .rev()
            .find(|(i, a)| *i != index && a.tx_slot == Some(old_slot))
            .map(|(i, _)| i);
        let outcome = if let Some(heir) = heir {
            entry.attempts[index].bumped_stall = false;
            entry.attempts[heir].bumped_stall = true;
            if is_trigger {
                entry.pending_stall_trigger =
                    Some(PendingStallTrigger::from_attempt(&entry.attempts[heir]));
            }
            ScheduledTxSlot::CountLeftWithSibling
        } else if entry
            .attempts
            .iter()
            .enumerate()
            .any(|(i, a)| i != index && a.bumped_stall && a.tx_slot == Some(target_slot))
        {
            entry.attempts[index].bumped_stall = false;
            if is_trigger {
                entry.pending_stall_trigger = None;
            }
            self.release_one_stall_count(entry, &mut progress.metadata, is_trigger);
            ScheduledTxSlot::DuplicateCountRemoved {
                stall_cycles: progress.metadata.stall_cycles,
            }
        } else {
            ScheduledTxSlot::CountMoved
        };
        debug!(
            target: "qso",
            "PAN-223: QSO {} attempt {:?} re-keyed from {} to the TX worker's slot {}: \
             {:?}, stall_cycles={}",
            qso_id, attempt_id, old_slot, target_slot, outcome, progress.metadata.stall_cycles
        );
        outcome
    }

    /// PAN-182: the TX worker reports a QSO frame that aired, at `aired_at`
    /// (stamped after playback). If it is the frame a raised stall action is
    /// waiting on — same text, same offset and remote binding (see
    /// [`UnsentAttemptPayload`]), reported no earlier than the start of the
    /// own-parity TX slot the trigger feeds — release the hold so the next
    /// drain can commit the action. Every aired frame of an active QSO is
    /// also recorded as its newest aired frame (PAN-83), which the known-good
    /// crediting in `process_message_for_qso` reads. Returns whether a hold
    /// was released.
    pub async fn confirm_stall_trigger_aired(
        &self,
        qso_id: QsoId,
        message_text: &str,
        aired: &UnsentAttemptPayload,
        aired_at: DateTime<Utc>,
    ) -> bool {
        let qsos = self.qsos.write().await;
        if !qsos.get(&qso_id).is_some_and(|p| p.state.is_active()) {
            return false;
        }
        let mut ledger = self
            .keep_call_ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // PAN-83: record every aired frame of an active QSO, hold or no hold
        // — arrival order is airing order, so the newest report is the frame
        // the DX's next reply, if any, answered.
        let entry = ledger.entry(qso_id).or_default();
        entry.last_aired = Some(AiredFrame {
            offset_hz: aired.frequency_hz,
            aired_at,
        });
        if !entry
            .pending_stall_trigger
            .as_ref()
            .is_some_and(|trigger| trigger.aired_as(message_text, aired, aired_at))
        {
            return false;
        }
        entry.pending_stall_trigger = None;
        true
    }

    #[cfg(test)]
    fn keep_call_ledger_len(&self, qso_id: QsoId) -> usize {
        self.keep_call_ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&qso_id)
            .map_or(0, |entry| entry.attempts.len())
    }

    /// PAN-83: the QSO's most recently reported aired frame, if any — what
    /// `answered_frame_offset_hz` reads at crediting time.
    #[cfg(test)]
    fn last_aired_frame(&self, qso_id: QsoId) -> Option<AiredFrame> {
        self.keep_call_ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&qso_id)
            .and_then(|entry| entry.last_aired)
    }

    /// Oldest first: each surviving ledger attempt's id and whether it holds
    /// its slot's stall increment.
    #[cfg(test)]
    fn keep_call_attempt_ids(&self, qso_id: QsoId) -> Vec<(TxAttemptId, bool)> {
        self.keep_call_ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&qso_id)
            .map_or_else(Vec::new, |entry| {
                entry
                    .attempts
                    .iter()
                    .map(|a| (a.attempt_id, a.bumped_stall))
                    .collect()
            })
    }

    /// Oldest first: each surviving attempt's id, whether it holds its slot's
    /// stall increment, and the slot it is keyed to (PAN-223).
    #[cfg(test)]
    fn keep_call_attempt_slots(
        &self,
        qso_id: QsoId,
    ) -> Vec<(TxAttemptId, bool, Option<DateTime<Utc>>)> {
        self.keep_call_ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&qso_id)
            .map_or_else(Vec::new, |entry| {
                entry
                    .attempts
                    .iter()
                    .map(|a| (a.attempt_id, a.bumped_stall, a.tx_slot))
                    .collect()
            })
    }

    /// Re-send the most recent outbound message for a QSO.
    ///
    /// Looks up the QSO, finds the most-recent `Sent` message in its message
    /// log, and re-emits it via the same `MessageToSend` path `send_message`
    /// uses (carrying the QSO's frequency and latched `tx_parity`). Returns
    /// `QsoNotFound` for an unknown id; returns `Ok(())` (a benign no-op) when
    /// the QSO has no prior outbound message to resend.
    ///
    /// Stamps `last_call_at` (and bumps `call_count`) exactly like the
    /// keep-call rearm (`rearm_manual_calls_at`) does on every re-emission,
    /// so an operator-triggered resend is accounted the same way a
    /// rearm-driven send is for watchdog/cap purposes, and so an operator
    /// re-pressing Space/resend near a slot boundary can't produce a
    /// same-text emission the rearm/coalescer machinery doesn't know about.
    pub async fn resend_last_tx(&self, qso_id: QsoId) -> Result<(), QsoManagerError> {
        let now = self.now_utc();
        let emit = {
            let mut qsos = self.qsos.write().await;
            let progress = qsos
                .get_mut(&qso_id)
                .ok_or(QsoManagerError::QsoNotFound { qso_id })?;
            self.charge_resend_locked(qso_id, progress, now)
        };
        match emit {
            Some((message, frequency, attempt_id)) => {
                self.send_attempt_message(qso_id, message, frequency, attempt_id)
                    .await
            }
            None => info!("resend_last_tx: no prior Sent message for QSO {}", qso_id),
        }
        Ok(())
    }

    /// The bookkeeping half of a re-send, run under the caller's `qsos` write
    /// guard: bump `call_count`, stamp `last_call_at` and record the PAN-75
    /// ledger attempt. Returns what to emit (with the recorded attempt's id)
    /// once the guard is dropped, or `None` when the QSO has no prior `Sent`
    /// message (nothing charged).
    fn charge_resend_locked(
        &self,
        qso_id: QsoId,
        progress: &mut QsoProgress,
        now: DateTime<Utc>,
    ) -> Option<(MessageType, f64, TxAttemptId)> {
        let message = Self::last_sent_frame(progress)?;
        Some(self.charge_frame_locked(qso_id, progress, now, message))
    }

    /// The QSO's most recent `Sent` frame, if any.
    fn last_sent_frame(progress: &QsoProgress) -> Option<MessageType> {
        progress
            .messages
            .iter()
            .rev()
            .find(|m| m.direction == MessageDirection::Sent)
            .map(|m| m.message_type.clone())
    }

    /// [`Self::charge_resend_locked`]'s bookkeeping for a given `message`.
    fn charge_frame_locked(
        &self,
        qso_id: QsoId,
        progress: &mut QsoProgress,
        now: DateTime<Utc>,
        message: MessageType,
    ) -> (MessageType, f64, TxAttemptId) {
        let frequency = progress.metadata.frequency;
        let prev_last_call_at = progress.metadata.last_call_at;
        progress.metadata.call_count += 1;
        progress.metadata.last_call_at = Some(now);
        let attempt_id = self.next_attempt_id();
        self.record_keep_call_attempt(
            &mut self
                .keep_call_ledger
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
            qso_id,
            KeepCallAttempt {
                attempt_id,
                emitted_at: now,
                raw_text: self.render_sent_text(&message),
                prev_last_call_at,
                bumped_stall: false,
                tx_slot: None,
                recorded_sent: false,
                frequency_hz: frequency,
                remote_origin: progress.metadata.remote_origin,
                remote_client_key_id: progress.metadata.remote_client_key_id.clone(),
            },
        );
        // PAN-280: a FIX B re-key of a completion still waiting on its 73
        // joins the pending set; whichever of them airs logs the QSO.
        if let Some(pending) = self
            .pending_completions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&qso_id)
        {
            pending.attempt_ids.push(attempt_id);
        }
        (message, frequency, attempt_id)
    }

    /// PAN-280: store a completion that queued a closing frame. Called under
    /// the caller's `qsos` write guard, before the frame's `MessageToSend`.
    fn hold_completion(&self, qso_id: QsoId, pending: PendingCompletion) {
        self.pending_completions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(qso_id, pending);
    }

    /// PAN-280: the TX worker reports a QSO frame aired (`TxFrameSent`). If
    /// it is a pending completion's frame, emit the held `QsoCompleted` once.
    /// With an attempt id only that id matches. A report without one matches
    /// on text, offset and remote binding, and only for a frame that finished
    /// airing after the completion was queued.
    pub async fn confirm_completion_aired(
        &self,
        qso_id: QsoId,
        attempt_id: Option<TxAttemptId>,
        message_text: &str,
        aired: &UnsentAttemptPayload,
        aired_at: DateTime<Utc>,
    ) -> CompletionAired {
        let event = {
            let qsos = self.qsos.read().await;
            let mut pending = self
                .pending_completions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(record) = pending.get(&qso_id) else {
                return CompletionAired::NotPending;
            };
            let matches = match attempt_id {
                Some(id) => record.attempt_ids.contains(&id),
                None => {
                    record.raw_text == message_text
                        && aired_at >= record.queued_at
                        && emission_carries(
                            record.frequency_hz,
                            record.remote_origin,
                            &record.remote_client_key_id,
                            aired,
                        )
                }
            };
            if !matches {
                return CompletionAired::NotPending;
            }
            let Some(record) = pending.remove(&qso_id) else {
                return CompletionAired::NotPending;
            };
            // Never log a QSO something else changed since the queue.
            if !qsos
                .get(&qso_id)
                .is_some_and(|p| p.state == record.completed_state)
            {
                return CompletionAired::NotPending;
            }
            QsoEvent::QsoCompleted {
                qso_id,
                metadata: record.metadata,
                state_history: record.state_history,
                messages: record.messages,
            }
        };
        self.emit_event(event).await;
        CompletionAired::Committed
    }

    /// PAN-280: the TX worker reports a frame not sent (`TxFrameNotSent`).
    /// Drops its attempt from the pending set and reverts the completion when
    /// the set empties. A report without an id never reverts: text alone
    /// cannot tell a pivot-replaced frame from the one that aired (PAN-75);
    /// the expiry covers it.
    pub async fn resolve_unsent_completion(
        &self,
        qso_id: QsoId,
        attempt_id: Option<TxAttemptId>,
    ) -> UnsentCompletion {
        self.resolve_unsent_completion_at(qso_id, attempt_id, self.now_utc())
            .await
    }

    /// Deterministic-clock form of [`Self::resolve_unsent_completion`].
    pub async fn resolve_unsent_completion_at(
        &self,
        qso_id: QsoId,
        attempt_id: Option<TxAttemptId>,
        now: DateTime<Utc>,
    ) -> UnsentCompletion {
        let Some(attempt_id) = attempt_id else {
            return UnsentCompletion::NotPending;
        };
        let reverted = {
            let mut qsos = self.qsos.write().await;
            let record = {
                let mut pending = self
                    .pending_completions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let Some(record) = pending.get_mut(&qso_id) else {
                    return UnsentCompletion::NotPending;
                };
                let Some(pos) = record.attempt_ids.iter().position(|id| *id == attempt_id) else {
                    return UnsentCompletion::NotPending;
                };
                record.attempt_ids.remove(pos);
                if !record.attempt_ids.is_empty() {
                    return UnsentCompletion::StillPending;
                }
                let Some(record) = pending.remove(&qso_id) else {
                    return UnsentCompletion::NotPending;
                };
                record
            };
            self.revert_completion_locked(&mut qsos, qso_id, record, now)
        };
        let Some(reverted) = reverted else {
            return UnsentCompletion::NotPending;
        };
        let to = reverted.to;
        self.emit_reverted_completion(reverted).await;
        UnsentCompletion::Reverted(to)
    }

    /// PAN-280: revert every pending completion whose closing frame has had
    /// no TX outcome for [`PENDING_COMPLETION_EXPIRY_SLOTS`] slots (at least
    /// [`PENDING_COMPLETION_EXPIRY_FLOOR_MS`]). Returns how many reverted.
    pub async fn expire_pending_completions_at(&self, now: DateTime<Utc>) -> usize {
        let expiry = Duration::milliseconds(
            (PENDING_COMPLETION_EXPIRY_SLOTS * self.rearm_slot_millis())
                .max(PENDING_COMPLETION_EXPIRY_FLOOR_MS),
        );
        let reverted: Vec<RevertedCompletion> = {
            let mut qsos = self.qsos.write().await;
            let expired: Vec<(QsoId, PendingCompletion)> = {
                let mut pending = self
                    .pending_completions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let ids: Vec<QsoId> = pending
                    .iter()
                    .filter(|(_, record)| now - record.queued_at >= expiry)
                    .map(|(id, _)| *id)
                    .collect();
                ids.into_iter()
                    .filter_map(|id| pending.remove(&id).map(|record| (id, record)))
                    .collect()
            };
            expired
                .into_iter()
                .filter_map(|(id, record)| {
                    self.revert_completion_locked(&mut qsos, id, record, now)
                })
                .collect()
        };
        let count = reverted.len();
        for one in reverted {
            warn!(
                "QSO {}: no TX outcome for its closing frame before the pending completion expired; not logged",
                one.qso_id
            );
            self.emit_reverted_completion(one).await;
        }
        count
    }

    /// PAN-280: undo a pending completion under the caller's `qsos` write
    /// guard. Acts only while the QSO still holds the record's `Completed`
    /// state. Returns the events to emit once the guard drops.
    fn revert_completion_locked(
        &self,
        qsos: &mut HashMap<QsoId, QsoProgress>,
        qso_id: QsoId,
        record: PendingCompletion,
        now: DateTime<Utc>,
    ) -> Option<RevertedCompletion> {
        let progress = qsos.get_mut(&qso_id)?;
        if progress.state != record.completed_state {
            return None;
        }
        match record.restore {
            CompletionRestore::RetireOpen => {
                let failed = QsoState::Failed {
                    reason: QsoFailureReason::CloseNotAired,
                    failed_at: now,
                    last_state: Box::new(record.completed_state.clone()),
                };
                progress.state = failed.clone();
                Some(RevertedCompletion {
                    qso_id,
                    to: RevertedTo::Retired,
                    old_state: record.completed_state,
                    new_state: failed,
                    metadata: progress.metadata.clone(),
                    state_history: progress.state_history.clone(),
                    messages: progress.messages.clone(),
                })
            }
            CompletionRestore::Revert {
                state,
                reports,
                end_time,
                completed_rf_frequency_hz,
                sent_record,
            } => {
                progress.state = state.clone();
                progress.metadata.reports = reports;
                progress.metadata.end_time = end_time;
                progress.metadata.completed_rf_frequency_hz = completed_rf_frequency_hz;
                if let Some(index) = progress
                    .state_history
                    .iter()
                    .rposition(|t| t.to_state == record.completed_state)
                {
                    progress.state_history.remove(index);
                }
                if let Some((timestamp, text)) = sent_record {
                    if let Some(index) = progress.messages.iter().rposition(|m| {
                        m.direction == MessageDirection::Sent
                            && m.timestamp == timestamp
                            && m.raw_text == text
                    }) {
                        progress.messages.remove(index);
                    }
                }
                self.completion_reverted_at
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(qso_id, now);
                Some(RevertedCompletion {
                    qso_id,
                    to: RevertedTo::Reopened,
                    old_state: record.completed_state,
                    new_state: state,
                    metadata: progress.metadata.clone(),
                    state_history: progress.state_history.clone(),
                    messages: progress.messages.clone(),
                })
            }
        }
    }

    /// PAN-280: announce a reverted completion after the `qsos` guard drops.
    /// A reopened QSO gets a `StateChanged` back to its pre-close state; a
    /// retired open gets `StateChanged` plus `QsoFailed { CloseNotAired }`
    /// and loses its callsign mapping. Neither emits `QsoCompleted`.
    async fn emit_reverted_completion(&self, reverted: RevertedCompletion) {
        let RevertedCompletion {
            qso_id,
            to,
            old_state,
            new_state,
            metadata,
            state_history,
            messages,
        } = reverted;
        self.emit_state_change(qso_id, old_state.clone(), new_state)
            .await;
        if to == RevertedTo::Retired {
            self.emit_event(QsoEvent::QsoFailed {
                qso_id,
                reason: QsoFailureReason::CloseNotAired,
                last_state: old_state,
                metadata: metadata.clone(),
                state_history,
                messages,
            })
            .await;
            if let Some(callsign) = &metadata.their_callsign {
                self.remove_callsign_mapping(callsign, qso_id).await;
            }
        }
    }

    /// External commit point for PAN-72's stall-switch/revert and the manual
    /// nudge keystroke — the only way outside code may change an
    /// already-active QSO's TX offset. Mirrors the mutation the removed
    /// stuck-DX hop used to perform inline; the caller (the coordinator) has
    /// already resolved WHAT the new offset should be (via the smart
    /// allocator for a Switch, or `last_known_good_offset_hz` for a Revert —
    /// `QsoManager` itself never calls the allocator, see the single-scorer
    /// invariant). Does not force an immediate retransmission — the next
    /// naturally-scheduled send picks up the new value since message
    /// construction reads `metadata.frequency` fresh each time.
    ///
    /// The requested offset is clamped defensively to
    /// [`ACTIVE_QSO_TX_OFFSET_MIN_HZ`]..=[`ACTIVE_QSO_TX_OFFSET_MAX_HZ`] —
    /// this is a `pub` method documented as the sole external mutator of an
    /// active QSO's TX offset, so it must not be able to park a QSO outside
    /// the transmittable envelope from outside the crate. Every current
    /// caller already supplies an in-band value.
    ///
    /// NOT [`TX_OFFSET_MIN_HZ`]..=[`TX_OFFSET_MAX_HZ`] (300–2700), the band
    /// the removed stuck-DX hop used: those govern where we autonomously
    /// *pick a fresh* offset, not where an in-progress QSO is allowed to sit.
    /// A `Revert`'s `target_hz` is a `last_known_good_offset_hz` that may be
    /// an unclamped reply offset up to ~2900 Hz (answering a CQ ties our reply
    /// to wherever we decoded the DX), and narrowing that to 2700 would move
    /// the QSO off the very offset it demonstrably worked on. See
    /// [`ACTIVE_QSO_TX_OFFSET_MIN_HZ`]'s doc comment.
    ///
    /// Returns the offset that was **actually applied** (post-clamp), so the
    /// coordinator can mirror it into its own `active_tx_offsets` snapshot
    /// without re-deriving the clamp.
    ///
    /// These guards run BEFORE anything is mutated. Guards 1–3 are staleness
    /// guards, all because the coordinator only drains its request mailbox
    /// once per 15-second slot, so a lot can happen between an action being
    /// raised and committed; guard 4 rejects a move that isn't one:
    ///
    /// 1. **Terminal QSO** (Codex round 1 on PR #350, finding 3). Completed
    ///    entries are retained in the map, and completion deliberately keeps
    ///    the QSO's id and offset in the coordinator's active snapshots for a
    ///    45-second trailing-73 grace window — so a late action still *finds*
    ///    its QSO. `QsoState::set_frequency` already refuses terminal states,
    ///    but `metadata.frequency` has no such guard: writing it would leave
    ///    the metadata disagreeing with the logged state, and the drain would
    ///    mirror that phantom offset into `active_tx_offsets` while the queued
    ///    final frame still goes out on the original one. Refuse outright.
    /// 2. **Superseded by a DX advance** (finding 8). If the QSO's
    ///    [`crate::states::QsoMetadata::advance_generation`] has moved since
    ///    the origin's `raised_at_generation` was captured, the DX answered in
    ///    the meantime: `stall_cycles` is already reset and the CURRENT offset
    ///    is already recorded known-good, so applying the queued action would
    ///    move us off the offset that just demonstrably worked.
    ///    [`OffsetRelocationOrigin::OperatorForced`] carries no generation —
    ///    an ordinary advance must never discard the nudge this way; guard 2a
    ///    covers the advances that do supersede it.
    ///
    ///    **2a. Operator nudge superseded by DX progress** (PAN-131, Codex
    ///    round 13 on PR #350, P2). An `OperatorForced` commit is refused when
    ///    the QSO's current state fails [`operator_nudge_retransmits`]: the DX
    ///    moved it, after the keypress, into a state neither
    ///    `rearm_manual_calls_at` nor this method's one-shot retransmission
    ///    sends anything from, so committing would move the offset with no
    ///    frame ever sent on it. An advance that stays inside the
    ///    nudge-eligible states (`RespondingToCq` → `SendingReport`) still
    ///    commits — a generation token would have refused that too.
    /// 3. **Outside the live Hound region** (Codex round 4, finding 2). A
    ///    Hound's TX offset is procedurally pinned to the calling region before
    ///    the Fox answers and to the response region after the QSY, and the
    ///    coordinator picks that window from a `get_qso` snapshot taken before
    ///    the allocator runs. The QSY can land in between — and for an
    ///    operator-forced `u` nudge, which carries no generation, guard 2
    ///    cannot catch it — so the window is re-derived here from the locked
    ///    `progress` (through the same [`hound_switch_range_hz`] the
    ///    coordinator resolved with) and an out-of-region offset is refused
    ///    rather than committed. Refusing, not re-clamping: the stall evidence
    ///    survives and the detector re-raises against the correct region on its
    ///    own, whereas a clamp would park us on a region edge nothing ranked.
    /// 4. **No-op relocation** (PAN-79; Codex round 3, finding 2). The
    ///    allocator's "no valid relocation exists" signal is `avoid_hz`
    ///    returned unchanged, which reaches this method as a requested offset
    ///    equal (within [`TX_OFFSET_NOOP_TOLERANCE_HZ`]) to the QSO's current
    ///    one. Nothing relocates, so nothing may be committed: clearing
    ///    `stall_cycles` here would throw away valid accumulated stall evidence
    ///    — the operator's `u` nudge over a partial streak is the live case —
    ///    and push automatic recovery out by another full
    ///    `qso_stall_switch_after` window, while `TxOffsetApplied` would
    ///    announce a move that never happened.
    /// 5. **Hold mode, re-validated at commit time** (Codex round 10, thread
    ///    on `autonomous.rs:988`). The caller's own Hold checks run before its
    ///    `.await` for this method's write lock, so a contended lock leaves a
    ///    window in which the operator's `f` press (Auto → Hold) lands after
    ///    the caller's last check but before this commit — re-reading
    ///    `tx_freq_mode` here, inside the same locked section, closes it.
    ///
    /// All these refusals are expected outcomes, not faults — see
    /// [`QsoManagerError::is_expected_offset_action_refusal`].
    ///
    /// `origin` is also what the known-good crediting site reads back off
    /// [`crate::states::PreSwitchOffset::operator_forced`] when the next
    /// forward advance arrives — see that field's doc comment (round 6,
    /// finding 2).
    pub async fn apply_tx_offset_switch(
        &self,
        qso_id: QsoId,
        new_offset_hz: f64,
        origin: OffsetRelocationOrigin,
    ) -> Result<f64, QsoManagerError> {
        let raised_at_generation = origin.raised_at_generation();
        let applied_hz =
            new_offset_hz.clamp(ACTIVE_QSO_TX_OFFSET_MIN_HZ, ACTIVE_QSO_TX_OFFSET_MAX_HZ);
        let mut qsos = self.qsos.write().await;
        let progress = qsos
            .get_mut(&qso_id)
            .ok_or(QsoManagerError::QsoNotFound { qso_id })?;
        if !progress.state.is_active() {
            return Err(QsoManagerError::QsoNotActive { qso_id });
        }
        if let Some(raised_at) = raised_at_generation {
            let current = progress.metadata.advance_generation;
            if current != raised_at {
                return Err(QsoManagerError::OffsetActionStale {
                    qso_id,
                    raised_at,
                    current,
                });
            }
            // PAN-75: a rollback since the action was raised may have taken
            // the streak back below the threshold that justified it.
            if progress.metadata.stall_cycles < self.config.timeouts.qso_stall_switch_after
                && self
                    .keep_call_ledger
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&qso_id)
                    .is_some_and(|entry| entry.stall_action_revoked)
            {
                return Err(QsoManagerError::OffsetActionStallRolledBack { qso_id });
            }
        }
        // 2a. **Operator nudge superseded by DX progress** (PAN-131, Codex
        //    round 13 on PR #350, P2). The `u` nudge was queued while its QSO
        //    was nudge-eligible, but it carries no `raised_at_generation`, so
        //    guard 2 cannot see the DX moving the QSO on before this drain. If
        //    the QSO now sits in a state nothing retransmits from
        //    (`WaitingForConfirmation`, an answered Auto self-CQ's
        //    `WaitingForReport`, …), committing would move the offset with no
        //    frame ever sent on it. Revalidating the STATE rather than adding
        //    a token keeps the nudge alive across an advance that stays
        //    retransmittable (`RespondingToCq` → `SendingReport` bumps the
        //    generation too). Ahead of guard 6, so the drain discards this
        //    refusal instead of requeuing it.
        if origin.is_operator_forced() && !operator_nudge_retransmits(progress) {
            return Err(QsoManagerError::OffsetActionNudgeSuperseded { qso_id });
        }
        // 3. **Outside the live Hound region** (Codex round 4 on PR #350,
        //    finding 2). The coordinator picks the window from a `get_qso`
        //    snapshot taken BEFORE the allocator runs, and the Fox's first
        //    report performs the mandatory calling-region -> response-region
        //    QSY (`process_message_for_qso`'s Hound block, which flips
        //    `hound_qsyed`). An operator-forced `u` nudge deliberately carries
        //    no `raised_at_generation`, so guard 2 above cannot catch that
        //    advance — the low, calling-region offset would commit and undo the
        //    QSY, putting our R+report where the Fox is not listening.
        //    Re-deriving the window HERE, from the same locked `progress` this
        //    method is about to mutate and through the same
        //    `hound_switch_range_hz` the coordinator resolved with, makes the
        //    check and the commit atomic by construction.
        if let Some((min_hz, max_hz)) = hound_switch_range_for(&progress.metadata, &self.config) {
            if applied_hz < min_hz || applied_hz > max_hz {
                return Err(QsoManagerError::OffsetActionOutsideHoundRegion {
                    qso_id,
                    offset_hz: applied_hz,
                    min_hz,
                    max_hz,
                });
            }
        }
        let old_off = progress.metadata.frequency;
        // 4. **Nothing to relocate** (PAN-79; Codex round 3 on PR #350, finding
        //    2). The allocator returns `avoid_hz` unchanged when every
        //    candidate is excluded, and the drain passes that straight here, so
        //    a "switch" can resolve back onto the offset the QSO is already on.
        //    Committing it would clear `stall_cycles` — valid evidence, and on
        //    an operator `u` nudge over a partial streak the ONLY evidence, so
        //    automatic recovery would be pushed out by another full
        //    `qso_stall_switch_after` window — and emit a `TxOffsetApplied`
        //    announcing a move that never happened. Refuse before mutating.
        if (applied_hz - old_off).abs() <= TX_OFFSET_NOOP_TOLERANCE_HZ {
            return Err(QsoManagerError::OffsetActionNoOp {
                qso_id,
                offset_hz: old_off,
            });
        }
        // 4a. **Trigger frame's outcome unknown** (PAN-182). The frame whose
        //    emission raised this stall action may still be waiting on the
        //    TX worker, and the commit below erases the ledger evidence a
        //    later not-sent report needs to undo the streak. Hold until the
        //    worker reports the frame aired (`confirm_stall_trigger_aired`)
        //    or unsent (`roll_back_replaced_attempt_at`, after which guard 2
        //    refuses it as `OffsetActionStallRolledBack`). After
        //    `STALL_TRIGGER_HOLD_SLOTS` with no report the action is dropped,
        //    never committed; the still-standing streak re-raises it with a
        //    fresh trigger. Scoped to stall origins: an operator `u` nudge is
        //    never held. After guard 4, so a no-op still reports as one.
        let pending_trigger_at = raised_at_generation.and_then(|_| {
            self.keep_call_ledger
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&qso_id)
                .and_then(|entry| entry.pending_stall_trigger.as_ref())
                .map(|trigger| trigger.emitted_at)
        });
        if let Some(emitted_at) = pending_trigger_at {
            return Err(
                if stall_trigger_hold_expired(emitted_at, self.now_utc(), self.rearm_slot_millis())
                {
                    QsoManagerError::OffsetActionTriggerOutcomeExpired { qso_id }
                } else {
                    QsoManagerError::OffsetActionTriggerOutcomePending { qso_id }
                },
            );
        }
        // 5. **Hold mode, re-validated inside this commit** (Codex round 10 on
        //    PR #350, thread on `autonomous.rs:988`; closed for real by
        //    PAN-143 — see the `ptt_sync_gate` field's doc comment). The
        //    caller's own Hold checks (`drain_pending_qso_offset_requests`'s
        //    batch-level and per-request re-reads) run BEFORE the `.await`
        //    for this method's write lock — a contended lock leaves a real
        //    window in which the operator's `f`/`o`-modal action (Auto ↔
        //    Hold) lands after the caller's last check but before this
        //    commit runs. Re-checking here is not enough by itself (`tx_freq_
        //    mode` is written by a completely independent task with no lock
        //    shared with `self.qsos` at all) — `ptt_sync_gate` below is what
        //    actually closes the window, by making this recheck and that
        //    task's store mutually exclusive.
        //
        // 6. **PTT already keyed, re-validated inside this commit** (PAN-140,
        //    Codex P1 on PR #369; closed for real by PAN-143). Same shape as
        //    guard 5, one line down: the caller's own pre-commit check
        //    (`drain_pending_qso_offset_requests`'s top-of-loop guard) runs
        //    BEFORE the `.await` for this method's write lock, and a bare
        //    recheck of the independently-written `ptt_active` atomic here
        //    can only narrow, not close, the window against the TX worker's
        //    `PttGuard::new` store. `ptt_sync_gate` closes it.
        //
        // Both guards, the mutation they gate, AND the resulting
        // `TxOffsetApplied`/one-shot-retransmit `MessageToSend` emission
        // near the end of this function all run under one acquisition of
        // `ptt_sync_gate` — acquired AFTER `self.qsos.write()` above
        // (never before: see the design spec's lock-ordering argument),
        // held all the way to the end of the function (including across
        // this method's own `.await` emit points — an async-aware
        // `tokio::sync::Mutex`, not a blocking one, exists precisely so
        // this is safe), and dropped only on function return (every early
        // return above this point releases it too, via normal scope
        // exit). Round 3 (Codex P1): releasing it right after the
        // mutation, before the resulting intent was published, left a
        // gap in which the TX worker could key PTT for an already-queued,
        // now-stale request — reopening the exact race this exists to
        // close, just moved later in the function.
        let _ptt_sync = self.ptt_sync_gate.lock().await;
        if !pancetta_core::TxFreqMode::from_u8(
            self.tx_freq_mode.load(std::sync::atomic::Ordering::Relaxed),
        )
        .allows_auto_change()
        {
            return Err(QsoManagerError::OffsetActionHeld { qso_id });
        }
        if self.ptt_active.load(std::sync::atomic::Ordering::Acquire) {
            return Err(QsoManagerError::OffsetActionPttInFlight { qso_id });
        }
        // PAN-145: invalidate snapshots before committing, under the PTT gate.
        self.relocation_epochs.bump(&qso_id.to_string());
        progress.metadata.frequency = applied_hz;
        progress.metadata.pending_freq_drift = None;
        progress.metadata.stall_cycles = 0;
        self.clear_recorded_stall_increments(qso_id);
        // PAN-72 (Codex round 4 on PR #350, finding 3): remember the offset we
        // are vacating, and when. The frame that triggered this move went out
        // on it — `rearm_manual_calls_at` re-sends and trips the stall
        // threshold in the same pass, and this commit only happens on the
        // coordinator's next drain — so any answer to that frame arrives at
        // `old_off` AFTER this mutation. For `PRE_SWITCH_OFFSET_GRACE` the
        // vacated offset therefore stays a valid RX baseline
        // (`is_message_relevant`) and stays a CANDIDATE for the offset a
        // forward advance credits as known-good (`process_message_for_qso`).
        //
        // Round 6, finding 2: the provenance travels with it. On any split —
        // including the one the latch below creates for an established simplex
        // QSO (PAN-132) — the DX's reply frequency cannot discriminate our two
        // offsets, and the right fallback there depends entirely on whether the
        // stall detector or the operator moved us. See
        // `QsoMetadata::pre_switch_offset` and
        // `PreSwitchOffset::operator_forced`.
        progress.metadata.pre_switch_offset = Some(crate::states::PreSwitchOffset {
            offset_hz: old_off,
            left_at: self.now_utc(),
            operator_forced: origin.is_operator_forced(),
        });
        // Mirror the Hound QSY block's state write (see `process_message_for_
        // qso`'s `QsoState::SendingReport` fix-up): several transitions —
        // `Completed` above all — are built from the PRECEDING state's own
        // `frequency` field, so updating only `metadata.frequency` would log
        // the pre-switch offset. `set_frequency` is a no-op on terminal and
        // `Idle` states by design.
        progress.state.set_frequency(applied_hz);
        // ...and because `QsoState::frequency()` is dual-purposed, that write
        // alone is not enough. Whenever `metadata.partner_freq` is `None`, the
        // state's own frequency is ALSO the RX-side baseline every relevance/
        // routing and drift gate keys on (`partner_freq.unwrap_or(qso_freq)` in
        // `is_message_relevant`, `classify_relevance` and
        // `maybe_confirm_frequency_drift_at`). This switch moves only OUR side:
        // the DX is still transmitting on the old offset. Latching the offset
        // we moved AWAY from into `partner_freq` keeps those gates pointed at
        // the DX while `metadata.frequency`/the state frequency track our new
        // TX offset — the same bookkeeping `compute_manual_tx_offset` performs
        // for the analogous case (`partner = (tx_off != dx_freq).then_some(
        // dx_freq)`), and it borrows that site's float-noise tolerance
        // ([`TX_OFFSET_NOOP_TOLERANCE_HZ`]) so a no-op switch never manufactures
        // a split — belt and braces now that guard 3 above refuses such a
        // switch outright. Without it the DX's
        // real replies fall outside `ESTABLISHED_FREQ_TOLERANCE_HZ` of our new
        // offset and stop routing to this QSO altogether, and the drift gate
        // then reads the DX's unchanged frequency as a confirmed drift and
        // relatches us straight back onto the offset the switch existed to
        // escape.
        //
        // An already-`Some` `partner_freq` (Hound, an offset hold, a collision
        // nudge, a passband clamp) is deliberately left alone: it is already
        // the correct "where we hear the DX", and this is purely a TX-side
        // move. The crediting block in `process_message_for_qso` treats the
        // split this latch creates exactly like an already-`Some` one, so its
        // provenance rule decides the known-good offset (PAN-132).
        //
        // The latch also requires an ESTABLISHED DX identity
        // (`their_callsign().is_some()` — the same discriminator
        // `is_message_relevant`/`classify_relevance` use to pick between
        // `FREQ_TOLERANCE_HZ` and `ESTABLISHED_FREQ_TOLERANCE_HZ`). On an
        // unanswered `CallingCq` there is no DX at the old offset at all: it
        // was OUR abandoned CQ frequency. Latching it would aim the RX gates
        // at dead air, and — being pre-establishment — they would judge a
        // caller answering us on our REAL (new) offset against it with the
        // tight 15 Hz bound, rejecting every answer for the life of the QSO
        // and spawning a duplicate QSO object in its place.
        if progress.metadata.partner_freq.is_none()
            && progress.state.their_callsign().is_some()
            && (applied_hz - old_off).abs() > TX_OFFSET_NOOP_TOLERANCE_HZ
        {
            progress.metadata.partner_freq = Some(old_off);
        }
        // `info!`, not `warn!`: the removed hop fired only on an identical-
        // repeat trigger and was genuinely rare, but the silence-based
        // detector this replaced it with can fire roughly once a minute per
        // stalled QSO. That is routine adaptive operation, not a warning.
        info!(
            target: "tx.freq",
            qso_id = %qso_id,
            dx = progress.metadata.their_callsign.as_deref().unwrap_or("?"),
            "Adaptive TX-offset action: {:.0} Hz -> {:.0} Hz",
            old_off, applied_hz
        );
        // PAN-72 Fix A (Codex round 10, thread on `autonomous.rs:1829`): an
        // operator-forced nudge of an autonomous self-CQ's `CallingCq` needs
        // an IMMEDIATE one-shot retransmission at the new offset.
        // `rearm_manual_calls_at`'s periodic cadence deliberately never
        // re-sends a `CallingCq` for `CallInitiation::Auto` (its own
        // `is_manual` guard — a separate, intentional design decision this
        // fix does NOT touch), so without this the commit above would
        // silently relocate the QSO with nothing ever sent at the new
        // offset while `AutonomousOperator::current_cq_offset_hz` stays on
        // the old one. This is a narrow, one-shot exception for a
        // deliberate, operator-supervised action — never for a
        // `StallDetected` origin, which is gated out below exactly like the
        // silence-driven stall detector's own dormancy for an Auto
        // `CallingCq` remains.
        //
        // Deliberately keyed on the CURRENT state only, not
        // `CallInitiation`: for a MANUAL `CallingCq` this is a harmless
        // redundant resend layered on top of the existing periodic cadence
        // (the double-send guard below keeps it harmless).
        //
        // PAN-72 round 14 (Codex round 14 on PR #350, thread on
        // `tui_relay.rs:3116`): the SAME gap, one state later. An
        // Auto-initiated QSO in `RespondingToCq`/`SendingReport` that has
        // ALREADY exhausted `AUTO_RESEND_MAX_CALLS` still passes
        // `resolve_nudge_tx_offset`'s eligibility check (state + initiation
        // only, no cap awareness) — the relay queues the nudge and reports
        // success, but `rearm_manual_calls_at`'s own
        // `if progress.metadata.call_count >= max_calls { continue; }` cap
        // gate then skips re-arming it forever, so the metadata updates and
        // nothing is ever retransmitted. Extending the SAME one-shot
        // mechanism (not a second one) to those two states closes it —
        // narrower than the CallingCq arm above: this only fires for an
        // Auto QSO ACTUALLY at/past the cap (below the cap, the very next
        // periodic rearm pass already retransmits on its own, so firing
        // here too would be a harmless but redundant double-send) and never
        // for a Manual QSO in these states (Manual's own
        // `manual_call_max_calls`-bounded watchdog handling here is
        // untouched — hitting IT is expected watchdog-retirement territory,
        // not a "should retry" scenario).
        let cq_retransmit = if origin.is_operator_forced() {
            let is_auto_past_resend_cap = progress.metadata.initiated_by == CallInitiation::Auto
                && progress.metadata.call_count >= AUTO_RESEND_MAX_CALLS;
            let message = match &progress.state {
                QsoState::CallingCq { .. } => Some(MessageType::Cq {
                    callsign: self.config.our_callsign.clone(),
                    grid: self.config.our_grid.clone(),
                }),
                // Reuse EXACTLY the same construction
                // `rearm_manual_calls_at`'s own
                // `QsoState::RespondingToCq { target_callsign, .. }` arm uses
                // — see that match block.
                QsoState::RespondingToCq {
                    target_callsign, ..
                } if is_auto_past_resend_cap => Some(MessageType::CqResponse {
                    calling_station: target_callsign.clone(),
                    responding_station: self.config.our_callsign.clone(),
                    grid: self.config.our_grid.clone(),
                }),
                // Reuse EXACTLY the same construction
                // `rearm_manual_calls_at`'s own
                // `QsoState::SendingReport { their_report: None, .. }` arm
                // uses — see that match block.
                QsoState::SendingReport {
                    their_callsign,
                    their_report: None,
                    our_report,
                    ..
                } if is_auto_past_resend_cap => Some(MessageType::SignalReport {
                    to_station: their_callsign.clone(),
                    from_station: self.config.our_callsign.clone(),
                    report: *our_report,
                }),
                // Reuse EXACTLY the same construction
                // `rearm_manual_calls_at`'s own
                // `QsoState::SendingReport { their_report: Some(_), .. }` arm
                // uses — see that match block.
                QsoState::SendingReport {
                    their_callsign,
                    their_report: Some(_),
                    our_report,
                    ..
                } if is_auto_past_resend_cap => Some(MessageType::ReportAck {
                    to_station: their_callsign.clone(),
                    from_station: self.config.our_callsign.clone(),
                    report: *our_report,
                }),
                _ => None,
            };
            message.map(|message| {
                let now = self.now_utc();
                // Double-send guard (Fix A, step 4; extended round 14):
                // bump call_count/last_call_at exactly as
                // `rearm_manual_calls_at` would after a normal resend, so
                // the VERY NEXT rearm pass sees "at least one slot hasn't
                // elapsed yet" and does not immediately re-fire a second
                // resend on top of this one. This matters for a Manual
                // `CallingCq` (which DOES get a periodic rearm); it applies
                // uniformly to whichever arm above fired — no separate
                // bookkeeping needed per state.
                let prev_last_call_at = progress.metadata.last_call_at;
                progress.metadata.call_count += 1;
                progress.metadata.last_call_at = Some(now);
                let raw_text = self.render_sent_text(&message);
                progress.messages.push(QsoMessage {
                    timestamp: now,
                    direction: MessageDirection::Sent,
                    message_type: message.clone(),
                    raw_text: raw_text.clone(),
                    signal_strength: None,
                    frequency: progress.metadata.frequency,
                });
                let attempt_id = self.next_attempt_id();
                self.record_keep_call_attempt(
                    &mut self
                        .keep_call_ledger
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()),
                    qso_id,
                    KeepCallAttempt {
                        attempt_id,
                        emitted_at: now,
                        raw_text,
                        prev_last_call_at,
                        bumped_stall: false,
                        tx_slot: None,
                        recorded_sent: true,
                        frequency_hz: progress.metadata.frequency,
                        remote_origin: progress.metadata.remote_origin,
                        remote_client_key_id: progress.metadata.remote_client_key_id.clone(),
                    },
                );
                (
                    message,
                    progress.metadata.frequency,
                    progress.metadata.tx_parity,
                    progress.metadata.remote_origin,
                    progress.metadata.remote_client_key_id.clone(),
                    attempt_id,
                )
            })
        } else {
            None
        };
        // PAN-72 (Codex round 1 on PR #350, finding 7): announce the applied
        // move so the coordinator can rebuild the `ActiveQsosSnapshot` the TUI
        // banner and the remote gateway render from. That snapshot is
        // otherwise rebuilt only on a state transition, and a stalled exchange
        // is precisely the case where no further transition arrives — the UI
        // would show the pre-switch offset through several switch/revert
        // cycles. Emitted AFTER the write guard is dropped, matching every
        // other emit site in this file.
        drop(qsos);
        self.emit_event(QsoEvent::TxOffsetApplied {
            qso_id,
            offset_hz: applied_hz,
        })
        .await;
        // The one-shot CQ retransmission (if any), also emitted after the
        // lock is released — same collect-then-emit-after-lock-drop pattern
        // `rearm_manual_calls_at`'s own `to_recall` uses.
        if let Some((
            message,
            frequency,
            tx_parity,
            remote_origin,
            remote_client_key_id,
            attempt_id,
        )) = cq_retransmit
        {
            self.emit_event(QsoEvent::MessageToSend {
                qso_id,
                message,
                frequency,
                tx_parity,
                remote_origin,
                remote_client_key_id,
                attempt_id: Some(attempt_id),
            })
            .await;
        }
        Ok(applied_hz)
    }

    /// Get next contest serial number
    pub async fn get_next_serial(&self) -> SerialNumber {
        let mut next_serial = self.next_serial.write().await;
        let serial = *next_serial;
        *next_serial += 1;
        serial
    }

    // Internal helper methods

    /// Render a `MessageType` to the FT8 text we would transmit, so the
    /// recorded `Sent` `QsoMessage.raw_text` matches what goes on the air.
    /// Without this, engine-emitted Sent records carried an empty string and
    /// the TUI "TX:" line was blank (UX audit Batch 2). Falls back to an
    /// empty string on the (unexpected) render error rather than failing the
    /// QSO; the message still transmits via the separate encode path.
    fn render_sent_text(&self, message: &MessageType) -> String {
        crate::exchange::MessageExchange::new(self.config.our_callsign.clone())
            .generate_message(message)
            .unwrap_or_default()
    }

    async fn process_message_for_qso(
        &self,
        qso_id: QsoId,
        message: QsoMessage,
        observed_slot_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<(), QsoManagerError> {
        let mut pending_rejections = Vec::new();
        let mut qsos = self.qsos.write().await;
        let progress = qsos
            .get_mut(&qso_id)
            .ok_or(QsoManagerError::QsoNotFound { qso_id })?;

        // Match the relocation commit lock order: QSO map, then PTT gate.
        // Hold through epoch invalidation, mutation and reply publication.
        // Mandatory Hound moves still apply in Hold mode.
        let hound_ptt_sync = if progress.metadata.hound
            && matches!(message.message_type, MessageType::SignalReport { .. })
            && self.is_hound_fox_frame(&progress.metadata, &message.message_type)
        {
            Some(self.ptt_sync_gate.lock().await)
        } else {
            None
        };

        let old_state = progress.state.clone();

        // --- First-decode parity refinement ---
        // This QSO's `tx_parity` may have been latched PROVISIONALLY: when
        // `respond_to_cq_with`/`respond_to_caller` answered a DX-cluster/
        // DX-Hunter spot with no observed live decode yet, there was no real
        // DX parity to oppose, so the latch fell back to a "nearest next
        // slot" guess (`tx_parity_provisional == true`). The first time a
        // frame genuinely FROM that latched partner arrives carrying a
        // determinable slot parity, refine the latch to the TRUE
        // opposite-of-DX parity. Sender verification reuses the same
        // `is_partner` check the rest of this function relies on elsewhere —
        // never weakened for this purpose. This runs exactly once (the flag
        // flip prevents re-running) and emits no event of its own; it must
        // happen BEFORE `qso_tx_parity` is captured just below so a reply
        // this SAME call emits already carries the refined value.
        if progress.metadata.tx_parity_provisional {
            if let (Some(sender), Some(latched), Some(observed_parity)) = (
                message.message_type.sender_callsign(),
                progress.metadata.their_callsign.as_deref(),
                observed_slot_parity,
            ) {
                if Self::is_partner(sender, latched) {
                    progress.metadata.tx_parity = Some(observed_parity.opposite());
                    progress.metadata.tx_parity_provisional = false;
                }
            }
        }

        // Capture per-QSO routing data while we hold the write lock so the
        // reply emission below does not need to re-acquire it (which would
        // deadlock against this guard).
        let mut qso_frequency = progress.metadata.frequency;
        let qso_tx_parity = progress.metadata.tx_parity;
        let qso_remote_origin = progress.metadata.remote_origin;
        let qso_remote_client_key_id = progress.metadata.remote_client_key_id.clone();
        let qso_initiated_by = progress.metadata.initiated_by;
        // PR #344 round-1 Codex P2: a natively-typed ContestReply (PAN-51 --
        // ft8_message_to_qso_type classifies a ReplyWithR decode directly,
        // never routing through the NonStandard reclassification below that
        // used to be the only place this QSO-engagement gate ran) must still
        // be gated the same way, or an ordinary non-contest QSO advances and
        // logs a contest-shaped exchange the moment its real partner happens
        // to send an "R"+grid/report shape.
        let qso_contest_engaged = Self::grid_contest_engaged(&progress.metadata);

        // PAN-259: a Hound answers a Fox report at the Fox's own offset. A
        // report whose offset is outside the response envelope (or not a
        // finite, transmittable offset) cannot be answered there, so it is
        // ignored before it changes any exchange state or schedules an
        // R-report; the QSO keeps waiting for a valid report.
        if progress.metadata.hound
            && matches!(message.message_type, MessageType::SignalReport { .. })
            && matches!(
                old_state,
                QsoState::RespondingToCq { .. } | QsoState::SendingReport { .. }
            )
            && self.is_hound_fox_frame(&progress.metadata, &message.message_type)
            && hound_report_offset(message.frequency, &self.config.hound).is_none()
        {
            warn!(
                target: "hound",
                qso_id = %qso_id,
                "Hound: ignoring Fox report at {:.0} Hz, outside the response envelope {}–{} Hz",
                message.frequency,
                self.config.hound.response_min_hz,
                self.config.hound.response_max_hz
            );
            return Ok(());
        }

        progress.messages.push(message.clone());

        // Compound-callsign equivalence (catalog C18 / peer D4): if this frame
        // came from our latched partner under a MORE-COMPLETE displayed call
        // (same station per `callsigns_match`, but a longer compound form — e.g.
        // we latched bare `G8BCG` and the DX now signs `EA8/G8BCG`), upgrade the
        // logged `their_callsign` to the fuller form. The compound carries DX /
        // portable info worth preserving in the ADIF.
        //
        // SECURITY (deep audit, Fix 1): the upgrade is gated by
        // `is_safe_compound_upgrade`, NOT a bare `callsigns_match` + length
        // check. `callsigns_match` treats two calls as the same station when
        // they share a base, so an RF attacker who knows the partner's on-air
        // base call could otherwise rewrite the LOGGED callsign by signing a
        // state-appropriate frame with arbitrary junk wrapped around that base
        // (e.g. `BOGUS9/G8BCG/MM`). The safe check additionally requires every
        // extra token to be a recognized prefix/suffix (per the same rules
        // `validate_callsign` uses) and forbids replacing a latched affix with
        // a different one — so only a genuine compound completion of the same
        // base is accepted. A rejected-but-matching upgrade is logged at
        // `warn!(target: "qso.security")`.
        if let (Some(sender), Some(latched)) = (
            message.message_type.sender_callsign(),
            progress.metadata.their_callsign.as_deref(),
        ) {
            if crate::exchange::is_safe_compound_upgrade(latched, sender) {
                let upgraded = sender.to_string();
                info!(
                    target: "qso.compound",
                    from = %latched,
                    to = %upgraded,
                    "upgrading logged partner callsign to more-complete compound form (C18)"
                );
                progress.metadata.their_callsign = Some(upgraded);
            } else if sender.len() > latched.len()
                && crate::exchange::callsigns_match(sender, latched)
            {
                // Same base + strictly longer, but NOT a safe compound
                // completion (junk affix token, or a different prefix/suffix
                // than already latched). Do not overwrite the logged call.
                warn!(
                    target: "qso.security",
                    latched = %latched,
                    rejected = %sender,
                    "rejected unsafe compound-callsign upgrade (unrecognized or substituted affix); keeping latched logged callsign"
                );
                pending_rejections.push(QsoEvent::MessageRejected {
                    qso_id,
                    reason: RejectionReason::UnsafeCompoundUpgrade,
                    from_callsign: Some(sender.to_string()),
                    to_callsign: message
                        .message_type
                        .addressee_callsign()
                        .map(str::to_string),
                });
            }
        }

        // Determine state transition based on current state and message.
        // `initiated_by` is threaded through so the manual-only state-regression
        // arms ("back up to where the DX thinks we are") never fire for
        // autonomous QSOs.
        let mut new_state = self
            .determine_state_transition(
                qso_id,
                &old_state,
                &message.message_type,
                message.signal_strength,
                qso_initiated_by,
                qso_contest_engaged,
            )
            .await?;

        // Did this received frame advance the QSO? Computed here (before
        // `old_state`/`new_state` are moved into emit_state_change) for the
        // forward-advance tracking below, which resets `stall_cycles`, bumps
        // `advance_generation` and records `last_known_good_offset_hz` on a
        // real advance.
        //
        // PAN-72 (Codex round 1 on PR #350, finding 2): this uses the
        // role-agnostic `progress_rank`, NOT the Caller-shaped `ladder_rank`
        // the manual context-reply path compares against `ResponseStep`. See
        // `progress_rank`'s doc comment: with `ladder_rank`, the CQer's real
        // `CallingCq -> WaitingForReport` advance read as "no progress", so a
        // stall streak built up over unanswered CQs carried into the
        // established exchange and could trip the switch threshold on the
        // very first missed report slot — with no known-good offset recorded.
        let dx_frame_advanced = Self::progress_rank(&new_state) > Self::progress_rank(&old_state);

        // PAN-72 (Codex round 5 on PR #350, finding 2): the RX tolerance the
        // relevance gate actually judged THIS frame with, captured from the
        // PRE-transition state — `is_message_relevant`/`classify_relevance` ran
        // against `old_state`, and the advance we are about to record may well
        // have just latched a contra callsign, which would widen the bound
        // retroactively. Used below to decide which offset the advance proves.
        let rx_tolerance_hz = if old_state.their_callsign().is_some() {
            ESTABLISHED_FREQ_TOLERANCE_HZ
        } else {
            FREQ_TOLERANCE_HZ
        };

        // Hound QSY gate: computed here while both `old_state` and `new_state`
        // are still in scope (before `old_state` is consumed by `emit_state_change`
        // below). The QSY fires after the state-update block where we can access
        // `progress` again.
        let was_responding_to_cq = matches!(old_state, QsoState::RespondingToCq { .. });
        let advanced_to_sending_report = matches!(new_state, QsoState::SendingReport { .. });

        // Auto-sequence the outbound reply for MANUAL-initiated QSOs only.
        // The reply is generated from the SAME (pre-transition state,
        // received message) pair that drove the transition, so the two never
        // disagree. Autonomous-initiated QSOs are deliberately left UNCHANGED
        // (no auto-reply) — that remains gated for Phase 5.
        //
        // `reply_to_emit` is captured here (under the lock) and emitted after
        // the write guard is released, since `emit_event` only needs the
        // broadcast channel, not the QSO map.
        // Detect a manual state regression: the DX sent an EARLIER-stage
        // message and `determine_state_transition` either backed us up the
        // ladder (rank decreased) or kept us in SendingReport on a repeated
        // report that was already latched. First acquisition (None -> Some)
        // is forward progress and keeps the watchdog reprieve (PAN-84).
        // When the regression queues a re-send (`reply_to_emit`), used
        // to (a) count that re-send against the manual watchdog cap and (b)
        // gate the per-slot rearm so it does not double-send in the same slot;
        // a repeated report in SendingReport queues none, so neither applies
        // (PAN-129). Every regression cancels the C3 reprieve.
        let is_manual_regression = qso_initiated_by == CallInitiation::Manual
            && (Self::ladder_rank(&new_state) < Self::ladder_rank(&old_state)
                || (matches!(
                    old_state,
                    QsoState::SendingReport {
                        their_report: Some(_),
                        ..
                    }
                ) && matches!(new_state, QsoState::SendingReport { .. })
                    && matches!(message.message_type, MessageType::SignalReport { .. })));

        let mut reply_to_emit: Option<MessageType> = None;
        // Phase 5 (autonomous auto-completion): forward auto-sequencing now fires
        // for BOTH Manual and Auto QSOs — an autonomous-opened pounce / CQ-answer
        // must advance its reply ladder (grid → R-report → RR73 → 73) exactly as a
        // manual QSO does, otherwise the autonomous operator opens a QSO and then
        // goes silent after the opening call. Regression handling stays Manual-only:
        // `is_manual_regression` is always false for an Auto QSO, so an Auto QSO
        // emits a reply ONLY on a genuine FORWARD advance (`new_state != old_state`).
        // This activates only while the autonomous operator is running (Auto QSOs
        // are created solely by `respond_to_cq` / `start_cq`), so it is gated behind
        // the autonomous toggle by construction.
        if new_state != old_state || is_manual_regression {
            let exchange = crate::exchange::MessageExchange::new(self.config.our_callsign.clone());
            match exchange.generate_response(
                &old_state,
                &message.message_type,
                message.signal_strength,
            ) {
                Ok(Some(reply)) => reply_to_emit = Some(reply),
                Ok(None) => {}
                Err(e) => {
                    warn!(
                        qso_id = %qso_id,
                        "failed to generate auto-sequence reply: {}",
                        e
                    );
                }
            }
        }
        // PAN-259: in the WSJT-X Fox/Hound procedure the Fox's RR73 ends the
        // contact. A Hound logs it and sends no 73: the Fox has already moved
        // on, and a 73 would only add QRM to the Fox's next Hounds.
        if progress.metadata.hound && hound_suppresses_reply(reply_to_emit.as_ref()) {
            reply_to_emit = None;
        }
        // PAN-386: a WSJT-X Fox logs a Hound when it sends RR73
        // (`mainwindow.cpp` Fox logging at RR73); the Hound sends no 73 back.
        // So for a Fox-role QSO the Hound's R-report completes the contact:
        // the `WaitingForConfirmation` result becomes `Completed`, and the
        // held-completion arm below keeps `QsoCompleted` until the RR73's
        // `TxFrameSent` arrives, reverting to `old_state` if it never airs
        // (PAN-280).
        if progress.metadata.fox
            && matches!(message.message_type, MessageType::ReportAck { .. })
            && matches!(
                old_state,
                QsoState::WaitingForReport { .. } | QsoState::SendingReport { .. }
            )
            && matches!(reply_to_emit, Some(MessageType::FinalConfirmation { .. }))
        {
            if let QsoState::WaitingForConfirmation {
                their_callsign,
                their_report,
                our_report,
                frequency,
                grid_square,
                started_at,
            } = &new_state
            {
                let completed_at = self.now_utc();
                let duration_seconds = (completed_at - *started_at).num_seconds().max(0) as u32;
                new_state = QsoState::Completed {
                    their_callsign: their_callsign.clone(),
                    their_report: *their_report,
                    our_report: *our_report,
                    frequency: *frequency,
                    grid_square: grid_square.clone(),
                    completed_at,
                    duration_seconds,
                };
            }
        }

        // UX audit Batch 2 #8: latch the DX's grid the moment it arrives in a
        // CqResponse (the opening "<us> <them> <grid>" / "CQ <them> <grid>"
        // exchange). The common close arm (SendingReport → Completed) hard-codes
        // grid_square: None, so without latching here the decoded grid never
        // reaches the logged ADIF GRIDSQUARE. We only overwrite when the
        // incoming grid is present so a later grid-less message can't clear it.
        if let MessageType::CqResponse {
            grid: Some(grid), ..
        } = &message.message_type
        {
            if !grid.is_empty() {
                progress.metadata.grids.theirs = Some(grid.clone());
            }
        }

        // PAN-49: latch the grid the moment a contest "R"+grid ack arrives,
        // mirroring the CqResponse grid-latch above — the transition arm's
        // WaitingForConfirmation.grid_square only feeds the Completed-state
        // latch (see the block below), not metadata directly, and ADIF
        // logging (adif.rs) reads metadata.grids.theirs, not QsoState.
        //
        // Guarded by sender verification against `old_state`'s ORIGINAL
        // partner (final review Finding 1): the transition arm's own
        // `reject_sender` check may have rejected this frame (spoofed or
        // third-party sender) and left `new_state == old_state`, but this
        // latch used to run unconditionally regardless of that outcome —
        // writing an unrelated station's grid into the QSO's ADIF-bound
        // metadata. The CqResponse latch above is protected the equivalent
        // way structurally: `(RespondingToCq, CqResponse)` has a dedicated
        // relevance arm requiring `is_partner` before it is ever reached.
        //
        // Also gated by `qso_contest_engaged` (PR #344 round-1 Codex P2):
        // this block is unconditional on the transition arm actually
        // advancing, so without this an ordinary non-contest QSO's
        // `grids.theirs` could still get overwritten by a stray
        // ContestReply-shaped decode from its real partner even after the
        // transition arm itself correctly declines to advance.
        if let MessageType::ContestReply {
            grid, from_station, ..
        } = &message.message_type
        {
            let from_partner = old_state
                .their_callsign()
                .is_some_and(|partner| Self::is_partner(from_station, partner));
            if qso_contest_engaged && from_partner && !grid.is_empty() {
                progress.metadata.grids.theirs = Some(grid.clone());
            }
        }

        // PAN-280: a completion that queues a reply (our 73 to the DX's RR73)
        // holds its QsoCompleted until that reply airs. The reply's attempt
        // id is minted here when it closes a QSO, so the held record and the
        // MessageToSend below carry the same id; `reply_sent_at` is the one
        // timestamp of its `Sent` record, so a revert can find that record.
        let mut reply_attempt_id: Option<TxAttemptId> = None;
        let reply_sent_at = self.now_utc();
        if new_state != old_state {
            // PAN-280: what a dropped closing reply puts back.
            let pre_close_reports = progress.metadata.reports.clone();
            let pre_close_end_time = progress.metadata.end_time;
            let pre_close_rf = progress.metadata.completed_rf_frequency_hz;
            // CQer flow: the QSO was created by start_cq with their_callsign
            // None (we didn't know who would answer). The moment a state
            // advance reveals the contra callsign (caller answered), latch it
            // so the logged ADIF/worked-station record carries the right call,
            // and register the callsign mapping for relevance/supersede.
            if progress.metadata.their_callsign.is_none() {
                if let Some(call) = new_state.their_callsign() {
                    progress.metadata.their_callsign = Some(call.to_string());
                    if progress.metadata.first_answer_at.is_none() {
                        progress.metadata.first_answer_at = Some(message.timestamp);
                    }
                }
            }

            // If transitioning to Completed, update metadata with signal reports and end time
            if let QsoState::Completed {
                their_report,
                our_report,
                grid_square,
                ..
            } = &new_state
            {
                progress.metadata.reports = SignalReports {
                    sent: Some(*our_report),
                    received: Some(*their_report),
                };
                progress.metadata.end_time = Some(self.now_utc());
                // Prefer the grid carried in the Completed state (CQer path
                // threads it through WaitingForConfirmation); otherwise keep
                // whatever was latched from the opening CqResponse above.
                if let Some(grid) = grid_square {
                    progress.metadata.grids.theirs = Some(grid.clone());
                }
            }

            let completed_metadata = if matches!(&new_state, QsoState::Completed { .. }) {
                // `progress.metadata.frequency` is the audio offset within the
                // slot and MUST stay that way for the QSO's entire lifetime --
                // see its own doc comment (PAN-25 round 2: an earlier version
                // of this fix overwrote it in place, which broke
                // `resend_last_tx`/the TX worker's modulation limit for a
                // close-step retry on a completed QSO). The logged RF
                // frequency (dial + offset; WSJT-X logs the actual on-air
                // frequency, not the dial) is stamped separately: into
                // `completed_rf_frequency_hz` (for the recent-completed
                // suppression's band check) and, only in this LOCAL CLONE, into
                // `frequency` for the emitted event (ADIF/DB logging wants the
                // real on-air frequency) -- the stored `progress.metadata`
                // never gets that clone's `frequency` value.
                let rx_dial = self.dial_frequency_hz.load(Ordering::Relaxed);
                let split = self.split_tx_frequency_hz.load(Ordering::Relaxed);
                let dial = effective_tx_dial(rx_dial, split);
                let rf_frequency = (dial > 0).then_some(progress.metadata.frequency + dial as f64);
                progress.metadata.completed_rf_frequency_hz = rf_frequency;
                let mut m = progress.metadata.clone();
                if let Some(rf) = rf_frequency {
                    m.frequency = rf;
                }
                Some(m)
            } else {
                None
            };

            // C3 fix (watchdog-vs-just-in-time-answer race): mark that this QSO
            // made a FORWARD state advance in the current watchdog cycle. The
            // manual keep-calling watchdog (`check_timeouts_at`) grants a
            // one-pass reprieve to any QSO that advanced since its previous
            // pass, so a just-in-time DX answer arriving in the very slot the
            // call cap trips is NOT thrown away as Failed{Timeout} in the same
            // tick it advanced. This is deliberately NOT a `call_count` reset —
            // the per-QSO cap still bounds total calls across the whole QSO
            // (C12: per-QSO, not per-step), and a QSO that advances once then
            // goes silent still retires at the cap on the NEXT pass (the flag
            // is cleared every watchdog pass). We set it only on a genuine
            // forward advance (not a manual regression — a DX repeating an
            // earlier message must keep counting against the cap so a stuck DX
            // cannot drive an unbounded ping-pong). Auto QSOs do not use the
            // manual watchdog and are unaffected.
            if qso_initiated_by == CallInitiation::Manual && !is_manual_regression {
                progress.metadata.progressed_this_cycle = true;
            }

            // Rearm-coordination fix: a genuine forward advance stamps
            // `last_call_at` like a manual regression that re-sends does
            // (below). Without this, the per-slot keep-call rearm
            // (`rearm_manual_calls_at`) has no way to know a response arrived
            // THIS slot — if the DX's decode lands late in the slot, the 5s
            // watchdog tick can re-emit the OLD rung (rearm fires on stale
            // `last_call_at`) in the same slot the response just advanced us
            // to a NEW rung, producing two TransmitRequests for one slot (a
            // redundant re-TX the operator observed). Stamping here suppresses
            // that slot's rearm at the source, the same way the regression
            // stamp already suppresses a double-send on a repeated DX frame.
            if qso_initiated_by == CallInitiation::Manual {
                progress.metadata.last_call_at = Some(message.timestamp);
            }

            progress.state = new_state.clone();
            progress.state_history.push(StateTransition {
                from_state: old_state.clone(),
                to_state: new_state.clone(),
                timestamp: message.timestamp,
                reason: TransitionReason::MessageReceived(message.message_type.clone()),
            });

            let closing_reply = completed_metadata
                .as_ref()
                .and(reply_to_emit.as_ref())
                .map(|reply| self.render_sent_text(reply));
            match (completed_metadata, closing_reply) {
                // PAN-280: our 73 is queued below; log only once it airs.
                (Some(metadata), Some(reply_text)) => {
                    let attempt_id = self.next_attempt_id();
                    reply_attempt_id = Some(attempt_id);
                    self.hold_completion(
                        qso_id,
                        PendingCompletion {
                            attempt_ids: vec![attempt_id],
                            raw_text: reply_text.clone(),
                            frequency_hz: qso_frequency,
                            remote_origin: qso_remote_origin,
                            remote_client_key_id: qso_remote_client_key_id.clone(),
                            queued_at: reply_sent_at,
                            completed_state: new_state.clone(),
                            restore: CompletionRestore::Revert {
                                state: old_state.clone(),
                                reports: pre_close_reports,
                                end_time: pre_close_end_time,
                                completed_rf_frequency_hz: pre_close_rf,
                                sent_record: Some((reply_sent_at, reply_text)),
                            },
                            metadata,
                            state_history: progress.state_history.clone(),
                            messages: progress.messages.clone(),
                        },
                    );
                    self.emit_state_change(qso_id, old_state, new_state).await;
                }
                // A completion with no closing frame (the DX's bare 73, a
                // Hound on the Fox's RR73) logs at once, as before.
                (Some(metadata), None) => {
                    self.emit_state_change(qso_id, old_state, new_state).await;
                    self.emit_event(QsoEvent::QsoCompleted {
                        qso_id,
                        metadata,
                        state_history: progress.state_history.clone(),
                        messages: progress.messages.clone(),
                    })
                    .await;
                }
                (None, _) => {
                    self.emit_state_change(qso_id, old_state, new_state).await;
                }
            }
        }

        // Count a manual regression RE-SEND against the keep-calling watchdog
        // so a DX that keeps repeating an earlier message cannot drive an
        // unbounded ping-pong. Only a regression that queued a reply this
        // decode (REGRESSION 1/3) re-sends: it bumps `call_count` and stamps
        // `last_call_at` to `message.timestamp`; the latter also gates
        // `rearm_manual_calls_at` (which only re-emits when ≥1 slot has
        // elapsed since `last_call_at`), so that re-send and the per-slot
        // rearm never both fire in the same slot. A repeated report in
        // SendingReport (REGRESSION 2) sends nothing — the rearm owns that
        // re-send — so it leaves both fields alone, and the rearm keeps its
        // silence cadence and counts the stall (PAN-129). Its old restamp
        // held the rearm back a full slot from every decode, which on FT2
        // (3.2 s slot, 5 s poll) skipped re-sends and delayed the stall
        // switch. `first_call_at` is left untouched — a regression must not
        // reset the watchdog clock.
        if is_manual_regression {
            if let Some(progress) = qsos.get_mut(&qso_id) {
                if reply_to_emit.is_some() {
                    progress.metadata.call_count += 1;
                    progress.metadata.last_call_at = Some(message.timestamp);
                }
                // A regression is the opposite of forward progress (the DX
                // repeated an earlier message), so it cancels any pending C3
                // reprieve from an earlier advance — a stuck DX repeating must
                // still retire at the cap, never earn an extra watchdog pass.
                progress.metadata.progressed_this_cycle = false;
            }
        }

        // PAN-72: a genuine forward advance resets the stall streak and
        // records the offset we were on as "known-good" — the offset a
        // future stall-triggered Switch/Revert decision reasons about. The
        // non-advancing case is no longer driven by inspecting incoming
        // frame content here; `QsoManager::rearm_manual_calls_at` is now the
        // sole site that increments `QsoMetadata::stall_cycles` (on silence
        // — a real per-slot re-send with no DX response) and emits
        // `QsoEvent::TxOffsetActionNeeded` once it trips
        // `TimeoutConfig::qso_stall_switch_after`.
        if let Some(progress) = qsos.get_mut(&qso_id) {
            if dx_frame_advanced {
                progress.metadata.stall_cycles = 0;
                self.clear_recorded_stall_increments(qso_id);
                // PAN-72 (Codex round 4 on PR #350, finding 3): credit the
                // offset the DX actually heard us on. A STALL-triggered
                // relocation is committed AFTER the frame that triggered it has
                // gone out on the old offset, so an advance arriving inside
                // `PRE_SWITCH_OFFSET_GRACE` of such a move is answering the
                // PRE-switch transmission — the new offset has not been
                // transmitted on even once. Crediting it would record an
                // unproven offset as known-good and degrade the next stall's
                // adaptation: with `last_known_good` equal to the offset we are
                // already on, the `>= f64::EPSILON` guard in
                // `rearm_manual_calls_at` skips `Revert` and falls through to a
                // fresh `Switch` — not a hard failure, but the genuinely proven
                // offset is lost and the QSO hops on blindly instead of
                // returning to what worked.
                //
                // Round 5, finding 2: but the clock alone does not identify
                // which frame was answered. An operator-forced `u` nudge is NOT
                // coupled to an old-offset triggering resend — it can land on a
                // QSO that is about to transmit, the next rearm goes out on the
                // NEW offset, and the answer to THAT lands well inside the same
                // grace. So the decisive evidence is where the reply actually
                // decoded: the vacated offset is credited only when the frame
                // matches that baseline, judged with the SAME tolerance the
                // relevance gate admitted it under (`rx_tolerance_hz`, taken
                // from the pre-transition state so a callsign latched by this
                // very advance cannot widen it retroactively). Otherwise we
                // credit the offset we are on now, which is where we are
                // actually transmitting. Where the two baselines overlap (a
                // move smaller than the tolerance) the vacated one still wins:
                // it is the offset with a proven history.
                //
                // Round 5 re-review: that comparison holds the DX's decode
                // frequency against OUR vacated TX offset, so it is evidence
                // ONLY on a Tx=Rx QSO, where those are the same number. A QSO
                // that already carried a `partner_freq` before the switch
                // (Hound, an `o` hold, a de-conflict nudge, a passband clamp)
                // keeps it — `apply_tx_offset_switch` leaves an already-`Some`
                // latch alone — so the DX still transmits at `partner_freq`,
                // which matches neither of our offsets. The check would fail for
                // a reason that says nothing about which frame was answered, and
                // crediting the never-transmitted new offset off the back of it
                // is strictly worse than round 4's behaviour. So the check is
                // applied only when the QSO is Tx=Rx: no `partner_freq` at all.
                //
                // PAN-132 (Codex round 15 on PR #350, P2): that includes a split
                // this switch itself created. `apply_tx_offset_switch`'s
                // established-QSO latch sets `partner_freq` to exactly the
                // offset it vacated, so round 5's re-review counted that QSO as
                // Tx=Rx. But the latch exists because the DX is still
                // transmitting there, whatever moved us, so its reply decodes at
                // the vacated offset whichever of our frames it answered. The
                // relevance gate has already admitted it against that same
                // baseline with the same tolerance, so the check could not fail:
                // an operator nudge credited the offset it had just left, and
                // the next stall `Revert`ed the nudge. A `partner_freq` that
                // merely coincides with the vacated offset (an earlier move
                // landed back on it) is the same case. Within the grace,
                // `partner_freq` is therefore `None` only after a switch on an
                // unanswered `CallingCq`, where the latch deliberately does not
                // fire — the only case where the reply frequency is evidence.
                //
                // Round 6, finding 2: on ANY split — one that predates the
                // switch, or the one its own latch just created (PAN-132) — the
                // fallback is not one answer but two, and the provenance of the
                // relocation picks between them
                // (`PreSwitchOffset::operator_forced`). The DX transmits at
                // `partner_freq` no matter where we key, so its reply frequency
                // is silent about which of our offsets it answered — but the two
                // provenances carry different priors:
                //
                // - a STALL-triggered switch only fires BECAUSE the old offset
                //   demonstrably stopped working, and the triggering resend went
                //   out on it a slot before this commit. The vacated offset is
                //   the one with a proven history and the new one has never been
                //   transmitted on, so credit the vacated offset — round 4/5's
                //   behaviour, unchanged;
                // - an OPERATOR-forced `u` nudge indicts nothing. The operator
                //   chose to move; our very next frame goes out on the NEW
                //   offset, so an advancing reply after it is better evidence
                //   for the new offset than for the one we left deliberately.
                //   Crediting the vacated one there would aim a later `Revert`
                //   straight back at the offset the operator just escaped.
                //
                // PAN-83: provenance is a prior, not evidence — it says why we
                // moved, never which of our transmitted frames the reply
                // answered. On a split the aired-frame record now answers
                // exactly that: the TX worker reports every frame that played
                // out and the offset it aired on
                // (`confirm_stall_trigger_aired`), and the reply answers the
                // frame of ours that aired in the slot just before its own
                // (`answered_frame_offset_hz`). Three outcomes follow: a
                // relocation committed AFTER that frame aired (the ordinary
                // `u`-nudge timing — the keypress queues while our frame is
                // keyed and commits at the next drain) credits the vacated
                // offset; a frame that aired on the new offset before the
                // reply credits the new offset; and across chained
                // relocations the credit is the offset the answered frame
                // actually aired on, whatever the chain did before or after.
                // When no aired frame pins the answer down — no report in the
                // reply's window, no decoded slot parity — the round-6
                // provenance rule above decides, unchanged. The Tx=Rx branch
                // below keeps the decode-frequency rule: a station answering
                // our CQ transmits where it clicked us, so its decode
                // frequency already names the frame it answered, whatever the
                // aired record says.
                //
                // Consumed here either way: the first advance after a switch is
                // the one that resolves the ambiguity, and everything after it
                // is unambiguously about the current offset.
                let vacated = live_pre_switch_offset_at(
                    &progress.metadata,
                    message.timestamp,
                    self.pre_switch_offset_grace(),
                );
                let current_hz = progress.metadata.frequency;
                progress.metadata.last_known_good_offset_hz = Some(match vacated {
                    None => current_hz,
                    Some(pre) => {
                        let tx_equals_rx = progress.metadata.partner_freq.is_none();
                        let (credited_hz, evidence) = if tx_equals_rx {
                            // The decode frequency IS evidence here: it is the
                            // same number as our own TX offset.
                            if (pre.offset_hz - message.frequency).abs() <= rx_tolerance_hz {
                                // PAN-72 round-12 P1 fix (Codex round 12,
                                // thread on `qso_manager.rs:4047`): this
                                // sub-case is exactly a rescued, unanswered
                                // `CallingCq` — the caller's decoded frame
                                // matched the vacated baseline, which is only
                                // possible when there was no DX (and so no
                                // `partner_freq`) to latch at switch time.
                                // `apply_tx_offset_switch` deliberately left
                                // `partner_freq` at `None` then (see its own
                                // `their_callsign().is_some()` guard), but THIS
                                // advance is the establishing one: a caller has
                                // just answered on the offset we vacated. Latch
                                // it now, or the next hint-recompute reads the
                                // QSO's brand-new TX offset instead of where
                                // the caller actually is, and a relocation
                                // bigger than the decoder's narrow window
                                // stalls the exchange right after rescuing it.
                                //
                                // Since PAN-132 `tx_equals_rx` is exactly
                                // `partner_freq.is_none()`, so this guard always
                                // holds here. It stays so that a future
                                // widening of `tx_equals_rx` can never let this
                                // latch overwrite a split an established QSO
                                // already carries.
                                if progress.metadata.partner_freq.is_none() {
                                    progress.metadata.partner_freq = Some(pre.offset_hz);
                                }
                                (pre.offset_hz, "reply frequency")
                            } else {
                                (current_hz, "reply frequency")
                            }
                        } else if let Some(answered_hz) = answered_frame_offset_hz(
                            self.keep_call_ledger
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .get(&qso_id)
                                .and_then(|entry| entry.last_aired),
                            message.timestamp,
                            observed_slot_parity,
                            self.active_slot_ns.load(Ordering::Relaxed),
                        ) {
                            (answered_hz, "aired frame")
                        } else if pre.operator_forced {
                            (current_hz, "operator nudge")
                        } else {
                            (pre.offset_hz, "stall switch")
                        };
                        // PAN-83: one line per grace-window credit, on the
                        // target the on-air `u`-nudge drill reads (the same
                        // `tx.freq` target as the commit line in
                        // `apply_tx_offset_switch`). The evidence label names
                        // which rule decided.
                        info!(
                            target: "tx.freq",
                            qso_id = %qso_id,
                            "Adaptive TX-offset credit: {:.0} Hz known-good after leaving {:.0} Hz (evidence: {})",
                            credited_hz, pre.offset_hz, evidence
                        );
                        credited_hz
                    }
                });
                progress.metadata.pre_switch_offset = None;
                // PAN-72 (Codex round 1 on PR #350, finding 8): bump the
                // staleness token in lockstep with the two fields above, so a
                // TX-offset action queued BEFORE this advance is recognized as
                // stale when the coordinator's once-per-slot drain finally
                // commits it — see `QsoMetadata::advance_generation`.
                progress.metadata.advance_generation =
                    progress.metadata.advance_generation.saturating_add(1);
            }

            // Hound QSY (PAN-259): when the Fox sends us a signal report
            // (RespondingToCq → SendingReport), the Hound moves its TX offset
            // to the Fox's own decoded offset and sends the R+report
            // (`ReportAck`) there — the WSJT-X Hound procedure. A later
            // addressed report from the Fox on another admissible stream
            // re-anchors the Hound onto that stream.
            //
            // Executes INDEPENDENT of `TxFreqMode` (procedure-mandated, not an
            // autonomous optimisation — unlike the Auto-gated silence stall
            // detector above, which only fires in `TxFreqMode::Auto`), so Hold
            // honours it too. The admissibility check already ran before the
            // transition (`hound_report_offset`), so the target is exactly the
            // received offset.
            //
            // This mutates `metadata.frequency` (used as `qso_frequency` on the
            // NEXT process_message call), `qso_frequency` (rides the ReportAck
            // emitted this cycle), `partner_freq` (where we hear the Fox, and
            // the pin `hound_switch_range_hz` holds us to), the known-good
            // anchor, and the frequency inside the `SendingReport` state so
            // the subsequent `Completed` state logs our actual TX offset.
            let hound_fox_hz = if progress.metadata.hound
                && advanced_to_sending_report
                && (was_responding_to_cq || progress.metadata.hound_qsyed)
                && matches!(message.message_type, MessageType::SignalReport { .. })
                && self.is_hound_fox_frame(&progress.metadata, &message.message_type)
            {
                hound_report_offset(message.frequency, &self.config.hound)
            } else {
                None
            };
            if let Some(fox_hz) = hound_fox_hz {
                let fox_call = progress.metadata.their_callsign.as_deref().unwrap_or("");
                let old_off = progress.metadata.frequency;
                let moved = !progress.metadata.hound_qsyed
                    || (fox_hz - old_off).abs() > TX_OFFSET_NOOP_TOLERANCE_HZ;
                if moved {
                    // PAN-145: a frame snapshotted at the old offset must not
                    // key after this move.
                    self.relocation_epochs.bump(&qso_id.to_string());
                }
                progress.metadata.frequency = fox_hz;
                progress.metadata.partner_freq = Some(fox_hz);
                progress.metadata.pending_freq_drift = None;
                progress.metadata.hound_qsyed = true;
                // PAN-72 (Codex round 1 on PR #350, finding 5): re-anchor the
                // known-good offset onto the Fox offset. The
                // `dx_frame_advanced` block a few lines above just recorded
                // the calling offset as known-good, so without this a later
                // `SendingReport` stall would read `frequency !=
                // last_known_good` and emit a `Revert` back to the calling
                // offset, where the Fox is no longer listening.
                progress.metadata.last_known_good_offset_hz = Some(fox_hz);
                // Keep the ReportAck emitted this cycle on the Fox offset.
                qso_frequency = fox_hz;
                if let QsoState::SendingReport {
                    frequency: ref mut state_freq,
                    ..
                } = progress.state
                {
                    *state_freq = fox_hz;
                }
                if moved {
                    info!(
                        target: "hound",
                        qso_id = %qso_id,
                        fox = %fox_call,
                        "Hound: Fox sent our report — QSY TX {:.0} Hz -> {:.0} Hz (Fox offset), sending R+report there",
                        old_off, fox_hz
                    );
                }
            }
        }

        self.emit_event(QsoEvent::MessageReceived { qso_id, message })
            .await;

        // Record the auto-sequenced reply as a Sent message (under the lock)
        // so it is available to `resend_last_tx` and the UI snapshot. Render
        // the FT8 text so the TUI "TX:" line shows what we sent (UX audit
        // Batch 2 — was String::new()).
        if let Some(reply) = reply_to_emit.as_ref() {
            let reply_text = self.render_sent_text(reply);
            if let Some(progress) = qsos.get_mut(&qso_id) {
                progress.messages.push(QsoMessage {
                    timestamp: reply_sent_at,
                    direction: MessageDirection::Sent,
                    message_type: reply.clone(),
                    raw_text: reply_text,
                    signal_strength: None,
                    frequency: qso_frequency,
                });
            }
        }

        // A Hound relocation publishes while both locks still protect the
        // binding and frequency. Do not reacquire the QSO map under the gate.
        if hound_ptt_sync.is_some() {
            if let Some(reply) = reply_to_emit.take() {
                self.emit_event(QsoEvent::MessageToSend {
                    qso_id,
                    message: reply,
                    frequency: qso_frequency,
                    tx_parity: qso_tx_parity,
                    remote_origin: qso_remote_origin,
                    remote_client_key_id: qso_remote_client_key_id.clone(),
                    attempt_id: Some(reply_attempt_id.unwrap_or_else(|| self.next_attempt_id())),
                })
                .await;
            }
        }
        drop(hound_ptt_sync);

        // Release the QSO map write lock before emitting the reply so the
        // emission path holds no locks (and a future change to send_message-
        // style routing cannot deadlock).
        drop(qsos);

        for event in pending_rejections {
            self.emit_event(event).await;
        }

        // Emit the auto-sequenced reply for manual QSOs. We transmit on the
        // QSO's own frequency and reuse the tx_parity latched at QSO start,
        // exactly as the initial-call MessageToSend does.
        if let Some(reply) = reply_to_emit {
            // Re-read the remote binding here rather than trusting the
            // pre-drop snapshot: a concurrent takeover can rebind this QSO's
            // remote identity (`resend_existing_if_permitted`) and publish its own
            // Remote resend in the window since the write lock was released
            // above. Emitting the stale origin/client-id would misattribute
            // this reply (e.g. advertise it as Local) and could let a later
            // pivot/coalesce bypass the new client's arm gate (Codex P1,
            // round 17).
            let (remote_origin, remote_client_key_id) = self
                .current_remote_binding(qso_id, (qso_remote_origin, qso_remote_client_key_id))
                .await;
            self.emit_event(QsoEvent::MessageToSend {
                qso_id,
                message: reply,
                frequency: qso_frequency,
                tx_parity: qso_tx_parity,
                remote_origin,
                remote_client_key_id,
                attempt_id: Some(reply_attempt_id.unwrap_or_else(|| self.next_attempt_id())),
            })
            .await;
        }

        Ok(())
    }

    /// Forward position of a state on the responder's FT8 QSO ladder:
    /// RespondingToCq → SendingReport → WaitingForConfirmation → Completed.
    /// Higher means later in the conversation. Used only to detect a manual
    /// state *regression* (a transition whose rank decreased). States off this
    /// ladder (CallingCq, Idle, Failed, Contest, …) return `None` so they never
    /// register as a regression.
    /// Does `call` refer to *our* station, allowing a compound form of our own
    /// callsign (e.g. we operate as `K5ARH/P`)? Thin wrapper over
    /// [`crate::exchange::callsigns_match`] against `our_callsign`. Used in the
    /// `to == us` / `calling_station == us` halves of sender verification so a
    /// message directed at our compound call is not rejected as "not for us".
    fn is_us(&self, call: &str) -> bool {
        crate::exchange::callsigns_match(call, &self.config.our_callsign)
    }

    /// Is `from` the same station as our latched QSO partner `partner`, allowing
    /// a compound↔base change mid-QSO (catalog C18 / peer D4)? Thin wrapper over
    /// [`crate::exchange::callsigns_match`]. Used in the `from == DX` half of
    /// sender verification so an established QSO does not stall when the DX's
    /// displayed call gains or loses a portable prefix/suffix between frames.
    /// Deliberately conservative: genuinely different calls (`K5ARH`/`K5ARG`)
    /// still mismatch — see `callsigns_match` docs.
    /// PAN-259: whether `message_type` is a report or RR73 addressed to us by
    /// this Hound QSO's latched Fox. Only such a frame may move a Hound to a
    /// new Fox stream.
    fn is_hound_fox_frame(&self, metadata: &QsoMetadata, message_type: &MessageType) -> bool {
        let (from, to) = match message_type {
            MessageType::SignalReport {
                from_station,
                to_station,
                ..
            }
            | MessageType::FinalConfirmation {
                from_station,
                to_station,
            } => (from_station, to_station),
            _ => return false,
        };
        metadata
            .their_callsign
            .as_deref()
            .is_some_and(|fox| Self::is_partner(from, fox))
            && self.is_us(to)
    }

    fn is_partner(from: &str, partner: &str) -> bool {
        crate::exchange::callsigns_match(from, partner)
    }

    fn verify_sender(&self, from: &str, partner: &str, to: &str) -> Option<RejectionReason> {
        RejectionReason::classify(Self::is_partner(from, partner), self.is_us(to))
    }

    fn verify_addressee(&self, to: &str) -> Option<RejectionReason> {
        (!self.is_us(to)).then_some(RejectionReason::AddresseeNotUs)
    }

    async fn reject_sender(&self, qso_id: QsoId, from: &str, partner: &str, to: &str) -> bool {
        let Some(reason) = self.verify_sender(from, partner, to) else {
            return false;
        };
        self.emit_event(QsoEvent::MessageRejected {
            qso_id,
            reason,
            from_callsign: Some(from.to_string()),
            to_callsign: Some(to.to_string()),
        })
        .await;
        true
    }

    async fn reject_addressee(&self, qso_id: QsoId, from: &str, to: &str) -> bool {
        let Some(reason) = self.verify_addressee(to) else {
            return false;
        };
        self.emit_event(QsoEvent::MessageRejected {
            qso_id,
            reason,
            from_callsign: Some(from.to_string()),
            to_callsign: Some(to.to_string()),
        })
        .await;
        true
    }

    /// The **responder-flow** ladder, deliberately aligned 1:1 with
    /// [`Self::step_ladder_rank`]'s [`pancetta_core::ResponseStep`] mapping so
    /// an operator's context reply can be compared against where an existing
    /// manual QSO currently sits. It is Caller-role-shaped by construction —
    /// there is no `ResponseStep` for "I called CQ" or "I sent my report as
    /// the CQer", so `CallingCq`/`WaitingForReport` have no rung here and
    /// must not gain one.
    ///
    /// For "did this QSO make forward progress?" use [`Self::progress_rank`]
    /// instead — see its doc comment for why the two ladders are separate.
    fn ladder_rank(state: &QsoState) -> Option<u8> {
        match state {
            QsoState::RespondingToCq { .. } => Some(0),
            QsoState::SendingReport { .. } => Some(1),
            QsoState::WaitingForConfirmation { .. } => Some(2),
            QsoState::SendingConfirmation { .. } => Some(2),
            QsoState::Completed { .. } => Some(3),
            _ => None,
        }
    }

    /// PAN-72 (Codex round 1 on PR #350, finding 2): the **role-agnostic**
    /// forward-progress ladder, covering the CQer flow as well as the Caller
    /// flow. Used solely by `process_message_for_qso`'s `dx_frame_advanced`
    /// predicate, which resets [`QsoMetadata::stall_cycles`] and records
    /// [`QsoMetadata::last_known_good_offset_hz`] on a genuine advance.
    ///
    /// This is deliberately NOT [`Self::ladder_rank`]. That ladder exists to
    /// be compared against a [`pancetta_core::ResponseStep`] in the manual
    /// context-reply path and is Caller-role-shaped for that reason; widening
    /// it would silently change which context replies advance an existing QSO
    /// versus re-send its current outbound. Stall detection needs the
    /// opposite property — every real advance in EITHER role must count —
    /// so it gets its own function.
    ///
    /// The CQer rungs mirror the Caller's stage for stage:
    ///   0. opening sent (`CallingCq` / `RespondingToCq`)
    ///   1. our report is on the air but theirs has not arrived
    ///      (`WaitingForReport` — the CQer's report goes out on the
    ///      `CallingCq -> WaitingForReport` transition — /
    ///      `SendingReport { their_report: None }`)
    ///   2. the partner's report is in hand (`SendingReport { their_report:
    ///      Some(_) }`)
    ///   3. exchange rogered (`WaitingForConfirmation` / `SendingConfirmation`)
    ///   4. `Completed`
    ///
    /// Without rung 0/1 for the CQer, the real `CallingCq -> WaitingForReport`
    /// advance (a caller finally answering an unanswered CQ) read as "no
    /// progress": the stall streak accumulated during the unanswered CQs
    /// carried straight into the established exchange, so a single missed
    /// report slot could trip the threshold immediately — and without ever
    /// recording the just-proven offset as known-good, so the resulting action
    /// was a blind `Switch` rather than the intended anchored ping-pong.
    ///
    /// PAN-72 (Codex round 6 on PR #350, finding 1): `SendingReport` carries a
    /// real sub-rung and must not be collapsed into one. `RespondingToCq +
    /// CqResponse` lands us in `SendingReport { their_report: None }` (the DX
    /// returned our call with no report yet); the DX's first `SignalReport`
    /// then moves us to `SendingReport { their_report: Some(_) }`, which is
    /// what turns our outbound from a plain report into an R-report — the
    /// single most load-bearing advance in the whole exchange. Ranked equal,
    /// that transition read as "no progress": a switch request queued by the
    /// preceding threshold-hitting rearm left `advance_generation` unmoved, so
    /// the commit-time stale-action guard accepted it and relocated the QSO at
    /// the exact moment the DX finally answered; below the threshold, the
    /// partial stall streak carried forward instead of resetting.
    ///
    /// A repeated report (`Some(_) -> Some(_)`, the "the DX never copied our
    /// R" regression arm) still ranks equal, so it correctly reads as no
    /// progress. [`Self::ladder_rank`] is deliberately NOT given this
    /// sub-rung: the manual regression predicate relies on both `SendingReport`
    /// shapes comparing equal there.
    ///
    /// `Contest(ExchangingInfo)` gets the analogous `their_serial`
    /// `None`/`Some` split for the same reason — the contest wiring is not
    /// live yet, and ranking it correctly here keeps it from inheriting the
    /// bug when it is.
    ///
    /// `Failed` and `Idle` stay `None`: `None` is ordered below every `Some`,
    /// so a QSO going terminal-`Failed` can never read as an advance.
    fn progress_rank(state: &QsoState) -> Option<u8> {
        match state {
            QsoState::CallingCq { .. } | QsoState::RespondingToCq { .. } => Some(0),
            QsoState::WaitingForReport { .. }
            | QsoState::SendingReport {
                their_report: None, ..
            } => Some(1),
            QsoState::SendingReport {
                their_report: Some(_),
                ..
            } => Some(2),
            // Not currently constructed by `determine_state_transition`, but
            // ranked here so contest wiring inherits correct stall detection
            // rather than re-introducing this same bug.
            QsoState::Contest(ContestState::ExchangingInfo {
                their_serial: None, ..
            }) => Some(1),
            QsoState::Contest(ContestState::ExchangingInfo {
                their_serial: Some(_),
                ..
            }) => Some(2),
            QsoState::WaitingForConfirmation { .. } | QsoState::SendingConfirmation { .. } => {
                Some(3)
            }
            QsoState::Completed { .. } => Some(4),
            QsoState::Contest(ContestState::ContestCompleted { .. }) => Some(4),
            QsoState::Idle | QsoState::Failed { .. } => None,
        }
    }

    async fn determine_state_transition(
        &self,
        qso_id: QsoId,
        current_state: &QsoState,
        message_type: &MessageType,
        signal_strength: Option<f32>,
        initiated_by: CallInitiation,
        contest_engaged: bool,
    ) -> Result<QsoState, QsoManagerError> {
        match (current_state, message_type) {
            // CQ call received response (CQer flow). A station answered our CQ
            // with "<us> <them> <grid>". Verify the response is addressed to us
            // (calling_station == our callsign) before advancing — a spurious
            // CqResponse to another station must not hijack our CQ QSO. We latch
            // their grid here (UX audit Batch 2 #8) so the eventual ADIF carries
            // GRIDSQUARE; the relevance filter already directs only
            // addressed-to-us responses here, but we re-verify for defence.
            (
                QsoState::CallingCq { frequency, .. },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    grid,
                },
            ) => {
                if self
                    .reject_addressee(qso_id, responding_station, calling_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        got_to = %calling_station,
                        got_from = %responding_station,
                        "CqResponse not addressed to us ignored — no CQ advance"
                    );
                    return Ok(current_state.clone());
                }
                // SM-F4: latch the report we're about to send (the reply
                // emitter answers with our SignalReport on this transition)
                // so a repeated CqResponse can re-send an IDENTICAL value —
                // see WaitingForReport::our_report's doc comment. No prior
                // report exists to fall back on here, so the no-SNR default
                // matches every other first-time-computed report in this
                // file (-15).
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                Ok(QsoState::WaitingForReport {
                    their_callsign: responding_station.clone(),
                    frequency: *frequency,
                    started_at: self.now_utc(),
                    their_grid: grid.clone(),
                    our_report,
                })
            }

            // A4 (CQer flow — caller skips the grid): a station answers our CQ
            // with a bare signal report ("<us> <them> -NN") instead of the
            // usual grid frame. On-air this means "I copied you, here's your
            // report" — the caller already has our copy. The protocol-correct
            // next move for us (the CQer) is to send THEM our report, exactly
            // as we would after a grid-bearing CqResponse — so we advance to
            // WaitingForReport (same rung as the CqResponse path) and the reply
            // emitter sends our SignalReport. Without this arm CallingCq had no
            // SignalReport transition and we kept re-CQing forever.
            //
            // Sender-verified like every other arm: the report must be TO us
            // (we don't yet know who will answer our CQ, so any from_station is
            // accepted as the contra). We latch their callsign (from_station)
            // so the QSO carries the right contra call; no grid is available.
            (
                QsoState::CallingCq { frequency, .. },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    ..
                },
            ) => {
                if self
                    .reject_addressee(qso_id, from_station, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        got_to = %to_station,
                        got_from = %from_station,
                        "SignalReport not addressed to us ignored — no CQ advance (A4)"
                    );
                    return Ok(current_state.clone());
                }
                // SM-F4: same latch as the CqResponse arm above — the reply
                // emitter answers with our SignalReport here too, so the
                // value must be captured now for anti-jitter re-sends.
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                Ok(QsoState::WaitingForReport {
                    their_callsign: from_station.clone(),
                    frequency: *frequency,
                    started_at: self.now_utc(),
                    their_grid: None,
                    our_report,
                })
            }

            // A5 (CQer flow — caller closes early): after we (the CQer) sent our
            // report (now WaitingForReport) the caller fires RR73 / a plain 73
            // instead of acking with their R-report. The caller is done — accept
            // the early close, complete, and log. This is the CQer-side mirror
            // of the FIX-2 early-close arm the Caller flow already has
            // (SendingReport → Completed on RR73/73). Without it a
            // WaitingForReport CQer ignored the close and the QSO never
            // completed.
            //
            // Sender-verified (from == caller && to == us). We never received a
            // numeric report-ack from them, so log with our computed report and
            // a defaulted their_report.
            (
                QsoState::WaitingForReport {
                    their_callsign,
                    frequency,
                    started_at,
                    ..
                },
                MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious RR73/73 in WaitingForReport ignored (CQer, A5)"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                let duration = (self.now_utc() - *started_at).num_seconds().max(0) as u32;
                Ok(QsoState::Completed {
                    their_callsign: their_callsign.clone(),
                    their_report: -15,
                    our_report,
                    frequency: *frequency,
                    grid_square: None,
                    completed_at: self.now_utc(),
                    duration_seconds: duration,
                })
            }

            // SM-F4: we (the CQer) sent our SignalReport on the CallingCq →
            // WaitingForReport transition, but the caller never copied it and
            // repeats their opening CqResponse (their grid again). There was
            // previously NO arm at all for (WaitingForReport, CqResponse) — it
            // fell through to the no-op default and the QSO silently died at
            // the 30s report_timeout even though the caller was still there.
            // STAY in WaitingForReport (do not advance) and re-send the
            // IDENTICAL `our_report` we already latched — mirrors REGRESSION
            // 2's anti-jitter principle (recomputing from this decode's SNR
            // would jitter the report across repeats). exchange.rs has no
            // (WaitingForReport, CqResponse) reply arm, so returning
            // WaitingForReport here is a no-op for the in-slot reply emitter;
            // the rearm (`rearm_manual_calls_at`) owns the actual re-send.
            //
            // Keep the ALREADY-latched grid: we've already answered with a
            // report, not a grid, so a repeat carrying a (possibly different)
            // grid doesn't need to overwrite what we logged the first time.
            // Sender-verified exactly like the original CallingCq + CqResponse
            // arm (calling_station must be us; the responding_station must be
            // the SAME caller we already latched, not a new one hijacking the
            // slot).
            (
                QsoState::WaitingForReport {
                    their_callsign,
                    frequency,
                    started_at,
                    their_grid,
                    our_report,
                },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    ..
                },
            ) => {
                if self
                    .reject_sender(qso_id, responding_station, their_callsign, calling_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %responding_station,
                        got_to = %calling_station,
                        "spurious CqResponse in WaitingForReport ignored — no regression (SM-F4)"
                    );
                    return Ok(current_state.clone());
                }
                info!(
                    target: "qso.regression",
                    %their_callsign,
                    "CQer QSO staying in WaitingForReport \
                     (caller repeated their grid; they never copied our report)"
                );
                Ok(QsoState::WaitingForReport {
                    their_callsign: their_callsign.clone(),
                    frequency: *frequency,
                    started_at: *started_at,
                    their_grid: their_grid.clone(),
                    our_report: *our_report,
                })
            }

            // CQer flow: we sent our SignalReport (on the CallingCq→
            // WaitingForReport transition) and the caller rogered it with their
            // R-report (ReportAck). Advance to WaitingForConfirmation; the reply
            // emitter answers our FinalConfirmation (RR73). Carry the latched
            // grid into the confirmation state so it reaches Completed/ADIF.
            // Sender-verified (from == DX && to == us) like every other arm.
            (
                QsoState::WaitingForReport {
                    their_callsign,
                    frequency,
                    their_grid,
                    ..
                },
                MessageType::ReportAck {
                    from_station,
                    to_station,
                    report,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious ReportAck in WaitingForReport ignored (CQer)"
                    );
                    return Ok(current_state.clone());
                }
                // The caller's R-report is their report OF US. Our report (of
                // them) was computed when we sent it; recover it from SNR or
                // fall back to the report they just acked.
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(*report);
                Ok(QsoState::WaitingForConfirmation {
                    their_callsign: their_callsign.clone(),
                    their_report: *report,
                    our_report,
                    frequency: *frequency,
                    grid_square: their_grid.clone(),
                    started_at: self.now_utc(),
                })
            }

            // STUCK-AT-GRID FIX: we answered a CQer with our grid (now in
            // RespondingToCq) and the DX returns OUR call — either a bare
            // "<us> <DX>" or a "<us> <DX> <grid>" (a CqResponse directed at
            // us, carrying no report). On-air this means "I copied you, here
            // I am" — the DX heard our grid. The protocol-correct next move is
            // for us (the answering station) to send the DX a signal report,
            // advancing the contact. Without this arm we re-sent our grid every
            // slot until the manual watchdog timed out — the single
            // highest-frequency stall in the on-air log (N8ME, F5NNN, N9FME,
            // IQ0VT, KB5YNF, KA0NC, first-K9HJZ).
            //
            // A bare-call or grid answer to us parses as a CqResponse with
            // calling_station = us, responding_station = DX. Verify both
            // directions (from DX, to us) before advancing, exactly as on every
            // other arm. We carry no report from the DX yet (their_report:
            // None) and compute OUR report from the SNR.
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    ..
                },
            ) => {
                if self
                    .reject_sender(qso_id, responding_station, target_callsign, calling_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %responding_station,
                        got_to = %calling_station,
                        "spurious CqResponse in RespondingToCq ignored — sender/target mismatch"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                info!(
                    target: "qso.advance",
                    their_callsign = %target_callsign,
                    "DX returned our call without a report — advancing grid -> signal report"
                );
                Ok(QsoState::SendingReport {
                    their_callsign: target_callsign.clone(),
                    their_report: None,
                    our_report,
                    frequency: *frequency,
                    started_at: self.now_utc(),
                })
            }

            // Response to CQ, waiting for report
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    report,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, target_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious SignalReport ignored — sender does not match QSO target"
                    );
                    return Ok(current_state.clone());
                }
                // Use received signal strength (SNR) as our report, default to received report
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(*report);
                Ok(QsoState::SendingReport {
                    their_callsign: target_callsign.clone(),
                    their_report: Some(*report),
                    our_report,
                    frequency: *frequency,
                    started_at: self.now_utc(),
                })
            }

            // Phase-5 skip-rung: the DX skipped the plain-report rung and sent an
            // R-report (ReportAck) directly while we are still at grid
            // (RespondingToCq). Treat it like the (WaitingForReport, ReportAck)
            // close for the CQer role: advance to WaitingForConfirmation; the
            // reply emitter sends our RR73, and the (WaitingForConfirmation,
            // RR73/73) arm completes + logs on the DX's roger. Sender-verified
            // exactly as the SignalReport arm above.
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::ReportAck {
                    from_station,
                    to_station,
                    report,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, target_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious ReportAck in RespondingToCq ignored — sender does not match QSO target"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(*report);
                Ok(QsoState::WaitingForConfirmation {
                    their_callsign: target_callsign.clone(),
                    their_report: *report,
                    our_report,
                    frequency: *frequency,
                    grid_square: None,
                    started_at: self.now_utc(),
                })
            }

            // PAN-49 skip-rung: a state-QSO-party / ARRL Intl Digital
            // partner acks our grid with "R"+grid instead of a numeric
            // report — same close-shape as the ReportAck skip-rung arm
            // above (RespondingToCq -> WaitingForConfirmation), but with no
            // real dB value to carry. `-15` is the established "no real
            // report" sentinel used throughout this file (e.g. lines 3238,
            // 3544, 10260) for exactly this situation. `is_ack: false`
            // (the plain first-exchange grid, already handled by the
            // ordinary CqResponse path) never reaches this arm: both
            // producers of `ContestReply` -- process_message_with_parity's
            // text reclassification (PAN-49) and ft8_message_to_qso_type's
            // direct typed classification (PAN-51) -- only ever set
            // `is_ack: true`.
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::ContestReply {
                    from_station,
                    to_station,
                    grid,
                    is_ack: true,
                },
            ) => {
                // PR #344 round-1 Codex P2: PAN-51 lets a native ReplyWithR
                // decode reach this arm directly, bypassing the QSO-
                // engagement check process_message_with_parity's text-
                // reclassification path applies before ever producing
                // ContestReply. Without this, an ORDINARY non-contest QSO
                // advances and logs a contest-shaped exchange the moment
                // its real partner sends an "R"+grid shape for any reason.
                // `contest_engaged` is this QSO's own
                // `metadata.contest_info.is_some()`, captured by the caller
                // -- scoped per-QSO, matching the reclassification gate's
                // own scoping (final review Finding 2 there).
                if !contest_engaged {
                    debug!(
                        target: "qso.security",
                        qso_id = %qso_id,
                        from = %from_station,
                        "native ContestReply in RespondingToCq ignored — QSO is not contest-engaged"
                    );
                    return Ok(current_state.clone());
                }
                if self
                    .reject_sender(qso_id, from_station, target_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious ContestReply in RespondingToCq ignored — sender does not match QSO target"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                Ok(QsoState::WaitingForConfirmation {
                    their_callsign: target_callsign.clone(),
                    their_report: -15,
                    our_report,
                    frequency: *frequency,
                    grid_square: Some(grid.clone()),
                    started_at: self.now_utc(),
                })
            }

            // qso-state-machine-analysis GAP-1: the DX skips BOTH remaining
            // rungs and closes directly from our opening grid (RespondingToCq)
            // with RR73/73 — they copied us on the first exchange and are
            // impatient. Mirrors the FIX-2 early-close below (SendingReport)
            // and the A5 early-close (WaitingForReport), which is asymmetric
            // without this arm: RespondingToCq had no early-close, so this
            // exchange previously stalled at grid until the watchdog retired
            // it. their_report is unknown (we never received one) — default to
            // -15 like every other early-close; our_report is best-effort from
            // this closing frame's SNR (we never got a chance to compute it
            // any other way, matching the skip-rung arm above).
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, target_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious RR73/73 in RespondingToCq ignored"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                Ok(QsoState::Completed {
                    their_callsign: target_callsign.clone(),
                    their_report: -15,
                    our_report,
                    frequency: *frequency,
                    grid_square: None,
                    completed_at: self.now_utc(),
                    duration_seconds: 0,
                })
            }

            // Received report acknowledgment
            (
                QsoState::SendingReport {
                    their_callsign,
                    their_report,
                    our_report,
                    frequency,
                    ..
                },
                MessageType::ReportAck {
                    from_station,
                    to_station,
                    report,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious ReportAck ignored"
                    );
                    return Ok(current_state.clone());
                }
                // With no report latched yet (we answered with a report, as a
                // Fox answers a Hound, or the DX returned our call bare), the
                // R-report the DX just sent is their report of us; log that,
                // not the -15 placeholder (PAN-386).
                Ok(QsoState::WaitingForConfirmation {
                    their_callsign: their_callsign.clone(),
                    their_report: their_report.unwrap_or(*report),
                    our_report: *our_report,
                    frequency: *frequency,
                    grid_square: None,
                    started_at: self.now_utc(),
                })
            }

            // FIX 2: the DX rogered our R-report directly with RR73 (or a
            // plain 73). Real FT8 is a 4-message QSO and RR73 is the close,
            // so we must complete (and the reply emitter answers our 73).
            // Without this arm the QSO stalled one message short — the DX's
            // RR73 was ignored and the contact was never logged. We accept
            // both FinalConfirmation (RR73/RRR-class close) and a bare
            // SeventyThree (73) here.
            (
                QsoState::SendingReport {
                    their_callsign,
                    their_report,
                    our_report,
                    frequency,
                    started_at,
                },
                MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious RR73/73 in SendingReport ignored"
                    );
                    return Ok(current_state.clone());
                }
                let duration = (self.now_utc() - *started_at).num_seconds().max(0) as u32;
                Ok(QsoState::Completed {
                    their_callsign: their_callsign.clone(),
                    their_report: their_report.unwrap_or(-15),
                    our_report: *our_report,
                    frequency: *frequency,
                    grid_square: None,
                    completed_at: self.now_utc(),
                    duration_seconds: duration,
                })
            }

            // Received final confirmation
            (
                QsoState::WaitingForConfirmation {
                    their_callsign,
                    their_report,
                    our_report,
                    frequency,
                    grid_square,
                    started_at,
                },
                MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious FinalConfirmation ignored"
                    );
                    return Ok(current_state.clone());
                }
                let duration = (self.now_utc() - *started_at).num_seconds().max(0) as u32;
                Ok(QsoState::Completed {
                    their_callsign: their_callsign.clone(),
                    their_report: *their_report,
                    our_report: *our_report,
                    frequency: *frequency,
                    grid_square: grid_square.clone(),
                    completed_at: self.now_utc(),
                    duration_seconds: duration,
                })
            }

            // === STATE REGRESSION (manual-initiated QSOs only) ===========
            // Operator principle: "if a DX station re-sends something EARLIER
            // in the conversation, they obviously didn't receive our response —
            // back ourselves up to where THEY think we are."
            //
            // These arms are gated on CallInitiation::Manual so autonomous
            // QSOs are unaffected. Sender verification (from == DX && to == us)
            // is preserved on every regression exactly as on forward arms.

            // REGRESSION 1: we sent RR73 (WaitingForConfirmation) but the DX is
            // still sending us their SignalReport — they never copied our R.
            // Back up two steps to SendingReport and re-send our R-report (the
            // reply emitter answers a ReportAck for this (state, msg) pair).
            // Latch the newest report value the DX sent.
            //
            // SM-F6 safety note: this arm resets `started_at: self.now_utc()` on
            // every fire, which would let a repeatedly-regressing QSO reset
            // its own timeout clock indefinitely. Deliberately left
            // Manual-only (not extended to Auto) rather than adding a bounded
            // counter — WaitingForConfirmation is also outside SM-F6's scope
            // (only RespondingToCq/SendingReport get Auto resend/regression),
            // and it already has a uniform `confirmation_timeout` bound for
            // BOTH Manual and Auto QSOs (it's not in the Manual keep-call
            // watchdog list), so an Auto QSO here is already safely bounded
            // by that timeout without this arm's help.
            (
                QsoState::WaitingForConfirmation {
                    their_callsign,
                    our_report,
                    frequency,
                    ..
                },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    report,
                },
            ) if initiated_by == CallInitiation::Manual => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious SignalReport in WaitingForConfirmation ignored — no regression"
                    );
                    return Ok(current_state.clone());
                }
                // Recompute our report from the freshest SNR (fall back to the
                // already-latched value), and latch the DX's newest report.
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(*our_report);
                info!(
                    target: "qso.regression",
                    %their_callsign,
                    "manual QSO regressing WaitingForConfirmation → SendingReport \
                     (DX repeated their report; they never copied our R)"
                );
                Ok(QsoState::SendingReport {
                    their_callsign: their_callsign.clone(),
                    their_report: Some(*report),
                    our_report,
                    frequency: *frequency,
                    started_at: self.now_utc(),
                })
            }

            // REGRESSION 2: we sent our R (SendingReport) and the DX re-sends
            // their SignalReport — they didn't copy our R. STAY in
            // SendingReport (do not advance); the per-slot rearm
            // (`rearm_manual_calls_at`, FIX 4) keeps re-sending our R-report.
            // We refresh THEIR report to the newest value for the log, but keep
            // OUR sent report LATCHED — the report we give a station must not
            // jitter with per-decode SNR noise when the DX re-requests (observed
            // on-air: R-7 → R-9 → R-6 across repeats of the same DX report).
            // Returning a SendingReport here drives the their-report update
            // without the reply emitter double-sending: exchange.rs has no
            // (SendingReport, SignalReport) response arm, so the in-slot emit
            // path is a no-op and the rearm owns the (now stable-valued) re-send.
            //
            // SM-F6: extended to Auto QSOs too (no `initiated_by` guard) —
            // unlike REGRESSION 1/3, this arm explicitly PRESERVES
            // `started_at` (see below) rather than resetting it, so an Auto
            // QSO stuck here is still bounded by the ordinary 30s
            // report_timeout counted from when SendingReport was first
            // entered; it cannot reset its own clock by looping through this
            // arm. Safe to extend.
            (
                QsoState::SendingReport {
                    their_callsign,
                    our_report,
                    frequency,
                    started_at,
                    ..
                },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    report,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious SignalReport in SendingReport ignored — no regression"
                    );
                    return Ok(current_state.clone());
                }
                // Keep OUR report latched (do NOT recompute from this decode's
                // SNR) — see the comment above; only their_report refreshes.
                Ok(QsoState::SendingReport {
                    their_callsign: their_callsign.clone(),
                    their_report: Some(*report),
                    our_report: *our_report,
                    frequency: *frequency,
                    // Preserve started_at so the manual watchdog keeps measuring
                    // from the original QSO start — a regression must not reset
                    // the keep-calling clock.
                    started_at: *started_at,
                })
            }

            // REGRESSION 3: we sent RR73 (WaitingForConfirmation) but the DX
            // re-sends their original grid/call (CqResponse) — they restarted
            // the whole exchange. Back up to RespondingToCq and re-send our
            // grid/call. Only observable when the repeated message parses as a
            // CqResponse directed appropriately for this QSO.
            //
            // SM-F6 safety note: like REGRESSION 1, this arm resets
            // `started_at: self.now_utc()` on every fire. Deliberately left
            // Manual-only for the same reason as REGRESSION 1 — it operates
            // on WaitingForConfirmation, which is outside SM-F6's scope and
            // already carries a uniform `confirmation_timeout` bound for both
            // Manual and Auto QSOs.
            (
                QsoState::WaitingForConfirmation {
                    their_callsign,
                    frequency,
                    ..
                },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    ..
                },
            ) if initiated_by == CallInitiation::Manual => {
                // A "DX K5ARH GRID" repeat parses with calling_station = us,
                // responding_station = DX. Verify both directions before
                // regressing so a spurious station cannot reset our QSO.
                if self
                    .reject_sender(qso_id, responding_station, their_callsign, calling_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %responding_station,
                        got_to = %calling_station,
                        "spurious CqResponse in WaitingForConfirmation ignored — no regression"
                    );
                    return Ok(current_state.clone());
                }
                info!(
                    target: "qso.regression",
                    %their_callsign,
                    "manual QSO regressing WaitingForConfirmation → RespondingToCq \
                     (DX restarted the exchange)"
                );
                Ok(QsoState::RespondingToCq {
                    target_callsign: their_callsign.clone(),
                    frequency: *frequency,
                    started_at: self.now_utc(),
                })
            }

            // No state change
            _ => Ok(current_state.clone()),
        }
    }

    async fn find_qsos_for_message(
        &self,
        message_type: &MessageType,
        frequency: f64,
    ) -> Vec<QsoId> {
        let mut matching_qsos = Vec::new();
        let mut rejections = Vec::new();
        {
            let qsos = self.qsos.read().await;

            // PAN-14 / SM-F7 (docs/qso-tx-deep-review-2026-07-18.md §A.3):
            // a station that already has an ESTABLISHED active QSO (i.e. its
            // `their_callsign` is latched — every state past CallingCq/Idle)
            // must not ALSO be accepted as a fresh answerer by an UNRELATED
            // CallingCq QSO's "any station" relevance arms. Without this, the
            // same decoded frame (e.g. that station's report of us) can
            // independently satisfy both QSOs' relevance arms —
            // `find_qsos_for_message` then advances BOTH, producing two
            // simultaneously-active QSO objects partnered with the same
            // real-world station. Each generates its own TX cadence
            // (possibly at a different audio offset), which is the "two
            // very-high-amplitude signals for one station in one TX window"
            // failure mode — see AGENTS.md "at most one QSO object exists
            // per (callsign, band)". Computed once per incoming message
            // (not per-QSO) since it doesn't depend on the candidate QSO.
            //
            // P2 (Codex round-1 review of PR #250): also treat a QSO that
            // COMPLETED with this exact sender within
            // `COMPLETED_QSO_REWORK_GRACE` as "already accounted for" —
            // mirrors `has_active_or_recent_qso_with`'s active-or-recent
            // pattern (and the coordinator's matching 45 s completed-TX
            // grace). Without this, the `is_active()`-only predicate goes
            // false the instant a QSO completes, so a station whose contact
            // with us just finished could be immediately re-claimed by an
            // unrelated CallingCq QSO's "any station" arm on a stray/
            // duplicate frame, opening a second QSO object for a station we
            // just worked seconds ago.
            //
            // PAN-25 (Codex round-2 review of PR #250): the completed arm must
            // also match band, not just callsign — the uniqueness invariant
            // (AGENTS.md) is per (callsign, band). Without this, a station
            // worked on band A stays "accounted for" against a fresh CQ reply
            // from the same call on band B for the whole grace window, so a
            // legitimate new QSO on the new band is silently dropped. The
            // active arm above is intentionally left unscoped by band — an
            // active QSO can only exist on the band currently being decoded.
            //
            // PAN-25 round 1 (Codex): `frequency` here is an audio offset (a
            // few hundred/thousand Hz), and so -- pre-fix -- was the stored
            // `p.metadata.frequency` for a Completed QSO; comparing two audio
            // offsets via `frequency_to_band` always yields the same "0MHZ"
            // bucket regardless of the real RF band, silently defeating this
            // check. A Completed QSO's `metadata.frequency` is now stamped
            // with the true RF frequency (dial + offset) at completion time
            // (see `process_message_for_qso`), so `frequency` must be
            // adjusted the same way here for a fair comparison.
            let now = self.now_utc();
            let want_band = crate::utils::frequency_to_band(self.current_rf_frequency(frequency));
            let sender_has_other_active_or_recent_partner =
                message_type.sender_callsign().is_some_and(|sender| {
                    qsos.values().any(|p| {
                        let same_sender = p
                            .metadata
                            .their_callsign
                            .as_deref()
                            .is_some_and(|c| crate::exchange::callsigns_match(c, sender));
                        if !same_sender {
                            return false;
                        }
                        if p.state.is_active() {
                            return true;
                        }
                        if let QsoState::Completed { completed_at, .. } = &p.state {
                            let completed_band = crate::utils::frequency_to_band(
                                p.metadata
                                    .completed_rf_frequency_hz
                                    .unwrap_or(p.metadata.frequency),
                            );
                            return completed_band == want_band
                                && now.signed_duration_since(*completed_at).num_seconds()
                                    <= COMPLETED_QSO_REWORK_GRACE.num_seconds();
                        }
                        false
                    })
                });

            for (&qso_id, progress) in qsos.iter() {
                if !progress.state.is_active() {
                    continue;
                }

                let verdict = self.classify_relevance(
                    &progress.state,
                    &progress.metadata,
                    message_type,
                    frequency,
                    sender_has_other_active_or_recent_partner,
                );
                if verdict.relevant {
                    matching_qsos.push(qso_id);
                } else if let Some(reason) = verdict.reason {
                    warn!(
                        target: "qso.security",
                        %qso_id,
                        reason = reason.as_str(),
                        got_from = ?message_type.sender_callsign(),
                        "frame entangled with an active QSO failed sender verification — not routed"
                    );
                    rejections.push(QsoEvent::MessageRejected {
                        qso_id,
                        reason,
                        from_callsign: message_type.sender_callsign().map(str::to_string),
                        to_callsign: message_type.addressee_callsign().map(str::to_string),
                    });
                }
            }

            // P1 (Codex round-1 review of PR #250): a reply can independently
            // satisfy MULTIPLE unpartnered `CallingCq` QSOs' relevance arms —
            // e.g. via repeated `StartCq`/`start_cq_manual` calls, or Fox
            // mode engaging while a CQ is already live, both of which insert
            // another `CallingCq` QSO with no guard against one already
            // existing. Every `CallingCq` QSO has `metadata.their_callsign ==
            // None`, so the guard above (keyed on an ESTABLISHED partner)
            // cannot see a conflict between two still-unpartnered `CallingCq`
            // QSOs — both independently pass their "any station" arm and
            // both would advance, again leaving one real station partnered
            // with two `qso_id`s. Restrict to the single earliest-created
            // (`metadata.start_time`) `CallingCq` match — first CQ up, first
            // CQ answered — dropping any others so only ONE advances.
            let calling_cq_matches: Vec<QsoId> = matching_qsos
                .iter()
                .copied()
                .filter(|id| {
                    matches!(
                        qsos.get(id).map(|p| &p.state),
                        Some(QsoState::CallingCq { .. })
                    )
                })
                .collect();
            if calling_cq_matches.len() > 1 {
                let earliest = calling_cq_matches
                    .iter()
                    .copied()
                    .min_by_key(|id| qsos.get(id).map(|p| p.metadata.start_time))
                    .expect("calling_cq_matches is non-empty");
                matching_qsos.retain(|id| *id == earliest || !calling_cq_matches.contains(id));
            }
        }

        // A frame that matched any active QSO is legitimate traffic for this
        // manager.  Do not report it as an impostor against every other
        // concurrent QSO whose state happens to accept the same message kind.
        if matching_qsos.is_empty() {
            for event in rejections {
                self.emit_event(event).await;
            }
        }

        matching_qsos
    }

    fn classify_relevance(
        &self,
        state: &QsoState,
        metadata: &QsoMetadata,
        message_type: &MessageType,
        frequency: f64,
        sender_has_other_active_or_recent_partner: bool,
    ) -> Relevance {
        let relevant = self.is_message_relevant(
            state,
            metadata,
            message_type,
            frequency,
            sender_has_other_active_or_recent_partner,
        );
        if relevant {
            return Relevance {
                relevant: true,
                reason: None,
            };
        }

        // Routing-stage diagnostics are only meaningful when the decode is
        // close enough to be entangled with this QSO.  Otherwise ordinary
        // traffic elsewhere in the passband would be classified against every
        // active QSO.
        // Mirrors `is_message_relevant`'s gate exactly, including the
        // recently-vacated-offset baseline (PAN-72, round 4, finding 3) — the
        // two must agree on what counts as "entangled with this QSO".
        let within_frequency_gate = state.frequency().is_none_or(|qso_frequency| {
            let match_frequency = metadata.partner_freq.unwrap_or(qso_frequency);
            let tolerance = if state.their_callsign().is_some() {
                ESTABLISHED_FREQ_TOLERANCE_HZ
            } else {
                FREQ_TOLERANCE_HZ
            };
            let matches_baseline = |baseline: f64| (baseline - frequency).abs() <= tolerance;
            // PAN-386: same Fox-slot baseline as `is_message_relevant`.
            let fox_slot_baseline = metadata.fox
                && state.their_callsign().is_some()
                && matches_baseline(metadata.frequency);
            matches_baseline(match_frequency)
                || fox_slot_baseline
                || live_pre_switch_offset_at(
                    metadata,
                    self.now_utc(),
                    self.pre_switch_offset_grace(),
                )
                .is_some_and(|pre| matches_baseline(pre.offset_hz))
        });
        if !within_frequency_gate {
            return Relevance {
                relevant: false,
                reason: None,
            };
        }

        let verify = |from: &str, partner: &str, to: &str| {
            RejectionReason::classify(Self::is_partner(from, partner), self.is_us(to))
                // A partner working a third party is routine band traffic, not
                // a security event.  At routing time only an impostor sending
                // to us is distinguishable from ordinary traffic.
                .filter(|reason| *reason == RejectionReason::SenderNotPartner)
        };
        let reason = match (state, message_type) {
            (
                QsoState::WaitingForReport { their_callsign, .. },
                MessageType::ReportAck {
                    from_station,
                    to_station,
                    ..
                }
                | MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => verify(from_station, their_callsign, to_station),
            (
                QsoState::RespondingToCq {
                    target_callsign, ..
                },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    ..
                },
            ) => verify(from_station, target_callsign, to_station),
            (
                QsoState::RespondingToCq {
                    target_callsign, ..
                },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    ..
                },
            ) => verify(responding_station, target_callsign, calling_station),
            (
                QsoState::SendingReport { their_callsign, .. },
                MessageType::ReportAck {
                    from_station,
                    to_station,
                    ..
                }
                | MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => verify(from_station, their_callsign, to_station),
            (
                QsoState::WaitingForConfirmation { their_callsign, .. },
                MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => verify(from_station, their_callsign, to_station),
            _ => None,
        };
        Relevance {
            relevant: false,
            reason,
        }
    }

    fn is_message_relevant(
        &self,
        state: &QsoState,
        metadata: &QsoMetadata,
        message_type: &MessageType,
        frequency: f64,
        sender_has_other_active_or_recent_partner: bool,
    ) -> bool {
        // Tolerances are module-level shared consts (`FREQ_TOLERANCE_HZ`,
        // `ESTABLISHED_FREQ_TOLERANCE_HZ`) — see their doc comments for the
        // security-review rationale (C-1) and the B15 established-QSO
        // widening rationale. `classify_relevance` and
        // `maybe_confirm_frequency_drift_at` must match these values; PAN-15
        // item 6 hoisted all three sites onto one definition.

        let matched = match (state, message_type) {
            // We're calling CQ. The responder's callsign is whoever is in the
            // `responding_station` field; the message must be addressed to us.
            // PAN-14/SM-F7: a CallingCq QSO has no established partner yet,
            // so it would otherwise accept a frame from ANY station — even
            // one that already has a different, established active QSO with
            // us. Excluding that case here stops the same real station from
            // becoming the partner of two simultaneously-active QSO objects.
            (
                QsoState::CallingCq { .. },
                MessageType::CqResponse {
                    calling_station, ..
                },
            ) => self.is_us(calling_station) && !sender_has_other_active_or_recent_partner,

            // A4 (routing half): a caller answered our CQ with a bare signal
            // report (grid skipped) — "<us> <them> -NN". Route it to this
            // CallingCq QSO so the transition arm can step CQ → report. Only
            // addressed-to-us reports qualify (any from_station, since we don't
            // yet know who will answer) — UNLESS that from_station already has
            // a different established active QSO (PAN-14/SM-F7, see above).
            (QsoState::CallingCq { .. }, MessageType::SignalReport { to_station, .. }) => {
                self.is_us(to_station) && !sender_has_other_active_or_recent_partner
            }

            // CQer flow: we called CQ, the caller answered, and we sent our
            // report (now WaitingForReport). The caller's R-report (ReportAck)
            // is the next message — route it to this QSO so it can close.
            // Verify both directions: from THEM, to US.
            (
                QsoState::WaitingForReport { their_callsign, .. },
                MessageType::ReportAck {
                    to_station,
                    from_station,
                    ..
                },
            ) => Self::is_partner(from_station, their_callsign) && self.is_us(to_station),

            // A5 (routing half): the caller closed early with RR73 / 73 from
            // WaitingForReport (before sending their R-report). Route the close
            // to this QSO so the transition arm can complete it. Both directions
            // verified: from THEM, to US.
            (
                QsoState::WaitingForReport { their_callsign, .. },
                MessageType::FinalConfirmation {
                    to_station,
                    from_station,
                }
                | MessageType::SeventyThree {
                    to_station,
                    from_station,
                },
            ) => Self::is_partner(from_station, their_callsign) && self.is_us(to_station),

            // We responded to a CQ from `target_callsign` and are waiting for
            // their report. Verify both directions: from THEM, to US.
            (
                QsoState::RespondingToCq {
                    target_callsign, ..
                },
                MessageType::SignalReport {
                    to_station,
                    from_station,
                    ..
                },
            ) => Self::is_partner(from_station, target_callsign) && self.is_us(to_station),

            // STUCK-AT-GRID FIX (routing half): the DX answered our grid by
            // returning our call (bare "<us> <DX>" or "<us> <DX> <grid>") — a
            // CqResponse directed at us. Route it to this QSO so the transition
            // arm can step grid -> report. Verify both directions: from THEM
            // (responding_station), to US (calling_station).
            (
                QsoState::RespondingToCq {
                    target_callsign, ..
                },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    ..
                },
            ) => {
                Self::is_partner(responding_station, target_callsign) && self.is_us(calling_station)
            }

            // We sent the report and are waiting for the report-ack. Same check.
            (
                QsoState::SendingReport { their_callsign, .. },
                MessageType::ReportAck {
                    to_station,
                    from_station,
                    ..
                },
            ) => Self::is_partner(from_station, their_callsign) && self.is_us(to_station),

            // FIX 2: the DX may close directly from our R-report with RR73
            // (or a plain 73) instead of acking first — accept it here so it
            // routes to this QSO. Both directions verified.
            (
                QsoState::SendingReport { their_callsign, .. },
                MessageType::FinalConfirmation {
                    to_station,
                    from_station,
                }
                | MessageType::SeventyThree {
                    to_station,
                    from_station,
                },
            ) => Self::is_partner(from_station, their_callsign) && self.is_us(to_station),

            // Awaiting RR73 — verify both directions. Accept a plain 73 too
            // (DX skipped RR73).
            (
                QsoState::WaitingForConfirmation { their_callsign, .. },
                MessageType::FinalConfirmation {
                    to_station,
                    from_station,
                }
                | MessageType::SeventyThree {
                    to_station,
                    from_station,
                },
            ) => Self::is_partner(from_station, their_callsign) && self.is_us(to_station),

            _ => {
                // Anything else: only relevant if addressed to us.
                message_type.is_addressed_to(&self.config.our_callsign)
            }
        };

        if !matched {
            return false;
        }

        // Apply the frequency gate AFTER the callsign/to/state match (B15). A
        // matched message from an ESTABLISHED QSO's partner is allowed the
        // wider drift bound; everything else uses the tight default. An
        // established QSO is one where we already know the contra callsign
        // (i.e. not CallingCq/Idle) — `their_callsign()` is Some.
        //
        // Split-TX: when `metadata.partner_freq` is `Some`, the DX transmits on
        // a different frequency than we do. This includes Hound/Fox as well as
        // ordinary manual offset holds, collision nudges, and passband clamps.
        // Match incoming frames against where we hear the DX (`partner_freq`),
        // not our TX offset. When it is `None`, `unwrap_or(qso_freq)` preserves
        // the ordinary Tx=Rx path byte-for-byte.
        //
        // PAN-72 (Codex round 4 on PR #350, finding 3): an offset we moved away
        // from moments ago is a SECOND valid baseline for the length of
        // `PRE_SWITCH_OFFSET_GRACE`. The frame that triggered the relocation was
        // transmitted there, so its answer necessarily arrives after the move —
        // and for an unanswered `CallingCq`, where `partner_freq` is
        // deliberately `None` and the gate is only 15 Hz wide, judging that
        // answer against the new offset rejects it outright and
        // `maybe_answer_caller` spawns a duplicate QSO in its place. The window
        // is bounded in time, uses the SAME tolerance, and is on top of (never
        // instead of) the current baseline; the callsign/state match above still
        // gates who may use it.
        // PAN-259: a Fox runs several streams and may answer us on any of
        // them, more than the established tolerance away from where we last
        // heard it. For an active Hound, a report or RR73 that is addressed
        // to us AND sent by the latched Fox (checked here, because some
        // state/message pairs above fall through to an addressee-only arm)
        // is relevant anywhere inside the Fox response envelope.
        if metadata.hound
            && self.is_hound_fox_frame(metadata, message_type)
            && hound_report_offset(frequency, &self.config.hound).is_some()
        {
            return true;
        }
        if let Some(qso_freq) = state.frequency() {
            let match_freq = metadata.partner_freq.unwrap_or(qso_freq);
            let tolerance = if state.their_callsign().is_some() {
                ESTABLISHED_FREQ_TOLERANCE_HZ
            } else {
                FREQ_TOLERANCE_HZ
            };
            let matches_baseline = |baseline: f64| (baseline - frequency).abs() <= tolerance;
            // PAN-386: a WSJT-X Hound answers on the audio frequency where it
            // decoded the Fox's report, which is the Fox-role QSO's own TX
            // slot, not the Hound's calling frequency in `partner_freq`. Once
            // the partner call is known, that slot is one more baseline; the
            // callsign and state match above still decide who may use it.
            let fox_slot_baseline = metadata.fox
                && state.their_callsign().is_some()
                && matches_baseline(metadata.frequency);
            if !matches_baseline(match_freq)
                && !fox_slot_baseline
                && !live_pre_switch_offset_at(
                    metadata,
                    self.now_utc(),
                    self.pre_switch_offset_grace(),
                )
                .is_some_and(|pre| matches_baseline(pre.offset_hz))
            {
                return false;
            }
        }

        true
    }

    async fn check_duplicate(
        &self,
        callsign: &str,
        frequency: f64,
    ) -> Result<bool, QsoManagerError> {
        self.check_duplicate_at(callsign, frequency, self.now_utc())
            .await
    }

    async fn check_duplicate_at(
        &self,
        callsign: &str,
        frequency: f64,
        now: DateTime<Utc>,
    ) -> Result<bool, QsoManagerError> {
        let config = &self.config.duplicate_checking;
        if !config.enabled || config.time_window_hours == 0 {
            return Ok(false);
        }
        let unavailable = |message: &str| QsoManagerError::Database {
            source: anyhow::anyhow!("QSO history unavailable: {message}"),
        };
        if self.require_database && self.database.is_none() {
            return Err(unavailable("persistent index is not attached"));
        }
        let rf = self.current_rf_frequency(frequency);
        let band = Some(crate::utils::frequency_to_band(rf)).filter(|b| !b.ends_with("MHZ"));
        // Standalone memory-only managers may have no rig source. Keep their
        // unknown-band history in memory; persistent checks require a real band.
        if config.check_frequency
            && band.is_none()
            && (self.require_database || self.database.is_some())
        {
            return Err(unavailable("TX band is unknown"));
        }
        let band = band.map(|b| b.to_uppercase());
        // Query first: even a memory hit must not hide an unavailable required index.
        if let Some(db) = &self.database {
            if db
                .check_duplicate(
                    callsign,
                    if config.check_frequency {
                        band.as_deref()
                    } else {
                        None
                    },
                    now,
                    config.time_window_hours,
                )
                .await
                .map_err(|e| QsoManagerError::Database {
                    source: anyhow::anyhow!("QSO history unavailable: {e}"),
                })?
                .is_some()
            {
                return Ok(true);
            }
        }
        // Reserve an in-flight exchange independently of worked-contact history.
        if self.has_active_qso_with(callsign).await {
            return Ok(true);
        }
        let cutoff = now - Duration::hours(config.time_window_hours as i64);
        let qsos = self.qsos.read().await;
        Ok(qsos.values().any(|p| {
            matches!(p.state, QsoState::Completed { .. })
                && p.metadata
                    .their_callsign
                    .as_deref()
                    .is_some_and(|c| c.trim().eq_ignore_ascii_case(callsign.trim()))
                && p.metadata.start_time > cutoff
                && p.metadata.start_time <= now
                && (!config.check_frequency
                    || p.metadata
                        .completed_rf_frequency_hz
                        .or(Some(p.metadata.frequency))
                        .and_then(|rf| {
                            Some(crate::utils::frequency_to_band(rf))
                                .filter(|b| !b.ends_with("MHZ"))
                        })
                        .map(|b| b.to_uppercase())
                        == band)
        }))
    }

    async fn add_callsign_mapping(&self, callsign: &str, qso_id: QsoId) {
        // UX audit Batch 2 #7: key the callsign map case-insensitively
        // (uppercase). A case/format mismatch between a DX-Hunter call and a
        // Callers reply for the same station would otherwise defeat supersede
        // and re-spawn a duplicate QSO. Callsigns are conventionally uppercase;
        // normalising here (and at every lookup) makes supersede robust.
        let key = callsign.to_uppercase();
        let mut qsos_by_callsign = self.qsos_by_callsign.write().await;
        qsos_by_callsign
            .entry(key)
            .or_insert_with(Vec::new)
            .push(qso_id);
    }

    /// FIX 1: return the id of an ACTIVE (non-terminal), MANUAL-initiated QSO
    /// with `callsign` on the same band as `frequency`, if one exists. Used by
    /// the operator re-call paths so mashing Call/Space on a station already in
    /// progress CONTINUES the one QSO rather than superseding it / spawning a
    /// duplicate. Case-insensitive callsign match (matching the callsign-map
    /// keying); "same band" derived via [`crate::utils::frequency_to_band`]
    /// exactly like [`Self::supersede_active_qsos_for`]. When several match
    /// (shouldn't happen post-FIX-1, but be robust), the most-recently-started
    /// one wins.
    async fn find_active_manual_qso_for(&self, callsign: &str, frequency: f64) -> Option<QsoId> {
        let want_band = crate::utils::frequency_to_band(frequency);
        let key = callsign.to_uppercase();
        let ids = self.qsos_by_callsign.read().await.get(&key).cloned()?;
        let qsos = self.qsos.read().await;
        ids.into_iter()
            .filter_map(|id| {
                qsos.get(&id).and_then(|p| {
                    let matches = p.state.is_active()
                        && p.metadata.initiated_by == CallInitiation::Manual
                        && crate::utils::frequency_to_band(p.metadata.frequency) == want_band;
                    matches.then_some((id, p.metadata.start_time))
                })
            })
            .max_by_key(|(_, started)| *started)
            .map(|(id, _)| id)
    }

    /// FIX B: return the id of the most-recently-COMPLETED, MANUAL-initiated
    /// QSO with `callsign` on the same band as `frequency`, if it completed
    /// within `within` of now. Used by [`Self::respond_to_caller`] so a
    /// close-step (`Rr73`/`SeventyThree`) reply for a station we JUST finished
    /// working — e.g. the operator mashing "send 73" after already
    /// completing — resolves to the SAME QSO object (re-key its last frame)
    /// instead of spawning a sibling with a brand-new `QsoId` (the double-73
    /// duplicate-ADIF-entry bug). Same `qsos_by_callsign` + band-match
    /// approach as [`Self::find_active_manual_qso_for`]; when several
    /// completed matches exist, the newest (`completed_at`) wins.
    ///
    /// Delegates to [`Self::find_recently_completed_manual_qso_for_at`] with
    /// the real clock; see that function for the testable, explicit-`now`
    /// variant (mirroring [`Self::check_timeouts_at`]'s pattern) — a 45s
    /// real-time grace window is awkward to exercise with `tokio::time::sleep`
    /// in a unit test.
    async fn find_recently_completed_manual_qso_for(
        &self,
        callsign: &str,
        frequency: f64,
        within: chrono::Duration,
    ) -> Option<QsoId> {
        self.find_recently_completed_manual_qso_for_at(callsign, frequency, within, self.now_utc())
            .await
    }

    /// Explicit-`now` variant of [`Self::find_recently_completed_manual_qso_for`]
    /// (for testability — see its doc comment).
    ///
    /// PAN-25 (Codex round-1 review): `frequency` here is an audio offset,
    /// but the COMPLETED QSOs this scans against now have their
    /// `metadata.frequency` stamped with the true RF frequency (dial +
    /// offset) at completion time — `frequency` must be adjusted the same
    /// way for the band comparison below to mean anything.
    async fn find_recently_completed_manual_qso_for_at(
        &self,
        callsign: &str,
        frequency: f64,
        within: chrono::Duration,
        now: DateTime<Utc>,
    ) -> Option<QsoId> {
        let want_band = crate::utils::frequency_to_band(self.current_rf_frequency(frequency));
        let key = callsign.to_uppercase();
        let ids = self.qsos_by_callsign.read().await.get(&key).cloned()?;
        let qsos = self.qsos.read().await;
        ids.into_iter()
            .filter_map(|id| {
                qsos.get(&id).and_then(|p| match &p.state {
                    QsoState::Completed { completed_at, .. }
                        if p.metadata.initiated_by == CallInitiation::Manual
                            && Self::completed_band(p) == want_band
                            && now - *completed_at <= within =>
                    {
                        Some((id, *completed_at))
                    }
                    _ => None,
                })
            })
            .max_by_key(|(_, completed_at)| *completed_at)
            .map(|(id, _)| id)
    }

    /// FIX 1: forward position of a [`ResponseStep`] on the responder's FT8 QSO
    /// ladder, aligned with [`Self::ladder_rank`] so the two are directly
    /// comparable. `Grid` → 0 (RespondingToCq), `Report`/`ReportAck` → 1
    /// (SendingReport), `Rr73` → 2 (WaitingForConfirmation), `SeventyThree` → 3
    /// (Completed). Used to decide whether a context reply is AHEAD of an
    /// existing QSO's current stage (→ advance) or at/behind it (→ re-send).
    fn step_ladder_rank(step: pancetta_core::ResponseStep) -> Option<u8> {
        use pancetta_core::ResponseStep;
        Some(match step {
            ResponseStep::Grid => 0,
            ResponseStep::Report | ResponseStep::ReportAck => 1,
            ResponseStep::Rr73 => 2,
            ResponseStep::SeventyThree => 3,
        })
    }

    /// FIX 1: advance an EXISTING manual QSO to the state/outbound implied by
    /// `step`, instead of creating a new QSO. Mirrors the (state, message)
    /// mapping in [`Self::respond_to_caller`] but mutates the existing QSO in
    /// place: sets its state, records the outbound as a `Sent` message, emits
    /// the `MessageToSend`, and — when advancing to `SeventyThree` — stamps the
    /// completion metadata and emits `QsoCompleted` so the contact is logged.
    /// The QSO's latched `tx_parity` and `initiated_by` are preserved (we reuse
    /// what was latched at QSO start). Used when an operator context-replies at
    /// a step ahead of where the QSO currently is.
    #[allow(clippy::too_many_arguments)]
    async fn advance_existing_qso_to_step(
        &self,
        qso_id: QsoId,
        target: &str,
        frequency: f64,
        step: pancetta_core::ResponseStep,
        our_report: SignalReport,
        their_report_val: SignalReport,
        remote_origin: bool,
        remote_client_key_id: Option<String>,
    ) -> Result<ManualCallOutcome, QsoManagerError> {
        use pancetta_core::ResponseStep;
        let now = self.now_utc();

        let (new_state, message): (QsoState, MessageType) = match step {
            ResponseStep::Grid => {
                // Grid never ranks ahead of an active QSO, so the caller never
                // routes here; re-send current as a safe fallback.
                return Ok(self
                    .resend_existing_if_permitted(qso_id, remote_origin, remote_client_key_id)
                    .await);
            }
            ResponseStep::Report => (
                QsoState::SendingReport {
                    their_callsign: target.to_string(),
                    their_report: None,
                    our_report,
                    frequency,
                    started_at: now,
                },
                MessageType::SignalReport {
                    to_station: target.to_string(),
                    from_station: self.config.our_callsign.clone(),
                    report: our_report,
                },
            ),
            ResponseStep::ReportAck => (
                QsoState::SendingReport {
                    their_callsign: target.to_string(),
                    their_report: Some(their_report_val),
                    our_report,
                    frequency,
                    started_at: now,
                },
                MessageType::ReportAck {
                    to_station: target.to_string(),
                    from_station: self.config.our_callsign.clone(),
                    report: our_report,
                },
            ),
            ResponseStep::Rr73 => (
                QsoState::WaitingForConfirmation {
                    their_callsign: target.to_string(),
                    their_report: their_report_val,
                    our_report,
                    frequency,
                    grid_square: None,
                    started_at: now,
                },
                MessageType::FinalConfirmation {
                    to_station: target.to_string(),
                    from_station: self.config.our_callsign.clone(),
                },
            ),
            ResponseStep::SeventyThree => (
                QsoState::Completed {
                    their_callsign: target.to_string(),
                    their_report: their_report_val,
                    our_report,
                    frequency,
                    grid_square: None,
                    completed_at: now,
                    duration_seconds: 0,
                },
                MessageType::SeventyThree {
                    to_station: target.to_string(),
                    from_station: self.config.our_callsign.clone(),
                },
            ),
        };

        let is_completed = matches!(new_state, QsoState::Completed { .. });
        let raw_text = self.render_sent_text(&message);

        // Mutate the existing QSO under the write lock, capturing what we need
        // for the emits after the lock is released.
        let emit = {
            let mut qsos = self.qsos.write().await;
            let Some(progress) = qsos.get_mut(&qso_id) else {
                return Err(QsoManagerError::QsoNotFound { qso_id });
            };
            // PAN-138 / round 13: check, rebind and advance under this one
            // guard, so no other `qsos` holder (QSO processing, a PAN-137
            // retire) runs between the check and the mutation. The check is
            // still a point-in-time read of `ArmState`, not a TX gate: a
            // disarm or expiry after it is caught only by the TX worker at
            // key time. A non-completing advance is not undone then (advances
            // record no PAN-75 attempt); a completing one holds its
            // QsoCompleted until the 73 airs and reverts if it does not
            // (PAN-280).
            if remote_origin && !(self.remote_tx_permitted)(remote_client_key_id.as_deref()) {
                return Ok(ManualCallOutcome::NotPermitted);
            }
            progress.metadata.remote_origin = remote_origin;
            progress.metadata.remote_client_key_id = remote_client_key_id;
            let old_state = progress.state.clone();
            // PAN-280: what a dropped 73 puts back.
            let pre_close_reports = progress.metadata.reports.clone();
            let pre_close_end_time = progress.metadata.end_time;
            let pre_close_rf = progress.metadata.completed_rf_frequency_hz;
            let closing_text = raw_text.clone();
            progress.state = new_state.clone();
            progress.state_history.push(StateTransition {
                from_state: old_state.clone(),
                to_state: new_state.clone(),
                timestamp: now,
                reason: TransitionReason::UserAction,
            });
            progress.messages.push(QsoMessage {
                timestamp: now,
                direction: MessageDirection::Sent,
                message_type: message.clone(),
                raw_text,
                signal_strength: None,
                frequency,
            });
            let tx_parity = progress.metadata.tx_parity;
            let remote_origin = progress.metadata.remote_origin;
            let remote_client_key_id = progress.metadata.remote_client_key_id.clone();

            // On completion, stamp reports/end-time and prepare the completed
            // metadata (with the real RF frequency = dial + offset) to log.
            // PAN-25 round 2: stamp `completed_rf_frequency_hz` (for the
            // suppression band-check) and the LOCAL CLONE's `frequency` (for
            // the emitted event) -- never the stored `progress.metadata.frequency`
            // itself, which must stay the audio offset for its whole
            // lifetime; see the matching comment in `process_message_for_qso`.
            let completed_metadata = if is_completed {
                progress.metadata.reports = SignalReports {
                    sent: Some(our_report),
                    received: Some(their_report_val),
                };
                progress.metadata.end_time = Some(now);
                let rx_dial = self.dial_frequency_hz.load(Ordering::Relaxed);
                let split = self.split_tx_frequency_hz.load(Ordering::Relaxed);
                let dial = effective_tx_dial(rx_dial, split);
                let rf_frequency = (dial > 0).then_some(progress.metadata.frequency + dial as f64);
                progress.metadata.completed_rf_frequency_hz = rf_frequency;
                let mut m = progress.metadata.clone();
                if let Some(rf) = rf_frequency {
                    m.frequency = rf;
                }
                Some(m)
            } else {
                None
            };
            // Layer 2 timeline persistence: capture the timeline as of this
            // mutation while the write lock is still held, for the held
            // QsoCompleted.
            let state_history = progress.state_history.clone();
            let messages = progress.messages.clone();
            let attempt_id = self.next_attempt_id();
            // PAN-280: a completing advance holds its QsoCompleted until the
            // 73 airs, stored under this guard and before the MessageToSend.
            if let Some(metadata) = completed_metadata {
                self.hold_completion(
                    qso_id,
                    PendingCompletion {
                        attempt_ids: vec![attempt_id],
                        raw_text: closing_text.clone(),
                        frequency_hz: frequency,
                        remote_origin,
                        remote_client_key_id: remote_client_key_id.clone(),
                        queued_at: now,
                        completed_state: new_state.clone(),
                        restore: CompletionRestore::Revert {
                            state: old_state.clone(),
                            reports: pre_close_reports,
                            end_time: pre_close_end_time,
                            completed_rf_frequency_hz: pre_close_rf,
                            sent_record: Some((now, closing_text)),
                        },
                        metadata,
                        state_history,
                        messages,
                    },
                );
            }
            (
                old_state,
                tx_parity,
                remote_origin,
                remote_client_key_id,
                attempt_id,
            )
        };
        let (old_state, tx_parity, remote_origin, remote_client_key_id, attempt_id) = emit;

        self.emit_state_change(qso_id, old_state, new_state).await;
        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
            remote_origin,
            remote_client_key_id,
            attempt_id: Some(attempt_id),
        })
        .await;
        Ok(ManualCallOutcome::Advanced(attempt_id))
    }

    /// FIX 3: retire every active (non-terminal) QSO with `callsign` on the
    /// same band as `frequency`, marking each `Failed{Superseded}` and
    /// clearing its callsign mapping. Emits a `StateChanged` per superseded
    /// QSO (terminal Failed → AP/snapshot clears in the coordinator). Called
    /// just before a new manual QSO is created so only the most-recent one
    /// remains active.
    ///
    /// "Same band" is derived from the QSO frequency via
    /// [`crate::utils::frequency_to_band`]. Within a single operating session
    /// every active QSO shares the RF band, so in practice this collapses to
    /// "same callsign"; deriving the band keeps the rule correct should
    /// per-QSO RF frequencies ever be threaded through.
    async fn supersede_active_qsos_for(&self, callsign: &str, frequency: f64) {
        let new_band = crate::utils::frequency_to_band(frequency);
        // Look up case-insensitively (Batch 2 #7) so a case/format mismatch
        // can't leak a duplicate active QSO past supersede.
        let key = callsign.to_uppercase();
        let superseded = {
            let mut qsos = self.qsos.write().await;
            let candidate_ids = self
                .qsos_by_callsign
                .read()
                .await
                .get(&key)
                .cloned()
                .unwrap_or_default();
            Self::supersede_active_locked(&mut qsos, &candidate_ids, &new_band, self.now_utc())
        };
        self.emit_superseded(callsign, &new_band, superseded).await;
    }

    /// PAN-280: the mutating core of [`Self::supersede_active_qsos_for`],
    /// run under a caller's `qsos` write guard so a new QSO's permission
    /// check, supersede and insert share one guard. Marks every active
    /// candidate on `new_band` `Failed{Superseded}` and returns what
    /// [`Self::emit_superseded`] announces once the guard drops.
    fn supersede_active_locked(
        qsos: &mut HashMap<QsoId, QsoProgress>,
        candidate_ids: &[QsoId],
        new_band: &str,
        now: DateTime<Utc>,
    ) -> Vec<SupersededQso> {
        let mut superseded = Vec::new();
        for qso_id in candidate_ids {
            let Some(progress) = qsos.get_mut(qso_id) else {
                continue;
            };
            if !(progress.state.is_active()
                && crate::utils::frequency_to_band(progress.metadata.frequency) == *new_band)
            {
                continue;
            }
            let old_state = progress.state.clone();
            progress.state = QsoState::Failed {
                reason: QsoFailureReason::Superseded,
                failed_at: now,
                last_state: Box::new(old_state.clone()),
            };
            // Capture metadata (Batch 4, SM-F5 — needed for QsoFailed) and
            // the timeline (Layer 2 persistence) — superseded QSOs stay in
            // the map and are picked up later by `cleanup_completed_qsos`,
            // but emitting it here too means a persistence subscriber
            // doesn't have to wait up to an hour for it.
            superseded.push(SupersededQso {
                qso_id: *qso_id,
                old_state,
                new_state: progress.state.clone(),
                metadata: progress.metadata.clone(),
                state_history: progress.state_history.clone(),
                messages: progress.messages.clone(),
            });
        }
        superseded
    }

    /// PAN-280: announce what [`Self::supersede_active_locked`] retired, in
    /// the order the supersede always used: `StateChanged`, `QsoFailed`,
    /// then the callsign mapping removal.
    async fn emit_superseded(
        &self,
        callsign: &str,
        new_band: &str,
        superseded: Vec<SupersededQso>,
    ) {
        for s in superseded {
            self.emit_state_change(s.qso_id, s.old_state.clone(), s.new_state)
                .await;
            // SM-F5: also emit QsoFailed alongside StateChanged — see the
            // cancel_qso comment for why (dead priority-scoring backoff
            // consumer; idempotent HashSet::remove on the coordinator
            // side makes emitting both events safe).
            self.emit_event(QsoEvent::QsoFailed {
                qso_id: s.qso_id,
                reason: QsoFailureReason::Superseded,
                last_state: s.old_state,
                metadata: s.metadata,
                state_history: s.state_history,
                messages: s.messages,
            })
            .await;
            self.remove_callsign_mapping(callsign, s.qso_id).await;
            info!(
                "Superseded older active QSO {} with {} on band {} (re-call)",
                s.qso_id, callsign, new_band
            );
        }
    }

    async fn remove_callsign_mapping(&self, callsign: &str, qso_id: QsoId) {
        // Match the uppercase keying of `add_callsign_mapping` (Batch 2 #7).
        let key = callsign.to_uppercase();
        let mut qsos_by_callsign = self.qsos_by_callsign.write().await;
        if let Some(qso_ids) = qsos_by_callsign.get_mut(&key) {
            qso_ids.retain(|&id| id != qso_id);
            if qso_ids.is_empty() {
                qsos_by_callsign.remove(&key);
            }
        }
    }

    async fn emit_event(&self, event: QsoEvent) {
        // 2026-07-18 operator finding: PTT keyed twice for the same QSO's
        // final 73 (confirmed via YO6BHN and UT7UJ — real rig set_ptt ON
        // events, one FT8 slot [~15s] apart, for the identical message and
        // qso_id), from what should be a single send. Every send site
        // upstream of the coordinator's TX worker has been individually
        // read and ruled out as the origin: the 4 places that construct a
        // `TransmitRequest`, `MessageBus::send_message` (plain point-to-
        // point, no retry/fan-out), `coalesce_backlog_into`'s drain/re-
        // enqueue (only re-enqueues the non-TX message that stops the
        // drain, never a `TransmitRequest`), `rearm_manual_calls_at`
        // (its state match excludes `Completed`), `maybe_auto_resend_73`
        // (needs a fresh directed RR73/RRR decode — none found), and the
        // autonomous engine's own TX path (its runtime gate is seeded
        // from `[autonomous].enabled` and stays closed when that's
        // false). This traces every `MessageToSend` at its true source —
        // inside the state machine itself, the one place not yet ruled
        // out — so the next occurrence shows definitively whether this
        // fires twice for one logical send (root cause is here or
        // upstream of here) or once (root cause is downstream, in event
        // delivery/consumption not yet found). Log-only; no behavior
        // change. Remove once root-caused.
        if let QsoEvent::MessageToSend {
            qso_id,
            ref message,
            ..
        } = event
        {
            info!(
                target: "pancetta::qso.send_diag",
                "emit_event(MessageToSend): qso={} message={:?}",
                qso_id, message
            );
        }
        if let Err(e) = self.event_sender.send(event) {
            warn!("Failed to emit QSO event: {}", e);
        }
    }

    async fn emit_state_change(&self, qso_id: QsoId, old_state: QsoState, new_state: QsoState) {
        self.emit_event(QsoEvent::StateChanged {
            qso_id,
            old_state,
            new_state,
            timestamp: self.now_utc(),
        })
        .await;
    }

    async fn cleanup_loop(&self) {
        loop {
            // Check if we should continue
            {
                let interval_guard = self.cleanup_interval.read().await;
                if interval_guard.is_none() {
                    break;
                }
            }

            // Wait for next tick
            {
                let mut interval_guard = self.cleanup_interval.write().await;
                if let Some(ref mut interval_timer) = *interval_guard {
                    interval_timer.tick().await;
                } else {
                    break;
                }
            }

            // Perform cleanup
            self.cleanup_completed_qsos().await;
        }
    }

    async fn timeout_check_loop(&self) {
        let mut detector = pancetta_core::slot_clock::ClockStepDetector::new(self.now_utc());
        let mut last_mono = std::time::Instant::now();
        let mut interval_timer = interval(TokioDuration::from_secs(5));
        loop {
            interval_timer.tick().await;
            self.watchdog_pass_at(
                self.now_utc(),
                std::time::Instant::now(),
                &mut detector,
                &mut last_mono,
            )
            .await;
        }
    }

    /// For every manual-initiated QSO still in `CallingCq`, `WaitingForReport`,
    /// `RespondingToCq`, or `SendingReport` (waiting for the DX to come
    /// back / copy our last frame), re-emit that frame at most once per FT8
    /// slot so the operator keeps calling every slot until the DX answers or
    /// the manual watchdog fires. The TX scheduler downstream resolves slot
    /// parity from the `tx_parity` latched on the QSO, so re-emitting more
    /// often than a slot is harmless, but we gate to ~one per slot to avoid
    /// flooding the bus.
    ///
    /// SM-F6: autonomous (`CallInitiation::Auto`) QSOs in `RespondingToCq` or
    /// `SendingReport` are ALSO re-armed here now, but under a small,
    /// hardcoded cap (`AUTO_RESEND_MAX_CALLS`) distinct from Manual's
    /// operator-supervised `manual_call_max_calls` — see the constant's doc
    /// comment for the safety rationale. `CallingCq` and `WaitingForReport`
    /// remain Manual-only (an Auto QSO doesn't reach those rearm-eligible
    /// states via a self-initiated CQ opening; extending that is a separate,
    /// out-of-scope initiative).
    ///
    /// Re-arming increments `call_count` and updates `last_call_at`; the
    /// watchdog ([`Self::check_timeouts_at`]) reads `call_count` and
    /// `first_call_at` (Manual) or the per-state `report_timeout` (Auto) to
    /// decide when to stop. The stall streak (`stall_cycles`) counts once
    /// per own-parity TX slot these re-sends feed, not once per emission
    /// (PAN-128).
    /// Failed TX outcomes can reverse accounting via [`Self::roll_back_unsent_attempt_at`].
    /// The ledger emission anchor preserves cadence even when accounting is restored.
    pub async fn rearm_manual_calls_at(&self, now: DateTime<Utc>) {
        // Re-arm only when at least one transmit slot has elapsed since the
        // last call, to keep ~one call per slot.
        //
        // PAN-72 (Codex round 7 on PR #350, finding 5): the slot length is the
        // ACTIVE MODE's, not a hardcoded 15 s. FT4 (7.5 s) and FT2 (3.2 s)
        // previously advanced this gate — and with it the `stall_cycles`
        // counter below, which the documented threshold measures in *slots* —
        // only every other (or every fifth) real slot.
        //
        // Round-7 re-review: read the coordinator's LIVE `active_slot_ns`
        // atomic, never the cloned `config.active_mode` string. This loop runs
        // on a `start()`-time clone, and `Clone` copies `config` by value, so
        // a config-derived cadence silently missed every runtime Shift+M
        // switch in both directions. With no injected source the atomic holds
        // `DEFAULT_SLOT_NS`, so mode=FT8 stays byte-identical; see
        // [`QsoManager::set_active_slot_ns_source`].
        let slot_millis = self.rearm_slot_millis();

        // SM-F6: bounded resend cap for AUTONOMOUS QSOs -- see the
        // module-level [`AUTO_RESEND_MAX_CALLS`] const's doc comment (hoisted
        // there in PAN-72 round 14 so `apply_tx_offset_switch` can reference
        // the SAME value).

        // Each entry carries the exact MessageType to re-emit so a
        // RespondingToCq QSO re-sends the call (CqResponse) while a
        // SendingReport QSO re-sends our R-report (ReportAck) — FIX 4.
        type RecallEntry = (
            QsoId,
            MessageType,
            f64,
            Option<pancetta_core::slot::SlotParity>,
            bool,
            Option<String>,
            TxAttemptId,
        );
        let mut to_recall: Vec<RecallEntry> = Vec::new();

        // PAN-72: TX-offset actions (Switch/Revert) a stall-tripped QSO
        // needs, collected here and emitted after the write lock below is
        // dropped — same reason `to_recall` above is collect-then-emit.
        let mut offset_actions_to_emit: Vec<(QsoId, OffsetAction, u32)> = Vec::new();

        // PAN-72 (Codex round 2 on PR #350, finding 5): read the global TX
        // policy ONCE for this pass. Under `Disabled` the coordinator's
        // `tx_hard_mute_reason` blocks every frame this loop re-emits, so
        // nothing goes on the air and the DX's continued silence is not
        // evidence of anything. See the `tx_policy` field's doc comment.
        let tx_muted = !pancetta_core::TxPolicy::from_u8(
            self.tx_policy.load(std::sync::atomic::Ordering::Relaxed),
        )
        .allows_any_tx();

        // PAN-72 (Codex round 7 on PR #350, finding 3): the SECOND, entirely
        // independent gate a rearmed frame must clear — the station-agent
        // armed-TX check the TX worker runs immediately before PTT for every
        // `TxOrigin::Remote` request. `TxPolicy` staying `Full` says nothing
        // about it: an arm expiry or an explicit remote disarm drops every
        // frame a `remote_origin` QSO rearms while the policy above still
        // reads "TX allowed". Read ONCE per pass, like `tx_policy`, and
        // consulted below only for QSOs that are actually remote-origin. See
        // the `remote_tx_permitted` field's doc comment — read-only evidence,
        // never a TX gate. PAN-91 review follow-up: evaluated PER-QSO below
        // (not once per pass) since it now takes the QSO's own bound client
        // identity — a QSO bound to client A must not read as reached-the-air
        // just because client B happens to be armed.
        let remote_tx_permitted = &self.remote_tx_permitted;

        // PAN-72 Fix E (Codex round 10, thread on `qso_manager.rs:6335`): the
        // THIRD independent "did this frame actually reach the air" check —
        // the TX worker's own pre-PTT Hamlib hard-mute (restart, command loop
        // not ready, an undelivered pending frequency/split command, or an
        // in-flight Hamlib command). Read ONCE per pass, like `tx_policy` and
        // `remote_tx_permitted` above. See the `hamlib_hard_muted` field's
        // doc comment.
        let hamlib_hard_muted = (self.hamlib_hard_muted)();

        {
            let mut qsos = self.qsos.write().await;
            let mut ledger = self
                .keep_call_ledger
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let cutoff = now - Duration::milliseconds(UNSENT_ATTEMPT_RETENTION_SLOTS * slot_millis);
            for entry in ledger.values_mut() {
                entry
                    .attempts
                    .retain(|attempt| attempt.emitted_at >= cutoff);
            }
            for (&qso_id, progress) in qsos.iter_mut() {
                let is_manual = progress.metadata.initiated_by == CallInitiation::Manual;
                let message = match &progress.state {
                    // Manual CQ (operator `c`): keep calling CQ every slot
                    // until a station answers (→ WaitingForReport, handled by
                    // the normal sequence) or the watchdog retires us. Manual
                    // only — SM-F6 deliberately does NOT extend autonomous
                    // keep-calling to a self-CQ opening; that's a separate,
                    // out-of-scope initiative (Auto QSOs don't currently
                    // keep-call a CQ this way at all).
                    QsoState::CallingCq { .. } if is_manual => MessageType::Cq {
                        callsign: self.config.our_callsign.clone(),
                        grid: self.config.our_grid.clone(),
                    },
                    // SM-F4: the CQer's report was sent on the CallingCq →
                    // WaitingForReport transition; if the caller never copied
                    // it and repeats their grid, re-send the SAME latched
                    // value (no SNR jitter — see the (WaitingForReport,
                    // CqResponse) regression arm's doc comment). Manual only,
                    // for the same reason CallingCq above is Manual only: an
                    // Auto QSO never reaches WaitingForReport via a
                    // rearm-eligible CQ opening.
                    QsoState::WaitingForReport {
                        their_callsign,
                        our_report,
                        ..
                    } if is_manual => MessageType::SignalReport {
                        to_station: their_callsign.clone(),
                        from_station: self.config.our_callsign.clone(),
                        report: *our_report,
                    },
                    // SM-F6: re-send our call/grid every slot while waiting
                    // for the DX to come back. Now covers BOTH Manual
                    // (keep-calling, long watchdog) and Auto (a single
                    // bounded resend, capped below) — an autonomous pounce
                    // whose CqResponse the DX missed previously got ZERO
                    // re-sends and died silently at the 30s report_timeout.
                    QsoState::RespondingToCq {
                        target_callsign, ..
                    } => MessageType::CqResponse {
                        calling_station: target_callsign.clone(),
                        responding_station: self.config.our_callsign.clone(),
                        grid: self.config.our_grid.clone(),
                    },
                    // SendingReport is entered two ways with DIFFERENT last-sent
                    // frames, so the rearm must re-send the SAME rung we're
                    // actually at, not always escalate to R-report:
                    //   - their_report == None: we're at the plain-report rung
                    //     (entered via the stuck-at-grid arm — RespondingToCq +
                    //     CqResponse — or a manual Report-step answer). We sent
                    //     a plain SignalReport (-NN) and have NOT yet heard the
                    //     DX's report, so keep-calling MUST re-send -NN, never
                    //     R-NN — escalating here was the KJ5NJF bug: we'd
                    //     advance our own TX to R-report with zero DX input.
                    //   - their_report == Some: FIX 4 — we sent R and the DX
                    //     re-sent their report (they did not copy our R) —
                    //     re-send our R-report each slot, under the SAME
                    //     watchdog, until the DX advances (RR73) or the
                    //     watchdog retires us.
                    // SM-F6: also covers Auto (a single bounded resend,
                    // capped below) — the second of the two Auto-eligible
                    // states (RespondingToCq, SendingReport) per this fix.
                    QsoState::SendingReport {
                        their_callsign,
                        their_report: None,
                        our_report,
                        ..
                    } => MessageType::SignalReport {
                        to_station: their_callsign.clone(),
                        from_station: self.config.our_callsign.clone(),
                        report: *our_report,
                    },
                    QsoState::SendingReport {
                        their_callsign,
                        their_report: Some(_),
                        our_report,
                        ..
                    } => MessageType::ReportAck {
                        to_station: their_callsign.clone(),
                        from_station: self.config.our_callsign.clone(),
                        report: *our_report,
                    },
                    // Any later state: the normal sequence drives the rest.
                    _ => continue,
                };

                // Stop re-arming once the watchdog bound is reached; the
                // watchdog itself will retire the QSO on its own pass.
                // Manual gets the long, operator-supervised bound; Auto gets
                // the small, hardcoded, strictly-bounded cap above (SM-F6).
                let max_calls = if is_manual {
                    self.config.timeouts.manual_call_max_calls
                } else {
                    AUTO_RESEND_MAX_CALLS
                };
                if progress.metadata.call_count >= max_calls {
                    continue;
                }

                // Millisecond resolution so a 7.5 s FT4 slot is expressible;
                // for FT8's whole-second 15_000 ms this is exactly the
                // previous `num_seconds() < 15` test (both truncate toward
                // zero, and the threshold is a whole number of seconds).
                let elapsed_since_last = progress
                    .metadata
                    .last_call_at
                    .max(ledger.get(&qso_id).and_then(|entry| entry.last_emitted_at))
                    .map(|t| (now - t).num_milliseconds())
                    .unwrap_or(i64::MAX);
                if elapsed_since_last < slot_millis {
                    continue;
                }

                // Round-3 review (Codex P2): decide whether this frame will
                // actually reach the air BEFORE touching any attempt
                // bookkeeping (`call_count`/`last_call_at`), not after. The
                // manual-call budget (`max_calls` above) and the on-air
                // timeline (`progress.messages`, below) both exist to bound
                // and record REAL transmission attempts — spending either on
                // a frame the TX-layer arm gate will deny anyway (a
                // `remote_origin` QSO left bound to a client that is no
                // longer the one armed) silently exhausts
                // `manual_call_max_calls` without ever having transmitted,
                // and records a `Sent` timeline entry for something that
                // never went out. THREE independent gates can swallow a
                // frame, and it has to clear ALL of them before an attempt
                // is real:
                //   - the global TX policy hard mute (round 2, finding 5);
                //   - for a `remote_origin` QSO only, the station-agent
                //     armed-TX gate the TX worker applies to every
                //     `TxOrigin::Remote` frame (round 7, finding 3). An arm
                //     expiry, explicit disarm, or a rebind to a different
                //     client leaves `TxPolicy` untouched, so the first check
                //     alone cannot see it;
                //   - the TX worker's own pre-PTT Hamlib hard mute (round 10,
                //     Fix E) — a Hamlib restart, the command loop not ready,
                //     an undelivered pending frequency/split command, or an
                //     in-flight Hamlib command. Neither of the first two
                //     checks can see this either.
                let frame_reaches_the_air = !tx_muted
                    && !hamlib_hard_muted
                    && (!progress.metadata.remote_origin
                        || remote_tx_permitted(progress.metadata.remote_client_key_id.as_deref()));
                if !frame_reaches_the_air {
                    continue;
                }

                let prev_last_call_at = progress.metadata.last_call_at;
                progress.metadata.call_count += 1;
                progress.metadata.last_call_at = Some(now);

                // PAN-72: this real per-slot re-send IS the silence signal —
                // the DX did not advance the QSO since the last slot, so we
                // count it against the stall streak. A forward advance
                // (`process_message_for_qso`) resets this to 0 elsewhere;
                // this is the sole increment site (see
                // `QsoMetadata::stall_cycles`'s doc comment). Reached only
                // when `frame_reaches_the_air` (above), so the existing
                // count-intact-on-mute behavior is preserved by construction.
                //
                // PAN-128: the QSO only transmits on its own parity — every
                // OTHER slot — so an emission counts once per own-parity TX
                // slot it feeds, not once per emission. The first emission
                // feeding a slot no surviving attempt has claimed holds the
                // slot's count (`bumped_stall`); its same-slot successors
                // keep pre-arming the TX worker without inflating the
                // streak, so `qso_stall_switch_after` measures real
                // unanswered TX opportunities. The slot an emission feeds is
                // derived with the TX scheduler's current-slot acceptance
                // rule (see `keep_call_tx_slot`), so an in-window rearm and
                // the off-parity rearm before it — both scheduled into the
                // same real TX slot — share one key (PR review follow-up).
                let tx_slot = keep_call_tx_slot(
                    now,
                    slot_millis,
                    self.tx_late_max_ms
                        .load(std::sync::atomic::Ordering::Acquire),
                    progress.metadata.tx_parity,
                );
                let counts_toward_stall = !ledger.get(&qso_id).is_some_and(|entry| {
                    entry
                        .attempts
                        .iter()
                        .any(|a| a.bumped_stall && a.tx_slot == Some(tx_slot))
                });
                if counts_toward_stall {
                    progress.metadata.stall_cycles =
                        progress.metadata.stall_cycles.saturating_add(1);
                }

                let tx_auto = pancetta_core::TxFreqMode::from_u8(
                    self.tx_freq_mode.load(std::sync::atomic::Ordering::Relaxed),
                )
                .allows_auto_change();

                let mut arm_stall_trigger = false;
                if tx_auto
                    && progress.metadata.stall_cycles >= self.config.timeouts.qso_stall_switch_after
                {
                    let current = progress.metadata.frequency;
                    let action = match progress.metadata.last_known_good_offset_hz {
                        Some(known_good) if (current - known_good).abs() >= f64::EPSILON => {
                            OffsetAction::Revert {
                                target_hz: known_good,
                            }
                        }
                        _ => OffsetAction::Switch { avoid_hz: current },
                    };
                    // PAN-72 (Codex round 7 on PR #350, finding 2): the streak
                    // is deliberately NOT cleared here. Raising an action is
                    // not relocating: the coordinator drains this mailbox once
                    // per slot and `apply_tx_offset_switch` can still refuse
                    // the result — most importantly with `OffsetActionNoOp`
                    // when exclusions left the allocator no valid candidate on
                    // a crowded band. Clearing at emission time threw the
                    // evidence away before anything could act on it, so PAN-79's
                    // commit-side refusal preserved a streak that had in fact
                    // already been reset, and recovery was pushed out by
                    // another full `qso_stall_switch_after` window even if a
                    // slot opened up immediately.
                    //
                    // The reset now happens in exactly one place — the
                    // successful commit in `apply_tx_offset_switch` (and, as
                    // before, a genuine DX advance in `process_message_for_qso`).
                    // Until one of those lands, the QSO stays at/above the
                    // threshold and re-raises the action once per slot; the
                    // coordinator's drain coalesces per QSO per batch (round 7,
                    // finding 1), so this costs one retry per slot, not a
                    // pile-up.
                    let entry = ledger.entry(qso_id).or_default();
                    entry.stall_action_revoked = false;
                    // PAN-182: the commit waits on the frame that brought the
                    // streak to the threshold (recorded below). A re-raise on
                    // a later slot leaves that hold alone — its own
                    // rollback cannot revoke the action, and re-arming the
                    // hold every slot could keep it from ever releasing — and
                    // only takes over an expired one.
                    arm_stall_trigger = (counts_toward_stall
                        && progress.metadata.stall_cycles
                            == self.config.timeouts.qso_stall_switch_after)
                        || entry.pending_stall_trigger.as_ref().is_some_and(|trigger| {
                            stall_trigger_hold_expired(trigger.emitted_at, now, slot_millis)
                        });
                    offset_actions_to_emit.push((
                        qso_id,
                        action,
                        // PAN-72 finding 8: the staleness token, captured
                        // here under the same write lock that raised the
                        // action so it can never disagree with it.
                        progress.metadata.advance_generation,
                    ));
                }

                // Record the re-emitted call as a Sent message so the TUI's
                // last-TX line and activity counter advance during keep-calling
                // (UX audit Batch 2 — the panel previously froze because rearm
                // appended nothing, making keep-calling look like a hang). The
                // raw_text is the rendered FT8 text we put on the air.
                let raw_text = self.render_sent_text(&message);
                progress.messages.push(QsoMessage {
                    timestamp: now,
                    direction: MessageDirection::Sent,
                    message_type: message.clone(),
                    raw_text: raw_text.clone(),
                    signal_strength: None,
                    frequency: progress.metadata.frequency,
                });

                let attempt_id = self.next_attempt_id();
                let attempt = KeepCallAttempt {
                    attempt_id,
                    emitted_at: now,
                    raw_text,
                    prev_last_call_at,
                    bumped_stall: counts_toward_stall,
                    tx_slot: Some(tx_slot),
                    recorded_sent: true,
                    frequency_hz: progress.metadata.frequency,
                    remote_origin: progress.metadata.remote_origin,
                    remote_client_key_id: progress.metadata.remote_client_key_id.clone(),
                };
                if arm_stall_trigger {
                    ledger.entry(qso_id).or_default().pending_stall_trigger =
                        Some(PendingStallTrigger::from_attempt(&attempt));
                }
                self.record_keep_call_attempt(&mut ledger, qso_id, attempt);
                to_recall.push((
                    qso_id,
                    message,
                    progress.metadata.frequency,
                    progress.metadata.tx_parity,
                    progress.metadata.remote_origin,
                    progress.metadata.remote_client_key_id.clone(),
                    attempt_id,
                ));
            }
            ledger.retain(|id, _| qsos.get(id).is_some_and(|p| p.state.is_active()));
        }

        for (
            qso_id,
            message,
            frequency,
            tx_parity,
            snapshot_remote_origin,
            snapshot_remote_client_key_id,
            attempt_id,
        ) in to_recall
        {
            debug!(
                "Manual keep-calling: re-emitting {:?} on {:.1} Hz (qso={})",
                message, frequency, qso_id
            );
            // Round-16 review (Codex P1): re-read the LIVE binding
            // immediately before emitting, rather than trusting the
            // snapshot taken under the write lock above (released before
            // this loop runs) — a concurrent rebind (a remote client
            // taking over this QSO between the snapshot and this emission)
            // would otherwise let a stale `Local`/wrong-identity
            // `MessageToSend` reach `LatestTxIntent`, which the TX worker's
            // pivot mechanism can prefer over the correctly-bound request,
            // bypassing the arm check entirely rather than merely
            // misattributing it. Falls back to the snapshot only if the
            // QSO has since left the active map (shouldn't happen here —
            // `to_recall` was built from this same set moments ago — kept
            // defensive rather than unwrapping).
            let (remote_origin, remote_client_key_id) = match self.qsos.read().await.get(&qso_id) {
                Some(live) => (
                    live.metadata.remote_origin,
                    live.metadata.remote_client_key_id.clone(),
                ),
                None => (snapshot_remote_origin, snapshot_remote_client_key_id),
            };
            self.emit_event(QsoEvent::MessageToSend {
                qso_id,
                message,
                frequency,
                tx_parity,
                remote_origin,
                remote_client_key_id,
                attempt_id: Some(attempt_id),
            })
            .await;
        }

        for (qso_id, action, raised_at_generation) in offset_actions_to_emit {
            self.emit_event(QsoEvent::TxOffsetActionNeeded {
                qso_id,
                action,
                raised_at_generation,
            })
            .await;
        }
    }

    /// Layer 2 timeline persistence note (docs/observability-diagnostics-plan.md):
    /// `progress.state_history`/`.messages` are dropped here along with the
    /// rest of `progress` — this WAS the "QSO leaves the active map" discard
    /// site the plan calls out. It's safe now: every path that can make a
    /// QSO terminal (`process_message_for_qso`, `advance_existing_qso_to_step`,
    /// the open-at-close branch of `respond_to_caller`, `cancel_qso`,
    /// `supersede_active_qsos_for`, `check_timeouts_at`) already captured and
    /// emitted the full timeline on `QsoEvent::QsoCompleted`/`QsoFailed` at
    /// the moment the QSO went terminal — well before it ever reaches this
    /// 1-hour-later cleanup. A `QsoLogger` with `persist_qso_timeline` on has
    /// already durably written it by the time this runs.
    async fn cleanup_completed_qsos(&self) {
        let mut qsos = self.qsos.write().await;
        let cutoff_time = self.now_utc() - Duration::hours(1); // Keep completed QSOs for 1 hour

        let to_remove: Vec<QsoId> = qsos
            .iter()
            .filter(|(_, progress)| match &progress.state {
                QsoState::Completed { completed_at, .. } => *completed_at < cutoff_time,
                QsoState::Failed { failed_at, .. } => *failed_at < cutoff_time,
                _ => false,
            })
            .map(|(&qso_id, _)| qso_id)
            .collect();

        for qso_id in to_remove {
            if let Some(progress) = qsos.remove(&qso_id) {
                if let Some(callsign) = &progress.metadata.their_callsign {
                    drop(qsos); // Release lock before acquiring another
                    self.remove_callsign_mapping(callsign, qso_id).await;
                    qsos = self.qsos.write().await; // Re-acquire lock
                }
                debug!("Cleaned up QSO: {}", qso_id);
            }
        }
    }

    async fn watchdog_pass_at(
        &self,
        now: DateTime<Utc>,
        mono_now: std::time::Instant,
        detector: &mut pancetta_core::slot_clock::ClockStepDetector,
        last_mono: &mut std::time::Instant,
    ) {
        let step = detector.observe(now, mono_now.duration_since(*last_mono));
        *last_mono = mono_now;
        if let Some(skew) = step {
            let qsos = self.qsos.write().await;
            let mut anchors = self
                .watchdog_anchors
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            anchors.clear();
            for (&id, progress) in qsos.iter().filter(|(_, p)| p.state.is_active()) {
                anchors.insert(
                    id,
                    WatchdogAnchors {
                        state: watchdog_state_identity(&progress.state, now)
                            .map(|identity| (identity, now)),
                        first_call: progress
                            .metadata
                            .first_call_at
                            .map(|original| (original, now)),
                    },
                );
            }
            warn!(
                skew_ms = skew.num_milliseconds(),
                "Wall-clock step: reset active QSO watchdog budgets"
            );
        }
        // Preserve rearm-before-retirement ordering and independent call caps.
        self.rearm_manual_calls_at(now).await;
        self.check_timeouts_at(now).await;
    }

    /// Watchdog pass at an explicit time (for testability).
    ///
    /// In addition to the standard per-state timeouts, this enforces the
    /// **manual keep-calling watchdog**: a manual-initiated QSO that is
    /// still in `RespondingToCq` is retired (→ `Failed`/idle, callsign
    /// mapping cleared) once it has either transmitted
    /// `manual_call_max_calls` calls OR `manual_call_watchdog_minutes`
    /// have elapsed since the first call — whichever comes first.
    pub async fn check_timeouts_at(&self, now: DateTime<Utc>) {
        // PAN-280: a completion whose closing frame never got a TX outcome
        // reverts first, so the exemption below applies to it at once.
        self.expire_pending_completions_at(now).await;
        // PAN-280: a QSO whose dropped 73 put it back in its pre-close state
        // gets one rework grace before any timeout applies again.
        let reworking: std::collections::HashSet<QsoId> = {
            let mut reverted = self
                .completion_reverted_at
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            reverted.retain(|_, at| now - *at < COMPLETED_QSO_REWORK_GRACE);
            reverted.keys().copied().collect()
        };
        let mut qsos = self.qsos.write().await;
        let mut timeouts = Vec::new();
        self.watchdog_anchors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|id, _| qsos.get(id).is_some_and(|p| p.state.is_active()));

        for (&qso_id, progress) in qsos.iter_mut() {
            if reworking.contains(&qso_id) {
                continue;
            }
            let (state_age, first_call_age) = {
                let mut anchors = self
                    .watchdog_anchors
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let mut state_age = progress.state.state_duration(now);
                let mut first_call_age = progress.metadata.first_call_at.map(|at| now - at);
                if let Some(anchor) = anchors.get_mut(&qso_id) {
                    if anchor.state.as_ref().is_some_and(|(identity, _)| {
                        Some(*identity) != watchdog_state_identity(&progress.state, now)
                    }) {
                        anchor.state = None;
                    }
                    if anchor.first_call.as_ref().is_some_and(|(original, _)| {
                        Some(*original) != progress.metadata.first_call_at
                    }) {
                        anchor.first_call = None;
                    }
                    if let Some((_, effective)) = anchor.state {
                        state_age = Some(now - effective);
                    }
                    if let Some((_, effective)) = anchor.first_call {
                        first_call_age = Some(now - effective);
                    }
                }
                (state_age, first_call_age)
            };
            let active_tx_state = matches!(
                progress.state,
                QsoState::CallingCq { .. }
                    | QsoState::RespondingToCq { .. }
                    | QsoState::SendingReport { .. }
                    | QsoState::WaitingForReport { .. }
                    | QsoState::WaitingForConfirmation { .. }
                    | QsoState::SendingConfirmation { .. }
            );

            // PAN-17: an outbound message that embeds a callsign the FT8
            // encoder can never represent (neither pack28's fixed 6-char
            // scheme nor the i3=4 nonstandard-callsign hash/58-bit path —
            // see `callsign_is_wire_representable`) would otherwise re-arm
            // and silently fail to transmit every slot for the full
            // watchdog window before self-retiring as a plain Timeout,
            // indistinguishable from "the DX never answered" (the live
            // incident this ticket fixes). Encoding is a pure function of
            // the callsign text, so there is nothing to gain by waiting or
            // retrying — retire on the first watchdog pass that observes
            // it, with a reason distinct from Timeout so the operator sees
            // "cannot transmit this message" rather than a bogus no-reply.
            //
            // Round 2 (Codex review #248, finding 4): checks BOTH sides of
            // the exchange, not just the DX's. Every directed frame also
            // embeds `config.our_callsign` — `StationConfig` permits an
            // arbitrarily long compound call there with no length cap, so a
            // misconfigured `our_callsign` (e.g. `VK9/W1XYZ/MM`, >11 chars)
            // would otherwise leave every QSO against a perfectly normal DX
            // stuck on the generic timeout — the exact PAN-17 symptom just
            // moved to the other callsign.
            //
            // Round 3 (Codex re-review, finding 1): ALSO fast-fails a
            // report-bearing rung (SendingReport / WaitingForReport — see
            // `report_stage_partner_callsign`; PAN-27 finding 3 removed
            // WaitingForConfirmation from that set) whose partner isn't
            // plausibly pack28-standard
            // (`callsign_is_plausibly_pack28_standard`), even though the
            // partner's callsign passes the plain wire-representability
            // check above (fits the i3=4 hash field fine). The round-1
            // encoder fix (`pancetta_ft8::encoder::try_encode_nonstandard`'s
            // doc) makes a numeric report/ack to such a partner fail
            // LOUDLY rather than silently blanking — correct — but nothing
            // stopped the QSO from re-arming that exact doomed encode every
            // slot once it reached this rung, which is the original PAN-17
            // "burns the watchdog window retrying an unencodable message"
            // symptom relocated to the report stage.
            //
            // Round 2 deliberately omitted this check because a naive
            // version (keyed on `metadata.their_callsign`, the "most
            // complete form ever seen") falsely retired healthy QSOs in
            // `adversarial_compound_calls.rs`'s compound-then-base
            // scenarios. That risk doesn't apply here: `report_stage_
            // partner_callsign` reads the STATE's own `their_callsign`
            // field, which is exactly the value `MessageExchange::
            // generate_response` embeds in the real outgoing
            // SignalReport/ReportAck (`to_station`/`from_station` come
            // straight from this same field — see exchange.rs) — so this
            // check fires if and only if the message the QSO is actually
            // about to (re-)send is genuinely unencodable, not on a
            // heuristic guess. `compound_first_then_base_completes` and
            // `cqer_caller_compound_then_base_completes` were updated to
            // assert the (now correct) fast MessageUnencodable retirement
            // — those scenarios need a real numeric report addressed to a
            // compound-form callsign, which round 1's encoder fix already
            // made impossible over real FT8; the tests just never
            // exercised the real encoder before, so they didn't notice.
            if active_tx_state {
                let our = self.config.our_callsign.as_str();
                let their = progress.metadata.their_callsign.as_deref();

                let unencodable_reason = if !callsign_is_wire_representable(our) {
                    Some(format!(
                        "our configured callsign '{}' cannot be represented in any FT8 message format",
                        our
                    ))
                } else if let Some(their) = their {
                    if !callsign_is_wire_representable(their) {
                        Some(format!(
                            "callsign '{}' cannot be represented in any FT8 message format",
                            their
                        ))
                    } else if let Some(report_partner) =
                        report_stage_partner_callsign(&progress.state)
                    {
                        if !callsign_is_plausibly_pack28_standard(report_partner)
                            || !callsign_is_plausibly_pack28_standard(our)
                        {
                            Some(format!(
                                "cannot send a numeric report/ack to '{}' — i3=4 (required \
                                 because a compound callsign is involved) has no field for one",
                                report_partner
                            ))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(reason) = unencodable_reason {
                    timeouts.push((qso_id, QsoFailureReason::MessageUnencodable(reason)));
                    continue;
                }
            }

            // Repetitive-TX watchdog (operator request): if a QSO has sat in the
            // same active TX state — i.e. we've been re-sending the SAME message
            // without the DX advancing us — longer than repetitive_tx_timeout_secs,
            // retire it. Applies to BOTH manual and auto QSOs and is checked first
            // (of the time-based watchdogs) so it bounds "stuck sending the same
            // thing" even while the manual keep-call watchdog (below) would
            // otherwise keep re-arming. A forward state advance resets the
            // state's `started_at`, so a healthy, progressing QSO never trips this.
            if active_tx_state {
                if let Some(dur) = state_age {
                    if dur.num_seconds() > self.config.timeouts.repetitive_tx_timeout_secs as i64 {
                        timeouts.push((qso_id, QsoFailureReason::Timeout));
                        continue;
                    }
                }
            }

            // Manual keep-calling watchdog. Covers CallingCq (operator `c`:
            // re-calling CQ until someone answers), RespondingToCq
            // (re-calling the DX), SendingReport (FIX 4: re-sending our
            // R-report when the DX repeats their report), and WaitingForReport
            // (SM-F4: re-sending our report when a caller repeats their grid
            // because they never copied it). In all phases the operator is
            // actively keep-calling, and `call_count` / `first_call_at` span
            // the whole QSO, so the 10-calls / 5-min bound applies to the QSO
            // as a whole. Once the DX advances past these states (a caller
            // answers / ReportAck / RR73 received), the normal state timeouts
            // take over.
            if progress.metadata.initiated_by == CallInitiation::Manual
                && matches!(
                    progress.state,
                    QsoState::CallingCq { .. }
                        | QsoState::RespondingToCq { .. }
                        | QsoState::SendingReport { .. }
                        | QsoState::WaitingForReport { .. }
                )
            {
                let max_calls = self.config.timeouts.manual_call_max_calls;
                let watchdog =
                    Duration::minutes(self.config.timeouts.manual_call_watchdog_minutes as i64);
                let elapsed = first_call_age.unwrap_or_else(Duration::zero);

                // C3 race guard: if this QSO made a forward state advance in the
                // current watchdog cycle (the DX just answered), grant a
                // one-pass reprieve — do NOT retire it in the same tick it
                // advanced. The flag is consumed (cleared) here, so a QSO that
                // advanced once then goes silent is retired on the NEXT pass
                // once it re-hits the cap (per-QSO bound preserved; see C12).
                let progressed = progress.metadata.progressed_this_cycle;
                progress.metadata.progressed_this_cycle = false;

                // The call-count cap exists to stop pounding a DX that never
                // answered our INITIAL call (CallingCq / RespondingToCq). Once
                // the DX has ENGAGED — we received their report and advanced to
                // SendingReport — they can clearly hear us, so abandoning the
                // exchange at the call cap is wrong: it can drop the QSO in the
                // very window the DX's closing RR73 arrives. (Observed on-air
                // 2026-06-22, 9K2MP: the 10-call cap retired the QSO at 00:07:29,
                // one slot before its RR73 at 00:07:44 — so the RR73 had no
                // active QSO to complete and the operator had to close it by
                // hand.) In SendingReport, only the (longer) time watchdog and
                // the DX's own messages drive the QSO; the call cap does not
                // apply.
                //
                // SM-F4: WaitingForReport belongs with SendingReport here, NOT
                // with CallingCq/RespondingToCq — a CQer in WaitingForReport
                // has already been ANSWERED (a station replied to our CQ and
                // we've already sent them our report); this is the identical
                // "DX has engaged us" situation as SendingReport, just on the
                // CQer ladder rather than the Caller ladder. Applying the
                // call-count cap here would risk the same 9K2MP-style
                // premature drop for a CQer whose caller is slow to close, so
                // WaitingForReport is likewise exempt from the cap and
                // governed only by the (longer) time watchdog.
                let in_initial_call = matches!(
                    progress.state,
                    QsoState::CallingCq { .. } | QsoState::RespondingToCq { .. }
                );
                // PAN-75: the frame that reaches the cap must get its TX
                // outcome before the cap retires the QSO, or an unsent final
                // frame could never be given back. Hold retirement while any
                // keep-call attempt is still inside the rollback window; a
                // rollback drops `call_count` below the cap and re-arming
                // resumes. Re-arming stops at the cap, so nothing new airs
                // while we wait, and the time watchdog still applies.
                let attempt_unresolved = {
                    let cutoff = now
                        - Duration::milliseconds(
                            UNSENT_ATTEMPT_RETENTION_SLOTS * self.rearm_slot_millis(),
                        );
                    self.keep_call_ledger
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&qso_id)
                        .is_some_and(|entry| entry.attempts.iter().any(|a| a.emitted_at >= cutoff))
                };
                let call_cap_hit = in_initial_call
                    && progress.metadata.call_count >= max_calls
                    && !attempt_unresolved;
                if !progressed && (call_cap_hit || elapsed >= watchdog) {
                    timeouts.push((qso_id, QsoFailureReason::Timeout));
                }
                // Manual calls do not use the (much shorter) per-state
                // timeout while keep-calling; the watchdog above governs.
                continue;
            }

            if let Some(duration) = state_age {
                let timeout_seconds = match &progress.state {
                    QsoState::CallingCq { .. } => self.config.timeouts.cq_timeout,
                    QsoState::WaitingForReport { .. } => self.config.timeouts.report_timeout,
                    QsoState::WaitingForConfirmation { .. } => {
                        self.config.timeouts.confirmation_timeout
                    }
                    // Phase 5: an AUTO pounce / CQ-answer that the DX never replies
                    // to must retire so it does not pin `max_concurrent_qsos`
                    // forever. Manual QSOs in these states are governed by the
                    // keep-call watchdog above (which already `continue`d), so these
                    // arms apply only to Auto QSOs. `report_timeout` is the natural
                    // bound — it is how long we wait for the DX's next frame before
                    // giving up on a one-shot autonomous call.
                    QsoState::RespondingToCq { .. } | QsoState::SendingReport { .. } => {
                        self.config.timeouts.report_timeout
                    }
                    _ => continue,
                };

                // Compare as signed seconds: a NEGATIVE elapsed (the state's
                // `started_at` is later than `now`) must never count as a
                // timeout. In production `started_at` and `now` are both the real
                // clock so elapsed is always ≥ 0, but the sim harness anchors its
                // virtual `now` to a slot boundary that can sit slightly behind
                // the real-clock `started_at` the engine stamps — casting a
                // negative `num_seconds()` to `u64` would wrap to a huge value
                // and spuriously retire a just-opened QSO.
                if duration.num_seconds() > timeout_seconds as i64 {
                    timeouts.push((qso_id, QsoFailureReason::Timeout));
                }
            }
        }

        for (qso_id, reason) in timeouts {
            if let Some(mut progress) = qsos.remove(&qso_id) {
                self.watchdog_anchors
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&qso_id);
                let old_state = progress.state.clone();
                progress.state = QsoState::Failed {
                    reason: reason.clone(),
                    failed_at: now,
                    last_state: Box::new(old_state.clone()),
                };
                // Capture metadata before it's consumed below (Batch 4,
                // SM-F5 — needed for QsoFailed).
                let metadata = progress.metadata.clone();
                // Layer 2 timeline persistence: this is the most common
                // "QSO leaves the active map" discard site (every
                // watchdog/timeout retirement runs through here) — capture
                // the timeline before `progress` is dropped at loop end.
                let state_history = progress.state_history.clone();
                let messages = progress.messages.clone();

                drop(qsos); // Release lock before emitting events
                self.emit_state_change(qso_id, old_state.clone(), progress.state.clone())
                    .await;
                // SM-F5: this is the most common failure producer (every
                // watchdog/timeout retirement runs through here) and
                // previously never emitted QsoFailed, so the coordinator's
                // priority-scoring failure backoff (`record_failure`) never
                // fired for a timed-out QSO. `reason` here is
                // QsoFailureReason::Timeout for every timing-based push site
                // above, or QsoFailureReason::MessageUnencodable for the
                // PAN-17 unencodable-callsign check.
                self.emit_event(QsoEvent::QsoFailed {
                    qso_id,
                    reason,
                    last_state: old_state,
                    metadata,
                    state_history,
                    messages,
                })
                .await;

                if let Some(callsign) = &progress.metadata.their_callsign {
                    self.remove_callsign_mapping(callsign, qso_id).await;
                }

                warn!("QSO timeout: {}", qso_id);
                qsos = self.qsos.write().await; // Re-acquire lock
            }
        }
    }
}

impl Clone for QsoManager {
    fn clone(&self) -> Self {
        Self {
            utc_clock: self.utc_clock.clone(),
            config: self.config.clone(),
            qsos: Arc::clone(&self.qsos),
            watchdog_anchors: Arc::clone(&self.watchdog_anchors),
            keep_call_ledger: Arc::clone(&self.keep_call_ledger),
            pending_completions: Arc::clone(&self.pending_completions),
            completion_reverted_at: Arc::clone(&self.completion_reverted_at),
            attempt_id_counter: Arc::clone(&self.attempt_id_counter),
            qsos_by_callsign: Arc::clone(&self.qsos_by_callsign),
            event_sender: self.event_sender.clone(),
            next_serial: Arc::clone(&self.next_serial),
            cleanup_interval: Arc::clone(&self.cleanup_interval),
            database: self.database.clone(),
            require_database: self.require_database,
            dial_frequency_hz: Arc::clone(&self.dial_frequency_hz),
            split_tx_frequency_hz: Arc::clone(&self.split_tx_frequency_hz),
            tx_freq_mode: Arc::clone(&self.tx_freq_mode),
            ptt_active: Arc::clone(&self.ptt_active),
            ptt_sync_gate: Arc::clone(&self.ptt_sync_gate),
            relocation_epochs: Arc::clone(&self.relocation_epochs),
            tx_policy: Arc::clone(&self.tx_policy),
            remote_tx_permitted: Arc::clone(&self.remote_tx_permitted),
            hamlib_hard_muted: Arc::clone(&self.hamlib_hard_muted),
            // Arc-shared, deliberately: `start()` runs the rearm/stall loop on
            // this clone and must observe runtime mode switches (see the
            // field's doc comment).
            active_slot_ns: Arc::clone(&self.active_slot_ns),
            // Reload publication also reaches the background timeout clone.
            tx_late_max_ms: self.tx_late_max_ms.clone(),
        }
    }
}

// Default implementations removed - using the ones at lines 191-226

#[cfg(test)]
#[path = "qso_manager/tests/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "qso_manager/tests/sender_verification_tests.rs"]
mod sender_verification_tests;

#[cfg(test)]
#[path = "qso_manager/tests/reply_emitter_tests.rs"]
mod reply_emitter_tests;

#[cfg(test)]
#[path = "qso_manager/tests/state_regression_tests.rs"]
mod state_regression_tests;

#[cfg(test)]
#[path = "qso_manager/tests/sm_f4_waiting_for_report_resend_tests.rs"]
mod sm_f4_waiting_for_report_resend_tests;

#[cfg(test)]
#[path = "qso_manager/tests/sm_f6_auto_bounded_resend_tests.rs"]
mod sm_f6_auto_bounded_resend_tests;

#[cfg(test)]
#[path = "qso_manager/tests/sm_f5_qso_failed_event_tests.rs"]
mod sm_f5_qso_failed_event_tests;

#[cfg(test)]
#[path = "qso_manager/tests/tx_frequency_hold_tests.rs"]
mod tx_frequency_hold_tests;

#[cfg(test)]
#[path = "qso_manager/tests/stall_detection_tests.rs"]
mod stall_detection_tests;

#[cfg(test)]
#[path = "qso_manager/tests/answered_frame_tests.rs"]
mod answered_frame_tests;

#[cfg(test)]
#[path = "qso_manager/tests/has_active_or_recent_qso_tests.rs"]
mod has_active_or_recent_qso_tests;

#[cfg(test)]
#[path = "qso_manager/tests/fox_role_tests.rs"]
mod fox_role_tests;
#[cfg(test)]
#[path = "qso_manager/tests/hound_tests.rs"]
mod hound_tests;

#[cfg(test)]
#[path = "qso_manager/tests/deconflict_tests.rs"]
mod deconflict_tests;

#[cfg(test)]
#[path = "qso_manager/tests/timeout_config_tests.rs"]
mod timeout_config_tests;

#[cfg(test)]
#[path = "qso_manager/tests/current_remote_binding_tests.rs"]
mod current_remote_binding_tests;

#[cfg(test)]
#[path = "qso_manager/tests/guard_recheck_tests.rs"]
mod guard_recheck_tests;

#[cfg(test)]
#[path = "qso_manager/tests/manual_call_outcome_tests.rs"]
mod manual_call_outcome_tests;

#[cfg(test)]
#[path = "qso_manager/tests/completed_history_tests.rs"]
mod completed_history_tests;

#[cfg(test)]
#[path = "qso_manager/tests/pending_completion_tests.rs"]
mod pending_completion_tests;

#[cfg(test)]
#[path = "qso_manager/tests/completed_close_tests.rs"]
mod completed_close_tests;

#[cfg(test)]
#[path = "qso_manager/tests/clock_step_watchdog_tests.rs"]
mod clock_step_watchdog_tests;
