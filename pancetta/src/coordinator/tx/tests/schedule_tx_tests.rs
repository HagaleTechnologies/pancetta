use super::*;
use pancetta_core::slot::{SlotParity, SLOT_NS};

/// FT4 slot period in nanoseconds (7.5s). FT8 is `SLOT_NS` (15s).
const FT4_SLOT_NS: i64 = 7_500_000_000;

fn at(seconds: f64) -> UtcTimestamp {
    // Reference: 2026-01-01 00:00:00 UTC. timestamp() = 1767225600,
    // divisible by 15. Slot 0 is Even (= 1767225600 / 15 % 2 = 0).
    let base = UtcTimestamp::from_ymd_hms(2026, 1, 1, 0, 0, 0).unwrap();
    base + SignedDuration::from_nanos((seconds * 1_000_000_000.0) as i64)
}

#[test]
fn early_pads_silent_no_skip() {
    // now = :05.0 (Even slot 0). Required = Odd. Current slot is Even
    // (wrong), so we advance to next Odd = :15. mstr_relative_to_target
    // = max(0, :05 - :15) = 0. 0 < 500 → pad 500ms, cursor 0.
    let s = schedule_tx(at(5.0), SlotParity::Odd, 8000, 12_000, SLOT_NS);
    assert_eq!((s.target_slot - at(0.0)).as_millis_i64(), 15_000);
    assert_eq!(s.silent_pad_samples, 500 * 12); // 12 samples/ms at 12kHz
    assert_eq!(s.cursor_offset_samples, 0);
}

#[test]
fn on_time_no_pad_no_skip() {
    // now = :15.500 (Odd slot 1). Required = Odd. Current slot matches;
    // mstr_in_current_slot = 500ms ≤ 8000 → target current slot :15.
    // mstr_relative_to_target = 500 = DELAY_MS → pad 0, cursor 0.
    let s = schedule_tx(at(15.5), SlotParity::Odd, 8000, 12_000, SLOT_NS);
    assert_eq!((s.target_slot - at(0.0)).as_millis_i64(), 15_000);
    assert_eq!(s.silent_pad_samples, 0);
    assert_eq!(s.cursor_offset_samples, 0);
}

#[test]
fn late_skips_cursor_in_current_slot() {
    // now = :20.0 (Odd slot 1, 5s in). Required = Odd. Current matches;
    // mstr_in_current_slot = 5000 ≤ 8000 → target current slot :15.
    // mstr_relative_to_target = 5000 > 500 → cursor = 4500ms × SR.
    let s = schedule_tx(at(20.0), SlotParity::Odd, 8000, 12_000, SLOT_NS);
    assert_eq!((s.target_slot - at(0.0)).as_millis_i64(), 15_000);
    assert_eq!(s.silent_pad_samples, 0);
    assert_eq!(s.cursor_offset_samples, 4500 * 12);
}

#[test]
fn too_late_defers_to_next_opposite_slot() {
    // now = :24.5 (Odd slot 1, 9.5s in). Required = Odd. Current
    // matches but mstr_in_current_slot = 9500 > 8000 → too late;
    // advance to next Odd = :45. mstr_relative_to_target = 0 (target
    // is in future) → pad 500ms, cursor 0.
    let s = schedule_tx(at(24.5), SlotParity::Odd, 8000, 12_000, SLOT_NS);
    assert_eq!((s.target_slot - at(0.0)).as_millis_i64(), 45_000);
    assert_eq!(s.silent_pad_samples, 500 * 12);
    assert_eq!(s.cursor_offset_samples, 0);
}

#[test]
fn collision_avoidance_does_not_pick_same_parity() {
    // now = :14.6 (Even slot 0, near end). DX on Even → required Odd.
    // Current parity Even ≠ Odd → advance to next Odd = :15.
    // mstr_relative_to_target = max(0, :14.6 - :15) = 0 → pad 500ms,
    // cursor 0. Most importantly: target is :15, NEVER :30 (the
    // collision case the original bug produced).
    let s = schedule_tx(at(14.6), SlotParity::Odd, 8000, 12_000, SLOT_NS);
    assert_eq!((s.target_slot - at(0.0)).as_millis_i64(), 15_000);
    assert_ne!((s.target_slot - at(0.0)).as_millis_i64(), 30_000);
    assert_eq!(s.silent_pad_samples, 500 * 12);
    assert_eq!(s.cursor_offset_samples, 0);
}

#[test]
fn at_exact_boundary_targets_current_slot() {
    // now = :15.000 exactly. Required = Odd. The :15 slot is Odd
    // and we're 0ms in — fully viable. Target the current slot,
    // pad 500ms before audio starts.
    let s = schedule_tx(at(15.0), SlotParity::Odd, 8000, 12_000, SLOT_NS);
    assert_eq!((s.target_slot - at(0.0)).as_millis_i64(), 15_000);
    assert_eq!(s.silent_pad_samples, 500 * 12);
    assert_eq!(s.cursor_offset_samples, 0);
}

#[test]
fn current_slot_correct_parity_but_too_late_defers() {
    // now = :29.0 (Odd slot 1, 14s in — past tx_late_max_ms=8000).
    // Even though parity matches, we're too late for skip-ahead.
    // Defer to next Odd = :45.
    let s = schedule_tx(at(29.0), SlotParity::Odd, 8000, 12_000, SLOT_NS);
    assert_eq!((s.target_slot - at(0.0)).as_millis_i64(), 45_000);
    assert_eq!(s.silent_pad_samples, 500 * 12);
    assert_eq!(s.cursor_offset_samples, 0);
}

#[test]
fn ft4_period_targets_7_5s_grid_boundary() {
    // FT4 slot period = 7.5s. On this grid, at(0.0) is Even (slot
    // 235630080 = even), at(7.5) Odd, at(15) Even, at(22.5) Odd.
    // now = :05.0 (Even slot 0 on the FT4 grid). Required = Odd.
    // Current parity Even ≠ Odd → advance to next Odd = :07.5.
    // mstr_relative_to_target = max(0, :05 - :07.5) = 0 < 500 →
    // pad 500ms, cursor 0. The target MUST land on a 7.5s-grid
    // boundary of the requested (Odd) parity — :07.5, NOT :15.
    let s = schedule_tx(at(5.0), SlotParity::Odd, 8000, 12_000, FT4_SLOT_NS);
    assert_eq!((s.target_slot - at(0.0)).as_millis_i64(), 7_500);
    // boundary lands on the 7.5s grid
    assert_eq!(
        (s.target_slot - at(0.0)).as_millis_i64() % 7_500,
        0,
        "target must sit on a 7.5s slot boundary"
    );
    // and it is the requested (Odd) parity
    assert_eq!(
        pancetta_core::slot::SlotParity::of_with_period(s.target_slot, FT4_SLOT_NS),
        SlotParity::Odd
    );
    assert_eq!(s.silent_pad_samples, 500 * 12);
    assert_eq!(s.cursor_offset_samples, 0);
}

#[test]
fn ft4_period_late_skips_cursor_in_current_slot() {
    // now = :10.0 (Odd slot at :07.5 on the FT4 grid, 2.5s in).
    // Required = Odd. Current matches; mstr_in_current_slot = 2500
    // ≤ 8000 → target current slot :07.5. mstr_relative_to_target
    // = 2500 > 500 → cursor = 2000ms × SR; no defer to :22.5.
    let s = schedule_tx(at(10.0), SlotParity::Odd, 8000, 12_000, FT4_SLOT_NS);
    assert_eq!((s.target_slot - at(0.0)).as_millis_i64(), 7_500);
    assert_eq!(s.silent_pad_samples, 0);
    assert_eq!(s.cursor_offset_samples, 2_000 * 12);
}

#[test]
fn ft4_resolve_required_parity_auto_picks_nearest_next_slot() {
    use pancetta_config::station::TxSelfParity;
    // now = :05 (Even slot 0 on FT4 grid). Next Even = :15,
    // next Odd = :07.5. Odd is closer → Auto picks Odd.
    let p = resolve_required_parity(None, TxSelfParity::Auto, at(5.0), FT4_SLOT_NS);
    assert_eq!(p, SlotParity::Odd);
}

#[test]
fn coalesce_collect_window_ft8_byte_identical() {
    assert_eq!(
        coalesce_collect_window_ms(pancetta_ft8::Protocol::Ft8),
        COALESCE_COLLECT_WINDOW_MS
    );
}

#[test]
fn coalesce_collect_window_scales_down_for_ft4() {
    // FT4 cycle = 7.5s, half of FT8's 15s → half the wait (400ms).
    assert_eq!(coalesce_collect_window_ms(pancetta_ft8::Protocol::Ft4), 400);
}

#[test]
#[cfg(feature = "ft2")]
fn coalesce_collect_window_scales_down_for_ft2() {
    // FT2 cycle = 3.2s → 800 * 3.2 / 15 = 170.67, rounds to 171ms.
    assert_eq!(coalesce_collect_window_ms(pancetta_ft8::Protocol::Ft2), 171);
}

#[test]
fn tx_late_max_ms_effective_ft8_byte_identical() {
    assert_eq!(
        tx_late_max_ms_effective(pancetta_ft8::Protocol::Ft8, 8000),
        8000
    );
}

#[test]
fn tx_late_max_ms_effective_scales_down_for_ft4() {
    // FT4 cycle = 7.5s, half of FT8's 15s → half the cap (4000ms).
    assert_eq!(
        tx_late_max_ms_effective(pancetta_ft8::Protocol::Ft4, 8000),
        4000
    );
}

