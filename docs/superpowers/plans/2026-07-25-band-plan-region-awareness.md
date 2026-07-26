# Band-Plan Region-Awareness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the TX-outside-band-plan warning IARU-region-aware instead of US-shaped, using the
already-existing (but unconsumed) `BandPlanConfig` config surface.

**Architecture:** Add an `IaruRegion` type + per-region band-edge overrides to `pancetta-core`'s
`Band` (only for the 5 bands with real regional divergence: 40m/60m/80m/2m/70cm; everything else
falls back to the existing global table). Wire `pancetta_tui::app::tx_rf_out_of_us_band` to consult
`BandPlanConfig.region`/`custom_bands`/`edge_warnings` instead of the hardcoded global table. No
new config fields, no `merge_with` changes — the config surface already exists and is already
merge-safe (whole-struct overwrite).

**Tech Stack:** Rust workspace (existing).

**Spec:** `docs/superpowers/specs/2026-07-25-band-plan-region-awareness-design.md` — read it first.

## Global Constraints

- **No new config fields.** `BandPlanConfig.region: u8` (default 2), `.custom_bands`, and
  `.edge_warnings: bool` (default true) already exist and are already correctly merge-safe. Do not
  add new fields or touch `merge_with` unless you discover during implementation that this
  assumption was wrong — if so, STOP and report NEEDS_CONTEXT rather than proceeding with an
  undocumented new field (that would need the full config-merge-guardrail treatment this plan
  deliberately avoided scoping).
- **Stays a soft warning, never a hard TX block** — this is an existing project invariant, not a
  new decision. Do not add any blocking/rejection behavior.
- **Only 5 bands get region-specific overrides** (40m, 60m, 80m, 2m, 70cm) per the design's §3.1 —
  every other band falls back to the existing global `frequency_range()` table unchanged. Do not
  invent region overrides for bands the design doesn't call out.
- **Re-verify the exact IARU segment edges cited in the design doc against a current band-plan
  reference before landing** — the design doc explicitly flags its values as a best-effort
  baseline, not guaranteed current. If you have no way to verify against a live source in this
  environment, note that in your report and use the design doc's values as-is rather than guessing
  differently.
- Local `cargo fmt` + `cargo clippy` before each commit.
- Subagent rules (standing): implementers never push / never destructive git; controller pushes at
  batch boundaries.

---

## Task 1: `IaruRegion` type + region-aware `Band` methods

**Files:**
- Modify: `pancetta-core/src/types/band.rs` (add `IaruRegion` + `frequency_range_for_region` +
  `from_frequency_for_region` near the existing `Band` impl)
- Test: same file's `#[cfg(test)] mod tests`

**Interfaces:**
- Produces (exact shapes per the design doc §3.1):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IaruRegion {
    Region1,
    Region2,
    Region3,
}

impl IaruRegion {
    /// `1`/`2`/`3` only — matches `BandPlanConfig.region`'s numbering.
    pub fn from_u8(n: u8) -> Option<Self> {
        match n {
            1 => Some(Self::Region1),
            2 => Some(Self::Region2),
            3 => Some(Self::Region3),
            _ => None,
        }
    }
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Region1 => 1,
            Self::Region2 => 2,
            Self::Region3 => 3,
        }
    }
}
```

And on `impl Band`: `frequency_range_for_region(&self, region: IaruRegion) -> (u64, u64)` and
`from_frequency_for_region(freq: u64, region: IaruRegion) -> Option<Band>` — exact match-arm
values per the design doc §3.1's code block (copy those region-edge values verbatim as your
starting point, subject to the Global Constraint above about re-verifying them).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn iaru_region_from_u8_round_trips_and_rejects_invalid() {
    assert_eq!(IaruRegion::from_u8(1), Some(IaruRegion::Region1));
    assert_eq!(IaruRegion::from_u8(2), Some(IaruRegion::Region2));
    assert_eq!(IaruRegion::from_u8(3), Some(IaruRegion::Region3));
    assert_eq!(IaruRegion::from_u8(0), None);
    assert_eq!(IaruRegion::from_u8(4), None);
    for r in [IaruRegion::Region1, IaruRegion::Region2, IaruRegion::Region3] {
        assert_eq!(IaruRegion::from_u8(r.to_u8()), Some(r));
    }
}

#[test]
fn region_specific_bands_diverge_by_region() {
    // 40m: Region 1/3 stop at 7.2 MHz; Region 2 (the existing global table) goes to 7.3 MHz.
    assert_eq!(Band::Band40m.frequency_range_for_region(IaruRegion::Region1), (7_000_000, 7_200_000));
    assert_eq!(Band::Band40m.frequency_range_for_region(IaruRegion::Region2), Band::Band40m.frequency_range());
    assert_ne!(
        Band::Band40m.frequency_range_for_region(IaruRegion::Region1),
        Band::Band40m.frequency_range_for_region(IaruRegion::Region2),
    );
}

#[test]
fn non_divergent_bands_are_identical_across_all_regions() {
    // 20m has no documented regional edge difference -- must fall back to the
    // same global table for all three regions.
    let global = Band::Band20m.frequency_range();
    for region in [IaruRegion::Region1, IaruRegion::Region2, IaruRegion::Region3] {
        assert_eq!(Band::Band20m.frequency_range_for_region(region), global);
    }
}

#[test]
fn from_frequency_for_region_resolves_within_the_narrower_region1_40m_edge() {
    // 7.15 MHz is inside Region 1's 40m (7.0-7.2) -- resolves to Band40m there.
    assert_eq!(Band::from_frequency_for_region(7_150_000, IaruRegion::Region1), Some(Band::Band40m));
    // 7.25 MHz is inside Region 2's 40m (7.0-7.3) but OUTSIDE Region 1's (7.0-7.2)
    // -- must resolve to None under Region 1 (it's broadcast band there), while
    // still resolving to Band40m under Region 2.
    assert_eq!(Band::from_frequency_for_region(7_250_000, IaruRegion::Region1), None);
    assert_eq!(Band::from_frequency_for_region(7_250_000, IaruRegion::Region2), Some(Band::Band40m));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-core iaru --lib` and `cargo test -p pancetta-core region --lib`
