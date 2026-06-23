# CQDX Integration Improvements — Design Spec

## Goal

Surface the rich metadata from cqdx.io live spots throughout the application: merged DX Hunter view, band-filtered polling, spot-aware frequency nudging, notable station alerts, activity window filtering, SNR-aware scoring, and dynamic rarity lookups replacing hardcoded prefix lists.

## Current State

- `CqdxCache` holds `Vec<SpotGroup>` with rich metadata (rarity, notable flags, reporter count, SNR, activity window) but only rarity scores are consumed
- `spot_groups()` accessor exists but is never called outside the cache
- DX Hunter displays only locally-decoded stations
- Spot poller always fetches all bands/modes (`None, None`)
- `is_rare_dx()` in dx_hunter.rs uses a hardcoded 15-prefix list
- Autonomous operator scores by rarity but ignores spot confidence, SNR, and staleness

## Feature 1: Merged DX Hunter with Live Spots

### DxStation changes (`pancetta-tui/src/app.rs`)

Add fields to `DxStation`:
```rust
pub source: SpotSource,          // RX (local decode) or NET (cqdx.io)
pub rarity_tier: Option<String>, // "legendary", "very_rare", "rare", "uncommon", "common"
pub reporter_count: Option<u32>, // network reporter count
pub is_notable: bool,            // notable flag from cqdx.io
pub notable_type: Option<String>,// e.g., "ATNO", "birthday"
pub confidence: Option<f64>,     // spot confidence score
pub best_snr_network: Option<i32>, // best SNR from network reporters

#[derive(Debug, Clone, PartialEq)]
pub enum SpotSource {
    Local,   // decoded by our receiver
    Network, // from cqdx.io live spots
    Both,    // seen locally AND in network
}
```

### Merge logic (`App::merge_spot_groups`)

New method called by coordinator when spot poller updates:
```rust
pub fn merge_spot_groups(&mut self, spots: &[SpotGroup]) {
    for spot in spots {
        let entry = self.dx_stations.entry(spot.dx_call.clone()).or_insert_with(|| DxStation::new_from_spot(spot));
        // If already exists from local decode, upgrade source to Both
        if entry.source == SpotSource::Local {
            entry.source = SpotSource::Both;
        }
        // Always update network metadata
        entry.rarity_tier = spot.rarity_tier.clone().into();
        entry.reporter_count = Some(spot.reporter_count);
        entry.is_notable = spot.is_notable;
        entry.notable_type = spot.notable_type.clone();
        entry.confidence = Some(spot.confidence);
        entry.best_snr_network = spot.best_snr;
    }
}
```

### DX Hunter rendering (`pancetta-tui/src/ui/dx_hunter.rs`)

Table columns: `Src | Call | Grid | Freq | SNR | Dist | Rarity | Rpt | Last | Pri`

- `Src`: "RX" (green), "NET" (blue), "RX+NET" (cyan)
- `Rarity`: from `rarity_tier` (colored: legendary=magenta, very_rare=red, rare=yellow)
- `Rpt`: reporter_count from network
- Notable stations: magenta bold with notable_type prefix (e.g., "★ ATNO 3Y/B")

## Feature 2: Band/Mode Filtering

### Band derivation (`pancetta-cqdx/src/cache.rs`)

Add utility function:
```rust
pub fn frequency_to_band(freq_hz: u64) -> Option<String> {
    match freq_hz / 1_000_000 {
        1..=2 => Some("160m"),
        3..=4 => Some("80m"),
        5..=6 => Some("60m"),
        7..=8 => Some("40m"),
        10..=11 => Some("30m"),
        14..=15 => Some("20m"),
        18..=19 => Some("17m"),
        21..=22 => Some("15m"),
        24..=25 => Some("12m"),
        28..=30 => Some("10m"),
        50..=54 => Some("6m"),
        _ => None,
    }
}
```

### Coordinator wiring (`pancetta/src/coordinator/components.rs`)

When starting the spot poller, derive band from `radio_frequency`:
```rust
let band = radio_frequency
    .map(|f| (f * 1_000_000.0) as u64)
    .and_then(frequency_to_band);
bridge.spawn_spot_poller(shutdown, last_decode, band, Some("FT8".to_string()));
```

When band changes (hamlib frequency update), restart or update the poller's band filter. Simplest approach: store current band in shared state, poller reads it each poll cycle.

## Feature 3: Spot-Aware Frequency Nudging

### Changes to `AutonomousOperator::allocate_smart_frequency` (`pancetta-qso/src/autonomous.rs`)

Add an optional `spot_groups: &[SpotGroup]` parameter. When calling CQ (target_freq = None):