#[test]
#[cfg(feature = "ft2")]
fn tx_late_max_ms_effective_scales_down_for_ft2() {
    // FT2 cycle = 3.2s → 8000 * 3.2 / 15 = 1706.67, rounds to 1707ms.
    assert_eq!(
        tx_late_max_ms_effective(pancetta_ft8::Protocol::Ft2, 8000),
        1707
    );
}

#[test]
fn resolve_required_parity_explicit_wins_over_config() {
    use pancetta_config::station::TxSelfParity;
    // tx_parity = Some(Even), config = Auto → returns Even
    let p = resolve_required_parity(Some(SlotParity::Even), TxSelfParity::Auto, at(5.0), SLOT_NS);
    assert_eq!(p, SlotParity::Even);
}

#[test]
fn resolve_required_parity_explicit_wins_over_explicit_config() {
    use pancetta_config::station::TxSelfParity;
    // tx_parity = Some(Even), config = Odd → tx_parity wins → Even
    let p = resolve_required_parity(Some(SlotParity::Even), TxSelfParity::Odd, at(5.0), SLOT_NS);
    assert_eq!(p, SlotParity::Even);
}

#[test]
fn resolve_required_parity_falls_back_to_config_when_none() {
    use pancetta_config::station::TxSelfParity;
    let p = resolve_required_parity(None, TxSelfParity::Even, at(5.0), SLOT_NS);
    assert_eq!(p, SlotParity::Even);
    let p = resolve_required_parity(None, TxSelfParity::Odd, at(5.0), SLOT_NS);
    assert_eq!(p, SlotParity::Odd);
}

#[test]
fn resolve_required_parity_auto_picks_nearest_next_slot() {
    use pancetta_config::station::TxSelfParity;
    // now = :05 (in Even slot 0). Next Even = :30, next Odd = :15.
    // Odd is closer → Auto picks Odd.
    let p = resolve_required_parity(None, TxSelfParity::Auto, at(5.0), SLOT_NS);
    assert_eq!(p, SlotParity::Odd);
    // now = :20 (in Odd slot 1). Next Odd = :45, next Even = :30.
    // Even is closer → Auto picks Even.
    let p = resolve_required_parity(None, TxSelfParity::Auto, at(20.0), SLOT_NS);
    assert_eq!(p, SlotParity::Even);
}

#[tokio::test(flavor = "current_thread")]
async fn interruptible_sleep_completes_when_no_shutdown() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let interrupted = interruptible_sleep(Duration::from_millis(120), &shutdown, &abort).await;
    assert!(
        !interrupted,
        "should not flag interrupted when both flags stay false"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn interruptible_sleep_returns_immediately_if_already_shutdown() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    let shutdown = Arc::new(AtomicBool::new(true));
    let abort = Arc::new(AtomicBool::new(false));
    let start = std::time::Instant::now();
    let interrupted = interruptible_sleep(Duration::from_secs(60), &shutdown, &abort).await;
    let elapsed = start.elapsed();
    assert!(
        interrupted,
        "must signal interrupted when shutdown is already set"
    );
    assert!(
        elapsed < Duration::from_millis(50),
        "should return without sleeping (elapsed={:?})",
        elapsed
    );
}

#[tokio::test(flavor = "current_thread")]
async fn interruptible_sleep_returns_immediately_if_already_aborted() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(true));
    let start = std::time::Instant::now();
    let interrupted = interruptible_sleep(Duration::from_secs(60), &shutdown, &abort).await;
    let elapsed = start.elapsed();
    assert!(
        interrupted,
        "must signal interrupted when abort is already set"
    );
    assert!(
        elapsed < Duration::from_millis(50),
        "should return without sleeping (elapsed={:?})",
        elapsed
    );
}

#[tokio::test(flavor = "current_thread")]
async fn interruptible_sleep_wakes_within_one_chunk_when_shutdown_fires() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let s2 = shutdown.clone();
    // After 200ms, flip the shutdown flag.
    let setter = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        s2.store(true, Ordering::Release);
    });
    let start = std::time::Instant::now();
    let interrupted = interruptible_sleep(Duration::from_secs(30), &shutdown, &abort).await;
    let elapsed = start.elapsed();
    let _ = setter.await;
    assert!(
        interrupted,
        "should signal interrupted when flag flips mid-sleep"
    );
    // Polling chunk is 50ms, so wake latency from flag flip is at most one chunk.
    // Total elapsed ≈ 200ms (when flag flipped) + ≤ 50ms (chunk poll) = ≤ 250ms.
    // Allow 300ms slack for test-runner jitter.
    assert!(
        elapsed < Duration::from_millis(300),
        "wake latency exceeded one chunk (elapsed={:?})",
        elapsed
    );
}

/// `current_tx_policy` round-trips the shared atomic. The TuneRequest /
/// TransmitRequest / MultiTransmitRequest worker arms all hard-mute when
/// this reads `Disabled`; assert the encoding so a stray atomic value can't
/// silently un-mute the tune carrier (UX audit Batch 1).
#[test]
fn current_tx_policy_reads_disabled_for_tune_mute() {
    use std::sync::atomic::AtomicU8;
    use std::sync::Arc;
    let p = Arc::new(AtomicU8::new(pancetta_core::TxPolicy::Disabled.as_u8()));
    assert_eq!(current_tx_policy(&p), pancetta_core::TxPolicy::Disabled);
    p.store(pancetta_core::TxPolicy::Full.as_u8(), Ordering::Release);
    assert_eq!(current_tx_policy(&p), pancetta_core::TxPolicy::Full);
    p.store(
        pancetta_core::TxPolicy::RespondOnly.as_u8(),
        Ordering::Release,
    );
    assert_eq!(current_tx_policy(&p), pancetta_core::TxPolicy::RespondOnly);
}

/// The worker's drop-decision helper reads the shared active-QSO set
/// through its RwLock and matches `super::tx_qso_is_live`'s semantics:
/// live id → transmit, absent id → drop, `None` → always transmit.
#[test]
fn worker_tx_qso_is_live_reads_shared_set() {
    use std::collections::HashSet;
    use std::sync::{Arc, RwLock};
    let set: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
    set.write()
        .unwrap()
        .insert(super::super::active_tx_qso_key("qso-live"));

    // Manual / tune (no qso_id) always transmits.
    assert!(super::tx_qso_is_live(None, &set));
    // Live QSO transmits (case-insensitive).
    assert!(super::tx_qso_is_live(Some("QSO-LIVE"), &set));
    // Ended QSO (not in set) is dropped.
    assert!(!super::tx_qso_is_live(Some("qso-ended"), &set));
}

/// A poisoned lock fails OPEN — a stuck lock must never silently mute a
/// legitimate TX. (The operator emergency stop covers the rare worst case.)
#[test]
fn worker_tx_qso_is_live_fails_open_on_poison() {
    use std::collections::HashSet;
    use std::sync::{Arc, RwLock};
    let set: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
    // Poison the lock.
    let s2 = set.clone();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _g = s2.write().unwrap();
        panic!("poison");
    }));
    assert!(set.is_poisoned());
    // Even for a qso_id that would otherwise be "ended", fail-open → true.
    assert!(super::tx_qso_is_live(Some("qso-ended"), &set));
}

/// 2026-07-17 fix: the multi-TX key-time gate must require EVERY bundled
/// item to still be live, not just one — a single still-live item must
/// NOT be enough to carry a cancelled bundle-mate's stale call onto the
/// air, since the summed waveform can't be partially re-filtered.
#[test]
fn multi_tx_bundle_requires_every_item_live() {
    use std::collections::HashSet;
    use std::sync::{Arc, RwLock};
    let set: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
    set.write()
        .unwrap()
        .insert(super::super::active_tx_qso_key("qso-peru"));

    // All live → transmit.
    assert!(super::multi_tx_bundle_still_fully_live(
        &[Some("qso-peru".to_string())],
        &set
    ));

    // One live (Peru), one stale (Kenya, cancelled — not in the set):
    // the operator's exact reported scenario. Must now DROP the whole
    // bundle instead of transmitting Kenya's stale call alongside
    // Peru's live one.
    assert!(!super::multi_tx_bundle_still_fully_live(
        &[Some("qso-kenya".to_string()), Some("qso-peru".to_string())],
        &set
    ));

    // All stale → drop (matches the pre-fix "all ended" case).
    assert!(!super::multi_tx_bundle_still_fully_live(
        &[Some("qso-kenya".to_string())],
        &set
    ));

    // A `None` qso_id (manual/tune item) never gates — mixed with a
    // still-live item, the bundle stays fully live.
    assert!(super::multi_tx_bundle_still_fully_live(
        &[None, Some("qso-peru".to_string())],
        &set
    ));

    // A `None` qso_id mixed with a STALE item: the stale item still
    // fails the gate for the whole bundle.
    assert!(!super::multi_tx_bundle_still_fully_live(
        &[None, Some("qso-kenya".to_string())],
        &set
    ));
}

// --- PAN-141: DX-parity freshness gate ---

const DIAL_20M: u64 = 14_074_000;
const DIAL_15M: u64 = 21_074_000;

/// An a7 entry heard on 20 m on the FT8 grid — the context every
/// pre-PAN-146 test implicitly assumed.
fn a7_call(
    callsign: &str,
    slot_parity: u8,
    decoded_at: std::time::SystemTime,
) -> pancetta_qso::A7ExpectedCall {
    a7_call_in(callsign, slot_parity, decoded_at, DIAL_20M, SLOT_NS)
}

fn a7_call_in(
    callsign: &str,
    slot_parity: u8,
    decoded_at: std::time::SystemTime,
    captured_dial_hz: u64,
    slot_ns: i64,
) -> pancetta_qso::A7ExpectedCall {
    pancetta_qso::A7ExpectedCall {
        callsign: callsign.to_string(),
        freq_hz: 1500.0,
        slot_parity,
        decoded_at,
        captured_dial_hz,
        slot_ns,
    }
}

