# Cross-cycle coherent symbol averaging (hb-056) — design spec

**Status:** proposed (design before implementation, per the bank's
plan-sized policy)
**Hypothesis:** hb-056 (priority 0.60, top of bank, from mr-002 JTDX harvest)
**Author:** research harness, 2026-05-25
**Estimated effort:** 2-3 sessions

## Goal

Port JTDX's headline sensitivity technique — averaging a repeating
station's symbol energies across consecutive 15 s cycles — so a station
calling CQ in several slots gets its weak slots recovered by integrating
with its stronger ones. JTDX (`lib/ft8b.f90`, subpasses isubp1={4,7,10})
computes `s2(i) = |cs|² + |csold|²` where `csold` is the same candidate's
symbol field from the previous cycle, then derives LLRs from the summed
energy.

## Two architecture findings that bound and reshape the port

These came out of grounding the design in pancetta's code; they change
the expected payoff and the corpus plan versus the bank entry's
assumptions.

### 1. pancetta can only do NON-coherent averaging (power, not complex)

JTDX's `csold` is **complex** (`complex csold(0:7,79)`) — it integrates
amplitude *with phase*, i.e. true coherent integration, which is the bulk
of its edge. pancetta's decode spectrogram stores **power only**
(`Spectrogram.power` is `10*log10(|FFT|²)`; phase is discarded in
`compute_spectrogram`). A candidate's evidence is
`tone_magnitudes: Vec<[f64; NUM_TONES]>` in **dB**.

⇒ pancetta can sum *powers* across cycles (`|cs|² + |csold|²`,
non-coherent) but cannot do coherent (phase-aligned) integration without
reworking the spectrogram to retain complex bins — a much larger change,
out of scope here. Non-coherent integration of N repeats still lowers
noise variance (≈ +1.5 dB for N=2, diminishing), so it's worthwhile, but
**the achievable gain is bounded below JTDX's coherent result.** The spec
ships the non-coherent variant and measures it honestly; coherent is a
possible future follow-up (spectrogram-retains-phase is a prerequisite).

### 2. The existing 90 s recordings already contain the repeats

Batch 11 (hb-012) established that the curated corpus is **90 s
continuous multi-slot recordings** decoded as one buffer, and the Costas
search scans `t0` across the whole buffer. A station calling CQ in
multiple slots therefore already appears as **multiple candidates at the
same `f0`, with `t0` values ~1 slot apart**, inside a single
`decode_window` call. So cross-cycle averaging is testable on the
**existing hard-200/hard-1000** with no new corpus — contrary to the bank
entry's "needs a new contiguous-slot corpus." (A controlled synth tier is
still nice-to-have for isolating the effect; it becomes step 4, optional.)

Note: **synth-clean is single-slot** (60 independent WAVs, no repeats), so
averaging cannot help it. The composite weights `snr_50pct_synth_clean`
(0.3) — which this won't move — and `real_decode_rate_hard_200` (0.5),
which it can. So hb-056's composite path is entirely through hard-200
recall.

## Mechanism

Within one `decode_window` pass, after `costas_sync_search` produces the
candidate list:

1. **Group candidates by repeating-station key:** `(freq_bin, freq_sub,
   t0 mod slot_steps)` with a small tolerance (±1 freq bin, ±a few time
   steps), where `slot_steps = round(15 s / symbol_period) * TIME_OSR`.
   Candidates in the same group at `t0` values ~k·slot_steps apart are
   candidate repetitions of one station.
2. **For each group of size ≥ 2**, extract each member's
   `tone_magnitudes`, convert dB→**linear power**, sum element-wise across
   members (optionally cap at the best M members), convert back to dB, and
   produce an **averaged candidate** whose `tone_magnitudes` feed
   `compute_soft_llrs_db` → LDPC. (Working in linear power is required;
   averaging in dB is the wrong operation — see hb-069's dB-vs-linear
   finding.)
