# Operator TX-Offset Control + Multi-TX De-confliction — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]`.

**Goal:** Manual calls/answers place our TX audio offset by operator intent first (held offset, WSJT-X "Hold Tx Freq" style), collision-avoidance second (de-conflict concurrent streams), Tx=Rx fallback last. Built on Hound's `partner_freq` Tx≠Rx decoupling.

**Spec:** `docs/superpowers/specs/2026-06-28-tx-offset-control-design.md`. **Decisions:** set-offset via `o` modal (verified free; implies Hold); first concurrent QSO honors held offset but de-conflicts if it'd collide with an active QSO; BOTH `StartQso` and `respond_to_caller` honor it; Tx=Rx fallback is regression-guarded (`partner_freq=None`).

**Branch:** `feat/tx-offset-control` off current main.

---

## Task 1: `deconflict_offset` pure helper

**Files:** `pancetta-qso/src/qso_manager.rs` (near `hound_offset_for`).

- [ ] **Step 1 (test-first):** `deconflict_offset(candidate: f64, occupied: &[f64], min_sep: f64, lo: f64, hi: f64) -> f64` — returns `candidate` if it's ≥ `min_sep` from every `occupied` and within `[lo,hi]`; else the nearest in-range offset (deterministic outward search, 25 Hz step) that is ≥ `min_sep` from all occupied and in range; if none found, returns `candidate.clamp(lo,hi)`. NO RNG. Tests: clear candidate unchanged; candidate within `min_sep` of an occupied nudges ≥ `min_sep` away, in-range; two occupied bracket → finds a gap; deterministic (same inputs → same output); empty occupied → candidate clamped.
- [ ] **Step 2:** implement; `cargo test -p pancetta-qso deconflict` pass.
- [ ] **Step 3:** Commit `feat(qso): deconflict_offset helper (deterministic own-stream separation)`.

## Task 2: held-offset shared state + TxFreqMode access in coordinator

**Files:** `pancetta/src/coordinator/mod.rs` (struct + ctor), wherever `TxFreqMode` atomic lives (trace: a `tx_freq_mode` atomic set by TUI `f`; find it).

- [ ] **Step 1:** add `tx_offset_hold_hz: Arc<AtomicU64>` (0 = unset/Auto) to `ApplicationCoordinator`, init 0 in the constructor (mirror `split_tx_frequency_hz`). Add a `pub(crate)` accessor if needed by the qso handler.
- [ ] **Step 2:** confirm the `TxFreqMode` (Hold/Auto) value is reachable from the QSO message handler (`coordinator/qso.rs`) — it's an atomic (`tx_freq_mode`/`cmd_tx_freq_mode`); thread a clone into the QSO component task if not already present. (READ how `f`→`ToggleTxFreqMode` stores it.)
- [ ] **Step 3:** `cargo build -p pancetta` clean. Commit `feat(coord): tx_offset_hold_hz shared state`.

## Task 3: manual-open offset selection (the core) + manager partner_freq param

**Files:** `pancetta-qso/src/qso_manager.rs` (`respond_to_cq_with`, `respond_to_caller`), `pancetta/src/coordinator/qso.rs` (`StartQso`, `RespondToCaller` handlers).