/// Keying on 20 m, FT8 grid.
fn ft8_20m() -> super::DxParityContext {
    super::DxParityContext {
        dial_hz: DIAL_20M,
        slot_ns: SLOT_NS,
    }
}

#[test]
fn dx_parity_conflict_true_when_fresh_and_same_parity() {
    let cts = pancetta_qso::CrossTimeState::empty();
    let now = std::time::SystemTime::now();
    cts.a7_recent_calls
        .write()
        .unwrap()
        .record(a7_call("5Z4VJ", 1 /* Odd */, now));

    // 5Z4VJ was just observed transmitting on Odd — the exact parity we
    // are about to key into (the PAN-141 collision shape).
    assert!(super::dx_parity_conflict(
        "5Z4VJ",
        SlotParity::Odd,
        ft8_20m(),
        &cts,
        now,
    ));
}

/// Codex P2 on PR #370 (round 2): the table is populated from on-air
/// decodes (canonically uppercase), but `their_callsign` can arrive
/// lowercase from a remote `callStation` command's `QsoMetadata`. A raw
/// case-sensitive `HashMap` lookup must not silently defeat the gate.
#[test]
fn dx_parity_conflict_true_for_lowercase_query_against_uppercase_table_entry() {
    let cts = pancetta_qso::CrossTimeState::empty();
    let now = std::time::SystemTime::now();
    cts.a7_recent_calls
        .write()
        .unwrap()
        .record(a7_call("5Z4VJ", 1, now));

    assert!(super::dx_parity_conflict(
        "5z4vj",
        SlotParity::Odd,
        ft8_20m(),
        &cts,
        now,
    ));
}

/// Codex P2 on PR #370 (round 3): a QSO can latch a compound spelling
/// (`EA8/G8BCG`) while a later on-air frame signs the bare base call
/// (`G8BCG`), or vice versa — the repo's own `callsigns_match` already
/// treats these as the same station; an exact-key lookup must not
/// silently defeat the gate for exactly the case it exists to catch.
#[test]
fn dx_parity_conflict_true_for_compound_callsign_against_bare_table_entry() {
    let cts = pancetta_qso::CrossTimeState::empty();
    let now = std::time::SystemTime::now();
    cts.a7_recent_calls
        .write()
        .unwrap()
        .record(a7_call("G8BCG", 1, now));

    assert!(super::dx_parity_conflict(
        "EA8/G8BCG",
        SlotParity::Odd,
        ft8_20m(),
        &cts,
        now,
    ));
}

/// Local review gate, round 3: a compound and bare spelling of the same
/// station are separate table entries with independent timestamps and
/// parities. An OLDER alias (`EA8/G8BCG`, Odd, 25s ago) must not win
/// over the DX's actual latest known state (`G8BCG`, Even, now) just
/// because it happens to match the parity being tested — the newest
/// fresh matching entry decides, not any historically-matching one.
#[test]
fn dx_parity_conflict_uses_newest_matching_entry_not_a_stale_alias() {
    let cts = pancetta_qso::CrossTimeState::empty();
    let now = std::time::SystemTime::now();
    let older = now - std::time::Duration::from_secs(25);
    cts.a7_recent_calls
        .write()
        .unwrap()
        .record(a7_call("EA8/G8BCG", 1 /* Odd */, older));
    cts.a7_recent_calls
        .write()
        .unwrap()
        .record(a7_call("G8BCG", 0 /* Even */, now));

    assert!(
        !super::dx_parity_conflict("G8BCG", SlotParity::Odd, ft8_20m(), &cts, now,),
        "the newest entry (Even, now) must win over the older Odd alias"
    );
}

#[test]
fn dx_parity_conflict_false_when_fresh_and_opposite_parity() {
    let cts = pancetta_qso::CrossTimeState::empty();
    let now = std::time::SystemTime::now();
    cts.a7_recent_calls
        .write()
        .unwrap()
        .record(a7_call("5Z4VJ", 0 /* Even */, now));

    // Still consistent with our latched assumption — proceed.
    assert!(!super::dx_parity_conflict(
        "5Z4VJ",
        SlotParity::Odd,
        ft8_20m(),
        &cts,
        now,
    ));
}

#[test]
fn dx_parity_conflict_false_when_entry_older_than_freshness_window() {
    let cts = pancetta_qso::CrossTimeState::empty();
    let decoded_at = std::time::SystemTime::now() - std::time::Duration::from_secs(45);
    cts.a7_recent_calls
        .write()
        .unwrap()
        .record(a7_call("5Z4VJ", 1, decoded_at));

    // Same-parity evidence exists but it's stale relative to OUR
    // freshness window — must not block a genuinely fine transmission.
    assert!(!super::dx_parity_conflict(
        "5Z4VJ",
        SlotParity::Odd,
        ft8_20m(),
        &cts,
        std::time::SystemTime::now(),
    ));
}

#[test]
fn dx_parity_conflict_false_when_no_entry() {
    let cts = pancetta_qso::CrossTimeState::empty();
    assert!(!super::dx_parity_conflict(
        "UNKNOWN",
        SlotParity::Odd,
        ft8_20m(),
        &cts,
        std::time::SystemTime::now(),
    ));
}

#[test]
fn dx_parity_conflict_fails_open_on_poisoned_lock() {
    let cts = pancetta_qso::CrossTimeState::empty();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _g = cts.a7_recent_calls.write().unwrap();
        panic!("poison");
    }));
    assert!(cts.a7_recent_calls.is_poisoned());
    assert!(!super::dx_parity_conflict(
        "5Z4VJ",
        SlotParity::Odd,
        ft8_20m(),
        &cts,
        std::time::SystemTime::now(),
    ));
}

#[test]
fn dx_parity_freshness_window_scales_with_slot_period() {
    assert_eq!(
        super::dx_parity_freshness_window(SLOT_NS),
        std::time::Duration::from_secs(30)
    );
    assert_eq!(
        super::dx_parity_freshness_window(FT4_SLOT_NS),
        std::time::Duration::from_millis(15_000)
    );
}

// --- PAN-146: DX-parity evidence is scoped to band and slot grid ---

/// The ticket's scenario: 5Z4VJ heard on 20 m on Odd; the operator
/// switches to 15 m while idle and calls 5Z4VJ there, keying Odd. The
/// 20 m observation says nothing about 15 m — proceed.
#[test]
fn dx_parity_conflict_ignores_entry_heard_on_another_band() {
    let cts = pancetta_qso::CrossTimeState::empty();
    let now = std::time::SystemTime::now();
    cts.a7_recent_calls
        .write()
        .unwrap()
        .record(a7_call_in("5Z4VJ", 1, now, DIAL_20M, SLOT_NS));
    let on_15m = super::DxParityContext {
        dial_hz: DIAL_15M,
        slot_ns: SLOT_NS,
    };
    assert!(!super::dx_parity_conflict(
        "5Z4VJ",
        SlotParity::Odd,
        on_15m,
        &cts,
        now
    ));
    // Control: the same entry still holds on its own band.
    assert!(super::dx_parity_conflict(
        "5Z4VJ",
        SlotParity::Odd,
        ft8_20m(),
        &cts,
        now
    ));
}

/// FT8 and FT4 parities describe different slot boundaries: an FT8-grid
/// Odd is not evidence against an FT4-grid Odd key-up on the same band.
#[test]
fn dx_parity_conflict_ignores_entry_from_another_slot_grid() {
    let cts = pancetta_qso::CrossTimeState::empty();
    let now = std::time::SystemTime::now();
    cts.a7_recent_calls
        .write()
        .unwrap()
        .record(a7_call_in("5Z4VJ", 1, now, DIAL_20M, SLOT_NS));
    let ft4_20m = super::DxParityContext {
        dial_hz: DIAL_20M,
        slot_ns: FT4_SLOT_NS,
    };
    assert!(!super::dx_parity_conflict(
        "5Z4VJ",
        SlotParity::Odd,
        ft4_20m,
        &cts,
        now
    ));
}

/// Band granularity, not exact dial: an entry keeps the exact dial its
/// window closed on (PAN-67), so an intra-band retune must still count.
#[test]
fn dx_parity_conflict_still_holds_after_intra_band_retune() {
    let cts = pancetta_qso::CrossTimeState::empty();
    let now = std::time::SystemTime::now();
    cts.a7_recent_calls
        .write()
        .unwrap()
        .record(a7_call_in("5Z4VJ", 1, now, DIAL_20M, SLOT_NS));
    let retuned = super::DxParityContext {
        dial_hz: 14_090_000,
        slot_ns: SLOT_NS,
    };
    assert!(super::dx_parity_conflict(
        "5Z4VJ",
        SlotParity::Odd,
        retuned,
        &cts,
        now
    ));
}

/// With the dial unknown (0) on either side there is no band switch to
/// detect, so the band check is skipped and the PAN-141 hold still
/// works (e.g. a station with no CAT dial, or decodes from before the
/// first rig poll). The slot-grid check still applies.
#[test]
fn dx_parity_conflict_skips_band_check_when_a_dial_is_unknown() {
    let now = std::time::SystemTime::now();
    for (entry_dial, live_dial) in [(0, 0), (0, DIAL_15M), (DIAL_20M, 0)] {
        let cts = pancetta_qso::CrossTimeState::empty();
        cts.a7_recent_calls
            .write()
            .unwrap()
            .record(a7_call_in("5Z4VJ", 1, now, entry_dial, SLOT_NS));
        let live = super::DxParityContext {
            dial_hz: live_dial,
            slot_ns: SLOT_NS,
        };
        assert!(
            super::dx_parity_conflict("5Z4VJ", SlotParity::Odd, live, &cts, now),
            "entry dial {entry_dial}, live dial {live_dial} must still hold"
        );
        let ft4 = super::DxParityContext {
            dial_hz: live_dial,
            slot_ns: FT4_SLOT_NS,
        };
        assert!(!super::dx_parity_conflict(
            "5Z4VJ",
            SlotParity::Odd,
            ft4,
            &cts,
            now
        ));
    }
}

