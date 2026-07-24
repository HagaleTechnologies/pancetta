# DX Watchlist Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remember `PerBandDxccNew`+/`Atno` CQs heard but not pounced on (busy, at capacity, or lost that cycle's single-pounce-slot competition), so the same station gets a fair shot the next time it's actively CQing and we have room — without ever calling anyone who isn't actively CQing that cycle.

**Architecture:** A new `DxWatchlist` struct (own module, `pancetta-qso/src/watchlist.rs`) owned by `AutonomousOperator`, populated as a side effect of the existing `feed_decoded_messages_at` CQ-scoring pass (no new decision path — it never injects synthetic pounce targets). Requires extending `DxEvaluator` with a tier-returning method, since the autonomous loop currently only sees a continuous score. Surfaced to the TUI via a small periodic bus broadcast (mirroring the existing `TxPlacementUpdate` pattern) and a new marker glyph on existing DX Hunter rows.

**Tech Stack:** Rust, `pancetta-qso` (core logic), `pancetta` (coordinator wiring), `pancetta-tui` (display).

## Global Constraints

- Never transmit to a callsign that is not actively, freshly CQing in the current decode cycle (spec: `docs/superpowers/specs/2026-07-24-dx-watchlist-design.md`).
- Single-scorer invariant: tier classification must come from the exact same `PriorityScorer::score_tiered` call DX Hunter's display already uses — no second, divergent tier computation.
- `pancetta-tui` must not depend on `pancetta-qso` — all cross-crate type conversion happens in `pancetta/src/coordinator/tui_relay.rs`.
- New config fields default such that behavior is additive-only; existing tests must not regress.

---

## Task 1: `DxWatchlist` core module

**Files:**
- Create: `pancetta-qso/src/watchlist.rs`
- Modify: `pancetta-qso/src/lib.rs` (register module)

**Interfaces:**
- Produces: `pancetta_qso::watchlist::{DxWatchlist, DxWatchlistEntry}`, re-exported at crate root via `pub use crate::watchlist::*;`. `DxWatchlist::new(ttl: chrono::Duration) -> Self`, `refresh(&mut self, callsign: &str, grid: Option<&str>, tier: PriorityTier, now: DateTime<Utc>)`, `prune(&mut self, now: DateTime<Utc>)`, `remove(&mut self, callsign: &str)`, `callsigns(&self) -> Vec<String>` (uppercased), `len(&self) -> usize`, `is_empty(&self) -> bool`.

- [ ] **Step 1: Write the failing tests**

Create `pancetta-qso/src/watchlist.rs`:

```rust
//! Short-lived (~2-3 minute) per-callsign memory of `PerBandDxccNew`+/`Atno`
//! CQs heard but not pounced on. See
//! `docs/superpowers/specs/2026-07-24-dx-watchlist-design.md`.
//!
//! Never triggers a transmission by itself — it is a one-way read off the
//! CQ-scoring pass in `AutonomousOperator::feed_decoded_messages_at`, purely
//! bookkeeping. A watchlisted callsign only ever gets pounced on the
//! ordinary way: by being freshly, actively re-decoded as a CQ on a later
//! cycle.

use crate::priority::PriorityTier;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;

/// One remembered near-miss.
#[derive(Debug, Clone, PartialEq)]
pub struct DxWatchlistEntry {
    pub callsign: String,
    pub grid: Option<String>,
    pub tier: PriorityTier,
    pub last_heard: DateTime<Utc>,
}

/// Short-lived memory of `PerBandDxccNew`+ CQs heard but not pounced on.
#[derive(Debug, Clone)]
pub struct DxWatchlist {
    entries: HashMap<String, DxWatchlistEntry>,
    ttl: Duration,
}

impl DxWatchlist {
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            ttl,
        }
    }

    /// Insert or refresh an entry (uppercased callsign key). Overwrites any
    /// existing entry for the same callsign, including its tier — a station
    /// re-heard at a different tier reflects the current, more accurate
    /// classification.
    pub fn refresh(&mut self, callsign: &str, grid: Option<&str>, tier: PriorityTier, now: DateTime<Utc>) {
        let key = callsign.to_uppercase();
        self.entries.insert(
            key.clone(),
            DxWatchlistEntry {
                callsign: key,
                grid: grid.map(|g| g.to_uppercase()),
                tier,
                last_heard: now,
            },
        );
    }

    /// Drop entries not refreshed within the TTL.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        let ttl = self.ttl;
        self.entries
            .retain(|_, e| now.signed_duration_since(e.last_heard) < ttl);
    }

    /// Remove a specific callsign immediately (e.g. once worked).
    pub fn remove(&mut self, callsign: &str) {
        self.entries.remove(&callsign.to_uppercase());
    }

    /// Currently-watchlisted callsigns (uppercased), for TUI/status surfacing.
    pub fn callsigns(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).unwrap()
    }

    #[test]
    fn refresh_inserts_new_entry() {
        let mut wl = DxWatchlist::new(Duration::seconds(150));
        wl.refresh("JA1ABC", Some("PM95"), PriorityTier::PerBandDxccNew, t(0));
        assert_eq!(wl.len(), 1);
        assert_eq!(wl.callsigns(), vec!["JA1ABC".to_string()]);
    }

    #[test]
    fn refresh_is_case_insensitive_and_uppercases_grid() {
        let mut wl = DxWatchlist::new(Duration::seconds(150));
        wl.refresh("ja1abc", Some("pm95"), PriorityTier::Atno, t(0));
        assert_eq!(wl.callsigns(), vec!["JA1ABC".to_string()]);
    }

    #[test]
    fn refresh_twice_updates_last_heard_without_duplicating() {
        let mut wl = DxWatchlist::new(Duration::seconds(150));
        wl.refresh("JA1ABC", None, PriorityTier::PerBandDxccNew, t(0));
        wl.refresh("JA1ABC", None, PriorityTier::Atno, t(10));
        assert_eq!(wl.len(), 1, "same callsign must not duplicate");
    }

    #[test]
    fn prune_removes_expired_entries() {
        let mut wl = DxWatchlist::new(Duration::seconds(150));
        wl.refresh("JA1ABC", None, PriorityTier::PerBandDxccNew, t(0));
        wl.prune(t(151));
        assert!(wl.is_empty(), "entry older than TTL must be pruned");
    }

    #[test]
    fn prune_keeps_entries_within_ttl() {
        let mut wl = DxWatchlist::new(Duration::seconds(150));
        wl.refresh("JA1ABC", None, PriorityTier::PerBandDxccNew, t(0));
        wl.prune(t(149));
        assert_eq!(wl.len(), 1, "entry within TTL must survive prune");
    }

    #[test]
    fn remove_deletes_a_specific_entry() {
        let mut wl = DxWatchlist::new(Duration::seconds(150));
        wl.refresh("JA1ABC", None, PriorityTier::PerBandDxccNew, t(0));
        wl.refresh("W1XYZ", None, PriorityTier::Atno, t(0));
        wl.remove("ja1abc");
        assert_eq!(wl.callsigns(), vec!["W1XYZ".to_string()]);
    }
}
```

