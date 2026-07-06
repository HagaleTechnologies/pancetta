# Decoder Speed Overhaul Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the native Rust FT8/FT4 decoder ~4–6x faster at identical recall, then turn remaining depth-vs-time trade-offs into a runtime effort continuum (budget-governed anytime decoding with config + TUI control).

**Architecture:** Phase 1 lands seven independent mechanical fixes in `pancetta-ft8` (each bit-exact or research-harness A/B gated). Phase 2 restructures `decode_window_with_ap_scoped_partner` into a stage pipeline governed by a `DecodeBudget` deadline checked between work items, with a BP escalation ladder replacing the flat 100-iteration cap. Phase 3 adds a `[decoder]` config section, hardware-tier auto-mapping, and a TUI effort-cycling key.

**Tech Stack:** Rust workspace (existing), rustfft/realfft, Rayon, ratatui TUI, `pancetta-research` eval harness for A/B gates.

**Spec:** `docs/superpowers/specs/2026-07-06-decoder-speed-overhaul-design.md` — read it first.

## Global Constraints

- **Licensing / clean room:** ft8_lib (vendored, `pancetta-ft8/vendor/ft8_lib`) is MIT — its constants/techniques may be used directly with a citation comment. WSJT-X/wsjtr/JTDX/ft8mon/MSHV are GPL — NO task here may read their source; none needs to. If that changes, stop and use the clean-room firewall (Reader→spec→Implementer).
- **Gating classes:** [BIT-EXACT] tasks must pass `cargo test --workspace --features transmit` with unchanged decode counts on the WAV fixtures. [A/B] tasks must pass the standing research-harness gate (hard-200/1000, bootstrap CI, ΔFP ≤ 2×ΔTP, elapsed hard gate) before default-flip; implement behind a default-off `Ft8Config` flag, A/B, then flip.
- **Benchmark every perf task:** before/after numbers from `cargo run --release -p pancetta-ft8 --example profile_decode -- native 10` (single-thread: prefix `RAYON_NUM_THREADS=1`) go in the commit message. Baseline at plan time: ~240 ms/window single-thread, ~143 ms multi-thread, 8 msgs on the fixture.
- **No recall-feature removal.** Multipass/cross-cycle/joint-pair/a7/neural-OSD stay; they become schedulable.
- **Config-merge rule:** any new config struct/field added in this plan MUST have its `merge_with` line and a regression test in the same task (2026-07-05 bug class).
- **Subagent rules (standing):** implementers never push / never destructive git; controller pushes at batch boundaries. Local `cargo fmt` + `cargo clippy` before each commit.
- **Determinism:** tests, CI, and the research harness always run with unlimited budget (`DecodeBudget::unlimited()`); no wall-clock reads on the eval path.
- **Don't execute this plan in the authoring session.** Operator will switch models first.

---

## Phase 1 — Mechanical fixes

### Task 1: Commit the profiling harness (F7)

The harness already exists uncommitted in the working tree from the investigation session.

**Files:**
- Create (already on disk, untracked): `pancetta-ft8/examples/profile_decode.rs`
- Modify: `pancetta-ft8/README.md` (add a "Profiling" section)
- Modify: `pancetta-ft8/src/decoder.rs` (doc comment on `decode_window`)

**Interfaces:**
- Produces: `profile_decode` example with modes `native` / `native-fresh` / `ft8lib`, iteration count arg, and ablation env vars `ABL_LDPC_ITERS`, `ABL_SYNC_CANDS`, `ABL_MULTIPASS`, `ABL_CROSS_CYCLE`, `ABL_JOINT_PAIR`. All later tasks cite its output.

- [ ] **Step 1: Verify the harness builds and runs**

Run: `cargo run --release -p pancetta-ft8 --example profile_decode -- native 3`
Expected: three `iter N: ... ms, 8 msgs` lines then a `native: ... ms/window wall` summary (~140–240 ms/window depending on thread count).

- [ ] **Step 2: Add the WINDOW_SAMPLES caller note**

In `pancetta-ft8/src/decoder.rs`, extend the doc comment on `pub fn decode_window` (line ~1727) with:

```rust
/// # Window size
///
/// Callers must pass exactly `WINDOW_SAMPLES` samples (pad or truncate first).
/// The Costas t0 sweep scales with trailing time slack: feeding a full 15 s
/// (180 000-sample) buffer instead of the 151 680-sample window was measured
/// at 4–5x the decode cost (2026-07-06 profiling session).
```

Add to `pancetta-ft8/README.md`:

```markdown
## Profiling

`cargo run --release -p pancetta-ft8 --example profile_decode -- [native|native-fresh|ft8lib] [iters]`

Decodes `tests/fixtures/wav/wsjt/210703_133430.wav` repeatedly and prints per-window
wall time. `RAYON_NUM_THREADS=1` gives clean single-thread numbers. For call-graph
profiles build with `CARGO_PROFILE_RELEASE_DEBUG=true CARGO_PROFILE_RELEASE_STRIP=false`
(the workspace release profile strips symbols) and use `sample`/Instruments (macOS).
Ablation env vars: `ABL_LDPC_ITERS`, `ABL_SYNC_CANDS`, `ABL_MULTIPASS`,
`ABL_CROSS_CYCLE` (0/1), `ABL_JOINT_PAIR` (0/1).
```