/// The context filter runs before the newest-entry pick: a newer alias
/// heard on another band must not hide the in-context entry.
#[test]
fn dx_parity_conflict_newest_pick_is_among_in_context_entries() {
    let cts = pancetta_qso::CrossTimeState::empty();
    let now = std::time::SystemTime::now();
    let older = now - std::time::Duration::from_secs(20);
    cts.a7_recent_calls.write().unwrap().record(a7_call_in(
        "EA8/G8BCG",
        1,
        older,
        DIAL_20M,
        SLOT_NS,
    ));
    cts.a7_recent_calls
        .write()
        .unwrap()
        .record(a7_call_in("G8BCG", 0, now, DIAL_15M, SLOT_NS));
    assert!(super::dx_parity_conflict(
        "G8BCG",
        SlotParity::Odd,
        ft8_20m(),
        &cts,
        now
    ));
}

/// End to end through the async resolver with the live dial atom: the
/// PAN-146 repro (20 m entry, operator now on 15 m) proceeds; the same
/// call on 20 m still holds.
#[tokio::test]
async fn dx_parity_conflict_for_qso_scopes_to_live_dial() {
    let manager = pancetta_qso::QsoManager::new(test_qso_manager_config());
    let qso_id = manager
        .respond_to_cq_manual("5Z4VJ".to_string(), 1500.0, Some(SlotParity::Even))
        .await
        .unwrap();
    let cts = pancetta_qso::CrossTimeState::empty();
    cts.a7_recent_calls.write().unwrap().record(a7_call_in(
        "5Z4VJ",
        1,
        std::time::SystemTime::now(),
        DIAL_20M,
        SLOT_NS,
    ));
    let (_tx, rx) = tokio::sync::watch::channel(Some(manager));
    let dial = std::sync::atomic::AtomicU64::new(DIAL_15M);
    assert!(
        !super::dx_parity_conflict_for_qso(
            Some(&qso_id.to_string()),
            SlotParity::Odd,
            &dial,
            SLOT_NS,
            &rx,
            &cts,
        )
        .await
    );
    dial.store(DIAL_20M, Ordering::Relaxed);
    assert!(
        super::dx_parity_conflict_for_qso(
            Some(&qso_id.to_string()),
            SlotParity::Odd,
            &dial,
            SLOT_NS,
            &rx,
            &cts,
        )
        .await
    );
}

/// Step 4d uses the same context rule as Step 4b.
#[test]
fn final_single_tx_gate_denial_ignores_out_of_context_entry() {
    use std::collections::HashSet;
    use std::sync::{Arc, RwLock};
    let active: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
    active
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key("qso-kenya"));
    let cts = pancetta_qso::CrossTimeState::empty();
    cts.a7_recent_calls.write().unwrap().record(a7_call_in(
        "5Z4VJ",
        1,
        std::time::SystemTime::now(),
        DIAL_20M,
        SLOT_NS,
    ));
    let on_15m = super::DxParityContext {
        dial_hz: DIAL_15M,
        slot_ns: SLOT_NS,
    };
    assert!(super::final_single_tx_gate_denial(
        Some("qso-kenya"),
        SlotParity::Odd,
        on_15m,
        &active,
        Some("5Z4VJ"),
        &cts,
    )
    .is_none());
    assert_eq!(
        super::final_single_tx_gate_denial(
            Some("qso-kenya"),
            SlotParity::Odd,
            ft8_20m(),
            &active,
            Some("5Z4VJ"),
            &cts,
        ),
        Some(super::FinalGateDenial::DxParityConflict)
    );
}

/// Multi-TX: only the item whose DX was observed in the bundle's
/// context is held.
#[test]
fn bundle_live_mask_holds_only_items_observed_in_context() {
    use std::collections::HashSet;
    use std::sync::{Arc, RwLock};
    let active: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
    active
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key("qso-kenya"));
    active
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key("qso-spain"));
    let cts = pancetta_qso::CrossTimeState::empty();
    let now = std::time::SystemTime::now();
    cts.a7_recent_calls
        .write()
        .unwrap()
        .record(a7_call_in("5Z4VJ", 1, now, DIAL_20M, SLOT_NS));
    cts.a7_recent_calls.write().unwrap().record(a7_call_in(
        "EA8BCG",
        1,
        now,
        DIAL_20M,
        FT4_SLOT_NS,
    ));
    let ids = vec![Some("qso-kenya".to_string()), Some("qso-spain".to_string())];
    let callsigns = vec![Some("5Z4VJ".to_string()), Some("EA8BCG".to_string())];
    let (live, held) =
        super::bundle_live_mask(&ids, &callsigns, SlotParity::Odd, ft8_20m(), &active, &cts);
    assert_eq!(held, vec![true, false]);
    assert_eq!(live, vec![false, true]);
}

fn test_qso_manager_config() -> pancetta_qso::QsoManagerConfig {
    pancetta_qso::QsoManagerConfig {
        our_callsign: "W5AU".to_string(),
        ..Default::default()
    }
}

/// End-to-end reproduction of the PAN-141 pileup-collision scenario at
/// the async resolver level: a DX (5Z4VJ) is decoded working N3UL on
/// Even, we manually call them (latching `tx_parity = Odd`), then 5Z4VJ
/// is decoded switching to a DIFFERENT caller (NV1U) on Odd — the exact
/// parity we're about to key into — inside the freshness window. The
/// resolver must report a conflict without ever touching `tx_parity`.
#[tokio::test]
async fn dx_parity_conflict_for_qso_reproduces_pan_141_pileup_collision() {
    let manager = pancetta_qso::QsoManager::new(test_qso_manager_config());
    let qso_id = manager
        .respond_to_cq_manual("5Z4VJ".to_string(), 1500.0, Some(SlotParity::Even))
        .await
        .unwrap();
    let before = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(before.metadata.tx_parity, Some(SlotParity::Odd));

    let cts = pancetta_qso::CrossTimeState::empty();
    let now = std::time::SystemTime::now();
    // 5Z4VJ decoded moving on to NV1U, now on Odd — our exact target slot.
    cts.a7_recent_calls
        .write()
        .unwrap()
        .record(a7_call("5Z4VJ", 1, now));

    let (_tx, rx) = tokio::sync::watch::channel(Some(manager.clone()));
    assert!(
        super::dx_parity_conflict_for_qso(
            Some(&qso_id.to_string()),
            SlotParity::Odd,
            &std::sync::atomic::AtomicU64::new(DIAL_20M),
            SLOT_NS,
            &rx,
            &cts,
        )
        .await,
        "must detect the collision: DX just observed on the exact parity we're about to key"
    );

    // tx_parity must be completely untouched by the check.
    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(after.metadata.tx_parity, Some(SlotParity::Odd));
}

/// Control case: the same setup, but 5Z4VJ's second decode has aged past
/// the freshness window — must NOT hold (this must not become a blanket
/// "never call a pileup DX" gate).
#[tokio::test]
async fn dx_parity_conflict_for_qso_proceeds_when_second_decode_is_stale() {
    let manager = pancetta_qso::QsoManager::new(test_qso_manager_config());
    let qso_id = manager
        .respond_to_cq_manual("5Z4VJ".to_string(), 1500.0, Some(SlotParity::Even))
        .await
        .unwrap();

    let cts = pancetta_qso::CrossTimeState::empty();
    let decoded_at = std::time::SystemTime::now() - std::time::Duration::from_secs(45);
    cts.a7_recent_calls
        .write()
        .unwrap()
        .record(a7_call("5Z4VJ", 1, decoded_at));

    let (_tx, rx) = tokio::sync::watch::channel(Some(manager));
    assert!(
        !super::dx_parity_conflict_for_qso(
            Some(&qso_id.to_string()),
            SlotParity::Odd,
            &std::sync::atomic::AtomicU64::new(DIAL_20M),
            SLOT_NS,
            &rx,
            &cts,
        )
        .await
    );
}

/// No `qso_id` (manual/tune send) and no live `QsoManager` (Qso component
/// not up) both fail open — this gate only ever fires for a specific,
/// currently-resolvable target.
#[tokio::test]
async fn dx_parity_conflict_for_qso_fails_open_without_target_or_manager() {
    let cts = pancetta_qso::CrossTimeState::empty();
    let (_tx, rx) = tokio::sync::watch::channel(None);
    assert!(
        !super::dx_parity_conflict_for_qso(
            None,
            SlotParity::Odd,
            &std::sync::atomic::AtomicU64::new(DIAL_20M),
            SLOT_NS,
            &rx,
            &cts,
        )
        .await
    );
    assert!(
        !super::dx_parity_conflict_for_qso(
            Some("00000000-0000-0000-0000-000000000000"),
            SlotParity::Odd,
            &std::sync::atomic::AtomicU64::new(DIAL_20M),
            SLOT_NS,
            &rx,
            &cts,
        )
        .await
    );
}

// --- PAN-148: final atomic pre-PTT gate ---

/// Sanity: a live QSO with no colliding DX-parity evidence and no
/// remote-arm involvement (Local origin) always proceeds.
#[test]
fn final_single_tx_gate_denial_none_when_live_and_no_conflict() {
    use std::collections::HashSet;
    use std::sync::{Arc, RwLock};
    let active: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
    active
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key("qso-peru"));
    let cts = pancetta_qso::CrossTimeState::empty();

    assert!(super::final_single_tx_gate_denial(
        Some("qso-peru"),
        SlotParity::Odd,
        ft8_20m(),
        &active,
        None,
        &cts,
    )
    .is_none());
}

