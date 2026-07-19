# DX Hunter: 5-tier priority scoring (#164) + last-10-QSOs panel (#165)

## Context

Builds on the #163 fix (`docs/DECISIONS/priority-scoring.md`, PR #169): the per-band-DXCC-needed
signal now actually reaches the score, and rarity has real entity-keyed coverage instead of
collapsing to a flat 0.5 default. That fix stayed inside the existing flat weighted-sum
architecture. This spec is the actual redesign the operator proposed: a strict 5-tier ordering
where a higher tier always outranks every station in a lower tier, regardless of any other
factor — something a flat weighted sum cannot guarantee (a big rarity number can currently
outweigh a smaller "needed" bonus in ways that don't reflect what the operator actually wants).

Tiers, descending priority:

1. ATNO (all-time new one, never worked on any band)
2. Per-band DXCC new-one (never worked this entity on this band)
3. Special stations (event/gov/research/UN — detection scoped below)
4. Per-band grid-square new-one
5. Everything else, varying only by rarity/signal quality

Two independent features are bundled in this one design doc because #164 forces changes to a
shared type (`WorkedStationLookup`/`CachedStationLookup`) that #165 doesn't touch — the two are
otherwise unrelated and will land as separate PRs.

## Part A — #164: 5-tier priority scoring

### Core types (`pancetta-qso/src/priority.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PriorityTier {
    Standard,        // tier 5 (lowest)
    PerBandGridNew,   // tier 4
    SpecialStation,   // tier 3
    PerBandDxccNew,   // tier 2
    Atno,             // tier 1 (highest)
}
```

Declaration order is ascending priority — Rust's derived `Ord` on a fieldless enum ranks by
declaration order, so `Atno > PerBandDxccNew > SpecialStation > PerBandGridNew > Standard` falls
out of `#[derive(Ord)]` with no hand-written comparator to get wrong.

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TieredScore {
    pub tier: PriorityTier,
    /// Existing continuous weighted-sum score (rarity, POTA/SOTA, signal
    /// strength, duplicate/failure penalties, staleness), 0.0..=1.0. Breaks
    /// ties WITHIN a tier — this is what makes "Malawi > Italy, both
    /// worked" and "stronger ATNO signal > weaker ATNO signal" both fall
    /// out naturally, reusing the existing formula rather than inventing a
    /// second one.
    pub secondary: f64,
}

