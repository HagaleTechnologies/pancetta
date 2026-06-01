# Within-QSO context graph (hb-173) — design spec

**Status:** proposed (Session 1 of 3; design only, no production code yet)
**Hypothesis:** hb-173 (cross-time ideation T1, spawned 2026-06-01)
**Author:** research harness, 2026-06-01
**Estimated effort:** 3 sessions (this scoping + diagnostic, then
template-table implementation, then decoder hook + eval)
**Parent / spawn context:** cross-time ideation T1
(`research/ideation/2026-06-01-cross-time.md`) flagged the within-QSO
template-injection mechanism as the highest-impact slot-local-assumption
break in pancetta's bank — every QSO turn after the first is template-
constrained by the prior turn's contents and the QSO state machine.

## Why this is the next lever above hb-048 a7

Pancetta's a7 design (hb-048,
`docs/superpowers/specs/2026-05-31-hb-048-a7-design.md`) injects
~206 candidate next-utterance templates rooted at **a single callsign**
that was decoded in the prior slot. The template enumeration walks
plausible OTHER calls, plausible reports, and the small structured-
message set; it does not condition on whether C is currently in an
in-flight QSO or who its partner is.

hb-173 is the strictly stronger variant: when slot N decodes a directed
exchange `K1ABC W1XYZ EM10`, the slot N+1 template space is sharply
narrower than a7's:

- a7 would generate ~206 templates rooted at W1XYZ alone (any partner,
  any payload).
- hb-173 generates **~20–30 templates rooted at the PAIR** `(K1ABC,
  W1XYZ)` and constrained by the QSO state machine's expected next
  message: `W1XYZ K1ABC <SNR>`, `W1XYZ K1ABC R-<SNR>`, `W1XYZ K1ABC
  RR73`, `W1XYZ K1ABC 73`, or (rarely) a re-call of the same payload
  (operator habit when no decode came back).

The narrower template set is the source of disruption: a7 needs `snr7
≥ 6.0` with `snr7b ≥ 1.8` (second-best margin) — the threshold has to
be tight because random noise spikes against 206 templates at 41 lags
gives the FP rate room. With ~25 templates, the same threshold lets
through weaker signal (because the chance of a noise spike across the
smaller template×lag product is correspondingly smaller).

## Diagnostic outcome (Session 1, this session)

`pancetta-research/examples/hb173_within_qso_diagnostic.rs` (committed
on this branch) grouped hard-200 WAVs into chronological sessions and
counted what fraction of jt9 baseline truth decodes were *downstream
turns* of an identifiable QSO. Definition: a directed message whose
callsign pair (order-ignored) already appeared in the same session
within the last 8 slots and within 30 Hz audio frequency, with at
least one DIRECTED prior sighting (excludes accidental same-CQ pair
matches).

**Result on hard-200:** **18.48%** of decoded messages (1594 / 8626)
are in-QSO continuations.
**Result on hard-1000:** **46.53%** of decoded messages (12775 / 27458)
are in-QSO continuations.

Verdict: **PROCEED** to Session 2 — well above the 10% PROCEED
threshold.

Notable shape:

- **Turn-2 dominates** (55% of continuations on hard-200; 29% on
  hard-1000). Turn-2 is the responder's FIRST reply to the caller —
  exactly the population a7 targets, but with pair-conditional
  templates instead of single-callsign.
- **Turn-3+ is substantial** (45% of continuations on hard-200; 71%
  on hard-1000). These are turns a7 cannot capture, because the
  rooted callsign no longer matches the responder's next message —
  the responder is now the originator. hb-173's pair-conditional
  templates capture these because the pair stays stable across role-
  swap turns.
- **Slot gap distribution** is bimodal at 1 and 2 slots (89% on
  hard-200): real QSOs alternate every slot (slot parity guarantees
  this in 80% of real ops). The 2-slot gap is from the responder
  waiting an extra slot to send their report. Beyond 2 slots is rare
  (10% combined for gaps 3–8). The window can be narrower than the
  8-slot diagnostic default in production (4–6 slots is plenty).