- [ ] **Step 2: Register the module**

In `pancetta-qso/src/lib.rs`, add alongside the other `pub mod`/`pub use` pairs (near `pub mod priority;` at line 143):

```rust
pub mod watchlist;
pub use watchlist::{DxWatchlist, DxWatchlistEntry};
```

- [ ] **Step 3: Run tests to verify they fail, then pass**

Run: `cargo test -p pancetta-qso watchlist:: -- --nocapture`
Expected first: FAIL (module doesn't compile/exist yet if Step 2 wasn't done first — do Step 2 before running). After Step 2: all 6 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add pancetta-qso/src/watchlist.rs pancetta-qso/src/lib.rs
git commit -m "feat(qso): add DxWatchlist core module (#197)"
```

---

## Task 2: `DxEvaluator` tier extension

**Files:**
- Modify: `pancetta-qso/src/autonomous.rs:306-308` (the `DxEvaluator` trait)
- Modify: `pancetta-qso/src/priority.rs:487-491` (`impl DxEvaluator for PriorityScorer`)

**Interfaces:**
- Consumes: `pancetta_qso::priority::{PriorityScorer, TieredScore}` (existing, from Task-independent code — `score_tiered` already exists at `priority.rs:473-485`).
- Produces: `DxEvaluator::evaluate_cq_tiered(&self, callsign: &str, grid: Option<&str>, snr: i8, freq_hz: f64) -> Option<TieredScore>`, default `None`. `PriorityScorer`'s impl returns `Some(self.score_tiered(...))`.

- [ ] **Step 1: Write the failing test**

In `pancetta-qso/src/priority.rs`, inside the existing `#[cfg(test)] mod tests` block (near `classify_tier_atno_beats_everything_even_with_weak_signal` around line 953), add:

```rust
#[test]
fn evaluate_cq_tiered_matches_score_tiered_exactly() {
    let mut lookup = TestLookup::new();
    lookup.needed_dxcc.insert("JA1ABC".to_string());
    let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));

    let via_trait = scorer.evaluate_cq_tiered("JA1ABC", Some("PM95"), -10, 14_074_000.0);
    let via_inherent = scorer.score_tiered("JA1ABC", Some("PM95"), -10, 14_074_000.0);

    assert_eq!(
        via_trait,
        Some(via_inherent),
        "DxEvaluator::evaluate_cq_tiered must delegate to score_tiered exactly \
         (single-scorer invariant) — no second, divergent tier computation"
    );
}
```

In `pancetta-qso/src/autonomous.rs`'s test module, near the existing `NullDxEvaluator`-adjacent tests, add:

```rust
#[test]
fn null_dx_evaluator_tiered_defaults_to_none() {
    let evaluator = NullDxEvaluator;
    assert_eq!(
        evaluator.evaluate_cq_tiered("W1ABC", None, -10, 14_074_000.0),
        None,
        "evaluators that don't implement tiered scoring must default to None"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-qso evaluate_cq_tiered -- --nocapture`