impl PartialOrd for TieredScore {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Eq for TieredScore {}
impl Ord for TieredScore {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.tier.cmp(&other.tier).then_with(|| {
            self.secondary
                .partial_cmp(&other.secondary)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }
}
```

(`secondary` is always produced via `.clamp(0.0, 1.0)` at the one call site that builds it, so
the `unwrap_or(Equal)` NaN fallback is defense-in-depth, not a real code path.)

### Classification (`PriorityScorer::classify_tier`)

New method alongside `score_cq_detailed`, same signature shape (`callsign, grid, freq_hz`), pure
function of the existing `WorkedStationLookup` trait plus the two new methods below:

```rust
fn classify_tier(&self, callsign: &str, grid: Option<&str>, freq_hz: f64) -> PriorityTier {
    let needed = self.lookup.is_needed_dxcc(callsign)
        || self.lookup.is_dxcc_needed_on_band(callsign, freq_hz);
    if needed && self.lookup.is_atno(callsign) {
        return PriorityTier::Atno;
    }
    if needed {
        return PriorityTier::PerBandDxccNew;
    }
    if self.lookup.is_notable(callsign) || is_special_event_callsign(callsign) {
        return PriorityTier::SpecialStation;
    }
    if let Some(g) = grid {
        if self.lookup.is_grid_needed_on_band(g, freq_hz) {
            return PriorityTier::PerBandGridNew;
        }
    }
    PriorityTier::Standard
}
```

`secondary` is deliberately NOT `score_cq_detailed`'s full total — that total already bakes in
`needed_dxcc`/`atno_bonus`/`notable_bonus`, the exact signals `classify_tier` just used. Reusing
it wholesale wouldn't corrupt ordering (those terms are constant across every member of a given
tier, since tier membership is derived from the same booleans — so it can't flip a within-tier
comparison), but it's a confusing false economy. Instead, a new private helper
`secondary_score(callsign, snr, freq_hz) -> f64` computes only the orthogonal factors — rarity,
POTA/SOTA, signal strength, duplicate/recent-failure penalties, network SNR bonus, staleness —
i.e. `score_cq_detailed`'s formula minus the `needed_dxcc`/`atno`/`notable_bonus` terms, clamped
to `[0,1]`. `score_cq_detailed`/`evaluate_cq` themselves are untouched (still used as-is by the
autonomous operator's continuous-threshold decision logic, which this redesign doesn't touch); a
new `pub fn score_tiered(&self, callsign, grid, snr, freq_hz) -> TieredScore` combines
`classify_tier` + `secondary_score` for callers (DX Hunter) that want the tiered ranking.

### Tier 3 detection: `is_special_event_callsign`

New free function in `priority.rs`, alongside `is_pota_sota_candidate`:

```rust
/// US 1x1 format (letter-digit-letter, e.g. `W1A`, `N4B`, `K9Z`) is FCC's
/// dedicated special-event format — never issued as a regular license
/// (the shortest regular US callsign is 4 characters), so this is a
/// zero-false-positive pattern. UK's GB-prefix convention (`GB` + digit +
/// suffix, e.g. `GB2RS`) is the equivalent in the UK. A small static list
/// covers well-known permanent international special-service stations
/// whose prefix carries no other meaning (UN/ITU HQ stations).
pub fn is_special_event_callsign(callsign: &str) -> bool {
    let upper = callsign.to_uppercase();
    if is_us_1x1_format(&upper) {
        return true;
    }
    if upper.starts_with("GB") && upper.chars().nth(2).is_some_and(|c| c.is_ascii_digit()) {
        return true;
    }
    matches!(upper.as_str(), "4U1UN" | "4U1ITU")
}

fn is_us_1x1_format(upper: &str) -> bool {
    let chars: Vec<char> = upper.chars().collect();
    chars.len() == 3
        && chars[0].is_ascii_alphabetic()
        && chars[1].is_ascii_digit()
        && chars[2].is_ascii_alphabetic()
}
```

Layered on top of the existing `is_notable`/`notable_callsigns` cqdx hook, not replacing it —
`classify_tier` above ORs the two.

### Tier 4: per-band grid tracking

New trait method (default `false`, mirroring `is_dxcc_needed_on_band`'s shape):

```rust
fn is_grid_needed_on_band(&self, _grid: &str, _freq_hz: f64) -> bool {
    false
}
```

`CachedStationLookup` (`pancetta/src/priority_evaluator.rs`) gets a new
`worked_grids_on_band: Arc<RwLock<HashMap<String, HashSet<String>>>>` field, populated the same
way as `worked_dxcc_on_band`:
- `seed_worked_grids_from_list(pairs: Vec<(String /*band*/, String /*grid*/)>)` at startup, from
  the QSO database (new query alongside `get_worked_bands_and_callsigns`, or extend it to also
  return grids — implementer's call at plan time based on what's cheapest against the existing
  schema).
- `record_worked` gains the grid square as a parameter (or a sibling `record_worked_grid` called
  alongside it) so live QSO completions keep the cache current, not just startup seeding.

No home-grid exclusion is needed (unlike DXCC's home-prefix exclusion) — a CQ from your own grid
square isn't a real scenario (you don't hear yourself), so there's no equivalent false-positive
to guard against.

`is_grid_needed_on_band` implementation mirrors `is_dxcc_needed_on_band` exactly, minus the
exclusion check:
```rust
fn is_grid_needed_on_band(&self, grid: &str, freq_hz: f64) -> bool {
    let trimmed = grid.trim();
    if trimmed.len() < 4 { return false; }
    let field = trimmed[..4].to_uppercase();
    let band = pancetta_qso::utils::frequency_to_band(freq_hz).to_uppercase();
    !self.worked_grids_on_band.read().get(&band).is_some_and(|s| s.contains(&field))
}
```

This tracks local history regardless of cqdx; the cqdx-sourced `needed_grid`/`is_needed_grid`
signal stays exactly as inert as it is today until `/api/v1/entities/needed-grids` ships
server-side — that's an orthogonal, already-tracked gap (see `project_open_followups.md`), not
something this design needs to unblock.

### Unifying the two scorers

Today `pancetta-tui`'s `dx_hunter.rs::dx_priority_score`/`calculate_dx_priority` duplicate a
coarse version of the same need-hierarchy, entirely inside `pancetta-tui` (confirmed via
Cargo.toml: `pancetta-tui` currently has no dependency on `pancetta-qso`, and `pancetta-qso` has
no dependency on `pancetta-tui` — so adding the edge is non-cyclic).

Plan: add `pancetta-qso = { path = "../pancetta-qso" }` to `pancetta-tui/Cargo.toml`. Delete
`dx_priority_score`/`calculate_dx_priority` and their tests from `dx_hunter.rs`. The two call
sites in `app.rs` (`calculate_dx_priority` at line ~1646, the network-spot path at ~3294) both
build a `pancetta_qso::priority::PriorityScorer` and call its public `score_tiered(...)` method —
since `app.rs` doesn't have a `CachedStationLookup` (that lives in the `pancetta` binary crate,
one layer up), it needs its own minimal `WorkedStationLookup` impl backed by whatever local
fields `App` already tracks (worked-before flags, atno/needed flags already carried on
`DxStation`, cqdx rarity_tier string, etc.) — this adapter is the one genuinely new piece of
unification work; exact field mapping is a plan-time detail, not a design blocker, since every
input it needs already exists on `DxStation` today.

`DxStation.priority_score` stays a `u32` for display/sort compatibility. New encoding:
```rust
let display_score = (tier as u32) * 1000 + (secondary.clamp(0.0, 1.0) * 999.0) as u32;
```
Ranges: Standard 0–999, PerBandGridNew 1000–1999, SpecialStation 2000–2999, PerBandDxccNew
3000–3999, Atno 4000–4999 — one sortable integer, strict tier dominance guaranteed by the 1000-gap.
`dx_hunter.rs`'s `priority_style` thresholds (`score > 100` / `score > 50`) get re-tuned to the
new range (e.g. `> 2000` bold/error, `> 1000` warning, else dim) since the old thresholds assumed
the additive scheme's much smaller numbers.

### Testing

- `PriorityTier` ordering: derived `Ord` produces the exact 5-way ranking (one test enumerating
  all pairs).
- `classify_tier`: one test per tier boundary (ATNO beats needed even with weak signal; needed
  beats special-station; special-station beats grid-new; grid-new beats standard) — mirrors the
  existing `dx_priority_hierarchy_atno_over_needed_over_rare_over_plain` shape already in
  `dx_hunter.rs`, now against the real scorer instead of the coarse one being deleted.
  - Regression pin: `test_atno_bonus_lifts_score_over_plain_needed` and the #163 regression tests
    (`dxcc_needed_on_band_boosts_score_even_when_cqdx_needed_set_disagrees`,
    `test_priority_score_non_us_high_with_default_exclusions`) get equivalent tier-based versions
    so #163's fix stays provably intact under the new scheme.
- `is_special_event_callsign`: 1x1 true positives (`W1A`, `N4B`, `K9Z`), GB true positives
  (`GB2RS`), international list hits, and explicit false-positive guards (`W1AW` — real 4-char
  call must not match; `GB` alone or `GBABC` with no digit must not match).
- `is_grid_needed_on_band`: same test shape as the existing `dxcc_needed_on_band_*` suite
  (true-before-worked, false-after-working-that-band, true-on-a-different-band).
- Display encoding: tier boundaries never bleed into each other even at `secondary = 1.0` (999 <
  1000, so no off-by-one at the edge).
- Full existing `pancetta-qso` + `priority_evaluator` + `dx_hunter` suites stay green; the
  now-deleted `dx_hunter.rs` tests for the coarse scorer are replaced 1:1 by tier-based
  equivalents, not just removed.

## Part B — #165: last-10-QSOs success indicator

### New bus message (`pancetta/src/message_bus.rs`)

```rust
/// Pushed once per QSO reaching a terminal state (Completed or Failed).
/// Additive to the enum — existing ActiveQsosSnapshot handling is untouched.
QsoHistoryEntry {
    call_sign: String,
    band: String,
    success: bool,
    /// Populated only when `success` is false (e.g. "Timeout", "SignalLost").
    reason: Option<String>,
    completed_at: chrono::DateTime<chrono::Utc>,
},
```

Wired from the same place `pancetta/src/coordinator/tui_relay.rs` already forwards
`ActiveQsosSnapshot` to the TUI — that task already listens on the QSO engine's event stream, so
this piggybacks on the existing subscription rather than spawning a new one. On
`QsoEvent::QsoCompleted` → push `success: true`; on `QsoEvent::QsoFailed { reason, .. }` → push
`success: false, reason: Some(format!("{reason:?}"))`.

### TUI side (`pancetta-tui/src/app.rs`)

```rust
pub struct QsoHistoryItem {
    pub call_sign: String,
    pub success: bool,
    pub completed_at: chrono::DateTime<chrono::Utc>,
}
```
New `qso_history: VecDeque<QsoHistoryItem>` on `App`, capped at 10 (push front, `truncate(10)`
after each insert). New match arm on the bus message handler pushes an entry.

### Render (`pancetta-tui/src/ui/qso_status.rs`)

One new line in the single-QSO-detail layout only (the multi-QSO table view is already denser
and doesn't get it) — add a `Constraint::Length(1)` row and a pure function:

```rust
pub(crate) fn format_qso_history_line(items: &[QsoHistoryItem]) -> Vec<Span<'static>>
```

Renders most-recent-first as `✓K5ARH ✗JA1ABC ✓DL5XYZ ...`, green for success / red for failure
(not color-only — the ✓/✗ glyph itself carries the meaning), following the exact pattern of
`format_queued_line` (pure, directly unit-testable, called from the render function which just
wraps it in a `Paragraph`).

### Testing

- `format_qso_history_line`: empty list → empty line; ordering (most recent first); glyph/color
  pairing for success vs failure; truncation behavior at exactly 10 items lives in the `App`
  push logic, tested there (VecDeque cap), not in the pure formatter (which just renders whatever
  slice it's given).
- `App`: pushing an 11th entry evicts the oldest; a `QsoHistoryEntry` bus message with
  `success: false` produces an item that renders with the failure glyph.

## Docs to update (same PRs)

- `docs/DECISIONS/priority-scoring.md`: append the #164 tiered-redesign section alongside the
  existing #163 writeup (same file, since it's already the scoring subsystem's narrative).
- `docs/DECISIONS/tui.md` (or wherever TUI panel additions are tracked): #165's history line.
- `docs/ARCHITECTURE.md`: note the new `pancetta-tui → pancetta-qso` dependency edge.
- `CLAUDE.md`: no new invariant needed for #164 (tier dominance is an implementation detail, not
  a cross-cutting invariant like the ones already listed); #165 doesn't need one either.

## Out of scope

- Any change to cqdx's server-side `needed-grids` endpoint (tier 4's cqdx-sourced half stays
  inert regardless — tracked separately, not blocking).
- A standalone panel for #165 (explicitly decided against — folding into QSO Status instead).
- Retuning `PriorityWeights` constants themselves — the `secondary` score reuses the existing
  formula unchanged; only the tier layer is new.
