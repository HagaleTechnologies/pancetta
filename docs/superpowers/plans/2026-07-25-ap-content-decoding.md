# AP Content-Decoding (Ap5) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the 4 gaps identified in `docs/ap-decoding-design.md` §0 — inject enumerated
content-bit hypotheses (report/RR73/RRR/73) as a new AP level (Ap5), rank concurrent QSOs by
priority for the AP context, promote the AP tuning constants to `Ft8Config`, and build the
recall/false-decode-vs-SNR eval harness that determines whether the new `content_ap_enabled` knob
graduates from its default of `false`.

**Architecture:** Extends the existing, production-wired Ap0–Ap4 soft-LLR-injection ladder
(`pancetta-ft8/src/ap.rs` + `decoder.rs`) with a new `ApLevel::Ap5` that injects the same
callsign-field bits as Ap3/Ap4 plus one enumerated hypothesis's content bits (58–76), gated by a
stricter confidence floor and an extended `ap_injection_survived` content-match check. The
coordinator widens its AP context from a single `active_qso` to a priority-ranked, capped list,
using the existing `PriorityScorer`. A new `pancetta-research` example measures the recall vs
false-decode tradeoff before any default changes.

**Tech Stack:** Rust workspace (existing), the FT8 LDPC/LLR pipeline in `pancetta-ft8`, the
existing `pancetta-qso::PriorityScorer`, `pancetta-research`'s example-based eval harness
convention.

**Spec:** `docs/ap-decoding-design.md` — read it first, especially §0 (state of the art), §3
(false-decode control — load-bearing), and §5 (the piece list this plan implements).

## Global Constraints

- **`content_ap_enabled` defaults to `false` and MUST stay `false` after this plan completes,
  regardless of what the Task 7 eval shows.** Flipping the default is a separate, later decision
  made by the operator after reviewing real numbers — not automatic, and not part of this plan's
  definition of done.
- **Soft injection only, never a hard pin.** Content bits are injected the same way callsign bits
  already are (`inject_bit`, `±AP_LLR_MAGNITUDE`, re-normalized) — never forced/unoverridable. This
  is what lets `ap_injection_survived` reject a wrong hypothesis.
- **The single most important new gate**: extend `ap_injection_survived` so a content-AP decode is
  rejected unless the decoded message's content matches the injected hypothesis exactly — not just
  the callsigns (which Ap3/Ap4 already verify). A decode that "succeeds" with drifted content is a
  CRC-coincidence false positive.
- **Trial budget is bounded**: `max_ap_hypotheses` per QSO (order candidates by SNR-seeded
  likelihood, nearest-to-measured-SNR first, so the true report is usually tried first) ×
  `max_ap_qsos` concurrent (ranked by `PriorityScorer::evaluate_cq`). Never iterate every
  hypothesis of every QSO unconditionally.