Expected: FAIL with "no method named `evaluate_cq_tiered` found" (trait method doesn't exist yet).

- [ ] **Step 3: Add the trait method**

In `pancetta-qso/src/autonomous.rs`, replace the `DxEvaluator` trait definition (currently lines 302-308):

```rust
pub trait DxEvaluator: Send + Sync {
    fn evaluate_cq(&self, callsign: &str, grid: Option<&str>, snr: i8, freq_hz: f64) -> f64;

    /// Tiered classification, when available. `None` for evaluators that
    /// don't implement tiered scoring (e.g. `NullDxEvaluator`, test
    /// scaffolding) — callers that need a tier (the DX watchlist) simply
    /// skip entries where this returns `None`.
    fn evaluate_cq_tiered(
        &self,
        _callsign: &str,
        _grid: Option<&str>,
        _snr: i8,
        _freq_hz: f64,
    ) -> Option<crate::priority::TieredScore> {
        None
    }
}
```

- [ ] **Step 4: Implement it for `PriorityScorer`**

In `pancetta-qso/src/priority.rs`, replace the existing `impl DxEvaluator for PriorityScorer` block (currently lines 487-491):

```rust
impl DxEvaluator for PriorityScorer {
    fn evaluate_cq(&self, callsign: &str, grid: Option<&str>, snr: i8, freq_hz: f64) -> f64 {
        self.score_cq_detailed(callsign, grid, snr, freq_hz).total
    }

    fn evaluate_cq_tiered(
        &self,
        callsign: &str,
        grid: Option<&str>,
        snr: i8,
        freq_hz: f64,
    ) -> Option<TieredScore> {
        Some(self.score_tiered(callsign, grid, snr, freq_hz))
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p pancetta-qso evaluate_cq_tiered null_dx_evaluator_tiered -- --nocapture`
Expected: both PASS.

- [ ] **Step 6: Run the full `pancetta-qso` suite to check for regressions**

Run: `cargo test -p pancetta-qso`
Expected: all existing tests still PASS (the new trait method has a default body, so `HighScoreEvaluator` and every other existing `DxEvaluator` impl in the test suite compiles unchanged).

- [ ] **Step 7: Commit**

```bash
git add pancetta-qso/src/autonomous.rs pancetta-qso/src/priority.rs
git commit -m "feat(qso): add DxEvaluator::evaluate_cq_tiered, wired for PriorityScorer (#197)"
```

---

## Task 3: Wire `DxWatchlist` into `AutonomousOperator`

**Files:**
- Modify: `pancetta-qso/src/autonomous.rs` — imports (top of file), `AutonomousConfig` (around line 486-511), `AutonomousOperator` struct (around line 767-836), `AutonomousOperator::new` (around line 838-874), `feed_decoded_messages_at` (around line 941-1057).

**Interfaces:**
- Consumes: `DxWatchlist`, `DxWatchlistEntry` (Task 1); `DxEvaluator::evaluate_cq_tiered` (Task 2).
- Produces: `AutonomousOperator::watchlist_callsigns(&self) -> Vec<String>`. `AutonomousConfig::watchlist_ttl_secs: u64` (default `150`).

- [ ] **Step 1: Write the failing tests**

In `pancetta-qso/src/autonomous.rs`'s `#[cfg(test)] mod tests` block, add a tiered test evaluator and the new tests (place near `HighScoreEvaluator`, around line 2691-2697):

```rust
/// Test helper: evaluator whose tier is controllable per-callsign, default
/// score irrelevant to these tests.
struct TieredTestEvaluator {
    tier_for: std::collections::HashMap<String, PriorityTier>,
}
impl TieredTestEvaluator {
    fn new() -> Self {
        Self {
            tier_for: std::collections::HashMap::new(),
        }
    }
    fn with_tier(mut self, callsign: &str, tier: PriorityTier) -> Self {
        self.tier_for.insert(callsign.to_string(), tier);
        self
    }
}
impl DxEvaluator for TieredTestEvaluator {
    fn evaluate_cq(&self, _: &str, _: Option<&str>, _: i8, _: f64) -> f64 {
        0.5
    }
    fn evaluate_cq_tiered(
        &self,
        callsign: &str,
        _: Option<&str>,
        _: i8,
        _: f64,
    ) -> Option<TieredScore> {
        self.tier_for.get(callsign).map(|&tier| TieredScore {
            tier,
            secondary: 0.5,
        })
    }
}

fn cq_message(callsign: &str, grid: &str) -> DecodedMessageInfo {
    DecodedMessageInfo {
        callsign: Some(callsign.to_string()),
        frequency_hz: 1500.0,
        snr: -10,
        message_text: format!("CQ {callsign} {grid}"),
        slot_parity: None,
        confidence: None,
        time_offset_s: None,
        decode_origin: None,
    }
}

#[test]
fn watchlist_gains_per_band_dxcc_new_cq_even_when_at_tx_capacity() {
    let config = AutonomousConfig::default();
    let mut op = AutonomousOperator::new(config, "W1ABC".to_string(), Some("FN42".to_string()));
    // Simulate being at full TX capacity — feed_decoded_messages_at must
    // still populate the watchlist regardless of decide_at()'s capacity gate.
    op.set_active_qso_count(999);

    let evaluator =
        TieredTestEvaluator::new().with_tier("JA1ABC", PriorityTier::PerBandDxccNew);
    op.feed_decoded_messages_at(&[cq_message("JA1ABC", "PM95")], &evaluator, Utc::now());

    assert_eq!(op.watchlist_callsigns(), vec!["JA1ABC".to_string()]);
}

#[test]
fn watchlist_ignores_standard_tier_cqs() {
    let config = AutonomousConfig::default();
    let mut op = AutonomousOperator::new(config, "W1ABC".to_string(), Some("FN42".to_string()));

    let evaluator = TieredTestEvaluator::new().with_tier("W2XYZ", PriorityTier::Standard);
    op.feed_decoded_messages_at(&[cq_message("W2XYZ", "FN42")], &evaluator, Utc::now());

    assert!(
        op.watchlist_callsigns().is_empty(),
        "Standard-tier CQs must never enter the watchlist"
    );
}

#[test]
fn watchlist_ignores_untiered_evaluators() {
    // NullDxEvaluator (and any evaluator that doesn't implement tiered
    // scoring) must never populate the watchlist — `evaluate_cq_tiered`
    // defaults to None, which this feed loop must skip.
    let config = AutonomousConfig::default();
    let mut op = AutonomousOperator::new(config, "W1ABC".to_string(), Some("FN42".to_string()));

    op.feed_decoded_messages_at(&[cq_message("JA1ABC", "PM95")], &NullDxEvaluator, Utc::now());

    assert!(op.watchlist_callsigns().is_empty());
}

#[test]
fn watchlist_entry_expires_after_ttl_with_no_rehear() {
    let mut config = AutonomousConfig::default();
    config.watchlist_ttl_secs = 150;
    let mut op = AutonomousOperator::new(config, "W1ABC".to_string(), Some("FN42".to_string()));

    let t0 = DateTime::<Utc>::from_timestamp(1_000_000, 0).unwrap();
    let evaluator =
        TieredTestEvaluator::new().with_tier("JA1ABC", PriorityTier::PerBandDxccNew);
    op.feed_decoded_messages_at(&[cq_message("JA1ABC", "PM95")], &evaluator, t0);
    assert_eq!(op.watchlist_callsigns(), vec!["JA1ABC".to_string()]);

    // Feed again, 151s later, with no re-hear of JA1ABC at all.
    let t1 = t0 + ChronoDuration::seconds(151);
    op.feed_decoded_messages_at(&[], &evaluator, t1);

    assert!(
        op.watchlist_callsigns().is_empty(),
        "entry must be pruned once its TTL elapses with no re-hear"
    );
}

#[test]
fn watchlist_entry_survives_within_ttl_with_no_rehear() {
    let mut config = AutonomousConfig::default();
    config.watchlist_ttl_secs = 150;
    let mut op = AutonomousOperator::new(config, "W1ABC".to_string(), Some("FN42".to_string()));

    let t0 = DateTime::<Utc>::from_timestamp(1_000_000, 0).unwrap();
    let evaluator =
        TieredTestEvaluator::new().with_tier("JA1ABC", PriorityTier::PerBandDxccNew);
    op.feed_decoded_messages_at(&[cq_message("JA1ABC", "PM95")], &evaluator, t0);

    // Feed again, only 30s later, with no re-hear — must still be present.
    let t1 = t0 + ChronoDuration::seconds(30);
    op.feed_decoded_messages_at(&[], &evaluator, t1);

    assert_eq!(op.watchlist_callsigns(), vec!["JA1ABC".to_string()]);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-qso watchlist -- --nocapture`
Expected: FAIL to compile — `AutonomousOperator` has no `watchlist_callsigns` method and `AutonomousConfig` has no `watchlist_ttl_secs` field yet.

- [ ] **Step 3: Add the config field**

In `pancetta-qso/src/autonomous.rs`, in `AutonomousConfig` (currently ending at the `dx_busy_window_secs` field, lines 505-511), add:

```rust
    /// DX-busy suppression window (seconds). If a DX station was seen
    /// participating in a non-CQ exchange (report / RR73 / 73 not directed
    /// at us) within this window, the autonomous operator will not start a
    /// new call to it even if it briefly CQs again — it is presumed busy
    /// with a third party. Default: 90 s.
    pub dx_busy_window_secs: u64,
    /// DX watchlist (#197) TTL in seconds: how long a `PerBandDxccNew`+/
    /// `Atno` CQ heard-but-not-pounced-on stays remembered before being
    /// dropped as presumed moved on. Default: 150 s (~2.5 min).
    pub watchlist_ttl_secs: u64,
}
```

And in `impl Default for AutonomousConfig` (lines 513-530), add after `dx_busy_window_secs: 90,`:

```rust
            dx_busy_window_secs: 90,
            watchlist_ttl_secs: 150,
```

- [ ] **Step 4: Add imports, the struct field, and constructor wiring**

At the top of `pancetta-qso/src/autonomous.rs`, add to the existing `use` block (near `use crate::frequency::{...}`):

```rust
use crate::priority::{PriorityTier, TieredScore};
use crate::watchlist::DxWatchlist;
```

In the `AutonomousOperator` struct (currently ending with `tx_offset_hold_hz` around line 835), add:

```rust
    tx_offset_hold_hz: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    /// DX watchlist (#197): short-lived memory of `PerBandDxccNew`+/`Atno`
    /// CQs heard but not pounced on. See `pancetta_qso::watchlist`.
    watchlist: DxWatchlist,
}
```

In `AutonomousOperator::new` (currently lines 838-874), add `watchlist_ttl_secs` capture before it's moved and initialize the field:

```rust
    pub fn new(config: AutonomousConfig, our_callsign: String, our_grid: Option<String>) -> Self {
        let slot_manager = SlotManager::new(config.slot_parity, &config.listen_cycle);
        let collision_detector = CollisionDetector::new(config.tx_offset_hz, 50.0);
        let band_strategy = BandStrategy::new(config.band_hopping.clone());
        // FT8 bandwidth: 8 tones * 6.25 Hz = 50 Hz, plus 25 Hz guard = 75 Hz min separation
        let frequency_allocator = FrequencyAllocator::new(75.0, (200.0, 2800.0));
        let decode_history = DecodeHistory::new(config.frequency.decode_history_cycles);
        let smart_allocator = SmartFrequencyAllocator::new(config.frequency.clone());
        let watchlist = DxWatchlist::new(chrono::Duration::seconds(config.watchlist_ttl_secs as i64));

        Self {
            config,
            slot_manager,
            collision_detector,
            band_strategy,
            frequency_allocator,
            state: OperatingState::Hunting,
            idle_cycles: 0,
            our_callsign,
            our_grid,
            pending_cqs: Vec::new(),
            active_qso_count: 0,
            pending_sequencer_messages: Vec::new(),
            decode_history,
            spectral_snapshot: None,
            smart_allocator,
            paused: false,
            live_spot_frequencies: Vec::new(),
            recently_responded_to: HashMap::new(),
            recently_in_qso: HashMap::new(),
            fp_filter: None,
            tx_freq_mode: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
                pancetta_core::TxFreqMode::Hold.as_u8(),
            )),
            tx_offset_hold_hz: None,
            watchlist,
        }
    }
```

- [ ] **Step 5: Populate the watchlist in `feed_decoded_messages_at`**

In `feed_decoded_messages_at`, the CQ-extraction loop (currently lines 1021-1049) already computes `call`, `grid`, `snr`, and `msg.frequency_hz` for each CQ. Extend it to also refresh the watchlist, then prune once after the loop. Replace the existing loop body:

```rust
        // Extract CQ candidates.
        self.pending_cqs.clear();
        for msg in messages {
            if is_cq_message(&msg.message_text) {
                if let Some(ref call) = msg.callsign {
                    // Don't respond to our own CQ.
                    if call.eq_ignore_ascii_case(&self.our_callsign) {
                        continue;
                    }

                    let grid = extract_grid_from_cq(&msg.message_text);
                    let snr = msg.snr.clamp(-128, 127) as i8;
                    let score = evaluator.evaluate_cq(call, grid.as_deref(), snr, msg.frequency_hz);

                    // DX watchlist (#197): remember PerBandDxccNew+/Atno CQs
                    // heard this cycle regardless of what decide_at() goes on
                    // to do with them — bridges the "heard while at TX
                    // capacity" and "lost this cycle's single pounce slot"
                    // gaps. Never triggers a transmission by itself; see
                    // pancetta_qso::watchlist module docs.
                    if let Some(tiered) =
                        evaluator.evaluate_cq_tiered(call, grid.as_deref(), snr, msg.frequency_hz)
                    {
                        if tiered.tier >= PriorityTier::PerBandDxccNew {
                            self.watchlist.refresh(call, grid.as_deref(), tiered.tier, now);
                        }
                    }

                    self.pending_cqs.push(CqCandidate {
                        callsign: call.clone(),
                        grid,
                        snr,
                        frequency_hz: msg.frequency_hz,
                        dx_score: score,
                        slot_parity: msg.slot_parity,
                        message_text: msg.message_text.clone(),
                        confidence: msg.confidence,
                        time_offset_s: msg.time_offset_s,
                        decode_origin: msg.decode_origin,
                    });
                }
            }
        }
        self.watchlist.prune(now);

        // Sort: best score first.
        self.pending_cqs.sort_by(|a, b| {
            b.dx_score
                .partial_cmp(&a.dx_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
```

(`now` is already the function's own parameter — `feed_decoded_messages_at(&mut self, messages: &[DecodedMessageInfo], evaluator: &dyn DxEvaluator, now: DateTime<Utc>)` — no new parameter needed.)

- [ ] **Step 6: Add the read-only accessor**

Add near `placement_snapshot` (after it, or near `frequency_allocator()`):

```rust
    /// Currently-watchlisted callsigns, for TUI/status surfacing. Read-only —
    /// mirrors `placement_snapshot`'s "instrument, not a decision" pattern.
    pub fn watchlist_callsigns(&self) -> Vec<String> {
        self.watchlist.callsigns()
    }
```

- [ ] **Step 7: Run the new tests, then the full crate suite**

Run: `cargo test -p pancetta-qso watchlist -- --nocapture`
Expected: all 6 new tests in this task PASS.

Run: `cargo test -p pancetta-qso`
Expected: full suite PASSES (additive-only change; every existing `AutonomousConfig::default()` call picks up `watchlist_ttl_secs: 150` automatically).

- [ ] **Step 8: Commit**

```bash
git add pancetta-qso/src/autonomous.rs
git commit -m "feat(qso): populate DxWatchlist from feed_decoded_messages_at (#197)"
```

---

## Task 4: Coordinator wiring — publish watchlist to the message bus

**Files:**
- Modify: `pancetta/src/message_bus.rs` (new `MessageType` variant, near line 181)
- Modify: `pancetta/src/coordinator/autonomous.rs` (construct `qso_auto_config.watchlist_ttl_secs`, around line 415; publish tick, after line 662)

**Interfaces:**
- Consumes: `AutonomousOperator::watchlist_callsigns()` (Task 3).
- Produces: `MessageType::DxWatchlistUpdate { callsigns: Vec<String> }`.

- [ ] **Step 1: Add the message type**

In `pancetta/src/message_bus.rs`, add after the `TxPlacementUpdate` variant (currently lines 181-183):

```rust
    TxPlacementUpdate {
        snapshot: pancetta_qso::frequency::PlacementSnapshot,
    },

    /// Currently-watchlisted callsigns (#197 DX watchlist) — sent every
    /// autonomous tick alongside `TxPlacementUpdate`, same housekeeping
    /// cadence. A full resync each tick (not a diff), matching this
    /// codebase's existing "bulk replace, self-healing" convention (see
    /// `FrequencyAllocator::set_own_frequencies`).
    DxWatchlistUpdate {
        callsigns: Vec<String>,
    },
```

- [ ] **Step 2: Wire the config field**

In `pancetta/src/coordinator/autonomous.rs`, replace the `dx_busy_window_secs` line in the `qso_auto_config` literal (currently lines 413-416):

```rust
            // DX-busy suppression window. Not yet plumbed to pancetta-config;
            // use the AutonomousConfig default (90 s).
            dx_busy_window_secs: pancetta_qso::AutonomousConfig::default().dx_busy_window_secs,
            // DX watchlist (#197) TTL. Not yet plumbed to pancetta-config;
            // use the AutonomousConfig default (150 s / 2.5 min), same
            // precedent as dx_busy_window_secs above.
            watchlist_ttl_secs: pancetta_qso::AutonomousConfig::default().watchlist_ttl_secs,
        };
```

- [ ] **Step 3: Publish the watchlist each tick**

In `pancetta/src/coordinator/autonomous.rs`, immediately after `op.feed_decoded_messages(&slot_messages, evaluator.as_ref());` (currently line 662), add:

```rust
                            op.feed_decoded_messages(&slot_messages, evaluator.as_ref());

                            // DX watchlist (#197): housekeeping broadcast every
                            // tick, same cadence as the TX-placement instrument
                            // below — sent regardless of whether the list is
                            // empty, so the TUI side can bulk-resync (self-
                            // healing; never diffed).
                            {
                                let msg = ComponentMessage::new(
                                    ComponentId::Autonomous,
                                    ComponentId::Tui,
                                    MessageType::DxWatchlistUpdate {
                                        callsigns: op.watchlist_callsigns(),
                                    },
                                    Instant::now(),
                                );
                                let _ = message_bus.send_message(msg).await;
                            }
```

- [ ] **Step 4: Build to confirm it compiles**

Run: `cargo build -p pancetta`
Expected: builds cleanly (no test changes in this task — it's coordinator glue with existing test coverage patterns for `TxPlacementUpdate`, which also isn't unit-tested at this exact call site).

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/message_bus.rs pancetta/src/coordinator/autonomous.rs
git commit -m "feat(agent): publish DX watchlist to the message bus each tick (#197)"
```

---

## Task 5: `DxStation.watchlisted` + bulk resync

**Files:**
- Modify: `pancetta-tui/src/app.rs` — `DxStation` struct (line 402-451), 5 construction sites (lines ~1679, ~2498, ~3380, ~5108, ~5219), new `apply_dx_watchlist` method, tests.

**Interfaces:**
- Consumes: nothing new — self-contained addition to `App`/`DxStation`, independently buildable and testable ahead of the bus plumbing in Task 6.
- Produces: `DxStation.watchlisted: bool`; `App::apply_dx_watchlist(&mut self, callsigns: &[String])`.

- [ ] **Step 1: Write the failing test**

In `pancetta-tui/src/app.rs`'s test module (near `displayed_dx_stations_excludes_self`, around line 5100), add:

```rust
#[test]
fn apply_dx_watchlist_bulk_resyncs_flag_case_insensitively() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut app = rt.block_on(fixture_app());

    app.dx_stations.insert(
        "ja1abc".to_string(),
        dx_fixture("ja1abc", 3000, -10, 14.074, false),
    );
    app.dx_stations.insert(
        "W1XYZ".to_string(),
        dx_fixture("W1XYZ", 500, -10, 14.074, false),
    );

    app.apply_dx_watchlist(&["JA1ABC".to_string()]);

    assert!(app.dx_stations["ja1abc"].watchlisted);
    assert!(!app.dx_stations["W1XYZ"].watchlisted);
}

#[test]
fn apply_dx_watchlist_clears_flag_once_no_longer_listed() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut app = rt.block_on(fixture_app());

    let mut station = dx_fixture("JA1ABC", 3000, -10, 14.074, false);
    station.watchlisted = true;
    app.dx_stations.insert("JA1ABC".to_string(), station);

    // Bulk resync with an empty list — must clear, not just leave stale.
    app.apply_dx_watchlist(&[]);

    assert!(!app.dx_stations["JA1ABC"].watchlisted);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-tui apply_dx_watchlist -- --nocapture`
Expected: FAIL to compile — `DxStation` has no `watchlisted` field, `App` has no `apply_dx_watchlist` method, and `dx_fixture` (the existing test helper) doesn't set `watchlisted` yet.

- [ ] **Step 3: Add the field to `DxStation`**

In `pancetta-tui/src/app.rs`, add after `slot_parity` in the `DxStation` struct (currently the last field, ending the struct at line 451):

```rust
    pub slot_parity: Option<pancetta_core::slot::SlotParity>,
    /// DX watchlist (#197): true while this callsign is currently
    /// remembered as a heard-but-not-pounced-on PerBandDxccNew+/Atno CQ.
    /// Bulk-resynced each autonomous tick via `apply_dx_watchlist` — never
    /// diffed, so a stale `true` can't linger past the next tick.
    pub watchlisted: bool,
}
```

- [ ] **Step 4: Update the 5 construction sites**

Add `watchlisted: false,` as the last field in each of these 5 `DxStation { ... }` literals in `pancetta-tui/src/app.rs`. Each edit below shows the exact existing lines (`old`) and the replacement (`new`) — match on the `old` block verbatim to locate the site.

1. `feed_decoded_message`'s merge-or-insert (`None` arm), currently lines 1703-1709:

Old:
```rust
                            confidence: None,
                            best_snr_network: None,
                            last_seen_network: None,
                            audio_offset_hz: Some(audio_offset_hz),
                            slot_parity: message.slot_parity,
                        },
                    );