- [ ] **Step 3: Full-suite check**

Run: `cargo test -p pancetta-ft8 --features transmit 2>&1 | tail -3`
Expected: all tests pass (examples compile under `cargo test`).

- [ ] **Step 4: Commit**

```bash
git add pancetta-ft8/examples/profile_decode.rs pancetta-ft8/README.md pancetta-ft8/src/decoder.rs
git commit -m "chore(ft8): commit decoder profiling harness + WINDOW_SAMPLES cost note"
```

---

### Task 2: BP housekeeping — lazy trajectory + array returns (F2) [BIT-EXACT]

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` — `belief_propagation_with_features` (~line 10419), `belief_propagation_with_trajectory` (~10400), `belief_propagation` (the plain variant near ~10390 that ends `Ok(output_llrs.to_vec())`), and their call sites in `decode_soft` (~10020) / `try_ldpc_with_ap` / `par_try_ldpc_with_ap`.
- Test: existing suite + one new unit test in the `decoder.rs` test module.

**Interfaces:**
- Produces: `belief_propagation_with_features(&self, channel_llrs: &[f32]) -> Ft8Result<([f32; 174], Option<Box<[[f32; 174]; 25]>>, (u8, f32))>` — note `[f32;174]` (was `Vec<f32>`) and `Box`ed trajectory (17.4 KB moved off the stack/copy path). Task 10 (escalation) consumes this exact signature.

Current cost being removed: every BP call zero-inits a 25×174 f32 trajectory (17.4 KB) and copies `output_llrs` into it once per iteration even when the decode converges (trajectory is only consumed on FAILURE, by neural-OSD ordering and bp_trajectory_capture); every return heap-allocates a 174-f32 `Vec`.

- [ ] **Step 1: Add a convergence-behavior pin test (this is the "failing test" for a refactor — it pins bit-exactness)**

In the decoder test module:

```rust
#[test]
fn bp_refactor_preserves_llrs_and_trajectory_contract() {
    // A converging case: encode a known message, perfect LLRs.
    // An unconverging case: pure noise LLRs.
    let decoder = LdpcDecoder::new(100, LdpcAlgorithm::SumProduct, true);
    let noise: Vec<f32> = (0..174).map(|i| ((i * 37 % 13) as f32 - 6.0) * 0.11).collect();
    let (llrs, traj, (iters, min_llr)) =
        decoder.belief_propagation_with_features(&noise).unwrap();
    assert!(traj.is_some(), "non-converging input must yield a trajectory");
    assert_eq!(llrs.len(), 174);
    assert!(iters >= 1);
    assert!(min_llr.is_finite());
}
```

(Adapt constructor call to the actual `LdpcDecoder` constructor signature in the file; keep the assertions.) Capture the exact `(llrs, iters, min_llr)` values printed once before the refactor and assert equality after — bit-exact pin.

- [ ] **Step 2: Refactor trajectory to lazy Box**

In `belief_propagation_with_features`:
- Replace `let mut trajectory = [[0.0f32; 174]; 25];` with `let mut trajectory: Option<Box<[[f32; 174]; 25]>> = None;`
- The layered loop's per-iteration store `trajectory[iteration] = output_llrs;` becomes:

```rust
if iteration < max_iters {
    trajectory
        .get_or_insert_with(|| Box::new([[0.0f32; 174]; 25]))
        [iteration] = output_llrs;
}
```

- The convergent early-return becomes `return Ok((output_llrs, None, (iters_used, min_llr)));` — and to make the convergent path truly copy-free, hoist the syndrome check BEFORE the trajectory store within each iteration (check `check_syndrome_fast(&total)` first; only record the trajectory row when the syndrome did NOT clear). This preserves the failure-path trajectory contents exactly (rows only matter on failure).
- The failure return becomes `Ok((output_llrs, Some(trajectory.take().unwrap_or_else(|| Box::new([output_llrs; 25])))), ...)` with the existing tail-fill loop applied to the boxed array.
- Apply the same treatment to the flooding branch below the layered one.

- [ ] **Step 3: Change return types Vec→array at all BP variants**

`rg -n "output_llrs.to_vec\(\)" pancetta-ft8/src/decoder.rs` — convert every site (6 at plan time) to return the array; update the fn signatures (`Vec<f32>` → `[f32; 174]`) and each caller (callers currently do `decoded_llrs[..174]` slicing — arrays deref to slices, so most call sites need only type-annotation changes; fix compile errors as the compiler surfaces them).

- [ ] **Step 4: Verify bit-exact + benchmark**

Run: `cargo test -p pancetta-ft8 --features transmit 2>&1 | tail -3` → PASS, and
`cargo run --release -p pancetta-ft8 --example profile_decode -- native 5` → decode count still 8; note ms delta.

- [ ] **Step 5: Commit**

```bash
git add pancetta-ft8/src/decoder.rs
git commit -m "perf(ft8): lazy BP trajectory + array returns — no per-call 17.4KB zero/copy on convergent path (F2)"
```

---

### Task 3: OSD sort key + hot-path alloc trims (F6) [BIT-EXACT]

**Files:**
- Modify: `pancetta-ft8/src/osd.rs` (reliability sort), `pancetta-ft8/src/decoder.rs` (`llrs_for_osd` copy at ~10144, `maybe_whiten_llrs` temporaries)
- Test: existing suite.

**Interfaces:** none new — internal-only.

- [ ] **Step 1: OSD sort key precompute**

In `osd.rs`, find the reliability ordering sort whose comparator calls `.abs()` (decoder-analysis A7b: ~1,300 comparator calls recompute `.abs()` even at depth 0). Replace with:

```rust
let mut order: Vec<(f32, usize)> = llrs.iter().map(|v| v.abs()).zip(0..).collect();
order.sort_unstable_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
```

(match the existing ascending/descending direction exactly — read the current comparator first and preserve its order and its tie behavior).

- [ ] **Step 2: Remove the `llrs_for_osd` copy when `bp_offset_subtract == 0.0`**

At decoder.rs ~10144 the no-op branch is `decoded_llrs[..174].to_vec()`. With Task 2's array return, `decoded_llrs` is already `[f32;174]` — borrow it directly (`let llr_arr: &[f32; 174] = &decoded_llrs;`) and only allocate in the `bp_offset_subtract > 0.0` branch.

- [ ] **Step 3: Test + benchmark + commit**

Run: `cargo test -p pancetta-ft8 --features transmit 2>&1 | tail -3` → PASS; harness decode count 8.

```bash
git add pancetta-ft8/src/osd.rs pancetta-ft8/src/decoder.rs
git commit -m "perf(ft8): precompute OSD sort keys, drop no-op LLR copies (F6)"
```

---

### Task 4: Flatten the spectrogram (F3) [BIT-EXACT]

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` — `struct Spectrogram` (~1357), `compute_spectrogram_with` (~3389), and every reader (16 `.power[` sites at plan time: `compute_costas_score_groups` ~4263/4284/4294, `lookup_time_interp`, `par_extract_symbols_from_spectrogram`, `average_spectrum_per_bin`, `noise_floor_db_median`, cross-cycle + coherent-subtract passes, `par_estimate_snr_spectrogram`). Same for the `complex` twin.
- Test: existing suite (covers all readers via fixture decode-count tests).