/// PAN-148 (Finding 2, PR #370 round 4): pins the actual race. Step
/// 4b's original liveness check happens BEFORE Step 4c's
/// pivot/remodulation and Step 5's status-announce awaits; if the QSO
/// ends during those awaits, only a SECOND check run afterward (this
/// gate) can catch it. Modeled here as two calls to the same gate
/// function against a set mutated in between — the first call matches
/// what Step 4b already saw (live, so it proceeded past that check);
/// the second call is what Step 4d now additionally runs immediately
/// before `SetPtt`, and must reflect the QSO having ended in the
/// interim.
#[test]
fn final_single_tx_gate_denial_catches_cancellation_landing_after_step_4b() {
    use std::collections::HashSet;
    use std::sync::{Arc, RwLock};
    let active: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
    active
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key("qso-peru"));
    let cts = pancetta_qso::CrossTimeState::empty();

    // Step 4b's moment: still live.
    assert!(
        super::final_single_tx_gate_denial(
            Some("qso-peru"),
            SlotParity::Odd,
            ft8_20m(),
            &active,
            None,
            &cts,
        )
        .is_none(),
        "Step 4b's check must see the QSO as live"
    );

    // Simulates the QSO ending during Step 4c/5's awaits, e.g. the Qso
    // component removing it from `active_tx_qsos` on completion/cancel.
    active
        .write()
        .unwrap()
        .remove(&super::super::active_tx_qso_key("qso-peru"));

    // Step 4d's moment, immediately before SetPtt: must now deny.
    assert_eq!(
        super::final_single_tx_gate_denial(
            Some("qso-peru"),
            SlotParity::Odd,
            ft8_20m(),
            &active,
            None,
            &cts,
        ),
        Some(super::FinalGateDenial::StaleQso),
        "Step 4d's final check must catch a cancellation Step 4b could not have seen"
    );
}

/// Same race shape as above, for the DX-parity half of the gate: a
/// fresh colliding `a7_recent_calls` entry lands (a decode arriving
/// during Step 4c/5's awaits) between the two checks. `their_callsign`
/// is passed in resolved (as the real Step 4d call site now does,
/// resolved before Step 3's trim) — only `a7_recent_calls` (read
/// synchronously inside this gate) changes between the two calls.
#[tokio::test]
async fn final_single_tx_gate_denial_catches_dx_parity_conflict_landing_after_step_4b() {
    use std::collections::HashSet;
    use std::sync::{Arc, RwLock};
    let manager = pancetta_qso::QsoManager::new(test_qso_manager_config());
    let qso_id = manager
        .respond_to_cq_manual("5Z4VJ".to_string(), 1500.0, Some(SlotParity::Even))
        .await
        .unwrap();
    let active: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
    active
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key(&qso_id.to_string()));
    let cts = pancetta_qso::CrossTimeState::empty();
    let (_tx, rx) = tokio::sync::watch::channel(Some(manager));
    let their_callsign = super::resolve_their_callsign(Some(&qso_id.to_string()), &rx).await;
    assert_eq!(their_callsign.as_deref(), Some("5Z4VJ"));

    // Step 4b's moment: no fresh colliding decode yet.
    assert!(
        super::final_single_tx_gate_denial(
            Some(&qso_id.to_string()),
            SlotParity::Odd,
            ft8_20m(),
            &active,
            their_callsign.as_deref(),
            &cts,
        )
        .is_none(),
        "Step 4b's check must see no conflict yet"
    );

    // A decode lands during Step 4c/5's awaits: 5Z4VJ observed on Odd,
    // the exact parity we're about to key into.
    cts.a7_recent_calls.write().unwrap().record(a7_call(
        "5Z4VJ",
        1, /* Odd */
        std::time::SystemTime::now(),
    ));

    assert_eq!(
        super::final_single_tx_gate_denial(
            Some(&qso_id.to_string()),
            SlotParity::Odd,
            ft8_20m(),
            &active,
            their_callsign.as_deref(),
            &cts,
        ),
        Some(super::FinalGateDenial::DxParityConflict),
        "Step 4d's final check must catch a collision decode Step 4b could not have seen"
    );
}

/// PAN-117: the key-time slot-timing gate must deny a frame that is
/// only ready to key several seconds past its slot boundary — the
/// ticket's gherkin scenario — carrying the elapsed time and cap for
/// the denial's log/diagnostic. An on-time key, a key exactly at the
/// cap, and a deferred (future) target deny nothing.
#[test]
fn key_time_gate_denial_flags_key_time_late_frame() {
    let target_slot = at(30.0);
    assert_eq!(
        super::key_time_gate_denial(
            target_slot + SignedDuration::from_secs(9),
            target_slot,
            3000
        ),
        Some(super::FinalGateDenial::TooLateToDecode {
            elapsed_ms: 9000,
            late_max_ms: 3000,
        }),
        "a frame only ready to key 9s into its slot must be refused, not keyed"
    );
    assert_eq!(
        super::key_time_gate_denial(
            target_slot + SignedDuration::from_millis(3001),
            target_slot,
            3000
        ),
        Some(super::FinalGateDenial::TooLateToDecode {
            elapsed_ms: 3001,
            late_max_ms: 3000,
        }),
    );
    assert!(super::key_time_gate_denial(target_slot, target_slot, 3000).is_none());
    assert!(super::key_time_gate_denial(
        target_slot + SignedDuration::from_millis(3000),
        target_slot,
        3000
    )
    .is_none());
    assert!(super::key_time_gate_denial(at(29.92), target_slot, 3000).is_none());
}

/// PAN-117 (PR #450 Codex review): the liveness/DX-parity gate no
/// longer carries the slot-timing check — that reads the clock, so it
/// runs separately as the last check before `SetPtt`. A live QSO with
/// no conflict clears this gate however late the frame is; only
/// `key_time_gate_denial` refuses it.
#[test]
fn final_single_tx_gate_denial_has_no_clock_dependency() {
    use std::collections::HashSet;
    use std::sync::{Arc, RwLock};
    let active: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
    active
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key("qso-peru"));
    let cts = pancetta_qso::CrossTimeState::empty();
    assert!(super::final_single_tx_gate_denial(
        Some("qso-peru"),
        SlotParity::Odd,
        ft8_20m(),
        &active,
        None,
        &cts,
    )
    .is_none());
    assert_eq!(
        super::final_gate_denial_text(
            super::FinalGateDenial::TooLateToDecode {
                elapsed_ms: 9000,
                late_max_ms: 3000,
            },
            Some("qso-peru"),
            SlotParity::Odd,
            "K1ABC W5AU R-05",
        )
        .1,
        "dropping stale TX at final pre-PTT check: 9000ms into the slot exceeds tx_late_max_ms=3000ms — no receiver could decode it: 'K1ABC W5AU R-05'"
    );
}

/// `bundle_live_mask` must reproduce the pre-refactor inline
/// computation's semantics exactly (this refactor's only job): all
/// live, all held, and mixed cases. `callsigns` is empty (no
/// established partner) — irrelevant to the liveness half under test.
#[test]
fn bundle_live_mask_matches_established_semantics() {
    use std::collections::HashSet;
    use std::sync::{Arc, RwLock};
    let active: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
    active
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key("qso-peru"));
    active
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key("qso-kenya"));
    let cts = pancetta_qso::CrossTimeState::empty();
    let ids = vec![Some("qso-peru".to_string()), Some("qso-kenya".to_string())];
    let callsigns: Vec<Option<String>> = vec![None, None];

    let (live, held) =
        super::bundle_live_mask(&ids, &callsigns, SlotParity::Odd, ft8_20m(), &active, &cts);
    assert_eq!(live, vec![true, true]);
    assert_eq!(held, vec![false, false]);

    active
        .write()
        .unwrap()
        .remove(&super::super::active_tx_qso_key("qso-kenya"));
    let (live, held) =
        super::bundle_live_mask(&ids, &callsigns, SlotParity::Odd, ft8_20m(), &active, &cts);
    assert_eq!(live, vec![true, false]);
    assert_eq!(held, vec![false, false]);
}

/// PAN-148 (Finding 1, PR #370 round 2): pins the multi-TX race. Step
/// 4b computes `live_mask` once; the partial-staleness branch's
/// per-item diagnostic/`TransmitComplete` awaits (and Step 5's
/// status-announce awaits) run AFTER that, before `SetPtt`. Modeled as
/// two calls to `bundle_live_mask` against a set mutated in between —
/// the first matches what Step 4b saw (both survivors live), the
/// second is what Step 4d now re-checks immediately before keying, and
/// must reflect a survivor ending in the interim.
#[test]
fn bundle_live_mask_catches_survivor_cancellation_landing_after_step_4b() {
    use std::collections::HashSet;
    use std::sync::{Arc, RwLock};
    let active: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
    active
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key("qso-peru"));
    active
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key("qso-kenya"));
    let cts = pancetta_qso::CrossTimeState::empty();
    // Bundle survivors as re-encoded by Step 4b's own rebuild — both
    // still live at that point.
    let survivors = vec![Some("qso-peru".to_string()), Some("qso-kenya".to_string())];
    let callsigns: Vec<Option<String>> = vec![None, None];

    let (step_4b_mask, _) = super::bundle_live_mask(
        &survivors,
        &callsigns,
        SlotParity::Odd,
        ft8_20m(),
        &active,
        &cts,
    );
    assert_eq!(
        step_4b_mask,
        vec![true, true],
        "Step 4b's check must see both survivors as live"
    );

    // qso-kenya ends during the partial-staleness branch's diagnostic
    // awaits (or Step 5's status-announce awaits).
    active
        .write()
        .unwrap()
        .remove(&super::super::active_tx_qso_key("qso-kenya"));

    let (step_4d_mask, _) = super::bundle_live_mask(
        &survivors,
        &callsigns,
        SlotParity::Odd,
        ft8_20m(),
        &active,
        &cts,
    );
    assert_eq!(
        step_4d_mask,
        vec![true, false],
        "Step 4d's final check must catch a survivor ending after Step 4b's own check"
    );
}