- **Sessions are mostly single-slot in the curated corpus** (80% on
  hard-200; 88% on hard-1000) — the curation deliberately picked WAVs
  spread across the operator's history to maximize SNR / decoder-
  difficulty diversity, NOT chronological continuity. This means
  the diagnostic UNDERESTIMATES production coverage; a real
  operator session is one continuous stream of slots, where every
  slot has potential QSO context from the prior slot.

## Mechanism (production view, ~250 words)

**Storage.** A `WithinQsoContext` table at coordinator scope, shared
across decoder threads via `Arc<RwLock<...>>` (mirroring the
`CallsignDtHistory` decision in hb-057,
`docs/superpowers/specs/2026-05-31-hb-057-median-dt-design.md` §117).

```rust
/// Per-QSO state for in-flight conversations. Keyed by an unordered
/// callsign pair plus audio-frequency neighborhood; populated on each
/// successful directed decode and consumed on the next slot's
/// `decode_window_with_ap` to inject pair-conditional AP templates.
pub struct WithinQsoContext {
    /// Active QSO entries. Capped at `MAX_ACTIVE` (default 256) by
    /// LRU eviction; entries also expire on TTL (`MAX_AGE`, default
    /// 90 s = 6 slots).
    entries: HashMap<QsoKey, QsoState>,
    max_active: usize,
    max_age: Duration,
}

#[derive(Hash, PartialEq, Eq, Clone)]
pub struct QsoKey {
    /// Sorted (call_a, call_b) — order ignored.
    pair: (String, String),
    /// Audio-freq bin: `(freq_hz / FREQ_BIN_HZ).round() as i32`. With
    /// FREQ_BIN_HZ = 25, an entry in bin K matches any decode in bin
    /// K-1..=K+1 (±50 Hz total tolerance — operator-stays-in-place
    /// QSOs typically drift <30 Hz across the exchange).
    freq_bin: i32,
}

pub struct QsoState {
    /// Last decoded message text for this pair.
    last_message: String,
    /// Phase classifier — drives which templates we inject next slot.
    phase: QsoPhase,
    /// Inferred slot parity (even/odd) of the LAST responder. Next
    /// expected turn is the OPPOSITE parity.
    last_responder_parity: u8,
    /// Wall-clock last seen (for TTL eviction).
    last_seen: SystemTime,
    /// Turn counter (1 = first directed exchange, 2 = first reply, …).
    turn_count: u32,
}

pub enum QsoPhase {
    /// Caller sent grid; expect responder's report next.
    GridSent,
    /// Report sent (incl R-report); expect ack next.
    ReportSent,
    /// Ack sent; expect RR73 / 73 next.
    AckSent,
    /// 73 sent; QSO complete (entry eligible for eviction).
    Complete,
    /// Unknown phase (free-text payload, irregular exchange).
    Unknown,
}
```

`QsoPhase` is inferred from the LAST message's tail token using a
small classifier (`pancetta-qso` already exposes `MessageType` from
its state machine; we reuse / lightly extend that). The classifier
runs at decode-time on every emitted decode and updates the entry.

**Template injection.** At the start of `decode_window_with_ap`, after
the standard sync_search but before LDPC, the decoder queries the
`WithinQsoContext` (read-lock, no contention) for every Costas
candidate within `freq_bin ± 1` of an in-flight pair. For each match
the decoder generates **20–30 pair-conditional templates** via a new
function:

```rust
pub fn pair_conditional_templates(
    state: &QsoState,
    next_responder: &str, // the OTHER half of the pair
    next_caller: &str,    // whoever spoke last
) -> Vec<A7Template>;
```

The template universe is much smaller than a7's because both callsign
positions are FIXED:

- `<next_responder> <next_caller> <SNR>` for SNR ∈ {-30..0..+10}
  (~25 values, but we cap at the ~15 most likely per the phase
  classifier — `GridSent` → 8 typical report values; `ReportSent` → 4
  typical R-report values)
