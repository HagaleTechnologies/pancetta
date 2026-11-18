use super::*;

/// PAN-141 (Step 4b) / PAN-148 (Step 4d): per-item liveness+DX-parity mask
/// for a multi-TX bundle — SYNCHRONOUS. Extracted so both call sites —
/// Step 4b's original key-time gate and the new Step 4d final gate — run
/// the textually identical check against fresh state; Step 4d's caller
/// MUST NOT `.await` anything else between this call returning and the
/// `SetPtt{true}` send it gates.
///
/// `callsigns` (one entry per `encoded_qso_ids`, same order) must be
/// resolved SEPARATELY via [`resolve_callsigns`] — round-1 review (Codex
/// P1) found an earlier version awaiting `dx_parity_conflict_for_qso` per
/// item in one sequential loop let item 1's synchronous parity read go
/// stale while item 2..N's lookups were still pending; round-2 review
/// (Codex P1) found that even a two-pass split awaiting INSIDE this
/// function still left an unbounded `QsoManager`-lock-contention wait
/// sitting after Step 3's timing-sensitive trim when called from the Step
/// 4d final gate. Splitting the resolution out entirely — leaving this
/// function with NO internal `.await` — fixes both: callers resolve
/// `callsigns` before Step 3's trim (bounding the misalignment risk to
/// wherever that resolution already had to happen), and this function's
/// own reads (`tx_qso_is_live`, `dx_parity_conflict`, both synchronous) run
/// for every item back-to-back with no yield point between them, so
/// nothing can interleave a change for one item's evidence while another's
/// is being read.
///
/// Returns `(live_mask, parity_hold_mask)`: `live_mask` is the "should this
/// item transmit" mask every caller ultimately wants; `parity_hold_mask` is
/// returned alongside it only so a caller that reports per-item drop
/// reasons (Step 4b's partial-staleness branch) can distinguish "ended" from
/// "held for colliding DX parity" without a second pass.
pub(super) fn bundle_live_mask(
    encoded_qso_ids: &[Option<String>],
    callsigns: &[Option<String>],
    required_parity: pancetta_core::slot::SlotParity,
    context: DxParityContext,
    active_tx_qsos: &std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    cross_time_state: &pancetta_qso::CrossTimeState,
) -> (Vec<bool>, Vec<bool>) {
    let now = std::time::SystemTime::now();
    let parity_hold_mask: Vec<bool> = callsigns
        .iter()
        .map(|callsign| {
            callsign.as_deref().is_some_and(|cs| {
                dx_parity_conflict(cs, required_parity, context, cross_time_state, now)
            })
        })
        .collect();
    let live_mask = encoded_qso_ids
        .iter()
        .zip(parity_hold_mask.iter())
        .map(|(id, &held)| tx_qso_is_live(id.as_deref(), active_tx_qsos) && !held)
        .collect();
    (live_mask, parity_hold_mask)
}

/// Build the TX-strip status items for a multi-TX bundle, tagging every item
/// with the SAME `deferred` flag — a bundle either defers as a whole or
/// doesn't (all its items share one parity/slot, per the bundling logic).
/// Shared by the initial "QUEUED" status (`deferred: false`) and the
/// Step-2b defer-time status refresh (`deferred: true`, TX-F8) so the two
/// call sites can't drift out of sync.
pub(super) fn multi_tx_status_items(
    items: &[crate::message_bus::TransmitRequestItem],
    deferred: bool,
) -> Vec<crate::message_bus::TxItem> {
    items
        .iter()
        .map(|it| crate::message_bus::TxItem {
            text: it.message_text.clone(),
            freq_hz: it.frequency_offset,
            qso_id: it.qso_id.clone(),
            deferred,
        })
        .collect()
}

/// Result of [`encode_and_modulate_multi_tx`].
pub(super) struct MultiTxEncodeOutcome {
    /// `Ok(samples)` — the summed waveform — when at least one item
    /// encoded and `modulate_multi_tx` succeeded. `Err(reason)` covers
    /// both "nothing encoded" and "modulation of the survivors failed";
    /// the caller only needs to diagnose the latter (when `item_texts`
    /// is non-empty — encode failures are already reported via
    /// `encode_failed`).
    pub(super) samples: Result<Vec<f32>, String>,
    /// The original `TransmitRequestItem`s that encoded successfully —
    /// parallel to `item_texts`/`encoded_qso_ids`, in the same order
    /// they were summed into `samples`.
    ///
    /// 2026-07-17: added after an independent review of an earlier
    /// version of this fix found that the CALLER was zipping the
    /// unfiltered input `items` list against this function's (possibly
    /// shorter, when any item failed to encode) `encoded_qso_ids` —
    /// silently misaligning positions and reintroducing the exact class
    /// of bug this feature exists to prevent (in the worst case,
    /// transmitting a cancelled QSO's call while dropping the genuinely
    /// still-live one). Callers MUST rebind their `items` to this field
    /// immediately after calling this function, rather than continuing
    /// to use their original input list.
    pub(super) encoded_items: Vec<crate::message_bus::TransmitRequestItem>,
    /// Message texts of items that encoded successfully — parallel to
    /// `encoded_items`/`encoded_qso_ids`, in the same order they were
    /// summed into `samples`.
    pub(super) item_texts: Vec<String>,
    /// QSO ids of items that encoded successfully — parallel to
    /// `item_texts`. `None` for manual/tune items (never gated by
    /// liveness), matching `tx_qso_is_live`'s contract.
    pub(super) encoded_qso_ids: Vec<Option<String>>,
    /// Items whose `encode_for_protocol` call itself failed (never
    /// reached modulation) — the caller reports each of these
    /// individually via `emit_tx_failure_diagnostic` + `TransmitComplete`.
    pub(super) encode_failed: Vec<crate::message_bus::TransmitRequestItem>,
}

/// Encode + modulate a set of multi-TX items into one summed waveform.
///
/// 2026-07-17: extracted from the original Step 1 inline block so the
/// SAME encode/modulate logic can run twice — once normally (the full
/// bundle), and again at Step 4b's key-time gate for just the
/// still-live subset when the bundle's item set shrank during the
/// pre-PTT wait — instead of either transmitting stale audio for a
/// cancelled QSO baked into an already-summed waveform, or unconditionally
/// dropping a still-live item's cycle for no reason.
///
/// This extraction also fixes a pre-existing index-misalignment bug in
/// the original inline version: it built each `MultiTxItem`'s frequency
/// via `items[i].frequency_offset` AFTER the encode loop had already
/// skipped failed items from `symbol_sets` — so if any item other than
/// the last failed to encode, every later surviving item was summed at
/// the WRONG (unrelated, earlier-indexed) frequency. Here, each item's
/// frequency offset is pushed in the SAME loop iteration as its
/// successful encode, so nothing can misalign.
pub(super) fn encode_and_modulate_multi_tx(
    encoder: &mut Ft8Encoder,
    active_protocol: pancetta_ft8::Protocol,
    tx_params: &pancetta_ft8::ProtocolParams,
    items: &[crate::message_bus::TransmitRequestItem],
) -> MultiTxEncodeOutcome {
    encode_and_modulate_multi_tx_with_fox(encoder, active_protocol, tx_params, items, &|_| false)
}

/// Whether a bundle item's QSO is a Fox-admitted stream (PAN-386), read from
/// the coordinator's `fox_admitted_qsos`. A poisoned lock reads as "not Fox",
/// which falls back to the 75 Hz spacing and one frame per item.
pub(super) fn fox_stream_predicate(
    set: &std::sync::RwLock<std::collections::HashSet<String>>,
) -> impl Fn(&str) -> bool + '_ {
    move |id| {
        set.read()
            .is_ok_and(|s| s.contains(&super::super::active_tx_qso_key(id)))
    }
}

