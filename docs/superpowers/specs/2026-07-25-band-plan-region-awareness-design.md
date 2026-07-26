# Global band-plan region-awareness — design

**Date:** 2026-07-25
**Status:** Implemented (2026-07-25). `pancetta_core::IaruRegion` +
`Band::frequency_range_for_region` added; the TX warning
(`tx_rf_out_of_band_plan`, `pancetta-tui`) now consumes `BandPlanConfig` region/custom_bands/
edge_warnings instead of the old hardcoded US-shaped check. Still a soft, once-per-session
warning — no TX blocking added.
**Scope crates:** `pancetta-core` (region-aware band data), `pancetta-config` (validation only —
no new fields needed), `pancetta-tui` (the warning check)

## 1. Motivation

The only TX-frequency safety check today is `pancetta_tui::app::tx_rf_out_of_us_band` — a
once-per-session soft warning (never a hard block, per this project's existing invariant) that
checks the TX frequency against `pancetta_core::Band::from_frequency`'s ranges. Those ranges are
generic-amateur-allocation shaped but in practice encode several **US-specific** band edges: 40m
extends to 7.3 MHz (IARU Region 1's 40m stops at 7.2 MHz — 7.2-7.3 MHz is the international
broadcast band there), 60m's 5.330-5.405 MHz is a US-specific channelized allocation (most other
countries have different or no 60m access), 80m's upper edge (4.0 MHz) is US-only (Region 1 stops
around 3.8 MHz), and 2m (144-148 MHz) / 70cm (420-450 MHz) both extend past Region 1's narrower
allocations (144-146 MHz / no 420-430 MHz secondary allocation outside the US in most countries).
A non-US operator running pancetta today gets zero warning for transmitting well outside their
actual regional band plan.

## 2. Existing groundwork (already exists, unconsumed — verified against current code)

`pancetta-config/src/rig.rs`'s `BandPlanConfig` (nested under `RigControlConfig.frequency.band_plan`)
already has exactly the shape this needs:

```rust
pub struct BandPlanConfig {
    pub region: u8,                              // already defaults to 2 (Americas)
    pub custom_bands: HashMap<String, BandDefinition>,
    pub edge_warnings: bool,                      // already defaults to true
}
pub struct BandDefinition { pub name: String, pub ranges: Vec<FrequencyRange> }
pub struct FrequencyRange { pub start: u64, pub end: u64, pub modes: Vec<String> }
```

`FrequencyConfig::merge_with` (the parent struct) is a blunt `*self = other` whole-struct
overwrite — not a field-by-field selective merge — so `band_plan` and its fields are already
correctly carried through config merge/hot-reload today. **No new config fields, and no
`merge_with` changes, are needed for this plan.** `region: 2` already matches the operator's own
US station out of the box.

## 3. Design

### 3.1 Region-aware band data (`pancetta-core`)

Add an `IaruRegion` type and per-region band segment overrides to `pancetta-core/src/types/band.rs`,
alongside the existing `Band` enum:

```rust
/// ITU/IARU amateur radio region. Numeric values match the ITU Region numbering
/// used by `BandPlanConfig.region` in pancetta-config (1/2/3), so the two stay
/// interchangeable at the config boundary without a translation table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IaruRegion {
    Region1, // Europe, Africa, Middle East, northern Asia (Russia)
    Region2, // The Americas
    Region3, // Asia-Pacific
}

impl IaruRegion {
    pub fn from_u8(n: u8) -> Option<Self> { ... } // 1/2/3 only
    pub fn to_u8(self) -> u8 { ... }
}
```

Extend `Band` with a region-aware range lookup. Only the bands with a REAL, documented regional
divergence get a region-specific override; everything else (WARC bands: 30m/17m/12m; bands with
no meaningful regional edge difference: 160m/20m/15m/10m) falls back to the existing
`frequency_range()` unchanged — this keeps the diff small and avoids inventing precision this
design doesn't need:

```rust
impl Band {
    /// Region-aware frequency range. Falls back to `frequency_range()` (today's
    /// single global table) for any band without a documented regional edge
    /// difference. Only 40m/60m/80m/2m/70cm have real per-region divergence.
    pub fn frequency_range_for_region(&self, region: IaruRegion) -> (u64, u64) {
        match (self, region) {
            (Band::Band40m, IaruRegion::Region1) => (7_000_000, 7_200_000),
            (Band::Band80m, IaruRegion::Region1) => (3_500_000, 3_800_000),
            (Band::Band60m, IaruRegion::Region1) => (5_351_500, 5_366_500), // WRC-15 channelized
            (Band::Band2m, IaruRegion::Region1) => (144_000_000, 146_000_000),
            (Band::Band70cm, IaruRegion::Region1) => (430_000_000, 440_000_000),
            // Region 3 (Asia-Pacific) generally mirrors Region 1's narrower edges
            // on these same bands (ITU-wide, not a US-style extended allocation) —
            // same overrides apply.
            (Band::Band40m, IaruRegion::Region3) => (7_000_000, 7_200_000),
            (Band::Band80m, IaruRegion::Region3) => (3_500_000, 3_900_000), // varies by country; use the ITU R3 core segment
            (Band::Band60m, IaruRegion::Region3) => (5_351_500, 5_366_500),
            (Band::Band2m, IaruRegion::Region3) => (144_000_000, 146_000_000),
            (Band::Band70cm, IaruRegion::Region3) => (430_000_000, 440_000_000),
            // Region 2 (Americas) and every other (band, region) pair use the
            // existing global table — it was effectively Region-2-shaped already.
            _ => self.frequency_range(),
        }
    }

    /// Region-aware band lookup — the region-aware analogue of `from_frequency`.
    pub fn from_frequency_for_region(freq: u64, region: IaruRegion) -> Option<Band> {
        Band::all().iter().copied().find(|b| {
            let (low, high) = b.frequency_range_for_region(region);
            freq >= low && freq <= high
        })
    }
}
```

Exact Region 1/3 segment edges should be re-verified against a current IARU band-plan reference at
implementation time (ham band plans are revised periodically) — the values above are this design's
best-effort baseline, not guaranteed current to the day of implementation. Document the source/date
in a code comment when landing.

### 3.2 Operator custom-band overrides (`custom_bands`, already-existing field)

If `BandPlanConfig.custom_bands` has an entry matching the band in question (keyed by the same
band name string `Band::Display` produces, e.g. `"40m"`), its `ranges` win over both the built-in
global AND region-specific table — this is the escape hatch for an operator whose national
allocation is narrower than their region's IARU default (e.g. a license class restriction).
Multiple `ranges` entries are OR'd (frequency is in-band if it falls in ANY listed range) — a
single band can have a fragmented allocation (e.g. a phone sub-band + a separate digital sub-band)
without a hard requirement to model mode-specific restrictions in this pass (the `modes: Vec<String>`
field on `FrequencyRange` exists for that but is out of scope here — FT8/FT4 are digital-mode
bands whose position within the band varies less by mode than voice/CW, so mode granularity isn't
needed for this project's purposes today).

### 3.3 The warning check (`pancetta-tui`)

Replace `tx_rf_out_of_us_band`'s body (keep the function name if renaming would touch too many
call sites for this scope — or rename to `tx_rf_out_of_band_plan` if it's a single, easily-updated
call site; check at implementation time) to:

1. Read `config.rig.frequency.band_plan` (verified against current `Config` struct: `Config.rig:
   RigConfig`, `RigConfig.frequency: FrequencyConfig`, `FrequencyConfig.band_plan: BandPlanConfig`).
2. If `band_plan.edge_warnings` is `false`, always return `false` (no warning) — wiring the
   existing toggle for the first time; today it's dead config, the warning always fires
   unconditionally regardless of this field's value.
3. Otherwise: check `custom_bands` first (§3.2), then `IaruRegion::from_u8(band_plan.region)`'s
   region-aware table (§3.1) — invalid/out-of-range `region` values fall back to the global table
   (today's behavior) rather than erroring, matching this project's fail-safe-to-current-behavior
   pattern.

## 4. Non-goals

- No hard TX block — stays a soft, once-per-session warning, preserving the existing invariant.
- No mode-specific (phone vs. digital vs. CW) sub-band modeling — FT8/FT4 operation doesn't need
  it for this pass.
- No UI for editing `custom_bands`/`region` from the TUI — config-file-only for this pass (matches
  how most `RigControlConfig` fields are configured today).
- No live IARU band-plan feed/auto-update — the region tables are static, hand-maintained code,
  same maintenance model as the existing `Band::frequency_range()` table.

## 5. Testing

- Unit: `IaruRegion::from_u8`/`to_u8` round-trip, rejects 0/4+.
- Unit: `frequency_range_for_region` for each of the 5 divergent bands × 3 regions, plus a
  non-divergent band (e.g. 20m) confirming it's identical across all 3 regions.
- Unit: `from_frequency_for_region` — a frequency inside Region 1's 40m (e.g. 7.15 MHz) resolves
  to `Band40m` under Region 1 but is correctly flagged out-of-band under a hypothetical
  Region-1-narrower check if it were e.g. 7.25 MHz.
- Unit: the warning check — `edge_warnings=false` suppresses regardless of frequency;
  `custom_bands` override wins over the region table; invalid `region` byte falls back to the
  global table without panicking.
- Existing `tx_rf_out_of_us_band_flags_only_out_of_band` test must still pass unchanged (Region 2
  is the default, and Region 2's table is unchanged from today's global table).