- **Any new config field needs its `merge_with` line and a regression test in the same task**
  (2026-07-05 config-merge bug class — see CLAUDE.md's §5 guardrail).
- **Bit-exact / A-B gating**: changes to existing Ap0–Ap4 behavior are [BIT-EXACT] (no behavior
  change to shipped levels); new Ap5 behavior is [A/B] once `content_ap_enabled` exists, gated by
  the Task 7 eval before any default changes — which this plan does not make.
- **Subagent rules (standing):** implementers never push / never destructive git; controller
  pushes at batch boundaries. Local `cargo fmt` + `cargo clippy` before each commit.
- **Clean-room note:** do not open any GPL source (WSJT-X, wsjtr, ft8mon, JTDX, MSHV). The prose
  specs already in `research/specs/` (cited in the design doc §6) are the only permitted source
  material beyond this project's own code.

---

## Task 1: Content-hypothesis bit builder

**Files:**
- Modify: `pancetta-ft8/src/ap.rs` (new function near `enumerate_a8_expected_texts`, currently
  line 417)
- Test: `ap.rs`'s existing `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `QsoAp.expected_next_message_texts: Vec<String>` (already populated today, verified
  at `ap.rs:356` — this task adds a consumer, doesn't touch the field itself).
- Produces:

```rust
/// One enumerated content hypothesis, ready for Ap5 injection: the source
/// text (for logging/content-match verification) plus its extracted
/// 19-bit content-field pattern (payload bits 58-76: report/token + i3
/// type bits), and the measured-SNR-seeded ordering key.
#[derive(Debug, Clone)]
pub struct ContentHypothesis {
    pub text: String,
    pub content_bits: [bool; 19],
}

/// Encode each of `qso.expected_next_message_texts` once (via the existing
/// `Ft8Message`/pack path this crate already uses for encoding) and
/// extract payload bits 58-76 from each. Returns hypotheses in the SAME
/// order as the input `Vec<String>` — ordering by SNR-seeded likelihood is
/// the caller's job (Task 3/5), not this pure builder's.
///
/// Returns an empty Vec if `qso.expected_next_message_texts` is empty or
/// any text fails to encode (an encode failure here indicates a bug in
/// the enumerator, not a runtime condition to recover from gracefully —
/// log a warning and skip that one hypothesis, don't panic).
pub fn build_content_hypotheses(qso: &QsoAp) -> Vec<ContentHypothesis> {
    // implementation in Step 3
}
```

- [ ] **Step 1: Read the existing encode path first.** Find how this crate encodes a canonical
  FT8 text string to its 77-bit payload today (grep `pub fn encode\b\|fn try_encode_standard\|
  Ft8Message::from_text\|Ft8Message::parse` in `pancetta-ft8/src/message.rs` and
  `pancetta-ft8/src/encoder.rs` — there should already be exactly one canonical text→payload path
  used elsewhere in this crate; reuse it, do not write a second one).

- [ ] **Step 2: Write the failing test**

```rust
#[test]
fn build_content_hypotheses_extracts_bits_56_to_76() {
    let mut qso = QsoAp::new("W1AW", QsoApProgress::WaitingForConfirmation).unwrap();
    qso = qso.with_expected_texts(["K1ABC W1AW RR73", "K1ABC W1AW RRR", "K1ABC W1AW 73"]);
    let hyps = build_content_hypotheses(&qso);
    assert_eq!(hyps.len(), 3);
    assert_eq!(hyps[0].text, "K1ABC W1AW RR73");
    // The three confirmation hypotheses must produce three DIFFERENT
    // content-bit patterns (if they collided, Ap5 could never
    // distinguish them) -- adjust this assertion once Step 1's real
    // encode path confirms the exact bit layout, but the inequality
    // itself is the load-bearing property to prove.
    assert_ne!(hyps[0].content_bits, hyps[1].content_bits);
    assert_ne!(hyps[1].content_bits, hyps[2].content_bits);
}

#[test]
fn build_content_hypotheses_empty_when_no_expected_texts() {
    let qso = QsoAp::new("W1AW", QsoApProgress::WaitingForReport).unwrap();
    assert!(build_content_hypotheses(&qso).is_empty());
}
```

- [ ] **Step 3: Implement** `ContentHypothesis` + `build_content_hypotheses`, encoding each text via
  the real path found in Step 1 and extracting payload bits 58-76 into a `[bool; 19]` (bits 56-57 belong to the from_callsign/suffix-flag area, not content -- verified byte-exact against the real encoder by Task 1's implementer).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta-ft8 build_content_hypotheses --lib`

- [ ] **Step 5: Commit**

```bash
git add pancetta-ft8/src/ap.rs
git commit -m "feat(ft8): content-hypothesis bit builder for Ap5 (gap 1 of 4, ap-decoding-design.md)"
```

---

## Task 2: `ApLevel::Ap5` injection + content-match survival check

**Files:**
- Modify: `pancetta-ft8/src/ap.rs` (`ApLevel` enum, currently starting line 210; `inject_ap_llrs`,
  line 552)
- Modify: `pancetta-ft8/src/decoder.rs` (`ap_injection_survived`, line 10558)
- Test: both files' existing test modules

**Interfaces:**
- Consumes: Task 1's `ContentHypothesis`.
- Produces: `ApLevel::Ap5(ContentHypothesis)` — carries the specific hypothesis being tried (unlike
  Ap0–Ap4 which are unit variants; Ap5 needs per-attempt data, so it becomes the first
  data-carrying variant. Re-check `ApLevel`'s derives — `Clone`/`Debug`/`PartialEq` etc. — and
  ensure `ContentHypothesis` supports whatever `ApLevel` currently derives, adding derives to
  `ContentHypothesis` as needed).

- [ ] **Step 1: Write the failing test for injection**

```rust
#[test]
fn ap5_injects_callsigns_and_content_bits() {
    let my = MyCallAp::new("W1AW").unwrap();
    let their = QsoAp::new("K1ABC", QsoApProgress::WaitingForConfirmation).unwrap();
    let hyp = ContentHypothesis { text: "K1ABC W1AW RR73".into(), content_bits: [/* fill after Task 1 confirms real values */] };
    let ctx = ApContext { my_call: Some(my), recent_calls: vec![], active_qso: Some(their) };
    let mut llrs = vec![0.0f32; 174];
    inject_ap_llrs(&mut llrs, ApLevel::Ap5(hyp.clone()), &ctx, None);
    // Bits 0-27 (to_callsign=my call) and 29-56 (from_callsign=their call)
    // must carry the SAME sign convention as the existing Ap3 test (mirror
    // whatever assertion style Ap3's own test already uses -- find it
    // first with `rg "fn ap3" pancetta-ft8/src/ap.rs` and match it).
    // Content bits 58-76 must carry hyp.content_bits' sign pattern.
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta-ft8 ap5_injects --lib`
Expected: FAIL — `ApLevel::Ap5` doesn't exist yet.

- [ ] **Step 3: Add the `Ap5` variant and its `inject_ap_llrs` arm**

Add to `ApLevel` (mirror the existing `Ap4` doc-comment style):

```rust
    /// Ap3 (own call at 0-27, partner at 29-56) + inject one specific
    /// enumerated content hypothesis's bits 58-76 (report/token + type).
    /// Soft injection, same ±AP_LLR_MAGNITUDE convention as every other
    /// level -- LDPC/CRC can still override a wrong hypothesis, which is
    /// exactly what makes the Step 4 survival check meaningful.
    Ap5(ContentHypothesis),
```

Add the `Ap5` arm to `inject_ap_llrs`'s match: inject exactly what `Ap3` injects (own call +
partner call — read Ap3's existing arm and reuse its two `inject_28_bits` calls) plus a new loop
injecting `hyp.content_bits` at payload positions 58..77 via the existing `inject_bit` helper.

- [ ] **Step 4: Extend `ap_injection_survived` with the content-match check**

Read the existing `Ap3`/`Ap4` arms in `ap_injection_survived` (`decoder.rs:10558` onward) fully
first — mirror their callsign-verification pattern for the `Ap5` arm's callsign half, then add:

```rust
        crate::ap::ApLevel::Ap5(ref hyp) => {
            // First, everything Ap3/Ap4 verify (own call as to_callsign,
            // partner as from_callsign) -- reuse that logic, don't
            // duplicate it inline (extract a shared helper if Ap3/Ap4's
            // check isn't already a callable function).
            //
            // Then the NEW, load-bearing check for content-AP: the
            // decoded message's canonical text must equal hyp.text
            // exactly. A decode whose content drifted from the injected
            // hypothesis is a CRC-coincidence false positive, not a real
            // rescue -- reject it.
        }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p pancetta-ft8 ap5 --lib`

- [ ] **Step 6: Full crate suite + benchmark unaffected (bit-exact for Ap0-Ap4)**

Run: `cargo test -p pancetta-ft8 --features transmit 2>&1 | tail -20` — confirm no existing
Ap0-Ap4 test regressed (this task must be additive-only to the existing ladder).

- [ ] **Step 7: Commit**

```bash
git add pancetta-ft8/src/ap.rs pancetta-ft8/src/decoder.rs
git commit -m "feat(ft8): ApLevel::Ap5 content injection + content-match survival gate (gaps 1+3 of 4)"
```

---

## Task 3: Multi-QSO ranked AP context

**Files:**
- Modify: `pancetta-ft8/src/ap.rs` (`ApContext`, line 470)
- Test: `ap.rs`'s test module

**Interfaces:**
- Produces:

```rust
pub struct ApContext {
    pub my_call: Option<MyCallAp>,
    pub recent_calls: Vec<RecentCallAp>,
    /// Retained for back-compat with existing Ap1-Ap4 call sites that read
    /// a single QSO -- always set to `active_qsos.first().cloned()` by
    /// whichever constructor populates both, so old and new readers agree.
    pub active_qso: Option<QsoAp>,
    /// Ranked (highest priority first), capped at `MAX_AP_QSOS`. Empty
    /// when no QSOs are in flight. The Ap5 hypothesis loop (Task 6)
    /// iterates this; existing Ap3/Ap4 keep reading `active_qso` unchanged.
    pub active_qsos: Vec<QsoAp>,
}
```

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn active_qso_stays_in_sync_with_active_qsos_first() {
    let qso1 = QsoAp::new("K1ABC", QsoApProgress::WaitingForReport).unwrap();
    let qso2 = QsoAp::new("K2DEF", QsoApProgress::WaitingForReport).unwrap();
    let ctx = ApContext {
        my_call: None,
        recent_calls: vec![],
        active_qso: Some(qso1.clone()),
        active_qsos: vec![qso1.clone(), qso2],
    };
    assert_eq!(ctx.active_qso.as_ref().unwrap().their_call, ctx.active_qsos[0].their_call);
}
```

(This test just pins the invariant in a plain struct literal since `ApContext` has no
constructor yet beyond `Default` — check whether one should be added here or left to Task 5's
coordinator wiring, which is the actual population site; if `ApContext` already has a builder
elsewhere, extend that instead of hand-writing struct literals at every call site.)

- [ ] **Step 2: Add the `active_qsos` field**, update `Default`/any existing constructor to
  initialize it empty, and audit every existing `ApContext { ... }` construction site (`rg
  "ApContext\s*\{" pancetta-ft8/ pancetta/`) to decide whether each needs updating (most will keep
  `active_qsos: vec![]` until Task 5 wires real population).

- [ ] **Step 3: Run tests, then the full crate suite** — confirm additive-only (existing
  `active_qso`-reading code paths must be bit-exact unchanged; this task doesn't touch them, only
  adds the new field).

- [ ] **Step 4: Commit**

```bash
git add pancetta-ft8/src/ap.rs
git commit -m "feat(ft8): ApContext.active_qsos ranked list, active_qso kept for back-compat (gap 2 of 4)"
```

---

## Task 4: `Ft8Config` AP tuning knobs

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` (`Ft8Config`, line 219)
- Test: `decoder.rs`'s test module

**Interfaces:**
- Produces (field names exactly as named in the design doc §3.5):

```rust
    /// Master switch for content-AP (Ap5) injection. Default false --
    /// graduated only after the Task 7 eval harness supports flipping it,
    /// per docs/ap-decoding-design.md's ship gate. This plan does NOT flip it.
    pub content_ap_enabled: bool,
    /// Soft-LLR injection magnitude for all AP levels (Ap1-Ap5). Existing
    /// hardcoded AP_LLR_MAGNITUDE=15.0 becomes this field's default;
    /// promoting it to config doesn't change today's behavior.
    pub ap_llr_magnitude: f32,
    /// Confidence floor for Ap1-Ap4 decodes (existing hardcoded
    /// MIN_AP_DECODE_CONFIDENCE=0.55 becomes this field's default).
    pub min_ap_decode_confidence: f32,
    /// Stricter confidence floor for Ap5 (content-AP) decodes specifically
    /// -- content injection is higher-risk than callsign-only AP.
    pub min_content_ap_confidence: f32,
    /// Max content hypotheses tried per QSO in the Ap5 loop (Task 6),
    /// SNR-seeded ordering (nearest-to-measured-SNR first).
    pub max_ap_hypotheses: usize,
    /// Max concurrent QSOs represented in ApContext.active_qsos,
    /// priority-ranked (Task 5).
    pub max_ap_qsos: usize,
```

Defaults: `content_ap_enabled: false`, `ap_llr_magnitude: 15.0`, `min_ap_decode_confidence: 0.55`,
`min_content_ap_confidence: 0.60`, `max_ap_hypotheses: 8`, `max_ap_qsos: 4` — every default
preserves today's behavior exactly (the two existing consts + two new tunable-but-inert-by-default
knobs).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn ft8_config_ap_knobs_default_preserve_current_behavior() {
    let cfg = Ft8Config::default();
    assert!(!cfg.content_ap_enabled);
    assert_eq!(cfg.ap_llr_magnitude, 15.0);
    assert_eq!(cfg.min_ap_decode_confidence, 0.55);
    assert_eq!(cfg.min_content_ap_confidence, 0.60);
    assert_eq!(cfg.max_ap_hypotheses, 8);
    assert_eq!(cfg.max_ap_qsos, 4);
}
```

Find `Ft8Config`'s existing `merge_with`-equivalent or config-carrying pattern first — this struct
lives in `pancetta-ft8`, not `pancetta-config`, so check whether it has its own merge/validate
convention or whether the CLAUDE.md §5 config-merge guardrail applies to a *different*,
higher-level config struct that wraps or mirrors `Ft8Config`'s fields (grep `Ft8Config` in
`pancetta-config/src/` — if a mirrored/duplicated struct exists there for hot-reload, this task's
new fields need to be added there too, with `merge_with` + a regression test, matching the
established pattern from this session's item-6/item-2 work).

- [ ] **Step 2: Run test to verify it fails, then implement, then verify it passes.**

- [ ] **Step 3: If a `pancetta-config`-side mirror exists**, add the matching fields there with
  `merge_with` + the regression test (this sub-step is NOT optional if the mirror exists — the
  2026-07-05 bug class is exactly "new field added to one config struct, forgotten in the
  merge/hot-reload path").

- [ ] **Step 4: Full crate suite.**

- [ ] **Step 5: Commit**

```bash
git add pancetta-ft8/src/decoder.rs # + pancetta-config/... if Step 3 applied
git commit -m "feat(ft8): promote AP tuning constants to Ft8Config knobs, all default-preserving (gap 3 of 4)"
```

---

## Task 5: Coordinator priority-ranking wiring

**Files:**
- Modify: `pancetta/src/coordinator/qso.rs` (wherever `active_qso_ap` is written, currently around
  line 1194/1563 — re-verify)
- Modify: `pancetta/src/coordinator/mod.rs` (the `active_qso_ap` field, line 685, and its
  `RwLock<Option<QsoAp>>` type — this needs to become `RwLock<Vec<pancetta_ft8::QsoAp>>` or a
  wrapper carrying both the ranked list and the back-compat single value; pick whichever keeps
  `ft8.rs`'s existing single-QSO read site working with minimal churn)
- Test: coordinator test module for the ranking logic

**Interfaces:**
- Consumes: Task 3's `ApContext.active_qsos`, `pancetta_qso::PriorityScorer::evaluate_cq(callsign:
  &str, grid: Option<&str>, snr: i8, freq_hz: f64) -> f64` (confirmed real signature,
  `pancetta-qso/src/priority.rs:488`).
- Produces: the coordinator's per-slot AP-context-population code (wherever it currently builds a
  single `QsoAp` per active QSO) now ranks ALL currently-active QSOs by `evaluate_cq` and writes
  the top-`max_ap_qsos` (Task 4's config knob) into the shared state, highest-priority first.

- [ ] **Step 1: Read the current write site fully.** Find exactly where/how the coordinator
  currently constructs the single `QsoAp` written into `active_qso_ap` (grep `active_qso_ap` in
  `pancetta/src/coordinator/qso.rs`) and how it gets each active QSO's data (callsign, progress,
  SNR, grid — needed for `evaluate_cq`). If the coordinator doesn't currently have easy access to
  every active QSO's grid/SNR at that call site, trace where that data IS available (likely the
  same `QsoManager`/`QsoProgress` structures this session's item-6 work already touched) rather
  than inventing a new data path.

- [ ] **Step 2: Write a failing test** for the ranking logic specifically (a pure function taking
  a list of `(callsign, grid, snr, freq, progress)` tuples + a `PriorityScorer` and returning the
  top-N `QsoAp`s ranked descending — keep this testable independent of the full coordinator
  wiring, mirroring how `PriorityScorer` itself is unit-tested without a live coordinator).

- [ ] **Step 3: Implement.** Extract a small ranking helper, call it from the existing per-slot
  write site, populate `active_qsos` (Task 3) with the ranked+capped list and `active_qso` with
  `active_qsos.first().cloned()` for back-compat.

- [ ] **Step 4: Also priority-order `recent_calls` (Ap2 candidates)** per the design's §1: rank the
  existing 20-entry recent-callsign pool by `evaluate_cq` too, capping via the same or a related
  knob — re-check the design doc's exact wording before deciding whether this needs its own config
  knob or reuses `max_ap_hypotheses`; if genuinely ambiguous, implement the simpler option (no new
  knob, just re-order the existing pool) and note the choice in your report.

- [ ] **Step 5: Full workspace suite.**

Run: `cargo test --workspace --features transmit 2>&1 | tail -40`

- [ ] **Step 6: Commit**

```bash
git add pancetta/src/coordinator/qso.rs pancetta/src/coordinator/mod.rs pancetta/src/coordinator/ft8.rs
git commit -m "feat(coordinator): rank concurrent QSOs by PriorityScorer for the AP context (gap 2 of 4)"
```

---

## Task 6: Wire Ap5 into the decode hot path

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` (`par_try_ap_decode`, line 9839; `try_ap_decode`, line
  6206 — the serial twin, keep both in sync per the existing Ap0-Ap4 pattern)

**Interfaces:**
- Consumes: Task 1's `build_content_hypotheses`, Task 2's `ApLevel::Ap5`, Task 3's
  `ApContext.active_qsos`, Task 4's `Ft8Config.content_ap_enabled`/`max_ap_hypotheses`/
  `max_ap_qsos`/`min_content_ap_confidence`.

- [ ] **Step 1: Read `par_try_ap_decode`'s full existing Ap0→Ap1→Ap2→Ap3→Ap4 sequence** (starting
  line 9839) before changing anything — this is a large, delicate hot-path function with
  whitening/impulse-robust-LLR/SNR-estimation steps already interleaved; understand exactly where
  today's AP-level loop ends and the CRC/confidence-gate/survival checks happen, so the new Ap5
  loop reuses the SAME post-BP pipeline (CRC → `is_plausible` → `ap_injection_survived` →
  confidence floor → suspicion gate) rather than duplicating it.

