# FT8 decoder — top-to-bottom analysis: efficiency + research frontier

A full pass over pancetta's FT8 decoder (`pancetta-ft8`, primarily the ~16K-line
`decoder.rs` plus `osd.rs`, `ldpc.rs`, `neural_osd.rs`, `a7.rs`, `ap.rs`), covering
(A) implementation inefficiencies and (B) the algorithmic / research frontier —
bleeding-edge directions that would move pancetta past its current state of the art.
**Analysis only** — nothing here is implemented; each item is sized and ranked for
later work. Ground-truthed against production `Ft8Config::default()` and cross-read
with `docs/gap-analysis.md`.

## Executive summary

- **The decoder is mature and heavily tuned.** Baseline vs ft8_lib truth: precision
  ≈ 0.80–0.845, miss ≈ 3–7%; self-assessed **~5–10% below WSJT-X/wsjtr** decode rate.
  That 5–10% is the headroom. `docs/gap-analysis.md` localized pancetta's *own* miss
  set to a **sync-detection gap**, not a threshold/recovery gap — which decides the
  priority order below.
- **Efficiency: one real hot-path bug + a cluster of allocation/caching wins.** The
  standout: the **layered belief-propagation branch (the production default)
  recomputes `fast_tanh` O(degree²)** — the exact inefficiency already fixed in the
  non-default flooding branch but never applied to the path that runs. Fixing it and
  the LDPC-matrix caching is a straight latency win at zero behavior change, which
  *buys budget* to run the heavier sensitivity levers below within the 15 s slot.
- **Research: two orthogonal high-leverage lanes.** (1) **Classical WSJT-X-gap
  closure** — the cached-bandpass downsampler (hb-243), partial-Costas sync (hb-242),
  and cross-sequence A7 (hb-237) directly attack the sync-detection gap the gap
  analysis found, are WSJT-X-proven, and together target ~1–2 dB. (2) **The channel
  frontier** — IBA-LDPC iterative phase tracking (hb-235) is the only lever that
  attacks FT8's *ionospheric channel* rather than its code; higher risk, higher
  ceiling. The neural lane (beyond the shipped neural-OSD) is real but correctly
  deferred.

Priority principle: **the efficiency fixes and the sync-lane research target the same
thing the gap analysis flagged** (candidate detection), so they compound. Do the
cheap latency fixes first (they fund the slot budget), then the sync lane.

---

# Part A — Implementation efficiency

Findings tiered by impact (hot path × frequency). Every Tier-1 item is on the default
decode path and behavior-preserving.

## Tier 1 — hot, on the default path

**A1. Layered BP recomputes `fast_tanh` in the inner loop — O(degree²), and it's the
default schedule.** `decoder.rs` ~L10400, the `SumProduct` arm of
`belief_propagation_with_features` under `if self.layered` (`layered_bp: true` is the
default): for each check node, `fast_tanh(ext[pos]/2.0)` is evaluated inside the
`target_pos` loop, so every edge's tanh is computed `degree` times → up to 7×7 = 49
tanh per check instead of 7. The **flooding branch already hoists this** (with a
comment explaining the fix) — it was simply never applied to the layered branch that
actually runs. Cost: 83 checks × ~30 wasted tanh × iterations × candidates × 2
freq-sub trials, every slot. **Fix:** hoist `tanh_half[pos] = fast_tanh(ext[pos]*0.5)`
into a `[f32;7]` scratch before the `target_pos` loop (mirror the flooding branch).
*Highest-impact, ~10-line change.*

**A2. LDPC parity matrix + `var_positions` rebuilt 3× per Rayon worker per window,
uncached.** `ldpc.rs::new_ft8` builds 257 heap `Vec`s from `const` tables; `Ldpc
Decoder::new` builds 174 more; the `ldpc_init` closure constructs **three** decoders
per worker. Nothing is memoized (no `OnceLock`). **Fix:** build the matrix +
`var_positions` once into a `static OnceLock<Arc<…>>` and borrow read-only.

