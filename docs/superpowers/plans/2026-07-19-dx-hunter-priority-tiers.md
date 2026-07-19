# DX Hunter 5-Tier Priority Scoring (#164) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the flat weighted-sum priority formula with a strict 5-tier lexicographic
scheme (ATNO > per-band-DXCC-new > special-station > per-band-grid-new > everything-else) so a
higher tier always outranks a lower one regardless of any other factor, and unify the two
currently-divergent DX Hunter scorers (the real `PriorityScorer` used for live decodes, and the
coarse `dx_priority_score` used for network-only spots) into one.

**Architecture:** A new `PriorityTier` enum (ranked via derived `Ord`) plus a `TieredScore {
tier, secondary }` type live in `pancetta-qso::priority` alongside the existing `PriorityScorer`.
`secondary` is a reduced continuous score (rarity, POTA/SOTA, signal, penalties, staleness —
deliberately excluding the signals now captured by tier) that breaks ties within a tier.
`pancetta-tui` gains a new `pancetta-qso` dependency (confirmed non-cyclic) so both the
live-decode path (`pancetta/src/coordinator/tui_relay.rs`) and the two TUI-side paths
(`pancetta-tui/src/app.rs`'s local-decode fallback and network-spot merge) go through the same
`PriorityScorer::score_tiered(...)`.

**Tech Stack:** Rust workspace (pancetta-qso, pancetta, pancetta-tui crates), existing `parking_lot::RwLock`-backed `CachedStationLookup`, existing `sqlx` SQLite QSO database.

## Global Constraints

- `cargo fmt` must be run for real (not just `--check`) before every commit — a non-empty
  `--check` diff means unformatted code, re-run plain `cargo fmt` and re-verify clean.
- Every new signal added to `WorkedStationLookup` gets a default trait-method implementation
  (`false`/`None`) so existing implementors don't break.
- Never change `record_worked`'s existing signature (`callsign, band`) — it has a production call
  site (`pancetta/src/coordinator/qso.rs:2022`) and multiple test call sites; add new methods
  alongside it instead.
- `pancetta-qso` must not gain a dependency on `pancetta-tui` (would create the cycle
  `pancetta-tui → pancetta-qso → pancetta-tui`) — the offline DXCC resolver
  (`pancetta_tui::dxcc::entity_for_callsign`) stays used only from the `pancetta` binary crate
  (`priority_evaluator.rs`), never from inside `pancetta-qso` itself.
- Run the full existing suite for every crate touched before each commit that isn't purely
  additive — this repo's own #163 fix caught a real regression this way; don't skip it.

---

## File Structure

| File | Responsibility |
|---|---|
| `pancetta-qso/src/priority.rs` | `PriorityTier`, `TieredScore`, `is_special_event_callsign`, `is_grid_needed_on_band` trait default, `PriorityScorer::classify_tier`/`secondary_score`/`score_tiered` |
| `pancetta-qso/src/async_database.rs` | New `get_worked_bands_and_grids()` query, mirroring `get_worked_bands_and_callsigns` |
| `pancetta/src/priority_evaluator.rs` | `CachedStationLookup::worked_grids_on_band` field + `seed_worked_grids_from_list`/`record_worked_grid` + `is_grid_needed_on_band` impl |
| `pancetta/src/coordinator/qso.rs` | Wire `record_worked_grid` (live QSO completion) and `seed_worked_grids_from_list` (startup) alongside the existing DXCC calls |
| `pancetta/src/coordinator/tui_relay.rs` | Switch live-decode `priority_score` from `evaluate_cq * 1000` to `score_tiered().as_display_u32()` |
| `pancetta-tui/Cargo.toml` | New `pancetta-qso` path dependency |
| `pancetta-tui/src/app.rs` | New `DxStationLookupAdapter` + `rarity_tier_to_f64`; rewire `calculate_dx_priority` and `merge_spot_groups`'s network-spot scoring |
| `pancetta-tui/src/ui/dx_hunter.rs` | Delete `dx_priority_score`/`calculate_dx_priority` + their tests; retune `priority_style` thresholds to the new 0–4999 range |
| `docs/DECISIONS/priority-scoring.md` | Append the #164 section |
| `docs/ARCHITECTURE.md` | Note the new `pancetta-tui → pancetta-qso` dependency edge |

---

## Task 1: `PriorityTier` and `TieredScore` types

**Files:**
- Modify: `pancetta-qso/src/priority.rs` (add near the top, after `PriorityWeights`)
- Test: same file, `#[cfg(test)] mod tests` block

**Interfaces:**
- Produces: `pub enum PriorityTier { Standard, PerBandGridNew, SpecialStation, PerBandDxccNew, Atno }` (declaration order = ascending priority), `pub struct TieredScore { pub tier: PriorityTier, pub secondary: f64 }` with `impl TieredScore { pub fn as_display_u32(&self) -> u32 }`, `Ord`/`PartialOrd`/`Eq` on `TieredScore`.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `pancetta-qso/src/priority.rs`:

```rust
#[test]
fn priority_tier_ordering_is_strictly_atno_gt_dxcc_gt_special_gt_grid_gt_standard() {
    use PriorityTier::*;
    assert!(Atno > PerBandDxccNew);
    assert!(PerBandDxccNew > SpecialStation);
    assert!(SpecialStation > PerBandGridNew);
    assert!(PerBandGridNew > Standard);
}

#[test]
fn tiered_score_orders_by_tier_first_secondary_second() {
    let low_tier_high_secondary = TieredScore {
        tier: PriorityTier::Standard,
        secondary: 0.99,
    };
    let high_tier_low_secondary = TieredScore {
        tier: PriorityTier::Atno,
        secondary: 0.01,
    };
    assert!(
        high_tier_low_secondary > low_tier_high_secondary,
        "tier must dominate regardless of secondary"
    );

    let a = TieredScore {
        tier: PriorityTier::Standard,
        secondary: 0.3,
    };
    let b = TieredScore {
        tier: PriorityTier::Standard,
        secondary: 0.7,
    };
    assert!(b > a, "within the same tier, secondary breaks the tie");
}

#[test]
fn tiered_score_display_u32_never_lets_secondary_bleed_into_the_next_tier() {
    let top_of_standard = TieredScore {
        tier: PriorityTier::Standard,
        secondary: 1.0,
    };
    let bottom_of_grid_new = TieredScore {
        tier: PriorityTier::PerBandGridNew,
        secondary: 0.0,
    };
    assert!(top_of_standard.as_display_u32() < bottom_of_grid_new.as_display_u32());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta-qso priority_tier_ordering_is_strictly -- --nocapture`
Expected: FAIL with `cannot find type PriorityTier in this scope` (or similar compile error) — the
type doesn't exist yet.

- [ ] **Step 3: Write minimal implementation**

Add to `pancetta-qso/src/priority.rs`, directly after the `PriorityWeights` struct/impl block (before `ScoreBreakdown`):

```rust
/// Lexicographic priority tier (#164 redesign). Declaration order below is
/// ascending priority — Rust's derived `Ord` on a fieldless enum ranks by
/// declaration order, so `Atno > PerBandDxccNew > SpecialStation >
/// PerBandGridNew > Standard` falls out for free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PriorityTier {
    /// Tier 5 (lowest): everything else, varying only by rarity/signal quality.
    Standard,
    /// Tier 4: per-band grid-square new-one.
    PerBandGridNew,
    /// Tier 3: special stations (event/gov/research/UN).
    SpecialStation,
    /// Tier 2: per-band DXCC new-one (never worked this entity on this band).
    PerBandDxccNew,
    /// Tier 1 (highest): all-time new one — never worked on any band.
    Atno,
}