/// [`encode_and_modulate_multi_tx`] with FT8 Fox streams (PAN-386).
///
/// `is_fox` says which items' QSOs are Fox-admitted. In FT8, the n-th Fox
/// RR73 item and the n-th Fox report item naming the same Fox air as ONE
/// type 0.1 frame (`K1ABC RR73; W9XYZ <KH7Z> -08`) at the report item's
/// frequency ([`fox_multiplex::plan_fox_frames`]); if that encode fails (a
/// nonstandard Hound call) the pair falls back to two standard frames. Two
/// frames that carry only Fox items may sit [`pancetta_qso::FOX_MIN_SEPARATION_HZ`]
/// apart; every other pair keeps the bandwidth + 25 Hz rule. With no Fox
/// items this is byte-identical to the legacy path.
///
/// `encoded_items`, `item_texts` and `encoded_qso_ids` keep one entry per
/// successfully encoded item, in input order, so every item keeps its own
/// attempt id, outcome report and frequency downstream.
pub(super) fn encode_and_modulate_multi_tx_with_fox(
    encoder: &mut Ft8Encoder,
    active_protocol: pancetta_ft8::Protocol,
    tx_params: &pancetta_ft8::ProtocolParams,
    items: &[crate::message_bus::TransmitRequestItem],
    is_fox: &dyn Fn(&str) -> bool,
) -> MultiTxEncodeOutcome {
    // TransmitRequestItem.frequency_offset is the ABSOLUTE audio
    // frequency (matching TransmitRequest semantics). modulate_multi_tx
    // wants per-item OFFSETS from a shared base. Use base=200 (lowest
    // valid) so any audio frequency in the 200-2500 Hz FT8 passband maps
    // to a non-negative per-item offset.
    const MULTI_TX_BASE_HZ: f64 = 200.0;
    /// Guard the modulator uses for every pair outside Fox mode.
    const DEFAULT_GUARD_HZ: f64 = 25.0;

    /// One frame ready to modulate.
    struct EncodedFrame {
        symbols: Vec<u8>,
        /// Offset from `MULTI_TX_BASE_HZ`.
        offset: f64,
        /// Every item in the frame is a Fox-admitted FT8 stream.
        fox: bool,
    }

    let fox_ft8 = active_protocol == pancetta_ft8::Protocol::Ft8;
    let item_is_fox = |item: &crate::message_bus::TransmitRequestItem| {
        fox_ft8 && item.qso_id.as_deref().is_some_and(is_fox)
    };

    let plan = fox_multiplex::plan_fox_frames(items, active_protocol, is_fox);
    let mut frames: Vec<EncodedFrame> = Vec::with_capacity(plan.len());
    let mut encoded = vec![false; items.len()];
    let mut failed = vec![false; items.len()];

    for frame in &plan {
        if let Some((rr73_to, report_to, fox_call, db)) = &frame.dxpedition {
            match encoder.encode_dxpedition_contacts(rr73_to, report_to, fox_call, *db) {
                Ok(symbols) => {
                    let texts: Vec<&str> = frame
                        .item_indices
                        .iter()
                        .map(|&i| items[i].message_text.as_str())
                        .collect();
                    info!(
                        "Fox multiplex: {} -> '{} RR73; {} <{}> {:+03}' @ {:.0} Hz",
                        texts
                            .iter()
                            .map(|t| format!("'{t}'"))
                            .collect::<Vec<_>>()
                            .join(" + "),
                        rr73_to,
                        report_to,
                        fox_call,
                        db,
                        frame.frequency_offset
                    );
                    for &i in &frame.item_indices {
                        encoded[i] = true;
                    }
                    frames.push(EncodedFrame {
                        symbols: symbols.to_vec(),
                        offset: frame.frequency_offset - MULTI_TX_BASE_HZ,
                        fox: true,
                    });
                    continue;
                }
                Err(e) => {
                    debug!(
                        "Fox multiplex not possible ({}); sending the pair as standard frames",
                        e
                    );
                }
            }
        }
        for &i in &frame.item_indices {
            let item = &items[i];
            match encode_for_protocol(encoder, active_protocol, &item.message_text) {
                Ok(symbols) => {
                    encoded[i] = true;
                    frames.push(EncodedFrame {
                        symbols,
                        offset: item.frequency_offset - MULTI_TX_BASE_HZ,
                        fox: item_is_fox(item),
                    });
                }
                Err(e) => {
                    warn!("Encoding failed for '{}': {}", item.message_text, e);
                    failed[i] = true;
                }
            }
        }
    }

    let mut encoded_items: Vec<crate::message_bus::TransmitRequestItem> = Vec::new();
    let mut item_texts: Vec<String> = Vec::new();
    let mut encoded_qso_ids: Vec<Option<String>> = Vec::new();
    let mut encode_failed: Vec<crate::message_bus::TransmitRequestItem> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if encoded[i] {
            encoded_items.push(item.clone());
            item_texts.push(item.message_text.clone());
            encoded_qso_ids.push(item.qso_id.clone());
        } else if failed[i] {
            encode_failed.push(item.clone());
        }
    }

    if frames.is_empty() {
        return MultiTxEncodeOutcome {
            samples: Err("no items to modulate".to_string()),
            encoded_items,
            item_texts,
            encoded_qso_ids,
            encode_failed,
        };
    }

    // PAN-386: only a pair of all-Fox FT8 frames may sit closer than the
    // modulator's default bandwidth + 25 Hz, and then only down to
    // `FOX_MIN_SEPARATION_HZ`. The modulator then runs with the narrower
    // guard, so this check is what keeps every other pair at the default.
    let any_fox = frames.iter().any(|f| f.fox);
    let guard_hz = if any_fox {
        let bandwidth = tx_params.signal_bandwidth();
        for (i, a) in frames.iter().enumerate() {
            for b in &frames[i + 1..] {
                let sep = (a.offset - b.offset).abs();
                let min_sep = if a.fox && b.fox {
                    pancetta_qso::FOX_MIN_SEPARATION_HZ
                } else {
                    bandwidth + DEFAULT_GUARD_HZ
                };
                if sep < min_sep {
                    let e = pancetta_ft8::Ft8Error::ConfigError(format!(
                        "Frequency separation {:.1} Hz too small (minimum {:.1} Hz) \
                         between signals at {:.1} and {:.1} Hz",
                        sep, min_sep, a.offset, b.offset
                    ));
                    warn!("Multi-TX modulation failed: {}", e);
                    return MultiTxEncodeOutcome {
                        samples: Err(format!("{e}")),
                        encoded_items,
                        item_texts,
                        encoded_qso_ids,
                        encode_failed,
                    };
                }
            }
        }
        (pancetta_qso::FOX_MIN_SEPARATION_HZ - bandwidth).min(DEFAULT_GUARD_HZ)
    } else {
        DEFAULT_GUARD_HZ
    };

    let multi_items: Vec<_> = frames
        .iter()
        .map(|f| pancetta_ft8::MultiTxItem {
            symbols: f.symbols.as_slice(),
            frequency_offset: f.offset,
            params: tx_params,
        })
        .collect();

    let samples = match pancetta_ft8::modulate_multi_tx_with_guard(
        &multi_items,
        12000,
        MULTI_TX_BASE_HZ,
        0.5,
        guard_hz,
    ) {
        Ok(samples) => {
            info!(
                "Multi-TX: {} messages -> {} samples ({:.2}s)",
                multi_items.len(),
                samples.len(),
                samples.len() as f64 / 12000.0
            );
            Ok(samples)
        }
        Err(e) => {
            warn!("Multi-TX modulation failed: {}", e);
            Err(format!("{e}"))
        }
    };

    MultiTxEncodeOutcome {
        samples,
        encoded_items,
        item_texts,
        encoded_qso_ids,
        encode_failed,
    }
}

/// Upper bound on the number of distinct TX streams the worker will retain
/// when coalescing a backlog. Mirrors the "max simultaneous TX in one slot"
/// ceiling: a single FT8 slot can only carry a handful of summed signals
/// cleanly, so retaining more than this serves no purpose and only risks
/// over-summing the waveform. There is no shared multi-TX cap constant in the
/// TX-worker scope (the QSO engine's `max_concurrent_qsos` lives in config and
/// is out of bounds here), so we pick a small, safe constant.
pub(super) const MAX_RETAINED_TX_STREAMS: usize = 8;