/// `resolve_callsigns` is the batch form of `resolve_their_callsign` —
/// order-preserving, one entry per input id.
#[tokio::test]
async fn resolve_callsigns_preserves_order_and_resolves_each_id() {
    let manager = pancetta_qso::QsoManager::new(test_qso_manager_config());
    let qso_id = manager
        .respond_to_cq_manual("5Z4VJ".to_string(), 1500.0, Some(SlotParity::Even))
        .await
        .unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(Some(manager));
    let ids = vec![None, Some(qso_id.to_string())];

    let resolved = super::resolve_callsigns(&ids, &rx).await;
    assert_eq!(resolved, vec![None, Some("5Z4VJ".to_string())]);
}

#[test]
fn pad_and_cursor_for_target_matches_schedule_tx_for_same_now() {
    // Sanity: calling the extracted helper with schedule_tx's own chosen
    // target and "now" must reproduce schedule_tx's own pad/cursor exactly
    // — this is what makes the Task 1 extraction provably behavior-neutral.
    let now = at(5.0);
    let s = schedule_tx(now, SlotParity::Odd, 8000, 12_000, SLOT_NS);
    let (pad, cursor) = pad_and_cursor_for_target(now, s.target_slot, 12_000);
    assert_eq!(pad, s.silent_pad_samples);
    assert_eq!(cursor, s.cursor_offset_samples);
}

#[test]
fn pad_and_cursor_for_target_refreshes_against_a_later_now() {
    // The key new behavior Tasks 3/4 rely on: given the SAME target_slot,
    // a later "now" produces a LARGER cursor (more of the waveform's front
    // trimmed) — because more real time has passed relative to the slot
    // boundary, independent of when target_slot was originally decided.
    let target = at(0.0); // slot boundary itself
    let (pad_early, cursor_early) = pad_and_cursor_for_target(at(0.2), target, 12_000);
    let (pad_late, cursor_late) = pad_and_cursor_for_target(at(3.0), target, 12_000);
    assert!(
        pad_early > 0,
        "200ms in: still inside the DELAY_MS pre-roll, expect padding"
    );
    assert_eq!(pad_late, 0, "3s in: past DELAY_MS, expect no padding");
    assert!(
        cursor_late > cursor_early,
        "later refresh must trim more of the waveform's front"
    );
}

#[test]
fn pad_and_cursor_for_target_stable_within_delay_ms_window() {
    // Two "now" reads a few ms apart, both still inside the DELAY_MS
    // pre-roll, should both land in the padding branch (cursor == 0) —
    // confirms there's no discontinuity right at the DELAY_MS boundary.
    let target = at(10.0);
    let (_, cursor_a) = pad_and_cursor_for_target(at(10.1), target, 12_000);
    let (_, cursor_b) = pad_and_cursor_for_target(at(10.3), target, 12_000);
    assert_eq!(cursor_a, 0);
    assert_eq!(cursor_b, 0);
}

fn tx_request(tx_parity: Option<SlotParity>) -> MessageType {
    MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "CQ TEST".to_string(),
        frequency_offset: 1500.0,
        qso_id: None,
        tx_parity,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    }
}

#[test]
fn adaptive_cap_shrinks_for_a_late_arriving_head() {
    // Arrives 6.0s into an 8000ms tx_late_max_ms budget: the base
    // window (800ms) + safety margin (500ms) eat 1300ms of it, leaving
    // 700ms of extension headroom. (PAN-117 PR review: arrivals later
    // than cap − 1300ms — e.g. 7.5s here — now resolve DEFERRED at the
    // queue-time acceptance check, so their headroom is measured against
    // that deferred slot's own start instead; see
    // current_slot_acceptance_reserves_the_base_coalesce_wait.)
    // at(6.0) falls inside slot 0, which is Even (base timestamp
    // 1767225600 / 15 % 2 == 0, per `at()`'s own doc comment) — so the
    // request must target Even parity for this to be a same-slot late
    // arrival (`use_current = true`) rather than a parity-mismatch
    // defer to the next Odd slot, whose headroom would be measured
    // against that later slot instead of the late same-slot arrival
    // this test is for.
    use pancetta_config::station::TxSelfParity;
    let head = tx_request(Some(SlotParity::Even));
    let cap = adaptive_coalesce_cap_ms(
        &head,
        at(6.0),
        TxSelfParity::Auto,
        8000,
        12_000,
        SLOT_NS,
        pancetta_ft8::Protocol::Ft8,
    );
    assert_eq!(cap, 700);
}

#[test]
fn adaptive_cap_has_room_for_an_early_arriving_head() {
    // Arrives 1s into the slot: 7000ms of raw headroom before tx_late_max_ms,
    // well above the protocol ceiling — cap should be the full FT8 ceiling
    // (3000ms), not the raw headroom. at(1.0) falls inside slot 0, which is
    // Even (base timestamp 1767225600 / 15 % 2 == 0, per `at()`'s own doc
    // comment) — so the request must target Even parity for this to be a
    // same-slot early arrival (`use_current = true`, `deferred = false`)
    // whose `headroom.min(protocol_ceiling)` is measured against the
    // current slot, rather than parity-mismatch deferring to the next Odd
    // slot and measuring the headroom from that slot's later start.
    use pancetta_config::station::TxSelfParity;
    let head = tx_request(Some(SlotParity::Even));
    let cap = adaptive_coalesce_cap_ms(
        &head,
        at(1.0),
        TxSelfParity::Auto,
        8000,
        12_000,
        SLOT_NS,
        pancetta_ft8::Protocol::Ft8,
    );
    assert_eq!(cap, 3000);
}

#[test]
fn adaptive_cap_uses_protocol_ceiling_for_ft4() {
    // Same early-arrival case, but FT4's cycle is half FT8's — the ceiling
    // should scale down proportionally (1500ms), not stay at FT8's 3000ms.
    // at(0.5) on FT4's 7.5s slot grid is also slot 0 = Even, so — same
    // reasoning as adaptive_cap_has_room_for_an_early_arriving_head above
    // — the request must target Even parity to hit `use_current = true`
    // / `deferred = false`, so `headroom.min(protocol_ceiling)` is
    // measured against the current slot instead of the next Odd slot.
    use pancetta_config::station::TxSelfParity;
    let head = tx_request(Some(SlotParity::Even));
    let cap = adaptive_coalesce_cap_ms(
        &head,
        at(0.5),
        TxSelfParity::Auto,
        8000,
        12_000,
        FT4_SLOT_NS,
        pancetta_ft8::Protocol::Ft4,
    );
    assert_eq!(cap, 1500);
}

#[test]
fn adaptive_cap_full_ceiling_when_already_deferred() {
    // A head that's already past tx_late_max_ms for the current slot
    // defers to the next Odd slot, which begins 16s later at :45. Its
    // headroom against that slot's own cap is far above the protocol
    // ceiling, so the cap is the ceiling. Deferral alone does not grant
    // it: a deferred slot that begins moments away is still bounded
    // (adaptive_cap_bounds_a_deferred_head_by_the_imminent_target_slot).
    use pancetta_config::station::TxSelfParity;
    let head = tx_request(Some(SlotParity::Odd));
    let cap = adaptive_coalesce_cap_ms(
        &head,
        at(29.0), // >8000ms into slot 1 (Odd), forces defer
        TxSelfParity::Auto,
        8000,
        12_000,
        SLOT_NS,
        pancetta_ft8::Protocol::Ft8,
    );
    assert_eq!(cap, 3000);
}

#[test]
fn adaptive_cap_subtracts_the_base_window() {
    // PAN-117: slot-start head under the new 3000ms default — the
    // extension must leave room for the 800ms base window + 500ms
    // margin → 1700ms, not 2500ms. (Queue-time stretch must never end
    // past the cap the key-time gate enforces.) at(0.0) is slot 0,
    // which is Even (per `at()`'s doc comment), so an Even-targeting
    // head is a same-slot arrival exercising the headroom math.
    use pancetta_config::station::TxSelfParity;
    let head = tx_request(Some(SlotParity::Even));
    let cap = adaptive_coalesce_cap_ms(
        &head,
        at(0.0),
        TxSelfParity::Auto,
        3000,
        12_000,
        SLOT_NS,
        pancetta_ft8::Protocol::Ft8,
    );
    assert_eq!(cap, 1700);
    // FT4: effective cap 1500, base window 400, margin 500 → 600.
    let cap = adaptive_coalesce_cap_ms(
        &head,
        at(0.0),
        TxSelfParity::Auto,
        3000,
        12_000,
        FT4_SLOT_NS,
        pancetta_ft8::Protocol::Ft4,
    );
    assert_eq!(cap, 600);
}