```

New:
```rust
                            confidence: None,
                            best_snr_network: None,
                            last_seen_network: None,
                            audio_offset_hz: Some(audio_offset_hz),
                            slot_parity: message.slot_parity,
                            watchlisted: false,
                        },
                    );
```

2. The `add_dx_spot`-style constructor (~line 2498-2526), currently ending:

Old:
```rust
            audio_offset_hz: None,
            slot_parity: None,
        };
        self.dx_stations.insert(callsign, dx_station);
```

New:
```rust
            audio_offset_hz: None,
            slot_parity: None,
            watchlisted: false,
        };
        self.dx_stations.insert(callsign, dx_station);
```

3. The cqdx live-spot `or_insert_with` (~line 3380-3411), currently ending:

Old:
```rust
                    best_snr_network: spot.best_snr,
                    last_seen_network: Some(spot.last_seen),
                    // Network spots carry no passband / slot information.
                    audio_offset_hz: None,
                    slot_parity: None,
                });
```

New:
```rust
                    best_snr_network: spot.best_snr,
                    last_seen_network: Some(spot.last_seen),
                    // Network spots carry no passband / slot information.
                    audio_offset_hz: None,
                    slot_parity: None,
                    watchlisted: false,
                });
```

4. The `displayed_dx_stations_excludes_self` test fixture (~line 5106-5133), currently ending:

Old:
```rust
                confidence: None,
                best_snr_network: None,
                last_seen_network: None,
                audio_offset_hz: None,
                slot_parity: None,
            },
        );