/// One drained `TransmitRequest`, reduced to the fields coalescing needs.
/// Pulled out of `MessageType::TransmitRequest` so the coalesce logic is a
/// pure function testable without the message bus.
#[derive(Debug, Clone, PartialEq)]
pub struct CoalesceEntry {
    pub message_text: String,
    pub frequency_offset: f64,
    pub qso_id: Option<String>,
    pub tx_parity: Option<pancetta_core::slot::SlotParity>,
    /// Origin of the drained request (`Local`/`Remote`). Threaded through the
    /// coalescer so a folded bundle preserves it — if ANY folded entry is
    /// `Remote`, the emitted request/bundle is `Remote` (the arm gate applies
    /// to the whole bundle; fail-safe). Defaults to `Local`.
    pub origin: crate::message_bus::TxOrigin,
    /// The client this entry's request is bound to, iff `origin == Remote`
    /// (PAN-91). Threaded through the coalescer alongside `origin` — see
    /// `MessageType::TransmitRequest::remote_client_key_id`.
    pub remote_client_key_id: Option<String>,
    /// The entry's own ledger attempt (PAN-183), kept on every request or
    /// bundle item rebuilt from it.
    pub attempt_id: Option<pancetta_qso::TxAttemptId>,
}

/// Result of draining + coalescing a backlog of `TransmitRequest`s.
#[derive(Debug, Default, PartialEq)]
pub struct CoalesceOutcome {
    /// The requests to actually transmit, after coalescing per `qso_id`
    /// (newest wins), dropping terminal-QSO requests, and capping the
    /// distinct-stream count. QSO-keyed streams Fox mode opened come first
    /// (PAN-76, see [`coalesce_transmit_requests_with_fox_priority`]), then
    /// the other QSO-keyed streams, then manual sends; each group keeps the
    /// order its keys were first seen, so with no Fox-admitted stream present
    /// the head entry is the oldest-surviving stream.
    pub retained: Vec<CoalesceEntry>,
    /// How many requests were superseded by a newer request for the SAME
    /// `qso_id` (i.e. older keep-call frames dropped in favor of the latest).
    pub coalesced: usize,
    /// The entries counted in `coalesced` (PAN-279). The caller rolls back
    /// each one's own ledger attempt; its QSO's surviving entry reports to
    /// Autonomous.
    pub coalesced_entries: Vec<CoalesceEntry>,
    /// How many requests were dropped because their `qso_id` is no longer in
    /// the active set (terminal / cancelled / completed-past-grace QSO).
    pub dropped_terminal: usize,
    /// The entries counted in `dropped_terminal` (PAN-185). The caller
    /// reports each to Autonomous as a failed completion: a pivot can leave
    /// a QSO's queued request as the one that owns a tracked self-CQ's
    /// outcome, and dropping it silently would leave that self-CQ pending.
    pub dropped_terminal_entries: Vec<CoalesceEntry>,
    /// How many distinct streams were dropped because the retained set hit
    /// either the operator-configured concurrent-QSO cap
    /// (`max_concurrent_qsos`, QSO-keyed streams only) or the hard
    /// [`MAX_RETAINED_TX_STREAMS`] backstop (silent truncation made visible).
    pub truncated: usize,
    /// The actual entries dropped by either truncation cap counted in
    /// `truncated` above — unlike `dropped_terminal` (a QSO that already
    /// ended, nothing to roll back) or `parity_excluded`/`freq_excluded`
    /// (temporary, the QSO's own cadence retries it next cycle), a truncated
    /// stream represents an ADMITTED, still-live QSO whose call-bookkeeping
    /// (call_count, last_call_at, Sent timeline) already advanced as if this
    /// frame would transmit. The caller must report a failed
    /// completion through `report_transmit_complete`, which sends
    /// `TxFrameNotSent` to `QsoManager::roll_back_unsent_attempt`, so it does not drift
    /// from reality (a stream that's always last in FIFO order would
    /// otherwise burn through its retry budget or time out having never
    /// actually gone on air).
    pub truncated_entries: Vec<CoalesceEntry>,
    /// The effective concurrent-QSO cap actually applied to QSO-keyed
    /// streams this cycle (`max_concurrent_qsos`, raised to the number of
    /// Fox-admitted streams present in this backlog (PAN-76), clamped into
    /// `1..=MAX_RETAINED_TX_STREAMS`) — exposed so a truncation diagnostic
    /// can report the cap that actually did the truncating instead of always
    /// citing the (possibly much larger) hard backstop.
    pub qso_cap: usize,
    /// How many retained streams were excluded from THIS cycle's bundle
    /// because their concrete `tx_parity` disagreed with the bundle anchor's
    /// (the first-seen retained stream's parity — see
    /// [`coalesce_transmit_requests`]'s parity-conflict check). Folding a
    /// disagreeing stream into the bundle would silently put it on the wrong
    /// slot window, which is exactly the "every concurrent active QSO
    /// transmits on the same parity" invariant this excludes to protect.
    /// Excluded streams are NOT retained here, but they are not dropped
    /// permanently either — the QSO's own TX cadence (keep-call/rearm) will
    /// produce a fresh request for it next slot, sent individually on its own
    /// parity.
    pub parity_excluded: usize,
    /// FQ-F4/TX-F6 defense-in-depth: how many retained streams were excluded
    /// from THIS cycle's bundle because their `frequency_offset` fell within
    /// [`pancetta_qso::MIN_TX_SEPARATION_HZ`] of an already-retained stream.
    /// `modulate_multi_tx`'s pairwise separation check (bandwidth + 25 Hz
    /// guard) fails the ENTIRE bundle — not just the colliding pair — if two
    /// folded streams are too close, so this excludes the later one instead
    /// of letting the whole bundle's TX silently vanish. Fix 5's own
    /// de-confliction at QSO-open time (`compute_manual_tx_offset` in
    /// `coordinator/qso.rs`) should make this rare in practice; this is
    /// cheap insurance against any other path that still produces
    /// close-together streams. Same "exclude, don't coerce, it retries
    /// individually next cycle" semantics as `parity_excluded` above.
    pub freq_excluded: usize,
    /// The entries counted in `parity_excluded` and `freq_excluded`
    /// (PAN-185). Reported to Autonomous only, like `dropped_terminal_entries`;
    /// their ledger accounting is unchanged.
    pub excluded_entries: Vec<CoalesceEntry>,
}

impl CoalesceOutcome {
    /// `true` when nothing was reduced — a single request (or a backlog that
    /// happened to be one fresh frame per distinct live QSO with no overflow).
    /// Used only for the log-suppression decision in the worker.
    pub(super) fn is_noop(&self) -> bool {
        self.coalesced == 0
            && self.dropped_terminal == 0
            && self.truncated == 0
            && self.parity_excluded == 0
            && self.freq_excluded == 0
    }
}

