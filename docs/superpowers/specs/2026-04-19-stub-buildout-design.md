# Stub Buildout — Design Spec

## Goal

Complete all stub implementations identified in the full codebase audit. These are features that have scaffolded interfaces but no functional implementation — callers get fake data, silently discarded output, or hardcoded returns.

## Stubs to Build Out

### Tier 1: Core Functionality (blocks real-world use)

**1. Audio Output Pipeline** (`pancetta-audio`)
- `queue_output` discards TX audio; output stream plays silence
- Need: ring buffer wired from `queue_output` → output stream callback
- Impact: TX audio can't reach the radio without this

**2. Noise Reduction Filter** (`pancetta-dsp`)
- `NoiseReductionFilter::process_frame` is passthrough (now disabled by default)
- Need: FFT-based spectral subtraction using already-allocated workspace
- Impact: No noise reduction available for weak signal work

**3. Spot Frequency Reporting** (`coordinator`)
- Frequency offset sent as 0 (was wrong absolute value, now zeroed)
- Need: thread operating frequency from hamlib through to spot reports
- Impact: cqdx.io spots have no frequency data

### Tier 2: DX Tracking (needed for autonomous operator intelligence)

**4. DXCC is_needed** (`pancetta-dx`)
- Always returns true — no award tracking consultation
- Need: query award_tracking table for band/mode-specific worked status
- Impact: Autonomous operator can't distinguish needed vs worked entities

**5. Band/Mode Statistics** (`pancetta-dx`)
- Return zeros / errors
- Need: real SQL queries against QSO database
- Impact: No activity statistics for display or decisions

**6. DXCC needed_entities in AwardSummary** (`pancetta-dx`)
- Hardcoded to empty Vec
- Need: compute from award_tracking vs entity database
- Impact: DX hunting panel can't show what's needed

**7. CTY.DAT Parser** (`pancetta-dx`)
- `parse_cty_line` always returns error
- Need: implement standard CTY.DAT format parser
- Impact: Can't load full DXCC database from CTY.DAT files

**8. Worked Station Persistence** (`coordinator`)
- `worked_on_band` not seeded from DB on startup
- Need: load historical worked callsigns from QSO database at init
- Impact: Restart forgets all worked stations, re-calls duplicates

### Tier 3: Network Services (enhances but not blocking)

**9. LoTW Upload Response Parser** (`pancetta-dx`)
- Returns `accepted: None, rejected: None` always
- Need: parse LoTW HTML response for upload confirmation
- Impact: Can't verify LoTW uploads succeeded

**10. Remote Config Loading** (`pancetta-config`)
- Returns error (was returning defaults, now properly errors)
- Need: HTTP fetch + TOML/JSON parse if remote config is desired
- Impact: Can't load config from URL (low priority)

### Tier 4: UI Polish (nice to have)

**11. Help Panel** (`pancetta-tui`)
- `toggle_help()` shows "not yet implemented" in status bar
- Need: overlay panel with key bindings and feature summary
- Impact: Users can't discover keyboard shortcuts

**12. Mouse Event Handling** (`pancetta-tui`)
- `handle_mouse_event` is a no-op
- Need: scroll wheel support, click-to-select in band activity/DX hunter
- Impact: Mouse users get no interaction

**13. Hamlib Stubs** (`pancetta-hamlib`)
- Antenna get/set/list, clear_memory_channel, hamlib_params all hardcoded
- Need: actual rigctld command calls
- Impact: Advanced rig control features don't work

## Recommended Implementation Order

1. **Audio Output Pipeline** — blocks TX functionality
2. **Spot Frequency Reporting** — simple wiring, high data quality impact
3. **Worked Station Persistence** — prevents duplicate QSOs after restart
4. **DXCC is_needed + needed_entities** — enables smart DX hunting
5. **Band/Mode Statistics** — enables activity tracking display
6. **CTY.DAT Parser** — completes DXCC database
7. **Noise Reduction Filter** — enhances weak signal reception
8. **Help Panel** — user discoverability
9. **Mouse Events** — UI polish
10. **LoTW Response Parser** — network service reliability
11. **Remote Config** — low priority
12. **Hamlib Stubs** — advanced rig control

## Architecture Notes

- Tier 1 items should each get their own spec + plan cycle — they're substantial features
- Tier 2 items can be grouped into a single "DX tracking buildout" spec
- Tier 3-4 items are independent and can be done as standalone tasks
- The audio output pipeline is the most architecturally significant — it needs ring buffer design, callback threading, and format conversion