3. **Decode the averaged candidate in addition to the individual ones**
   (union the results, dedup by message text). Averaging is *additive*
   recall: a repeat that each fail individually may succeed averaged; we
   never drop the per-slot attempts.

### The mismatch risk (the crux)

Two *different* stations can share an audio frequency across slots (one
stops, another starts). Averaging their symbols produces garbage. Because
we don't know the message until after decoding, the guard must be
pre-decode and conservative:

- Tight proximity (freq within ±1 bin, t0 within ±2 steps of the
  slot-multiple).
- Only average members whose individual `sync_score` is within a band of
  each other (a genuine repeat has similar sync strength slot-to-slot; a
  frequency that's reused by a louder station won't).
- Averaging is **additive** (union with per-slot decodes) so a corrupted
  averaged candidate that fails CRC simply contributes nothing — it can't
  *remove* a real decode. The only downside is the extra averaged
  candidate occasionally passing CRC as an FP, which the production FP
  filter (hb-052/062) then catches. So the precision-wall exposure is the
  familiar one, already mitigated.

## Touchpoints

- `pancetta-ft8/src/decoder.rs`:
  - candidate grouping helper (by the repeating-station key) in the
    decode dispatch, before the per-candidate rayon map.
  - a linear-power averaging helper producing an averaged
    `tone_magnitudes` (reuses `extract_symbols_from_spectrogram`).
  - feed averaged candidates through the existing LLR→LDPC path; union +
    dedup with the per-slot results.
  - `Ft8Config::cross_cycle_averaging: bool` (default false until graduated).
- `pancetta-research/src/decoder.rs` + `bin/eval.rs`: `with_cross_cycle_averaging`
  builder + `--cross-cycle-averaging` flag for the A/B.
- **No coordinator/pipeline change for the eval path** (it's all within one
  `decode_window`). A live-path note: production already feeds continuous
  audio, so the same in-buffer grouping works on-air; no cross-slot state
  machine needed (a simplification vs JTDX's `csold` persistence and vs
  the bank entry's "~50 LOC coordinator handoff").

Revised LOC estimate: **~150-250 LOC** (grouping + linear-power averaging
+ flag/plumbing), down from the bank's 200-400 because the multi-slot
buffer removes the cross-slot persistence + new-corpus work.

## Build sequence

1. **Spec approval** (this doc).
2. **Implement** the grouping + non-coherent linear-power averaging behind
   `cross_cycle_averaging` (default off) + the eval flag. Unit test: a
   synthesized two-cycle case where each cycle is sub-threshold but the
   sum decodes.
3. **A/B on hard-200** (flag off vs on), then hard-1000 if promising.
   Success = +recovered with no fixture/synth regression; novels handled
   by the FP filter. Measure both no-filter (raw effect) and
   with-filter (shipped reality), per the batch-10 methodology lesson.
4. **(optional) contiguous-slot synth tier** to isolate the effect and
   quantify the N=2/3 gain cleanly; only if step 3 is ambiguous.
5. **Decide:** graduate (flag default true + main.json refresh) or shelve
   with the measured non-coherent ceiling documented.

## Success criteria / kill criteria

- **Graduate** if hard-200 recovered rises with no guard-tier regression
  and the filtered novel cost is small (precision-wall mitigated).
- **Shelve** if the non-coherent averaging yields no recall (plausible:
  the max-of-dB LLR is dominated by the strongest tone, and non-coherent
  power summation of a sub-threshold repeat may not cross the LDPC/CRC
  threshold without phase). That would be a clean, documented negative
  bounding the technique for pancetta's power-spectrogram architecture —
  and would motivate the larger "retain complex spectrogram bins" project
  as the only path to JTDX's coherent gain.

## Open questions for the implementer

- Best M (cap on members averaged per group) — start with all, cap if
  wall-clock balloons.
- Whether to also emit LLRs from the *best single* member when the average
  fails (belt-and-suspenders; the union already covers this since per-slot
  candidates decode independently).