- [ ] **Step 2: Write a failing integration test.** Construct a synthetic scenario: a real encoded
  FT8 signal at a weak SNR where AP0-AP4 fail but the correct content hypothesis (known from the
  test's own QSO-state setup) would rescue it via Ap5. Mirror whatever existing test in this file
  proves an Ap3/Ap4 rescue today (grep for one, e.g. via `rg "fn.*ap[34].*rescue\|fn.*ap4.*test"
  pancetta-ft8/src/decoder.rs`) and adapt its structure — do not invent test scaffolding from
  scratch if a working pattern already exists.

- [ ] **Step 3: Implement the Ap5 loop.** Gated on `ctx.ft8_config.content_ap_enabled` (default
  false — with the flag off, this whole loop must not run, zero behavior change). When enabled:
  after the existing Ap1-Ap4 attempts (only reached if they all failed, matching the existing
  "AP runs after AP0 fails" pattern), for each QSO in `ApContext.active_qsos` (already capped at
  `max_ap_qsos` by Task 5) build its content hypotheses (Task 1), order by SNR-seeded likelihood
  (nearest to the candidate's own measured SNR first — the measured SNR is already computed
  earlier in this function as `snr_db`, reuse it), cap at `max_ap_hypotheses`, and try each via
  `ApLevel::Ap5` through the same LLR-clone → inject → normalize → decode → CRC pipeline Ap3/Ap4
  already use. Early-exit the hypothesis loop on the first CRC-valid, `ap_injection_survived`-passing
  decode that also clears `min_content_ap_confidence`.

- [ ] **Step 4: Apply the same change to `try_ap_decode`** (the serial twin, line 6206) — check
  whether it's a thin wrapper around shared logic or a genuine duplicate; if duplicate, mirror the
  Step 3 change there too (existing Ap0-Ap4 handling presumably already keeps both in sync the
  same way — follow that established pattern).

- [ ] **Step 5: Verify bit-exact with the flag off.**

Run: `cargo test -p pancetta-ft8 --features transmit 2>&1 | tail -30` — every existing test must
still pass unchanged (flag defaults false).

- [ ] **Step 6: Verify the new rescue test passes with the flag manually enabled** (temporarily, in
  the test itself — set `content_ap_enabled: true` on the test's own `Ft8Config`, not the crate
  default).

- [ ] **Step 7: Benchmark.** `cargo run --release -p pancetta-ft8 --example profile_decode --
  native 5` with the flag off — confirm no measurable regression (the loop must not execute at
  all when disabled).

- [ ] **Step 8: Commit**

```bash
git add pancetta-ft8/src/decoder.rs
git commit -m "feat(ft8): wire Ap5 content-hypothesis loop into the decode hot path, gated content_ap_enabled=false (gap 1 of 4, completes injection mechanism)"
```

---

## Task 7: Eval harness — recall vs false-decode vs SNR

**⚠️ CARRY-FORWARD FROM TASK 6 REVIEW — READ THIS FIRST, IT CHANGES THIS TASK'S FIRST STEP:**
Task 6's implementer searched real audio via 3 methodologies and found **no natural window where
AP0-AP4 (the existing ladder) all fail but Ap5 (content injection) succeeds** at this decoder's
default LDPC strength — the rescue test had to isolate Ap5 by structurally disabling AP2-4 in the
test's `ApContext` (a legitimate way to prove the *wiring* is correct, per Task 6's own review, but
NOT proof that Ap5 provides real recall lift over the full ladder). **This is the single most
consequential open question for this task and for whether `content_ap_enabled` ever graduates**: if
this harness's synthetic AWGN/burst corruption *also* can't construct a window where the full AP0-4
ladder fails but Ap5 rescues it, the feature has no demonstrable benefit to measure, regardless of
how the eval numbers otherwise look. **Before running the full §4A/§4B sweep, first verify the
harness's corruption methodology can construct at least a handful of genuine AP0-4-fails/Ap5-rescues
cases** (e.g. a small pilot run at a few SNR points with the full ladder enabled, checking the
count of such cases is non-zero) — if it cannot, report that finding prominently in Step 6's
journal entry as the primary result, not a footnote, since it would mean the ship-gate question
("does recall rise meaningfully") has a knowable answer (no) before the rest of the sweep even
needs to run.

