# Pancetta FT8 Decoder — Recall, Sensitivity & False-Positive Cost

How good is Pancetta's **native Rust decoder**, really? This note leads with the
most rigorous, most honest numbers we have: recall measured against **jt9**
(WSJT-X's own decoder — the closest thing to ground truth in this domain), a
properly WSJT-X-calibrated SNR-sensitivity curve, an explicit false-positive
cost measured on pure noise, and a verified/unverified split for decodes jt9
doesn't independently confirm. All of it comes from the
**decoder-tp-sensitivity** effort (spec:
[`docs/superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md`](superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md),
plan:
[`docs/superpowers/plans/2026-07-06-decoder-tp-sensitivity.md`](superpowers/plans/2026-07-06-decoder-tp-sensitivity.md)),
which was commissioned specifically because an earlier, more casual
ft8_lib-referenced comparison (below, now demoted to a historical/secondary
table) overstated how much it proved.

## TL;DR

- **Recall against jt9 on real off-air recordings**: Pancetta recovers **62.5%**
  of what `jt9 -d 3` decodes on the curated hard-200 corpus (1,251 of 2,001
  jt9-verified truth messages, current production defaults) — up from a
  59.7% baseline (1,195/2,001) measured at the start of this effort. The
  entire +56-truth gain is attributable to one shipped fix: Task W1.4's
  LLR-whitening dB/linear unit bug. jt9 recall — not "beats ft8_lib" — is the
  headline metric from here forward.
- **False-positive cost is real and nonzero**: at current production defaults,
  Pancetta hallucinates **1 decode per 1,000 pure-noise recordings (0.1%)**
  — down from a 3/1000 baseline measured at the start of this effort (Task
  W0.1), improved as a side effect of the same whitening fix. This number is
  reported explicitly alongside recall so a higher recall claim can never be
  read without its FP price.
- **Novel decodes are split, not lumped**: of the 4,956 decodes on hard-200
  that jt9's own truth set doesn't confirm, 3,161 (63.8%) pass an independent
  plausibility check and 1,795 (36.2%) don't (Task W0.3's report-only
  classification) — see [Methodology](#methodology) for exactly what
  "verified" means here (a proxy signal, not a correctness proof).
- **Calibrated SNR-sensitivity gap**: on a synthetic AWGN corpus using the
  exact WSJT-X 2500 Hz noise convention, Pancetta needs **~2.1 dB more SNR**
  than jt9 to reach 50% recall on FT8 (Task W0.2). FT4 is substantially worse
  — **~3.95 dB more**, and Pancetta never reaches 90% recall on FT4 at all,
  capping at 78% (Task W0.4) — a real, previously-unmeasured, mode-specific
  deficiency flagged for future work.
- The old **"+11.6% more decodes than ft8_lib"** headline is still true as far
  as it goes, but it's now a [secondary, historical comparison](#historical-comparison-vs-ft8_lib-superseded-methodology)
  — see why it was demoted.

## Methodology

The measurements above use `pancetta-research`'s `eval`/`compare`/`gen-noise`
harness (not the older `benchmark-decode` command used for the ft8_lib
comparison below), built out during Workstream 0 of the decoder-tp-sensitivity
plan specifically to close three pre-existing measurement-trust gaps: no
false-positive measurement existed at all, the synthetic SNR axis wasn't
comparable to the field-standard WSJT-X convention, and "novel" decodes (ones
without a jt9-confirmed match) were being reported as an undifferentiated
count with no signal about how many were plausible vs. not.

- **jt9 as the reference oracle.** `jt9 -d 3` (WSJT-X's own decoder,
  vendor-neutral relative to Pancetta) is run once per WAV and cached
  (`research/baselines/ft8/<wav_sha256>.json`); every tier that has a jt9
  truth set reports recall against it directly.
- **Corpora:**
  - **curated hard-200**: 200 real off-air 15-second recordings, hand-curated
    for difficulty, with 2,001 total jt9-decoded truth messages across them.
    This is where the headline recall number and the novel-decode split come
    from.
  - **noise_1000**: 1,000 pure-noise WAVs (`gen-noise --count 1000 --seed
    20260706 --birdie-fraction 0.3`), deterministic/seeded, no signal present
    by construction — **any** decode on this tier is a false positive, full
    stop (Task W0.1).
  - **synth-clean**: 550 synthetic AWGN WAVs (11 SNR steps from −24 to −14 dB,
    50 distinct messages per step, randomized audio frequency and timing
    offset), generated with a noise scaling formula that matches WSJT-X's
    published SNR convention **exactly**:
    `noise_rms = signal_rms / 10^(snr/20) * sqrt(6000.0/2500.0)` (Task W0.2).
    `jt9` is run over the identical corpus to produce a directly-comparable
    reference curve.
- **Novel-decode verified/unverified split (Task W0.3):** any decode that
  doesn't match a jt9 truth message for its WAV is a "novel." Each novel is
  additionally run (report-only — this never filters or changes any count
  that gates a decision) through Pancetta's existing false-positive
  plausibility filter (the callsign-continuity heuristic, `hb-052`). A novel
  that passes is counted as **verified**; one that doesn't is **unverified**.
  This is a *plausibility proxy*, not proof either way — a verified novel
  could still be a well-formed false decode, and an unverified one could
  still be a genuine catch on a station jt9 simply missed. Treat the split as
  a confidence signal, not a ground-truth label.
- **Build:** release (`opt-level=3`, LTO), Apple Silicon, `Ft8Config::default()`
  (current production config) unless noted.

## Results — the honest headline

### Recall against jt9 (real off-air recordings, curated hard-200)

| Metric | Value |
|---|---:|
| jt9 truth messages (200 files) | 2,001 |
| Pancetta recovered — **current production defaults** | **1,251 (62.5%)** |
| Pancetta recovered — measurement-start baseline (pre-Task-W1.4) | 1,195 (59.7%) |
| Improvement | **+56 truths**, bootstrap 95% CI **[+34, +80]** (excludes zero) |

The +56-truth gain is Task W1.4's fix to `whiten_llrs`: the function was
mixing dB-scale and linear-magnitude values in the same median/floor
computation. Fixing the unit bug and re-measuring is, so far, this entire
plan's single largest verified recall win — larger than any per-candidate
sync/demod change attempted in Workstreams 3–5. This is directionally
consistent with the measurement that motivated the whole effort: an earlier,
larger hard-200 corpus (`research/scorecards/main.json`, dated 2026-06-06,
5,253/8,853 truths) put Pancetta at ~59.3% of jt9 — the same ballpark as this
plan's own pre-fix baseline, from an independently-constructed corpus.

### Novel decodes: verified vs. unverified split (same hard-200 run)

| | Count | Share of novels |
|---|---:|---:|
| Novel decodes (no jt9-truth match) | 4,956 | 100% |
| — verified (passes plausibility check) | 3,161 | 63.8% |
| — unverified (does not) | 1,795 | 36.2% |

Both directions moved with the whitening fix (3,059→3,161 verified,
1,755→1,795 unverified) — a Δunverified of +40 against a Δverified-TP of +56,
comfortably inside the plan's own standing gate (unverified-novel growth must
not exceed 2× the verified true-positive gain it rode in on).

