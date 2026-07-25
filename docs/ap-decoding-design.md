# A-priori (AP) decoding from live QSO context — design

**Status (2026-07-25): scope confirmed, moving to implementation.** The operator confirmed
building the full design below — content-hypothesis injection (§2), multi-QSO priority ranking
(§1), the `Ft8Config` tradeoff knobs (§3.5), and the §4 eval harness — with one binding
constraint: **`content_ap_enabled` stays `false` regardless of eval outcome**; flipping the
default is a separate decision made after seeing real recall/false-decode numbers, not
automatic. This updates §5's original "STOP before live wiring" framing — the mechanism itself
now ships (default-off), only the *default flip* stays gated. The prerequisite AP1-AP4 injection
bug (backwards callsign-field bit offsets, found 2026-07-07) is already fixed and independently
re-verified — the injection engine this design builds on is correct as of today.

Design doc for using live QSO state to generate a-priori message hypotheses that
pull weak FT8 signals out of the noise, recovering decodes **without inflating the
false-decode rate**. **This is a plan, not an implementation** — it stops at a
detailed design + an eval harness spec, before any wiring into the live engine loop.

Clean-room note: the AP *ladders* referenced here derive from prose specs already in
`research/specs/` (`spec-ft8mon-apriori-bit-prior.md`, `spec-wsjtx-improved-a8-
decoding.md`, `spec-wsjtr-cross-sequence-a7.md`), each a GPL-source clean-room
extraction. This doc cites those prose specs only — no GPL source is read or ported.

## 0. State of the art (what already exists — read first)

Pancetta already has a **mature, production-wired AP injection engine**. The design
below bridges specific gaps in it, not a greenfield build.

**Exists and live (`pancetta-ft8/src/ap.rs`, `decoder.rs`, `pancetta/src/coordinator/`):**
- **The Ap0–Ap4 LLR-injection ladder** (`ap.rs::inject_ap_llrs`): AP enters as a
  **soft LLR bias of ±`AP_LLR_MAGNITUDE` (=15.0)** on known payload bits *before*
  LDPC/OSD (then re-normalized), NOT a hard pin — so LDPC parity can overrule a
  wrong prior. Ap1 = my-call (bits 28–55); Ap2 = my-call + a specific recent caller
  (bits 0–27); Ap3 = QSO-partner (0–27) + my-call (28–55); Ap4 = Ap3 + i3 type bits
  74–76 forced 0. Runs per candidate **only after the AP0 pass fails**, serial
  (`try_ap_decode`) and parallel (`par_try_ap_decode`), gated by
  `MIN_SYNC_SCORE_FOR_AP = 3.0`.
- **Live QSO-state → decoder wiring:** the coordinator maps each `QsoState` change
  to a `QsoAp` and shares it via `active_qso_ap: Arc<RwLock<Option<QsoAp>>>`
  (`coordinator/mod.rs:472`, written in `coordinator/qso.rs:1450-1498`); the decode
  thread reads it, plus a 20-entry recent-callsign pool and the station call, into
  `ApContext { my_call, recent_calls, active_qso }` every window
  (`coordinator/ft8.rs:514-565`). **AP context is populated from live QSO state, not
  static.**