```

New:
```rust
                confidence: None,
                best_snr_network: None,
                last_seen_network: None,
                audio_offset_hz: None,
                slot_parity: None,
                watchlisted: false,
            },
        );
```

5. The `dx_fixture` test helper (~line 5218-5245), currently ending:

Old:
```rust
            audio_offset_hz: Some(1200),
            slot_parity: None,
        }
    }
```

New:
```rust
            audio_offset_hz: Some(1200),
            slot_parity: None,
            watchlisted: false,
        }
    }
```

- [ ] **Step 5: Add `apply_dx_watchlist`**

Add near `apply_placement` (or any other `apply_*` bulk-update method) in `pancetta-tui/src/app.rs`:

```rust
    /// Bulk-resync `DxStation.watchlisted` from the autonomous operator's
    /// current DX watchlist (#197). Full replace, not a diff — a callsign
    /// missing from `callsigns` is cleared, matching this codebase's
    /// existing "self-healing bulk replace" convention (e.g.
    /// `FrequencyAllocator::set_own_frequencies`).
    pub fn apply_dx_watchlist(&mut self, callsigns: &[String]) {
        let watchlisted: std::collections::HashSet<String> =
            callsigns.iter().map(|c| c.to_uppercase()).collect();
        for station in self.dx_stations.values_mut() {
            station.watchlisted = watchlisted.contains(&station.call_sign.to_uppercase());
        }
    }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p pancetta-tui apply_dx_watchlist -- --nocapture`
Expected: both new tests PASS.

Run: `cargo test -p pancetta-tui`
Expected: full suite PASSES (every existing `DxStation` literal now includes `watchlisted: false`, additive-only).

- [ ] **Step 7: Commit**

```bash
git add pancetta-tui/src/app.rs
git commit -m "feat(tui): add DxStation.watchlisted + bulk resync (#197)"
```

---

## Task 6: TUI message plumbing

**Files:**
- Modify: `pancetta-tui/src/tui_runner.rs` (new `TuiMessage` variant near line 91; handler near line 885)
- Modify: `pancetta/src/coordinator/tui_relay.rs` (relay `MessageType::DxWatchlistUpdate`, near line 600-627)

**Interfaces:**
- Consumes: `MessageType::DxWatchlistUpdate` (Task 4); `App::apply_dx_watchlist` (Task 5, already built).
- Produces: `TuiMessage::DxWatchlistUpdate { callsigns: Vec<String> }`.

- [ ] **Step 1: Add the `TuiMessage` variant**

In `pancetta-tui/src/tui_runner.rs`, add near the `TxPlacementUpdate` handling (the enum itself is defined starting at line 91 — add this variant near wherever `WaterfallUpdate` or other simple data variants sit, e.g. right before the closing of the enum):

```rust
    /// Currently-watchlisted callsigns (#197 DX watchlist). Full resync each
    /// time — `App::apply_dx_watchlist` bulk-replaces, never diffs.
    DxWatchlistUpdate { callsigns: Vec<String> },