**A3. Three identical LDPC decoders built when `adaptive_ldpc_iters` is off (the
default).** With adaptive off, `iters_low == iters_mid == iters_high`, so the three
per-worker decoders are byte-identical — 3× A2's cost for zero benefit. **Fix:** build
one decoder and alias it unless the adaptive flag is set.

## Tier 2 — real, secondary / feature-gated

**A4. Every BP return heap-allocates a 174-`f32` `Vec`** (`output_llrs.to_vec()`, ~6
sites). Per candidate × 2 freq-sub trials (+AP+rescue) → a steady stream of 696-byte
allocs/slot. `output_llrs` is already `[f32;174]`. **Fix:** return the array / let
callers borrow.

**A5. Spectrogram is a jagged `Vec<Vec<Vec<f64>>>`** (`decoder.rs` ~L1360), built as
~632 inner `Vec`s/pass; the hottest read (`lookup_time_interp`, per candidate × 79
symbols × 8 tones) triple-derefs it — pointer-chasing + cache-hostile. **Fix:** flatten
to one `Vec<f64>` with `idx = (t*freq_osr+fs)*num_bins + bin`.

**A6. AP fallback uses a 79-per-candidate per-symbol FFT** (`par_extract_symbols_
complex`, sps=1920 FFT × 79 symbols/candidate) vs the primary path's cheap spectrogram
lookup — and it fires for every candidate that fails AP0 while AP context is present
(common in live operation). **Fix direction:** read AP-path tone magnitudes from the
existing spectrogram where alignment permits, or gate the fine-FFT more tightly.

**A7. OSD trial re-encoding does work the CRC never reads** (only at `osd_depth ≥ 1`;
default is 0, so dormant in production but relevant if depth is raised for the
research below): full 174-bit scatter + 83-bit parity fold + per-trial array memcpy
where the CRC gate reads only 91 bits. **Fix:** precompute the CRC-window indices, fill
only those per trial, flip/try/unflip one shared buffer, packed-word XOR. Also **A7b:**
the OSD reliability sort recomputes `.abs()` in the comparator across ~1,300 compares
**even at depth 0** — precompute a key array + `sort_unstable_by`.

## Tier 3 — cleanup / latent

- **Dead code to delete:** `sync.rs::TimeSync` (unwired), `signal_processing.rs::
  SymbolCorrelator::correlate` (O(N²), and a time-vs-frequency-domain mismatch bug,
  kept alive only by its own bench), `baseband.rs` (unwired).
- **Duplicated serial/parallel decode implementations** (`try_ldpc_with_ap` vs
  `par_try_ldpc_with_ap`; two LLR variants) can silently drift — the confidence-floor
  work in `docs/gap-analysis.md` already had to touch four parallel copies of one gate.
  Consider consolidating or adding a cross-check test.
- **Soft-combiner global `Mutex`** serializes inside `par_iter` when enabled (default
  off) — a contention point to watch before any default-on flip.
- **`MinSum` LDPC variant exists but is hardcoded off** — relevant to A1 (a
  normalized-min-sum swap would remove both the tanh and atanh transcendentals if a
  profile justifies it).

**Ranked efficiency fixes:** A1 (layered-BP tanh hoist) → A2/A3 (OnceLock matrix + one
decoder when non-adaptive) → A4 (BP returns array) → A5 (flatten spectrogram) → A7b
(OSD sort key) → delete dead code. A1–A3 are pure latency wins that fund the slot
budget for Part B.

---

# Part B — Algorithmic / research frontier

