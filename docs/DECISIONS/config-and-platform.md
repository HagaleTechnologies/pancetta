<!-- Covers: config merge, hardware tier, band-switch, decoder harness/speed. Content moved from CLAUDE.md 2026-07-07.
     For current behavior trust the code and specs in `docs/superpowers/specs/`. -->

# Config and platform

## `merge_with` field-drop bug postmortem (FIXED 2026-07-05)

Several hand-written `ConfigSection::merge_with` impls silently dropped fields added after the impl was written, reverting them to compiled-in defaults regardless of the operator's config file. Root-caused via a live operator report (QRZ Logbook upload enabled in `pancetta.toml` but never firing): every `merge_with` is a manually-maintained list of `self.field = other.field` lines, so a field added to the struct later is trivially forgotten and the bug is invisible (both the parse and the validation pass — only the *merge* silently drops it). Confirmed two real, currently-consumed bugs and fixed both:

- `NetworkConfig::merge_with` only copied `psk_reporter`/`wspr`/`dx_cluster`/`web_api`/`proxy` — `tls`, `rate_limiting`, `reliability`, `cqdx`, `clublog`, `qrz_logbook`, `lotw`, `eqsl`, `qrz_xml`, `remote_gateway`, `station_agent` were never merged. Fixed in `pancetta-config/src/network.rs`; regression test `merge_with_carries_over_every_opt_in_integration`.
- `StationConfig::merge_with` never merged `tx_late_max_ms`/`tx_self_parity`/`ptt_lead_ms` — these drive the live TX-slot scheduler and PTT keying lead time in `coordinator/tx.rs`, so an operator customizing any of the three via config would always get the compiled-in defaults (8000ms / Auto / 80ms) instead. Fixed in `pancetta-config/src/station.rs`; regression test `merge_with_carries_over_tx_timing_fields`.
- Also closed `AudioConfig.bit_depth` (`pancetta-config/src/audio.rs`) opportunistically — currently unused elsewhere in the codebase, so not user-impacting today, but the same latent bug.
- **Audited and left alone (real gaps, but currently inert — nothing in the codebase reads these fields, so they don't affect behavior today):** `UiConfig` (`typography`/`colors`/`panels`/`accessibility`/`animations`/`toolbars`/`status_bar`/`keyboard`/`logging`/`spectrum`, plus most of `WindowConfig`) and `CatInterfaceConfig` (`data_bits`/`stop_bits`/`parity`/`flow_control`/`timeout_ms`/`protocol`/`termination`/`response_timeout_ms`/`retry_count` — only `port`/`baud_rate`/`enabled` are actually wired to hamlib). Worth fixing if/when any of these get wired up; not worth the churn while they're dead config.
- **Process gap:** nothing enforces that a new `NetworkConfig`/`StationConfig`/etc. field gets a `merge_with` line — this class of bug can recur. A macro-derived or reflection-based merge (or a test that diffs struct fields against merge_with via `serde_json::to_value` round-tripping) would close this structurally. (A structural `merge_with` guardrail test per `ConfigSection` is planned — not yet implemented; see `pancetta-config` `merge_with_*` for the existing regression tests.)

## Hardware-tier auto-classification

`pancetta/src/coordinator/tier.rs`, hb-216 S2: on coordinator startup, the host is classified into Fast / Moderate / Slow via a background `probe_hardware_tier(10)` call (or a cache hit from `~/.pancetta/tier_cache.json` keyed on `(cpu_model, core_count, pancetta_version)`). Moderate/Slow tiers flip the `scoped_fast_path: Arc<AtomicBool>` (replaces the old env-var read in the FT8 hot loop) — this mechanism is unchanged. Operator override: `PANCETTA_SCOPED_FAST_PATH=1` forces on, `=0` forces off, both skip the tier-driven `scoped_fast_path` decision. **Tier-driven `Ft8Config` rewrites retired (decoder-speed-overhaul Task 14):** the old ad-hoc per-tier `Ft8Config` field rewrites (Fast preset `mp=2, ldpc=200` retired Batch 83; Slow preset `max_decode_passes=1` + `max_sync_candidates=150` from Batch 78/72) are gone — `apply_tier` no longer touches `Ft8Config` at all. Tuning decode thoroughness per tier is now owned by the **`[decoder]` effort-preset system** (`pancetta/src/coordinator/effort.rs`): `preset_budget_ms(effort, tier)` maps a `DecodeEffort` (`Auto`/`Eco`/`Standard`/`Deep`/`Max`, config `[decoder].effort`) — plus, for `Auto`, the probed `HardwareTier` — to a per-window wall-time budget in ms, written into the `decode_effort_budget_ms` atomic (Task 12) that the FT8 decode loop reads. An explicit `[decoder].budget_ms` always overrides the preset. Seeded at coordinator startup (assuming Fast until the tier is known) and re-seeded once the tier resolves (cache hit or background-probe completion, mirroring how `scoped_fast_path` itself gets re-applied). Spec: `docs/superpowers/specs/2026-06-04-hb-216-s2-tier-wiring-design.md` (tier probe) and `docs/superpowers/specs/2026-07-06-decoder-speed-overhaul-design.md` §6.1 (effort presets).

## Band-switch in-flight-window flush

`pancetta/src/coordinator/dsp.rs`: on a band change the shared `operating_frequency_hz` atomic flips synchronously but the DSP `ft8_buffer` still holds up to a full window of OLD-band audio. The DSP thread (which already reads that atomic for WAV stamping) detects the switch itself via `is_band_change` against a per-buffer baseline (`band_flush_decision`) and flushes the in-flight audio + waterfall floor history, so the old band's last window is never decoded into the new band as phantom stations. The next full window (~12.64 s later) rebuilds from new-band audio. No new shared state; no changes to the three band-change senders.

## Decoder research harness

`pancetta-research/`, `research/`, `scripts/research-env.sh`: a local-only iteration harness for improving the decoder. Excluded from `default-members` and CI by construction. Spec: `docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md`. Plans 1-3 complete; the loop is operational. Run `./scripts/research-env.sh --status` to see active experiments; read `research/hypothesis_bank.md` for the current backlog.

## Live rig-config switch, and why the hot-reload scaffolding stays dead (2026-09-02)

PAN-59 (user report: switching rig config today requires re-running `pancetta setup` and
restarting, unlike audio's live device picker). Considered wiring up the existing
`classify_config_reload` (`pancetta/src/coordinator/health.rs:112`, already classifies `rig` as
`SafeLive`) + `pancetta_config::ConfigHotReload` file-watcher (`pancetta-config/src/hot_reload.rs`)
— both confirmed to have **zero production call sites** (only direct unit tests). Rejected: it's a
*file-watch* reload, not an in-TUI operator action, so wiring it would be a bigger, differently-
shaped change than reusing the already-proven `TuiCommand::SelectDevice` pattern. Built a dedicated
`TuiCommand::SelectRig` + ratatui modal instead, reusing the existing crash-restart reconnect pair
(`teardown_hamlib()` → `start_hamlib_component()`, `pancetta/src/coordinator/hamlib.rs`) routed
through a new channel `run_main_loop` consumes (Hamlib's reconnect needs `&mut
ApplicationCoordinator`, unlike audio's independent cpal-stream-reopen). No named rig-profile
concept added — out of scope. Full design: `docs/superpowers/specs/2026-09-02-pan-59-live-rig-switch-design.md`.

## Decoder speed overhaul — budget-governed anytime decoder

PAN-7 added the default-off `Ft8Config::ft8lib_sync_seeds_enabled` research path, which runs the vendored MIT ft8_lib candidate finder on pass-0 audio, translates and re-scores its positions on pancetta's Costas lattice, and unions them before truncation. The design is in `docs/superpowers/specs/2026-08-03-ft8lib-sync-seed-union-design.md`; the sixteen cap-200/cap-400 scorecards are under `research/scorecards/pan7/`; and the verdict is recorded in `research/experiments/2026-08-03-ft8lib-sync-seed-union-declined.md`. Both caps produced exact seeded/control nulls: hard-jt9-rich and curated-hard ΔTP=0 with 95% CI `[0,0]`, Δunverified=0, synth recovery unchanged, and noise stayed at the same 1/1000 baseline (incremental ΔFP=0). Seed-survival counters confirmed that representable ft8_lib positions were already present on the exhaustive native lattice and did not survive as novel candidates. The standing TP gate therefore failed and the flag remains `false`; only a separate negative-dt/slot-edge design warrants reopening.

`pancetta-ft8/src/decoder.rs`, `pancetta/src/coordinator/effort.rs`, `pancetta-config/src/decoder.rs`, `pancetta-tui/src/tui_runner.rs`; spec `docs/superpowers/specs/2026-07-06-decoder-speed-overhaul-design.md`, plan `docs/superpowers/plans/2026-07-06-decoder-speed-overhaul.md`, ledger `.superpowers/sdd/progress.md`, final report `research/experiments/2026-07-06-decoder-speed-overhaul-final.md`: a 16-task subagent-driven-development effort making one decode window an **anytime algorithm** — always produces the same result if allowed to run to completion, but can be stopped early under a wall-clock budget and still return everything decoded so far. **Phase 1 (Tasks 1-7, mechanical/A/B fixes):** lazy BP trajectory + array returns (no per-call 17.4KB alloc on the convergent path), OSD sort-key precompute, flattened contiguous spectrogram storage (kills a triple-`Vec` pointer-chase in the Costas kernel), a piecewise Padé `fast_atanh` (`pade_atanh=true`, Padé for `|x|≤0.95` else `ln` — the *raw* Padé form failed its hard-200 A/B gate and was replaced by the piecewise fallback before shipping), and an f32 real-input FFT + f32 spectrogram (`SpecScalar=f32`, `realfft`). Measured on the profiling harness's one fixture (`cargo run --release -p pancetta-ft8 --example profile_decode -- native 10`, `tests/fixtures/wav/wsjt/210703_133430.wav`): **multi-thread 144.01→107.18 ms/window (-25.6%), single-thread 246.43→189.63 ms/window (-23.1%)** — see `research/experiments/2026-07-06-phase1-checkpoint.md` for the full A/B table. This is *less* than the plan's originally-hoped ~60-100ms/~75% reduction: that bigger estimate assumed the stacked effect under a workload that stresses BP/OSD/FFT harder than this one 8-message fixture does — the corpus-level (hard-200) per-task deltas (Padé -16.0% elapsed, f32 FFT -3.8% elapsed) are consistent with the smaller single-fixture number. One planned flip (Task 6, `costas_half_loop_disabled`) was investigated and **did not ship**: the plan's own text asserted Batch 92 supported flipping it, but Batch 92's actual recorded verdict (`research/experiments/2026-06-12-batch-92.md`) was the opposite (flipping costs real recall, explicit do-not-ship) — Task 6 independently re-confirmed this on hard-200 (Δrec=-58) and correctly declined the flip per the standing "gate fails → no flip" rule. **This corrects the plan document's own factual error for future readers: Batch 92 never supported the flip.** **Phase 2 (Tasks 8-12): `DecodeBudget`/`DecodeBudgetReport` + per-window anytime staging.** Decode work is split into ranked stages, each checkpointed against a `DecodeBudget` (unlimited in tests/CI/research harness by construction — no wall-clock reads on the eval path): **S1-floor** (top-ranked sync candidates, unconditional — same recall as the legacy single-pass decoder), **S2-rest** (remaining candidates, budget-gated), **S3** (BP escalation ladder — a floor-then-continue-to-deep BP retry for candidates that fail at the floor iteration count; ships with `escalation_enabled=false` — its own A/B data showed BP iteration count is a small fraction of total decode cost for this decoder, Costas/FFT dominate, so flipping it wasn't worth it, same data-driven non-flip pattern as Task 6), **S4-cross-cycle**, **S5-multipass**, **S6-joint-pair**, **S7-a7** (the existing recall-boosting stages, now each individually budget-gated rather than always running to completion). The coordinator wires a real wall-clock deadline via a `decode_effort_budget_ms: Arc<AtomicU64>` atomic (defaults to `0`/unlimited until Phase 3 seeds it). **KNOWN LIMITATION (S3, currently inert since escalation ships disabled):** the BP escalation ladder escalates **inline, in candidate sync-score-rank order** — NOT the plan's originally-envisioned design of collecting all floor-failures, sorting by parity/promise, and escalating the most-likely-to-succeed candidates first. Harmless today (escalation is off and all production paths use `DecodeBudget::unlimited()`), but if ever enabled under real time pressure, a tight budget could be spent on an early low-promise candidate while skipping a later near-certain one — flagged as a TODO, not built to the original spec. **Phase 3 (Tasks 13-15): operator control surface.** `[decoder]` config section (`effort: DecodeEffort` = `Auto`/`Eco`/`Standard`/`Deep`/`Max`, default `Auto`; `budget_ms: Option<u64>` explicit override, always wins over the preset). `preset_budget_ms(effort, tier)` (`pancetta/src/coordinator/effort.rs`) maps preset+tier to a per-window ms budget — `Eco`=1ms (floor-only, deliberately *not* 0/unlimited — a 1ms budget deterministically stops after S1-floor), `Standard`=250ms, `Deep`=1000ms, `Max`=0 (unlimited), `Auto`→ tier-derived (`Slow`=1, `Moderate`=250, `Fast`=1000). This **subsumes and deletes** the old ad-hoc per-tier `Ft8Config` rewrite hack (Fast `mp=2,ldpc=200` / Slow `max_decode_passes=1,max_sync_candidates=150`) — `apply_tier` no longer touches `Ft8Config` at all; tuning decode thoroughness per tier is now entirely the effort-preset system's job. Seeded at coordinator startup (assumes `Fast` before the tier is known) and re-seeded on tier-probe completion (cache hit or background probe). Live operator control via the **`e`** TUI key (`CycleDecodeEffort` → `Eco→Standard→Deep→Max→Auto→Eco`, no optimistic local flip — waits for the coordinator's authoritative `DecodeEffortUpdate` echo before updating the status chip), persisted to `~/.pancetta/tui_state.json`. **KNOWN LIMITATION: config hot-reload does NOT re-seed the effort budget.** Investigated and found genuinely infeasible today, not a corner cut: `pancetta_config::ConfigHotReload`'s file watcher exists but is never constructed anywhere in the coordinator crate — hot-reload is a documented, deliberate no-op (so a reload can never clobber latched QSO state). Only coordinator startup and tier-probe completion seed the atomic; a config-file edit to `[decoder]` takes effect only on restart (the `e` key is the only *live* control). **Validation:** full workspace suite green throughout every task; decode counts on every WAV fixture unchanged from the pre-Phase-1 baseline (BIT-EXACT tasks) and A/B-gated on hard-200/hard-1000 (perf tasks) per the standing bootstrap-CI ΔFP≤2×ΔTP rule. **Remaining:** on-air soak (operator-gated, per spec §7.5 — compare `deep` vs `auto` effort sessions' decode counts + telemetry) before declaring the full success criteria met.

### Open follow-ups (decoder speed overhaul)

- **BP escalation ladder (S3) escalates in candidate rank order, not by promise** — inert today (`escalation_enabled=false`), but if ever flipped on under real time pressure this could waste a tight budget on an early low-promise candidate while skipping a later near-certain one. Fix is a global collect-sort-escalate restructure per the original design, not yet built.
- **Effort budget is not re-seeded by config hot-reload.** `seed_effort_budget` only runs at coordinator startup and tier-probe completion; editing `[decoder]` in a running instance's config file has no live effect (restart required). The live `e`-key TUI control is unaffected — it writes the atomic directly. Root cause: `pancetta-config`'s hot-reload file watcher is a documented no-op in this codebase today.

## Audio auto-recovery supervisor (2026-07-11)

Closed `docs/audio-robustness-plan.md`'s remaining design gaps (items 1 and 4 — items 2/3 shipped
earlier in PR #85). The audio thread's `process_audio()` `Err` arm now runs a capped-exponential
backoff (`RecoveryBackoff`, `pancetta/src/coordinator/audio_recovery.rs`: immediate first attempt,
250ms->5s capped thereafter) calling the existing `AudioManager::reopen_devices` primitive with
`force: true` — required because `resolve_reopen_targets` treats an unchanged device selection as a
no-op before `force` is even consulted, so a same-device recovery call with `force: false` would
silently do nothing. A silent-death (wedged-device) watchdog was added to the async relay task's
existing 2-second no-data timeout: instead of only flipping the (already-fixed, PR #85)
`health_audio_alive` reporting flag, it now also sends a synthetic, force-flagged
`AudioReopenRequest` through the SAME `reopen_tx` channel the TUI device picker already uses,
edge-detected via `StaleWatchdog` so a persistently wedged device gets one reopen attempt per stale
episode rather than one every 2s tick. Both new trigger paths ultimately call `reopen_devices` from
inside the audio thread's single-threaded loop, which already serializes every call (operator picker,
Err-arm recovery, watchdog self-trigger) — no new mutex/atomic coordination primitive was needed to
satisfy the design doc's "don't fight the operator's deliberate device switch" risk note. Real RX
drop-rate (`AudioCommShared.dropped_samples`, already incremented in the RT callback — NOT
`AudioManagerStats.underruns`/`.overruns`, confirmed always-zero and dead in this crate) is now
surfaced as a rate-limited `DiagnosticEvent` (at most once per 30s, only when nonzero). Plan:
`docs/superpowers/plans/2026-07-11-audio-auto-recovery.md`. Outstanding: the real unplug/replug
validation is operator-gated hardware time, tracked separately.

## Pre-push hook branch-clobber root cause + fix (2026-07-13)

Six occurrences (2026-07-03 -> 07-12) of a just-pushed branch getting silently hard-reset,
local and remote, to a stale commit within seconds of `git push`. Root-caused via live process
capture during a probe push: `scripts/check.sh` runs as the pre-push hook (`.git/hooks/pre-push`
symlinks to it), and git exports `GIT_DIR` (plus `GIT_WORK_TREE`/`GIT_INDEX_FILE`/`GIT_PREFIX`)
into every hook-spawned process. `GIT_DIR` **overrides** `git -C`-based repo discovery, so
`cargo deny check advisories`'s RustSec advisory-db auto-update — which shells out to
`git -C ~/.cargo/advisory-dbs/<hash> reset --hard && fetch && reset --hard FETCH_HEAD` — ran
those commands against the *pushing repo* instead of the advisory-db checkout, hard-resetting
the branch being pushed (and, mid-push, transferring the moved ref to the remote). Fixed with a
one-line `unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX` at the top of `scripts/check.sh`
(before the `cd "$(git rev-parse --show-toplevel)"` that follows) — protects every cargo child
process the hook spawns, not just cargo-deny. Live-verified: two full hook runs including the
advisory-db update produced zero `reset:` reflog entries. **Belt-and-braces (plan Task 2,
pinning cargo-deny to a non-git-CLI fetch mechanism) was investigated and found not applicable:**
cargo-deny 0.19.4 exposes only `-d/--disable-fetch` (which weakens the gate by skipping advisory
freshness entirely, ruled out) — no flag or `deny.toml` key exists to switch the fetch
*mechanism* while keeping it fresh. Plan: `docs/superpowers/plans/2026-07-13-fix-prepush-git-clobber.md`.

## Onboarding Phase 1 — unbreak the clone→run funnel (2026-07-14)

Six independent fixes closing the gap between "clone the repo" and "a running, decoding TUI,"
per `docs/superpowers/specs/2026-07-12-onboarding-world-class-design.md`'s empirical build test
(README's own quickstart commands didn't run as written; a broken saved config permanently
bricked startup with no recovery path). Plan:
`docs/superpowers/plans/2026-07-12-onboarding-phase1-funnel.md`.

- **README/CLAUDE.md doc-truth pass:** clone command now includes `--recursive` (with a fallback
  note for `git submodule update --init`); both run sections use `cargo run --release -p pancetta`
  instead of ambiguous bare forms; Hamlib marked optional-at-runtime; crate counts corrected
  12→14 across README.md and CLAUDE.md (`pancetta-agent`/`pancetta-protocol`/`pancetta-research`
  were undocumented); CI description corrected to match `.github/workflows/ci.yml` reality
  (macOS-only cross-platform lane, `cargo deny` not `cargo audit`).
- **Loud ft8lib-stub fallback:** the pure-Rust decoder fallback (used when the C `ft8_lib`
  submodule isn't built) was silent at every layer. `pancetta-ft8/build.rs`'s stub branch now
  emits two `cargo:warning=` lines at build time; `pancetta/src/coordinator/pipeline.rs` elevates
  its startup log to `warn!` and sends a retained `DiagnosticEvent` (`target: "decode.engine"`,
  `DiagnosticLevel::Warn`, following the `tx.rs:358-380` `emit_diagnostic` construction pattern)
  so the degraded-recall state is visible in the TUI's Shift+D overlay, not just a log line
  nobody reads. Fatal-ness was explicitly ruled out — CI worktrees and research flows build
  without the submodule by design; this is loud, not blocking.
- **Wizard validates + normalizes at the prompt:** `pancetta/src/main.rs` gained
  `normalize_grid` (Maidenhead field/square uppercase, subsquare lowercase),
  `try_set_station_field` (clone-mutate-validate-return, never mutates in place before
  validation passes), and a `StationField` enum. `setup_station`'s callsign/grid prompts now
  loop until valid input or an empty Enter (keep existing value) instead of accepting anything;
  `run_first_time_setup` gained a belt-and-braces validate-before-save check so an invalid
  config can no longer reach disk. Field-level validators in `pancetta-config` (`validate_callsign`
  / `validate_grid_square`) were deliberately left untouched — normalization satisfies them, it
  never loosens them. **Known pre-existing gap, not fixed by this task:** `run_first_time_setup`
  can still return `Ok(Some(new_config))` when its own final validation fails (prints a warning,
  doesn't save, but the invalid config becomes the live in-memory session) — a softer version of
  the silent-invalid-config trap; flagged during review, worth a follow-up.
- **Startup de-brick:** previously, a saved `~/.pancetta/pancetta.toml` that failed to load
  (e.g. hand-edited, or written before the wizard hardening above) permanently exited the
  process — the only recovery was manual file editing. `load_configuration_with_warnings` now
  offers to re-run the setup wizard on load failure, gated by a new pure function
  `offer_wizard_on_load_failure(headless, wav, interactive) -> bool` that reproduces the exact
  same TTY-only gate as the first-run wizard (`!headless && !wav && interactive`, checked via
  `std::io::stdin().is_terminal()`). The gate function only calls the non-blocking `is_terminal()`
  check — no code path can reach a stdin-reading prompt when the gate is false, so headless/
  `--wav`/piped-stdin runs still error out cleanly with zero risk of hanging on stdin that will
  never arrive.
- **Bare `cargo run` disambiguation:** removed `pancetta-audio` from the workspace
  `default-members` list (kept in `members` — still fully buildable/testable via `--workspace`
  or `-p pancetta-audio`). `pancetta-audio` ships its own `[[bin]]` helper that collided with
  the main `pancetta` binary for bare `cargo run`/`cargo build` at the workspace root; CI is
  unaffected since every CI lane already passes explicit `--workspace` flags.
- **CONTRIBUTING.md truth pass:** replaced fabricated template boilerplate — wrong GitHub org
  (`pancetta-project` → `HagaleTechnologies`), a nonexistent Discord link (repo has
  `has_discussions=false`, so Issues-only), placeholder `[@username]` maintainer list (→
  "Maintained by Hagale Technologies (K5ARH)"), and references to nonexistent
  `CONTRIBUTORS.md`/`./scripts/pre-submit.sh` (→ the real gate, `scripts/check.sh`). Merge
  policy corrected from a fabricated "two approvals required" to the real practice ("PRs are
  reviewed and merged by the maintainer; CI must be green"). License line corrected to the
  project's actual MIT OR Apache-2.0 dual license.

## Onboarding Phase 2 — docs tell the truth (2026-07-14)

Eight tasks closing the gap between "what the docs claim" and "what the code does," per
`docs/superpowers/specs/2026-07-12-onboarding-world-class-design.md` §Phase 2. Plan:
`docs/superpowers/plans/2026-07-13-onboarding-phase2-docs-truth.md`.

- **CONFIG.md `[autonomous]` regen:** the documented schema (`[autonomous_operator]`, a `mode`
  key, `slot_parity_preference`, a top-level `[priority_weights]` with `snr`) never existed in
  code — none of those keys, section name, or nesting matched `AutonomousConfig`
  (`pancetta-config/src/autonomous.rs`). Regenerated from the real struct: `[autonomous]` with
  9 top-level keys, `[autonomous.priorities]` with `signal_strength` (not `snr`). Every value in
  the new doc table was independently cross-checked against `AutonomousConfig::default()` and its
  `#[validate(...)]` ranges during review.
- **RUNBOOK.md truth pass:** fixed the config filename (`pancetta.toml`, not `config.toml`), a
  false "autonomous is config-only, no runtime toggle" claim (the `a`/`Shift+P` keys are live —
  toggle and pause/resume, respectively, confirmed against `tui_runner.rs`), and log paths
  (`~/.pancetta/logs/` plural, `pancetta.log.YYYY-MM-DD` daily-rotated, 14-file retention —
  traced into the vendored `tracing-appender` source to confirm UTC timestamp semantics).
- **FEATURES.md corrections:** removed fabricated hunt/CQ/hybrid autonomous "modes" and a
  nonexistent runtime mode/aggressiveness knob (the operator makes cycle-by-cycle scored
  decisions, no mode concept exists); fixed device-picker key (`d`, not `D` — `D`/Shift+D is the
  diagnostics overlay).
- **`docs/README.md` index:** new file separating curated (maintained, trustworthy) docs from
  working notes (point-in-time design/analysis artifacts, unmaintained) — nothing moved, only
  labeled.
- **`[duplicate_checking]` wired into config (zero behavior change):** the section was documented
  in CONFIG.md but never actually read — the coordinator always built `QsoManagerConfig` with
  `..Default::default()`. New `pancetta-config/src/duplicate_check.rs` module, threaded into
  `pancetta/src/coordinator/qso.rs` following the existing Hound-regions pattern. Config-side
  defaults (`enabled=true, time_window_hours=24, check_frequency=true`) guaranteed identical to
  the pre-existing hard-coded `pancetta_qso::DuplicateCheckConfig::default()` via a cross-crate
  parity test — so a config file without the section behaves byte-identically to the pre-wiring
  binary. `check_band` (defined in `pancetta-qso`, never actually read anywhere) was deliberately
  left out of the exposed schema rather than shipping a dead knob; the coordinator still supplies
  its qso-side default. **Follow-up found, not fixed (out of scope):** `pancetta-qso/src/
  async_database.rs`'s persistent-DB duplicate check hard-codes a 100 Hz frequency filter and
  ignores `check_frequency` entirely, unlike the in-memory check's 50 Hz `check_frequency`-gated
  logic — a real latent bug (an operator setting `check_frequency = false` still gets
  frequency-gated suppression once a QSO ages out of memory into the DB-only path). Docs were
  worded to disclose this discrepancy rather than claim unified behavior.
- **Warn on unknown top-level config sections:** serde silently drops unrecognized TOML keys, so
  a misspelled or obsolete section (the exact `[autonomous_operator]`/`[priority_weights]` ghosts
  this same plan killed from the docs) was invisibly ignored. `pancetta-config/src/loader.rs` now
  sweeps a parsed file's top-level table keys against `Config`'s own schema (derived dynamically
  via `serde_json::to_value(Config::default())`, so the known-set can never drift when a section
  is added) and records a load-warning per stranger, surfaced via the existing `load_warnings`
  mechanism. TOML only this phase (`parse_json` untouched). **Known gap, disclosed not fixed:**
  config hot-reload shares the same `parse_toml` sweep (so it fires and logs), but
  `Config::load_from_file`'s throwaway `ConfigLoader` instance discards its `load_warnings()`, so
  hot-reload's unknown-section warnings reach logs but never the TUI-facing warnings list the way
  startup's `load_default_with_warnings` does — worth a tracked follow-up.
- **`defaults.toml` regenerated from `Config::default()`, drift-tested:** the file is pure
  documentation — the runtime never reads it (`load_embedded_defaults()` returns
  `Config::default()` directly) — and was lying: missing `[autonomous]`/`[decoder]`/`[hound]`/
  `[fox]`/`[tx_placement]` entirely while carrying several TOML sections the plan's own draft
  believed were dead. **Correction during implementation: `[network.web_api]`, `[network.wspr]`,
  `[ui.keyboard]`, `[ui.accessibility]`, `[ui.animations]` are NOT dead** — all five are real,
  live struct fields reachable from `Config`, confirmed by grep and kept in the regenerated
  output rather than deleted per the plan's now-corrected premise. New
  `pancetta-config/tests/defaults_drift.rs` regenerates and drift-tests the file. **Determinism
  fix:** direct `toml::to_string_pretty(&Config::default())` was empirically flaky (a
  `HashMap<String, KeyboardShortcut>` at `ui.keyboard.shortcuts` serializes in randomized
  per-process order) — fixed by routing serialization through `toml::Value` (BTreeMap-backed in
  the pinned `toml` crate version, confirmed against vendored source), test-file-only, no
  `Config` type change. CONFIG.md's billing updated to point at the generated file as the
  complete schema; a pre-existing, unrelated `[ui]` example-block bug (documented `time_format`/
  `target_fps` keys that exist in no Rust struct) was found adjacent to this task's edits and
  cleaned up in the same pass (only `theme` has confirmed runtime effect).
- **Watch-item closed:** Task 1's regenerated CONFIG.md text asserts "startup now warns about
  unknown top-level sections" — true only once the unknown-section-warning task above landed
  later in this same plan; confirmed true by the time of this final gate.

## Onboarding Phase 4 — the five-minute on-air path (2026-07-14)

Eight tasks implementing the operator's clarified goal — **5 minutes measured from build-complete
to on-air**, not from download to decode — per
`docs/superpowers/specs/2026-07-12-onboarding-world-class-design.md` §Phase 4. Plan:
`docs/superpowers/plans/2026-07-13-onboarding-phase4-five-minute-on-air.md`.

- **Wizard rig step:** `run_first_time_setup` (`pancetta/src/main.rs`) gained a skippable
  rig/CAT step reusing the existing `setup_rig`/`setup_ptt`/`setup_frequency` free functions
  already used by `pancetta setup` — no new prompt logic. Prompt defaults to **No** (pressing
  Enter through the whole wizard still produces a decode-only, never-transmits config); the
  closing summary states the rig outcome (model/port/baud/PTT if configured, or a
  `pancetta setup` hint if skipped).
- **`pancetta doctor`:** new subcommand, seven independent checks as `Vec<DoctorCheck>` data
  (`pancetta/src/doctor.rs`) — config, system clock (hand-rolled SNTP client over UDP per
  RFC 4330, no new crate), audio input device, a 2-second audio-level RMS/flat-line capture, the
  `ft8_lib` decoder, rigctld/rig-model reachability, and the git submodule state — each with a
  one-line fix. Must work with NO config file (every config-dependent check degrades to
  `WARN … not configured`, never panics/prompts) and never treats the stub decoder as fatal
  (`hard: false`). `hamlib_model_id` (`coordinator/hamlib.rs`) made `pub` so the bin crate can
  pre-validate a configured model against the same table the rigctld spawner uses.
  (See `docs/DECISIONS/tui.md`'s 2026-07-14 entry for the two TUI-surfacing tasks in this same
  phase: RX input-device fallback badge, rigctld failure surfacing.)
- **`docs/GUIDE.md`:** new task-oriented stranger guide (first 5 minutes, first QSO, how-do-I
  recipes). Every keybinding/config-key/compliance claim independently verified against real
  code during authoring; two real drifts caught and fixed in the guide (not the code): `Shift+M`
  actually cycles FT8→FT4→FT2 (gated behind the off-by-default `ft2` feature), and LoTW upload
  is actually wired (shells out to `tqsl`, config keys `enabled`/`tqsl_path`/`station_location`)
  rather than deferred as earlier drafted.
- **README CLI surface documented + GUIDE.md linked.** Deliberately omits `benchmark` (bare) and
  `test-audio --device`/`--duration` — both still print "not yet implemented" and exit 1;
  documenting them would recreate exactly the kind of dead end this initiative exists to close.
- **`scripts/five-minute-drill.sh`:** the phase's actual exit criterion — a stopwatch-timed
  acceptance script (wizard → doctor-green → first decode, ≤300s) for the operator to run at the
  rig. Syntax-verified; the real timed run needs the FTdx10 attached (operator-gated,
  meatspace-pending).

**Two real, pre-existing bugs found during Phase 4's verification passes and filed as tracked
issues (correctly left unfixed — out of scope for a docs/observability phase):**
- **#141** — `pancetta config --validate`'s implicit `-v` short flag collides with the global
  `--verbose` flag (both `#[arg(short, long, ...)]`). Panics under `cfg(debug_assertions)`
  (`cargo run`/debug builds) via clap's own schema assert; silent in `--release` since the
  workspace's `[profile.release]` explicitly sets `debug-assertions = false` — confirmed by
  reading the actual clap_builder source and the workspace Cargo.toml, not just inference. Never
  trips on the only build path README's Quick Start documents.
- **#140** — `docs/CONFIG.md`'s `[network.lotw]` example still shows a stale
  `enabled/username/password` schema; the real `LotwUploadConfig` struct is
  `enabled`/`tqsl_path`/`station_location`. Found while cross-verifying GUIDE.md's LoTW recipe.

**Operational note:** mid-Task-7, `/System/Volumes/Data` hit 100% capacity (120Mi free),
discovered while attempting a build. Reclaimed ~38GB via `cargo sweep --maxsize 10GB` across the
main checkout and three idle worktrees (health-panel, onboarding-phase1, onboarding-phase2) per
this repo's disk-hygiene practice — none were mid-build, so this was safe. Back to 22GB free/90%
capacity afterward.

## 2026-07-13 — v0.9.5 release infrastructure (Onboarding Phase 3)

- **Hand-rolled release workflow over cargo-dist.** cargo-dist is alive and
  maintained (v0.32, 2026) and was genuinely considered, but rejected on two
  hard requirements: (1) Windows binaries must build with the MinGW/GNU
  toolchain — MSVC cannot compile ft8_lib's VLAs (ci.yml cross-platform note,
  2026-05-23) and windows-gnu is off cargo-dist's happy path; (2) the release
  gate must run the built binary and fail on `ft8lib_stub` (custom smoke step).
  A ~150-line matrix workflow in the existing ci.yml house style keeps both
  under direct control. Revisit cargo-dist if installers/updaters are wanted.
- Releases are draft-first and tags are operator-gated: the tag push builds
  and uploads; only the operator publishes.
- LICENSE-APACHE was discovered to be a *paraphrase* of the Apache-2.0 text
  (§6 truncated, §9 retitled, appendix garbled); restored verbatim. This also
  fixes GitHub's NOASSERTION license detection.
- GitHub community-profile API `documentation` field hardcodes
  `tree/master/docs` (GitHub-side link generation; no repository setting
  controls it). Not fixable repo-side without creating a decoy `master`
  branch — rejected. Mitigation: repo `homepage` now points at
  `tree/main/docs`.
- Repo-wide K5ARH→N0CALL fixture sweep deferred: ~800 occurrences across ~85
  Rust files, several of which encode callsign *semantics* (near-miss
  K5ARG/K5ARH tests, compound-call bases, CTY prefix expectations) where a
  blind sed changes test meaning. The remote-TX security crate
  (pancetta-agent) was swept now; the rest is its own pass.
- **`pancetta-agent`'s sweep has one PERMANENT, intentional exception:**
  `pancetta-agent/tests/fixtures/tx-arm-grant.vectors.v1.json` still contains
  `K5ARH` in its `operatorCallsign` field. This is a frozen, Ed25519-signed
  cross-repo interop vector (cqdx/pancetta/panino concurred, per the file's
  own `$comment`) — `canonicalBytesHex`/`clientSig` are computed once over
  the exact literal string and re-verified byte-for-byte at test time
  (`tx_arm_grant_vectors.rs`); the same file's own
  `mutated_grant_fails_verification` test proves a blind sed would desync
  the fixture and break verification. Changing it needs cross-repo
  coordination via `dispensa`'s `questions/`/`contracts/` process (per this
  repo's own cross-repo-contracts convention), not a unilateral pancetta
  edit. If a future `grep K5ARH pancetta-agent/` shows exactly this one hit,
  that's expected — not a regression.

## Rubato 3.0 -> 4.0 migration (2026-07-14)

`pancetta-dsp/src/resampler.rs`: migrated off rubato 3.0.0 (dependabot PR #126, deferred because
the raw version bump alone didn't compile). The `Resampler` trait itself is unchanged in 4.0 —
`Adjustable`/`Resizable` are new additive sub-traits pancetta never touches, not a split of the
trait pancetta uses. Two real breaking changes, both confined to this one file:
`Resampler::process()` collapsed `(input_offset: usize, active_channels_mask: Option<&[bool]>)`
into a single `Option<&Indexing>`; `SincInterpolationParameters.f_cutoff` changed from `f32` to
`Option<f32>` (`None` now means "auto-select cutoff", a new 4.0 feature — `Some(0.95)` preserves
the original explicit value). Verified behavior-preserving via a new bit-exact characterization
test (`test_resampler_golden_vector_48k_to_12k`) whose baseline was captured against the live
rubato 3.0.0 code before migrating, then re-asserted unchanged against 4.0.0 — confirmed the
migration is a pure call-site fix with zero numerical impact. Note for future migrations of this
crate: neither `pancetta-research`'s eval harness nor `pancetta`'s `loopback_qso` test exercises
`pancetta-dsp`'s resampler at all (`pancetta-research` doesn't depend on `pancetta-dsp`;
`loopback_qso` runs entirely at a fixed 12000 Hz), so a dedicated resampler-level regression test
is the only thing that actually catches a resampling regression in this codebase today.

## General auto-merge for trusted-author PRs (2026-07-18)

Operator asked why a green-CI PR (#155) didn't merge itself — the repo only had auto-merge wired
up for `dependabot[bot]` (`.github/workflows/dependabot-auto-merge.yml`), and `main` had **no
branch protection at all**: no required status checks, no required reviews. That gap matters more
than it looks — `gh pr merge --auto` only waits for whatever branch protection formally marks
"required"; with nothing required, auto-merge can complete before CI even finishes. (The
dependabot workflow's own `classify_breaking_updates` comment blames exactly this for a past
incident, though a dedicated docs entry for it was never actually written here — grep found
nothing under "PR #125" before this entry.)

Two changes, so "auto-merge when checks pass" is actually true, not just intended:
1. **Branch protection added to `main`**: `required_status_checks.contexts = ["CI"]` (the existing
   aggregate job in `ci.yml`, purpose-built as "a single check branch-protection only needs to
   require one name of" per its own comment), `strict: false` (a PR doesn't need to be re-based on
   the latest `main` to merge — this is a low-traffic solo-maintainer repo, not worth the extra
   friction), `enforce_admins: false` (the repo owner can still bypass in an emergency without
   first disabling protection), no required reviews (this repo has no third-party reviewers on the
   normal path; gating happens via the trusted-author check below instead).
2. **New workflow `.github/workflows/auto-merge.yml`**: on `pull_request_target`
   (opened/reopened/synchronize/ready_for_review), calls `gh pr merge --auto --squash` — but ONLY
   when `github.event.pull_request.author_association` is `OWNER`, `MEMBER`, or `COLLABORATOR`
   (mirrors the dependabot workflow's own `github.actor == 'dependabot[bot]'` author-gate pattern).
   This repo is **public**: without the gate, any external contributor's PR would merge itself the
   instant CI turned green, with zero human in the loop. A first-time/outside contributor's PR
   still needs a manual merge even after CI passes. Squash merge matches the repo's actual practice
   (checked via `git show --no-patch --format=%P` on a recent merge commit — single parent, not a
   2-parent merge commit) and the repo's `allow_auto_merge`/`allow_squash_merge` settings were
   already `true` before this change, so no repo-settings change was needed beyond branch
   protection.

**Superseded (2026-09-01):** `auto-merge.yml` is deleted and `dependabot-auto-merge.yml` no longer
calls `gh pr merge --auto` — both native auto-merge paths are retired in favor of Mergify's merge
queue (`.mergify.yml`, new this same change). The trusted-author/breaking-update gating logic this
entry describes is preserved (dependabot's classify/auto-approve/label-breaking steps are
unchanged; the general trusted-author case is now covered by Mergify's own conditions —
`base = main`, `-draft`, `-closed`, thread-resolution, and all three required checks — plus a new
`or: [author = thagale, author = dependabot[bot]]` condition in `.mergify.yml`, added as a
2026-09-01 follow-up fix after this addendum's initial version shipped with no author condition at
all (a live gap: with this repo's ruleset at `required_approving_review_count: 0`, any outside
contributor's green-CI PR would have auto-queued with zero human review). `author = thagale` is
Mergify's closest available primitive to the deleted workflow's `author_association in (OWNER,
MEMBER, COLLABORATOR)` check — Mergify has no direct equivalent of that GitHub-native field (no
built-in role/collaborator lookup), only an exact-login match — and `bypass_actors` is not a
substitute for it: `bypass_actors` governs who may bypass the ruleset entirely, not whether a normal
(non-bypass) PR gets auto-queued by Mergify, so it does nothing to gate this path. Today the human
side of that condition matches the deleted check's practical effect (`gh api
repos/HagaleTechnologies/pancetta/collaborators` lists exactly one collaborator, thagale, admin), but
the mechanism will need revisiting the moment a second trusted collaborator is added — their PRs
won't auto-queue under a single-login condition. `author = dependabot[bot]` is included alongside it
deliberately, not a loophole: a bare `author = thagale` condition (this fix's own first-pass version)
would have silently revoked Dependabot's pre-existing, still-desired auto-queue path for non-breaking
bumps, which `dependabot-auto-merge.yml`'s own classify step already vets (breaking/major bumps are
labeled `needs-review` and excluded regardless of author via the existing `-label = needs-review`
condition) — caught in review before landing. Branch protection (`required_status_checks`) is
unchanged by this addendum.