/// The effective concurrent-QSO ceiling for THIS cycle: the operator's
/// configured `autonomous.max_concurrent_qsos`, raised to Fox mode's own
/// `fox_max_streams + 1` (the `+1` for Fox's independent CQ stream, which
/// occupies a slot alongside — not carved out of — the Hound-answer count;
/// see `maybe_answer_caller`'s "Capacity bound" comment in `coordinator/qso.rs`)
/// whenever Fox mode is active.
///
/// Both `fox_mode` and `fox_max_streams` are live `Atomic`s the operator/
/// engine can flip at runtime, so this must be recomputed per cycle rather
/// than folded into the `max_concurrent_qsos` snapshot the worker captures
/// once at startup — Fox mode staying capped at the normal single/dual-QSO
/// limit would truncate its own intended multi-stream Hound-answer bundle
/// down to one signal.
///
/// Hound exchanges still running after `SetFoxMode(false)` (PAN-76): a floor
/// at `active_tx_qsos.len()` was tried and reverted in PR #348 rounds 4-5,
/// because `active_tx_qsos` cannot tell a Fox-admitted Hound from an
/// ordinary over-cap manual QSO, so that floor let a manual pileup through.
/// The QSO component now records the QSOs Fox mode opened in the
/// coordinator's `fox_admitted_qsos`. The backlog coalescer gets this
/// UNRAISED value plus that set, and
/// [`coalesce_transmit_requests_with_fox_priority`] raises its cap only to
/// the Fox-admitted streams actually present in the backlog — never the
/// set's size, which would lend the slots of Hounds idling in their 45 s
/// completed grace to ordinary QSOs. The mid-TX supersede sites use
/// [`supersede_max_concurrent_qsos`] over [`live_fox_admitted_count`].
pub(crate) fn effective_max_concurrent_qsos(
    max_concurrent_qsos: u32,
    fox_mode: &std::sync::atomic::AtomicBool,
    fox_max_streams: &std::sync::atomic::AtomicUsize,
) -> u32 {
    if fox_mode.load(Ordering::Relaxed) {
        let fox_total = fox_max_streams.load(Ordering::Relaxed) as u32 + 1;
        max_concurrent_qsos.max(fox_total)
    } else {
        max_concurrent_qsos
    }
}

/// PAN-76: Fox-admitted QSOs that are still TX-live (in `active_tx_qsos`).
/// Either lock poisoned → 0, i.e. no raise over today's ceiling.
pub(crate) fn live_fox_admitted_count(
    fox_admitted_qsos: &std::sync::RwLock<std::collections::HashSet<String>>,
    active_tx_qsos: &std::sync::RwLock<std::collections::HashSet<String>>,
) -> u32 {
    let (Ok(fox), Ok(active)) = (fox_admitted_qsos.read(), active_tx_qsos.read()) else {
        return 0;
    };
    fox.iter().filter(|k| active.contains(*k)).count() as u32
}

/// PAN-76: ceiling for the mid-TX supersede paths: the live Fox-admitted
/// streams on top of the normal cap, the same shape as the coalescer's rule
/// (Fox-admitted streams are never truncated; ordinary ones are capped). A
/// mixed in-flight bundle of Hounds plus ordinary QSOs therefore stays under
/// it, so a Hound's fresh frame folds in instead of collapsing the bundle
/// (PR #421 review). A supersede never adds a QSO stream
/// (`classify_incoming_during_tx` only supersedes on an in-flight QSO or a
/// manual send), so this never admits more than the coalescer already did.
pub(crate) fn supersede_max_concurrent_qsos(effective: u32, live_fox_admitted: u32) -> u32 {
    effective.saturating_add(live_fox_admitted)
}

/// Pure backlog coalescer for the single-threaded TX worker.
///
/// Given the requests drained from the channel (oldest first — the channel is
/// FIFO) and a predicate for whether a `qso_id` is still live, collapse the
/// backlog to "current intent":
///
/// 1. **Coalesce per `qso_id`, newest wins.** Two requests sharing a non-`None`
///    `qso_id` are the same stream's keep-call cadence; only the latest matters
///    (a newer keep-call frame supersedes the older). The older one is counted
///    in `coalesced` and discarded.
/// 2. **Never coalesce manual / free-text / tune sends.** A request with
///    `qso_id == None` is its own non-coalescable stream — every such entry is
///    retained verbatim, so a flood of keep-calls can never swallow an
///    operator's manual send.
/// 3. **Drop terminal QSOs.** A request whose `qso_id` is no longer live (same
///    predicate Step 4b uses) is dropped during the drain and counted in
///    `dropped_terminal`. `None`-keyed requests are never gated.
/// 4. **Bound the retained set.** At most [`MAX_RETAINED_TX_STREAMS`] distinct
///    streams survive; the rest are counted in `truncated` and dropped. The
///    earliest-seen streams are kept (FIFO fairness).
/// 5. **Never coerce a bundle-parity conflict.** When ≥2 distinct streams
///    survive, the caller folds them into one `MultiTransmitRequest` stamped
///    with a single `tx_parity` (the first-seen/oldest-retained stream's — the
///    "bundle anchor"). Any later stream whose OWN concrete `tx_parity`
///    disagrees with the anchor is excluded here (counted in
///    `parity_excluded`) rather than silently forced onto the anchor's
///    parity — every concurrent active QSO must transmit on the same parity
///    (see CLAUDE.md), and coercing would put the disagreeing stream in the
///    wrong slot window. Excluded streams are not gone permanently: the QSO's
///    own cadence produces a fresh request for it next slot.
///
/// The single-request, no-backlog case returns `retained == [that request]`
/// with all counters zero, so the worker's normal path is unchanged.
///
/// `max_concurrent_qsos` bounds the number of distinct QSO-keyed streams
/// (never manual/`None`-keyed sends, which aren't concurrent QSOs) folded
/// into this cycle's bundle — the same operator-configured limit the mid-TX
/// supersede path (`supersede_and_rekey_or_bundle`) enforces. Without this,
/// several manual QSOs opened in the same pre-PTT window bundled unconditionally
/// up to [`MAX_RETAINED_TX_STREAMS`], silently exceeding the configured cap.
///
/// Equivalent to [`coalesce_transmit_requests_with_fox_priority`] with no
/// Fox-admitted QSOs.
pub fn coalesce_transmit_requests(
    drained: Vec<CoalesceEntry>,
    max_concurrent_qsos: u32,
    qso_is_live: impl FnMut(Option<&str>) -> bool,
) -> CoalesceOutcome {
    coalesce_transmit_requests_with_fox_priority(drained, max_concurrent_qsos, qso_is_live, |_| {
        false
    })
}

/// [`coalesce_transmit_requests`] plus one rule for QSOs Fox mode opened:
///
/// 6. **Fox-admitted streams go first and are never cap-truncated (PAN-76).**
///    `is_fox_admitted` receives the canonical [`super::super::active_tx_qso_key`]
///    of each QSO-keyed stream. Fox-admitted streams are ordered before the
///    other QSO-keyed streams (each group keeps first-seen order), so they
///    anchor the bundle parity and win frequency-conflict tie-breaks, and the
///    QSO cap is raised to the number of Fox-admitted streams that survive
///    into this cycle's bundle. Only ordinary streams are truncated, and only
///    to the room `max_concurrent_qsos` leaves. A Fox-admitted QSO with no
///    frame in this backlog (e.g. a completed Hound in its grace window)
///    lends no slot to anyone. Two Fox-admitted streams need only
///    [`pancetta_qso::FOX_MIN_SEPARATION_HZ`] between them (60 Hz Fox slots,
///    PAN-386); any pair that includes an ordinary stream, and every manual
///    send (never Fox), keeps [`pancetta_qso::MIN_TX_SEPARATION_HZ`]. With no
///    Fox-admitted stream present the outcome is identical to
///    [`coalesce_transmit_requests`].
pub fn coalesce_transmit_requests_with_fox_priority(
    drained: Vec<CoalesceEntry>,
    max_concurrent_qsos: u32,
    qso_is_live: impl FnMut(Option<&str>) -> bool,
    is_fox_admitted: impl FnMut(&str) -> bool,
) -> CoalesceOutcome {
    coalesce_with_fox(
        drained,
        max_concurrent_qsos,
        qso_is_live,
        is_fox_admitted,
        true,
    )
}