/// A tier plus a continuous tiebreaker within that tier. `secondary` is
/// deliberately NOT the full `score_cq_detailed` total — it excludes the
/// `needed_dxcc`/`atno_bonus`/`notable_bonus` terms, since those signals
/// now drive tier classification instead (see `PriorityScorer::secondary_score`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TieredScore {
    pub tier: PriorityTier,
    pub secondary: f64,
}

impl TieredScore {
    /// Encode as a single sortable u32 for display: tier dominates via a
    /// 1000-wide band per tier, secondary breaks ties within a tier.
    /// Ranges: Standard 0-999, PerBandGridNew 1000-1999, SpecialStation
    /// 2000-2999, PerBandDxccNew 3000-3999, Atno 4000-4999.
    pub fn as_display_u32(&self) -> u32 {
        (self.tier as u32) * 1000 + (self.secondary.clamp(0.0, 1.0) * 999.0).round() as u32
    }
}

impl Eq for TieredScore {}

impl PartialOrd for TieredScore {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

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

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pancetta-qso priority_tier_ordering_is_strictly tiered_score_orders_by_tier_first tiered_score_display_u32 -- --nocapture`
Expected: 3 passed

- [ ] **Step 5: Commit**

```bash
git add pancetta-qso/src/priority.rs
git commit -m "feat(qso): add PriorityTier and TieredScore types for #164"
```

---

## Task 2: `is_special_event_callsign` detection

**Files:**
- Modify: `pancetta-qso/src/priority.rs` (add near `is_pota_sota_candidate`)
- Test: same file

**Interfaces:**
- Consumes: nothing new
- Produces: `pub fn is_special_event_callsign(callsign: &str) -> bool`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn is_special_event_callsign_detects_us_1x1_format() {
    assert!(is_special_event_callsign("W1A"));
    assert!(is_special_event_callsign("N4B"));
    assert!(is_special_event_callsign("K9Z"));
    assert!(is_special_event_callsign("w1a")); // case-insensitive
}

#[test]
fn is_special_event_callsign_rejects_regular_us_calls() {
    // Shortest real US callsign is 4 characters (1x2 format) — never 3.
    assert!(!is_special_event_callsign("W1AW"));
    assert!(!is_special_event_callsign("K5ARH"));
}

#[test]
fn is_special_event_callsign_detects_uk_gb_convention() {
    assert!(is_special_event_callsign("GB2RS"));
    assert!(is_special_event_callsign("gb2rs")); // case-insensitive
    assert!(is_special_event_callsign("GB0ABC"));
}

#[test]
fn is_special_event_callsign_rejects_gb_prefix_without_a_digit() {
    assert!(!is_special_event_callsign("GBABC"));
    assert!(!is_special_event_callsign("GB"));
}

#[test]
fn is_special_event_callsign_detects_curated_international_list() {
    assert!(is_special_event_callsign("4U1UN"));
    assert!(is_special_event_callsign("4U1ITU"));
    assert!(!is_special_event_callsign("4U1XYZ")); // not in the curated list
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta-qso is_special_event_callsign -- --nocapture`
Expected: FAIL with `cannot find function is_special_event_callsign in this scope`

- [ ] **Step 3: Write minimal implementation**

Add directly after `is_pota_sota_candidate` in `pancetta-qso/src/priority.rs`:

```rust
/// US 1x1 format (letter-digit-letter, e.g. `W1A`, `N4B`, `K9Z`) is FCC's
/// dedicated special-event format — never issued as a regular license (the
/// shortest regular US callsign is 4 characters), so this is a
/// zero-false-positive pattern. UK's GB-prefix convention (`GB` + digit +
/// suffix, e.g. `GB2RS`) is the equivalent in the UK. A small static list
/// covers well-known permanent international special-service stations
/// (UN/ITU HQ stations).
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

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pancetta-qso is_special_event_callsign -- --nocapture`
Expected: 5 passed

- [ ] **Step 5: Commit**

```bash
git add pancetta-qso/src/priority.rs
git commit -m "feat(qso): add is_special_event_callsign detection for #164 tier 3"
```

---

## Task 3: `is_grid_needed_on_band` trait default + `classify_tier`/`secondary_score`/`score_tiered`

**Files:**
- Modify: `pancetta-qso/src/priority.rs`
- Test: same file (extend `TestLookup` in the existing test module)

**Interfaces:**
- Consumes: `PriorityTier`, `TieredScore`, `is_special_event_callsign`, `is_pota_sota_candidate`, `normalize_snr` (all from Tasks 1–2 / already in this file), `WorkedStationLookup` trait
- Produces: `WorkedStationLookup::is_grid_needed_on_band(&self, grid: &str, freq_hz: f64) -> bool` (default `false`), `PriorityScorer::score_tiered(&self, callsign: &str, grid: Option<&str>, snr: i8, freq_hz: f64) -> TieredScore` (public)

- [ ] **Step 1: Write the failing test**

Extend `TestLookup` (in the existing `#[cfg(test)] mod tests` block) with a new field and trait override, then add the classification tests:

```rust
struct TestLookup {
    duplicates: HashSet<String>,
    recent_failures: HashSet<String>,
    needed_dxcc: HashSet<String>,
    needed_grids: HashSet<String>,
    dxcc_needed_on_band: HashSet<String>,
    grid_needed_on_band: HashSet<String>,
    atno: HashSet<String>,
    notable: HashSet<String>,
}

impl TestLookup {
    fn new() -> Self {
        Self {
            duplicates: HashSet::new(),
            recent_failures: HashSet::new(),
            needed_dxcc: HashSet::new(),
            needed_grids: HashSet::new(),
            dxcc_needed_on_band: HashSet::new(),
            grid_needed_on_band: HashSet::new(),
            atno: HashSet::new(),
            notable: HashSet::new(),
        }
    }
}

impl WorkedStationLookup for TestLookup {
    fn is_duplicate(&self, callsign: &str, _freq_hz: f64) -> bool {
        self.duplicates.contains(callsign)
    }
    fn is_recent_failure(&self, callsign: &str) -> bool {
        self.recent_failures.contains(callsign)
    }
    fn is_needed_dxcc(&self, callsign: &str) -> bool {
        self.needed_dxcc.contains(callsign)
    }
    fn is_dxcc_needed_on_band(&self, callsign: &str, _freq_hz: f64) -> bool {
        self.dxcc_needed_on_band.contains(callsign)
    }
    fn is_needed_grid(&self, grid: &str) -> bool {
        self.needed_grids.contains(grid)
    }
    fn is_grid_needed_on_band(&self, grid: &str, _freq_hz: f64) -> bool {
        self.grid_needed_on_band.contains(grid)
    }
    fn is_atno(&self, callsign: &str) -> bool {
        self.atno.contains(callsign)
    }
    fn is_notable(&self, callsign: &str) -> bool {
        self.notable.contains(callsign)
    }
}

#[test]
fn classify_tier_atno_beats_everything_even_with_weak_signal() {
    let mut lookup = TestLookup::new();
    lookup.needed_dxcc.insert("JA1ABC".to_string());
    lookup.atno.insert("JA1ABC".to_string());
    let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));
    let score = scorer.score_tiered("JA1ABC", Some("PM95"), -24, 14_074_000.0);
    assert_eq!(score.tier, PriorityTier::Atno);
}

#[test]
fn classify_tier_needed_without_atno_is_per_band_dxcc_new() {
    let mut lookup = TestLookup::new();
    lookup.needed_dxcc.insert("JA1ABC".to_string());
    let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));
    let score = scorer.score_tiered("JA1ABC", Some("PM95"), -10, 14_074_000.0);
    assert_eq!(score.tier, PriorityTier::PerBandDxccNew);
}

#[test]
fn classify_tier_notable_non_needed_is_special_station() {
    let mut lookup = TestLookup::new();
    lookup.notable.insert("VP8STI".to_string());
    let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));
    let score = scorer.score_tiered("VP8STI", None, -10, 14_074_000.0);
    assert_eq!(score.tier, PriorityTier::SpecialStation);
}

#[test]
fn classify_tier_special_event_pattern_is_special_station_even_without_cqdx_notable() {
    let lookup = TestLookup::new();
    let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));
    let score = scorer.score_tiered("W1A", None, -10, 14_074_000.0);
    assert_eq!(score.tier, PriorityTier::SpecialStation);
}

#[test]
fn classify_tier_grid_needed_is_per_band_grid_new() {
    let mut lookup = TestLookup::new();
    lookup.grid_needed_on_band.insert("PM95".to_string());
    let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));
    let score = scorer.score_tiered("W1XYZ", Some("PM95"), -10, 14_074_000.0);
    assert_eq!(score.tier, PriorityTier::PerBandGridNew);
}

#[test]
fn classify_tier_plain_station_is_standard() {
    let lookup = TestLookup::new();
    let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));
    let score = scorer.score_tiered("W1XYZ", Some("FN42"), -10, 14_074_000.0);
    assert_eq!(score.tier, PriorityTier::Standard);
}

#[test]
fn secondary_score_varies_by_rarity_within_standard_tier() {
    let mut rarity_map = HashMap::new();
    rarity_map.insert("3Y0J".to_string(), 0.95);
    struct RarityOnlyLookup {
        map: HashMap<String, f64>,
    }
    impl WorkedStationLookup for RarityOnlyLookup {
        fn is_duplicate(&self, _c: &str, _f: f64) -> bool {
            false
        }
        fn is_recent_failure(&self, _c: &str) -> bool {
            false
        }
        fn is_needed_dxcc(&self, _c: &str) -> bool {
            false
        }
        fn is_needed_grid(&self, _g: &str) -> bool {
            false
        }
        fn rarity(&self, c: &str) -> f64 {
            self.map.get(c).copied().unwrap_or(0.5)
        }
    }
    let rare_scorer = PriorityScorer::new(
        PriorityWeights::default(),
        Box::new(RarityOnlyLookup {
            map: rarity_map.clone(),
        }),
    );
    let common_scorer = PriorityScorer::new(
        PriorityWeights::default(),
        Box::new(RarityOnlyLookup {
            map: HashMap::new(),
        }),
    );
    let rare = rare_scorer.score_tiered("3Y0J", None, -10, 14_074_000.0);
    let common = common_scorer.score_tiered("W1XYZ", None, -10, 14_074_000.0);
    assert_eq!(rare.tier, PriorityTier::Standard);
    assert_eq!(common.tier, PriorityTier::Standard);
    assert!(
        rare.secondary > common.secondary,
        "rarer station must rank higher within the same (Standard) tier: {} vs {}",
        rare.secondary,
        common.secondary
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta-qso classify_tier secondary_score -- --nocapture`
Expected: FAIL — `score_tiered` and `is_grid_needed_on_band` don't exist yet.

- [ ] **Step 3: Write minimal implementation**

Add `is_grid_needed_on_band` to the `WorkedStationLookup` trait definition (after `is_dxcc_needed_on_band`):

```rust
    /// Is this grid square needed specifically on THIS band — never worked
    /// there before, per the local QSO database (mirrors
    /// `is_dxcc_needed_on_band`'s per-band-not-all-time semantics, one tier
    /// down). Defaults to `false` for lookups that don't track this.
    fn is_grid_needed_on_band(&self, _grid: &str, _freq_hz: f64) -> bool {
        false
    }
```

Add to `impl PriorityScorer` (after `score_cq_detailed`):

```rust
    /// Classify into one of the 5 lexicographic tiers (#164). ATNO always
    /// wins when the entity is also needed (mirrors `score_cq_detailed`'s
    /// existing "ATNO premium only meaningful when needed" rule); needed
    /// alone (not ATNO) is tier 2; special-station patterns/cqdx-notable is
    /// tier 3; per-band grid-new is tier 4; everything else is Standard.
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

    /// Continuous tiebreaker within a tier. Deliberately excludes
    /// `needed_dxcc`/`atno_bonus`/`notable_bonus` — those signals now drive
    /// `classify_tier` instead, and re-including them here would be a
    /// redundant (if harmless, since they're constant within a tier) false
    /// economy. Reuses the `rarity`/`pota_sota`/`signal_strength`/penalty/
    /// staleness weights from `score_cq_detailed`'s formula.
    fn secondary_score(&self, callsign: &str, snr: i8, freq_hz: f64) -> f64 {
        let pota_sota = if is_pota_sota_candidate(callsign) {
            1.0
        } else {
            0.0
        };
        let rarity = self.lookup.rarity(callsign);
        let signal_strength = normalize_snr(snr);
        let duplicate_penalty = if self.lookup.is_duplicate(callsign, freq_hz) {
            1.0
        } else {
            0.0
        };
        let recent_failure_penalty = if self.lookup.is_recent_failure(callsign) {
            1.0
        } else {
            0.0
        };
        let staleness = if let Some(last_seen) = self.lookup.network_last_seen(callsign) {
            let now = chrono::Utc::now().timestamp();
            let age_secs = (now - last_seen).max(0);
            match age_secs {
                0..=300 => 1.0,
                301..=600 => 0.7,
                601..=900 => 0.4,
                _ => 0.2,
            }
        } else {
            1.0
        };
        let snr_bonus = if let Some((reporter_count, best_snr)) = self.lookup.network_snr(callsign)
        {
            if reporter_count >= 5 && best_snr >= -20 {
                0.1
            } else if reporter_count == 1 && best_snr < -25 {
                -0.1
            } else {
                0.0
            }
        } else {
            0.0
        };
        let raw = (rarity * self.weights.rarity
            + pota_sota * self.weights.pota_sota
            + signal_strength * self.weights.signal_strength
            + duplicate_penalty * self.weights.duplicate_penalty
            + recent_failure_penalty * self.weights.recent_failure_penalty
            + snr_bonus)
            * staleness;
        raw.clamp(0.0, 1.0)
    }

    /// Tiered score (#164): combines `classify_tier` (dominant) with
    /// `secondary_score` (tiebreak within a tier). This is what the DX
    /// Hunter display should use; `evaluate_cq`/`score_cq_detailed` stay
    /// unchanged for the autonomous operator's continuous-threshold
    /// decision logic, which this redesign doesn't touch.
    pub fn score_tiered(&self, callsign: &str, grid: Option<&str>, snr: i8, freq_hz: f64) -> TieredScore {
        TieredScore {
            tier: self.classify_tier(callsign, grid, freq_hz),
            secondary: self.secondary_score(callsign, snr, freq_hz),
        }
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pancetta-qso classify_tier secondary_score -- --nocapture`
Expected: 7 passed

- [ ] **Step 5: Run the full `pancetta-qso` suite to check for regressions**

Run: `cargo test -p pancetta-qso`
Expected: all pass (no existing test references the new trait method or types, so this should be
a clean addition — but confirm before committing per this repo's established discipline).

- [ ] **Step 6: Commit**

```bash
git add pancetta-qso/src/priority.rs
git commit -m "feat(qso): add classify_tier/secondary_score/score_tiered for #164"
```

---

## Task 4: `get_worked_bands_and_grids` database query

**Files:**
- Modify: `pancetta-qso/src/async_database.rs` (add after `get_worked_bands_and_callsigns`, ~line 841)
- Test: same file's `#[cfg(test)] mod tests` block

**Interfaces:**
- Produces: `pub async fn get_worked_bands_and_grids(&self) -> Vec<(String, String)>` on `QsoDatabase`

- [ ] **Step 1: Write the failing test**

Add near `get_worked_bands_and_callsigns_returns_every_band_in_one_query` in the test module:

```rust
#[tokio::test]
async fn get_worked_bands_and_grids_returns_every_band_in_one_query() {
    let db = QsoDatabase::new_in_memory().await.unwrap();
    db.insert_qso(&duplicate_check_test_progress("JA1ABC", 14_074_000.0))
        .await
        .unwrap();
    db.insert_qso(&duplicate_check_test_progress("VK2XYZ", 7_074_000.0))
        .await
        .unwrap();

    let mut pairs = db.get_worked_bands_and_grids().await;
    pairs.sort();

    // duplicate_check_test_progress doesn't set a grid, so with the default
    // fixture this should be empty (grids.theirs is None) — this pins the
    // "no grid on the QSO -> no pair emitted" behavior, not a false-positive.
    assert!(pairs.is_empty());
}

#[tokio::test]
async fn get_worked_bands_and_grids_includes_grid_when_present() {
    let db = QsoDatabase::new_in_memory().await.unwrap();
    let mut progress = duplicate_check_test_progress("JA1ABC", 14_074_000.0);
    progress.metadata.grids.theirs = Some("PM95".to_string());
    db.insert_qso(&progress).await.unwrap();

    let pairs = db.get_worked_bands_and_grids().await;
    assert_eq!(pairs.len(), 1);
    assert!(pairs[0].0.eq_ignore_ascii_case("20m"));
    assert_eq!(pairs[0].1, "PM95");
}

#[tokio::test]
async fn get_worked_bands_and_grids_empty_db_returns_empty() {
    let db = QsoDatabase::new_in_memory().await.unwrap();
    assert!(db.get_worked_bands_and_grids().await.is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta-qso get_worked_bands_and_grids -- --nocapture`
Expected: FAIL — method doesn't exist.

- [ ] **Step 3: Write minimal implementation**

Add directly after `get_worked_bands_and_callsigns` in `pancetta-qso/src/async_database.rs`:

```rust
    /// Mirrors `get_worked_bands_and_callsigns` but for the DX's grid square
    /// instead of callsign — feeds `CachedStationLookup::seed_worked_grids_from_list`
    /// for #164's per-band-grid-new tier. Rows with no grid on the QSO are
    /// simply absent (`json_extract` on a missing/null field filters them via
    /// the `IS NOT NULL` clause), not an error.
    pub async fn get_worked_bands_and_grids(&self) -> Vec<(String, String)> {
        let result: Result<Vec<(String, String)>, sqlx::Error> = sqlx::query_as(
            "SELECT DISTINCT json_extract(adif_data, '$.band'), \
                             json_extract(metadata, '$.grids.theirs') \
             FROM qsos \
             WHERE json_extract(adif_data, '$.band') IS NOT NULL \
               AND json_extract(metadata, '$.grids.theirs') IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await;

        match result {
            Ok(pairs) => pairs,
            Err(e) => {
                tracing::warn!(
                    "get_worked_bands_and_grids: query failed: {} — treating as empty",
                    e
                );
                Vec::new()
            }
        }
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pancetta-qso get_worked_bands_and_grids -- --nocapture`
Expected: 3 passed

- [ ] **Step 5: Commit**

```bash
git add pancetta-qso/src/async_database.rs
git commit -m "feat(qso): add get_worked_bands_and_grids query for #164 tier 4"
```

---

## Task 5: `CachedStationLookup` per-band grid tracking

**Files:**
- Modify: `pancetta/src/priority_evaluator.rs`
- Test: same file's `#[cfg(test)] mod tests` block

**Interfaces:**
- Consumes: `pancetta_qso::priority::WorkedStationLookup::is_grid_needed_on_band` (Task 3), `pancetta_qso::utils::frequency_to_band` (existing)
- Produces: `CachedStationLookup::seed_worked_grids_from_list(&self, pairs: Vec<(String, String)>)`, `CachedStationLookup::record_worked_grid(&self, grid: &str, band: &str)`, trait impl of `is_grid_needed_on_band`

- [ ] **Step 1: Write the failing test**

Add to the test module, following the exact shape of the existing `dxcc_needed_on_band_*` tests:

```rust
#[test]
fn grid_needed_on_band_true_before_ever_worked() {
    let lookup = CachedStationLookup::new();
    assert!(lookup.is_grid_needed_on_band("PM95", 14_074_000.0));
}

#[test]
fn grid_needed_on_band_false_after_working_that_band() {
    let lookup = CachedStationLookup::new();
    lookup.record_worked_grid("PM95", "20m");
    assert!(!lookup.is_grid_needed_on_band("PM95", 14_074_000.0));
}

#[test]
fn grid_needed_on_band_true_on_a_different_band() {
    let lookup = CachedStationLookup::new();
    lookup.record_worked_grid("PM95", "20m");
    assert!(lookup.is_grid_needed_on_band("PM95", 7_074_000.0));
}

#[test]
fn grid_needed_on_band_case_insensitive_and_4_char_field() {
    let lookup = CachedStationLookup::new();
    lookup.record_worked_grid("pm95xx", "20m"); // 6-char, lowercase
    assert!(!lookup.is_grid_needed_on_band("PM95", 14_074_000.0));
}

#[test]
fn grid_needed_on_band_false_for_too_short_grid() {
    let lookup = CachedStationLookup::new();
    assert!(!lookup.is_grid_needed_on_band("PM", 14_074_000.0));
}

#[test]
fn seed_worked_grids_from_list_matches_record_worked_grid() {
    let seeded = CachedStationLookup::new();
    seeded.seed_worked_grids_from_list(vec![("20m".to_string(), "PM95".to_string())]);

    let recorded = CachedStationLookup::new();
    recorded.record_worked_grid("PM95", "20m");

    for lookup in [&seeded, &recorded] {
        assert!(!lookup.is_grid_needed_on_band("PM95", 14_074_000.0));
        assert!(lookup.is_grid_needed_on_band("PM95", 7_074_000.0));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta --lib priority_evaluator::tests::grid_needed_on_band priority_evaluator::tests::seed_worked_grids -- --nocapture`
Expected: FAIL — `record_worked_grid`/`seed_worked_grids_from_list`/`is_grid_needed_on_band` don't
exist on `CachedStationLookup` yet.

- [ ] **Step 3: Write minimal implementation**

Add a new field to the `CachedStationLookup` struct (after `worked_dxcc_on_band`):

```rust
    /// Grid squares worked per band, local history only (mirrors
    /// `worked_dxcc_on_band` one tier down — #164 tier 4). Key = uppercase
    /// band name, value = set of 4-char uppercase Maidenhead fields.
    worked_grids_on_band: Arc<RwLock<HashMap<String, HashSet<String>>>>,
```

Add it to `Self { ... }` in `CachedStationLookup::new()`:

```rust
            worked_grids_on_band: Arc::new(RwLock::new(HashMap::new())),
```

Add these methods to `impl CachedStationLookup` (after `seed_worked_dxcc_from_list`):

```rust
    /// Seed `worked_grids_on_band` from (band, grid) pairs loaded at startup
    /// (`QsoDatabase::get_worked_bands_and_grids`). Grids shorter than 4
    /// chars are skipped (not a valid Maidenhead field).
    pub fn seed_worked_grids_from_list(&self, pairs: Vec<(String, String)>) {
        let mut map = self.worked_grids_on_band.write();
        let mut inserted = 0usize;
        for (band, grid) in pairs {
            let trimmed = grid.trim();
            if trimmed.len() < 4 {
                continue;
            }
            let field = trimmed[..4].to_uppercase();
            map.entry(band.to_uppercase()).or_default().insert(field);
            inserted += 1;
        }
        tracing::info!(
            "CachedStationLookup: seeded {} worked grid(s) across {} band(s)",
            inserted,
            map.len()
        );
    }
```

Add alongside `record_worked` (after it, in the same `impl` block):

```rust
    /// Record a worked grid square for #164 tier 4, called alongside
    /// `record_worked` on live QSO completion when the DX's grid is known.
    pub fn record_worked_grid(&self, grid: &str, band: &str) {
        let trimmed = grid.trim();
        if trimmed.len() < 4 {
            return;
        }
        let field = trimmed[..4].to_uppercase();
        self.worked_grids_on_band
            .write()
            .entry(band.to_uppercase())
            .or_default()
            .insert(field);
    }
```

Add to `impl WorkedStationLookup for CachedStationLookup` (after `is_dxcc_needed_on_band`):

```rust
    fn is_grid_needed_on_band(&self, grid: &str, freq_hz: f64) -> bool {
        let trimmed = grid.trim();
        if trimmed.len() < 4 {
            return false;
        }
        let field = trimmed[..4].to_uppercase();
        let band = pancetta_qso::utils::frequency_to_band(freq_hz).to_uppercase();
        !self
            .worked_grids_on_band
            .read()
            .get(&band)
            .is_some_and(|s| s.contains(&field))
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pancetta --lib priority_evaluator -- --nocapture`
Expected: all pass, including the 6 new tests.

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/priority_evaluator.rs
git commit -m "feat(coordinator): add per-band grid tracking to CachedStationLookup for #164 tier 4"
```

---

## Task 6: Wire grid tracking into the live coordinator

**Files:**
- Modify: `pancetta/src/coordinator/qso.rs` (two sites: startup seeding ~line 1443, live completion ~line 2022)

**Interfaces:**
- Consumes: `CachedStationLookup::seed_worked_grids_from_list`, `CachedStationLookup::record_worked_grid` (Task 5), `QsoDatabase::get_worked_bands_and_grids` (Task 4)

- [ ] **Step 1: Locate and update the startup-seeding site**

In `pancetta/src/coordinator/qso.rs`, find:

```rust
                        let band_callsign_pairs = db.get_worked_bands_and_callsigns().await;
```

Immediately after that block (which calls `qso_lookup.seed_worked_dxcc_from_list(band_callsign_pairs)`), add:

```rust
                        let band_grid_pairs = db.get_worked_bands_and_grids().await;
                        qso_lookup.seed_worked_grids_from_list(band_grid_pairs);
```

- [ ] **Step 2: Locate and update the live-completion site**

In `pancetta/src/coordinator/qso.rs`, find:

```rust
                                    let band =
                                        pancetta_qso::utils::frequency_to_band(metadata.frequency);
                                    qso_lookup.record_worked(their_call, &band);
```

Change to:

```rust
                                    let band =
                                        pancetta_qso::utils::frequency_to_band(metadata.frequency);
                                    qso_lookup.record_worked(their_call, &band);
                                    if let Some(grid) = metadata.grids.theirs.as_deref() {
                                        qso_lookup.record_worked_grid(grid, &band);
                                    }
```

- [ ] **Step 3: Build to verify no compile errors**

Run: `cargo build -p pancetta`
Expected: clean build.

- [ ] **Step 4: Run the coordinator's qso test suite**

Run: `cargo test -p pancetta --lib coordinator::qso`
Expected: all pass (this is a pure addition alongside existing calls, no existing assertions
should be affected).

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/coordinator/qso.rs
git commit -m "feat(coordinator): seed and record per-band worked grids for #164 tier 4"
```

---

## Task 7: Switch `tui_relay.rs`'s live-decode score to the tiered encoding

**Files:**
- Modify: `pancetta/src/coordinator/tui_relay.rs` (~lines 165-172 comment, ~251-277)
- Test: existing tests in this file (run, don't add new ones — this task changes an internal
  computation the existing tests already exercise via `PriorityScorer` directly)

**Interfaces:**
- Consumes: `PriorityScorer::score_tiered` + `TieredScore::as_display_u32` (Tasks 1 & 3)

- [ ] **Step 1: Update the doc comment above the scorer construction**

Find (around line 165-168):

```rust
            // Build the display priority scorer once per relay thread.
            // Uses the same weights and lookup (via Arc-shared internals)
            // as the autonomous scorer so the DX Hunter's "Pri" column
            // reflects the real continuous score in [0,1] mapped to [0,1000].
```

Replace with:

```rust
            // Build the display priority scorer once per relay thread.
            // Uses the same weights and lookup (via Arc-shared internals)
            // as the autonomous scorer, but the DX Hunter's "Pri" column now
            // reflects the #164 tiered score (0-4999, strict tier dominance
            // — see TieredScore::as_display_u32) rather than the old
            // continuous [0,1] mapped to [0,1000].
```

- [ ] **Step 2: Replace the `priority_score` computation**

Find (around line 251-277):

```rust
                            // Compute nuanced priority score via the real
                            // PriorityScorer (continuous f64 in [0,1] mapped
                            // to [0,1000]). This is the same scorer the
                            // autonomous operator uses for call/no-call
                            // decisions, so the DX Hunter's "Pri" column now
                            // reflects the full weighted signal (rarity,
                            // ATNO, needed-DXCC/grid, SNR, staleness, etc.)
                            // rather than the coarse 0/500/1000 buckets.
                            // Only meaningful for CQ frames that carry a
                            // callsign; non-CQ decodes (RR73/73/reports) get
                            // the same score but it won't influence the DX
                            // Hunter because only CQ frames are listed there.
                            let priority_score = call_sign.as_deref().map(|cs| {
                                use pancetta_qso::DxEvaluator;
                                let freq_hz = dial_mhz * 1_000_000.0;
                                let snr_i8 = decoded_msg.snr_db.round().clamp(-128.0, 127.0) as i8;
                                let score = relay_scorer.evaluate_cq(
                                    cs,
                                    grid_square.as_deref(),
                                    snr_i8,
                                    freq_hz,
                                );
                                // Map [0.0, 1.0] → [0, 1000] for the u32
                                // display field. Values outside [0,1] are
                                // clamped by PriorityScorer before we get here.
                                (score * 1000.0).round() as u32
                            });
```

Replace with:

```rust
                            // Compute the #164 tiered priority score via the
                            // real PriorityScorer's classification (ATNO >
                            // per-band-DXCC-new > special-station >
                            // per-band-grid-new > everything else, encoded
                            // to a single sortable u32 — see
                            // TieredScore::as_display_u32). Only meaningful
                            // for CQ frames that carry a callsign; non-CQ
                            // decodes (RR73/73/reports) get the same score
                            // but it won't influence the DX Hunter because
                            // only CQ frames are listed there.
                            let priority_score = call_sign.as_deref().map(|cs| {
                                let freq_hz = dial_mhz * 1_000_000.0;
                                let snr_i8 = decoded_msg.snr_db.round().clamp(-128.0, 127.0) as i8;
                                relay_scorer
                                    .score_tiered(cs, grid_square.as_deref(), snr_i8, freq_hz)
                                    .as_display_u32()
                            });
```

- [ ] **Step 3: Build and run the existing test suite for this file**

Run: `cargo test -p pancetta --lib coordinator::tui_relay`
Expected: all pass (no existing test asserts on the specific `priority_score` u32 value produced
by this code path, per the earlier read of this file — confirm this holds; if any test does
assert an exact old-scale number, update it to assert tier-relative ordering instead, following
the pattern in Task 3's tests).

- [ ] **Step 4: Commit**

```bash
git add pancetta/src/coordinator/tui_relay.rs
git commit -m "feat(coordinator): switch live-decode DX Hunter score to the #164 tiered encoding"
```

---

## Task 8: `pancetta-tui` dependency + `DxStationLookupAdapter`

**Files:**
- Modify: `pancetta-tui/Cargo.toml`
- Modify: `pancetta-tui/src/app.rs` (add near the top, after imports)
- Test: `pancetta-tui/src/app.rs`'s existing `#[cfg(test)] mod tests` block

**Interfaces:**
- Produces: `struct DxStationLookupAdapter { needed, atno, band_needed, worked_before, rarity_tier, is_notable }` implementing `pancetta_qso::priority::WorkedStationLookup`, `fn rarity_tier_to_f64(tier: Option<&str>) -> f64`

- [ ] **Step 1: Add the dependency**

In `pancetta-tui/Cargo.toml`, under `[dependencies]`, add (alongside the existing
`pancetta-core` line):

```toml
pancetta-qso = { path = "../pancetta-qso" }
```

- [ ] **Step 2: Write the failing test**

Add to `pancetta-tui/src/app.rs`'s test module:

```rust
#[test]
fn rarity_tier_to_f64_maps_known_tiers_and_defaults_to_neutral() {
    assert!((rarity_tier_to_f64(Some("legendary")) - 0.98).abs() < f64::EPSILON);
    assert!((rarity_tier_to_f64(Some("very_rare")) - 0.85).abs() < f64::EPSILON);
    assert!((rarity_tier_to_f64(Some("rare")) - 0.65).abs() < f64::EPSILON);
    assert!((rarity_tier_to_f64(Some("uncommon")) - 0.4).abs() < f64::EPSILON);
    assert!((rarity_tier_to_f64(Some("unknown_tier")) - 0.5).abs() < f64::EPSILON);
    assert!((rarity_tier_to_f64(None) - 0.5).abs() < f64::EPSILON);
}

#[test]
fn dx_station_lookup_adapter_reflects_its_fields_through_the_trait() {
    use pancetta_qso::priority::WorkedStationLookup;
    let adapter = DxStationLookupAdapter {
        needed: true,
        atno: true,
        band_needed: false,
        worked_before: true,
        rarity_tier: Some("rare".to_string()),
        is_notable: true,
    };
    assert!(adapter.is_needed_dxcc("JA1ABC"));
    assert!(adapter.is_atno("JA1ABC"));
    assert!(!adapter.is_dxcc_needed_on_band("JA1ABC", 14_074_000.0));
    assert!(adapter.is_duplicate("JA1ABC", 14_074_000.0));
    assert!(adapter.is_notable("JA1ABC"));
    assert!((adapter.rarity("JA1ABC") - 0.65).abs() < f64::EPSILON);
    assert!(!adapter.is_recent_failure("JA1ABC"));
    assert!(!adapter.is_needed_grid("PM95"));
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p pancetta-tui rarity_tier_to_f64 dx_station_lookup_adapter -- --nocapture`
Expected: FAIL — `pancetta_qso` crate not yet in scope / `DxStationLookupAdapter`/`rarity_tier_to_f64` don't exist.

- [ ] **Step 4: Write minimal implementation**

Add near the top of `pancetta-tui/src/app.rs`, after the existing `use` block (before
`DecodedMessageView`):

```rust
/// Adapts a `DxStation`'s already-known fields into a `WorkedStationLookup`
/// so the TUI can score local-decode-fallback and network-only rows through
/// the SAME tiered `PriorityScorer` the coordinator's `tui_relay` uses for
/// live decodes (#164 unification). The TUI has no access to the
/// coordinator's `CachedStationLookup` (recent-failure history, per-band
/// worked-grid history), so those two signals are always `false` here —
/// an existing limitation carried forward from the coarse function this
/// replaces, not a regression.
struct DxStationLookupAdapter {
    needed: bool,
    atno: bool,
    band_needed: bool,
    worked_before: bool,
    rarity_tier: Option<String>,
    is_notable: bool,
}

impl pancetta_qso::priority::WorkedStationLookup for DxStationLookupAdapter {
    fn is_duplicate(&self, _callsign: &str, _freq_hz: f64) -> bool {
        self.worked_before
    }
    fn is_recent_failure(&self, _callsign: &str) -> bool {
        false
    }
    fn is_needed_dxcc(&self, _callsign: &str) -> bool {
        self.needed
    }
    fn is_atno(&self, _callsign: &str) -> bool {
        self.atno
    }
    fn is_dxcc_needed_on_band(&self, _callsign: &str, _freq_hz: f64) -> bool {
        self.band_needed
    }
    fn is_needed_grid(&self, _grid: &str) -> bool {
        false
    }
    fn rarity(&self, _callsign: &str) -> f64 {
        rarity_tier_to_f64(self.rarity_tier.as_deref())
    }
    fn is_notable(&self, _callsign: &str) -> bool {
        self.is_notable
    }
}

/// Map cqdx's string rarity tier to the `[0,1]` numeric scale
/// `WorkedStationLookup::rarity` expects. `None`/unrecognized -> neutral
/// 0.5, matching the trait's own default.
fn rarity_tier_to_f64(tier: Option<&str>) -> f64 {
    match tier {
        Some("legendary") => 0.98,
        Some("very_rare") => 0.85,
        Some("rare") => 0.65,
        Some("uncommon") => 0.4,
        _ => 0.5,
    }
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p pancetta-tui rarity_tier_to_f64 dx_station_lookup_adapter -- --nocapture`
Expected: 2 passed

- [ ] **Step 6: Commit**

```bash
git add pancetta-tui/Cargo.toml pancetta-tui/src/app.rs
git commit -m "feat(tui): add pancetta-qso dependency and DxStationLookupAdapter for #164"
```

---

## Task 9: Rewire `calculate_dx_priority` (local-decode fallback)

**Files:**
- Modify: `pancetta-tui/src/app.rs` (~line 1646)
- Test: same file's test module

**Interfaces:**
- Consumes: `DxStationLookupAdapter`, `rarity_tier_to_f64` (Task 8), `pancetta_qso::priority::{PriorityScorer, PriorityWeights}` (existing)

- [ ] **Step 1: Write the failing test**

Add to the test module (this exercises the fallback branch — `message.priority_score: None`):

```rust
#[tokio::test]
async fn calculate_dx_priority_fallback_uses_tiered_scorer_when_no_precomputed_score() {
    let app = App::new(Config::default(), None).await.unwrap();
    let message = DecodedMessageView {
        timestamp: chrono::Utc::now(),
        frequency: 14.074,
        mode: "FT8".to_string(),
        snr: -10,
        delta_time: 0.0,
        delta_freq: 0.0,
        call_sign: Some("JA1ABC".to_string()),
        grid_square: Some("PM95".to_string()),
        message: "CQ JA1ABC PM95".to_string(),
        distance: None,
        bearing: None,
        slot_parity: None,
        is_directed_at_us: false,
        worked_before: false,
        needed: true,
        atno: true,
        band_needed: false,
        priority_score: None,
    };
    let score = app.calculate_dx_priority(&message);
    // ATNO + needed -> PriorityTier::Atno -> display range 4000-4999.
    assert!(score >= 4000, "expected ATNO tier range, got {score}");
}
```

(`App::new` is async and returns `Result`, matching every other test in this file's construction
pattern — e.g. the tests around line 3927 of `app.rs`.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta-tui calculate_dx_priority_fallback_uses_tiered_scorer -- --nocapture`
Expected: FAIL (old coarse-function behavior — verify the assertion's threshold doesn't
accidentally already pass under the OLD code before proceeding; if it does, tighten the assertion,
e.g. also assert `score < 5000`, to make the change observable).

- [ ] **Step 3: Write minimal implementation**

Replace the existing `calculate_dx_priority` method body:

```rust
    fn calculate_dx_priority(&self, message: &DecodedMessageView) -> u32 {
        // If the coordinator pre-computed a nuanced score from the real
        // PriorityScorer (#164 tiered encoding), use it directly.
        if let Some(pre_computed) = message.priority_score {
            return pre_computed;
        }
        // Fallback: no precomputed score (legacy/test paths). Same
        // reduced-signal adapter as merge_spot_groups's network-spot path.
        let adapter = DxStationLookupAdapter {
            needed: message.needed,
            atno: message.atno,
            band_needed: message.band_needed,
            worked_before: message.worked_before,
            rarity_tier: None,
            is_notable: false,
        };
        let scorer = pancetta_qso::priority::PriorityScorer::new(
            pancetta_qso::priority::PriorityWeights::default(),
            Box::new(adapter),
        );
        let freq_hz = message.frequency * 1_000_000.0;
        scorer
            .score_tiered(
                message.call_sign.as_deref().unwrap_or(""),
                message.grid_square.as_deref(),
                message.snr.clamp(-128, 127) as i8,
                freq_hz,
            )
            .as_display_u32()
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pancetta-tui calculate_dx_priority_fallback_uses_tiered_scorer -- --nocapture`
Expected: 1 passed

- [ ] **Step 5: Commit**

```bash
git add pancetta-tui/src/app.rs
git commit -m "feat(tui): rewire calculate_dx_priority fallback through the #164 tiered scorer"
```

---

## Task 10: Rewire `merge_spot_groups`'s network-spot scoring

**Files:**
- Modify: `pancetta-tui/src/app.rs` (~lines 3215-3307)
- Test: same file's test module (find the existing `merge_spot_groups` test(s) — grep
  `fn.*merge_spot_groups` in the test module — and extend/add alongside them)

**Interfaces:**
- Consumes: `DxStationLookupAdapter`, `rarity_tier_to_f64` (Task 8)

- [ ] **Step 1: Write the failing test**

Add to the test module (`CqdxSpotInfo`'s full field list, confirmed from
`pancetta-tui/src/tui_runner.rs`):

```rust
#[tokio::test]
async fn merge_spot_groups_scores_atno_spot_into_the_atno_display_range() {
    let mut app = App::new(Config::default(), None).await.unwrap();
    let spot = crate::tui_runner::CqdxSpotInfo {
        dx_call: "JA1ABC".to_string(),
        band: "20m".to_string(),
        mode: "FT8".to_string(),
        frequency_hz: 14_074_000,
        grid: Some("PM95".to_string()),
        rarity_tier: "rare".to_string(),
        reporter_count: 3,
        best_snr: Some(-10),
        confidence: 0.9,
        first_seen: chrono::Utc::now().timestamp(),
        last_seen: chrono::Utc::now().timestamp(),
        is_notable: false,
        notable_type: None,
        entity_name: "Japan".to_string(),
        needed: true,
        atno: true,
    };
    app.merge_spot_groups(&[spot]);
    let entry = app.dx_stations.get("JA1ABC").expect("spot merged");
    assert!(
        entry.priority_score >= 4000,
        "expected ATNO tier range, got {}",
        entry.priority_score
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta-tui merge_spot_groups_scores_atno_spot -- --nocapture`
Expected: FAIL under the old coarse function if its ATNO bucket (1000 + rarity/distance/SNR)
never reaches 4000 — confirm this is actually a meaningful pre/post distinction before proceeding
(it is: old max is ~1000+400+50+34 ≈ 1484, well under 4000).

- [ ] **Step 3: Write minimal implementation**

In `merge_spot_groups`, delete the now-unused `our_grid` variable:

```rust
        let our_grid = self.station_info.grid_square.clone();
```

(remove this line — it was only ever passed to the function being replaced below).

Replace:

```rust
            let net_score = crate::ui::dx_hunter::calculate_dx_priority(
                entry,
                &our_grid,
                entry.worked_before,
                false,
                false,
            );
            entry.priority_score = entry.priority_score.max(net_score);
```

with:

```rust
            let adapter = DxStationLookupAdapter {
                needed: entry.needed,
                atno: entry.atno,
                band_needed: entry.band_needed,
                worked_before: entry.worked_before,
                rarity_tier: entry.rarity_tier.clone(),
                is_notable: entry.is_notable,
            };
            let scorer = pancetta_qso::priority::PriorityScorer::new(
                pancetta_qso::priority::PriorityWeights::default(),
                Box::new(adapter),
            );
            let freq_hz = entry.frequency * 1_000_000.0;
            let net_score = scorer
                .score_tiered(
                    &entry.call_sign,
                    entry.grid_square.as_deref(),
                    entry.snr.clamp(-128, 127) as i8,
                    freq_hz,
                )
                .as_display_u32();
            entry.priority_score = entry.priority_score.max(net_score);
```

Also update the function's doc comment (directly above `pub fn merge_spot_groups`) to reflect the
new scorer:

```rust
    /// Merge live spot groups from cqdx.io into the DX station list.
    ///
    /// Network spots used to land with `priority_score: 0`, so they always
    /// sorted to the bottom of the DX Hunter list regardless of how rare
    /// they were. We now run the same #164 tiered `PriorityScorer` the
    /// live-decode path uses (via `DxStationLookupAdapter`), so a
    /// needed/ATNO/notable cluster spot ranks in the tier it actually
    /// belongs to, not just a coarse bucket.
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pancetta-tui merge_spot_groups -- --nocapture`
Expected: all `merge_spot_groups` tests (existing + new) pass.

- [ ] **Step 5: Commit**

```bash
git add pancetta-tui/src/app.rs
git commit -m "feat(tui): rewire merge_spot_groups network-spot scoring through the #164 tiered scorer"
```

---

## Task 11: Delete the coarse scorer from `dx_hunter.rs`, retune display thresholds

**Files:**
- Modify: `pancetta-tui/src/ui/dx_hunter.rs`

**Interfaces:**
- Consumes: nothing new (pure deletion + a threshold retune)

- [ ] **Step 1: Delete `dx_priority_score`, `calculate_dx_priority`, and their tests**

Delete these functions entirely from `pancetta-tui/src/ui/dx_hunter.rs`:
- `pub fn dx_priority_score(...)` (with its doc comment block above it)
- `pub fn calculate_dx_priority(...)`

Delete these tests from the `#[cfg(test)] mod tests` block:
- `test_calculate_dx_priority`
- `dx_priority_hierarchy_atno_over_needed_over_rare_over_plain`
- `snr_full_range_produces_distinct_scores`

(Keep `is_rare_dx_fallback`, `is_rare_dx_from_tier`, `is_new_dxcc`, `extract_dxcc_prefix`,
`extract_base_prefix`, `need_marker`, `format_dx_snr` and their tests — those are styling/coloring
helpers, unrelated to the deleted scoring functions.)

- [ ] **Step 2: Retune `priority_style` thresholds**

In `create_dx_row`, find:

```rust
    let priority_style = match station.priority_score {
        score if score > 100 => Style::default()
            .fg(app.theme.error_color())
            .add_modifier(Modifier::BOLD),
        score if score > 50 => Style::default()
            .fg(app.theme.warning_color())
            .add_modifier(Modifier::BOLD),
        _ => dim,
    };
```

Replace with (thresholds now aligned to the #164 tier boundaries — `>= 2000` is
SpecialStation-or-above, `>= 1000` is PerBandGridNew-or-above):

```rust
    let priority_style = match station.priority_score {
        score if score >= 2000 => Style::default()
            .fg(app.theme.error_color())
            .add_modifier(Modifier::BOLD),
        score if score >= 1000 => Style::default()
            .fg(app.theme.warning_color())
            .add_modifier(Modifier::BOLD),
        _ => dim,
    };
```

- [ ] **Step 3: Build to verify no dangling references**

Run: `cargo build -p pancetta-tui`
Expected: clean build — if this fails with "function not found" anywhere else, that call site was
missed in Tasks 9-10; go back and fix it before proceeding.

- [ ] **Step 4: Run the full `pancetta-tui` test suite**

Run: `cargo test -p pancetta-tui`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add pancetta-tui/src/ui/dx_hunter.rs
git commit -m "refactor(tui): delete the coarse dx_priority_score scorer, retune priority_style for #164"
```

---

## Task 12: Full workspace verification, docs, PR

**Files:**
- Modify: `docs/DECISIONS/priority-scoring.md` (append)
- Modify: `docs/ARCHITECTURE.md` (note new dependency edge)

- [ ] **Step 1: Run the full workspace test suite**

Run: `cargo test --workspace --features transmit`
Expected: all pass. If anything fails, treat it as this repo's established discipline requires —
root-cause it (per `superpowers:systematic-debugging`) rather than patching around it, the same
way the #163 fix caught and fixed a real regression via this exact step.

- [ ] **Step 2: Format for real and verify clean**

Run: `cargo fmt`
Run: `cargo fmt --check`
Expected: the second command produces no output/diff. If the first command changed anything,
re-run `cargo fmt --check` again to confirm — do not trust an unverified first pass.

- [ ] **Step 3: Clippy**

Run: `cargo clippy --workspace --features transmit -- -D warnings`
Expected: clean. Fix anything flagged before proceeding.

- [ ] **Step 4: Append to `docs/DECISIONS/priority-scoring.md`**

Add a new section at the end of the file (after the existing #163 section):

```markdown

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
(`TieredScore::as_display_u32`): `tier_rank * 1000 + secondary*999`, giving five clean 1000-wide
bands (Standard 0-999 through Atno 4000-4999) that can never bleed into each other.

Full design: `docs/superpowers/specs/2026-07-19-dx-hunter-priority-tiers-and-history-panel-design.md`.
```

- [ ] **Step 5: Update `docs/ARCHITECTURE.md`**

Find the section describing crate dependencies (search for where `pancetta-tui`'s dependencies
are listed or diagrammed) and add a note that `pancetta-tui` now depends on `pancetta-qso`
(read-only use of `PriorityScorer`/`WorkedStationLookup` for DX Hunter scoring — #164), alongside
its existing `pancetta-core` dependency.

- [ ] **Step 6: Commit docs**

```bash
git add docs/DECISIONS/priority-scoring.md docs/ARCHITECTURE.md
git commit -m "docs: record the #164 5-tier priority scoring redesign"
```

- [ ] **Step 7: Push and open the PR**

```bash
git push -u origin <branch-name>
gh pr create --title "feat: 5-tier DX Hunter priority scoring (#164)" --body "$(cat <<'EOF'
## Summary
- Replaces the flat weighted-sum priority formula with a strict 5-tier lexicographic scheme
  (ATNO > per-band-DXCC-new > special-station > per-band-grid-new > everything-else).
- Unifies the two previously-divergent DX Hunter scorers (real PriorityScorer for live decodes,
  coarse dx_priority_score for network spots) into one — pancetta-tui gains a pancetta-qso
  dependency.
- Adds tier-3 special-station detection (US 1x1 format, UK GB convention, curated international
  list) and tier-4 per-band grid-new tracking (mirrors the #163 per-band-DXCC fix).

## Test plan
- [ ] `cargo test --workspace --features transmit` green
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy --workspace --features transmit -- -D warnings` clean
- [ ] On-air: confirm the DX Hunter "Pri" column now spreads across the full 0-4999 range instead
  of clustering, and that ATNO/needed/special/grid-new rows visibly rank above plain rarity
  variation

Closes #164

🤖 Generated with [Claude Code](https://claude.com/claude-code)

https://claude.ai/code/session_01VqMW4dvjiSpBJCoprJZsug
EOF
)"
```

- [ ] **Step 8: Verify**

Run: `gh pr view --json number,url,mergeable`
Expected: PR created, `mergeable: MERGEABLE` (or `UNKNOWN` pending CI — recheck shortly after).
