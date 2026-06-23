# Remaining TODOs — Bug Fixes & Integration Gaps

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix 3 code bugs (POTA/SOTA false positives, band-blind dedup, unpopulated grid needed set) and verify the cqdx.io spots endpoint against the live API.

**Architecture:** These are 4 independent fixes — no ordering dependencies. Each touches a small, well-defined surface. The POTA/SOTA and dedup fixes are pure logic changes with existing test patterns to follow. The grid needed set requires wiring an existing method to the cqdx bridge. The cqdx.io spots verification requires a live API call.

**Tech Stack:** Rust, cargo test, pancetta-qso, pancetta-cqdx, pancetta (coordinator)

---

## File Map

| Task | Create | Modify | Test |
|------|--------|--------|------|
| 1. POTA/SOTA fix | — | `pancetta-qso/src/priority.rs` | `pancetta-qso/src/priority.rs` (inline tests) |
| 2. Band-aware dedup | — | `pancetta/src/priority_evaluator.rs`, `pancetta-qso/src/priority.rs` | `pancetta-qso/src/priority.rs` (inline), `pancetta/tests/loopback_qso.rs` |
| 3. Grid needed set | — | `pancetta/src/cqdx_bridge.rs`, `pancetta-cqdx/src/cache.rs` | `pancetta/src/cqdx_bridge.rs` (if testable), `pancetta-cqdx/src/cache.rs` |
| 4. cqdx.io spots envelope | — | `pancetta-cqdx/src/client.rs`, `pancetta-cqdx/src/types.rs` | `pancetta-cqdx/src/client.rs` (mock test update) |

---

### Task 1: Fix POTA/SOTA False Positives

**Files:**
- Modify: `pancetta-qso/src/priority.rs:111-123` — `is_pota_sota_candidate()`
- Test: `pancetta-qso/src/priority.rs:284-290` — `test_pota_sota_detection()`

**Context:** The current function returns `true` for any callsign containing `/`, including prefix-style calls like `VE3/W1ABC` (a US station operating from Canada — NOT a POTA activation). Only suffix-style portable indicators (`/P`, `/QRP`) should match.

- [ ] **Step 1: Update the test to assert correct behavior**

In `pancetta-qso/src/priority.rs`, find the `test_pota_sota_detection` test (around line 284) and replace it:

```rust
#[test]
fn test_pota_sota_detection() {
    // Portable suffixes — should match
    assert!(is_pota_sota_candidate("W1ABC/P"));
    assert!(is_pota_sota_candidate("K2DEF/QRP"));
    assert!(is_pota_sota_candidate("w1abc/p")); // case insensitive

    // Prefix-style calls — should NOT match
    assert!(!is_pota_sota_candidate("VE3/W1ABC"));  // operating from VE3
    assert!(!is_pota_sota_candidate("DL/K1ABC"));   // operating from Germany
    assert!(!is_pota_sota_candidate("F/W1ABC"));     // operating from France

    // Other suffixes — should NOT match
    assert!(!is_pota_sota_candidate("W1ABC/M"));     // mobile
    assert!(!is_pota_sota_candidate("W1ABC/MM"));    // maritime mobile
    assert!(!is_pota_sota_candidate("W1ABC/LGT"));   // lighthouse

    // Regular calls — should NOT match
    assert!(!is_pota_sota_candidate("W1ABC"));
    assert!(!is_pota_sota_candidate("K2DEF"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta-qso test_pota_sota_detection -- --exact`
Expected: FAIL — `VE3/W1ABC` currently returns `true` due to `contains('/')`

- [ ] **Step 3: Fix the implementation**

Replace the `is_pota_sota_candidate` function (line 111-123):

```rust
pub fn is_pota_sota_candidate(callsign: &str) -> bool {
    let upper = callsign.to_uppercase();
    upper.ends_with("/P") || upper.ends_with("/QRP")
}
```

