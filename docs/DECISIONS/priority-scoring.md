# Priority scoring (DX Hunter)

## Bimodal score collapse (issue #163), fixed 2026-07-19

Operator-observed on-air: DX Hunter priority scores clustered into two flat buckets (~425 for
non-home entities, ~75 for home) instead of a true gradient reflecting rarity/need. Root-caused via
direct code tracing (`pancetta-qso/src/priority.rs::score_cq_detailed`,
`pancetta/src/priority_evaluator.rs::CachedStationLookup`) — two independent gaps, not one:

1. **Dead per-band-needed wiring.** `is_dxcc_needed_on_band` (added 2026-07-18 specifically to give
   each DX Hunter row its own band-accurate "needed" signal, since cqdx's `is_needed_dxcc` only
   reflects whichever band the operator's last cqdx sync happened to be tuned to) was computed at
   both DX Hunter call sites (`coordinator/tui_relay.rs`'s `band_needed`) but only ever fed into the
   **display** DTO — never into `score_cq_detailed`, which still called `is_needed_dxcc` alone. Real
   per-row band awareness never reached the score. Fixed by OR-ing `is_dxcc_needed_on_band(callsign,
   freq_hz)` into the `needed_dxcc` term.
   - Follow-on bug this exposed: `is_dxcc_needed_on_band` had no home-country exclusion — in a
     fresh session (nothing worked on any band yet) it trivially returned `true` even for the
     operator's own callsign. Fixed by mirroring `is_needed_dxcc`'s `excluded_dxcc_prefixes` check
     inside `is_dxcc_needed_on_band` itself, so the display badge and the score agree and neither
     ever flags a home station as "needed."
2. **Rarity coverage gap.** `rarity_scores` (`CachedStationLookup`) is populated only from cqdx's
   `fetch_live_spots` poll — a sparse, current-band-scoped set of spot groups. Any locally-decoded
   callsign not in that specific poll (the large majority) silently fell through to `rarity()`'s
   neutral `0.5` default, so real per-callsign rarity essentially never differentiated ordinary
   rows. Fixed with an entity-keyed fallback cache (`rarity_by_entity`, derived in
   `update_rarity_scores` via the same offline prefix→entity resolver `worked_dxcc_on_band` already
   uses): a callsign never itself spotted now inherits real rarity data reported for any other
   callsign from the same DXCC entity, dramatically increasing effective coverage without a new
   cqdx endpoint. Also de-duplicated `CachedStationLookup`'s two copies of `rarity()` (inherent
   method + trait impl, previously identical bodies that could silently diverge) — the trait impl
   now delegates to the inherent one.

Deliberately NOT done as part of this fix (tracked separately, issue #164): the full 5-tier
lexicographic scoring redesign (ATNO > per-band-DXCC-new > special-stations > per-band-grid-new >
rarity-within-worked). This fix stays within the existing weighted-sum architecture — it makes the
existing weights and signals actually work as designed, not a redesign of the scheme itself. #164's
own scope note says the tiered redesign should build on these signals actually working, which they
now do.

`docs/qso-tx-deep-review-2026-07-18.md` and the issue itself (#163) have the full investigation
trail and on-air symptom description.