The live backlog is large (`research/hypothesis_bank.md`: 252 hb-IDs, ~64 shipped,
~200 shelved). What follows is the *non-terminal* set that bears on sensitivity /
precision, then a ranked shortlist. Production is already ahead of the public catalog
on two things — **neural OSD** (shipped, default-on, ~125K→~200 OSD trials) and **FP
discipline** (6 filter layers vs peers' ~0).

## B.1 — The backlog by theme (live items only)

- **Sync / candidate (the gap-analysis pressure point):** **hb-243** (0.55) cached
  192k-FFT complex-baseband downsampler + 5×5 fine-sync — *"single biggest documented
  sensitivity gap vs WSJT-X," ~1–2 dB*; **hb-242** partial-Costas (`sync_bc/sync_abc`)
  + 40th-percentile sync normalization — *targets slot-edge*; **hb-222** ft8mon
  `search_both_known` subtraction refit; **hb-220** (0.50, quantified) slot-edge sync
  expansion — *64% of strong-misses are slot-edge; est +150–200 TPs*; hb-230 relaxed
  sync near partner. (Note: naive `FREQ_OSR 2→4` is a **documented dead end** — cost
  −151 TPs.)
- **LLR / soft-metric:** **hb-223** (0.50) ft8mon `soft_decode_pairs` — coherent
  adjacent-symbol pair LLRs, *1–3 dB flat-fading, ~150 LOC (cheapest big-claim item)*;
  **hb-252/253/259** BICM-ID / exact-Bessel / EM channel re-estimation — **built,
  default-off**, +0.506 dB composed synthetic but blocked in the hard-200 A/B by the
  **CRC-14 collision FP floor** (~1/16k); their validation needs an on-air A/B or a
  marginal-signal corpus, **not more code**. hb-227 empirical `apriori174[]` bit-prior.
- **LDPC / OSD / BP:** **hb-254** (0.55) post-BP-failure saturation/perturbation retry
  (EQML/MRBP) — *near-ML on short LDPC; caveat: pancetta's failures sit at ~68% LLR
  sign-agreement, far from the paper regime — probe the near-converged subpop first*;
  **f64 tanh-domain BP** (`spec-wsjtr-f64-tanh-bp.md`) — small recall lift, ~1.5×
  slower; **hb-218b** (0.45) joint LDPC for dual-miss capture pairs — *477 dual-miss
  frontier truths / ~250 realistic headroom; the only viable dual-miss attack*.
- **A-priori / context:** **hb-237** (0.60, **highest live priority**) cross-sequence
  A7 — previous-opposite-parity-slot callsigns → ≤206 reply candidates, `dmin/dmin2`
  gate; *WSJT-X since v2.6.0; wsjtr +5–6% unique decodes; ~30% of response-shaped
  misses*. (See `docs/ap-decoding-design.md` for the committed-QSO AP content-injection
  gap, a sibling to this.)
- **Multi-receiver:** **hb-115** (0.45, mechanism-proven) dual-KiwiSDR MRC — real-world
  ~+1.5 dB; blocked on paired-Kiwi capture hardware.
- **Channel model (the frontier):** **hb-235** (0.50, HIGH-POTENTIAL, untouched)
  IBA-LDPC — iterate a Wiener-process phase estimator ↔ LDPC; *1.4 dB @ BER 4e-3, 3 dB
  @ PER 1e-2; the only finding that attacks FT8's channel, not its code.*

## B.2 — SOTA catalog: what's dead, what transfers

- **GRAND / ORBGRAND family: documented dead end** (hb-260) — targets short *high-rate*
  codes; FT8's (174,91) rate-0.52 with 83 redundancy bits is outside the tractable
  guessing regime; query counts explode at weak SNR. The old shovel-ready GRAND specs
  are now **stale**. Soft-Output GRAND survives only as a posterior-calibration feeder.
- **Neural decoders:** **do not wire one yet.** ECCT (MIT-licensed transformer) is the
  best candidate *if* neural decoding becomes a priority (needs retrain for (174,91));
  Neural-Min-Sum (learns per-edge×iteration normalization, drops into the existing
  min-sum loop) is the smallest-mechanism second choice. Runtime cost outweighs benefit
  while classical headroom (sync lane, soft-output fusion, cross-seq A7) remains. Neural
  M-FSK demod is a **documented dead end** (inferior to max-log).
- **Peer-decoder gaps pancetta hasn't closed:** wsjtr's cached-baseband downsampler
  (hb-243), 3-position Costas partials (hb-242), 5×5 grid refinement, f64 BP; WSJT-X
  cross-sequence a7 (hb-237) + AP pre-pass; ft8mon `search_both_known`/`soft_decode_
  pairs`/Gaussian-ramp-subtract/`apriori174`. JTDX 3-method spectral sweep is **built
  and shelved-null** (Costas peak *locations* are magnitude-compression-invariant).
- **Dead ends banked (don't resurface):** GRAND/ORBGRAND, neural M-FSK, 3-method sweep,
  phase-coherent matched filter (sub-Costas sign-agreement = 50% = chance), AGC
  diversity (decoder is scale-invariant by construction), joint-pair soft-cancellation,
  MAP65-style MUD.

## B.3 — Ranked research shortlist

**(a) Shovel-ready — spec exists, just build (all attack the gap-analysis sync gap):**
1. **hb-237 cross-sequence A7** (0.60) — ~30% of response-shaped misses / +5–6% unique;
   ~550 LOC; needs a `pancetta-qso → pancetta-ft8` dataflow. *Best strategic fit — it
   directly serves the autonomous QSO loop pancetta uniquely runs.*
2. **hb-243 cached-bandpass downsampler + 5×5 fine-sync** (0.55) — ~1–2 dB, the single
   biggest WSJT-X gap; a decoder restructure (spectrogram → complex baseband), 3–4
   sessions. *This is the most direct attack on the sync-detection miss set.*
3. **hb-242 partial-Costas + 40th-pct sync norm** (cheap) — +50–150 slot-edge truths;
   1–2 sessions, low risk.
4. **hb-223 `soft_decode_pairs`** (0.50) — 1–3 dB flat-fading, ~150 LOC (cheapest
   big-claim); risk: the coherent-pair assumption may not hold on dispersive HF.

**(b) Bleeding-edge but plausible — needs research:**
5. **hb-235 IBA-LDPC iterative phase tracking** (0.50) — 1.4–3 dB, the *only* lever
   attacking FT8's ionospheric channel; plan-sized channel-decode loop, unproven on the
   real corpus. **Highest-upside untouched item.**
6. **hb-220 slot-edge sync expansion** (0.50, quantified) — +150–200 TPs; coordinator
   audio-buffering change (buffer ±2 s of adjacent slots).
7. **hb-254 post-BP saturation/perturbation retry** (0.55) — near-ML rescue; must probe
   the near-converged failure subpop first or it's FP noise.
8. **Validate the built opt-in family (hb-252/253/259)** — the +0.5 dB is already *in
   the code*; the blocker is a validation corpus (a marginal-signal/storm capture) + an
   on-air A/B, **not engineering**. Cheapest path to banked gains once data exists.

**(c) Moonshots:** hb-115 dual-Kiwi MRC (+1.5 dB real, hardware-blocked); a clean-room
neural decoder (ECCT/Neural-Min-Sum, deferred); GPU OSD order-4–6 on the M-series (the
tier classifier already has the plumbing to gate it).

---

## Synthesis — the recommended order

1. **Land the free latency (A1–A3).** The layered-BP tanh hoist + LDPC-matrix caching
   are behavior-neutral wins that *create the slot-time budget* every sensitivity lever
   below spends. Do these first regardless.
2. **Attack the sync-detection gap directly** (the gap-analysis root cause), cheapest
   first: **hb-242** (partial-Costas + slot-edge norm) → **hb-237** (cross-seq A7, best
   strategic fit) → **hb-243** (cached-baseband downsampler, biggest single gap). This
   is where pancetta's actual ~3.7% true miss set lives.
3. **Then the LLR/channel levers:** **hb-223** (cheap, big claim) and, as the high-
   ceiling research bet, **hb-235** (IBA-LDPC — the only channel-model attack).
4. **Unblock the already-built opt-in family** (hb-252/253/259) with a marginal-signal
   corpus + on-air A/B — banked dB waiting on data, not code.
5. **Defer neural decoding** (beyond the shipped neural-OSD) and the GRAND family
   (dead end) until the classical lane above is exhausted.

Every gate here is the project's standard: hard-200/1000 production A/B, bootstrap CI,
FP budget (`ΔFP ≤ 2×ΔTP`), and an elapsed-time hard gate — offline metrics do not ship.
The neural-OSD retarget (`docs/neural-osd-design.md`) and AP content-injection
(`docs/ap-decoding-design.md`) are the sibling design docs for two of these lanes.