/// [`coalesce_transmit_requests_with_fox_priority`] with the Fox-pair spacing
/// switchable: `fox_spacing` is `true` only while the worker's protocol is
/// FT8, the only protocol whose encode relaxes the modulator guard for Fox
/// frames (PAN-386). With `false` every pair keeps
/// [`pancetta_qso::MIN_TX_SEPARATION_HZ`], as before PAN-386.
pub(super) fn coalesce_with_fox(
    drained: Vec<CoalesceEntry>,
    max_concurrent_qsos: u32,
    mut qso_is_live: impl FnMut(Option<&str>) -> bool,
    mut is_fox_admitted: impl FnMut(&str) -> bool,
    fox_spacing: bool,
) -> CoalesceOutcome {
    use std::collections::HashMap;

    let mut outcome = CoalesceOutcome::default();
    // Insertion-ordered map keyed by qso_id (uppercased to match the
    // active-set canonicalization). `None`-keyed entries bypass the map and go
    // straight to `manual` so they're never coalesced.
    let mut order: Vec<String> = Vec::new();
    let mut by_qso: HashMap<String, CoalesceEntry> = HashMap::new();
    // Retained manual/None entries, kept in drain order.
    let mut manual: Vec<CoalesceEntry> = Vec::new();

    for entry in drained {
        // Drop terminal-QSO requests (None is never gated).
        if !qso_is_live(entry.qso_id.as_deref()) {
            outcome.dropped_terminal += 1;
            outcome.dropped_terminal_entries.push(entry);
            continue;
        }
        match entry.qso_id.as_deref() {
            None => manual.push(entry),
            Some(id) => {
                let key = super::super::active_tx_qso_key(id);
                if let Some(older) = by_qso.insert(key.clone(), entry) {
                    // Superseded an older frame for the same QSO.
                    outcome.coalesced += 1;
                    outcome.coalesced_entries.push(older);
                } else {
                    order.push(key);
                }
            }
        }
    }

    // Assemble QSO-keyed streams (first-seen order) followed by manual sends
    // — UNCAPPED for now. The concurrent-QSO cap is applied further below,
    // AFTER the parity/frequency conflict passes: capping first can truncate
    // away a stream that would have survived while keeping one that gets
    // excluded moments later for a conflict, missing an otherwise-valid
    // bundle combination entirely (e.g. cap=2 with streams A/B/C where B
    // conflicts with A — capping first keeps [A, B] then drops B to a
    // conflict, leaving just A; filtering conflicts first correctly leaves
    // [A, C] as the surviving two-stream bundle). Manual sends go last so a
    // single-stream QSO backlog keeps the QSO as the headline item.
    // Fox-admitted streams go before the other QSO-keyed streams (PAN-76) so
    // the cap below truncates only ordinary streams and a Hound anchors the
    // bundle parity whenever one is present.
    let mut retained: Vec<CoalesceEntry> = Vec::with_capacity(order.len() + manual.len());
    let mut ordinary: Vec<CoalesceEntry> = Vec::new();
    for key in order {
        if let Some(e) = by_qso.remove(&key) {
            if is_fox_admitted(&key) {
                retained.push(e);
            } else {
                ordinary.push(e);
            }
        }
    }
    retained.append(&mut ordinary);
    retained.append(&mut manual);

    // Bundle-parity conflict check. When ≥2 distinct streams survive, they may
    // get folded into a single `MultiTransmitRequest` by the caller, stamped
    // with ONE `tx_parity` (the first-seen/oldest-retained stream's — the
    // "bundle anchor"). A later stream whose OWN concrete `tx_parity` disagrees
    // with the anchor must never silently ride that bundle: every concurrent
    // active QSO must transmit on the same parity (CLAUDE.md invariant), and
    // coercing a disagreeing stream onto the bundle's parity would put it in
    // the wrong slot window for its actual partner. `None` (no preference) is
    // NOT a disagreement — only two concrete, differing `Some` values are a
    // genuine conflict. Excluded streams are dropped from `retained` and
    // reported to the QSO engine as not sent (PAN-279): the rollback returns
    // a keep-call frame's call to the budget, so the engine's rearm can
    // re-emit it on its own parity. They are not requeued: a requeued entry
    // would anchor the next bundle on the other parity.
    if retained.len() > 1 {
        let anchor = retained[0].tx_parity;
        let mut kept = Vec::with_capacity(retained.len());
        for (idx, entry) in retained.into_iter().enumerate() {
            let disagrees = match (anchor, entry.tx_parity) {
                (Some(a), Some(p)) => a != p,
                _ => false,
            };
            if idx > 0 && disagrees {
                warn!(
                    target: "pancetta::tx.policy",
                    "TX bundle parity conflict: stream qso_id={:?} requested parity {:?} \
                     but this cycle's bundle is anchored to {:?} (first-seen stream); \
                     excluding it from this bundle and reporting it to the QSO engine as not sent",
                    entry.qso_id, entry.tx_parity, anchor
                );
                outcome.parity_excluded += 1;
                outcome.excluded_entries.push(entry);
            } else {
                kept.push(entry);
            }
        }
        retained = kept;
    }

    // Split QSO-keyed streams from manual/tune sends for the remaining
    // passes. The frequency-conflict check and the concurrent-QSO cap must
    // be resolved between QSO streams FIRST, entirely independent of manual
    // sends — otherwise a manual stream can be wrongly excluded for
    // conflicting with a QSO stream that turns out to be cap-truncated
    // anyway (PR #348 review round 2: cap=1, QSO A/B where B conflicts with
    // a manual send — checking the manual send against B before the cap
    // decides B is truncated wrongly sacrifices a manual send that was
    // actually compatible with the surviving QSO set). `partition`
    // preserves each side's relative (first-seen / drain) order.
    let (qso_entries, manual_entries): (Vec<CoalesceEntry>, Vec<CoalesceEntry>) =
        retained.into_iter().partition(|e| e.qso_id.is_some());

    // FQ-F4/TX-F6 defense-in-depth: pairwise frequency-separation check among
    // QSO-keyed streams only. `modulate_multi_tx` fails the WHOLE bundle (not
    // just the colliding pair) if any two folded streams' `frequency_offset`s
    // are closer than its minimum separation (signal bandwidth + 25 Hz
    // guard). Fix 5's own de-confliction at QSO-open time should make a
    // collision here rare, but this is cheap insurance against any other
    // path that still produces close-together streams. Greedy,
    // order-preserving: the earliest-seen (already-kept) stream at a given
    // offset wins; a later stream too close to ANY already-kept stream is
    // excluded from this cycle's bundle, not coerced onto a different offset.
    // It is reported to the QSO engine as not sent (PAN-279): the rollback
    // returns a keep-call frame's call to the budget, so the engine's rearm
    // can re-emit it.
    // PAN-386: two Fox-admitted streams sit on 60 Hz Fox slots, so a pair of
    // them needs only `FOX_MIN_SEPARATION_HZ`; any pair with an ordinary
    // stream (or a manual send, which is never Fox) keeps
    // `MIN_TX_SEPARATION_HZ`.
    let fox_keys: std::collections::HashSet<String> = qso_entries
        .iter()
        .filter_map(|e| e.qso_id.as_deref())
        .map(super::super::active_tx_qso_key)
        .filter(|k| is_fox_admitted(k))
        .collect();
    let entry_is_fox = |e: &CoalesceEntry| {
        e.qso_id
            .as_deref()
            .is_some_and(|id| fox_keys.contains(&super::super::active_tx_qso_key(id)))
    };
    let min_separation = |a: &CoalesceEntry, b: &CoalesceEntry| {
        if fox_spacing && entry_is_fox(a) && entry_is_fox(b) {
            pancetta_qso::FOX_MIN_SEPARATION_HZ
        } else {
            pancetta_qso::MIN_TX_SEPARATION_HZ
        }
    };
    let mut qso_survivors: Vec<CoalesceEntry> = Vec::with_capacity(qso_entries.len());
    for entry in qso_entries {
        let conflict = qso_survivors.iter().find(|k: &&CoalesceEntry| {
            (k.frequency_offset - entry.frequency_offset).abs() < min_separation(k, &entry)
        });
        if let Some(k) = conflict {
            warn!(
                target: "pancetta::tx.policy",
                "TX bundle frequency conflict: stream qso_id={:?} at {:.0} Hz is within \
                 {:.0} Hz of an already-retained stream this cycle; excluding it from \
                 this bundle and reporting it to the QSO engine as not sent",
                entry.qso_id, entry.frequency_offset, min_separation(k, &entry)
            );
            outcome.freq_excluded += 1;
            outcome.excluded_entries.push(entry);
        } else {
            qso_survivors.push(entry);
        }
    }

    // Enforce the operator's configured concurrent-QSO cap on the QSO-keyed
    // survivors — now that QSO-vs-QSO conflicts are already resolved, cap
    // truncation only ever drops a genuinely bundleable stream. A truncated
    // entry represents an ADMITTED, still-live QSO whose call-bookkeeping
    // has already advanced as if this frame would transmit, so its
    // `CoalesceEntry` is retained in `truncated_entries` for the caller to
    // use `report_transmit_complete`, which routes `TxFrameNotSent` to
    // `QsoManager::roll_back_unsent_attempt` (PAN-75).
    // PAN-76: raise the cap to the Fox-admitted streams present — they sit at
    // the front, so `split_off` only ever truncates ordinary streams.
    let fox_present = qso_survivors
        .iter()
        .filter(|e| {
            e.qso_id
                .as_deref()
                .is_some_and(|id| is_fox_admitted(&super::super::active_tx_qso_key(id)))
        })
        .count();
    let qso_cap = (max_concurrent_qsos.max(1) as usize)
        .max(fox_present)
        .min(MAX_RETAINED_TX_STREAMS);
    outcome.qso_cap = qso_cap;
    if qso_survivors.len() > qso_cap {
        let overflow = qso_survivors.split_off(qso_cap);
        outcome.truncated += overflow.len();
        outcome.truncated_entries.extend(overflow);
    }

    // Check manual/tune sends against the FINAL surviving QSO set (post
    // conflict-filter, post-cap) plus each other — never against a QSO
    // stream that was excluded or truncated above. Manual sends are never
    // subject to the concurrent-QSO cap themselves.
    let mut kept: Vec<CoalesceEntry> = qso_survivors;
    for entry in manual_entries {
        let too_close = kept.iter().any(|k: &CoalesceEntry| {
            (k.frequency_offset - entry.frequency_offset).abs() < pancetta_qso::MIN_TX_SEPARATION_HZ
        });
        if too_close {
            warn!(
                target: "pancetta::tx.policy",
                "TX bundle frequency conflict: stream qso_id={:?} at {:.0} Hz is within \
                 {:.0} Hz of an already-retained stream this cycle; excluding it from \
                 this bundle and reporting it to the QSO engine as not sent",
                entry.qso_id, entry.frequency_offset, pancetta_qso::MIN_TX_SEPARATION_HZ
            );
            outcome.freq_excluded += 1;
            outcome.excluded_entries.push(entry);
        } else {
            kept.push(entry);
        }
    }
    let mut retained = kept;

    // Final hard safety cap across everything retained (QSO streams + manual
    // sends together), independent of the concurrent-QSO setting.
    if retained.len() > MAX_RETAINED_TX_STREAMS {
        let overflow = retained.split_off(MAX_RETAINED_TX_STREAMS);
        outcome.truncated += overflow.len();
        outcome.truncated_entries.extend(overflow);
    }

    outcome.retained = retained;
    outcome
}

