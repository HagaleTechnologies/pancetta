# FT8 Hound Mode v1 — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]` checkboxes.

**Goal:** Operator manually engages Hound on a chosen DXpedition (Fox); pancetta calls low, QSYs up on the Fox's report, completes on RR73, logs it flagged.

**Architecture:** Additive QSO-state-machine arms + a manual engage path on the existing FT8 stack (no modulator/decoder changes). The one cross-cutting change: split the Fox's RX frequency (`metadata.partner_freq`) from our TX offset (`metadata.frequency`); `partner_freq = None` reproduces today's behavior exactly.

**Tech Stack:** Rust workspace (pancetta-qso, pancetta-config, pancetta, pancetta-tui). TDD per task; per-crate builds during dev; full gate only at land.

**Spec:** `docs/superpowers/specs/2026-06-27-hound-mode-design.md`. **Decisions:** engage key `h`; deterministic offset spread; log flag = `tags`→COMMENT + `APP_PANCETTA_HOUND`.

**Branch:** `feat/hound-mode` (spec already committed `ba939dbd`). Implementers commit here; never push (controller lands via PR).

---

## Task 1: `HoundConfig` (pancetta-config)

**Files:** Create `pancetta-config/src/hound.rs`; modify `pancetta-config/src/lib.rs` (add `pub mod hound;`, add `#[serde(default)] pub hound: HoundConfig` to `Config` at ~`:115`).

- [ ] **Step 1 (test-first):** in `hound.rs` tests, assert a `Config`/`HoundConfig` deserialized from TOML with no `[hound]` section yields defaults `call_min_hz=300.0, call_max_hz=900.0, response_min_hz=1000.0, response_max_hz=2700.0`; and that `HoundConfig::validate()` rejects min>=max or out-of-[200,3000] by returning a warning + falling back to defaults (mirror `AutonomousConfig` validation style).
- [ ] **Step 2:** Implement `HoundConfig { call_min_hz, call_max_hz, response_min_hz, response_max_hz: f64 }` with `#[serde(default=...)]` fns + `Default` + `validate()`. Derive `Debug, Clone, Serialize, Deserialize`.
- [ ] **Step 3:** `cargo test -p pancetta-config` → pass.
- [ ] **Step 4:** Commit `feat(config): HoundConfig (call/response audio regions, defaulted+validated)`.

## Task 2: `QsoMetadata` Hound fields + freq constants/helpers (pancetta-qso)

**Files:** `pancetta-qso/src/states.rs` (`QsoMetadata` ~`:303-409`), `pancetta-qso/src/qso_manager.rs` (constants ~`:31-36`).

- [ ] **Step 1 (test-first):** unit test that `QsoMetadata::default()` (or the existing test ctor) has `hound=false, partner_freq=None, hound_qsyed=false`; and a `hound_offset_for(seed: &str, lo: f64, hi: f64) -> f64` helper returns a value in `[lo,hi]`, deterministic for a given seed, and spreads (two different seeds → different offsets in-range). NO `Math.random`/`rand` — hash the seed (e.g. callsign) into the range.
- [ ] **Step 2:** Add fields `hound: bool`, `partner_freq: Option<f64>`, `hound_qsyed: bool` to `QsoMetadata` (all `#[serde(default)]`, defaults false/None/false). Add constants `HOUND_CALL_MIN_HZ=300.0`, `HOUND_CALL_MAX_HZ=900.0`, `HOUND_RESPONSE_MIN_HZ=1000.0`, `HOUND_RESPONSE_MAX_HZ=2700.0`. Add `fn hound_offset_for(seed, lo, hi)`.
- [ ] **Step 3:** `cargo build -p pancetta-qso` clean (fix all `QsoMetadata { .. }` construction sites — use `..Default::default()` or add the fields); `cargo test -p pancetta-qso hound_offset` pass.
- [ ] **Step 4:** Commit `feat(qso): QsoMetadata Hound fields + deterministic offset-spread helper`.

## Task 3: relevance-gate `partner_freq` split (the cross-cutting change)

**Files:** `pancetta-qso/src/qso_manager.rs` (`is_message_relevant` ~`:2428-2515`, freq tolerance compare ~`:2439/2454`), `pancetta-qso/src/qso_filter.rs` (the acknowledged Hound TODO ~`:21`).