```

- [ ] **Step 2: Handle it**

In `pancetta-tui/src/tui_runner.rs`, add a match arm right after the existing `TuiMessage::TxPlacementUpdate { view } => { app.apply_placement(view); }` (currently lines 885-887):

```rust
            TuiMessage::TxPlacementUpdate { view } => {
                app.apply_placement(view);
            }
            TuiMessage::DxWatchlistUpdate { callsigns } => {
                app.apply_dx_watchlist(&callsigns);
            }
        }
```

- [ ] **Step 3: Relay it from the coordinator side**

In `pancetta/src/coordinator/tui_relay.rs`, add a match arm right before the catch-all `_ => {}` (currently line 628), after the existing `MessageType::TxPlacementUpdate { ref snapshot } => { ... }` block:

```rust
                        MessageType::DxWatchlistUpdate { ref callsigns } => {
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::DxWatchlistUpdate {
                                    callsigns: callsigns.clone(),
                                },
                            );
                        }
                        _ => {}
```

- [ ] **Step 4: Build to confirm it compiles**

Run: `cargo build -p pancetta-tui -p pancetta`
Expected: builds cleanly — `App::apply_dx_watchlist` already exists from Task 5, so this task closes the loop with no deferred/expected-failure build step.

- [ ] **Step 5: Commit**

```bash
git add pancetta-tui/src/tui_runner.rs pancetta/src/coordinator/tui_relay.rs
git commit -m "feat(tui): relay DX watchlist bus updates into TuiMessage (#197)"
```

---

## Task 7: DX Hunter marker glyph

**Files:**
- Modify: `pancetta-tui/src/ui/dx_hunter.rs` — `create_dx_row`'s `call_display` construction (currently lines 173-183).

**Interfaces:**
- Consumes: `DxStation.watchlisted` (Task 5).

- [ ] **Step 1: Write the failing test**

In `pancetta-tui/src/ui/dx_hunter.rs`'s test module, add near `need_marker_priority_and_stacking`:

```rust
#[test]
fn watchlist_glyph_appears_only_when_watchlisted_and_not_atno() {
    // Mirrors the band_needed precedent: the glyph is additive to the
    // need_marker cluster, not folded into need_marker itself (keeps
    // need_marker's existing signature/tests untouched).
    let mut station = make_test_station("JA1ABC");
    station.watchlisted = true;
    let call_display = format_call_display(&station, false);
    assert!(call_display.contains('◇'), "expected watchlist glyph in {call_display}");
}