/// The frame a coalescer entry stands for (PAN-279).
pub(super) fn coalesce_entry_frame(entry: &CoalesceEntry) -> TxFrame {
    TxFrame {
        qso_id: entry.qso_id.clone(),
        message_text: entry.message_text.clone(),
        attempt_id: entry.attempt_id,
    }
}

/// Drain the queued backlog behind a head `TransmitRequest` and collapse it to
/// current intent, returning the `MessageType` the worker should actually
/// process this cycle.
///
/// `head` MUST be a `MessageType::TransmitRequest` (the caller checks). The
/// channel is drained non-blockingly: every additional `TransmitRequest` is
/// folded into the coalesce buffer; the FIRST non-`TransmitRequest`
/// (`MultiTransmitRequest` / `TuneRequest` / anything else) stops the drain and
/// is put back at the front of the worker's inbox so it is never reordered
/// relative to other non-TX messages, coalesced, or dropped.
///
/// Returns:
/// - the original single `TransmitRequest` when nothing was queued (normal,
///   no-backlog path — byte-for-byte unchanged),
/// - a single `TransmitRequest` carrying the freshest retained frame when the
///   backlog collapsed to one distinct live stream,
/// - a `MultiTransmitRequest` folding the freshest frame of each distinct live
///   stream when several survived (reuses the existing multi-TX path).
///
/// A `tx.policy` warning is logged whenever anything was coalesced, dropped, or
/// truncated, so silent backlog reduction is always operator-visible.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn coalesce_backlog_into(
    head: MessageType,
    inbox: &TxInbox,
    message_bus: &MessageBus,
    active_tx_qsos: &std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    fox_admitted_qsos: &std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    max_concurrent_qsos: u32,
    remote_tx_arm: &std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    audit_log: &pancetta_agent::audit::AuditLog,
    display_feed_enabled: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    pivots: Option<(
        &mut std::collections::HashMap<String, (String, f64)>,
        &mut PivotReplaced,
    )>,
) -> MessageType {
    coalesce_backlog_into_with_fox_spacing(
        head,
        inbox,
        message_bus,
        active_tx_qsos,
        fox_admitted_qsos,
        max_concurrent_qsos,
        true,
        remote_tx_arm,
        audit_log,
        display_feed_enabled,
        pivots,
    )
    .await
}