- [ ] **Step 1 (test-first):** unit/engine test — a QSO with `partner_freq=Some(fox_freq)` and `frequency=<low>`: an incoming partner frame at `fox_freq` (±tolerance) is relevant; a frame at `frequency`(our TX) is NOT. And the **regression guard**: with `partner_freq=None`, relevance is byte-identical to today (frame at `frequency` relevant).
- [ ] **Step 2:** In `is_message_relevant`, compute `let match_freq = metadata.partner_freq.unwrap_or(metadata.frequency);` and compare the incoming frame against `match_freq` (keep the existing `FREQ_TOLERANCE_HZ`/`ESTABLISHED_FREQ_TOLERANCE_HZ` logic). Apply the same `partner_freq`-when-set rule in `qso_filter.rs` and clear its "Hound not implemented" comment. NO other behavior change.
- [ ] **Step 3:** `cargo test -p pancetta-qso` → all pass (existing + new). Confirm no regression in existing relevance/sender-verification tests.
- [ ] **Step 4:** Commit `feat(qso): relevance gate keys on partner_freq when set (Hound Tx≠Rx); None = unchanged`.

## Task 4: `engage_hound` constructor

**Files:** `pancetta-qso/src/qso_manager.rs` (near `respond_to_cq_with` ~`:831`).

- [ ] **Step 1 (test-first):** test that `engage_hound(fox_call, fox_freq, fox_grid?, dx_parity)` creates a QSO in `RespondingToCq` with `metadata.hound=true`, `partner_freq=Some(fox_freq)`, `initiated_by=Manual`, `role=Caller`, `tx_parity=dx_parity.opposite()`, and `frequency` in `[300,900]` (the low calling offset via `hound_offset_for`), and emits an opening `CqResponse` (`<Fox> <us> <grid>`) on that low offset.
- [ ] **Step 2:** Implement `engage_hound` as a thin wrapper over `respond_to_cq_with` (or inline) setting the Hound metadata + low offset. Reuse the manual self-duplicate bypass + parity latch already in the manual path.
- [ ] **Step 3:** `cargo test -p pancetta-qso engage_hound` pass.
- [ ] **Step 4:** Commit `feat(qso): engage_hound ctor (manual Hound QSO, call-low offset)`.

## Task 5: QSY-on-report + R-report on new offset

**Files:** `pancetta-qso/src/qso_manager.rs` (`(RespondingToCq, SignalReport)` transition ~`:2050-2083` + reply-emit/stuck-hop window ~`:1718-1773`).

- [ ] **Step 1 (test-first, engine sim):** drive the real `QsoManager` through a full Hound exchange (mirror `pancetta-qso/tests/autonomous_scenarios.rs` style): `engage_hound` → assert call-low; feed a Fox `SignalReport` directed at us **at `fox_freq`** → assert state→`SendingReport`, `metadata.hound_qsyed=true`, `metadata.frequency` now in `[1000,2700]`, and the emitted `ReportAck` text is `<Fox> <us> R-NN` carried on the new high offset; feed Fox `RR73` (at `fox_freq`) → `Completed`. Also assert the QSY fires even when `TxFreqMode::Hold`.
- [ ] **Step 2:** At the report transition + reply-emit window, if `metadata.hound && !metadata.hound_qsyed`: set `qsy = hound_offset_for(<seed>, HOUND_RESPONSE_MIN_HZ, HOUND_RESPONSE_MAX_HZ)`, set `metadata.frequency=qsy`, the next-state `frequency=qsy`, `qso_frequency=qsy`, `metadata.hound_qsyed=true` — mirroring the stuck-hop mutation but unconditional on `TxFreqMode`. The existing emitter then sends `ReportAck` on the new offset.
- [ ] **Step 3:** `cargo test -p pancetta-qso` (engine scenarios) → pass.
- [ ] **Step 4:** Commit `feat(qso): Hound QSY-up on Fox report + R-report on new offset (TxFreqMode-independent)`.

## Task 6: ADIF Hound flag (tags→COMMENT + APP_PANCETTA_HOUND)

**Files:** `pancetta-qso/src/adif.rs` (~`:144-147,421-422,588-592`), wherever the completed Hound QSO sets `tags` (in `engage_hound`/completion: append `"HOUND"`).

- [ ] **Step 1 (test-first):** given a completed `QsoMetadata` with `hound=true`/`tags` containing `"HOUND"`, `qso_to_adif` renders `MODE FT8` (no SUBMODE), a `COMMENT` containing `HOUND`, and an `APP_PANCETTA_HOUND` field = `true`. A non-Hound QSO renders neither (byte-identical to today).
- [ ] **Step 2:** Ensure `engage_hound`/completion appends `"HOUND"` to `tags`; in `adif.rs` render `tags`→COMMENT (if not already) and emit `<APP_PANCETTA_HOUND:4>true` when `metadata.hound`. Keep MODE/SUBMODE = FT8.
- [ ] **Step 3:** `cargo test -p pancetta-qso adif` pass.
- [ ] **Step 4:** Commit `feat(qso): flag Hound contacts in ADIF (COMMENT tag + APP_PANCETTA_HOUND)`.

