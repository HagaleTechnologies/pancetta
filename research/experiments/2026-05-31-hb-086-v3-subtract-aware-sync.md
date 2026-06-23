---
slug: hb-086-v3-subtract-aware-sync
mode: ft8
state: shelved
created: 2026-05-31T05:00:00Z
last_updated: 2026-05-31T05:30:00Z
branch: iter/2026-05-31-hb-086-v3
parent_hypothesis: hb-086 V3
wild_card: false
scorecard: (n/a — sweep produced 0 additional decoded messages at every relax_db; no main.json update)
delta_vs_main: composite 0 (zero added decodes on hard-200 at every sweep point); elapsed +1.4% to +2.0% (V3 pass runs but finds only noise)
disposition: SHELVE — diagnostic geometry passed (56.8% of V1-uncoverable truths sit within ±8 bins of a subtracted decode, well above the 20% PROCEED gate), but empirical sweep shelved the mechanism: it surfaces ~100-131 truly-new candidates per worst-WAV but they are noise, not signal. CRC catches ~98%, plausibility rejects the rest. Plumbing kept at default-off for future revisit.
---

## Hypothesis

V1 (`joint_pair_retry`, GRADUATED 2026-05-28) closed the "candidate is in
sync list but failed pass-1 LDPC due to interference" leak by retrying
original sync_candidates against the post-multipass residual. V2 (soft
cancellation, SHELVED across two corpora 2026-05-31; Phase A honesty
pass 2026-06-02 replaced "DEFINITIVELY SHELVED") was closed by pancetta's
hard-decision pipeline (CRC-validated neighbors yield delta-function
posteriors → soft ≡ hard). Re-test gate: hb-146 synth-pair-200.

V3 attacks the OTHER V1 leak documented in the V1 journal:

  leak (a): candidates whose pass-1 `sync_search` never surfaced them.
    V1 only retries what `sync_search` already flagged; sync-search
    misses stay missed.

The V2 diagnostic (refreshed corpus) measured this leak's mass: **47.5%
of missed truths in top-20 hard-200 have ZERO nearby pancetta decode in
±25 Hz** — V1's uncoverable subset.

**V3's mechanism**: after multipass + V1 saturate, run ONE more
`costas_sync_search` on the residual spectrogram at a relaxed threshold
(`min_sync_score + joint_residual_sync_relax_db` where `relax_db < 0.0`),
restricted to freq_bins within `±joint_residual_sync_window_bins` of any
subtracted-eligible decode position. Decode each new candidate against
the residual via the same LDPC+CRC+plausibility path as V1.

## Why this is structurally different from hb-082 (SHELVED)

hb-082 relaxed `min_sync_score` GLOBALLY on the residual. It found
nothing (no-op at every threshold ∈ {2.0, 2.5, 3.5}) because the
residual's GLOBAL noise floor is mostly unchanged by subtraction —
subtraction is localized to bins where signal was removed, not the
whole spectrogram. V3 relaxes ONLY at bins where subtraction
demonstrably dropped the noise floor (the bin-targeted window). This
is a structurally different bet: the relaxation is conditioned on
"signal was here, now it isn't" rather than "globally less noisy",
which is exactly the empirical claim that hb-079's coherent subtract
makes.

## Kill-switch diagnostic (PROCEED — geometric)

Per the V3 spec: before implementing, measure on top-20 worst-WAVs
hard-200 — of missed truths with NO V1-reachable neighbor (no decode
in ±25 Hz), what fraction have a subtracted-eligible decode within
±N freq_bins for some sweep N. PROCEED if any N ≥ 20%.

`pancetta-research/examples/hb086_v3_subtract_window_potential.rs`
(commit a1333e9):

| metric | value |
|---|---:|
| total missed truths (top-20) | 653 |
| V1-uncoverable (no neighbor in ±25 Hz) | 310 (47.5%) |
| in ±4 bins  (±25 Hz) | 0 (0.0%) — by construction |
| in ±6 bins  (±37.5 Hz) | 104 (33.5%) |
| **in ±8 bins**  (±50 Hz) | **176 (56.8%)** |
| in ±12 bins (±75 Hz) | 255 (82.3%) |

PROCEED on geometry. The V1-uncoverable mass sits 25-100 Hz from a
subtracted decode, exactly where bin-targeted subtraction localizes
its noise-floor reduction. The diagnostic suggested the production
window would land in 6-12 bins.

## Implementation

`Ft8Config::joint_residual_sync_relax_db: f64` (default 0.0 =
disabled) and `Ft8Config::joint_residual_sync_window_bins: usize`
(default 8 ≈ ±50 Hz). When `relax_db < 0.0`, after the V1 retry pass:

1. Build subtracted-eligible position set from `pass_decoded` entries
   with `tone_symbols.is_some()`.
2. Compute allowed freq_bin set: union of ±N around each subtracted
   position (per freq_sub).
3. Run `localized_costas_sync_search` on the residual spectrogram at
   `min_sync_score + relax_db`, restricted to the allowed bins.