/// The worker's backlog coalescer. `fox_spacing` is `true` while the worker's
/// protocol is FT8, so two Fox-admitted streams may sit on adjacent 60 Hz Fox
/// slots (PAN-386); otherwise every pair keeps 75 Hz.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn coalesce_backlog_into_with_fox_spacing(
    head: MessageType,
    inbox: &TxInbox,
    message_bus: &MessageBus,
    active_tx_qsos: &std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    fox_admitted_qsos: &std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    max_concurrent_qsos: u32,
    fox_spacing: bool,
    remote_tx_arm: &std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    audit_log: &pancetta_agent::audit::AuditLog,
    display_feed_enabled: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    pivots: Option<(
        &mut std::collections::HashMap<String, (String, f64)>,
        &mut PivotReplaced,
    )>,
) -> MessageType {
    // Decompose the head into a CoalesceEntry. (Caller guarantees the variant.)
    let head_entry = match head {
        MessageType::TransmitRequest {
            attempt_id,
            message_text,
            frequency_offset,
            qso_id,
            tx_parity,
            origin,
            remote_client_key_id,
        } => CoalesceEntry {
            attempt_id,
            message_text,
            frequency_offset,
            qso_id,
            tx_parity,
            origin,
            remote_client_key_id,
        },
        // Defensive: not a TransmitRequest — hand it back unchanged.
        other => return other,
    };

    // Drain queued TransmitRequests behind the head; stop at the first
    // non-TransmitRequest and put it back so it is processed next cycle.
    let mut drained = vec![head_entry];
    while let Ok(msg) = inbox.try_recv() {
        match msg.message_type {
            MessageType::TransmitRequest {
                attempt_id,
                message_text,
                frequency_offset,
                qso_id,
                tx_parity,
                origin,
                remote_client_key_id,
            } => {
                drained.push(CoalesceEntry {
                    attempt_id,
                    message_text,
                    frequency_offset,
                    qso_id,
                    tx_parity,
                    origin,
                    remote_client_key_id,
                });
            }
            _ => {
                // Non-TX message: put it back at the inbox front and stop
                // draining, so Tune/Multi keeps its place relative to TX
                // intent.
                inbox.requeue_front(vec![msg]);
                break;
            }
        }
    }

    // PAN-91 review follow-up: filter out any drained entry that is
    // Remote-origin and bound to a DIFFERENT client than the one currently
    // armed, BEFORE `coalesce_transmit_requests` applies its cap/dedup
    // selection — NOT after (round 2, Codex P2). Filtering after allowed a
    // stale, no-longer-authorized entry (e.g. from a controller that has
    // since been disarmed) to occupy a cap slot ahead of the currently
    // -authorized entry in FIFO order, only for the identity filter to
    // remove it afterward — discarding the authorized entry for no reason,
    // since the capacity it needed was never really unavailable to it.
    // Filtering here, on the raw drained list, means `coalesce_transmit_requests`
    // only ever competes authorized entries against each other.
    //
    // Without this filter at all, control transferring from client A to
    // client B mid-session (A's still-active QSO keeps rearming while B
    // starts new ones) could put both A-bound and B-bound entries in the
    // same backlog; the bundle fold picks ONE representative identity for
    // the whole bundle, so a B-bound entry landing first while B is armed
    // would silently authorize A's frame too. Filtering per-entry here —
    // using each entry's OWN bound identity, exactly like the TX worker's
    // real per-frame gate — closes that gap.
    // Round-3 review (Codex P1): ONE arm snapshot for the WHOLE partition,
    // not a fresh lock per entry — see [`ArmSnapshot`]'s doc for the TOCTOU
    // this closes.
    // PAN-75 (Codex P2 on PR #418): consume pivot duplicates BEFORE
    // newest-per-QSO coalescing. A request whose payload already aired via an
    // earlier pivot could otherwise be discarded here in favour of a newer
    // same-QSO request, so Step 0-dup never sees it: its tombstone goes stale
    // and the frame the pivot replaced, which never aired, stays counted.
    // The newest entry is left for Step 0-dup when every entry is a duplicate.
    if let Some((pivoted_once, pivoted_from)) = pivots {
        if drained.len() > 1 {
            let mut duplicates = Vec::new();
            let mut kept = Vec::with_capacity(drained.len());
            let last = drained.len() - 1;
            for (i, entry) in drained.into_iter().enumerate() {
                let spare = i < last || !kept.is_empty();
                if spare
                    && super::super::is_pivot_duplicate(
                        entry.qso_id.as_deref(),
                        &entry.message_text,
                        entry.frequency_offset,
                        pivoted_once,
                    )
                {
                    // Silent to Autonomous: the pivoted frame keyed and
                    // reported (ownership rule: `report_unaired_frame`).
                    let replaced = entry.qso_id.as_deref().and_then(|id| {
                        let key = super::super::active_tx_qso_key(id);
                        pivoted_once.remove(&key);
                        pivoted_from.remove(&key)
                    });
                    duplicates.push((entry, replaced));
                } else {
                    kept.push(entry);
                }
            }
            drained = kept;
            for (entry, replaced) in duplicates {
                info!(
                    target: "pancetta::tx.policy",
                    "dropping stale TX for '{}' before coalescing — already sent via pivot",
                    entry.message_text
                );
                report_pivot_duplicate_dropped(message_bus, entry.qso_id, replaced).await;
            }
        }
    }

    let arm_snapshot = ArmSnapshot::take(remote_tx_arm, pancetta_agent::arm::ArmTime::now);
    let now_ms = arm_snapshot.decided_at_unix_ms;
    let (admitted, identity_denied): (Vec<_>, Vec<_>) = drained.into_iter().partition(|e| {
        e.origin != crate::message_bus::TxOrigin::Remote
            || arm_snapshot.permits(e.remote_client_key_id.as_deref())
    });
    // (round 2, Codex P2): route through the SAME diagnostic + durable
    // audit + client-visible relay every other arm-gate rejection uses
    // (dispensa Q-0051), not just a log line + TransmitComplete — an
    // attempted cross-client transmission must stay visible in the
    // security audit trail exactly like a pickup-time or mid-flight one.
    //
    // Round-9 review (Codex P2): this reporting is now DETACHED
    // (`tokio::spawn`), not awaited inline — a large backlog left behind
    // by a since-disarmed controller could otherwise force this function
    // to await a diagnostic + synchronous audit-file write + relay + bus
    // send PER denied entry before ever returning the admitted frame to
    // its caller, delaying that frame's own key-time gates by however
    // long the stale backlog happened to be. The admitted result no
    // longer waits on how much (or how slowly) denied traffic there is to
    // report.
    if !identity_denied.is_empty() {
        let message_bus = message_bus.clone();
        let audit_log = audit_log.clone();
        let display_feed_enabled = display_feed_enabled.clone();
        let arm_snapshot = arm_snapshot.clone();
        tokio::spawn(async move {
            for entry in &identity_denied {
                // Round-3 review (Codex P2): only say "bound to a
                // different client" when that's actually why this entry
                // was denied — every other gate failure (nobody armed,
                // heartbeat lost, TTL expired, local consent/kill) is a
                // real safety event but not an identity problem, and
                // reporting it as one obscures what actually happened in
                // a durable security audit record.
                let denial_reason = if arm_snapshot
                    .is_identity_mismatch(entry.remote_client_key_id.as_deref())
                {
                    format!(
                            "bound to a different client than the one currently armed: '{}' at {:.0} Hz",
                            entry.message_text, entry.frequency_offset
                        )
                } else {
                    format!(
                        "remote TX not currently permitted: '{}' at {:.0} Hz",
                        entry.message_text, entry.frequency_offset
                    )
                };
                emit_diagnostic(
                    &message_bus,
                    "agent.tx",
                    pancetta_core::DiagnosticLevel::Warn,
                    format!("Remote TX denied ({denial_reason})"),
                    entry.qso_id.as_deref(),
                )
                .await;
                audit_log.record(&pancetta_agent::audit::AuditEvent {
                    // Round-11 review (Codex P2): the denial-time instant
                    // (the SAME `now_ms` `arm_snapshot` was taken under),
                    // not a fresh timestamp read whenever this detached
                    // task happens to actually run — a task delayed behind
                    // executor load or a large earlier backlog would
                    // otherwise timestamp entries AFTER later, unrelated
                    // arm/disarm events actually occurred, corrupting the
                    // audit trail's own chronological order.
                    ts_unix_ms: now_ms,
                    kind: pancetta_agent::audit::AuditKind::TxDenied,
                    // Round-10 review (Codex P2): attribute from the
                    // FROZEN snapshot, not a fresh lock — see
                    // `ArmSnapshot::operator_callsign`'s doc for why this
                    // detached task must not re-lock the live `ArmState`.
                    operator_callsign: arm_snapshot
                        .operator_attribution(entry.remote_client_key_id.as_deref()),
                    detail: denial_reason.clone(),
                });
                super::super::remote_gateway::relay_to_gateway(
                    &message_bus,
                    &display_feed_enabled,
                    ComponentId::Ft8Transmitter,
                    MessageType::TxDenied {
                        reason: denial_reason,
                        qso_id: entry.qso_id.clone(),
                    },
                )
                .await;
                if let Err(e) = report_transmit_complete(
                    &message_bus,
                    false,
                    entry.message_text.clone(),
                    0,
                    entry.qso_id.clone(),
                    entry.attempt_id,
                )
                .await
                {
                    warn!(
                        "Failed to send TransmitComplete for an identity-denied stream: {}",
                        e
                    );
                }
            }
        });
    }

    // Every drained entry was identity-denied. Hand back an empty
    // MultiTransmitRequest, matching the existing "every drained request
    // belonged to an ended QSO" empty-backlog outcome below.
    if admitted.is_empty() {
        return MessageType::MultiTransmitRequest {
            items: Vec::new(),
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        };
    }

    // Fast path: only one entry survived identity filtering — nothing left
    // to coalesce.
    if admitted.len() == 1 {
        let e = admitted.into_iter().next().expect("len == 1");
        return MessageType::TransmitRequest {
            attempt_id: e.attempt_id,
            message_text: e.message_text,
            frequency_offset: e.frequency_offset,
            qso_id: e.qso_id,
            tx_parity: e.tx_parity,
            origin: e.origin,
            remote_client_key_id: e.remote_client_key_id,
        };
    }

    let backlog_total = admitted.len();
    // PAN-76: snapshot the Fox-admitted set (poisoned → empty, i.e. today's
    // cap) so the coalescer keeps Hound exchanges Fox mode already opened.
    let fox_admitted: std::collections::HashSet<String> = fox_admitted_qsos
        .read()
        .map(|s| s.clone())
        .unwrap_or_default();
    let mut outcome = coalesce_with_fox(
        admitted,
        max_concurrent_qsos,
        |id| tx_qso_is_live(id, active_tx_qsos),
        |key| fox_admitted.contains(key),
        fox_spacing,
    );

    if !outcome.is_noop() {
        let text = format!(
            "TX backlog coalesced: drained {} request(s) → {} retained; \
             coalesced {} stale (newest-per-QSO wins), dropped {} for ended QSOs, \
             truncated {} over the effective {}-QSO cap (hard backstop {}), excluded {} for a \
             bundle-parity conflict, excluded {} for a bundle-frequency conflict",
            backlog_total,
            outcome.retained.len(),
            outcome.coalesced,
            outcome.dropped_terminal,
            outcome.truncated,
            outcome.qso_cap,
            MAX_RETAINED_TX_STREAMS,
            outcome.parity_excluded,
            outcome.freq_excluded,
        );
        warn!(target: "pancetta::tx.policy", "{}", text);
        emit_diagnostic(
            message_bus,
            "tx.policy",
            pancetta_core::DiagnosticLevel::Warn,
            text,
            None,
        )
        .await;
    }

    // A truncated entry is an ADMITTED, still-live QSO whose call-bookkeeping
    // (call_count, last_call_at, Sent timeline) already advanced as if this
    // frame would transmit. Its settlement mirrors the failure as
    // TxFrameNotSent to QsoManager::roll_back_unsent_attempt (PAN-75).
    for entry in &outcome.truncated_entries {
        settle_unaired(
            message_bus,
            coalesce_entry_frame(entry),
            None,
            AutonomousOutcome::Report,
        )
        .await;
    }

    // PAN-185: a terminal or parity/frequency-excluded QSO request may be the
    // one that owns a tracked self-CQ's outcome (a refused pivot leaves it as
    // the owner, see `report_unaired_frame`), so Autonomous hears a failed
    // completion for each. PAN-279: the QSO engine also rolls back each one's
    // own attempt, which returns a keep-call frame's call to the budget so the
    // engine's rearm can re-emit it.
    for entry in outcome
        .dropped_terminal_entries
        .iter()
        .chain(&outcome.excluded_entries)
    {
        settle_unaired(
            message_bus,
            coalesce_entry_frame(entry),
            None,
            AutonomousOutcome::Report,
        )
        .await;
    }

    // PAN-279: a request coalesced into a newer one for the same QSO rolls
    // back its own attempt; the newer one owns the Autonomous outcome. An
    // attempt-less folded frame stays silent: a text match could roll back
    // the survivor's attempt instead.
    let surviving_attempts: std::collections::HashSet<(String, pancetta_qso::TxAttemptId)> =
        outcome
            .retained
            .iter()
            .chain(&outcome.excluded_entries)
            .chain(&outcome.truncated_entries)
            .filter_map(|e| {
                Some((
                    super::super::active_tx_qso_key(e.qso_id.as_deref()?),
                    e.attempt_id?,
                ))
            })
            .collect();
    for entry in &outcome.coalesced_entries {
        let (Some(qso_id), Some(attempt_id)) = (entry.qso_id.as_deref(), entry.attempt_id) else {
            continue;
        };
        if surviving_attempts.contains(&(super::super::active_tx_qso_key(qso_id), attempt_id)) {
            continue;
        }
        settle_unaired(
            message_bus,
            coalesce_entry_frame(entry),
            None,
            AutonomousOutcome::Withheld,
        )
        .await;
    }

    // A bundle has only one remote identity and therefore one consent channel.
    // Keep the first retained remote channel and defer the other, preserving
    // its identity for the next worker pass and the final key-time check.
    // Do this after newest-per-QSO selection so an older frame cannot survive
    // just because a newer request for that QSO came from the other channel.
    let bundle_channel = outcome
        .retained
        .iter()
        .find(|e| e.origin == crate::message_bus::TxOrigin::Remote)
        .map(|e| consent_channel_for(e.remote_client_key_id.as_deref()));
    let (retained, deferred): (Vec<_>, Vec<_>) = outcome.retained.into_iter().partition(|e| {
        e.origin != crate::message_bus::TxOrigin::Remote
            || Some(consent_channel_for(e.remote_client_key_id.as_deref())) == bundle_channel
    });
    outcome.retained = retained;
    // Ahead of the non-TX message the drain stopped at: these arrived first.
    inbox.requeue_front(
        deferred
            .into_iter()
            .map(|entry| {
                self_message(MessageType::TransmitRequest {
                    attempt_id: entry.attempt_id,
                    message_text: entry.message_text,
                    frequency_offset: entry.frequency_offset,
                    qso_id: entry.qso_id,
                    tx_parity: entry.tx_parity,
                    origin: entry.origin,
                    remote_client_key_id: entry.remote_client_key_id,
                })
            })
            .collect(),
    );

    // Every drained request belonged to an ended QSO (rare: needs ≥2 queued
    // requests, all terminal). Hand back an empty MultiTransmitRequest; the
    // multi-TX arm's "empty after dropping stale items" branch consumes and
    // skips it without keying PTT. These QSOs already transitioned terminal;
    // their Autonomous completions were reported above (PAN-185).
    if outcome.retained.is_empty() {
        return MessageType::MultiTransmitRequest {
            items: Vec::new(),
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        };
    }

    // Single retained distinct stream → single TransmitRequest (unchanged arm).
    if outcome.retained.len() == 1 {
        let e = outcome.retained.into_iter().next().expect("len == 1");
        return MessageType::TransmitRequest {
            attempt_id: e.attempt_id,
            message_text: e.message_text,
            frequency_offset: e.frequency_offset,
            qso_id: e.qso_id,
            tx_parity: e.tx_parity,
            origin: e.origin,
            remote_client_key_id: e.remote_client_key_id,
        };
    }

    // Several distinct live streams survived → fold into the existing multi-TX
    // path. All bundle items share one slot, so the bundle parity is the
    // FIRST-SEEN/oldest-retained stream's parity (not the freshest — order
    // here is first-seen order from the coalescer), which the existing arm
    // resolves via resolve_required_parity. Any stream that disagreed with
    // this anchor has already been excluded from `outcome.retained` above
    // (see the parity-conflict check in `coalesce_transmit_requests`), so
    // every remaining entry's `tx_parity` is either `None` or equal to this
    // value.
    let bundle_parity = outcome.retained[0].tx_parity;
    // PAN-136: each item keeps the binding of the entry it came from, so a
    // later key-time pivot can compare and swap it per item.
    let items: Vec<crate::message_bus::TransmitRequestItem> = outcome
        .retained
        .into_iter()
        .map(|e| crate::message_bus::TransmitRequestItem {
            attempt_id: e.attempt_id,
            message_text: e.message_text,
            frequency_offset: e.frequency_offset,
            qso_id: e.qso_id,
            origin: e.origin,
            remote_client_key_id: e.remote_client_key_id,
        })
        .collect();
    // Fail-safe origin fold: if ANY folded stream is Remote, the whole bundle is
    // Remote so the arm gate applies. (In practice a coalesced backlog is one
    // origin; this is defense-in-depth for a future mixed-origin backlog.)
    // All retained remote entries now share one consent channel, and the
    // snapshot already checked every bound identity against the armed client.
    // Folding that identity preserves the channel for the final key-time gate.
    // PAN-136: the fold is shared with the TX worker's post-pivot re-fold.
    let (bundle_origin, bundle_remote_client_key_id) =
        super::super::fold_bundle_identity(items.iter());
    MessageType::MultiTransmitRequest {
        items,
        tx_parity: bundle_parity,
        origin: bundle_origin,
        remote_client_key_id: bundle_remote_client_key_id,
    }
}
