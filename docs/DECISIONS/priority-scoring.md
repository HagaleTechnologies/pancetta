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

## Explain the score and retain the latest observation (PAN-94, 2026-10-18)

`PriorityScorer::score_tiered_explained` captures each lookup input once in
`PriorityFactors` and computes the tier and secondary score from that record.
`score_tiered` delegates to it. Tier ordering, weights, operation order and the
continuous autonomous scorer are unchanged. A pre-refactor fingerprint over
18,432 input combinations verifies bit-identical scores; the explanation carries
its actual weighted terms, network bonus, ceiling and freshness multiplier.

DX Hunter stores the latest local and latest cqdx scoring separately. It displays
the higher of those two scores, preferring local data on ties. This replaces the
running maximum that kept old scores after a station was worked or a QSO failed.
The reason's headline and supporting clauses follow the winning score. Cautions
also retain authoritative failure/worked information from the other source.
The cqdx path still uses default weights and has no session failure history;
the detail view states that limitation. Local relay scores use operator weights
and the coordinator's shared lookup. DX-cluster-only rows remain unscored.

## Needed is a claim with availability (PAN-93, 2026-10-22)

cqdx.io's needed-DXCC and needed-grid lists were plain sets, and an empty set
stood for "no data". That merged two different facts into one bool. An empty
needed-DXCC set, whether cqdx never answered or answered "nothing needed", fell
back to "needed unless a home or logged prefix", or "needed" for every call when
there were no exclusions. DX Hunter then showed `+` and "needed entity (cqdx
list)" for a value cqdx never supplied. An empty needed-grid set read as "not
needed" whether cqdx never answered, answered 404, or answered "none". A failed
first fetch also left the spot poller unstarted, with no retry and one `warn!`
line as the only trace.

**Storage (D1).** `CachedStationLookup.needed_dxcc` and `needed_grids` are each
a `NeedList { entries, availability }` behind one lock, so a reader never sees
a new list with an old availability. `NeedListAvailability` is `NotConfigured`
(the default; `[network.cqdx]` off), `Loading`, `NotOffered`,
`Loaded { fetched_at, band }` (`band` is `None` for the all-bands list) or
`Unavailable { since, error, retrying }`. `needed_atno` stays a separate set,
written by the same `apply_needed` call.

**A verdict beside the trait (D2).** `needed_dxcc_status` and
`needed_grid_status` return `NeedStatus::{Needed, NotNeeded, Unknown}`. They
return `Unknown` whenever the list is not `Loaded`, and for a callsign or grid
that cannot be resolved. `WorkedStationLookup::is_needed_dxcc` and
`is_needed_grid` keep `-> bool` and become the scoring policy over that verdict,
so none of the trait's implementors change. This follows the PAN-85/86
precedent of adding inherent siblings beside the trait.

**Scoring policy (D3).** `Needed` scores true and `NotNeeded` scores false. For
`Unknown`, `is_needed_dxcc` keeps the Phase-5 hardening #2 guess: needed unless
the call matches an excluded (home or logged) prefix, and needed for every call
when there are no exclusions. `is_needed_grid` is true only for `Needed`, so an
unknown grid list still adds nothing. `PriorityWeights`, `score_cq_detailed`
and `score_tiered_explained` are unchanged.

`Unknown` keeps the guess because this ticket fixes what pancetta claims, not
what the unattended station calls during a cqdx outage. Giving `Unknown` zero
needed weight would drop every DX station's needed term while cqdx is down, a
change to autonomous behavior that deserves its own decision. The Why overlay
now says the score used a guess.

A loaded empty answer now means "not needed" for every station, in scoring and
display. cqdx answered and named nothing. Product assessment P04 requires that
"an empty successful response must not mean every station is needed", and the
old rule read an answered empty list as missing data. cqdx's contract
(`docs/cqdx-api-requirements.md`) already returns every entity for a user with
no log data, so an empty answer is not how a new account looks.

**Display and wire (D4).** The DX Hunter `+`, the station card's `needed` flag,
the remote `DecodedView.needed` and `CqdxSpotInfo.needed` are true only for
`NeedStatus::Needed`. They all read one helper,
`CachedStationLookup::display_needed_atno`. `is_needed_dxcc` is for scoring
only. The per-frame `DxNeedStatus` carries the verdict (`dxcc_needed`) and the
loaded list's band and fetch time (`dxcc_list`) to the Why surfaces. Remote
clients such as panino receive `needed = false` while the list is unknown,
because the wire field stays a bool.