Remove the broad `contains('/')` catch-all. Only explicit portable suffixes qualify.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pancetta-qso test_pota_sota_detection -- --exact`
Expected: PASS

- [ ] **Step 5: Run full pancetta-qso tests for regressions**

Run: `cargo test -p pancetta-qso`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add pancetta-qso/src/priority.rs
git commit -m "fix: POTA/SOTA detection — only match /P and /QRP suffixes

Removed broad contains('/') check that false-positived on prefix-style
calls like VE3/W1ABC (operating from Canada, not a POTA activation).
Now only explicit portable suffixes /P and /QRP are detected."
```

---

### Task 2: Band-Aware Duplicate Detection

**Files:**
- Modify: `pancetta-qso/src/priority.rs:54-56` — trait signature (no change needed, already has `freq_hz`)
- Modify: `pancetta/src/priority_evaluator.rs:17,51-60,119-124` — production implementation
- Test: `pancetta-qso/src/priority.rs:269-271,336-351` — test implementation + tests
- Test: `pancetta/tests/loopback_qso.rs:662-687` — integration test implementation

**Context:** `is_duplicate(callsign, freq_hz)` accepts a frequency parameter but ignores it. The `worked_on_band` field is a `HashSet<String>` keyed only by callsign. A station worked on 20m (14 MHz) should not be considered a duplicate on 40m (7 MHz).

- [ ] **Step 1: Add a frequency-to-band helper to priority_evaluator.rs**

In `pancetta/src/priority_evaluator.rs`, add this function before the `CachedStationLookup` impl block:

```rust
/// Map a frequency in Hz to its amateur band name.
/// Returns None for frequencies outside amateur bands.
fn freq_to_band(freq_hz: f64) -> Option<&'static str> {
    match freq_hz as u64 {
        1_800_000..=2_000_000 => Some("160m"),
        3_500_000..=4_000_000 => Some("80m"),
        5_330_000..=5_410_000 => Some("60m"),
        7_000_000..=7_300_000 => Some("40m"),
        10_100_000..=10_150_000 => Some("30m"),
        14_000_000..=14_350_000 => Some("20m"),
        18_068_000..=18_168_000 => Some("17m"),
        21_000_000..=21_450_000 => Some("15m"),
        24_890_000..=24_990_000 => Some("12m"),
        28_000_000..=29_700_000 => Some("10m"),
        50_000_000..=54_000_000 => Some("6m"),
        _ => None,
    }
}
```

- [ ] **Step 2: Write a unit test for freq_to_band**

Add below the function:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_freq_to_band() {
        assert_eq!(freq_to_band(14_074_000.0), Some("20m"));
        assert_eq!(freq_to_band(7_074_000.0), Some("40m"));
        assert_eq!(freq_to_band(21_074_000.0), Some("15m"));
        assert_eq!(freq_to_band(3_573_000.0), Some("80m"));
        assert_eq!(freq_to_band(1_840_000.0), Some("160m"));
        assert_eq!(freq_to_band(50_313_000.0), Some("6m"));
        assert_eq!(freq_to_band(100_000.0), None); // out of band
    }
}
```

- [ ] **Step 3: Run the test**

Run: `cargo test -p pancetta test_freq_to_band -- --exact`
Expected: PASS

- [ ] **Step 4: Change worked_on_band from HashSet<String> to HashMap<String, HashSet<String>>**

In `pancetta/src/priority_evaluator.rs`, change the field type (line 17):

```rust
// Before:
worked_on_band: Arc<RwLock<HashSet<String>>>,

// After:
worked_on_band: Arc<RwLock<HashMap<String, HashSet<String>>>>,
```

Update the `new()` constructor (around line 40):

```rust
// Before:
worked_on_band: Arc::new(RwLock::new(HashSet::new())),