#[test]
fn adaptive_cap_bounds_a_deferred_head_by_the_imminent_target_slot() {
    // PAN-117 (PR #450 Codex review): a head arriving at :14.5 in the
    // Even slot 0 but requiring Odd parity defers to slot 1, which
    // begins only 500ms later at :15.0. The full 3000ms ceiling would
    // let base (800) + extension (3000) run to :18.3 before encode,
    // past the 3000ms key-time cap, so the final gate would drop it.
    // Headroom must be measured against the deferred target too:
    // 3000 + 500 (time until :15.0) - 800 - 500 = 2200ms.
    use pancetta_config::station::TxSelfParity;
    let head = tx_request(Some(SlotParity::Odd));
    let cap = adaptive_coalesce_cap_ms(
        &head,
        at(14.5),
        TxSelfParity::Auto,
        3000,
        12_000,
        SLOT_NS,
        pancetta_ft8::Protocol::Ft8,
    );
    assert_eq!(cap, 2200);
    // 100ms before the boundary: 3000 + 100 - 1300 = 1800.
    let cap = adaptive_coalesce_cap_ms(
        &head,
        at(14.9),
        TxSelfParity::Auto,
        3000,
        12_000,
        SLOT_NS,
        pancetta_ft8::Protocol::Ft8,
    );
    assert_eq!(cap, 1800);
    // A deferred target a whole slot or more away keeps the ceiling.
    let cap = adaptive_coalesce_cap_ms(
        &head,
        at(1.0),
        TxSelfParity::Auto,
        3000,
        12_000,
        SLOT_NS,
        pancetta_ft8::Protocol::Ft8,
    );
    assert_eq!(cap, 3000);
    // FT4 (7.5s slots, Odd slot 1 at 7.5s): effective cap 1500,
    // base 400, margin 500, 300ms until the boundary → 900.
    let cap = adaptive_coalesce_cap_ms(
        &head,
        at(7.2),
        TxSelfParity::Auto,
        3000,
        12_000,
        FT4_SLOT_NS,
        pancetta_ft8::Protocol::Ft4,
    );
    assert_eq!(cap, 900);
}