**Bridge lifecycle (D5).** The coordinator makes one inline startup attempt,
then registers the bridge and spawns its poller whether or not that attempt
succeeded. The poller is still registered under `ComponentId::DxCluster`. Spot
polling waits until the needed-entity list has loaded. This keeps the earlier
guarantee that cqdx spots flow only alongside a loaded list, so TUI-scored
network rows and coordinator-scored decodes do not score differently during a
partial outage.

**Retry policy (D6).** Pending loads (the entity table, the needed-entity list
while the dial is unknown, and a grid list that can still be retried) retry on
the first poll tick at least `NEED_DATA_RETRY` (60 s) after the last attempt.
While the dial is known, the per-band needed fetch keeps retrying every tick,
as before. Permanent errors (`Unauthorized`, `NotConfigured`, `InvalidToken`,
`InvalidBaseUrl`, `HttpInit`) stop retries, and the strip says to fix the token
and restart. An enabled `[network.cqdx]` with a missing or empty token is
marked the same way. At startup, a transport error (`CqdxError::Http`) or a
permanent error stops the remaining requests, so the worst-case startup delay
stays one 30 s client timeout. A `Server` or `Json` error on one endpoint does
not stop the others. A needed-grids 404 is recorded as `NotOffered`, not as an
empty answer, and is not retried. When the needed-entity list loads after
failures, the log says `cqdx.io needed-entity list loaded after N failed
attempt(s)`.

**Failed refresh after a load (D7).** The list stays `Loaded` with its earlier
entries. The Why overlay shows its scope and age, so staleness is visible.
There is no staleness threshold and no periodic refresh of the needed lists.

**Strip warning (D8).** One Warning chip per outage: `⚠ CQDX NEEDS UNKNOWN`
(`ReadinessKey::CqdxNeeds`) when the needed-entity list is `Unavailable`,
otherwise `⚠ CQDX GRID NEEDS UNKNOWN` (`ReadinessKey::CqdxGridNeeds`) when only
the grid list is. The keys carry no error text, so an acknowledgement survives
a changing error and is pruned on recovery. There is no chip for
`NotConfigured`, `Loading` or `NotOffered`. Wording is in
[the TUI decision](tui.md#needed-entity-only-from-a-loaded-cqdxio-list-pan-93-2026-10-22).

**Dead copy (D9).** `pancetta_cqdx::CqdxCache::is_needed_dxcc` returns
`Option<bool>`, `None` while no needed data is loaded, so the repository no
longer carries a "no data means needed" rule.

States the operator can now tell apart:

| Situation | Availability | `+` | Strip | Why "needed entity" |
|---|---|---|---|---|
| `[network.cqdx]` off (no account) | `NotConfigured` | none | nothing | `unknown`, cqdx list not loaded |
| Enabled, token empty or invalid | `Unavailable { retrying: false }` | none | `⚠ CQDX NEEDS UNKNOWN` (fix token, restart) | `unknown` |
| cqdx rejects the token (401) | `Unavailable { retrying: false }` | none | `⚠ CQDX NEEDS UNKNOWN` (fix token, restart) | `unknown` |
| Outage at startup (refused, timeout, 5xx) | `Unavailable { retrying: true }` | none | `⚠ CQDX NEEDS UNKNOWN` (pancetta keeps retrying) | `unknown` |
| First fetch in flight | `Loading` | none | nothing | `unknown` |
| Loaded, entity needed | `Loaded` | `+` | nothing | `yes`, scope and age |
| Loaded, empty answer ("nothing needed") | `Loaded` | none | nothing | `no`, scope and age |
| Loaded earlier, later refresh failed | `Loaded` (kept) | from the kept list | nothing | `yes`/`no`, with the old scope and age |
| Needed-grid endpoint 404 | `NotOffered` | n/a | nothing | n/a |

Operators without cqdx.io lose the `+` marker, which was never backed by cqdx
data for them. Autonomous ranking for them is unchanged.

Follow-ups, not done here:

- A tri-state `needStatus` on the remote `DecodedView`/`DxRow`. It is a
  remote-protocol contract change, so it is proposed in dispensa first. Until
  then panino shows unknown as not needed.
- Mode scope. cqdx's needed endpoint takes a band only.
- Worked versus confirmed, and an incomplete-history or new-account signal.
  cqdx returns no per-entity worked/confirmed state and no log-completeness
  signal; both need a cqdx API, raised as a dispensa/cqdx question.