- `<next_responder> <next_caller> RR73` (Phase ∈ {ReportSent, AckSent})
- `<next_responder> <next_caller> 73` (Phase ∈ {AckSent, Complete})
- A small set of phase-irregular fallback templates (operator repeats
  prior payload because no decode came back).

The templates are passed into a `pair_conditional_cross_correlation`
pass that mirrors a7's correlation primitive but reuses the existing
`a7::cross_correlate` from hb-048's Session 2 (if hb-048 graduates
first) or implements a local primitive.

**Acceptance thresholds.** Initial design: `snr7 ≥ 4.5`, `snr7b ≥ 1.5`
(both relaxed relative to a7's 6.0 / 1.8 because the smaller template
set has lower FP rate). Threshold sweep is part of Session 3 eval.

**Bidirectional state flow (deferred — V2 idea).** The cross-time
ideation T1 entry notes a "decode in reverse" possibility: a confirmed
RR73 at slot N+1 retroactively validates slot N's R-report decode AND
could revisit a slot-N LDPC-fail candidate at the expected report
template. This is intriguing but DEFERRED to a separate hypothesis
(implies state mutation backwards in time, which complicates the
coordinator's emit-on-decode contract). V1 ships forward-only.

## Composition with active / scoped hypotheses

**hb-048 a7 (single-callsign cross-correlation, scoped):** Cleanly
ORTHOGONAL. a7 fires on EVERY slot N+1 candidate where slot N had a
matching callsign at the same audio freq; hb-173 fires on the smaller
subset where the slot-N decode was a DIRECTED message AND its pair is
in the within-QSO table. The two mechanisms can run independently:

- a7 catches "K1ABC just CQ'd, next slot is probably an answer to
  K1ABC" — any incoming caller, any payload.
- hb-173 catches "K1ABC and W1XYZ are mid-QSO; next slot is W1XYZ's
  RR73 or K1ABC's 73". Tighter prior, smaller template set.

When both fire on the same candidate, hb-173's templates win (more
constrained = lower FP rate). The two share `pancetta-ft8/src/a7.rs`
infrastructure if both graduate.

**hb-057 median DT history (scoped):** Cleanly ORTHOGONAL. hb-057 is a
SYNC-time prior (where in time to look); hb-173 is a TEMPLATE-time
prior (what message to expect). Both can fire on the same candidate
without interaction. hb-057 also lives at coordinator scope behind an
`Arc<RwLock<...>>`; hb-173's table uses the same pattern — see "Shared
infra" below.