#[test]
fn adaptive_cap_zero_for_non_transmit_request() {
    use pancetta_config::station::TxSelfParity;
    let head = MessageType::MultiTransmitRequest {
        items: Vec::new(),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    let cap = adaptive_coalesce_cap_ms(
        &head,
        at(1.0),
        TxSelfParity::Auto,
        8000,
        12_000,
        SLOT_NS,
        pancetta_ft8::Protocol::Ft8,
    );
    assert_eq!(cap, 0);
}

// --- PAN-114: wall-clock pre-PTT wait and the Step 6 PTT-hold bound ---

/// Default `ptt_lead_ms` (`pancetta-config/src/station.rs`).
const PTT_LEAD_MS: i64 = 80;

/// `(ptt_target, target_slot)` for an FT8 frame keyed at `slot_s`.
fn ptt_and_slot(slot_s: f64) -> (UtcTimestamp, UtcTimestamp) {
    let target_slot = at(slot_s);
    (
        target_slot - SignedDuration::from_millis(PTT_LEAD_MS),
        target_slot,
    )
}

#[test]
fn ptt_wait_sleeps_in_chunks_until_the_target() {
    let (ptt_target, target_slot) = ptt_and_slot(30.0);
    let far = ptt_target - SignedDuration::from_secs(20);
    assert_eq!(
        ptt_wait_step(far, ptt_target, target_slot, true, 8000, SLOT_NS),
        PttWaitStep::Sleep(Duration::from_millis(50))
    );
    let near = ptt_target - SignedDuration::from_millis(30);
    assert_eq!(
        ptt_wait_step(near, ptt_target, target_slot, true, 8000, SLOT_NS),
        PttWaitStep::Sleep(Duration::from_millis(30))
    );
}

#[test]
fn ptt_wait_reaches_on_time() {
    let (ptt_target, target_slot) = ptt_and_slot(30.0);
    assert_eq!(
        ptt_wait_step(ptt_target, ptt_target, target_slot, true, 8000, SLOT_NS),
        PttWaitStep::Reached
    );
}

#[test]
fn ptt_wait_already_late_request_keeps_todays_path() {
    // A request that was already a late start when the wait began keeps
    // today's path, even past late_max: schedule_tx already decided it.
    let (ptt_target, target_slot) = ptt_and_slot(30.0);
    let now = target_slot + SignedDuration::from_secs(9);
    assert_eq!(
        ptt_wait_step(now, ptt_target, target_slot, false, 8000, SLOT_NS),
        PttWaitStep::Reached
    );
}

// --- PAN-117: key-time late-cap enforcement ---

#[test]
fn key_time_too_late_boundary_is_inclusive() {
    // Exactly at cap: keys (matches schedule_tx's `mstr <= tx_late_max_ms`).
    let (_, target_slot) = ptt_and_slot(30.0);
    assert!(!key_time_too_late(
        target_slot + SignedDuration::from_millis(3000),
        target_slot,
        3000
    ));
    // 1ms past cap: drops.
    assert!(key_time_too_late(
        target_slot + SignedDuration::from_millis(3001),
        target_slot,
        3000
    ));
    // Deferred target (slot in the future): never late.
    assert!(!key_time_too_late(
        target_slot - SignedDuration::from_secs(5),
        target_slot,
        3000
    ));
}

#[test]
fn key_time_too_late_covers_the_ticket_scenario() {
    // The PAN-117 gherkin: several seconds of slot elapsed, default-ish cap.
    let (_, target_slot) = ptt_and_slot(30.0);
    assert!(key_time_too_late(
        target_slot + SignedDuration::from_secs(9),
        target_slot,
        3000
    ));
}

#[test]
fn current_slot_acceptance_reserves_the_base_coalesce_wait() {
    // PAN-117 PR #450 review finding: the queue-time `use_current`
    // decision is made from `request_received_at`, frozen BEFORE the
    // unconditional base coalesce wait (800ms FT8) and the overhead the
    // safety margin (500ms) covers — while the key-time gate compares a
    // FRESH clock against the raw effective cap. A request arriving in
    // the band between the reserved overhead and the cap (2.2-3.0s at
    // the 3000ms FT8 default; 1.1-1.5s FT4) was therefore accepted into
    // the current slot and then ALWAYS dropped at the gate — never
    // deferred. The worker now decides against the acceptance cap, so
    // such a request defers 30s instead.
    use pancetta_config::station::TxSelfParity;
    assert_eq!(
        current_slot_acceptance_cap_ms(pancetta_ft8::Protocol::Ft8, 3000),
        1700,
        "FT8 default: 3000 − 800 base window − 500 margin"
    );
    assert_eq!(
        current_slot_acceptance_cap_ms(pancetta_ft8::Protocol::Ft4, 3000),
        600,
        "FT4: 1500 effective − 400 base window − 500 margin"
    );
    // Band edge: 2.3s into an Even slot (slot 0 is Even per at()'s
    // doc) is inside the raw 3000ms cap but past the acceptance cap —
    // scheduling with what the worker arms pass must DEFER, not accept
    // the current slot the key-time gate would drop.
    let s = schedule_tx(
        at(2.3),
        SlotParity::Even,
        current_slot_acceptance_cap_ms(pancetta_ft8::Protocol::Ft8, 3000),
        12_000,
        SLOT_NS,
    );
    assert!(s.deferred, "2.3s arrival must defer at the 3000ms default");
    assert_eq!(
        (s.target_slot - at(0.0)).as_millis_i64(),
        30_000,
        "deferral targets the next Even slot, 30s out"
    );
    // An early-enough arrival still makes the current slot.
    let s = schedule_tx(
        at(1.0),
        SlotParity::Even,
        current_slot_acceptance_cap_ms(pancetta_ft8::Protocol::Ft8, 3000),
        12_000,
        SLOT_NS,
    );
    assert!(!s.deferred, "a 1.0s arrival still makes the current slot");
    assert_eq!(
        (s.target_slot - at(0.0)).as_millis_i64(),
        0,
        "same-slot acceptance targets the current Even slot"
    );
    // Coherence: the adaptive-coalesce probe answers with the SAME
    // decision the worker makes, so a band head resolves DEFERRED and
    // grants the full protocol ceiling (not a saturated-zero headroom
    // against a current-slot target it will never get).
    let head = tx_request(Some(SlotParity::Even));
    let cap = adaptive_coalesce_cap_ms(
        &head,
        at(2.3),
        TxSelfParity::Auto,
        3000,
        12_000,
        SLOT_NS,
        pancetta_ft8::Protocol::Ft8,
    );
    assert_eq!(
        cap, 3000,
        "a 2.3s head resolves deferred, so extension gets the full ceiling"
    );
}

#[test]
fn multi_tx_acceptance_reserves_the_base_wait_only_for_coalesced_bundles() {
    // PAN-117 PR #450 review round 2: a `MultiTransmitRequest` dequeued
    // directly (mid-TX supersede re-enqueue, autonomous bundle) never
    // takes the base coalesce wait, so the multi-TX arm must not
    // reserve it — only the 500ms margin.
    assert_eq!(
        multi_tx_acceptance_cap_ms(pancetta_ft8::Protocol::Ft8, 3000, true),
        1700,
        "coalesced FT8 bundle: 3000 − 800 base window − 500 margin"
    );
    assert_eq!(
        multi_tx_acceptance_cap_ms(pancetta_ft8::Protocol::Ft8, 3000, false),
        2500,
        "direct FT8 bundle: 3000 − 500 margin"
    );
    assert_eq!(
        multi_tx_acceptance_cap_ms(pancetta_ft8::Protocol::Ft4, 3000, false),
        1000,
        "direct FT4 bundle: 1500 effective − 500 margin"
    );
    // A direct bundle re-key 2.0s into its Even slot keys in the
    // current slot instead of deferring 30s.
    let s = schedule_tx(
        at(2.0),
        SlotParity::Even,
        multi_tx_acceptance_cap_ms(pancetta_ft8::Protocol::Ft8, 3000, false),
        12_000,
        SLOT_NS,
    );
    assert!(!s.deferred, "a direct 2.0s bundle makes the current slot");
    assert_eq!((s.target_slot - at(0.0)).as_millis_i64(), 0);
    // A coalesced bundle frozen at the same instant already lost 800ms
    // to the base wait, so it still defers.
    let s = schedule_tx(
        at(2.0),
        SlotParity::Even,
        multi_tx_acceptance_cap_ms(pancetta_ft8::Protocol::Ft8, 3000, true),
        12_000,
        SLOT_NS,
    );
    assert!(s.deferred, "a coalesced 2.0s bundle defers");
}

#[test]
fn ptt_wait_forward_jump_within_late_policy_ships_late() {
    let (ptt_target, target_slot) = ptt_and_slot(30.0);
    let now = target_slot + SignedDuration::from_secs(5);
    assert_eq!(
        ptt_wait_step(now, ptt_target, target_slot, true, 8000, SLOT_NS),
        PttWaitStep::Reached
    );
}

#[test]
fn ptt_wait_forward_jump_past_late_policy_is_dropped() {
    let (ptt_target, target_slot) = ptt_and_slot(30.0);
    let now = target_slot + SignedDuration::from_secs(3600);
    match ptt_wait_step(now, ptt_target, target_slot, true, 8000, SLOT_NS) {
        PttWaitStep::ClockJumped(pancetta_core::slot_clock::WallClockJump::Forward(d)) => {
            assert_eq!(d, now - ptt_target);
        }
        other => panic!("expected ClockJumped(Forward), got {other:?}"),
    }
}

#[test]
fn ptt_wait_backward_step_beyond_two_slots_is_dropped() {
    let (ptt_target, target_slot) = ptt_and_slot(30.0);
    let now = ptt_target - SignedDuration::from_secs(620);
    match ptt_wait_step(now, ptt_target, target_slot, true, 8000, SLOT_NS) {
        PttWaitStep::ClockJumped(pancetta_core::slot_clock::WallClockJump::Backward(d)) => {
            assert_eq!(d, SignedDuration::from_secs(620));
        }
        other => panic!("expected ClockJumped(Backward), got {other:?}"),
    }
}

#[test]
fn ptt_wait_small_backward_step_is_waited_out() {
    // 25 s ahead is inside schedule_tx's two-slot horizon (30 s for FT8).
    let (ptt_target, target_slot) = ptt_and_slot(30.0);
    let now = ptt_target - SignedDuration::from_secs(25);
    assert_eq!(
        ptt_wait_step(now, ptt_target, target_slot, true, 8000, SLOT_NS),
        PttWaitStep::Sleep(Duration::from_millis(50))
    );
}

#[test]
fn ptt_wait_bounds_scale_with_ft4() {
    // 16 s ahead: beyond FT4's 2 × 7.5 s horizon, inside FT8's 30 s.
    let (ptt_target, target_slot) = ptt_and_slot(30.0);
    let now = ptt_target - SignedDuration::from_secs(16);
    assert!(matches!(
        ptt_wait_step(now, ptt_target, target_slot, true, 8000, FT4_SLOT_NS),
        PttWaitStep::ClockJumped(pancetta_core::slot_clock::WallClockJump::Backward(_))
    ));
    assert_eq!(
        ptt_wait_step(now, ptt_target, target_slot, true, 8000, SLOT_NS),
        PttWaitStep::Sleep(Duration::from_millis(50))
    );
}

#[test]
fn ptt_hold_bound_allows_normal_lead_and_rejects_a_step() {
    assert!(!ptt_hold_exceeds_bound(Duration::from_millis(80), 80));
    assert!(!ptt_hold_exceeds_bound(Duration::from_millis(1080), 80));
    assert!(ptt_hold_exceeds_bound(Duration::from_millis(1081), 80));
    assert!(ptt_hold_exceeds_bound(Duration::from_secs(600), 80));
}

/// Injectable wall clock for the `wait_for_ptt_instant` tests.
fn test_wall_clock(start: UtcTimestamp) -> std::sync::Arc<std::sync::Mutex<UtcTimestamp>> {
    std::sync::Arc::new(std::sync::Mutex::new(start))
}

fn shift_clock(clock: &std::sync::Arc<std::sync::Mutex<UtcTimestamp>>, by: SignedDuration) {
    let mut now = clock.lock().unwrap();
    *now += by;
}

#[tokio::test(flavor = "current_thread")]
async fn wait_for_ptt_instant_reaches_target_on_the_wall_clock() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let start_wall = at(10.0);
    let clock = test_wall_clock(start_wall);
    let ticker_clock = clock.clone();
    let ticker = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(20)).await;
            shift_clock(&ticker_clock, SignedDuration::from_millis(20));
        }
    });
    let ptt_target = start_wall + SignedDuration::from_millis(200);
    let target_slot = ptt_target + SignedDuration::from_millis(PTT_LEAD_MS);
    let reader = clock.clone();
    let started = std::time::Instant::now();
    let outcome = wait_for_ptt_instant(
        ptt_target,
        target_slot,
        8000,
        SLOT_NS,
        &shutdown,
        &abort,
        move || *reader.lock().unwrap(),
    )
    .await;
    let elapsed = started.elapsed();
    ticker.abort();
    assert_eq!(outcome, PttWait::Reached);
    assert!(
        *clock.lock().unwrap() >= ptt_target,
        "must not report Reached before the wall clock reaches the PTT instant"
    );
    assert!(
        elapsed < Duration::from_millis(400),
        "should reach a 200 ms target within 400 ms (elapsed={elapsed:?})"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn wait_for_ptt_instant_detects_a_backward_step_mid_wait() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let start_wall = at(10.0);
    let clock = test_wall_clock(start_wall);
    let stepper_clock = clock.clone();
    let stepper = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        shift_clock(&stepper_clock, -SignedDuration::from_secs(600));
    });
    let ptt_target = start_wall + SignedDuration::from_millis(200);
    let target_slot = ptt_target + SignedDuration::from_millis(PTT_LEAD_MS);
    let reader = clock.clone();
    let started = std::time::Instant::now();
    let outcome = wait_for_ptt_instant(
        ptt_target,
        target_slot,
        8000,
        SLOT_NS,
        &shutdown,
        &abort,
        move || *reader.lock().unwrap(),
    )
    .await;
    let elapsed = started.elapsed();
    let _ = stepper.await;
    match outcome {
        PttWait::ClockJumped(pancetta_core::slot_clock::WallClockJump::Backward(d)) => {
            assert!(d > SignedDuration::from_secs(600), "jump size {d:?}");
        }
        other => panic!("expected ClockJumped(Backward), got {other:?}"),
    }
    // Step at ~60 ms + at most one 50 ms chunk ≈ 110-150 ms; slack for jitter.
    assert!(
        elapsed < Duration::from_millis(250),
        "backward step must be seen within about one chunk (elapsed={elapsed:?})"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn wait_for_ptt_instant_detects_a_forward_jump_mid_wait() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let start_wall = at(10.0);
    let clock = test_wall_clock(start_wall);
    let jumper_clock = clock.clone();
    let jumper = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        shift_clock(&jumper_clock, SignedDuration::from_secs(3600));
    });
    let ptt_target = start_wall + SignedDuration::from_secs(10);
    let target_slot = ptt_target + SignedDuration::from_millis(PTT_LEAD_MS);
    let reader = clock.clone();
    let started = std::time::Instant::now();
    let outcome = wait_for_ptt_instant(
        ptt_target,
        target_slot,
        8000,
        SLOT_NS,
        &shutdown,
        &abort,
        move || *reader.lock().unwrap(),
    )
    .await;
    let elapsed = started.elapsed();
    let _ = jumper.await;
    match outcome {
        PttWait::ClockJumped(pancetta_core::slot_clock::WallClockJump::Forward(d)) => {
            assert_eq!(d, SignedDuration::from_secs(3590));
        }
        other => panic!("expected ClockJumped(Forward), got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_millis(250),
        "forward jump must be seen within about one chunk (elapsed={elapsed:?})"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn wait_for_ptt_instant_still_honours_shutdown_and_abort() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    // A frozen wall clock 10 s short of the target: only a flag can end
    // the wait.
    let start_wall = at(10.0);
    let ptt_target = start_wall + SignedDuration::from_secs(10);
    let target_slot = ptt_target + SignedDuration::from_millis(PTT_LEAD_MS);

    for flip_shutdown in [false, true] {
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let flag = if flip_shutdown {
            shutdown.clone()
        } else {
            abort.clone()
        };
        let flipper = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            flag.store(true, Ordering::Release);
        });
        let started = std::time::Instant::now();
        let outcome = wait_for_ptt_instant(
            ptt_target,
            target_slot,
            8000,
            SLOT_NS,
            &shutdown,
            &abort,
            move || start_wall,
        )
        .await;
        let elapsed = started.elapsed();
        let _ = flipper.await;
        assert_eq!(outcome, PttWait::Interrupted, "shutdown={flip_shutdown}");
        // Same ~50 ms wake-latency guarantee as `interruptible_sleep`
        // (flag flips at ~20 ms).
        assert!(
            elapsed < Duration::from_millis(200),
            "shutdown={flip_shutdown}: wake latency exceeded one chunk (elapsed={elapsed:?})"
        );
    }

    // Already-set flags return without evaluating the clock at all.
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(true));
    let outcome = wait_for_ptt_instant(
        ptt_target,
        target_slot,
        8000,
        SLOT_NS,
        &shutdown,
        &abort,
        || -> UtcTimestamp { panic!("clock read after abort") },
    )
    .await;
    assert_eq!(outcome, PttWait::Interrupted);
}