1. Score candidate frequencies as before (center preference, avoid own QSOs, avoid busy)
2. Add bonus for proximity to high-rarity spots: if a rare station is calling CQ on frequency F, calling CQ within ±200 Hz increases chance of being heard by stations tuned to that area
3. Bonus: `+0.2 * rarity` for candidates within 200 Hz of a spot with `rarity_tier` in ["legendary", "very_rare", "rare"]

This is a soft preference, not a hard constraint. The existing separation and busy-area penalties still dominate.

### Data flow

Coordinator passes `cache.spot_groups()` to autonomous operator each slot cycle. The autonomous operator stores a reference and uses it in `allocate_smart_frequency()`.

## Feature 4: Notable Station Alerts

### Priority evaluator bonus (`pancetta/src/priority_evaluator.rs`)

In `evaluate_cq()`, if the callsign matches a notable spot, add +0.3 to the dx_score. The evaluator needs access to notable status — add it to `CachedStationLookup`:

```rust
pub fn is_notable(&self, callsign: &str) -> bool {
    self.notable_callsigns.read().unwrap().contains(&callsign.to_uppercase())
}
```

Updated by the spot poller alongside rarity scores.

### TUI rendering

Notable stations in DX Hunter get magenta bold styling with a star prefix. If `notable_type` is present, show it: `★ATNO 3Y/B` or `★ VK0AI`.

## Feature 5: Activity Window Filtering

### Staleness scoring (`pancetta/src/priority_evaluator.rs`)

In `evaluate_cq()`, apply staleness multiplier based on spot age:

```rust
let staleness = if let Some(last_seen) = network_last_seen {
    let age_secs = (now - last_seen).max(0);
    match age_secs {
        0..=300 => 1.0,      // <5 min: fresh
        301..=600 => 0.7,    // 5-10 min: aging
        601..=900 => 0.4,    // 10-15 min: stale
        _ => 0.2,            // >15 min: very stale
    }
} else {
    1.0 // no network data = no penalty
};
score *= staleness;
```

Locally decoded stations always use their own decode timestamp, not network timestamps.

### DX Hunter visual

Stale spots (>10 min since last_seen, not locally decoded) render in dim/gray text.

## Feature 6: SNR-Aware Target Selection

### Priority evaluator (`pancetta/src/priority_evaluator.rs`)

Add network SNR data to scoring:

```rust
// Well-confirmed, workable station
if reporter_count >= 5 && best_snr >= -20 {
    score += 0.1; // likely to be heard
}
// Weak, single-reporter spot — uncertain
if reporter_count == 1 && best_snr < -25 {
    score -= 0.1; // might not be workable
}
```

This biases toward stations with multiple confirming reports at reasonable signal levels.

### Data flow

`CachedStationLookup` gets `network_snr: HashMap<String, (u32, i32)>` mapping callsign → (reporter_count, best_snr). Updated by spot poller.

## Feature 7: Replace Hardcoded Rare DX List

### Current (`dx_hunter.rs::is_rare_dx`)

Hardcoded ~15 prefixes: 3Y, VP8, VK0, ZS8, etc.

### Replacement

Replace with cache lookup:
```rust
fn is_rare_dx(station: &DxStation) -> bool {
    match station.rarity_tier.as_deref() {
        Some("legendary") | Some("very_rare") => true,
        _ => false,
    }
}
```

When CQDX is disabled (no rarity_tier data), fall back to the existing hardcoded list. This preserves functionality without the network dependency.

### Priority scoring update

In `calculate_dx_priority()`, replace the fixed +150 for rare DX with a rarity-tier-based bonus:
- legendary: +200
- very_rare: +150
- rare: +100
- uncommon: +50
- common: +0

## File Map

| File | Changes |
|------|---------|
| `pancetta-tui/src/app.rs` | `SpotSource` enum, new `DxStation` fields, `merge_spot_groups()` |
| `pancetta-tui/src/ui/dx_hunter.rs` | Src column, notable styling, rarity tiers, staleness dim, replace `is_rare_dx()` |
| `pancetta/src/coordinator/components.rs` | Feed spot groups to TUI, pass band to poller, feed spots to autonomous |
| `pancetta/src/priority_evaluator.rs` | Notable bonus, staleness multiplier, SNR bonus, network metadata storage |
| `pancetta/src/cqdx_bridge.rs` | Expose `spot_groups()` for coordinator, update notable/SNR metadata |
| `pancetta-qso/src/autonomous.rs` | Accept spot groups for frequency nudging, scoring bonus near rare spots |
| `pancetta-cqdx/src/cache.rs` | `frequency_to_band()` utility |

## Success Criteria

- DX Hunter shows both local and network spots with source indicator
- Spot poller filters by current band
- Notable stations are visually distinct and get scoring bonus
- Stale spots fade visually and score lower
- Autonomous operator prefers frequencies near rare DX
- No regression in decode sensitivity or speed
- Graceful degradation when CQDX is disabled