## Task 7: bus message + coordinator handler

**Files:** `pancetta/src/message_bus.rs` (`QsoMessage` ~`:416`), `pancetta/src/coordinator/qso.rs` (handler ~`:1893`).

- [ ] **Step 1:** Add `QsoMessage::EngageHound { callsign: String, fox_freq: u64, dx_parity: Option<SlotParity> }`.
- [ ] **Step 2:** In `qso.rs`, handle `EngageHound` → `qso_manager.engage_hound(...)`, reusing the manual parity-admit + `PendingManualCalls` deferral the manual `StartQso` arm uses, and the `StateChanged`/`MessageToSend` plumbing (so it keys PTT + keep-calls). Add a coord-level unit test if feasible (or cover via Task 9 coord_sim).
- [ ] **Step 3:** `cargo build -p pancetta` clean.
- [ ] **Step 4:** Commit `feat(coord): QsoMessage::EngageHound + handler (manual, parity-gated)`.

## Task 8: TUI engage key + Hound badge

**Files:** `pancetta-tui/src/tui_runner.rs` (`TuiCommand` ~`:205`, DX-Hunter key handling ~`:1197-1242`), `pancetta/src/coordinator/tui_relay.rs` (relay arm ~`:792-847`, TX-policy gate ~`:824`), `pancetta-tui/src/ui/qso_status.rs` + `states.rs` `ladder_view` (badge).

- [ ] **Step 1:** Add `TuiCommand::EngageHound { callsign, fox_freq, dx_parity }`. Bind **`h`** on `ActivePanel::DxHunter` (selected row) → build it from `get_selected_station` (callsign + its freq + parity).
- [ ] **Step 2:** In `tui_relay.rs`, add the arm `EngageHound → QsoMessage::EngageHound`, **gated by `TxPolicy`** exactly like `CallStation` (refuse + warn when RespondOnly/Disabled).
- [ ] **Step 3:** Add a `Hound` badge to the QSO-status ladder/detail (and an engage status line). Additive render only.
- [ ] **Step 4:** `cargo build -p pancetta-tui` + `-p pancetta` clean; `git diff --stat` shows only additive TUI changes.
- [ ] **Step 5:** Commit `feat(tui): 'h' engages Hound on selected DX + Hound badge (TX-policy gated)`.

## Task 9: coordinator sim — rig-level QSY proof

**Files:** `pancetta/tests/coord_sim.rs`.

- [ ] **Step 1:** New scenario: `EngageHound` → pump → `drive_slot` → assert PTT keys with the modulator offset in `[300,900]` (call-low); feed a Fox report event; assert the next keyed TX uses an offset in `[1000,2700]` (QSY actually changed on the wire); feed RR73 → QSO completes. Assert at the rig/offset level (mirror existing coord_sim scenarios).
- [ ] **Step 2:** `cargo test -p pancetta --test coord_sim` pass.
- [ ] **Step 3:** Commit `test(coord): Hound engage → call-low → QSY-high on report (rig-level)`.

## Task 10: docs + final gate + land

- [ ] **Step 1:** CLAUDE.md architecture bullet for Hound (manual engage `h`, Tx≠Rx `partner_freq`, QSY-on-report, FT8/ADIF-flagged, manual-only v1, Fox is next). Note the dispensa `mode`-field item stays deferred to FT4.
- [ ] **Step 2:** Full gate — push `feat/hound-mode`; `gh pr create` + `gh pr merge --auto --rebase --delete-branch`; sync main.
- [ ] **Step 3 (dispensa, optional):** none required — Hound is mode FT8, no rig-api change. (When Fox/FT4 add the `mode` field, file the rig-api.v1.1 question then.)

---

## Self-review checkpoints
- **No-regression invariant:** `partner_freq=None` ⇒ identical relevance/transitions (Task 3 regression test + the non-Hound ADIF test).
- **QSY fires once + TxFreqMode-independent** (Task 5 `hound_qsyed` + Hold test).
- **TX-policy gated** engage (Task 8) like every other initiation.
- **Manual-only / Hound-only** scope held (no autonomous, no Fox).
- **Determinism:** offsets via seed-hash, no `rand`/`Date.now` (Task 2).