// After:
worked_on_band: Arc::new(RwLock::new(HashMap::new())),
```

Add `HashMap` to the imports at the top of the file if not already present:

```rust
use std::collections::{HashMap, HashSet};
```

- [ ] **Step 5: Update seed_worked_from_list and update_worked_on_band**

Find `seed_worked_from_list` (around line 51) and update it. This method takes a list of callsigns for the current band, so it needs a band parameter:

```rust
pub fn seed_worked_from_list(&self, band: &str, callsigns: &[String]) {
    let mut worked = self.worked_on_band.write().unwrap();
    let band_set = worked.entry(band.to_string()).or_default();
    for cs in callsigns {
        band_set.insert(cs.to_uppercase());
    }
}
```

Find `update_worked_on_band` (around line 62) and update:

```rust
pub fn update_worked_on_band(&self, band: &str, callsign: &str) {
    let mut worked = self.worked_on_band.write().unwrap();
    worked.entry(band.to_string()).or_default().insert(callsign.to_uppercase());
}
```

- [ ] **Step 6: Update is_duplicate to use band**

Update the `is_duplicate` implementation (around line 119):

```rust
fn is_duplicate(&self, callsign: &str, freq_hz: f64) -> bool {
    let band = match freq_to_band(freq_hz) {
        Some(b) => b,
        None => return false, // unknown band = not a duplicate
    };
    let worked = self.worked_on_band.read().unwrap();
    worked
        .get(band)
        .map_or(false, |set| set.contains(&callsign.to_uppercase()))
}
```

- [ ] **Step 7: Fix callers of seed_worked_from_list and update_worked_on_band**

Search for all call sites of these methods and add the band parameter. Run:

```bash
cargo build -p pancetta 2>&1
```

Fix each compiler error by adding the band argument. The coordinator's QSO component (in `pancetta/src/coordinator/components.rs`) likely calls these — check the rig frequency context to determine the band string. If the rig frequency is available, use `freq_to_band(rig_freq)`. If not, derive from the config's default frequency.

- [ ] **Step 8: Run all tests**

Run: `cargo test -p pancetta-qso && cargo test -p pancetta --lib`
Expected: All pass (loopback test may need updating if it calls `seed_worked_from_list`)

- [ ] **Step 9: Update loopback test if needed**

If `pancetta/tests/loopback_qso.rs` has a `TestDupLookup` that implements `is_duplicate`, it's fine as-is (test implementations are independent). But check it compiles:

Run: `cargo test -p pancetta --test loopback_qso -- --list`
Expected: Lists tests without errors

- [ ] **Step 10: Commit**

```bash
git add pancetta/src/priority_evaluator.rs
git commit -m "feat: band-aware duplicate detection

is_duplicate now maps freq_hz to amateur band and checks duplicates
per-band. Working K9ZZ on 20m no longer suppresses K9ZZ on 40m."
```

---

### Task 3: Populate Grid Needed Set

**Files:**
- Modify: `pancetta/src/cqdx_bridge.rs:52-71` — `startup()` method
- Modify: `pancetta-cqdx/src/cache.rs` — check if grid needed data is available
- Test: `pancetta-cqdx/src/cache.rs` — existing tests

**Context:** `CachedStationLookup.needed_grids` is initialized empty and `update_needed_grids()` is never called. The cqdx.io API provides needed DXCC entities but grid needed data isn't currently available from the API. Since cqdx.io is first-party (owned by the developer), a new endpoint could be added, but for now we should populate grids from the QSO log — grids already worked are NOT needed.

The conservative fallback (`needed.is_empty() => treat everything as needed`) means this is low-urgency but still a gap.

- [ ] **Step 1: Check how worked grids are tracked**

Read `pancetta/src/coordinator/components.rs` and search for where QSOs are logged. When a QSO completes, the other station's grid is known. We can collect worked grids from the QSO database.

- [ ] **Step 2: Add a method to load worked grids from the QSO database**

In `pancetta/src/priority_evaluator.rs`, add:

```rust
pub fn seed_worked_grids(&self, grids: HashSet<String>) {
    self.update_needed_grids(grids);
}
```

Wait — the logic is inverted. `needed_grids` should contain grids we HAVEN'T worked. But the QSO database tells us what we HAVE worked. The current fallback "empty = everything needed" is actually correct behavior when we don't have a definitive "need" list.

The proper fix is to populate `needed_grids` from cqdx.io (which knows the user's confirmed grids). Since cqdx.io is first-party, this requires an API endpoint addition on the server side.

- [ ] **Step 3: Add a TODO comment documenting the dependency**

In `pancetta/src/cqdx_bridge.rs`, after the `update_needed_dxcc` call in `startup()`, add:

```rust
    // TODO: Populate needed_grids when cqdx.io adds a grid-needed endpoint.
    // Until then, the conservative fallback treats all grids as needed.
    // See: docs/cqdx-api-requirements.md