Expected: FAIL — `IaruRegion` doesn't exist yet.

- [ ] **Step 3: Implement** `IaruRegion` + the two `Band` methods per the design doc §3.1. Add
  `use serde::{Deserialize, Serialize};` if not already imported in this file (check the existing
  `Band` enum's derives first — this file already derives `Serialize, Deserialize` on `Band`, so
  the import should already be present).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta-core --lib 2>&1 | tail -20`

- [ ] **Step 5: Commit**

```bash
git add pancetta-core/src/types/band.rs
git commit -m "feat(core): IaruRegion + region-aware Band frequency-range lookups"
```

---

## Task 2: Wire the TUI warning check to `BandPlanConfig`

**Files:**
- Modify: `pancetta-tui/src/app.rs` (`tx_rf_out_of_us_band`, currently line 3968 — re-verify)
- Test: `app.rs`'s existing test module (the existing
  `tx_rf_out_of_us_band_flags_only_out_of_band` test, line ~6349, must still pass unchanged)

**Interfaces:**
- Consumes: Task 1's `IaruRegion`/`Band::frequency_range_for_region`/`Band::from_frequency_for_region`.
- Changes the function's signature to take the band-plan config as a parameter (it currently takes
  only `tx_rf_hz: u64` — read the real current call site in `tui_runner.rs:1052` first to see what
  config access is actually available there, and thread through only what's needed; do not
  introduce a global/thread-local config read if the call site can pass the relevant
  `BandPlanConfig` fields directly).

- [ ] **Step 1: Read the real current call site** (`pancetta-tui/src/tui_runner.rs` around line
  1052, re-verify) to see what `app`/config access is already available there, so you know how to
  thread the band-plan config in without inventing new plumbing.

- [ ] **Step 2: Write the failing tests**

```rust
#[test]
fn tx_rf_out_of_band_plan_respects_edge_warnings_toggle() {
    let band_plan = pancetta_config::BandPlanConfig {
        region: 2,
        custom_bands: Default::default(),
        edge_warnings: false,
    };
    // 15 MHz is between bands (out of band under any region) -- but the
    // toggle being off must suppress the warning regardless.
    assert!(!tx_rf_out_of_band_plan(15_000_000, &band_plan));
}

#[test]
fn tx_rf_out_of_band_plan_is_region_aware() {
    let region1 = pancetta_config::BandPlanConfig {
        region: 1,
        custom_bands: Default::default(),
        edge_warnings: true,
    };
    let region2 = pancetta_config::BandPlanConfig {
        region: 2,
        custom_bands: Default::default(),
        edge_warnings: true,
    };
    // 7.25 MHz: in-band for Region 2 (US-shaped 40m), out-of-band for Region 1
    // (international broadcast band there).
    assert!(!tx_rf_out_of_band_plan(7_250_000, &region2));
    assert!(tx_rf_out_of_band_plan(7_250_000, &region1));
}

#[test]
fn tx_rf_out_of_band_plan_invalid_region_falls_back_to_global_table() {
    let bogus_region = pancetta_config::BandPlanConfig {
        region: 9, // invalid -- IaruRegion::from_u8 returns None
        custom_bands: Default::default(),
        edge_warnings: true,
    };
    // Must not panic; must fall back to the existing global-table behavior
    // (7.25 MHz is in-band under the global/Region-2-shaped table).
    assert!(!tx_rf_out_of_band_plan(7_250_000, &bogus_region));
}

