# CQDX Integration Improvements — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Surface rich cqdx.io spot metadata throughout the app: merged DX Hunter, band-filtered polling, spot-aware frequency nudging, notable alerts, staleness filtering, SNR scoring, and dynamic rarity lookups.

**Architecture:** Most changes are wiring — the data already exists in `CqdxCache`. The plan adds new fields to `DxStation`, extends `CachedStationLookup` with network metadata, adds a `TuiMessage::SpotGroupUpdate` variant, and threads spot data into the autonomous operator's frequency allocator.

**Tech Stack:** Rust, ratatui, tokio, pancetta-cqdx, pancetta-qso

**Spec:** `docs/superpowers/specs/2026-04-19-cqdx-integration-design.md`

---

## Task 1: Add SpotSource enum and new DxStation fields

**Files:**
- Modify: `pancetta-tui/src/app.rs:56-68`

- [ ] **Step 1: Add SpotSource enum**

In `pancetta-tui/src/app.rs`, add before the `DxStation` struct:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum SpotSource {
    /// Decoded by our receiver
    Local,
    /// From cqdx.io live spots
    Network,
    /// Seen locally AND in network
    Both,
}

impl std::fmt::Display for SpotSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpotSource::Local => write!(f, "RX"),
            SpotSource::Network => write!(f, "NET"),
            SpotSource::Both => write!(f, "RX+N"),
        }
    }
}
```

- [ ] **Step 2: Add new fields to DxStation**

Extend the `DxStation` struct (line 57):

```rust
#[derive(Debug, Clone)]
pub struct DxStation {
    pub call_sign: String,
    pub grid_square: Option<String>,
    pub frequency: f64,
    pub mode: String,
    pub last_seen: DateTime<Utc>,
    pub snr: i32,
    pub distance: Option<f64>,
    pub bearing: Option<f64>,
    pub worked_before: bool,
    pub priority_score: u32,
    // CQDX network metadata
    pub source: SpotSource,
    pub rarity_tier: Option<String>,
    pub reporter_count: Option<u32>,
    pub is_notable: bool,
    pub notable_type: Option<String>,
    pub confidence: Option<f64>,
    pub best_snr_network: Option<i32>,
    pub last_seen_network: Option<i64>,
}
```

- [ ] **Step 3: Fix all DxStation construction sites**

Find every place that constructs a `DxStation` and add the new fields with defaults:

```rust
source: SpotSource::Local,
rarity_tier: None,
reporter_count: None,
is_notable: false,
notable_type: None,
confidence: None,
best_snr_network: None,
last_seen_network: None,
```

Search for `DxStation {` in `app.rs` to find all construction sites. There should be one in `add_decoded_message()`.

- [ ] **Step 4: Build and verify**

```bash
touch pancetta-tui/src/app.rs
cargo build -p pancetta-tui 2>&1 | tail -5
```

Expected: clean build.

- [ ] **Step 5: Commit**

```bash
git add pancetta-tui/src/app.rs
git commit -m "feat: add SpotSource enum and network metadata fields to DxStation"
```

---

## Task 2: Add merge_spot_groups to App

**Files:**
- Modify: `pancetta-tui/src/app.rs`
- Modify: `pancetta-tui/src/tui_runner.rs:57-78` (TuiMessage enum)

- [ ] **Step 1: Add TuiMessage::SpotGroupUpdate variant**

In `pancetta-tui/src/tui_runner.rs`, add to the `TuiMessage` enum (line 57):

```rust
/// Live spot groups from cqdx.io
SpotGroupUpdate {
    spots: Vec<CqdxSpotInfo>,
},
```

And define the transfer struct (doesn't depend on pancetta-cqdx types):

```rust
/// Lightweight spot info for TUI display (avoids pancetta-cqdx dependency)
#[derive(Debug, Clone)]
pub struct CqdxSpotInfo {
    pub dx_call: String,
    pub band: String,
    pub mode: String,
    pub frequency_hz: u64,
    pub grid: Option<String>,
    pub rarity_tier: String,
    pub reporter_count: u32,
    pub best_snr: Option<i32>,
    pub confidence: f64,
    pub first_seen: i64,
    pub last_seen: i64,
    pub is_notable: bool,
    pub notable_type: Option<String>,
    pub entity_name: String,
}
```

- [ ] **Step 2: Add merge_spot_groups method to App**

In `pancetta-tui/src/app.rs`, add a public method:

```rust
/// Merge live spot groups from cqdx.io into the DX station list.
/// Network-only stations are added; locally-decoded stations get
/// their source upgraded to Both and network metadata populated.
pub fn merge_spot_groups(&mut self, spots: &[crate::tui_runner::CqdxSpotInfo]) {
    for spot in spots {
        let entry = self
            .dx_stations
            .entry(spot.dx_call.clone())
            .or_insert_with(|| DxStation {
                call_sign: spot.dx_call.clone(),
                grid_square: spot.grid.clone(),
                frequency: spot.frequency_hz as f64 / 1_000_000.0, // Hz to MHz
                mode: spot.mode.clone(),
                last_seen: chrono::Utc::now(),
                snr: spot.best_snr.unwrap_or(0),
                distance: None,
                bearing: None,
                worked_before: false,
                priority_score: 0,
                source: SpotSource::Network,
                rarity_tier: Some(spot.rarity_tier.clone()),
                reporter_count: Some(spot.reporter_count),
                is_notable: spot.is_notable,
                notable_type: spot.notable_type.clone(),
                confidence: Some(spot.confidence),
                best_snr_network: spot.best_snr,
                last_seen_network: Some(spot.last_seen),
            });

        // If already exists from local decode, upgrade source
        if entry.source == SpotSource::Local {
            entry.source = SpotSource::Both;
        }
        // Always update network metadata
        entry.rarity_tier = Some(spot.rarity_tier.clone());
        entry.reporter_count = Some(spot.reporter_count);
        entry.is_notable = spot.is_notable;
        entry.notable_type = spot.notable_type.clone();
        entry.confidence = Some(spot.confidence);
        entry.best_snr_network = spot.best_snr;
        entry.last_seen_network = Some(spot.last_seen);
    }
}
```

- [ ] **Step 3: Handle SpotGroupUpdate in TUI runner message loop**

Find where `TuiMessage` variants are matched in `tui_runner.rs` and add:

```rust
TuiMessage::SpotGroupUpdate { spots } => {
    self.app.merge_spot_groups(&spots);
}
```

- [ ] **Step 4: Build and verify**

```bash
touch pancetta-tui/src/app.rs pancetta-tui/src/tui_runner.rs
cargo build -p pancetta-tui 2>&1 | tail -5
```

- [ ] **Step 5: Commit**

```bash
git add pancetta-tui/src/app.rs pancetta-tui/src/tui_runner.rs
git commit -m "feat: merge cqdx.io spot groups into TUI DxStation list"
```

---

## Task 3: Update DX Hunter rendering with Src column and notable styling

**Files:**
- Modify: `pancetta-tui/src/ui/dx_hunter.rs`

- [ ] **Step 1: Add Src column to header**

Change the header array (line 18) from:

```rust
["Call", "Grid", "Freq", "SNR", "Dist", "Bear", "Last", "Pri"]
```

to:

```rust
["Src", "Call", "Grid", "Freq", "SNR", "Rarity", "Rpt", "Last", "Pri"]
```

Update the column constraints to accommodate the new columns. Remove Bear (least useful) to keep width manageable.

- [ ] **Step 2: Update create_dx_row with new columns and notable styling**

Replace the `create_dx_row` function:

```rust
fn create_dx_row<'a>(station: &'a DxStation, app: &App) -> Row<'a> {
    // Source indicator
    let src_str = station.source.to_string();
    let src_style = match station.source {
        SpotSource::Local => Style::default().fg(app.theme.success_color()),
        SpotSource::Network => Style::default().fg(app.theme.accent_color()),
        SpotSource::Both => Style::default().fg(ratatui::style::Color::Cyan),
    };

    // Callsign with notable prefix
    let call_display = if station.is_notable {
        format!("★{}", station.call_sign)
    } else {
        station.call_sign.clone()
    };

    // Staleness check for network-only spots
    let is_stale = if station.source != SpotSource::Local {
        station.last_seen_network
            .map(|ts| {
                let age = chrono::Utc::now().timestamp() - ts;
                age > 600 // >10 minutes
            })
            .unwrap_or(false)
    } else {
        false
    };

    let call_style = if is_stale {
        Style::default().fg(app.theme.muted_color())
    } else if station.is_notable {
        Style::default()
            .fg(ratatui::style::Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else if station.worked_before {
        Style::default().fg(app.theme.muted_color())
    } else if is_rare_dx_from_tier(station) {
        Style::default()
            .fg(app.theme.error_color())
            .add_modifier(Modifier::BOLD)
    } else if station.distance.unwrap_or(0.0) > 5000.0 {
        Style::default()
            .fg(app.theme.warning_color())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(app.theme.success_color())
            .add_modifier(Modifier::BOLD)
    };

    let grid_str = station.grid_square.as_deref().unwrap_or("---");
    let freq_str = format!("{:.3}", station.frequency);
    let snr_str = format!("{:+}", station.snr);
    let rarity_str = station.rarity_tier.as_deref().unwrap_or("-").to_string();
    let rpt_str = station.reporter_count.map(|r| r.to_string()).unwrap_or_default();
    let last_str = format_time_ago(station.last_seen);
    let pri_str = format!("{}", station.priority_score);

    let dim = if is_stale {
        Style::default().fg(app.theme.muted_color())
    } else {
        Style::default().fg(app.theme.foreground_color())
    };

    let rarity_style = match station.rarity_tier.as_deref() {
        Some("legendary") => Style::default().fg(ratatui::style::Color::Magenta).add_modifier(Modifier::BOLD),
        Some("very_rare") => Style::default().fg(app.theme.error_color()).add_modifier(Modifier::BOLD),
        Some("rare") => Style::default().fg(app.theme.warning_color()),
        _ => dim,
    };

    let snr_style = Style::default().fg(get_snr_color(station.snr, &app.theme));

    let priority_style = match station.priority_score {
        score if score > 100 => Style::default()
            .fg(app.theme.error_color())
            .add_modifier(Modifier::BOLD),
        score if score > 50 => Style::default()
            .fg(app.theme.warning_color())
            .add_modifier(Modifier::BOLD),
        _ => dim,
    };

    Row::new([
        Cell::from(src_str).style(src_style),
        Cell::from(call_display).style(call_style),
        Cell::from(grid_str.to_string()).style(dim),
        Cell::from(freq_str).style(Style::default().fg(app.theme.accent_color())),
        Cell::from(snr_str).style(snr_style),
        Cell::from(rarity_str).style(rarity_style),
        Cell::from(rpt_str).style(dim),
        Cell::from(last_str).style(dim),
        Cell::from(pri_str).style(priority_style),
    ])
}
```

- [ ] **Step 3: Replace is_rare_dx with rarity-tier lookup**

Replace the `is_rare_dx` function (line 171) with:

```rust
/// Check if a station is rare DX using cqdx.io rarity tier (preferred)
/// or fallback hardcoded prefix list.
fn is_rare_dx_from_tier(station: &DxStation) -> bool {
    match station.rarity_tier.as_deref() {
        Some("legendary") | Some("very_rare") => true,
        Some(_) => false,
        // No network data — fall back to hardcoded list
        None => is_rare_dx_fallback(&station.call_sign),
    }
}

/// Fallback rare DX check when cqdx.io data is unavailable.
fn is_rare_dx_fallback(call_sign: &str) -> bool {
    let rare_prefixes = [
        "1A", "3Y", "4U", "7O", "8Q", "9Q", "BS7", "BV9", "BY9", "CY0", "CY9", "E3", "E4",
        "EK0", "FT/G", "FT/J", "FT/W", "FT/X", "FT/Z", "H40", "HK0", "P5", "S0", "T31",
        "T32", "T33", "VK0H", "VK0M", "VK9C", "VK9L", "VK9M", "VK9N", "VK9W", "VK9X",
        "VP8G", "VP8H", "VP8O", "VP8S", "XF4", "XU", "XW", "XX9", "YJ0", "Z2", "ZS8",
    ];
    rare_prefixes.iter().any(|&prefix| call_sign.starts_with(prefix))
}
```

Also update `calculate_dx_priority` (line 234) to use rarity-tier-based scoring instead of the flat +150:

```rust
// Replace the is_rare_dx check with tiered scoring:
match station.rarity_tier.as_deref() {
    Some("legendary") => score += 200,
    Some("very_rare") => score += 150,
    Some("rare") => score += 100,
    Some("uncommon") => score += 50,
    _ => {
        // Fallback to hardcoded check
        if is_rare_dx_fallback(&station.call_sign) {
            score += 150;
        }
    }
}
```

- [ ] **Step 4: Update placeholder row to match new column count**

Update the empty-state placeholder (line 43) to have 9 cells instead of 8.

- [ ] **Step 5: Build and verify**

```bash
touch pancetta-tui/src/ui/dx_hunter.rs
cargo build -p pancetta-tui 2>&1 | tail -5
```

- [ ] **Step 6: Commit**

```bash
git add pancetta-tui/src/ui/dx_hunter.rs
git commit -m "feat: DX Hunter — Src column, notable styling, rarity tiers, staleness dim"
```

---

## Task 4: Extend CachedStationLookup with network metadata

**Files:**
- Modify: `pancetta/src/priority_evaluator.rs`

- [ ] **Step 1: Add network metadata fields**

Add to the `CachedStationLookup` struct (line 15):

```rust
/// Notable callsigns from cqdx.io spot groups.
notable_callsigns: Arc<RwLock<HashSet<String>>>,
/// Network SNR data: callsign -> (reporter_count, best_snr).
network_snr: Arc<RwLock<HashMap<String, (u32, i32)>>>,
/// Network last-seen timestamps: callsign -> unix timestamp.
network_last_seen: Arc<RwLock<HashMap<String, i64>>>,
```

Initialize in `new()`:

```rust
notable_callsigns: Arc::new(RwLock::new(HashSet::new())),
network_snr: Arc::new(RwLock::new(HashMap::new())),
network_last_seen: Arc::new(RwLock::new(HashMap::new())),
```

- [ ] **Step 2: Add update methods**

```rust
pub fn update_notable_callsigns(&self, callsigns: HashSet<String>) {
    *self.notable_callsigns.write().unwrap() = callsigns;
}

pub fn update_network_snr(&self, data: HashMap<String, (u32, i32)>) {
    *self.network_snr.write().unwrap() = data;
}

pub fn update_network_last_seen(&self, data: HashMap<String, i64>) {
    *self.network_last_seen.write().unwrap() = data;
}

pub fn is_notable(&self, callsign: &str) -> bool {
    self.notable_callsigns
        .read()
        .unwrap()
        .contains(&callsign.to_uppercase())
}

pub fn network_snr(&self, callsign: &str) -> Option<(u32, i32)> {
    self.network_snr
        .read()
        .unwrap()
        .get(&callsign.to_uppercase())
        .copied()
}

pub fn network_last_seen(&self, callsign: &str) -> Option<i64> {
    self.network_last_seen
        .read()
        .unwrap()
        .get(&callsign.to_uppercase())
        .copied()
}
```

- [ ] **Step 3: Add WorkedStationLookup trait methods**

Add to the `WorkedStationLookup` trait in `pancetta-qso/src/priority.rs`:

```rust
fn is_notable(&self, _callsign: &str) -> bool { false }
fn network_snr(&self, _callsign: &str) -> Option<(u32, i32)> { None }
fn network_last_seen(&self, _callsign: &str) -> Option<i64> { None }
```

And implement them in `CachedStationLookup`:

```rust
fn is_notable(&self, callsign: &str) -> bool {
    self.notable_callsigns.read().unwrap().contains(&callsign.to_uppercase())
}

fn network_snr(&self, callsign: &str) -> Option<(u32, i32)> {
    self.network_snr.read().unwrap().get(&callsign.to_uppercase()).copied()
}

fn network_last_seen(&self, callsign: &str) -> Option<i64> {
    self.network_last_seen.read().unwrap().get(&callsign.to_uppercase()).copied()
}
```

- [ ] **Step 4: Build and verify**

```bash
touch pancetta/src/priority_evaluator.rs pancetta-qso/src/priority.rs
cargo build 2>&1 | tail -5
```

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/priority_evaluator.rs pancetta-qso/src/priority.rs
git commit -m "feat: extend WorkedStationLookup with notable, SNR, last_seen metadata"
```

---

## Task 5: Wire spot poller to update new metadata and TUI

**Files:**
- Modify: `pancetta/src/cqdx_bridge.rs:126-144`
- Modify: `pancetta/src/coordinator/pipeline.rs`

- [ ] **Step 1: Update spot poller to push notable/SNR/last_seen metadata**

In `cqdx_bridge.rs`, inside the `Ok(groups)` arm of the poller (line 130), after the existing `update_rarity_scores` call, add:

```rust
// Update notable callsigns
let notables: HashSet<String> = groups
    .iter()
    .filter(|g| g.is_notable)
    .map(|g| g.dx_call.to_uppercase())
    .collect();
cached_lookup.update_notable_callsigns(notables);

// Update network SNR data
let snr_data: HashMap<String, (u32, i32)> = groups
    .iter()
    .filter_map(|g| {
        g.best_snr.map(|snr| (g.dx_call.to_uppercase(), (g.reporter_count, snr)))
    })
    .collect();
cached_lookup.update_network_snr(snr_data);

// Update network last-seen timestamps
let last_seen_data: HashMap<String, i64> = groups
    .iter()
    .map(|g| (g.dx_call.to_uppercase(), g.last_seen))
    .collect();
cached_lookup.update_network_last_seen(last_seen_data);
```

- [ ] **Step 2: Add method to CqdxBridge to get spot infos for TUI**

Add a public method:

```rust
/// Get current spot groups as lightweight TUI-friendly structs.
pub async fn spot_infos_for_tui(&self) -> Vec<pancetta_tui::tui_runner::CqdxSpotInfo> {
    let cache = self.cache.read().await;
    cache.spot_groups().iter().map(|g| {
        pancetta_tui::tui_runner::CqdxSpotInfo {
            dx_call: g.dx_call.clone(),
            band: g.band.clone(),
            mode: g.mode.clone(),
            frequency_hz: g.frequency,
            grid: g.dx_grid.clone(),
            rarity_tier: g.rarity_tier.clone(),
            reporter_count: g.reporter_count,
            best_snr: g.best_snr,
            confidence: g.confidence,
            first_seen: g.first_seen,
            last_seen: g.last_seen,
            is_notable: g.is_notable,
            notable_type: g.notable_type.clone(),
            entity_name: g.dx_entity_name.clone(),
        }
    }).collect()
}
```

- [ ] **Step 3: Send SpotGroupUpdate to TUI from coordinator pipeline**

In `pancetta/src/coordinator/pipeline.rs`, find the spot poller success path and add a TUI message send. The simplest approach: in the poller's `Ok(groups)` arm, after updating the cache, also send to the TUI channel:

This requires passing the `tui_tx` sender into the poller. Alternatively, add a periodic check in the TUI pipeline loop that reads from the bridge. The cleanest approach: after each spot poll update, clone the spot infos and send via the existing TUI channel.

Add to the spot poller success path in `cqdx_bridge.rs`:

```rust
// If tui_tx is available, send spot update
if let Some(ref tx) = tui_tx {
    let spot_infos: Vec<_> = groups.iter().map(|g| {
        pancetta_tui::tui_runner::CqdxSpotInfo {
            dx_call: g.dx_call.clone(),
            band: g.band.clone(),
            mode: g.mode.clone(),
            frequency_hz: g.frequency,
            grid: g.dx_grid.clone(),
            rarity_tier: g.rarity_tier.clone(),
            reporter_count: g.reporter_count,
            best_snr: g.best_snr,
            confidence: g.confidence,
            first_seen: g.first_seen,
            last_seen: g.last_seen,
            is_notable: g.is_notable,
            notable_type: g.notable_type.clone(),
            entity_name: g.dx_entity_name.clone(),
        }
    }).collect();
    let _ = tx.send(pancetta_tui::tui_runner::TuiMessage::SpotGroupUpdate { spots: spot_infos });
}
```

This means `spawn_spot_poller` needs an additional parameter: `tui_tx: Option<crossbeam_channel::Sender<TuiMessage>>`. Update the signature and call site accordingly.

- [ ] **Step 4: Build and verify**

```bash
touch pancetta/src/cqdx_bridge.rs pancetta/src/coordinator/pipeline.rs
cargo build 2>&1 | tail -5
```

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/cqdx_bridge.rs pancetta/src/coordinator/pipeline.rs
git commit -m "feat: wire spot poller to update network metadata and push to TUI"
```

---

## Task 6: Band-filtered spot polling

**Files:**
- Modify: `pancetta-cqdx/src/cache.rs`
- Modify: `pancetta/src/cqdx_bridge.rs`
- Modify: `pancetta/src/coordinator/pipeline.rs`

- [ ] **Step 1: Add frequency_to_band utility**

In `pancetta-cqdx/src/cache.rs`, add:

```rust
/// Derive ham radio band name from frequency in Hz.
pub fn frequency_to_band(freq_hz: u64) -> Option<String> {
    match freq_hz / 1_000_000 {
        1..=2 => Some("160m".to_string()),
        3..=4 => Some("80m".to_string()),
        5..=6 => Some("60m".to_string()),
        7..=8 => Some("40m".to_string()),
        10..=11 => Some("30m".to_string()),
        14..=15 => Some("20m".to_string()),
        18..=19 => Some("17m".to_string()),
        21..=22 => Some("15m".to_string()),
        24..=25 => Some("12m".to_string()),
        28..=30 => Some("10m".to_string()),
        50..=54 => Some("6m".to_string()),
        _ => None,
    }
}
```

- [ ] **Step 2: Store current band in shared state for poller**

In `CqdxBridge`, add a shared band field:

```rust
pub struct CqdxBridge {
    // ... existing fields ...
    /// Current band, updated by coordinator when radio frequency changes.
    current_band: Arc<RwLock<Option<String>>>,
}
```

Add a public setter:

```rust
pub async fn set_current_band(&self, band: Option<String>) {
    *self.current_band.write().await = band;
}
```

In the poller loop, read the current band each cycle instead of using the initial `band` parameter:

```rust
let current_band = current_band_ref.read().await.clone();
match client.fetch_live_spots(current_band.as_deref(), mode.as_deref()).await {
```

- [ ] **Step 3: Update coordinator to set band on frequency change**

In the coordinator pipeline where `FrequencyUpdate` messages are handled, add:

```rust
if let Some(ref bridge) = cqdx_bridge {
    let band = pancetta_cqdx::cache::frequency_to_band(frequency);
    bridge.set_current_band(band).await;
}
```

- [ ] **Step 4: Build and verify**

```bash
touch pancetta-cqdx/src/cache.rs pancetta/src/cqdx_bridge.rs pancetta/src/coordinator/pipeline.rs
cargo build 2>&1 | tail -5
```

- [ ] **Step 5: Commit**

```bash
git add pancetta-cqdx/src/cache.rs pancetta/src/cqdx_bridge.rs pancetta/src/coordinator/pipeline.rs
git commit -m "feat: band-filtered spot polling — derive band from radio frequency"
```

---

## Task 7: Scoring improvements — notable, staleness, SNR

**Files:**
- Modify: `pancetta-qso/src/priority.rs` (PriorityScorer)

- [ ] **Step 1: Find PriorityScorer::evaluate_cq and add scoring factors**

Read `pancetta-qso/src/priority.rs` to find the `evaluate_cq` implementation. Add three scoring factors:

**Notable bonus:**
```rust
// Notable station bonus
if lookup.is_notable(callsign) {
    score += 0.3;
}
```

**Staleness multiplier:**
```rust
// Staleness: deprioritize network spots that haven't been seen recently
if let Some(last_seen) = lookup.network_last_seen(callsign) {
    let now = chrono::Utc::now().timestamp();
    let age_secs = (now - last_seen).max(0);
    let staleness = match age_secs {
        0..=300 => 1.0,      // <5 min: fresh
        301..=600 => 0.7,    // 5-10 min: aging
        601..=900 => 0.4,    // 10-15 min: stale
        _ => 0.2,            // >15 min: very stale
    };
    score *= staleness;
}
```

**SNR-aware scoring:**
```rust
// Network SNR bonus/penalty
if let Some((reporter_count, best_snr)) = lookup.network_snr(callsign) {
    if reporter_count >= 5 && best_snr >= -20 {
        score += 0.1; // well-confirmed, likely workable
    }
    if reporter_count == 1 && best_snr < -25 {
        score -= 0.1; // uncertain, might not be workable
    }
}
```

- [ ] **Step 2: Build and test**

```bash
touch pancetta-qso/src/priority.rs
cargo test -p pancetta-qso 2>&1 | tail -5
cargo build 2>&1 | tail -5
```

- [ ] **Step 3: Commit**

```bash
git add pancetta-qso/src/priority.rs
git commit -m "feat: priority scoring — notable bonus, staleness decay, network SNR"
```

---

## Task 8: Spot-aware frequency nudging

**Files:**
- Modify: `pancetta-qso/src/autonomous.rs`

- [ ] **Step 1: Add spot data to AutonomousOperator**

Add a field to the `AutonomousOperator` struct:

```rust
/// Live spot groups for frequency nudging.
live_spot_frequencies: Vec<(f64, f64)>, // (frequency_hz, rarity 0.0-1.0)
```

Initialize to empty in `new()`.

Add a setter method:

```rust
/// Update live spot frequencies from cqdx.io for frequency nudging.
pub fn update_live_spots(&mut self, spots: &[(f64, f64)]) {
    self.live_spot_frequencies = spots.to_vec();
}
```

- [ ] **Step 2: Use spots in allocate_smart_frequency**

In `allocate_smart_frequency` (line 824), after the existing candidate ranking, add a spot proximity bonus when calling CQ (`dx_target_hz` is `None`):

```rust
if dx_target_hz.is_none() {
    // When calling CQ, prefer frequencies near rare DX spots
    for candidate in &mut candidates {
        for &(spot_freq, spot_rarity) in &self.live_spot_frequencies {
            let distance = (candidate.offset_hz - spot_freq).abs();
            if distance < 200.0 && spot_rarity > 0.7 {
                candidate.score += 0.2 * spot_rarity;
            }
        }
    }
    candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
}
```

- [ ] **Step 3: Wire coordinator to feed spots**

In `pancetta/src/coordinator/components.rs`, in the autonomous operator's slot loop, before `decide()`, add:

```rust
if let Some(ref bridge) = cqdx_bridge_for_auto {
    let cache = bridge.cache.read().await;
    let spot_freqs: Vec<(f64, f64)> = cache.spot_groups().iter().map(|g| {
        (g.frequency as f64, pancetta_cqdx::types::rank_to_rarity(g.rarity_rank))
    }).collect();
    op.update_live_spots(&spot_freqs);
}
```

Note: this requires exposing `cache` field on `CqdxBridge` as `pub(crate)` or adding a method. The cleanest approach: add `pub async fn spot_frequencies(&self) -> Vec<(f64, f64)>` to `CqdxBridge`.

- [ ] **Step 4: Build and verify**

```bash
touch pancetta-qso/src/autonomous.rs pancetta/src/coordinator/components.rs pancetta/src/cqdx_bridge.rs
cargo build 2>&1 | tail -5
```

- [ ] **Step 5: Commit**

```bash
git add pancetta-qso/src/autonomous.rs pancetta/src/coordinator/components.rs pancetta/src/cqdx_bridge.rs
git commit -m "feat: spot-aware frequency nudging — prefer CQ near rare DX"
```

---

## Execution Notes

- **Task dependencies:** Tasks 1→2→3 are sequential (each builds on prior fields/types). Task 4 is independent. Task 5 depends on Tasks 2 and 4. Task 6 is independent. Task 7 depends on Task 4. Task 8 is independent of TUI tasks but depends on cqdx_bridge changes.
- **Parallelizable:** Tasks 4 and 6 can run in parallel with Tasks 1-3.
- **Touch modified `.rs` files** before building (cargo cache issue).
- **Subagents cannot run git commands** — commit from main session.
- The `WorkedStationLookup` trait changes (Task 4) have default implementations, so existing code won't break.
- All features degrade gracefully when CQDX is disabled (None/empty defaults).
