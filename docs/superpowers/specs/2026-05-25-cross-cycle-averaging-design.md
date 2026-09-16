# Cross-cycle non-coherent symbol averaging (hb-056) — design spec

**Title corrected 2026-09-16** (was "Cross-cycle *coherent* symbol
averaging") — hb-056 is, and has always been, the non-coherent variant;
the original title contradicted the hypothesis bank's own name for this
same entry. See the 2026-09-16 correction in Section 1 below for the full
context: the reference mechanism this ports is non-coherent too, so this
was never an approximation of a coherent original.

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

### 1. CORRECTED 2026-09-16 — JTDX's mechanism is non-coherent too; no gap to bound

An earlier version of this section claimed JTDX's `csold` (`complex
csold(0:7,79)` in `lib/ft8b.f90`) "integrates amplitude with phase, i.e.
true coherent integration," and that pancetta's power-only spectrogram
therefore bounds its achievable gain below JTDX's. **This was wrong, and
self-contradicted the very formula stated two lines below it**
(`|cs|² + |csold|²` is already the non-coherent sum, not `|cs + csold|`).

A direct clean-room read of JTDX's actual source (`lib/ft8b.f90`, subpasses
`isubp1={4,7,10}` and `{5,8,11}`) confirms: `csold` is declared complex
because phase is needed for *intra-frame* multi-symbol combining (2-3
adjacent symbols within the same 15s frame, subpasses' `nsym` handling) —
a separate, orthogonal mechanism from the cross-cycle averaging this spec
targets. The cross-cycle combination itself discards phase: it adds
`abs(cs)**2 + abs(csold)**2` (or `abs(cs) + abs(csold)` in the
non-squared subpasses) as two independent real terms, never
`abs(cs + csold)`. Gating is also more specific than "any repeating
station": `csold` is only populated for a candidate independently
classified as a CQ, MyCall, or QSO-partner signal (via tone-pattern
hit-counting, not a generic similarity score), matched to a *failed*
decode from the same-parity slot ~30 seconds earlier within 2 Hz / 0.05s.

**Net effect, precisely scoped:** matching JTDX's cross-cycle power-add
term specifically (`s2(i) = |cs|² + |csold|²`, the single-symbol `bmeta`
lane) never required a coherent-vs-non-coherent gap to bound — pancetta's
power-only spectrogram already suffices for that ONE lane, no spectrogram
rework needed. This does NOT extend to the rest of what the same
`isubp1={4,7,10}`/`{5,8,11}` subpasses compute: they also produce
`bmetb`/`bmetc`, two- and three-symbol joint metrics that coherently sum
*adjacent* complex `cs` values within the same frame (`research/specs/spec-wsjtx-mainline-ft8b.md:200-209`)
— power-only bins cannot reconstruct those phases, so a claim of
replicating JTDX's *isubp1 pass as a whole* "exactly" would be wrong. This
spec, and PAN-159, target only the cross-cycle power-add term, not the
multi-symbol lanes.

This is separate from whether phase-aware combining is independently worth
having on its own merits: it is — hb-075 (shipped, default-on) genuinely IS
coherent (extracts complex symbols, phase-aligns them, reliability-weights
via MRC, `coherent_sum_complex_to_db` in `pancetta-ft8/src/decoder.rs`) and
that real, working mechanism should not be confused with JTDX-parity on the
cross-cycle term, which never needed it.

**CORRECTED 2026-09-16 (cycle 2):** the sentence that used to appear here
overclaimed PAN-159's novelty. The power-add ARITHMETIC this section
describes is not untested — THIS spec's own mechanism (below), graduated
as hb-056, already sums linear power across grouped candidates and ships
today (`cross_cycle_averaging_pass`, `pancetta-ft8/src/decoder.rs:7577`).
What PAN-159 actually targets is narrower: JTDX gates its power-add
specifically to candidates classified as CQ, MyCall, or QSO-partner
signals (not hb-056's broader freq/t0-proximity + sync-score-similarity
grouping), and specifically matches against a *failed* decode held in
cross-window persistent state from ~30 seconds earlier (not hb-056's
within-one-buffer grouping of whatever the sync search already found,
successful or not, inside the existing 90 s recording). PAN-159 is a test
of whether that specific classification-gating + failed-decode-only +
persistent-state shape finds anything hb-056's broader grouping doesn't
already catch — it is not "port an untested mechanism," and the honest
expectation is that any remaining delta is small, since hb-056 already
operates on the same 90 s buffers JTDX's cross-window state exists to
reach across separate windows for.

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
  bounding the technique for pancetta's power-spectrogram architecture.
  **CORRECTED 2026-09-16:** the following clause originally said this
  would motivate "retain complex spectrogram bins... as the only path to
  JTDX's coherent gain" — false. JTDX's cross-cycle power-add term is
  non-coherent; there is no JTDX-parity reason to retain phase for it. A
  "retain complex bins" project is still independently motivated by
  hb-075's coherent (MRC-weighted) mechanism and by JTDX's separate
  multi-symbol `bmetb`/`bmetc` lanes (Section 1's precisely-scoped note
  above) — but not by this term.

## Open questions for the implementer

- Best M (cap on members averaged per group) — start with all, cap if
  wall-clock balloons.
- Whether to also emit LLRs from the *best single* member when the average
  fails (belt-and-suspenders; the union already covers this since per-slot
  candidates decode independently).