**Interfaces:**
- Produces (used by Tasks 7 and Phase 2):

```rust
struct Spectrogram {
    /// Flattened [time_step][freq_sub][freq_bin] — index via `Self::idx`.
    power: Vec<f64>,
    complex: Option<Vec<Complex<f64>>>, // same indexing
    num_steps: usize,
    num_bins: usize,
    freq_osr: usize,
    time_padding: usize,
}
impl Spectrogram {
    #[inline(always)]
    fn idx(&self, t: usize, sub: usize, bin: usize) -> usize {
        (t * self.freq_osr + sub) * self.num_bins + bin
    }
    #[inline(always)]
    fn at(&self, t: usize, sub: usize, bin: usize) -> f64 {
        self.power[self.idx(t, sub, bin)]
    }
    #[inline(always)]
    fn row(&self, t: usize, sub: usize) -> &[f64] {
        let base = (t * self.freq_osr + sub) * self.num_bins;
        &self.power[base..base + self.num_bins]
    }
}
```

- [ ] **Step 1: Record the pre-change fixture baseline** — run the harness (`native 5`), and `cargo test -p pancetta-ft8 --features transmit -- wav_decode 2>&1 | tail -5`; save decode counts.

- [ ] **Step 2: Swap the struct + builder.** Change the struct as above; in `compute_spectrogram_with`, allocate `vec![0.0f64; num_steps * freq_osr * num_bins]` once and write via `idx()` where the jagged pushes were. Mirror for `complex` when `cross_cycle_coherent` is on.

- [ ] **Step 3: Convert readers mechanically.** `spec.power[t][sub][bin]` → `spec.at(t, sub, bin)`; the hoisted `let row = &spec.power[time_idx][freq_sub];` in the Costas kernel → `let row = spec.row(time_idx, freq_sub);` (indexing into `row` unchanged). Mutating sites in the subtract passes use `let i = spec.idx(..); spec.power[i] = ...`. Compile-error-driven: the type change finds every site.

- [ ] **Step 4: Verify bit-exact + benchmark.** Full `cargo test -p pancetta-ft8 --features transmit` → PASS; fixture decode counts identical to Step 1; harness ms in commit message (expect a visible win on the Costas-heavy share).

- [ ] **Step 5: Commit**

```bash
git add pancetta-ft8/src/decoder.rs
git commit -m "perf(ft8): flatten spectrogram to contiguous storage (F3) — kills triple-Vec pointer-chase in Costas kernel"
```

---