#[test]
fn watchlist_glyph_absent_when_not_watchlisted() {
    let station = make_test_station("W1XYZ");
    let call_display = format_call_display(&station, false);
    assert!(!call_display.contains('◇'));
}
```

These reference a small helper, `format_call_display`, factored out of `create_dx_row` so the glyph logic is unit-testable without constructing a full ratatui `Row`/`App`. Also add the fixture helper:

```rust
fn make_test_station(call: &str) -> crate::app::DxStation {
    crate::app::DxStation {
        call_sign: call.to_string(),
        grid_square: None,
        frequency: 14.074,
        mode: "FT8".to_string(),
        last_seen: chrono::Utc::now(),
        snr: -10,
        distance: None,
        bearing: None,
        worked_before: false,
        needed: false,
        atno: false,
        band_needed: false,
        priority_score: 0,
        source: crate::app::SpotSource::Local,
        entity_name: None,
        rarity_tier: None,
        reporter_count: None,
        is_notable: false,
        notable_type: None,
        confidence: None,
        best_snr_network: None,
        last_seen_network: None,
        audio_offset_hz: None,
        slot_parity: None,
        watchlisted: false,
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-tui watchlist_glyph -- --nocapture`
Expected: FAIL to compile — `format_call_display` doesn't exist yet.

- [ ] **Step 3: Factor out `format_call_display` and add the glyph**

In `pancetta-tui/src/ui/dx_hunter.rs`, replace the `call_display` construction inside `create_dx_row` (currently lines 173-183):

```rust
    let call_display = format_call_display(station, is_engaged);
```

And add the new function (e.g. right after `create_dx_row`, before `need_marker`):

```rust
/// Build the full callsign cell display: need/notable markers, band-needed
/// triangle, DX-watchlist diamond (#197), engaged dot, then the callsign.
/// Factored out of `create_dx_row` so the glyph logic is unit-testable
/// without a full ratatui `Row`.
fn format_call_display(station: &DxStation, is_engaged: bool) -> String {
    format!(
        "{}{}{}{}{}",
        need_marker(station.atno, station.needed, station.is_notable),
        if station.band_needed && !station.atno {
            "▲"
        } else {
            ""
        },
        if station.watchlisted { "◇" } else { "" },
        if is_engaged { "● " } else { "" },
        station.call_sign
    )
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta-tui watchlist_glyph -- --nocapture`
Expected: both PASS.

Run: `cargo test -p pancetta-tui`
Expected: full suite PASSES, including the existing `need_marker_priority_and_stacking` test (untouched — `need_marker`'s signature didn't change).

- [ ] **Step 5: Commit**

```bash
git add pancetta-tui/src/ui/dx_hunter.rs
git commit -m "feat(tui): show a diamond marker for watchlisted DX Hunter rows (#197)"
```

---

## Task 8: Full workspace verification

**Files:** none (verification only).

- [ ] **Step 1: Run the full workspace test suite**

Run: `cargo test --workspace --features transmit`
Expected: all tests PASS, no regressions.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --features transmit -- -D warnings`
Expected: clean (matches this repo's pre-push gate).

- [ ] **Step 3: Update the spec's open questions with the resolution made in Task 3**

In `docs/superpowers/specs/2026-07-24-dx-watchlist-design.md`, under "Open questions for planning", update the QSO-completion hook item to record the decision actually made (relying on TTL expiry rather than an explicit removal hook, since `AutonomousOperator` isn't currently reachable from the QSO-completion call site in `pancetta/src/coordinator/qso.rs` without new plumbing, and a stale watchlist entry has no side effect — it's inert bookkeeping, never a pounce trigger). Replace the first bullet under "Open questions for planning" with:

```markdown
- **Resolved during planning:** no explicit "QSO completed → `watchlist.remove(callsign)`" hook was
  added. `AutonomousOperator` (which owns the watchlist) isn't reachable from the QSO-completion
  call site (`pancetta/src/coordinator/qso.rs`'s `record_worked` call) without new cross-module
  plumbing, and a stale entry has no side effect — it never triggers a transmission, it's inert
  bookkeeping that self-clears via TTL (~2.5 min) either way. If on-air experience shows the DX
  Hunter marker lingering on a just-worked station is confusing, add the hook then.
```

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/2026-07-24-dx-watchlist-design.md
git commit -m "docs(specs): record DX watchlist QSO-completion-hook resolution (#197)"
```