- **Strong false-decode gates** on every injection-AP decode: `MIN_SYNC_SCORE_FOR_AP`
  → CRC-14 → `is_plausible` (rejects FreeText/Telemetry/contest) →
  **`ap_injection_survived`** (the AP-identity gate: verifies the LDPC output KEPT
  the injected callsign(s), else it's a CRC-coincidence FP) → `MIN_AP_DECODE_
  CONFIDENCE = 0.55` (vs 0.41 for non-AP) → suspicion gate (`suspicion_score ≥ 2`
  under `SCRUTINY_THRESHOLD = 0.65`).
- **Template AP (a7)**, default-OFF: within-window (`a7_enabled`) and cross-sequence
  (`cross_sequence_a7_enabled`) template cross-correlation against residual LLRs
  (`snr7 ≥ 6.0`, `snr7b ≥ 1.8`, ≤32 templates/call). a7 emits the template's own
  text on correlation — no LDPC/CRC — so its only gate is the snr7/snr7b thresholds.

**The gaps this design bridges:**
1. **Content-bit hypotheses are enumerated but never injected.** The specific
   expected message content — report value, `RR73`/`RRR`/`73` — for the current QSO
   stage is enumerated (`ap.rs::enumerate_a8_expected_texts`, ~24 report / 3
   confirmation strings) but is used ONLY to relax a confidence gate
   (`a8_text_matches`, behind `a8_qso_state_ap_enabled`, **default OFF**). It is
   **never turned into an LLR prior on the content bits (56–76)** — `inject_ap_llrs`
   touches only callsign bits (0–55) + type bits. This is the single biggest unused
   recovery lever: for a committed QSO the legal next message collapses to ~10
   candidates (a8/a7 specs), and injecting that content is what buys the ~6–8 dB.
2. **Multi-QSO AP is single-slot, last-writer-wins.** `ApContext.active_qso` is
   `Option<QsoAp>` (one QSO); under N concurrent autonomous QSOs it reflects
   *whichever changed state most recently*. The **priority engine
   (`PriorityScorer`) is not wired into the AP path at all**.
3. **No sensitivity/false-decode tradeoff knob.** `AP_LLR_MAGNITUDE`,
   `MIN_AP_DECODE_CONFIDENCE`, `MIN_SYNC_SCORE_FOR_AP` are hardcoded consts;
   injection AP has no on/off or magnitude config.
4. **No joint recall + false-decode-rate-vs-SNR eval.** Existing AP evals report
   recall only (`ap_recovery_ceiling`, `hb087_*`) or corpus precision without SNR
   stratification (`batch75b_*`). No harness measures decodes-recovered AND
   false-decode-rate vs SNR with AP on/off — the exact tradeoff the brief requires.

## 1. Hypothesis generation from QSO state

**What is knowable per stage** (from the QSO state machine, `pancetta-qso/src/
states.rs`; every non-opening state has BOTH callsigns known, only content varies):

| QSO state | Known | Expected DX message → hypothesis set |
|---|---|---|
| `CallingCq` | my call | a `CqResponse` `<my> <their?> <grid>` — their call UNKNOWN ⇒ callsign-AP not applicable; fall back to Ap1 (my-call only) + `recent_calls` |
| `RespondingToCq` / `WaitingForReport` | my + their call | `<their> <my> <report>` — enumerate report ∈ {−22..0 step 2, with/without `R`} ≈ 24 content hypotheses |
| `SendingReport` | my + their call, our report | `<their> <my> R<report>` — same ~24, `R`-prefixed |
| `WaitingForConfirmation` / `SendingConfirmation` | my + their call, both reports | `<their> <my> {RR73, RRR, 73}` — **3** content hypotheses |

The enumerator for this already exists (`enumerate_a8_expected_texts`); the design
**reuses it as the hypothesis source** and extends injection to consume it (§2).

**Per-hypothesis content bits.** Each enumerated text is a full FT8 message; encode
it once (the existing `Ft8Message`/pack path) to get its 77-bit payload, and extract
the **content bits 56–76** (report/grid + type). Those become the per-hypothesis
injection pattern layered on top of the always-injected callsign bits (0–55). Cache
per QSO stage (the set changes only on state transition), so the hot path does no
re-encoding.

**Multi-QSO + priority ranking (the anti-brute-force rule).** Do NOT inject every
hypothesis of every in-flight QSO — the false-decode rate scales with total trials
(§3). Instead:
- Widen the shared AP context from `Option<QsoAp>` to a **ranked, capped
  `Vec<QsoAp>`** (`ApContext.active_qsos: Vec<QsoAp>`, bounded `MAX_AP_QSOS`, e.g. 4).
- Rank concurrent QSOs by the existing **`PriorityScorer::evaluate_cq(callsign, grid,
  snr, freq) -> f64`** (`pancetta-qso/src/priority.rs`), which already scores
  needed-DXCC / ATNO / needed-grid / rarity / SNR. The coordinator writes the
  top-`MAX_AP_QSOS` QSOs (by priority × recency) into the context each slot.
- Within a QSO, the hypothesis set is already tiny (3–24); cap reports to the most
  likely band (e.g. −24..−3, the common on-air range) via a `MAX_AP_HYPOTHESES`
  knob and order them by prior likelihood (reports cluster near the actual SNR — seed
  the enumeration order from the candidate's measured SNR so the true report is tried
  first, minimizing trials before CRC hit).
- Rank `recent_calls` (Ap2) candidates by `evaluate_cq` too, and cap — today the
  pool is 20 by raw SNR; priority-ordering + a cap cuts Ap2 trials on low-value calls.

This keeps the injected-hypothesis budget per candidate to `O(callsign-AP levels +
MAX_AP_HYPOTHESES × MAX_AP_QSOS)`, small and priority-weighted, not `2^77`.

## 2. Injection mechanism

**Reuse the existing soft-LLR-bias path** (`decoder.rs::par_try_ldpc_with_ap`): clone
base LLRs → `inject_ap_llrs` → `normalize_llrs` → `decode_soft_with_features` →
CRC/parse. The design adds one step: a new AP level (call it **Ap5 / content-AP**)
that injects the callsign bits (as Ap3/Ap4) **plus** a specific hypothesis's content
bits 56–76:

```
Ap3/Ap4 (today):  inject their-call[0..28], my-call[28..56], (Ap4) type[74..77]
Ap5 (new):        Ap3 bits  +  inject content[56..77] from ONE enumerated hypothesis
```

- **Soft, not hard.** Keep the ±`AP_LLR_MAGNITUDE` soft-prior convention — a wrong
  content hypothesis must be overridable by the real signal so `ap_injection_survived`
  can reject it (§3). (Hard-pinning content bits would manufacture a valid codeword
  from noise — exactly the false-decode failure mode.)
- **Hook site.** Extend the per-candidate AP loop (`par_try_ap_decode`,
  `decoder.rs:7349-7537`): after the existing Ap1→Ap2→Ap3→Ap4 attempts, iterate the
  ranked hypothesis set of each ranked QSO, calling `try_ldpc_with_ap` with the
  content pattern. Early-exit the hypothesis loop on the first CRC-valid,
  survival-passing decode (the true content, ordered first by SNR seeding, is
  normally hit immediately).
- **Relationship to a7/a8.** This is the **LLR-injection analogue** of the a8/a7
  template approach: a7 cross-correlates a pre-encoded template against residual LLRs
  (no LDPC), whereas content-AP injects the template's content bits and lets LDPC+CRC
  adjudicate. Injection composes with the existing OSD/BP recovery and reuses the
  identity gate; a7 is a separate, complementary path (keep both, default-OFF for the
  new one). Cross-sequence a7 (previous-slot seeds, `cross_sequence_a7_enabled`)
  remains the mechanism for rescuing a partner *not yet in a tracked QSO*; content-AP
  covers the *committed-QSO* partner.

## 3. False-decode control (the load-bearing section)

Content-AP raises false-alarm probability because each injected hypothesis is another
CRC-14 trial, and CRC-14 collides at ≈2⁻¹⁴ per trial. Total AP false decodes scale
with (candidates × AP levels × hypotheses). Controls, in layers:

1. **Keep every existing injection-AP gate** (they already work): `MIN_SYNC_SCORE_FOR_
   AP = 3.0` (never inject on noise) → CRC-14 → `is_plausible` → **`ap_injection_
   survived`** → AP confidence floor → suspicion gate.
2. **Extend `ap_injection_survived` to content.** Today it verifies the decoded
   callsigns match the injected ones (identity). For content-AP, additionally verify
   the **decoded message content matches the injected hypothesis** (the report/token
   the LDPC output carries equals the one injected). A decode whose content drifted
   from the hypothesis is a CRC-coincidence FP → drop. This is the single most
   important new gate: it makes a wrong-hypothesis false decode nearly impossible,
   because the injected content must *survive* as-is.
3. **Bound the trial count** (§1): `MAX_AP_HYPOTHESES` per QSO, `MAX_AP_QSOS`
   concurrent, priority-ranked, SNR-seeded ordering + first-hit early exit. Fewer
   trials ⇒ proportionally fewer collision FPs.
4. **A stricter floor for content-AP.** Content injection is higher-risk than
   callsign-only AP, so gate it at a **separate, higher confidence floor**
   (`min_content_ap_confidence`, default ≥ `MIN_AP_DECODE_CONFIDENCE`). This is one
   arm of the tradeoff knob.
5. **The explicit sensitivity ↔ false-decode tradeoff knob** (the brief's ask —
   promote today's hardcoded consts to `Ft8Config`, defaults preserving current
   behavior):
   - `ap_llr_magnitude: f32` (default 15.0) — higher = stronger prior = more recall,
     more FPs.
   - `min_ap_decode_confidence: f32` (default 0.55) and `min_content_ap_confidence:
     f32` (default e.g. 0.60) — the primary FP dial.
   - `max_ap_hypotheses: usize` (default e.g. 8) and `max_ap_qsos: usize` (default 4)
     — trial-budget dials.
   - `content_ap_enabled: bool` (default **false**) — the whole content-AP path ships
     off, graduated only by the §4 eval.
   A single documented "AP aggressiveness" preset can map one operator-facing level to
   this vector, but the knobs stay independent for research sweeps.
6. **a7 note.** If cross-sequence a7 is graduated alongside, adopt the spec-
   recommended **weighted-Hamming `dmin`/`dmin2` gate** (spec-wsjtr-cross-sequence-a7)
   rather than reusing the hb-048 `snr7/snr7b` thresholds — the specs report the
   dmin2/dmin ratio (≥1.3–1.4) as the better false-decode discriminator. Out of scope
   to change here, but flagged.

## 4. Eval protocol (recall AND false-decode vs SNR — the ship gate)

A fix that recovers decodes but pushes false decodes past threshold is a **failure**.
The harness must measure both, stratified by SNR, against the no-AP baseline. New
`pancetta-research` example (mirrors `gap_confidence_floor_sweep.rs` conventions):

**A. Synthetic weak-signal QSO sequences (primary — gives ground truth for FP).**
- Generate two-station QSO exchanges with the existing encoder/modulator: for each
  QSO stage, encode the *true* partner message, modulate, add AWGN at a **SNR sweep**
  (e.g. −24..−6 dB, 2 dB steps, many trials per point).
- Build the `ApContext` the coordinator *would* have at that stage (correct partner
  call + progress + the enumerated hypothesis set).
- **Recall:** fraction of true partner messages recovered, AP-on vs AP-off, per SNR.
- **False-decode — the critical measurement:** two decoy protocols run at every SNR:
  (i) **Wrong-context injection** — feed AP a *mismatched* QSO context (wrong partner
  call / wrong stage) over the true audio, and count any decode that passes all gates
  (a true AP false decode: the prior fabricated a codeword). (ii) **Noise-only
  injection** — feed AP context over pure noise (no signal) and count decodes. The
  false-decode rate is decodes-per-slot under (i)+(ii); it MUST stay at or below the
  AP-off baseline's spurious rate + a small budget.
- Report the **tradeoff curve**: recall-Δ vs false-decode-Δ across the knob vector
  (`ap_llr_magnitude`, `min_content_ap_confidence`, `max_ap_hypotheses`), per SNR.

**B. Corpus A/B (secondary — realism / regression).** Run the real decoder over the
curated hard tiers + a `~/.pancetta/recordings` stride sample with content-AP on vs
off (context reconstructed from the decode stream), reporting decode-rate Δ and, as an
FP proxy, ft8_lib-truth precision (as in `docs/gap-analysis.md`) + elapsed. Note the
gap-analysis caveat: much of the native miss set is *sync* misses AP can't touch, so
the corpus recall lift is bounded by the committed-QSO subset — the synthetic harness
(A) is where the real recall+FP tradeoff is characterized.

**Ship gate:** graduate `content_ap_enabled` (and/or a knob preset) only if, at a
fixed operating point, synthetic recall rises meaningfully AND the false-decode rate
stays within budget across the SNR sweep AND the corpus A/B shows non-negative
decode-rate at non-negative precision and bounded elapsed (bootstrap CI, per the
project's A/B discipline). Otherwise document the null and leave it default-off.

## 5. Rust core — the pieces to build (design-only; STOP before live wiring)

When implemented (separately — this doc stops here):
- `pancetta-ft8/src/ap.rs`: a content-hypothesis builder (enumerated text →
  cached content-bit patterns 56–76 per stage), an `ApLevel::Ap5`/content arm in
  `inject_ap_llrs`, and `ApContext.active_qsos: Vec<QsoAp>` (ranked, capped) beside
  the retained `active_qso` for back-compat.
- `pancetta-ft8/src/decoder.rs`: extend `par_try_ap_decode`/`try_ap_decode` with the
  ranked-hypothesis loop + early exit; extend `ap_injection_survived` with the
  content-match check; add the `Ft8Config` knobs (§3.5), all defaulting to today's
  behavior (`content_ap_enabled=false`).
- `pancetta/src/coordinator/`: rank concurrent QSOs via `PriorityScorer::evaluate_cq`
  and write the top-`MAX_AP_QSOS` into `active_qsos` each slot; priority-order
  `recent_calls`. (This is the live-engine wiring — **designed but NOT built here**.)
- `pancetta-research/examples/ap_recall_fp_sweep.rs`: the §4 synthetic + corpus
  harness emitting the recall/false-decode-vs-SNR tradeoff curve.
- **Do NOT** change `AP_LLR_MAGNITUDE`/floors' effective defaults, enable content-AP,
  or wire the coordinator ranking into production until the §4 eval graduates it.

## 6. Provenance + adjacent mechanisms (clean-room)

- **a8** (`spec-wsjtx-improved-a8-decoding.md`): committed-QSO legal-next-message
  collapse (~10 candidates), the direct inspiration for content-AP. The spec's
  *early frequency-locked pre-pass* (decode the partner 0.5–1 s sooner) is a **latency**
  benefit, out of scope here (this design targets recall, and pancetta decodes whole
  windows synchronously) — noted as a separate future item.
- **cross-sequence a7** (`spec-wsjtr-cross-sequence-a7.md`): previous-opposite-parity-
  slot seeds → ~206 reply candidates, weighted-Hamming `dmin/dmin2` gate. Pancetta's
  `cross_sequence_a7_enabled` path is the adaptation; the design keeps it as the
  complementary "partner not yet in a tracked QSO" mechanism and flags the `dmin/dmin2`
  gate upgrade.
- **ft8mon statistical bit-prior** (`spec-ft8mon-apriori-bit-prior.md`): a diffuse
  174-bit empirical `P(bit=1)` prior fused into initial LLRs once before BP —
  **orthogonal** to QSO-context AP (a structural prior on common message shapes, not a
  known-callsign pin), and **not implemented** in pancetta (no `APRIORI174` table). A
  cheap complementary lever worth a separate spike: re-derive the table from pancetta's
  own CRC-validated corpus (the spec forbids lifting ft8mon's numbers) and add it as an
  always-on `+few%` prior — but keep it distinct from this QSO-context design.