**Files:**
- Create: `pancetta-research/examples/ap5_content_recall_fp_sweep.rs`
- Test: none (research example, excluded from CI per CLAUDE.md's `pancetta-research` note)

**Interfaces:**
- Consumes: Tasks 1-6's full Ap5 mechanism (this is the first real exercise of it beyond unit
  tests).

- [ ] **Step 1: Read the existing synthetic-injection conventions this harness must mirror.** Read
  `pancetta-research/examples/hb048_a7_synthetic_injection.rs` (AP-adjacent synthetic injection,
  closest existing precedent) and `pancetta-research/examples/batch30_snr_recall_curve.rs` (SNR
  sweep conventions) in full before writing anything — match their CLI-arg style, AWGN-generation
  helper (reuse if one already exists in `pancetta-research/src/`, don't reimplement), and output
  format.

- [ ] **Step 2: Implement the synthetic sweep** per `docs/ap-decoding-design.md` §4A exactly:
  - Generate two-station QSO exchange sequences with the existing encoder/modulator, one encoded
    message per QSO stage.
  - SNR sweep: -24..-6 dB, 2 dB steps, many trials per point (match the existing sweep examples'
    trial-count convention — likely 100-1000/point, check what `batch30_snr_recall_curve.rs` uses
    and mirror it).
  - Build the `ApContext` the coordinator would actually have at each stage (correct partner call
    + progress + Task 1's enumerated hypothesis set).
  - **Recall**: fraction of true partner messages recovered, AP5-on vs AP5-off, per SNR point.
  - **False-decode, protocol (i) wrong-context**: feed Ap5 a mismatched QSO context (wrong partner
    call or wrong stage) over the TRUE audio at each SNR point; count decodes that pass every gate
    (a genuine AP false decode — the prior fabricated a codeword).
  - **False-decode, protocol (ii) noise-only**: feed Ap5 context over pure AWGN (no real signal);
    count decodes.
  - Report the false-decode rate as decodes-per-slot under (i)+(ii), compared against the AP-off
    baseline's spurious rate.

- [ ] **Step 3: Implement the tradeoff-curve report** across the knob vector (`ap_llr_magnitude`,
  `min_content_ap_confidence`, `max_ap_hypotheses`) per SNR, per §4's requirement — at minimum, run
  the default knob values plus 2-3 alternate points per knob to show the tradeoff shape, matching
  whatever sweep-reporting convention the existing `batch*_sweep.rs` examples use for multi-knob
  output.

- [ ] **Step 4: Implement the corpus A/B (§4B, secondary).** Run the real decoder over whatever
  corpus the existing hard-tier examples use (check `research/README.md` for the current
  corpus-selection convention) with `content_ap_enabled` on vs off, reporting decode-rate delta and
  ft8_lib-truth precision as an FP proxy (per `docs/gap-analysis.md`'s established method) + elapsed
  time.

- [ ] **Step 5: Run the harness.** `cargo run --release -p pancetta-research --example
  ap5_content_recall_fp_sweep -- --help` first to confirm the CLI works, then run it for real
  against the synthetic sweep (§4A) at minimum — the corpus A/B (§4B) additionally if the corpus is
  available in this environment; if it isn't (e.g. `~/.pancetta/recordings` doesn't exist here),
  report that gap explicitly rather than fabricating results.

- [ ] **Step 6: Write the results to `research/experiments/2026-07-25-ap5-content-decoding.md`**
  (mirror the journal format of an existing entry in that directory) — recall-Δ, false-decode-Δ,
  the tradeoff curve, and an explicit statement of whether the design's ship-gate criteria (§4,
  "recall rises meaningfully AND false-decode rate stays within budget AND corpus A/B shows
  non-negative decode-rate at non-negative precision") are met — **as information for the operator
  to decide on, not as authorization to flip `content_ap_enabled`.**

- [ ] **Step 7: Commit**

```bash
git add pancetta-research/examples/ap5_content_recall_fp_sweep.rs research/experiments/2026-07-25-ap5-content-decoding.md
git commit -m "research(ft8): Ap5 content-AP recall/false-decode-vs-SNR eval harness + results (gap 4 of 4)"
```

---

## Task 8: Docs + final gate

**Files:**
- Modify: `docs/ap-decoding-design.md` (status header: mark all 4 gaps closed at the mechanism
  level; record Task 7's eval outcome and that `content_ap_enabled` remains `false` pending
  operator review)
- Modify: `CLAUDE.md` if a new invariant is worth one line (keep under budget)

- [ ] **Step 1: Update the status header** with Task 7's actual eval numbers (not a placeholder —
  pull the real figures from Task 7's report/journal).
- [ ] **Step 2: Full workspace suite one final time:** `cargo test --workspace --features transmit`.
- [ ] **Step 3: `cargo fmt --check` + `cargo clippy --workspace --exclude pancetta-research
  --features transmit`.**
- [ ] **Step 4: Commit docs; controller pushes the batch and opens a PR.**

---

## Self-review notes (author)

- Spec coverage: gap 1 (content injection) → Tasks 1, 2, 6; gap 2 (multi-QSO ranking) → Tasks 3, 5;
  gap 3 (tradeoff knobs) → Task 4 (+ Task 2's `min_content_ap_confidence` consumption); gap 4 (eval
  harness) → Task 7.
- Type consistency: `ContentHypothesis` (Task 1) flows into `ApLevel::Ap5` (Task 2) flows into the
  hot-path loop (Task 6); `ApContext.active_qsos` (Task 3) is populated by Task 5 and consumed by
  Task 6; `Ft8Config`'s 6 new fields (Task 4) are consumed by Tasks 5 (max_ap_qsos) and 6 (the
  rest).
- Deliberately left as an implementer judgment call rather than over-specified: the exact shared
  `RwLock` type change in Task 5 (single `Option<QsoAp>` → some ranked-list-carrying type) — the
  real current code needs reading before locking that in, per that task's own Step 1.
- Known risk flagged for whoever reviews Task 6: this is the highest-risk task in the plan (deep
  hot-path integration in a 22K-line file) — recommend the same "most capable model" review
  treatment this session gave the equivalent hot-path task in the task-supervision plan.