### Task 5: Padé `fast_atanh` (F1) [A/B]

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs:9688` (`fast_atanh`), `Ft8Config` (new flag) + its `Default`
- Test: new unit test + research-harness A/B.

**Interfaces:**
- Produces: `Ft8Config.pade_atanh: bool` (default **false** until the A/B passes; the flip is Step 6).

- [ ] **Step 1: Write the accuracy unit test**

```rust
#[test]
fn pade_atanh_matches_ln_form_in_bp_operating_range() {
    // BP products of ≤6 tanh(|x|/2) values rarely approach ±1; the Padé
    // (from MIT ft8_lib, vendor/ft8_lib/ft8/ldpc.c) is accurate to ~1e-4
    // for |x| ≤ 0.90 and saturates gracefully above.
    for i in 0..=1800 {
        let x = (i as f32 / 1000.0) - 0.9;
        let exact = 0.5 * ((1.0 + x) / (1.0 - x)).ln();
        let pade = fast_atanh_pade(x);
        assert!((exact - pade).abs() < 2e-3, "x={x}: exact={exact} pade={pade}");
    }
}
```

- [ ] **Step 2: Implement behind the flag**

```rust
/// Padé approximant for atanh, matching ft8_lib's approach
/// (vendor/ft8_lib/ft8/ldpc.c, MIT). Saturates near |x|→1 (max ≈ 2.28 at
/// the BP clamp) vs the ln form's ≈ 8.4 — hence the A/B gate.
#[inline]
fn fast_atanh_pade(x: f32) -> f32 {
    let x2 = x * x;
    let a = x * (945.0 + x2 * (-735.0 + x2 * 64.0));
    let b = 945.0 + x2 * (-1050.0 + x2 * 225.0);
    a / b
}
```

Thread the flag: `LdpcDecoder` gains `pade_atanh: bool` from `Ft8Config`; the two `fast_atanh(product)` call sites (layered ~10486 + flooding branch) become `if self.pade_atanh { fast_atanh_pade(product) } else { fast_atanh(product) }` — measure; if the branch itself shows in the profile, split the loop instead. Add the config field with doc comment and `Default` false.

- [ ] **Step 3: Bench both settings** — harness with the flag hacked on locally: expect ~−20% total at production config, decode count unchanged on the fixture.

- [ ] **Step 4: Research-harness A/B (the gate).** Follow the standing research workflow (`research/README.md`, `feedback_research_experiment_workflow`): experiment branch, `cargo run -p pancetta-research --release --bin eval -- --help` then eval per its usage on the hard-200 corpus with flag on vs `research/scorecards/main.json`, `--bin compare`, journal in `research/experiments/2026-XX-XX-pade-atanh.md`. Gate: recall within CI of baseline, ΔFP ≤ 2×ΔTP, elapsed improved. **If recall regresses:** implement the piecewise fallback (Padé for |x| ≤ 0.95, ln form above) and re-run.

- [ ] **Step 5: Commit implementation (flag off)** then **Step 6: flip default to true in a separate commit citing the A/B journal.**

```bash
git commit -m "perf(ft8): Padé fast_atanh behind Ft8Config::pade_atanh (F1, default off pending A/B)"
# after gate passes:
git commit -m "perf(ft8): default pade_atanh=true — hard-200 A/B <numbers>, −20% wall at production config"
```

---

### Task 6: Flip `costas_half_loop_disabled` default (F5) [A/B]

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs:1235` (`costas_half_loop_disabled: false` → `true` after A/B)
- Test: research-harness A/B only (mechanism + tests exist since Batch 92; see ~10998/11003 test pins).

- [ ] **Step 1: A/B on the research harness** — same workflow as Task 5 Step 4, experiment = default-flip. Batch 92 already argued redundancy at `TIME_OSR ≥ 2`; the A/B confirms on hard-200.
- [ ] **Step 2: Flip the default + update the Batch 92 comment; run full suite (two pinned tests at ~10998/11003 may assert the default — update them intentionally).**
- [ ] **Step 3: Commit** — `git commit -m "perf(ft8): default costas_half_loop_disabled=true (F5) — Batch 92 redundancy, A/B <journal ref>"`

---

### Task 7: f32 real-FFT spectrogram (F4) [A/B]

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` — planner (~1613–1627), `compute_spectrogram_with`, `Spectrogram` scalar type, `log10` sites; `pancetta-ft8/Cargo.toml` if adopting `realfft`.
- Depends on: Task 4 (layout already flat).

**Interfaces:**
- Produces: `Spectrogram { power: Vec<f32>, complex: Option<Vec<Complex<f32>>> , .. }` and `at()/row()` returning f32. Downstream f64 consumers take `as f64` at the boundary initially (keeps the diff reviewable); Costas scoring switches to f32 accumulation only if the A/B is clean.

- [ ] **Step 1:** Introduce a `SpecScalar` type alias (`type SpecScalar = f64;`) and convert the spectrogram build + storage + readers to the alias with explicit casts at boundaries. Test: bit-exact suite still passes (alias = f64, zero change). Commit as refactor.
- [ ] **Step 2:** Flip the alias to `f32`, change the FFT plan to `FftPlanner::<f32>` (or `realfft::RealFftPlanner` for the real-input transform — preferred: input is real audio; halves FFT work), `log10` → `log10f` (`f32::log10`). Fix casts.
- [ ] **Step 3:** Fixture check — decode counts on all WAV fixtures; expect identical (f32 has ~7 significant digits; spectrogram dB values are compared at ~0.1 dB granularity). Bench: expect most of the ~24% FFT/log share to shrink.
- [ ] **Step 4:** Research-harness A/B (same workflow as Task 5 Step 4) since numerics changed. Gate passes → keep; regression → investigate which stage is precision-sensitive before conceding (the complex subtract path is the likely suspect; it may stay f64 independently via its own alias).
- [ ] **Step 5:** Commit — `git commit -m "perf(ft8): f32 real-input FFT + f32 spectrogram (F4) — A/B <journal ref>"`

**Phase 1 checkpoint:** re-run the full ablation sweep from the harness and record the new production-config single-thread number in `research/experiments/` (expect ~60–100 ms vs 240 ms). Push the batch (controller-only, one gate run).

---

## Phase 2 — Anytime decoder

### Task 8: `DecodeBudget` type + plumbing (no behavior change)

**Files:**
- Create: `pancetta-ft8/src/budget.rs`
- Modify: `pancetta-ft8/src/lib.rs` (export), `pancetta-ft8/src/decoder.rs` — add a `budget: DecodeBudget` field threaded from a new entry point.
- Test: `pancetta-ft8/src/budget.rs` unit tests.

**Interfaces:**
- Produces (consumed by Tasks 9–12):

```rust
// pancetta-ft8/src/budget.rs
use std::time::Instant;