4. Filter out candidates already at subtracted positions or original
   sync_candidate positions (V1's tolerance: ±1 freq_bin, ±2 time_step).
5. Decode each truly-new candidate against the residual (same
   LDPC+CRC+plausibility path V1 uses).

Research builder: `with_joint_residual_sync_relax_db(f64)` +
`with_joint_residual_sync_window_bins(usize)`. Eval flags:
`--joint-residual-sync-relax-db <f64>` +
`--joint-residual-sync-window-bins <usize>`.

## Sweep result — hard-200, default window N=8

Baseline (refreshed main.json, composite 0.579114; FP-filter ON; hard-200
rec=4942, novel=1024). V3 disabled at default 0.0 reproduces the
baseline exactly (rec=4942, novel=1024).

| relax_db | hard-200 rec | novel | rate | elapsed | Δ rec vs baseline |
|---:|---:|---:|---:|---:|---:|
| baseline (V3 off) | 4942 | 1024 | 0.55823 | 171.4s | — |
| -0.5 | 4942 | 1024 | 0.55823 | 171.4s | **0** |
| -1.0 | 4942 | 1024 | 0.55823 | 173.9s | **0** |
| -1.5 | 4942 | 1024 | 0.55823 | 174.5s | **0** |
| -2.0 | 4942 | 1024 | 0.55823 | 174.7s | **0** |

Zero added decodes at every threshold. Elapsed +0.0% to +2.0% — V3 is
running (the localized sync + per-candidate decode work has nonzero cost),
but produces no output.

## Mechanism trace (the structural finding)

`pancetta-research/examples/hb086_v3_trace.rs` ran V3 with per-WAV
instrumentation (counting localized candidates, post-filter candidates,
LDPC-pass, CRC-pass, plausibility-pass, final-decoded) on the top-3
worst hard-200 WAVs at every relax_db:

| WAV | subtracted | localized | new_after_filter | ldpc_ok | crc_ok | plausible | decoded |
|---|---:|---:|---:|---:|---:|---:|---:|
| 566cf9fc | 139 | 300 | 131 | 131 | 2 | 0 | **0** |
| eb762156 | 132 | 300 | 110 | 110 | 4 | 0 | **0** |
| 9e5b9243 | 155 | 300 | 104 | 104 | 1 | 0 | **0** |

Identical counts across ALL four relax_db values ({-0.5, -1.0, -1.5, -2.0}).

**Three structural facts:**

1. **`localized=300` and `new_after_filter` stable across thresholds.**
   The localized sync_search caps at `max_sync_candidates=300`. Even at
   the gentlest relaxation (-0.5 → threshold 2.5), the targeted window
   surfaces 300 candidates — the cap binds. The top-300 by score are
   stable across deeper relaxations (those don't add new top-300
   candidates, just admit weaker ones we already truncate away).
2. **`ldpc_ok = new_after_filter`.** Every candidate "decodes" — LDPC BP
   always returns Ok at the production 100 iterations on garbage LLRs.
3. **`crc_ok = 1-4 per WAV` (~98% CRC FP rate); `plausible = 0`.** The
   2/4/1 CRC-passing decodes per WAV are CRC false positives — random
   bit patterns whose 14-bit CRC accidentally matches — and all of them
   are caught by `is_plausible()` (they parse as message-type-aware
   non-FT8-shaped strings).

**Net: the V3-surfaced candidates are not real signals.** The residual
at sub-3.0 sync_score in the bin-targeted window is *noise*, not weak
signal masked by neighbors.

## Why the geometric diagnostic misled the mechanism

The PROCEED signal (56.8% of V1-uncoverable truths sit within ±8 bins
of a subtracted decode) was a *geometric* fact about where truths exist
relative to subtractions. It does NOT imply those truths produce
decodable Costas patterns at *any* sync_score threshold.

The V1 journal warned of this in its "Why the win is smaller than the
diagnostic suggested" section: *"Many missed signals never produced a
strong enough Costas pattern in the raw spectrogram for `sync_search`
to find them at all (sync_score < `min_sync_score`)."* The V3 hypothesis
was that subtraction's localized noise-floor reduction would push those
weak Costas patterns above a relaxed threshold. **Empirically, it
doesn't** — the patterns are too weak (well below 1.0, not 2.0-3.0) OR
the patterns don't exist (the missed truths' RF energy is too smeared
across bins/time for Costas to integrate, even on the cleaned residual).

The structural insight: **for the dense busy-band WAVs that dominate
hard-200, the wall is sub-Costas-threshold weak signals, not
mutually-masked-but-Costas-strong signals.** V1 captured the latter
(+12 hard-200 in the V1 sweep on the old corpus, baked into the
refreshed main.json). V3 attacked the former and the corpus says they
aren't recoverable via Costas-relaxation.

## Comparison: the three hb-086 variants