```

- [ ] **Step 4: Document the needed endpoint in cqdx-api-requirements.md**

In `docs/cqdx-api-requirements.md`, add a section for the grid needed endpoint:

```markdown
### `GET /api/v1/grids/needed`

Returns grid squares the user still needs for grid-chasing awards.

**Response:**
```json
{
  "grids": ["AA00", "AB01", ...]
}
```

**Priority:** Low — conservative fallback (treat all grids as needed) is acceptable until this endpoint exists.
```

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/cqdx_bridge.rs docs/cqdx-api-requirements.md
git commit -m "docs: document grid-needed API dependency

Grid needed set requires a cqdx.io endpoint that doesn't exist yet.
Added TODO in cqdx_bridge and endpoint spec in API requirements doc.
Conservative fallback (all grids needed) is correct until then."
```

---

### Task 4: Verify cqdx.io Spots Endpoint Envelope

**Files:**
- Modify: `pancetta-cqdx/src/client.rs:47-73` — `fetch_live_spots()` (potentially)
- Modify: `pancetta-cqdx/src/types.rs:88-91` — `LiveSpotsResponse` (potentially)
- Test: `pancetta-cqdx/src/client.rs:186-224` — mock test

**Context:** The client sends `GET /api/v1/spots?live=true` and expects `{ "groups": [...] }`. This has only been tested against mock responses, never the live API. Since cqdx.io is first-party, the developer can verify and fix either side.

- [ ] **Step 1: Write a manual verification script**

Create a temporary test that hits the live API. In `pancetta-cqdx/src/client.rs`, add an ignored test:

```rust
#[tokio::test]
#[ignore] // Run manually: cargo test -p pancetta-cqdx test_live_spots_envelope -- --ignored
async fn test_live_spots_envelope() {
    // Requires CQDX_TOKEN env var
    let token = std::env::var("CQDX_TOKEN")
        .expect("Set CQDX_TOKEN to run this test");
    let client = CqdxClient::new("https://cqdx.io", &token);

    // Try fetching live spots — this validates the real envelope
    match client.fetch_live_spots(Some("20m"), Some("FT8")).await {
        Ok(groups) => {
            println!("SUCCESS: Got {} spot groups", groups.len());
            for g in groups.iter().take(3) {
                println!("  {} on {} @ {} Hz (rarity: {:?})",
                    g.dx_call, g.band, g.frequency, g.rarity_rank);
            }
        }
        Err(e) => {
            panic!("FAILED: Spots endpoint returned error: {}", e);
        }
    }
}
```

- [ ] **Step 2: Run the verification**

Run: `CQDX_TOKEN=<your-token> cargo test -p pancetta-cqdx test_live_spots_envelope -- --ignored --nocapture`

If it passes: the envelope is correct, mark this TODO as resolved.
If it fails with a deserialization error: the envelope key is wrong — fix `LiveSpotsResponse` to match the actual response.
If it fails with 404: the endpoint doesn't exist yet on cqdx.io — document this.

- [ ] **Step 3: Fix or confirm based on result**

If the envelope key needs changing (e.g., `"spots"` instead of `"groups"`):

```rust
// types.rs — update LiveSpotsResponse
#[derive(Debug, Deserialize)]
pub struct LiveSpotsResponse {
    pub spots: Vec<SpotGroup>,  // was: groups
}
```

And update `fetch_live_spots` to use the correct field.

If the endpoint doesn't exist yet, add a note to `docs/cqdx-api-requirements.md` marking it as unimplemented.

- [ ] **Step 4: Update mock test to match reality**

Update the mock test in `client.rs` to use the verified envelope structure:

```rust
// Ensure mock matches actual API response shape
.set_body_json(serde_json::json!({
    "groups": [{ ... }]  // or "spots" if that's what the live API uses
}))
```

- [ ] **Step 5: Commit**

```bash
git add pancetta-cqdx/src/client.rs pancetta-cqdx/src/types.rs
git commit -m "fix: verify cqdx.io spots endpoint envelope against live API

Added ignored integration test for manual verification.
[Confirmed/Fixed] envelope key is [groups/spots]."
```