/// Wall-clock budget for one decode window. `deadline == None` is unlimited
/// (tests, research harness, `max` preset). Checked BETWEEN work items only.
#[derive(Debug, Clone, Copy)]
pub struct DecodeBudget {
    deadline: Option<Instant>,
}

impl DecodeBudget {
    pub fn unlimited() -> Self { Self { deadline: None } }
    pub fn until(deadline: Instant) -> Self { Self { deadline: Some(deadline) } }
    /// True when work may continue.
    #[inline]
    pub fn has_time(&self) -> bool {
        self.deadline.map(|d| Instant::now() < d).unwrap_or(true)
    }
}

/// Per-window telemetry filled in by the stage driver.
#[derive(Debug, Clone, Default)]
pub struct DecodeBudgetReport {
    /// (stage label, elapsed ms, items done, items skipped)
    pub stages: Vec<(&'static str, u32, u32, u32)>,
    pub budget_exhausted: bool,
}
```

- Produces: `Ft8Decoder::decode_window_budgeted(&mut self, samples: &[f32], budget: DecodeBudget) -> Ft8Result<(Vec<DecodedMessage>, DecodeBudgetReport)>` — all existing entry points (`decode_window`, `decode_window_with_ap_scoped_partner`, …) delegate with `DecodeBudget::unlimited()` and discard the report, so every existing caller and test is byte-identical.

- [ ] **Step 1:** Write `budget.rs` with the code above + tests:

```rust
#[test]
fn unlimited_always_has_time() { assert!(DecodeBudget::unlimited().has_time()); }
#[test]
fn expired_deadline_has_no_time() {
    let b = DecodeBudget::until(Instant::now() - std::time::Duration::from_millis(1));
    assert!(!b.has_time());
}
```

- [ ] **Step 2:** Add `decode_window_budgeted`; store the budget in a `self.current_budget` field (set at entry, reset to unlimited at exit) so deep helpers can read it without threading a param through every private fn. Existing entry points delegate.
- [ ] **Step 3:** Full suite → PASS (nothing consults the budget yet). Commit: `feat(ft8): DecodeBudget type + budgeted entry point (inert)`.

---

### Task 9: Floor/budgeted candidate split (stages S1/S2)

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` — the candidate `par_iter` block (~2350–2379) and `Ft8Config` (two new fields).
- Test: new fixture test + suite.

**Interfaces:**
- Produces: `Ft8Config { pub floor_candidates: usize /* default 50 */, pub floor_iters: usize /* default 25 — consumed by Task 10 */ }`.
- Consumes: `self.current_budget` from Task 8.

- [ ] **Step 1: Failing test** — with a tiny budget, decode still returns the floor results:

```rust
#[test]
fn budget_floor_still_decodes_top_candidates() {
    let samples = read_fixture_window("wsjt/210703_133430.wav"); // helper mirrors wav_decode_tests.rs
    let mut d = Ft8Decoder::new(Ft8Config::default()).unwrap();
    let (msgs, report) = d
        .decode_window_budgeted(&samples, DecodeBudget::until(std::time::Instant::now()))
        .unwrap();
    assert!(report.budget_exhausted);
    assert!(msgs.len() >= 7, "floor (top-50 @ shallow BP) must still run, got {}", msgs.len());
}
```

Expected first run: FAIL (budget not consulted → `budget_exhausted` false).

- [ ] **Step 2: Implement the split.** Partition `sync_candidates` into `floor = ..min(floor_candidates, len)` and `rest`. Decode `floor` exactly as today (Rayon `par_iter`). Then, **only if `self.current_budget.has_time()`**, decode `rest` the same way, checking the budget between chunks: use `rest.par_chunks(8)` with a shared `AtomicBool` set by the driver when time expires; workers check it per candidate and yield `None` past expiry. Record stage telemetry (`"S1-floor"`, `"S2-rest"`) into the report.
- [ ] **Step 3:** Suite + fixture decode counts unchanged with unlimited budget (the partition is order-preserving: floor∪rest = the same ranked list). New test passes. Bench. Commit: `feat(ft8): stage S1/S2 candidate split under DecodeBudget`.

---

### Task 10: BP escalation ladder (S3) [A/B]

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` — `LdpcDecoder` (continuation support), `par_decode_candidate` (record failure state), new `escalate_bp_failures` stage fn; `Ft8Config` (`deep_iters: usize` default 100, `escalation_parity_max: usize` default 24, `escalation_enabled: bool` default false until A/B).
- Test: continuation-equivalence unit test + harness A/B.

**Interfaces:**
- Consumes: Task 2's BP signature; Task 9's `floor_iters`.
- Produces:

```rust
/// Saved BP state for a failed candidate, allowing continuation.
struct BpContinuation {
    c2v: [[f32; 7]; 83],
    total: [f32; 174],        // running posteriors (layered) at floor_iters
    parity_errors: usize,     // at floor_iters — escalation sort key
    // candidate identity for re-emission:
    candidate: CostasCandidate,
    channel_llrs: [f32; 174],
}
fn belief_propagation_continue(
    &self,
    cont: &mut BpContinuation,
    additional_iters: usize,
) -> ([f32; 174], Option<Box<[[f32; 174]; 25]>>, (u8, f32))
```

- [ ] **Step 1: Failing equivalence test** (the mathematical core — layered BP state is exactly (c2v, posteriors), so 25-then-continue-75 must equal flat-100):

```rust
#[test]
fn bp_continuation_equals_flat_run() {
    let noise: Vec<f32> = (0..174).map(|i| ((i * 41 % 17) as f32 - 8.0) * 0.13).collect();
    let flat = LdpcDecoder::new(100, ...).belief_propagation_with_features(&noise).unwrap();
    let short = LdpcDecoder::new(25, ...);
    let (out25, _, _) = short.belief_propagation_with_features(&noise).unwrap();
    let mut cont = short.take_continuation(&noise, &out25); // returns BpContinuation
    let (out100, _, _) = short.belief_propagation_continue(&mut cont, 75);
    assert_eq!(flat.0, out100, "continued BP must be bit-identical to flat 100-iter run");
}
```

Expected: FAIL (`take_continuation` undefined).

- [ ] **Step 2: Implement continuation.** Factor the layered iteration body into a helper both `belief_propagation_with_features` and `belief_propagation_continue` call; `with_features` optionally exports `(c2v, total)` on failure (behind a `want_continuation: bool` to avoid the 2.3 KB copy when the ladder is off). Make the equivalence test pass **bit-exactly** (same syndrome-check placement, same trajectory row semantics — trajectory rows for iterations ≥ 25 are filled by the continuation run).
- [ ] **Step 3: Wire the stage.** With `escalation_enabled`: S1/S2 run BP at `floor_iters`; failures with `parity_errors <= escalation_parity_max` push a `BpContinuation`. Stage S3 sorts by `parity_errors` ascending and, budget-permitting per candidate, continues each to `deep_iters`, feeding successes through the SAME post-BP path (CRC, OSD/neural-OSD, message assembly) as a flat success — extract that post-BP block into a helper if needed rather than duplicating it. With the flag OFF, S1/S2 run at `ldpc_iterations` (100) exactly as today.
- [ ] **Step 4: Measurement sub-task (sets `escalation_parity_max` with data).** Instrument on the harness corpus: for every candidate that fails at 25 but succeeds by 100, log its parity-error count at 25. Pick the threshold covering ≥ 99% of those successes; record the histogram in the experiment journal.
- [ ] **Step 5: A/B gate** (workflow per Task 5 Step 4): `escalation_enabled=true` + `floor_iters=25` vs baseline flat-100. Gate: recall within CI (the equivalence argument says identical for escalated candidates; the threshold excludes some hopeless ones — the measurement task bounds that loss), elapsed strongly improved. Flip default on pass.
- [ ] **Step 6: Commit** implementation and default-flip separately, citing the journal.

---

### Task 11: Budget checkpoints around S4–S7 + superset invariant

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` — the post-par_iter stage block (~2406–2530: cross-cycle at 2413, multipass rounds at 2458, joint-pair at 2496, a7 at ~2532).
- Test: superset-invariant integration test.

**Interfaces:** consumes `self.current_budget` + report from Task 8.

- [ ] **Step 1: Failing test — unlimited budget is a superset of baseline:**

```rust
#[test]
fn unlimited_budget_matches_baseline_decode_set() {
    let samples = read_fixture_window("wsjt/210703_133430.wav");
    let baseline: std::collections::BTreeSet<String> = {
        let mut d = Ft8Decoder::new(Ft8Config::default()).unwrap();
        d.decode_window(&samples).unwrap().into_iter().map(|m| m.text).collect()
    };
    let budgeted: std::collections::BTreeSet<String> = {
        let mut d = Ft8Decoder::new(Ft8Config::default()).unwrap();
        d.decode_window_budgeted(&samples, DecodeBudget::unlimited())
            .unwrap().0.into_iter().map(|m| m.text).collect()
    };
    assert!(budgeted.is_superset(&baseline), "missing: {:?}", baseline.difference(&budgeted));
}
```

Extend to every WAV fixture in `tests/fixtures/wav/wsjt/` via a loop.

- [ ] **Step 2: Add the checkpoints.** Wrap each stage entry: `if !self.current_budget.has_time() { report.budget_exhausted = true; break-to-finalize; }` — cross-cycle (per pass), multipass (per round, inside the existing `for _round` loop), joint-pair (per pass), a7 (per pass). Time each stage into the report. The finalize path (dedup + return) is the existing tail code — never skipped.
- [ ] **Step 3:** Suite + invariant test PASS; a small-budget run of the harness shows stages skipped in the report. Commit: `feat(ft8): budget checkpoints for cross-cycle/multipass/joint-pair/a7 stages`.

---

### Task 12: Coordinator deadline wiring

**Files:**
- Modify: `pancetta/src/coordinator/ft8.rs` (decode call sites ~570 and ~708), `pancetta/src/coordinator/mod.rs` (effort state — placeholder atomic until Task 14 maps presets), `pancetta-core/src/slot.rs` reference only (mode ceilings derive from the active protocol timing).
- Test: unit test for the ceiling fn; `coord_sim` unaffected (decode not in its path).

**Interfaces:**
- Produces:

```rust
/// Ceiling on decode wall time so the DSP pipeline never backs up.
/// FT8: window every 15 s, decode-phase at 13 s → ceiling 2000 ms.
/// FT4: window every 7.5 s, decode-phase at 6.5 s → ceiling 800 ms.
fn decode_budget_ceiling_ms(slot_ns: u64) -> u64 {
    if slot_ns <= 7_500_000_000 { 800 } else { 2000 }
}
```

- Produces: `decode_effort_budget_ms: Arc<AtomicU64>` on the coordinator (0 = unlimited), read each window; Task 14 writes it from config/TUI.

- [ ] **Step 1:** Unit-test `decode_budget_ceiling_ms` (FT8 → 2000, FT4 → 800; boundary at 7.5 s inclusive).
- [ ] **Step 2:** At both decode call sites compute `let budget = { let cfg = decode_effort_budget_ms.load(Relaxed); let ceil = decode_budget_ceiling_ms(active_slot_ns.load(Relaxed)); let ms = if cfg == 0 { ceil } else { cfg.min(ceil) }; DecodeBudget::until(window_ready_at + Duration::from_millis(ms)) };` and call `decode_window_budgeted`. Log the report at `debug!(target: "decode.budget", ...)` and forward `(elapsed_ms, budget_exhausted)` on the existing decode-metrics path to the TUI (extend the metrics struct minimally).
- [ ] **Step 3:** Full workspace suite; run the app briefly (`--headless`) to confirm the debug log line appears per window. Commit: `feat(coordinator): mode-aware decode deadline wiring`.

---

## Phase 3 — Effort control surface

### Task 13: `[decoder]` config section

**Files:**
- Create: `pancetta-config/src/decoder.rs`
- Modify: `pancetta-config/src/lib.rs` (register section, add to root config struct + its `merge_with`/validate)
- Test: in-file unit tests.

**Interfaces:**
- Produces:

```rust
// pancetta-config/src/decoder.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DecodeEffort {
    #[default]
    Auto,
    Eco,
    Standard,
    Deep,
    Max,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DecoderConfig {
    /// Effort preset; `auto` maps from the hardware tier probe.
    pub effort: DecodeEffort,
    /// Explicit per-window budget in ms; overrides `effort` when Some.
    pub budget_ms: Option<u64>,
}
impl Default for DecoderConfig {
    fn default() -> Self { Self { effort: DecodeEffort::Auto, budget_ms: None } }
}
```

with `ConfigSection` impl (mirror `pancetta-config/src/audio.rs:725`'s pattern), `merge_with` copying BOTH fields, and `validate` rejecting `budget_ms == Some(0)`.

- [ ] **Step 1: Failing tests first** (parse round-trip incl. `effort = "deep"` string form; validation rejects 0; **`merge_with_carries_over_decoder_section`** asserting both fields survive a merge — the 2026-07-05 bug-class regression test).
- [ ] **Step 2: Implement; suite passes.**
- [ ] **Step 3: Commit:** `feat(config): [decoder] effort/budget_ms section with merge_with regression test`.

---

### Task 14: Preset mapping + tier auto-selection

**Files:**
- Create: `pancetta/src/coordinator/effort.rs`
- Modify: `pancetta/src/coordinator/mod.rs` (seed the atomic from config at startup), `pancetta/src/coordinator/tier.rs` (retire the Slow-tier `max_decode_passes`/`max_sync_candidates` `Ft8Config` rewrites — the eco preset replaces them; `scoped_fast_path` handling is UNTOUCHED)
- Test: `effort.rs` unit tests.

**Interfaces:**
- Consumes: `DecodeEffort` (Task 13), `HardwareTier` (existing, `tier.rs`), `decode_effort_budget_ms` atomic (Task 12).
- Produces:

```rust
// pancetta/src/coordinator/effort.rs
/// Budget in ms for a preset; 0 = unlimited sentinel (matches the atomic's contract).
/// Eco is the floor-only budget: 1 ms forces floor-stages-only deterministically.
pub fn preset_budget_ms(effort: DecodeEffort, tier: HardwareTier) -> u64 {
    match effort {
        DecodeEffort::Eco => 1,
        DecodeEffort::Standard => 250,
        DecodeEffort::Deep => 1000,
        DecodeEffort::Max => 0,
        DecodeEffort::Auto => match tier {
            HardwareTier::Slow => 1,
            HardwareTier::Moderate => 250,
            HardwareTier::Fast => 1000,
        },
    }
}
```

(Values are the spec's starting points — revisit against post-Phase-1 measurements in the A/B journal before the on-air soak; `budget_ms` config override wins over the preset at the seeding site.)

- [ ] **Step 1:** Unit tests for the mapping (each arm + auto×tier matrix).
- [ ] **Step 2:** Seed `decode_effort_budget_ms` at coordinator startup: `config.decoder.budget_ms.unwrap_or_else(|| preset_budget_ms(config.decoder.effort, tier))`; re-seed on tier-probe completion (the probe is async — mirror how tier.rs currently applies its result) and on config hot-reload.
- [ ] **Step 3:** Delete the Slow-tier `Ft8Config` rewrite block in `tier.rs` (keep `scoped_fast_path`); update its module doc + the CLAUDE.md tier bullet.
- [ ] **Step 4:** Full suite; commit: `feat(coordinator): effort preset→budget mapping, tier auto-selection subsumes Slow-tier config rewrites`.

---

### Task 15: TUI effort cycling + status chip

**Files:**
- Modify: `pancetta-tui/src/tui_runner.rs` (key handler — `e` is unbound at plan time per the key audit; re-verify against `app.rs` handlers before binding), `pancetta-tui/src/app.rs` (state), `pancetta-tui/src/ui/mod.rs` (status chip), `pancetta/src/coordinator/tui_relay.rs` (command handler), `pancetta/src/message_bus.rs` (message variants — mirror the Shift+M `CycleOperatingMode` plumbing exactly)
- Test: relay round-trip unit test (mirror the existing mode-switch test in `coord_sim` style where applicable).

**Interfaces:**
- Produces: `TuiCommand::CycleDecodeEffort`, echo `MessageType::DecodeEffortStatus { effort: String, budget_ms: u64 }` → `TuiMessage::DecodeEffortUpdate`, chip text `DECODE: <PRESET> <last_ms>ms` with a trailing `✂` when the last window's report had `budget_exhausted` (fed by Task 12's metrics extension).
- Cycle order: Eco → Standard → Deep → Max → Auto → Eco. Takes effect next window (writes the shared atomic via `preset_budget_ms`); **no active-QSO gate needed** (budget changes never invalidate in-flight state — invariant from the spec §6.2).

- [ ] **Step 1:** Key audit re-check: `rg "Char\('e'\)" pancetta-tui/src/` → expect no hits; if taken, fall back to `E`.
- [ ] **Step 2:** Failing relay test: send `CycleDecodeEffort`, assert the atomic changed and the echo message came back with the next preset name.
- [ ] **Step 3:** Implement (key → command → relay handler cycles a `current_effort` state on the coordinator, writes the atomic, echoes status; TUI renders chip; persist the operator's chosen preset in `~/.pancetta/tui_state.json` alongside the existing view persistence).
- [ ] **Step 4:** Full suite + manual TUI smoke (`cargo run --release -- --headless` for pipeline, then TUI run: press `e`, watch the chip). Commit: `feat(tui): live decode-effort cycling ('e') + status chip`.

---

### Task 16: Docs + final gate

**Files:**
- Modify: `CLAUDE.md` (new decoder-effort bullet; update tier bullet + known-gaps), `docs/ARCHITECTURE.md` (decode pipeline stages diagram/paragraph), `README.md`/`FEATURES.md` (operator-facing effort knob), `research/hypothesis_bank.md` (retire/annotate items obsoleted by escalation ladder, e.g. flat-iteration sweeps)
- Test: none (docs).

- [ ] **Step 1:** Write the docs (CLAUDE.md bullet summarizes: budget-governed anytime decoder, stage order, effort presets, `e` key, config section, tier subsumption, and the Phase-1 fix list with measured numbers).
- [ ] **Step 2:** Full workspace suite one final time: `cargo test --workspace --features transmit` + `cargo test -p pancetta --test loopback_qso` + `cargo test -p pancetta-hamlib --lib -- --test-threads=1`.
- [ ] **Step 3:** Record the final before/after benchmark table in `research/experiments/2026-XX-XX-decoder-speed-overhaul.md`.
- [ ] **Step 4:** Commit docs; controller pushes the batch. **On-air soak** (operator-gated, per spec §7.5): run `deep` vs `auto` sessions comparing decode counts + telemetry before declaring success criteria met.

---

## Self-review notes (author)

- Spec coverage: F1→T5, F2→T2, F3→T4, F4→T7, F5→T6, F6→T3, F7→T1; anytime §5→T8–T12 (S1/S2→T9, escalation §5.2→T10, checkpoints §5.3→T11+T12, determinism §5.4→Global Constraints); control surface §6→T13–T15; validation §7→embedded per task + T16; success criteria §8→T16 Step 3/4.
- Type consistency: `DecodeBudget`/`DecodeBudgetReport` (T8) consumed by T9/T11/T12; `floor_iters` (T9) consumed by T10; `preset_budget_ms` (T14) consumed by T15; BP array signature (T2) consumed by T10.
- Known deliberate deferrals to execution time (not placeholders): exact `LdpcDecoder` constructor signatures in test snippets (adapt to file reality), research-harness CLI flags (read `--help`/README at run time per standing workflow), preset budget values revisited post-Phase-1 measurement (spec says so).