- [ ] **Step 1:** add an optional `partner_freq: Option<f64>` param to `respond_to_cq_with` (and the `respond_to_caller` path), defaulting to `None` at existing callers (so today's Tx=Rx behavior is unchanged). When `Some`, set `metadata.partner_freq` on the created QSO (same mechanism `engage_hound` uses). `metadata.frequency` = the passed `frequency` (our TX offset) as today. Keep existing callers (`respond_to_cq`, `respond_to_cq_manual`, autonomous) passing `None`.
- [ ] **Step 2:** in `coordinator/qso.rs` `StartQso` + `RespondToCaller` handlers, compute the TX offset before calling the manager:
  ```
  let dx_freq = <message frequency as f64>;
  let held = tx_offset_hold_hz.load(Relaxed);          // 0 = none
  let hold_mode = TxFreqMode::from_u8(tx_freq_mode.load(..)) == Hold;
  let active_offsets: Vec<f64> = <TX offsets of currently-active QSOs>;  // see Step 3
  let candidate = if hold_mode && held != 0 { held as f64 } else { dx_freq };
  let tx_off = deconflict_offset(candidate, &active_offsets, 75.0, 300.0, 2700.0);
  let partner = ((tx_off - dx_freq).abs() > 1.0).then_some(dx_freq);
  manager.respond_to_cq_with(call, tx_off, dx_parity, Manual, partner);   // or respond_to_caller
  ```
  Regression: when `!hold_mode` (Auto) AND no collision ⇒ `candidate=dx_freq`, `deconflict` returns it unchanged, `partner=None` ⇒ exact Tx=Rx as today.
- [ ] **Step 3:** add a `QsoManager::active_tx_offsets() -> Vec<f64>` (TX `metadata.frequency` of non-terminal active QSOs) so the handler can de-conflict against live streams. Unit-test it.
- [ ] **Step 4 (tests):** engine/coord unit — held offset honored (single QSO: `metadata.frequency==held`, `partner_freq==Some(dx)`); two near DX → second de-conflicted (≥75 Hz apart); **regression**: Auto + single + no-collision → `frequency==dx_freq`, `partner_freq==None`.
- [ ] **Step 5:** `cargo test -p pancetta-qso && cargo build -p pancetta` → pass. Commit `feat(qso+coord): TX-offset selection (held → de-conflict → Tx=Rx) on manual calls`.

## Task 4: TUI set-offset modal (`o`) + status

**Files:** `pancetta-tui/src/{tui_runner.rs,app.rs,ui/...}`, `pancetta/src/coordinator/tui_relay.rs`, `message_bus.rs`.

- [ ] **Step 1:** `TuiCommand::SetTxOffset { offset_hz: Option<u64> }` (`None` = clear → Auto). Bind `o` (verified free) on the main view → opens a small numeric modal (mirror the `Shift+F` freq modal): prompt audio offset Hz (200–2900); blank = clear. Setting a value implies `TxFreqMode::Hold`.
- [ ] **Step 2:** relay arm `SetTxOffset` → write `tx_offset_hold_hz` atomic (0 if clear) + set `TxFreqMode::Hold` when a value is given (or Auto on clear); echo status.
- [ ] **Step 3:** status display: show "TX off: NNNN Hz (HOLD)" vs "TX: auto" in the TX strip/status line (additive).
- [ ] **Step 4:** `cargo build -p pancetta-tui -p pancetta` clean; `cargo test -p pancetta-tui` pass. Commit `feat(tui): 'o' set-TX-offset modal + Hold status`.

## Task 5: coord_sim rig-level proof

**Files:** `pancetta/tests/coord_sim.rs`.

- [ ] **Step 1:** scenarios: (a) set held offset (via the atomic) → manual QSO keys PTT on the held offset, `partner_freq` routes the DX reply; (b) two manual QSOs at near DX offsets → keyed on distinct offsets ≥75 Hz apart; (c) regression: Auto, single, distinct DX → keys on the DX freq (Tx=Rx).
- [ ] **Step 2:** `cargo test -p pancetta --test coord_sim` pass. Commit `test(coord): held-offset honored + multi-TX de-confliction (rig-level)`.

## Task 6: docs + gate + land

- [ ] **Step 1:** CLAUDE.md architecture bullet (TX-offset selection priority, `o` modal, `partner_freq` reuse, de-confliction). Note Tx=Rx is fallback not invariant (operator runs Hold).
- [ ] **Step 2:** final code-review subagent over the branch (focus: regression invariant Auto+no-collision==Tx=Rx; `partner_freq` on normal QSOs routes replies; de-conflict determinism; TX-policy unaffected).
- [ ] **Step 3:** fast gate (`--all-targets` now) → PR → **wait for CI green** → merge; sync main.

---

## Self-review checkpoints
- **Regression:** Auto + no operator offset + no collision ⇒ Tx=Rx, `partner_freq=None`, byte-identical (Task 3 Step 4 + property/coord regression).
- **`partner_freq` reuse:** normal QSOs now set it when Tx≠Rx — same gate Hound shipped + regression-tested.
- **Determinism:** `deconflict_offset` no RNG.
- **Scope:** manual path only; autonomous/pounce/Hound untouched.