| variant | leak attacked | structural test | result |
|---|---|---|---|
| V1 hard-decision retry | (b) pass-1 LDPC failed on interfered candidate present in sync list | 78% pair-likely → PROCEED | **GRADUATED** (+12 hard-200, +17 hard-1000) |
| V2 soft cancellation | (b) deeper: multi-neighbor LLR conditioning | 0% marginal-SNR neighbors → SHELVE pre-impl | **SHELVED across two real-audio corpora** (hard-200 May + refreshed; pancetta's CRC selects for sharp posteriors). Re-test gate: hb-146 synth-pair-200. (Phase A honesty pass replaced "DEFINITIVELY SHELVED".) |
| V3 relaxed sync at subtracted bins | (a) candidate not in sync list | 56.8% geometric proximity → PROCEED | **SHELVED post-impl** (V3-surfaced candidates are noise; CRC catches 98%, plausibility catches the rest) |

The V2 spec's primary kill-switch (SNR-quality of neighbors) was a
*decodability* test — that's why it cleanly shelved V2 pre-implementation.
V3's geometric diagnostic measured *where truths are*, not whether the
residual at those locations has decodable Costas. The V3 result motivates
a doctrine refinement: **geometric proximity diagnostics need a paired
decodability sub-test (e.g., "of those truths near subtracted bins, how
many have residual Costas score above the relaxed threshold AND how
many of THOSE produce CRC-passing decodes when LDPC runs on a residual
extracted at the truth's *known* coordinates?").** That's a more
expensive diagnostic but would have shelved V3 before the
implementation cost.

## Decision

**SHELVE.** Plumbing kept at default-off:

- `Ft8Config::joint_residual_sync_relax_db: f64 = 0.0`
- `Ft8Config::joint_residual_sync_window_bins: usize = 8`
- Research builder: `with_joint_residual_sync_relax_db`,
  `with_joint_residual_sync_window_bins`
- Eval CLI: `--joint-residual-sync-relax-db`,
  `--joint-residual-sync-window-bins`
- Helper methods: `joint_residual_localized_sync_pass`,
  `localized_costas_sync_search`
- Diagnostic example: `hb086_v3_subtract_window_potential.rs`
- Mechanism-trace example: `hb086_v3_trace.rs`

The hook stays in `decode_window_with_ap` after V1 (one `if
relax_db < 0.0` check). Future revisits that change the noise/signal
discrimination at the residual (e.g., callsign-priors-on-residual, OSD-
without-Costas pre-gate) can land without re-plumbing.

main.json unchanged. No graduations from this iter.

## Learnings

- **Geometric proximity is not decodability.** The V3 diagnostic
  measured "where truths exist relative to subtracted bins" and earned
  PROCEED at 56.8% (>>20% gate). The implementation revealed that
  decodability at those locations is independently zero. A geometric
  diagnostic alone cannot test the LDPC+CRC+plausibility funnel; for
  any future mechanism whose value depends on decoding new candidates
  surfaced from a perturbation of the production pipeline, the kill-
  switch needs a per-truth decodability micro-test (extract LLRs at
  the truth's coordinates from the residual and check if LDPC+CRC
  pass), not just a geometric "does the truth sit near where the
  mechanism would fire" test.

- **Sub-3.0 sync_score in the residual is noise, not weak signal.**
  hb-082 found this globally (no candidates surface at relaxed
  threshold). V3 finds it bin-targeted: 100+ candidates DO surface in
  the targeted window at threshold 1.0, but they're noise. The dense
  busy-band wall on hard-200 isn't masking-pair-structured weak
  signals (V1 captured those); it's sub-Costas signals or sub-LDPC
  signals — different mechanisms (AP without sync, fractional-bin
  refinement on residual, callsign-priors-on-residual) would be
  needed to crack them.

- **The hb-086 family is closed on current corpora + implementations
  tested.** V1 GRADUATED, V2 SHELVED across two real-audio corpora
  (hb-146 synth-pair is the re-test gate), V3 SHELVED on hard-200
  (hb-146 synth-pair is also a candidate re-test gate). The
  joint-decoding implementations tested to date show closure on this
  signal class; the design space documented in
  `docs/superpowers/specs/2026-05-27-joint-decoding-design.md` is
  exhausted on organic corpora with the implementations tried — the
  synth_pair_revisit_candidate flag preserves V2/V3 unshelve paths.
  (Phase A honesty pass 2026-06-02 replaced "joint-decoding design
  space exhausted" with this scoped phrasing.) The remaining hard-200
  wall is for a different family of mechanism.

- **Plumbing-kept-at-default-off is the right pattern for shelved
  mechanisms with non-trivial hooks.** V3's `joint_residual_*` config
  fields and helper functions stay in the source as a load-bearing
  documentation surface ("we tried this, here's why it doesn't work,
  here's where to re-enter the experiment"). The runtime cost is
  zero at default 0.0. hb-082 used the same pattern with
  `residual_min_sync_score: None`.

## New spawns

- (None for V3 directly — the joint-decoding family is closed.)
- **Future doctrine spawn**: any geometric kill-switch diagnostic
  for a "find more candidates" mechanism should be paired with a
  per-truth decodability micro-test before earning PROCEED. Worth
  baking into the spec template. Not a hypothesis-bank entry,
  just a methodology note.
