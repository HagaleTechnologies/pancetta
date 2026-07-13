# Onboarding Phase 2: Docs Tell the Truth — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Zero config keys documented-but-ignored or wired-but-undocumented; one source of truth for keybindings, enforced by a drift test; every operator-facing doc (CONFIG, RUNBOOK, FEATURES, README, defaults.toml) states only things the code actually does. Spec: `docs/superpowers/specs/2026-07-12-onboarding-world-class-design.md` §Phase 2.

**Architecture:** Four doc-only truth passes first (CONFIG.md `[autonomous]`, RUNBOOK, FEATURES, docs index), then four code-touching tasks: wire `[duplicate_checking]` into `pancetta-config` and the coordinator (defaults identical to today's hard-coded values — zero behavior change), a post-parse unknown-top-level-key warning riding the existing `load_warnings` mechanism, a static keybinding table in `pancetta-tui` that renders the `?` overlay AND generates `docs/KEYBINDINGS.md` under a drift test, and a drift-tested regeneration of `pancetta-config/defaults.toml` from `Config::default()`. No new crates, no new dependencies.

**Tech Stack:** Rust stable, existing workspace. Drift tests follow the existing `merge_with` guard pattern (`pancetta-config/src/lib.rs:852-912`, module `merge_guard` at `lib.rs:676`): schema drift fails a test, not a human. `Config` already derives `Serialize` (`pancetta-config/src/lib.rs:122`), so TOML regeneration is a serde round-trip.

## Global Constraints

- Every commit passes `cargo fmt --all` + `cargo clippy --workspace --features transmit` clean before committing.
- Doc regeneration must be drift-TESTED, not manual, wherever feasible (KEYBINDINGS.md, defaults.toml, the duplicate-checking defaults-parity check). A generated file always carries a "GENERATED — do not edit" header with the regen command.
- **No behavior changes to TX paths.** Nothing in this plan touches the TX scheduler, armed-TX gate, parity logic, or coalesce windows.
- **`[duplicate_checking]` wiring must preserve current hard-coded defaults when the section is absent** — `pancetta-config` defaults must equal `DuplicateCheckConfig::default()` (`pancetta-qso/src/qso_manager.rs:418-427`: `enabled=true, time_window_hours=24, check_frequency=true, check_band=false`), guarded by a cross-crate test. Zero behavior change for every existing user.
- Line numbers below were verified against this branch (`onboarding-plans-2026-07-12c`). Phase 1 (`docs/superpowers/plans/2026-07-12-onboarding-phase1-funnel.md`) edits README.md and CLAUDE.md; if it lands first, re-verify README line numbers before editing (anchor on headings, not raw numbers).
- Commit messages follow repo convention (`fix:`/`feat:`/`docs:`/`test:` prefixes, imperative).

---

### Task 1: CONFIG.md — regenerate the `[autonomous]` section from the real schema

**Files:**
- Modify: `docs/CONFIG.md:127-166` (the `[autonomous_operator]` + `[priority_weights]` sections) and `docs/CONFIG.md:42-43`

**Interfaces:**
- Consumes (read-only, as the source of truth): `AutonomousConfig` (`pancetta-config/src/autonomous.rs:196-230`), its `Default` (`autonomous.rs:232-250`), `PriorityWeightsConfig` (`autonomous.rs:93-117`) with defaults (`autonomous.rs:123-136`), `SlotParitySetting` (`autonomous.rs:10-17`, `rename_all = "lowercase"`), validation ranges (`autonomous.rs:252-286`).
- Produces: a CONFIG.md section Task 6's unknown-key warning can point users at. Leaves `[duplicate_checking]` (CONFIG.md:168-181) untouched — Task 5 owns it, because its truth changes when the wiring lands.

The current doc is wrong four ways: section name `[autonomous_operator]` (real: `[autonomous]`, `pancetta-config/src/lib.rs:145-146`), a `mode = "hybrid"` key that has never existed in `AutonomousConfig`, `slot_parity_preference` (real key: `slot_parity`), and a top-level `[priority_weights]` with `snr` (real: `[autonomous.priorities]` with `signal_strength`, `autonomous.rs:91-103`).

- [ ] **Step 1: Replace CONFIG.md lines 127-166 wholesale**

Replace from the `## [autonomous_operator] — the brain` heading (line 127) through the end of the priority-weights prose (line 166, just before `### [duplicate_checking]` at 168) with:

`````markdown
## `[autonomous]` — the brain

```toml
[autonomous]
enabled = false            # Master enable. Off by default; opt-in to TX.
slot_parity = "auto"       # "even", "odd", or "auto"
cq_after_idle_cycles = 10  # Idle TX cycles before calling CQ (~150 s at 10)
max_concurrent_qsos = 1    # Cap on simultaneous in-flight QSOs
tx_offset_hz = 1500.0      # Preferred TX audio offset (100–3000 Hz)
min_dx_score = 0.3         # Minimum DX score (0–1) to answer a CQ
min_multi_slot_score = 0.7 # Higher bar (0–1) for opening a 2nd+ concurrent QSO
cq_direction = ""          # Directed CQ text ("DX", "NA", …); empty = general CQ
dry_run = false            # Log autonomous TX decisions without keying the rig
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `false` | When false, the autonomous engine never initiates TX. |
| `slot_parity` | enum | `"auto"` | FT8 alternates even/odd 15 s slots; `auto` picks per conditions. |
| `cq_after_idle_cycles` | integer | `10` | TX cycles with nothing to do before calling CQ. Must be ≥ 1. |
| `max_concurrent_qsos` | integer | `1` | Simultaneous in-flight QSOs (multi-stream TX). Must be ≥ 1. |
| `tx_offset_hz` | float | `1500.0` | Validated to 100–3000 Hz. |
| `min_dx_score` | float | `0.3` | 0.0–1.0. Decoded CQs scoring below this are not answered. |
| `min_multi_slot_score` | float | `0.7` | 0.0–1.0. Applies only to second-and-later concurrent QSOs. |
| `cq_direction` | string | `""` | Appended to CQ (`CQ DX <call> <grid>`). |
| `dry_run` | bool | `false` | Autonomous TransmitRequests are logged, not sent. Manual TX unaffected. |

> **There is no `mode` key.** Earlier revisions of this document described
> `[autonomous_operator]` with `mode = "hunt" / "cq" / "hybrid"`,
> `slot_parity_preference`, and a top-level `[priority_weights]`. Those keys
> never existed in the code and were silently ignored. The real behavior is
> always both: answer scored CQs above `min_dx_score`, and fall back to
> calling CQ after `cq_after_idle_cycles` idle cycles. Startup now warns
> about unknown top-level sections, so a stale config will tell you.

### `[autonomous.priorities]` — what to prioritize

Each decoded CQ is scored against these weights (each validated to
−1.0…1.0; positive attracts, negative penalizes; the final score is
clamped to 0.0–1.0 and compared against `min_dx_score`).

```toml
[autonomous.priorities]
needed_dxcc            = 0.35
needed_grid            = 0.20
pota_sota              = 0.15
rarity                 = 0.10
signal_strength        = 0.05   # SNR weight — stronger = more likely to complete
duplicate_penalty      = -0.40  # already worked on this band
recent_failure_penalty = -0.15  # recently called, QSO didn't complete
atno_bonus             = 0.15   # extra premium on top of needed_dxcc for an
                                # all-time-new-one; inert unless cqdx.io flags it
```

### Sub-tables you'll rarely touch

- `[autonomous.frequency]` — the `SmartFrequencyAllocator` knobs (center
  bias, DX-proximity window, own-QSO separation, neighbor guard).
- `[autonomous.listen_cycle]` — adaptive forced-listen-slot cadence for
  collision detection.
- `[autonomous.band_hopping]` — off by default; ordered band list with a
  low-activity hop threshold.

Defaults for all three live in `Config::default()`; see
`pancetta-config/src/autonomous.rs` for every field with doc comments.
`````

- [ ] **Step 2: Fix the "minimum viable config" claim (CONFIG.md:42-43)**

Replace "That's enough to run the autonomous operator." with:

```markdown
That's enough to decode and work stations manually. Hands-off operation
additionally needs `[autonomous] enabled = true` (see below).
```

- [ ] **Step 3: Verify no stale key names survive anywhere in the repo's docs**

```bash
grep -rn "autonomous_operator\|slot_parity_preference\|priority_weights" docs/ README.md FEATURES.md CLAUDE.md
```

Expected: no matches (the only historical mentions may remain inside `docs/superpowers/specs/2026-07-12-onboarding-world-class-design.md` and this plan, which quote the bug — those are fine; anything else must be fixed).

- [ ] **Step 4: Sanity-check the documented schema actually parses**

The doc snippet mirrors `test_dry_run_toml_roundtrip_true` at `autonomous.rs:380-427`, which already proves these exact keys parse:

```bash
cargo test -p pancetta-config --lib 2>&1 | tail -3
```

Expected: existing config tests all pass.

- [ ] **Step 5: Commit**

```bash
git add docs/CONFIG.md
git commit -m "docs(config): regenerate [autonomous] section from the real schema — kill autonomous_operator/mode/priority_weights ghosts"
```

---

### Task 2: RUNBOOK.md truth pass

**Files:**
- Modify: `docs/RUNBOOK.md:5-6` (config filename), `docs/RUNBOOK.md:24-26` (autonomous "config-only" claim), `docs/RUNBOOK.md:249-256` (log paths)

**Interfaces:**
- Consumes (verify before writing): the `a` and `Shift+P` handlers (`pancetta-tui/src/tui_runner.rs:1512`, `tui_runner.rs:1526`); the logging setup (`pancetta/src/main.rs:1286-1302`: directory `~/.pancetta/logs/`, `tracing_appender` daily rotation with `filename_prefix("pancetta.log")` → files named `pancetta.log.YYYY-MM-DD`, retention 14 files).

- [ ] **Step 1: Verify the runtime autonomous toggle exists**

```bash
sed -n '1512,1534p' pancetta-tui/src/tui_runner.rs
```

Expected: `KeyCode::Char('a')` toggles autonomous (sends a TuiCommand) and `KeyCode::Char('P')` pauses/resumes. If either is absent, stop and re-diagnose before editing the RUNBOOK.

- [ ] **Step 2: Fix the header config path (RUNBOOK.md:5-6)**

Change "config lives at `~/.pancetta/config.toml`" to "config lives at `~/.pancetta/pancetta.toml`" (the wizard writes `pancetta.toml`; `config.toml` is only a legacy fallback name in the loader's search list, `pancetta-config/src/loader.rs:345-350`).

- [ ] **Step 3: Fix the "config-only" autonomous claim (RUNBOOK.md:24-26)**

Replace:

```markdown
The autonomous-mode toggle is **config-only** — there is no runtime
key binding to enable or disable it. To switch in or out of autonomous
mode, edit config and restart pancetta.
```

with:

```markdown
`[autonomous].enabled` sets the **startup** state. At runtime, the TUI
toggles autonomous mode live with `a` and pauses/resumes it with
`Shift+P` (no restart needed); `Shift+Q` is the emergency stop (halts
TX and forces autonomous off). Headless runs have no runtime toggle —
for a supervised headless station, config is the only switch.
```

- [ ] **Step 4: Fix the log paths (RUNBOOK.md:249-256)**

The real layout (`pancetta/src/main.rs:1286-1302`) is `~/.pancetta/logs/` (plural) with daily-rotated files `pancetta.log.YYYY-MM-DD`, capped at 14 files. Replace the two command blocks:

```bash
ls -lt ~/.pancetta/logs/ | head -5
```

```bash
grep -E "WARN|ERROR" ~/.pancetta/logs/pancetta.log.$(date -u +%Y-%m-%d) | head -50
```

and add one sentence after them: "Logs rotate daily (UTC) and pancetta keeps the newest 14 files."

- [ ] **Step 5: Verify no stale paths remain**

```bash
grep -n "config.toml\|\.pancetta/log/\|log/\$(date" docs/RUNBOOK.md
```

Expected: no matches.

- [ ] **Step 6: Commit**

```bash
git add docs/RUNBOOK.md
git commit -m "docs(runbook): fix autonomous-toggle claim (a key is live), real log paths, pancetta.toml naming"
```

---

### Task 3: FEATURES.md corrections

**Files:**
- Modify: `FEATURES.md:13` (Autonomous Operator paragraph), `FEATURES.md:37` (device-picker key)

**Interfaces:**
- Consumes: the real `AutonomousConfig` schema (Task 1's source), and the device-picker handler `KeyCode::Char('d')` at `pancetta-tui/src/tui_runner.rs:1337` (`D` — Shift+D — is the diagnostics overlay, `tui_runner.rs:1373`).

- [ ] **Step 1: Rewrite the Autonomous Operator paragraph (FEATURES.md:13)**

The current text claims "three modes: hunt mode …, CQ mode …, and hybrid mode" and "mode, aggressiveness, slot preference … set at runtime". No mode or aggressiveness knob exists. Replace the paragraph body with:

```markdown
The autonomous operator makes cycle-by-cycle decisions: it answers decoded
CQs whose priority score clears `min_dx_score` (pouncing on rare or needed
stations first), answers stations calling it directly, and falls back to
calling CQ after a configurable number of idle TX cycles
(`cq_after_idle_cycles`). It manages even/odd 15-second slot parity, drives
the full QSO state machine (CQ → grid report → signal report → RR73 → 73),
and monitors the TX slot to detect doubling. Slot parity, concurrency,
CQ cadence, and all priority weights are configured under `[autonomous]`
in `pancetta.toml` (see docs/CONFIG.md) — no code changes required.
```

- [ ] **Step 2: Fix the device-picker key (FEATURES.md:37)**

Change "`D` for the audio device picker" to "`d` for the audio device picker".

- [ ] **Step 3: Verify**

```bash
grep -n "hybrid\|hunt mode\|aggressiveness\|\`D\` for" FEATURES.md
```

Expected: no matches.

- [ ] **Step 4: Commit**

```bash
git add FEATURES.md
git commit -m "docs(features): remove nonexistent hunt/cq/hybrid autonomous modes; device picker is d not D"
```

---

### Task 4: `docs/README.md` index — curated docs vs. working notes

**Files:**
- Create: `docs/README.md`

**Interfaces:**
- Consumes: the current contents of `docs/` (verified this branch: ARCHITECTURE.md, CONFIG.md, RUNBOOK.md, DECISIONS/ (8 files), fcc-part97-compliance.md, decoder-comparison.md, cqdx-api-requirements.md, plus ~12 analysis/plan/audit docs and the archive/, engineering/, operations/, superpowers/ directories).
- Produces: an index Task 7 appends one row to (KEYBINDINGS.md). **Move nothing; label only.**

- [ ] **Step 1: Write `docs/README.md`**

```markdown
# Pancetta documentation index

Two kinds of documents live here. **Curated docs** are maintained,
current, and safe to trust; **working notes** are point-in-time design
and analysis artifacts — accurate when written, not maintained since.
When a working note disagrees with a curated doc or the code, the
working note loses.

## Curated — read these

| Doc | What it is |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Crate relationships, component diagram, channel topology |
| [CONFIG.md](CONFIG.md) | Configuration reference for `~/.pancetta/pancetta.toml` |
| [RUNBOOK.md](RUNBOOK.md) | Operator procedures: modes, supervision, Phase 5 checklist |
| [DECISIONS/](DECISIONS/) | Decision digests by subsystem (tx-scheduling, qso-engine, modes, remote-operation, tui, logging-uploads, config-and-platform) + ADR-001 |
| [fcc-part97-compliance.md](fcc-part97-compliance.md) | Regulatory / TX-safety notes |
| [decoder-comparison.md](decoder-comparison.md) | Pancetta decoder vs. peer FT8 decoders |
| [cqdx-api-requirements.md](cqdx-api-requirements.md) | Cross-repo API contract with cqdx.io |

## Working notes / point-in-time (unmaintained)

Design plans, audits, and analyses, kept for context:

- Decoder: `decoder-analysis.md`, `ap-decoding-design.md`, `neural-osd-design.md`
- QSO engine: `qso-state-machine-analysis.md`, `qso-engine-bugs.md`, `qso-scenario-catalog-2026-06-16.md`
- Platform: `audio-robustness-plan.md`, `observability-diagnostics-plan.md`, `task-supervision-plan.md`
- Audits: `ux-audit-2026-06-14.md`, `security-review-2026-04-29.md`, `security-deep-analysis-2026-06-17.md`, `security-review-remote-rig-2026-07-02.md`
- `engineering/` — methodology audits; `operations/` — one-off procedures

## Process directories

- `superpowers/specs/` + `superpowers/plans/` — design specs (authoritative for current behavior) and their implementation plans
- `archive/` — superseded plans, kept for history
```

- [ ] **Step 2: Verify every linked file exists**

```bash
cd docs && for f in ARCHITECTURE.md CONFIG.md RUNBOOK.md fcc-part97-compliance.md decoder-comparison.md cqdx-api-requirements.md DECISIONS; do test -e "$f" || echo "MISSING: $f"; done
```

Expected: no output.

- [ ] **Step 3: Commit**

```bash
git add docs/README.md
git commit -m "docs: add docs/README.md index separating curated docs from working notes"
```

---

### Task 5: Wire `[duplicate_checking]` into config (zero behavior change)

**Files:**
- Create: `pancetta-config/src/duplicate_check.rs`
- Modify: `pancetta-config/src/lib.rs` (module decl at 58-68, `Config` struct at 122-167, `Default` at 185-206, `validate` at 235-252, `merge_with` at 255-276, merge-guard test at 852-912)
- Modify: `pancetta/src/coordinator/qso.rs` (config snapshot ~1027, import at 1090, `QsoManagerConfig` construction at 1092-1103, defaults-parity test in the existing test module at ~3872)
- Modify: `docs/CONFIG.md:168-181` and `README.md:214-220` (reconcile docs with wired reality)

**Interfaces:**
- Consumes: `pancetta_qso::DuplicateCheckConfig` (`pancetta-qso/src/qso_manager.rs:313-326`, re-exported via `pub use crate::qso_manager::*` at `pancetta-qso/src/lib.rs:131`) and its defaults (`qso_manager.rs:418-427`). The coordinator currently builds `QsoManagerConfig` with `..Default::default()` (`pancetta/src/coordinator/qso.rs:1102`), so `duplicate_checking` is always the hard-coded default today.
- Produces: `pancetta_config::duplicate_check::DuplicateCheckingConfig` threaded into the coordinator following the exact pattern used for Hound regions (`qso.rs:1023-1027` snapshot → field-by-field copy at 1095-1100, deliberately avoiding a `pancetta-qso → pancetta-config` dependency).
- Deliberate scope cut: `check_band` (`qso_manager.rs:325`) is defined but **never read** anywhere in `pancetta-qso` (only `check_frequency` is consulted, `qso_manager.rs:3139`). Do NOT expose a dead knob in the new config schema — that's the disease this phase cures. The coordinator fills it from the qso-side default.

- [ ] **Step 1: Write the failing cross-crate defaults-parity test**

In the existing `#[cfg(test)]` module of `pancetta/src/coordinator/qso.rs` (around line 3872, which already imports `pancetta_qso::{QsoManager, QsoManagerConfig}`):

```rust
    /// Guard for the [duplicate_checking] wiring: the pancetta-config defaults
    /// must equal pancetta-qso's hard-coded DuplicateCheckConfig::default(),
    /// so a config file WITHOUT the section produces byte-identical behavior
    /// to the pre-wiring binary. If either side changes, this fails.
    #[test]
    fn config_duplicate_defaults_match_qso_manager_defaults() {
        let c = pancetta_config::Config::default().duplicate_checking;
        let q = pancetta_qso::DuplicateCheckConfig::default();
        assert_eq!(c.enabled, q.enabled);
        assert_eq!(c.time_window_hours, q.time_window_hours);
        assert_eq!(c.check_frequency, q.check_frequency);
    }
```

```bash
cargo test -p pancetta --lib config_duplicate_defaults 2>&1 | tail -3
```

Expected: FAIL — `Config` has no field `duplicate_checking`.

- [ ] **Step 2: Create `pancetta-config/src/duplicate_check.rs`**

```rust
//! Duplicate-QSO checking configuration.
//!
//! Corresponds to the `[duplicate_checking]` section in the TOML config file.
//! Threaded by the coordinator into `pancetta_qso::QsoManagerConfig` — the
//! defaults here MUST match `pancetta_qso::DuplicateCheckConfig::default()`
//! (guarded by `config_duplicate_defaults_match_qso_manager_defaults` in the
//! `pancetta` crate) so an absent section changes nothing.

use crate::{ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};

/// Duplicate-QSO checking: refuse to call a station already worked recently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateCheckingConfig {
    /// Enable duplicate checking. When false, pancetta will happily call the
    /// same station again immediately.
    pub enabled: bool,
    /// A prior QSO only counts as a duplicate if it started within this many
    /// hours.
    pub time_window_hours: u32,
    /// When true, a prior QSO only blocks a re-call if it was within 50 Hz of
    /// the same RF frequency (so the same station on another band — or after a
    /// substantial QSY — can be worked again). When false, any QSO with that
    /// callsign inside the window blocks.
    pub check_frequency: bool,
}

impl Default for DuplicateCheckingConfig {
    fn default() -> Self {
        // MUST mirror pancetta-qso qso_manager.rs DuplicateCheckConfig::default().
        Self {
            enabled: true,
            time_window_hours: 24,
            check_frequency: true,
        }
    }
}

impl ConfigSection for DuplicateCheckingConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        // All fields are bools or an inherently bounded u32; a 0-hour window
        // is coherent (nothing is ever inside it → checking effectively off).
        Ok(())
    }

    fn merge_with(&mut self, other: Self) {
        self.enabled = other.enabled;
        self.time_window_hours = other.time_window_hours;
        self.check_frequency = other.check_frequency;
    }
}
```

- [ ] **Step 3: Register the section in `pancetta-config/src/lib.rs`**

Four mechanical edits (each site listed with its current line):

1. Module list (after `pub mod decoder;` at line 60): add `pub mod duplicate_check;`
2. `Config` struct (after the `decoder` field at lines 160-162):

```rust
    /// Duplicate-QSO checking (don't call the same station twice)
    #[serde(default)]
    pub duplicate_checking: duplicate_check::DuplicateCheckingConfig,
```

3. `Default for Config` (after `decoder: decoder::DecoderConfig::default(),` at line 197): add `duplicate_checking: duplicate_check::DuplicateCheckingConfig::default(),`
4. `validate()` (after `self.decoder.validate_section()?;` at line 248): add `self.duplicate_checking.validate_section()?;` — and `merge_with()` (after line 267): add `self.duplicate_checking.merge_with(other.duplicate_checking);`

Then extend the merge-guard test (`merge_with_carries_every_field`, `lib.rs:852-912`) — add alongside the other sections:

```rust
        assert_carries_all::<duplicate_check::DuplicateCheckingConfig>(
            "DuplicateCheckingConfig",
            &[],
            |a, b| a.merge_with(b),
        );
```

- [ ] **Step 4: Thread it into the coordinator**

In `pancetta/src/coordinator/qso.rs`, add to the config-snapshot block (immediately after `let hound_cfg = config.hound.clone();` at line 1027):

```rust
        // Operator-configured duplicate-QSO checking. Copied field-by-field
        // into pancetta_qso::DuplicateCheckConfig (same pattern as HoundRegions
        // above) to avoid a pancetta-qso → pancetta-config dependency. The
        // config-side defaults equal the qso-side defaults (guard test:
        // config_duplicate_defaults_match_qso_manager_defaults), so a config
        // without the section behaves exactly like the pre-wiring binary.
        let dup_cfg = config.duplicate_checking.clone();
```

Extend the import at line 1090:

```rust
                use pancetta_qso::{
                    DuplicateCheckConfig, HoundRegions, LoggerConfig, QsoManager, QsoManagerConfig,
                };
```

And in the `QsoManagerConfig` literal (lines 1092-1103), add the field above `..Default::default()`:

```rust
                    duplicate_checking: DuplicateCheckConfig {
                        enabled: dup_cfg.enabled,
                        time_window_hours: dup_cfg.time_window_hours,
                        check_frequency: dup_cfg.check_frequency,
                        // check_band is defined but unread in pancetta-qso;
                        // keep the qso-side default rather than exposing a
                        // dead knob in the config schema.
                        ..Default::default()
                    },
```

- [ ] **Step 5: Run the tests**

```bash
cargo test -p pancetta-config --lib 2>&1 | tail -3
cargo test -p pancetta --lib config_duplicate_defaults 2>&1 | tail -3
cargo clippy --workspace --features transmit 2>&1 | tail -3
```

Expected: merge-guard + new parity test PASS, clippy clean.

- [ ] **Step 6: Reconcile the docs**

`docs/CONFIG.md:168-181` — replace the `### [duplicate_checking]` block with:

`````markdown
### `[duplicate_checking]` — don't call the same station twice

```toml
[duplicate_checking]
enabled = true
time_window_hours = 24
check_frequency = true
```

The duplicate check is what makes Space-to-call return `Call X failed:
duplicate QSO ...` for stations you've already worked. With the default
`check_frequency = true`, a prior QSO only blocks a re-call when it was
within 50 Hz of the same RF frequency — so the same station on a
different band can be worked again. Set `check_frequency = false` for
strict one-QSO-per-callsign inside the window, or `enabled = false` to
turn duplicate checking off entirely.
`````

(The old text claimed the default was `check_frequency = false` with "one-and-done per UTC day" semantics — the code default is `true`, `qso_manager.rs:423`, and the window is a rolling 24 h from QSO start, not a UTC day, `qso_manager.rs:3130-3134`.)

`README.md:214-220` — in the `Call X failed: duplicate QSO` troubleshooting entry, replace "the same station on the same band twice within the configured `duplicate_checking.time_window_hours`" with "the same station within the configured `duplicate_checking.time_window_hours` rolling window (by default scoped to within 50 Hz of the same frequency — see `[duplicate_checking]` in [`docs/CONFIG.md`](docs/CONFIG.md))". Before finalizing the 50 Hz claim, spot-check the persistent-DB path too (`async_database`'s `check_duplicate`, called at `qso_manager.rs:3155-3162`) and soften the wording to "in-memory check" scoping if the DB path ignores frequency.

- [ ] **Step 7: Commit**

```bash
git add pancetta-config/src/duplicate_check.rs pancetta-config/src/lib.rs pancetta/src/coordinator/qso.rs docs/CONFIG.md README.md
git commit -m "feat(config): wire [duplicate_checking] into config loading — defaults identical to prior hard-coded values"
```

---

### Task 6: Warn on unknown top-level config sections

**Files:**
- Modify: `pancetta-config/src/loader.rs` (`parse_toml` at 565-573, new helper + tests in the existing `mod tests`)

**Interfaces:**
- Consumes: the existing `load_warnings` mechanism — warnings pushed to `self.load_warnings` (`loader.rs:249-254`) are returned by `load_warnings()` (`loader.rs:285-290`) and surfaced to console + TUI via `Config::load_default_with_warnings` (`lib.rs:221-226`). Serde ignores unknown keys by default (no `deny_unknown_fields` anywhere on `Config`), which is the silent-ignored-knob bug class.
- Produces: a warning per unknown top-level table key. The known-key set is **derived from `Config` itself** (serialize `Config::default()`, take the object keys) so it can never drift when a section is added — this is why Task 5 (which adds `duplicate_checking`) lands first, but the derivation makes the order safe regardless.
- Scope: TOML only (`parse_toml`). JSON configs (`parse_json`) are an exotic path; deliberately untouched this phase.

- [ ] **Step 1: Write the failing tests**

In the existing `mod tests` of `pancetta-config/src/loader.rs` (near `test_parse_toml` at line 915):

```rust
    #[test]
    fn test_unknown_top_level_section_warns() {
        let loader = ConfigLoader::new().unwrap();
        // The exact ghost sections CONFIG.md used to document.
        let toml_content = r#"
[autonomous_operator]
enabled = true

[priority_weights]
snr = 0.05

[station]
callsign = "N0CALL"
"#;
        let parsed = loader.parse_toml(toml_content);
        assert!(parsed.is_ok(), "unknown keys must stay non-fatal");
        let warnings = loader.load_warnings();
        assert_eq!(warnings.len(), 2, "one warning per unknown section: {warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("autonomous_operator")));
        assert!(warnings.iter().any(|w| w.contains("priority_weights")));
    }

    #[test]
    fn test_known_top_level_sections_do_not_warn() {
        let loader = ConfigLoader::new().unwrap();
        let toml_content = r#"
[station]
callsign = "N0CALL"

[autonomous]
enabled = false

[duplicate_checking]
enabled = true
"#;
        loader.parse_toml(toml_content).unwrap();
        assert!(loader.load_warnings().is_empty());
    }
```

```bash
cargo test -p pancetta-config --lib unknown_top_level 2>&1 | tail -5
```

Expected: `test_unknown_top_level_section_warns` FAILS (0 warnings recorded).

- [ ] **Step 2: Implement the sweep**

In `loader.rs`, replace `parse_toml` (lines 565-573) with:

```rust
    /// Parse TOML configuration
    fn parse_toml(&self, content: &str) -> ConfigResult<Config> {
        // Tilde-only (`~`) expansion — deliberately NOT `shellexpand::full`,
        // which would also expand `$VAR` env references in the raw config text
        // and risk leaking secrets (e.g. tokens) into the parsed config and
        // downstream logs/errors. See security fix I-7.
        let expanded_content = shellexpand::tilde(content);

        let config: Config = toml::from_str(&expanded_content).map_err(ConfigError::Toml)?;

        // Serde silently drops unknown keys, so a misspelled or obsolete
        // section ([autonomous_operator], [priority_weights], ...) used to be
        // invisibly inert. Sweep the file's top-level table keys against the
        // sections Config actually has and record a load-warning for each
        // stranger — load_warnings() surfaces these to console + TUI.
        self.warn_unknown_top_level_keys(&expanded_content);

        Ok(config)
    }

    /// Top-level section names `Config` understands, derived from the struct
    /// itself (serialize a default and take the object keys) so this set can
    /// never drift when a config section is added or renamed.
    fn known_top_level_keys() -> std::collections::BTreeSet<String> {
        serde_json::to_value(Config::default())
            .ok()
            .and_then(|v| {
                v.as_object()
                    .map(|o| o.keys().cloned().collect::<std::collections::BTreeSet<_>>())
            })
            .unwrap_or_default()
    }

    /// Compare the raw file's top-level table keys against
    /// [`Self::known_top_level_keys`] and record one load-warning per unknown
    /// section. Non-fatal by design; a second-parse failure here is
    /// unreachable in practice (the typed parse already succeeded).
    fn warn_unknown_top_level_keys(&self, content: &str) {
        let Ok(value) = content.parse::<toml::Value>() else {
            return;
        };
        let Some(table) = value.as_table() else {
            return;
        };
        let known = Self::known_top_level_keys();
        for key in table.keys() {
            if !known.contains(key) {
                warn!(
                    "Unknown top-level config section [{key}] — pancetta ignores it. \
                     Check the spelling against docs/CONFIG.md."
                );
                if let Ok(mut wlist) = self.load_warnings.lock() {
                    wlist.push(format!(
                        "Unknown config section [{key}] — ignored (check spelling; see docs/CONFIG.md)"
                    ));
                }
            }
        }
    }
```

Note: `serde_json` and `toml::Value` are already in use in this file (`parse_json` at 576-581); no new dependencies. `Config::default()`'s `metadata` is `Some(..)` (`lib.rs:198-203`), so `metadata` lands in the derived known set and a user-file `[metadata]` table stays warning-free.

- [ ] **Step 3: Check the hot-reload path shares this parse**

```bash
grep -n "parse_toml\|load_from_file\|from_str" pancetta-config/src/hot_reload.rs
```

Expected: hot-reload funnels through the loader (`load_from_file` → `parse_toml`), so reload picks up the sweep for free. If it parses independently, leave it — the startup warning is this task's contract; note the gap in the commit message.

- [ ] **Step 4: Run tests + clippy**

```bash
cargo test -p pancetta-config --lib 2>&1 | tail -3
cargo clippy -p pancetta-config 2>&1 | tail -3
```

Expected: all pass (including the two new tests), clippy clean.

- [ ] **Step 5: Commit**

```bash
git add pancetta-config/src/loader.rs
git commit -m "feat(config): warn on unknown top-level config sections — kills the silently-ignored-knob class"
```

---

### Task 7: Keybindings single source of truth

**Files:**
- Create: `pancetta-tui/src/keymap.rs`
- Modify: `pancetta-tui/src/lib.rs:38-45` (add module)
- Modify: `pancetta-tui/src/tui_runner.rs:1951-2010` (`render_help_overlay` reads the table)
- Create: `docs/KEYBINDINGS.md` (generated)
- Modify: `README.md:137-163` (essentials table + link), `docs/README.md` (add row)

**Interfaces:**
- Consumes: the current overlay list (`tui_runner.rs:1952-2010`, 36 entries — the most complete surface) and the three handlers it omits: `g` cycle-TX-policy (`tui_runner.rs:1535-1540`), `Shift+M` cycle-operating-mode (`tui_runner.rs:1542-1548`), `e` cycle-decode-effort (`tui_runner.rs:1550-1558`). The README table (README.md:139-163) is ~40% wrong/incomplete: `Enter` described as "Send the TX text in the input buffer" (real Enter behavior is context-dependent — Callers reply / TX-Placement park, `tui_runner.rs:1970-1973`; free-text is composed via `/`), and ~14 bindings missing including `Shift+Q` EMERGENCY STOP.
- Produces: `keymap::KEYBINDINGS` (renders the overlay AND generates the doc) + a drift test in the spirit of the `merge_with` guard (`pancetta-config/src/lib.rs:852-912`): drift fails a test, not a human.

- [ ] **Step 1: Create `pancetta-tui/src/keymap.rs`**

```rust
//! Single source of truth for TUI keybindings.
//!
//! `KEYBINDINGS` drives BOTH the `?` help overlay
//! (`tui_runner::render_help_overlay`) and the generated
//! `docs/KEYBINDINGS.md` (drift-guarded by `keybindings_doc_is_current`).
//! When you add or change a key handler in `tui_runner.rs`, update this
//! table, then regenerate the doc:
//! `PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-tui keybindings_doc_is_current`

/// Doc-grouping category for a keybinding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Navigation,
    TxControl,
    QsoOperation,
    ModesViews,
    AudioDevices,
    Diagnostics,
    Session,
}

impl Category {
    /// All categories in docs/KEYBINDINGS.md section order.
    pub const ALL: [Category; 7] = [
        Category::Navigation,
        Category::QsoOperation,
        Category::TxControl,
        Category::ModesViews,
        Category::AudioDevices,
        Category::Diagnostics,
        Category::Session,
    ];

    /// Section heading used in the generated markdown.
    pub fn heading(self) -> &'static str {
        match self {
            Category::Navigation => "Navigation",
            Category::QsoOperation => "Calling & QSOs",
            Category::TxControl => "TX control",
            Category::ModesViews => "Modes & views",
            Category::AudioDevices => "Audio & devices",
            Category::Diagnostics => "Diagnostics & help",
            Category::Session => "Session & safety",
        }
    }
}

/// One keyboard binding: display key, action text, doc category, and whether
/// it appears in README.md's 10-row essentials table.
pub struct KeyBinding {
    pub key: &'static str,
    pub action: &'static str,
    pub category: Category,
    pub essential: bool,
}

const fn kb(
    key: &'static str,
    action: &'static str,
    category: Category,
    essential: bool,
) -> KeyBinding {
    KeyBinding { key, action, category, essential }
}

/// Every top-level TUI binding, in `?`-overlay display order.
/// Modal-scoped keys (y/n confirms, digit entry in the freq modal, j/k in
/// the device picker) are documented by each modal's own footer, not here.
pub const KEYBINDINGS: &[KeyBinding] = &[
    kb("?", "Toggle this help", Category::Diagnostics, true),
    kb("Tab / Shift+Tab", "Switch panel", Category::Navigation, true),
    kb("Up / Down", "Scroll list", Category::Navigation, true),
    kb("Home / End (or < / >)", "Jump to newest (realtime) / oldest", Category::Navigation, false),
    kb("PgUp / PgDn", "Page scroll", Category::Navigation, false),
    kb("1/2/3/4/5", "Jump: Band/QSO/Callers/DX/Placement", Category::Navigation, false),
    kb("Left / Right", "TX offset −/+ 50 Hz (Callers: cycle reply step)", Category::TxControl, false),
    kb("[ / ]", "TX offset −/+ 50 Hz", Category::TxControl, false),
    kb("= / -", "Band up / down", Category::TxControl, false),
    kb("Space", "Call selected station", Category::QsoOperation, true),
    kb("/", "Compose free-text TX (Enter sends, Esc cancels)", Category::QsoOperation, false),
    kb("Enter", "Callers: reply at shown step; TX Placement: park at selected slice", Category::QsoOperation, false),
    kb("c / s", "Start / stop CQ", Category::QsoOperation, true),
    kb("k", "Abort selected QSO (QSO Status panel only)", Category::QsoOperation, false),
    kb("r", "Re-send last TX (QSO Status panel only)", Category::QsoOperation, false),
    kb("t", "Find clear TX offset (auto-pick + pin)", Category::TxControl, false),
    kb("f", "TX freq mode: HOLD (pin offset) / AUTO (pancetta picks)", Category::TxControl, false),
    kb("o", "Set TX audio offset Hz (blank=Auto) — implies Hold", Category::TxControl, false),
    kb("Shift+F", "Set dial / split freq (RX MHz + optional TX MHz)", Category::TxControl, false),
    kb("Shift+T", "Tune (12 s tone; blocked while TX DISABLED)", Category::TxControl, false),
    kb("h", "Halt current TX", Category::TxControl, true),
    kb("p", "Toggle PTT (blocked while TX DISABLED)", Category::TxControl, false),
    kb("g", "Cycle TX policy: Full → Respond-only → Disabled", Category::TxControl, false),
    kb("v / V", "Cycle activity view: Operate/Hunt/Run/Monitor", Category::ModesViews, false),
    kb("z", "Zoom focused panel (again/Esc to restore)", Category::ModesViews, false),
    kb("a", "Toggle autonomous mode", Category::ModesViews, true),
    kb("Shift+P", "Pause / resume autonomous", Category::ModesViews, false),
    kb("Shift+M", "Cycle operating mode (FT8 → FT4; waits for coordinator confirm)", Category::ModesViews, false),
    kb("e", "Cycle decode-effort preset: Eco → Standard → Deep → Max → Auto", Category::ModesViews, false),
    kb("Shift+H", "Engage Hound on selected DX Hunter station", Category::QsoOperation, false),
    kb("Shift+X", "Toggle Fox (DXpedition) mode", Category::ModesViews, false),
    kb("Shift+D", "Toggle Diagnostics overlay (retained event history)", Category::Diagnostics, false),
    kb("Shift+S", "Toggle station-health panel (is the station healthy?)", Category::Diagnostics, false),
    kb("m", "Toggle audio monitoring", Category::AudioDevices, false),
    kb("d", "Device picker", Category::AudioDevices, true),
    kb("x", "Clear decoded messages (press twice within 3s)", Category::Diagnostics, false),
    kb("q", "Quit (with confirm)", Category::Session, true),
    kb("Shift+Q", "EMERGENCY STOP (halt TX, autonomous off)", Category::Session, true),
    kb("Esc", "Dismiss overlay / cancel modal / clear stop banner", Category::Session, false),
];

/// Render the generated `docs/KEYBINDINGS.md` content.
pub fn render_markdown() -> String {
    let mut out = String::from(
        "<!-- GENERATED FILE - do not edit by hand.\n     \
         Source: pancetta-tui/src/keymap.rs (KEYBINDINGS).\n     \
         Regenerate: PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-tui keybindings_doc_is_current -->\n\n\
         # Pancetta TUI keybindings\n\n\
         Press `?` inside the TUI for the same list as an overlay.\n",
    );
    for cat in Category::ALL {
        out.push_str(&format!("\n## {}\n\n| Key | Action |\n|---|---|\n", cat.heading()));
        for b in KEYBINDINGS.iter().filter(|b| b.category == cat) {
            out.push_str(&format!("| `{}` | {} |\n", b.key, b.action));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift guard, same philosophy as pancetta-config's merge_with guard
    /// (pancetta-config/src/lib.rs merge_guard): the generated doc failing to
    /// match the table fails a test, not a human review.
    #[test]
    fn keybindings_doc_is_current() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/KEYBINDINGS.md");
        let expected = render_markdown();
        if std::env::var("PANCETTA_REGEN_DOCS").is_ok() {
            std::fs::write(path, &expected).expect("write docs/KEYBINDINGS.md");
            return;
        }
        let actual = std::fs::read_to_string(path).unwrap_or_default();
        assert_eq!(
            actual, expected,
            "docs/KEYBINDINGS.md is stale. Regenerate with:\n  \
             PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-tui keybindings_doc_is_current"
        );
    }

    #[test]
    fn essentials_table_is_exactly_ten_rows() {
        let n = KEYBINDINGS.iter().filter(|b| b.essential).count();
        assert_eq!(n, 10, "README essentials table is specified as exactly 10 rows");
    }

    #[test]
    fn no_duplicate_keys() {
        let mut seen = std::collections::HashSet::new();
        for b in KEYBINDINGS {
            assert!(seen.insert(b.key), "duplicate keybinding entry: {}", b.key);
        }
    }
}
```

Add `pub mod keymap;` to `pancetta-tui/src/lib.rs` (module list at lines 38-45).

- [ ] **Step 2: Refactor `render_help_overlay` to read the table**

In `pancetta-tui/src/tui_runner.rs:1951-2010`, replace the hardcoded `let lines: &[(&str, &str)] = &[...]` block with:

```rust
    /// Render help overlay as a centered modal.
    /// Content comes from the single-source-of-truth keybinding table
    /// (`crate::keymap::KEYBINDINGS`) — the same table that generates
    /// docs/KEYBINDINGS.md, so overlay and docs can never disagree.
    fn render_help_overlay(f: &mut Frame, area: Rect) {
        let lines: Vec<(&str, &str)> = crate::keymap::KEYBINDINGS
            .iter()
            .map(|b| (b.key, b.action))
            .collect();
```

The sizing code below (lines 2012-2029) already iterates `lines` generically (`lines.iter()`, `lines.len()`); it compiles unchanged against the `Vec`. Note the overlay grows by 3 rows (`g`, `Shift+M`, `e` were missing); the existing height clamp (`modal_height.min(area.height.saturating_sub(2))`, line 2025) already handles short terminals.

- [ ] **Step 3: Generate the doc, then prove the drift test bites**

```bash
PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-tui keybindings_doc_is_current 2>&1 | tail -3
cargo test -p pancetta-tui keymap 2>&1 | tail -3
echo "stale" >> docs/KEYBINDINGS.md
cargo test -p pancetta-tui keybindings_doc_is_current 2>&1 | tail -5   # must FAIL
PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-tui keybindings_doc_is_current 2>&1 | tail -3  # regen
cargo test -p pancetta-tui keybindings_doc_is_current 2>&1 | tail -3   # green again
```

Expected: generate → pass; corrupt → FAIL with the regen hint; regenerate → pass.

- [ ] **Step 4: Replace README's keybinding table with the 10 essentials + link**

In `README.md`, under `## How to drive the TUI` (line 137), replace the whole 24-row table (lines 139-163) with:

```markdown
The essentials (full reference: [`docs/KEYBINDINGS.md`](docs/KEYBINDINGS.md),
or press `?` in the TUI):

| Key | Action |
|---|---|
| `?` | Toggle help overlay (every binding, in-app) |
| `Tab` / `Shift+Tab` | Switch panel |
| `↑` / `↓` | Scroll / select within the active panel |
| `Space` | Call selected station |
| `c` / `s` | Start / stop repeating CQ |
| `h` | Halt current TX |
| `a` | Toggle autonomous mode |
| `d` | Open audio device picker |
| `q` | Quit (with confirm) |
| `Shift+Q` | **EMERGENCY STOP** — halt TX, autonomous off |
```

These 10 rows must match the `essential: true` entries in `keymap.rs` (the `essentials_table_is_exactly_ten_rows` test pins the count; keeping the texts aligned is a review-time check since README isn't generated).

- [ ] **Step 5: Add KEYBINDINGS.md to the docs index**

In `docs/README.md` (Task 4), add to the curated table after the RUNBOOK row:

```markdown
| [KEYBINDINGS.md](KEYBINDINGS.md) | Generated TUI keybinding reference (source: `pancetta-tui/src/keymap.rs`; drift-tested) |
```

- [ ] **Step 6: Full crate tests + clippy**

```bash
cargo test -p pancetta-tui 2>&1 | tail -3
cargo clippy -p pancetta-tui 2>&1 | tail -3
```

Expected: all pass (including the existing overlay/handler tests around `tui_runner.rs:2272+`), clippy clean.

- [ ] **Step 7: Commit**

```bash
git add pancetta-tui/src/keymap.rs pancetta-tui/src/lib.rs pancetta-tui/src/tui_runner.rs docs/KEYBINDINGS.md docs/README.md README.md
git commit -m "feat(tui): single-source-of-truth keybinding table — drives ? overlay, generates drift-tested docs/KEYBINDINGS.md, README keeps 10 essentials"
```

---

### Task 8: Regenerate `pancetta-config/defaults.toml` from `Config::default()`

**Files:**
- Create: `pancetta-config/tests/defaults_drift.rs`
- Regenerate: `pancetta-config/defaults.toml`
- Modify: `docs/CONFIG.md:7-11`, `docs/CONFIG.md:299-301`, `docs/CONFIG.md:358-360`

**Interfaces:**
- Consumes: `Config: Serialize` (`pancetta-config/src/lib.rs:122`) — regeneration is chosen over billing-demotion because the serde round-trip is already proven per-section (`autonomous.rs:328-334` round-trips via `toml::to_string`). Crucial truth discovered in audit verification: **`defaults.toml` is never loaded at runtime** — the "defaults" source resolves to `load_embedded_defaults()`, which just returns `Config::default()` (`pancetta-config/src/loader.rs:426-431`). The file is pure documentation, currently lying: it omits `[autonomous]`, `[decoder]`, `[hound]`, `[fox]`, `[tx_placement]` entirely while carrying dead sections that exist in no Rust struct (`[network.web_api]`, `[network.wspr]`, `[ui.animations]`, `[ui.keyboard]`, `[ui.accessibility]`, …).
- Produces: a generated, drift-tested `defaults.toml` that is exactly what `Config::default()` serializes to — so CONFIG.md's "any key you don't set inherits the value from there" becomes true by construction.

- [ ] **Step 1: Probe serialization feasibility (decision gate)**

The drift test in Step 2 IS the probe. **Fallback branch:** if `toml::to_string_pretty` errors on `Config::default()` (e.g. a value-after-table ordering issue in some nested struct), do NOT fight it — instead demote the billing: rewrite CONFIG.md:7-11 to say the authoritative schema is the Rust structs under `pancetta-config/src/` (`Config::default()`), delete `pancetta-config/defaults.toml`'s dead sections manually, and skip the generator. Record whichever branch was taken in the commit message.

- [ ] **Step 2: Create `pancetta-config/tests/defaults_drift.rs`**

```rust
//! Drift guard: `pancetta-config/defaults.toml` is GENERATED from
//! `Config::default()` and must always match it.
//!
//! The file is documentation — the runtime never reads it: the loader's
//! "defaults" source returns `Config::default()` directly
//! (src/loader.rs, `load_embedded_defaults`). This test keeps the
//! documentation byte-honest. Same drift-fails-a-test philosophy as the
//! `merge_with` guard in src/lib.rs.

use pancetta_config::Config;

const HEADER: &str = "\
# GENERATED FILE - do not edit by hand.
# This is the full pancetta configuration schema with every default value,
# serialized from Config::default() (the runtime source of truth; the loader
# never reads this file). Annotated key documentation: docs/CONFIG.md.
# Regenerate: PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-config --test defaults_drift
";

fn render_defaults_toml() -> String {
    let mut cfg = Config::default();
    // metadata carries a fresh uuid + timestamp per construction — per-run
    // noise, not schema. Config's serde skips it when None.
    cfg.metadata = None;
    let body = toml::to_string_pretty(&cfg)
        .expect("Config::default() must serialize to TOML (see plan Task 8 fallback)");
    format!("{HEADER}\n{body}")
}

#[test]
fn defaults_toml_is_current() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/defaults.toml");
    let expected = render_defaults_toml();
    if std::env::var("PANCETTA_REGEN_DOCS").is_ok() {
        std::fs::write(path, &expected).expect("write defaults.toml");
        return;
    }
    let actual = std::fs::read_to_string(path).unwrap_or_default();
    assert_eq!(
        actual, expected,
        "pancetta-config/defaults.toml is stale. Regenerate with:\n  \
         PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-config --test defaults_drift"
    );
}

#[test]
fn generated_defaults_round_trip() {
    // The generated file must parse back into a Config equal (via re-serialize)
    // to what produced it — guards against serialize-only fields.
    let text = render_defaults_toml();
    let reparsed: Config = toml::from_str(&text).expect("generated defaults.toml must parse");
    let mut original = Config::default();
    original.metadata = None;
    let mut reparsed = reparsed;
    reparsed.metadata = None;
    assert_eq!(
        toml::to_string_pretty(&reparsed).unwrap(),
        toml::to_string_pretty(&original).unwrap()
    );
}
```

(`toml` is a regular dependency of `pancetta-config`, available to `tests/` targets; no Cargo.toml change needed.)

- [ ] **Step 3: Regenerate and verify the file content**

```bash
PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-config --test defaults_drift defaults_toml_is_current 2>&1 | tail -3
cargo test -p pancetta-config --test defaults_drift 2>&1 | tail -3
grep -c "^\[autonomous" pancetta-config/defaults.toml     # expect >= 1
grep -n "^\[decoder\]\|^\[hound\]\|^\[fox\]\|^\[tx_placement\]" pancetta-config/defaults.toml
grep -n "web_api\|animations\|wspr\|ui.keyboard\|accessibility" pancetta-config/defaults.toml   # expect NO matches
```

Expected: both tests pass; the previously-missing sections (`[autonomous]`, `[decoder]`, `[hound]`, `[fox]`, `[tx_placement]`) now present; every dead section gone.

- [ ] **Step 4: Fix CONFIG.md's billing**

1. CONFIG.md:7-11 — replace with:

```markdown
This document covers the keys you'll actually touch, with explanations.
The complete schema — every section, every key, every default — is
[`pancetta-config/defaults.toml`](../pancetta-config/defaults.toml),
which is **generated from the code's `Config::default()`** and
drift-tested in CI, so it can't lie. Any key you don't set in your user
config keeps its default value from there.
```

2. CONFIG.md:299-301 — the `[ui]` section's claim "The TUI also reads its layout, key bindings, and color scheme details from `[ui]`" is false (keybindings are compiled in — see `docs/KEYBINDINGS.md` from Task 7; the old `[ui.keyboard]` block in defaults.toml matched no Rust struct and just vanished in Step 3). Replace with:

```markdown
The remaining `[ui]` keys are in `defaults.toml`; the ones above are the
ones with practical effect. Keybindings are not configurable — the full
map is [`docs/KEYBINDINGS.md`](KEYBINDINGS.md) (or `?` in the TUI).
```

3. CONFIG.md:358-360 ("Where to look next") — change "The annotated source of truth is `pancetta-config/defaults.toml`" to "The complete generated schema is `pancetta-config/defaults.toml`; the annotated source of truth is the Rust structs under `pancetta-config/src/`."

- [ ] **Step 5: Full config-crate gate**

```bash
cargo test -p pancetta-config 2>&1 | tail -3
cargo clippy -p pancetta-config 2>&1 | tail -3
```

Expected: all tests (lib + new integration test) pass, clippy clean.

- [ ] **Step 6: Commit**

```bash
git add pancetta-config/tests/defaults_drift.rs pancetta-config/defaults.toml docs/CONFIG.md
git commit -m "feat(config): generate defaults.toml from Config::default() with drift test — adds missing [autonomous]/[decoder]/[hound]/[fox]/[tx_placement], drops dead sections"
```

---

## Final gate (after all tasks)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --features transmit 2>&1 | tail -3
cargo test --workspace --features transmit 2>&1 | tail -5
```

Expected: all green (the three new drift tests — merge-guard-extended, `keybindings_doc_is_current`, `defaults_toml_is_current` — plus `config_duplicate_defaults_match_qso_manager_defaults` all run in the workspace suite).

Then, per the repo documentation policy:

- [ ] Append dated entries to `docs/DECISIONS/config-and-platform.md` ([duplicate_checking] wiring rationale + zero-behavior-change guard; unknown-top-level-key warning; defaults.toml generation) and `docs/DECISIONS/tui.md` (keybindings single source of truth + generated doc).
- [ ] Push the branch and open a PR titled "Onboarding Phase 2: docs tell the truth" referencing `docs/superpowers/specs/2026-07-12-onboarding-world-class-design.md`.