**hb-052 / 062 callsign continuity filter (graduated):** Cleanly
ORTHOGONAL. Continuity is OUT-bound plausibility (reject implausible
decodes after CRC); hb-173 is IN-bound template injection (create
decodes LDPC otherwise wouldn't find). Continuity will run on hb-173's
outputs as normal — its callsigns are the same pair already seen, so
continuity passes trivially.

**hb-079 / 080 multipass + hb-086 V1 joint-pair-retry (all
graduated):** Cleanly ORTHOGONAL. The multipass loop runs first
(pass 1 → subtract → pass 2 → subtract → pass 3); hb-086 V1 runs the
joint-pair retry on the residual. hb-173's pair-conditional template
correlation can run as either an additional pass on the residual
(after V1) OR — more interesting — at the START of pass 1, providing
ULTRA-strong AP pins that survive into the LDPC iterations. The
Session 3 design will choose; the diagnostic doesn't constrain this.

## Shared infrastructure: `CrossTimeState` crate-level handle

hb-057, hb-048 a7, and hb-173 all need cross-slot state shared across
decoder threads. Three separate `Arc<RwLock<HashMap<...>>>` structs is
the obvious anti-pattern. The cross-time ideation's §Cross-cutting
observations recommends a shared `CrossTimeState` (or per-instance
`Arc<RwLock<…>>` cluster). The proposal:

```rust
/// Coordinator-level cross-slot state. One per coordinator instance.
/// Decoder threads hold an `Arc<CrossTimeState>` clone and acquire
/// per-table read/write locks individually (no global lock).
pub struct CrossTimeState {
    /// hb-057 per-callsign DT history.
    pub dt_history: Arc<RwLock<CallsignDtHistory>>,
    /// hb-048 per-slot a7 expected-call table.
    pub a7_recent_calls: Arc<RwLock<A7RecentCallTable>>,
    /// hb-173 within-QSO context.
    pub within_qso: Arc<RwLock<WithinQsoContext>>,
}
```

This is a **bookkeeping** unification, not a behavioral one. Each
inner table evolves independently. The benefit is one lifetime to
manage in coordinator code (`pancetta/src/coordinator/components.rs`)
and one place to test the cold-start case.

Implementation order: whichever of (hb-057, hb-048) graduates first
introduces `CrossTimeState` with its single table; the second adds
its inner field; hb-173 adds the third. If hb-173 lands first by
chance, it ships its own `Arc<RwLock<WithinQsoContext>>` and refactors
into `CrossTimeState` when hb-057 or hb-048 lands. Coordinator-level
state plumbing is the binding cost across all three, so the
infrastructure investment is shared.

## Build sequence (3 sessions)

### Session 1 — design spec + diagnostic (THIS SESSION, DONE)

- This document.
- `pancetta-research/examples/hb173_within_qso_diagnostic.rs`.
- `research/experiments/2026-06-01-hb-173-session1.md` (verdict +
  decision journal).
- Hypothesis-bank entry updated with `status_2026_06_01_session1`.
- **No production code changes.** Branch is
  `iter/2026-06-01-hb-173`. Commits: 1 diagnostic + 1 spec + 1
  journal.

### Session 2 — `WithinQsoContext` + phase classifier + templates, gated, no production hook

Build on its own branch (`iter/<date>-hb-173-session2`):

- New module `pancetta-ft8/src/within_qso.rs`:
  - `WithinQsoContext`, `QsoKey`, `QsoState`, `QsoPhase` per the
    structures above.
  - `pair_conditional_templates(state, next_responder, next_caller)
    -> Vec<A7Template>` returning the 20–30 phase-conditional
    templates.
  - `update_from_decode(&mut self, decoded: &DecodedMessage, now:
    SystemTime)` for the coordinator to call after each emitted
    decode. Internally infers phase from the message tail.
  - `evict_expired(&mut self, now: SystemTime)` and `evict_lru()`.
- Phase classifier: small standalone function
  `classify_phase(message: &str) -> QsoPhase` (regex-free; uses the
  existing FT8 message tokenizer). 8 cases (CQ-shaped is filtered
  upstream so we don't see them).
- 12 unit tests:
  1. `pair_key_is_order_independent` — `QsoKey::new("K1ABC", "W1XYZ")
     == QsoKey::new("W1XYZ", "K1ABC")`.
  2. `freq_bin_neighbors_match_within_50hz` — three decodes at 1000,
     1015, 1049 Hz map to neighboring bins.
  3. `phase_grid_sent_classified` — `"K1ABC W1XYZ EM10"` →
     `GridSent`.
  4. `phase_report_sent_classified` — `"K1ABC W1XYZ -12"`,
     `"K1ABC W1XYZ R-12"` → `ReportSent`.
  5. `phase_ack_sent_classified` — `"K1ABC W1XYZ RRR"` → `AckSent`.
  6. `phase_complete_classified` — `"K1ABC W1XYZ RR73"`,
     `"K1ABC W1XYZ 73"` → `Complete`.
  7. `update_from_decode_inserts_new_entry`.
  8. `update_from_decode_updates_existing_entry` — same pair, second
     turn, `turn_count == 2`, phase advanced.
  9. `evict_expired_removes_old_entry` — 95 seconds elapsed, age 90.
  10. `evict_lru_caps_at_max_active`.
  11. `pair_conditional_templates_grid_phase_emits_report_templates`
      — `Phase::GridSent` produces 5–8 SNR-shaped templates.
  12. `pair_conditional_templates_complete_phase_emits_73_only`.
- No `pancetta-ft8/src/decoder.rs` changes yet. No coordinator wiring.
  Pure library expansion + tests.

**Session 2 GRADUATE-to-3 gate:**
- All 12 tests pass.
- `cargo test -p pancetta-ft8 --features transmit` clean.
- Template generation count for a `GridSent` entry is 6–10 (sanity).
- Phase-classification accuracy on hard-1000 baseline truths ≥ 95%
  (`pancetta-research/examples/hb173_phase_classifier_audit.rs`,
  built as part of Session 2).

### Session 3 — production wiring + eval

- `pancetta/src/coordinator/components.rs`: instantiate
  `WithinQsoContext` (or extend `CrossTimeState` if it exists);
  thread an `Arc<RwLock<...>>` handle into the decoder constructor.
- `pancetta-ft8/src/decoder.rs`: `decode_window_with_ap` gains an
  optional `within_qso: Option<Arc<RwLock<WithinQsoContext>>>`
  parameter. When `Some`, after V1 joint-pair-retry, run a
  `within_qso_pair_correlation_pass` that:
  1. Acquires read-lock.
  2. For each Costas candidate, looks up `(freq_bin ± 1)` × any
     non-Complete `QsoState` entry → loads expected-pair-conditional
     templates.
  3. Runs `a7::cross_correlate` on each template against the post-V1
     residual.
  4. Emits decodes meeting `snr7 ≥ snr7_threshold` AND `snr7b ≥
     snr7b_threshold`.
- Post-emission: `update_from_decode` on the within-QSO table for
  every successfully emitted directed decode (NOT for hb-173's own
  emissions — those would be self-confirming; see below).
- New `Ft8Config` fields:
  - `within_qso_enable: bool` (default `false` for V1 cautious
    ship; flip to `true` after eval).
  - `within_qso_snr7_threshold: f32` (default 4.5).
  - `within_qso_snr7b_threshold: f32` (default 1.5).
  - `within_qso_freq_bin_hz: f32` (default 25.0).
  - `within_qso_max_qso_slots: u32` (default 6 — tighter than
    diagnostic's 8 because production slot stream is denser).
  - `within_qso_max_age_secs: u32` (default 90).
- Eval:
  - hard-200 with `within_qso_enable={false,true}` (held everything
    else at production defaults; multipass N=3 + V1 ON).
  - hard-1000 with same A/B.
  - Diagnostic-vs-eval comparison: does the diagnostic-predicted 18%
    coverage translate to ~5–15 recall lift on hard-200? (The
    diagnostic measures *coverage*; lift = coverage × P(miss |
    covered). Pancetta misses ~12% of hard-200 truths — so ~2.2% of
    truths are both covered AND missed, giving an upper bound of ~190
    decodes recall lift on hard-200's 8626 truths — i.e., ~20 hard-200
    recoveries at the very top. Realistic 5–10 recall lift is
    expected.)
  - Fixture preservation: no fixture WAV regressions.
  - Composite score: target ≥ +0.0005 (within historical batch range).
  - Elapsed: ≤ +5% (pair-correlation is a few ms per slot worst case).

**Session 3 GRADUATE gate:**
- hard-200 recall lift ≥ +5 decodes.
- hard-1000 recall lift ≥ +20 decodes.
- Composite score ≥ +0.0005.
- Zero novel false-positive shape (the FP-filter callsign-continuity
  rule should reject any FP whose callsign pair never appeared
  before).
- Elapsed regression < 5%.
- Fixture parity preserved.

## Open questions

1. **Should within-QSO emissions self-feed the table?** The natural
   answer is "no" — hb-173 emissions are PREDICTED by the table; if
   they feed back, we get a self-reinforcing loop (predicted RR73 in
   slot N+1 → write to table → predict 73 in slot N+2 → etc.). The
   write should be gated to "only emissions that ALSO passed the
   normal Costas + LDPC gate, not the within-QSO pass". This means
   the table grows only from independent evidence. Settle in
   Session 2.

2. **How does this interact with the autonomous operator's
   `recently_responded_to`?** Both are cross-slot state with similar
   shape; both are read by the decode/decide path. `recently_
   responded_to` is TX-side (don't double-respond to a station I
   already replied to); within-QSO is RX-side (boost decode of
   stations I'm currently exchanging with). No direct conflict. They
   could share an underlying `CrossSlotMemory` trait but the
   semantics differ enough that two structs is cleaner.

3. **Chronological eval tier need.** The diagnostic shows 80% of
   hard-200 sessions are single-slot. The corpus was curated for SNR
   diversity, not chronology. Session 3 eval results will be on the
   *covered* WAVs (the 20% multi-slot sessions). For the FULL
   production effect, we need a chronological-replay tier in the eval
   harness — replay an operator's complete slot stream in true order.
   This is INFRASTRUCTURE work that benefits hb-173, T8 propagation-
   regime, T10 time-aware calibration, and hb-091 a8. Recommend
   filing a separate infra hypothesis ("chronological eval tier")
   after hb-173 Session 2 graduates.

## Risks

| risk | mitigation |
|---|---|
| Cross-slot state staleness — stale entry from a 30-min-old QSO that resumed on a different band would inject wrong templates | TTL (90 s default) + LRU cap (256 entries) + per-band keying once `WithinQsoContext` is band-aware (V2 — V1 single-band is fine for K5ARH's single-rig operation) |
| Self-reinforcing prediction loop | Only emissions that pass the NORMAL Costas + LDPC gate update the table (NOT hb-173's own emissions). Documented in Open Question 1. |
| Pair-FP inflation if multiple QSOs share a freq bin in a pile-up | The pair history requires at least one DIRECTED prior sighting; CQ-vs-CQ pairs are ignored. The freq-bin tolerance ±50 Hz means two simultaneous QSOs at the same audio bin would interfere, but FT8 traffic at 25 Hz bins is sparse enough that this is rare in practice. Threshold sweep in Session 3 will reveal if FP escapes. |
| Diagnostic measures coverage, not lift | Session 3 measures lift directly via A/B eval. Lift = coverage × P(miss \| covered). The diagnostic establishes the upper bound; real lift could be 1/3 to 1/2 of that. Still positive on hard-200 (5–10 decode lift expected). |
| Eval harness shuffles WAVs — cross-slot effect is suppressed in offline eval | This is the same trap as a7's Risk 2. Mitigation: build the chronological-replay tier (Open Question 3) before declaring final V2 ready. V1 eval on the 20% multi-slot population is still informative — the per-session effect generalizes. |
| Coordinator-level lock contention | `parking_lot::RwLock` with reads on the hot path and writes on emission. Decoder reads ~100/slot worst case; emission writes ~10/slot worst case. Negligible vs the decoder's other work. |

## Acceptance criteria (V1 ship)

- hard-200 recall lift ≥ +5 decodes; no recall regression on any tier
- composite score ≥ +0.0005
- fixtures all preserved
- elapsed ≤ +5%
- zero novel FP shape (callsign-continuity filter passes)
- coordinator hot-reload survives a `WithinQsoContext` swap (V1
  default is `within_qso_enable = false` so the swap is a no-op
  until operator flips the config)

## References

- Cross-time ideation T1: `research/ideation/2026-06-01-cross-time.md`
- a7 design (single-callsign sibling): `docs/superpowers/specs/2026-05-31-hb-048-a7-design.md`
- hb-057 design (coordinator-level cross-slot state pattern): `docs/superpowers/specs/2026-05-31-hb-057-median-dt-design.md`
- QSO state machine (phase classifier reference): `pancetta-qso/src/qso_manager.rs::determine_state_transition`
- Autonomous responder's per-callsign memory (analog, different semantics): `pancetta-qso/src/autonomous.rs::recently_responded_to`
- Diagnostic source: `pancetta-research/examples/hb173_within_qso_diagnostic.rs`
- Session 1 journal: `research/experiments/2026-06-01-hb-173-session1.md`
