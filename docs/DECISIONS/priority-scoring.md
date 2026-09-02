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

## 5-tier lexicographic redesign (issue #164), landed 2026-07-19

Replaces the flat weighted-sum with a strict tier ordering — ATNO > per-band-DXCC-new >
special-station > per-band-grid-new > everything-else — where a higher tier always outranks
every station in a lower tier regardless of any other factor. `PriorityTier`/`TieredScore` live in
`pancetta-qso::priority`; `PriorityScorer::score_tiered` combines tier classification with a
reduced continuous `secondary_score` (rarity/POTA-SOTA/signal/penalties/staleness — deliberately
excluding the needed/ATNO/notable signals now captured by tier) as the within-tier tiebreaker.

Tier 3 (special stations) detects US 1x1-format callsigns (`W1A`) and the UK `GB`-prefix
convention, plus a small curated international list (`4U1UN`, `4U1ITU`), layered on cqdx's
existing `is_notable` hook. Tier 4 (per-band grid-new) mirrors tier 2's per-band-DXCC tracking
exactly, via a new `CachedStationLookup::worked_grids_on_band` local-history cache — independent
of cqdx's still-unbuilt `needed-grids` server endpoint.

The two previously-divergent DX Hunter scorers (the real `PriorityScorer` for live decodes, and a
coarse `dx_priority_score` bucket function for network-only spots) are now unified: `pancetta-tui`
took on a new `pancetta-qso` dependency (confirmed non-cyclic — `pancetta-qso` has no dependency
back on `pancetta-tui`) so every DX Hunter row, regardless of source, is scored by the same
`PriorityScorer::score_tiered`. Display encoding is a single `u32`
(`TieredScore::as_display_u32`): `tier_rank * 1000 + secondary*999`, giving six clean 1000-wide
bands (Suspect 0-999, Standard 1000-1999, PerBandGridNew 2000-2999, SpecialStation 3000-3999,
PerBandDxccNew 4000-4999, Atno 5000-5999) that can never bleed into each other.

Full design: `docs/superpowers/specs/2026-07-19-dx-hunter-priority-tiers-and-history-panel-design.md`.

## Secondary-score compression (post-#164 clustering), fixed 2026-07-29

Operator reported the "no gradient" symptom was still present after #164's tier redesign — just
rescaled: scores now clustered around ~3000 and ~1000 (tier bands) instead of the original
~425/~75, with each cluster still visually indistinguishable internally. Root-caused via direct
code tracing + a quantitative test, not a re-guess at the original #163 hypothesis (which had
already been fixed and was confirmed still correct: `rarity()` and `is_dxcc_needed_on_band` are
both genuinely wired and varying).

The actual cause: `PriorityScorer::secondary_score`'s weighted sum (`rarity 0.10`,
`pota_sota 0.15`, `signal_strength 0.05`, plus a `±0.1` network SNR bonus) was left over from a
formula originally dominated by `needed_dxcc 0.35` + `atno_bonus 0.15` + `notable_bonus 0.3` —
terms #164 moved into tier classification. With those large terms gone, `secondary_score` clamped
its raw sum directly to `[0,1]`, but the remaining weights can only ever produce a raw value in a
narrow low sub-band (max ~0.40, and typically ~0.05-0.15 for a real station with no POTA/network
data) — so `TieredScore::as_display_u32` only ever moved a station within roughly the bottom 10%
of its tier's 999-wide band, regardless of true rarity or signal quality. The existing test
(`secondary_score_varies_by_rarity_within_standard_tier`) only asserted rare-beats-common
*ordering*, never magnitude, so this shipped invisibly.

Fixed by rescaling `secondary_score`'s raw sum by its actual achievable positive ceiling
(`rarity + pota_sota + signal_strength weights + the SNR bonus magnitude`) before clamping, so
real variation spans close to the full `[0,1]` range instead of a fixed narrow sub-band. A new
test (`secondary_score_rarity_spread_moves_the_display_by_a_meaningful_fraction_of_the_tier_band`)
asserts the full rarity spectrum (never-spotted vs. maximally-rare) must move the display by
>200 of the 999-wide band — it failed at 99 before the fix, passes at 250 after. On-air
re-verification of the visible gradient is still operator-gated.