#[test]
fn tx_rf_out_of_band_plan_custom_bands_override_wins() {
    use pancetta_config::{BandDefinition, FrequencyRange};
    let mut custom_bands = std::collections::HashMap::new();
    custom_bands.insert(
        "40m".to_string(),
        BandDefinition {
            name: "40m".to_string(),
            ranges: vec![FrequencyRange { start: 7_000_000, end: 7_100_000, modes: vec![] }],
        },
    );
    let with_override = pancetta_config::BandPlanConfig {
        region: 2, // Region 2's global 40m would allow 7.25 MHz, but the custom override doesn't
        custom_bands,
        edge_warnings: true,
    };
    assert!(tx_rf_out_of_band_plan(7_250_000, &with_override));
    assert!(!tx_rf_out_of_band_plan(7_050_000, &with_override));
}
```

(Adjust field names/types to match `pancetta_config`'s real current struct shapes if they differ
slightly from what's shown — check `pancetta-config/src/rig.rs`'s real `BandPlanConfig`/
`BandDefinition`/`FrequencyRange` definitions first.)

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p pancetta-tui out_of_band_plan --lib`
Expected: FAIL — function doesn't exist / old signature doesn't match.

- [ ] **Step 4: Implement.** Rename `tx_rf_out_of_us_band` to `tx_rf_out_of_band_plan` (matches the
  new region-aware semantics — it's no longer US-specific) taking `(tx_rf_hz: u64, band_plan:
  &pancetta_config::BandPlanConfig) -> bool`. Logic per the design doc §3.3:
  1. If `!band_plan.edge_warnings`, return `false`.
  2. Check `custom_bands` first — if the resolved `Band::from_frequency(tx_rf_hz)` (global table,
     just for naming which band we're in) has a matching entry in `custom_bands` (keyed by
     `Band::Display`'s string form, e.g. `"40m"`), the frequency is in-band only if it falls
     within ANY of that entry's `ranges` (OR semantics per the design's §3.2) — return the negation.
  3. Otherwise: `IaruRegion::from_u8(band_plan.region)`; if `None` (invalid), fall back to
     `Band::from_frequency(tx_rf_hz).is_none()` (today's exact behavior). If `Some(region)`, return
     `Band::from_frequency_for_region(tx_rf_hz, region).is_none()`.

- [ ] **Step 5: Update the call site** (`tui_runner.rs` ~line 1052) to pass the real
  `BandPlanConfig` from the loaded config instead of calling the old single-arg signature.

- [ ] **Step 6: Update the existing test's function name reference** — the existing
  `tx_rf_out_of_us_band_flags_only_out_of_band` test (line ~6349) calls the old function name; it
  must be updated to call the new name + pass a Region-2 `BandPlanConfig` (which reproduces the
  exact old global-table behavior, since Region 2 falls back to the unchanged global table) — its
  assertions should not need to change, only the call shape.

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p pancetta-tui --lib 2>&1 | tail -30`

- [ ] **Step 8: Full workspace check.**

Run: `cargo test --workspace --features transmit 2>&1 | tail -40`

- [ ] **Step 9: Commit**

```bash
git add pancetta-tui/src/app.rs pancetta-tui/src/tui_runner.rs
git commit -m "feat(tui): region-aware band-plan TX warning (was US-shaped), wires BandPlanConfig"
```

---

## Task 3: Docs + final gate

**Files:**
- Modify: `docs/superpowers/specs/2026-06-25-arbitrary-freq-split-design.md` (the line ~32-35
  reference to "unenforced `BandPlanConfig`" — update to note it's now wired)
- Modify: `docs/superpowers/specs/2026-07-25-band-plan-region-awareness-design.md` (status header:
  mark shipped)

- [ ] **Step 1: Update both status references.**
- [ ] **Step 2: Full workspace suite one final time:** `cargo test --workspace --features transmit`.
- [ ] **Step 3: `cargo fmt --check` + `cargo clippy --workspace --exclude pancetta-research
  --features transmit`.**
- [ ] **Step 4: Commit docs; controller pushes the batch and opens a PR.**

---

## Self-review notes (author)

- Spec coverage: §3.1 → Task 1; §3.2 + §3.3 → Task 2; §4 non-goals respected throughout (no hard
  block, no mode-specific modeling, no TUI editor, no live feed).
- Type consistency: `IaruRegion` (Task 1) consumed by Task 2's `tx_rf_out_of_band_plan`.
- Deliberately small scope: only 5 bands get region overrides, matching the design's explicit
  non-goal against inventing precision the project doesn't need.