### SNR-sensitivity curve (2500 Hz-calibrated, matches the WSJT-X convention)

| SNR (dB) | Pancetta decoded / 50 | jt9 decoded / 50 |
|---:|---:|---:|
| −24 | 0 | 1 |
| −23 | 0 | 3 |
| −22 | 0 | 14 |
| −21 | 1 | 30 |
| −20 | 3 | 48 |
| −19 | 31 | 50 |
| −18 | 46 | 48 |
| −17 | 50 | 49 |
| −16 | 50 | 49 |
| −15 | 50 | 50 |
| −14 | 50 | 49 |

- **Pancetta SNR@50%** = −19.214 dB (interpolated between −20 dB and −19 dB).
- **jt9 SNR@50%** = −21.313 dB (interpolated between −22 dB and −21 dB).
- **Gap: Pancetta needs ~+2.10 dB more SNR than jt9 for 50% recall on FT8.**

This is Task W0.2's baseline measurement of `Ft8Config::default()` **at that
point in the plan** (before Task W1.4's whitening-default flip). Smaller-scale
spot-checks on the same corpus later in the plan (Tasks W1.3, W1.4) showed a
further small SNR@50% improvement in the same direction (roughly −0.1 to
−0.3 dB), but the full 550-file calibrated curve was not independently
re-run end-to-end against final production defaults — treat −19.2 dB /
+2.1 dB as the plan's reference baseline gap, not necessarily today's exact
number to the decimal.

**FT4 is worse, and previously unmeasured.** Task W0.4 ran the identical
2500 Hz-calibrated methodology on FT4: Pancetta needs **~+3.95 dB more SNR**
than jt9 for 50% recall (−14.538 dB vs. −18.484 dB), and across the entire
−24…−14 dB sweep Pancetta **never reaches 90% recall on FT4 at all**,
capping at 78% — nearly double FT8's already-measured gap, and a real,
substantial, mode-specific deficiency that had never been measured before
this plan. It was flagged as probably warranting its own future workstream,
not investigated further here.

### False-positive cost (pure-noise tier)

| | False positives | Rate |
|---|---:|---:|
| Measurement-start baseline (Task W0.1) | 3 / 1,000 | 0.3% |
| Current production defaults | 1 / 1,000 | 0.1% |

The one recall-improving default flip this plan shipped (Task W1.4) also
*reduced* the false-positive rate — recall and FP cost moved the same
direction, not traded off against each other. But **1/1000 is not zero**:
every future recall-improving change in this decoder is gated on this number
not going up (see the plan's standing `[A/B]` gate, "FP-on-noise tier = 0 new
decodes" — read as a *delta* requirement against whatever the current
baseline is, not an absolute-zero requirement).

## Historical comparison vs. ft8_lib (superseded methodology)

This is the original comparison that used to lead this document. It's still
a real, reproducible measurement — but ft8_lib is a *weaker* reference than
jt9 (it's not WSJT-X, and Pancetta already cross-validates bit-exactness
against it on the shared decode path, which makes it a less independent
yardstick), and it doesn't separate genuine extra catches from false
positives the way the jt9-referenced measurements above do. Kept here for
continuity, not as the headline.

On **1,201 real off-air recordings** sampled across a 28,822-file corpus,
Pancetta's native decoder produced **+11.6% more decodes than ft8_lib on the
same audio** (5,581 vs. 5,003) while recovering **90.7%** of everything
ft8_lib found.

| Metric | Pancetta | ft8_lib |
|---|---:|---:|
| Total decodes | **5,581** | 5,003 |
| Per file (avg) | 4.6 | 4.2 |

| Comparison | Count |
|---|---:|
| Δ total | **+578 (+11.6%)** |
| Agreed (decoded by both) | 4,540 |
| Pancetta-only (we got, ft8_lib didn't) | 1,041 |
| ft8_lib-only (it got, we didn't) | 463 |
| Recall of ft8_lib's set | **90.7%** |
| Parity (`both / max(total)`) | 81.3% |

- **Tool:** `pancetta benchmark-decode <dir>` decodes every WAV with **both**
  decoders on **identical audio** — Pancetta's native Rust decoder
  (`decode_window`) and ft8_lib (via FFI) — then dedups and compares the
  message sets. The native side runs *without* a-priori (AP) context (in
  production Pancetta runs ft8_lib as the primary decoder plus its native
  decoder as a secondary, AP-enhanced pass, so live yield is higher than
  these no-AP numbers).
- **ft8_lib is a reference, not neutral truth**, and not every "extra" is a
  win: the 1,041 Pancetta-only decodes are a mix of real catches and false
  positives; the jt9-referenced novel-decode split above is the more honest
  way to look at that same question now.

## Our approach: parallel multi-candidate decoding

FT8 has a hard **15-second slot budget**: all decoding for a window must
finish before the next window arrives. That budget is the real constraint —
the more candidate signals you can fully evaluate within it, the more you
decode.

ft8_lib's reference decoder is **single-threaded**. Pancetta's native decoder
**fans the per-candidate decode out across CPU cores with [Rayon]** — Costas
2-D sync produces a list of candidate (time, frequency) positions, and each
one is run through symbol extraction → max-log LLRs → sum-product LDPC → OSD
fallback **in parallel** (`par_decode_candidate`, with the AP0 candidate loop
running across Rayon workers). That parallelism is what makes it affordable
to keep more sync candidates and run deeper recovery per candidate inside the
real-time budget — see the coordinator's `[decoder]` effort-preset system
(`pancetta/src/coordinator/effort.rs`) for how this is tuned per hardware
tier today.

## What this plan changed, and what it didn't

This document's numbers come from the decoder-tp-sensitivity plan's five
workstreams (W0 measurement trust, W1 correctness bugs, W2 acceptance
metric/OSD, W3 per-candidate fine sync, W4 subtraction fidelity/multipass, W5
candidate pipeline). The overwhelming majority of the plan's A/B experiments
— across every workstream — **correctly declined to change production
defaults** once measured rigorously (several real regressions were caught,
and a few of the plan's own inherited assumptions turned out to be stale,
e.g. a design-spec statistic that had already been superseded, and corpus
files that turned out unable to exercise the mechanism they were meant to
test). A small number of real production changes did ship:

- **Task W1.4** — the LLR-whitening dB/linear unit fix above (the plan's
  single biggest verified win, +56 truths, `llr_whitening_enabled` flipped
  off by default).
- **Task W1.7** — fixed a real, pre-existing bug where AP1–AP4's
  callsign-field injection had the addressee/sender bit-field placement
  backwards.
- **Task W2.6** — shipped AP4's full-message-content mask (default on),
  after review caught and fixed a self-consistency bug in its own
  supporting measurement.
- **Task W5.3** — shipped a new opt-in coordinator capability
  (`[decoder].extended_capture_window_enabled`, default off), widening the
  audio capture window's negative-dt lead-in.

Two separate mechanisms were measured as real, positive, and still unshipped
— they are easy to conflate (an earlier draft of this section did) but come
from different tasks with different, non-interchangeable numbers:

- **Task W3.3 / W3.3b — per-candidate matched-demodulation fine sync.**
  Task W3.3 measured a genuine **+54-truth win** on hard-200 under an
  *unbounded* decode budget, but declined to ship because of a ~2.1–2.9x
  elapsed-cost regression at that budget. Task W3.3b built a real bounded-budget
  eval mode and re-measured the same mechanism realistically: under the
  **Standard preset (250ms — the realistic production default)** it
  **regresses**, Δ=**-23** (flagged by `compare`'s own regression scanner) —
  not a win. Under the more conservative **Eco preset (1ms)** it *is* a clean,
  gate-passing win, Δ=**+27** (95% CI [+15, +40]), and faster besides. The
  cause is architectural, not a tuning miss: the anytime decoder's budget is
  only checked *between* candidates, so one expensive matched-demod candidate
  can starve the rest of its window's budget — harmless at Eco (there's no
  "rest" to starve, only the unconditional floor set runs) but costly at
  Standard. This remains a genuine, unresolved gap, not a clean win waiting
  to be flipped on.
- **Task W4.3 — real multipass (consuming W4.1/W4.2's improved subtraction).**
  A *separate* mechanism, measured under the same realistic Standard bounded
  preset, shows a clean **+32-truth win** on hard-200 (95% CI [+12, +57],
  excludes zero, ~4.0x elapsed cost, zero FP-on-noise cost) — and unlike
  W3.3/W3.3b, it does **not** regress at Standard. This is the plan's single
  biggest **unresolved** opportunity: a genuine, gate-passing win under the
  realistic production regime that still isn't the global default, because
  that default also has to stay safe for `DecodeBudget::unlimited()`
  consumers (the test suite, direct `pancetta-ft8` API callers, and the
  operator-selectable `Max` effort preset), for whom the unbounded
  measurement shows no benefit at meaningfully higher cost. Shipping it
  safely needs the same regime-conditional wiring through the coordinator's
  effort-preset system that W3.3/W3.3b's mechanism would also need — a
  well-supported next step, not something this plan built.

## Reproduce

```bash
# jt9-referenced recall + novel split (curated hard-200):
cargo build --release -p pancetta-research
./target/release/eval --tier curated-hard-200 --mode ft8 \
  --output research/scorecards/my-run.json

# False-positive rate on pure noise:
./target/release/gen-noise --count 1000 --seed 20260706 --birdie-fraction 0.3
./target/release/eval --tier noise_1000 --mode ft8 \
  --output research/scorecards/my-noise-run.json

# Calibrated SNR-sensitivity curve (synth-clean, 2500 Hz WSJT-X convention):
./target/release/eval --tier synth-clean --mode ft8 \
  --output research/scorecards/my-snr-run.json

# Compare two scorecards (A/B, with bootstrap CIs + standing gates):
./target/release/compare research/scorecards/control.json research/scorecards/variant.json

# Historical ft8_lib-vs-Pancetta comparison (secondary table above):
cargo run --release -- benchmark-decode /path/to/wavs --format text
```

[Rayon]: https://github.com/rayon-rs/rayon
