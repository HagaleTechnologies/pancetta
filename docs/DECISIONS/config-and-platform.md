<!-- Covers: config merge, hardware tier, band-switch/dial-change flush, decoder harness/speed. Content moved from CLAUDE.md 2026-07-07.
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

`pancetta/src/coordinator/tier.rs`, hb-216 S2: on coordinator startup, the host is classified into Fast / Moderate / Slow via a background `probe_hardware_tier(10)` call (or a cache hit from `~/.pancetta/tier_cache.json` keyed on `(cpu_model, core_count, pancetta_version)`). Moderate/Slow tiers flip the `scoped_fast_path: Arc<AtomicBool>` (replaces the old env-var read in the FT8 hot loop) — this mechanism is unchanged. Operator override: `PANCETTA_SCOPED_FAST_PATH=1` forces on, `=0` forces off, both skip the tier-driven `scoped_fast_path` decision. **Tier-driven `Ft8Config` rewrites retired (decoder-speed-overhaul Task 14):** the old ad-hoc per-tier `Ft8Config` field rewrites (Fast preset `mp=2, ldpc=200` retired Batch 83; Slow preset `max_decode_passes=1` + `max_sync_candidates=150` from Batch 78/72) are gone — `apply_tier` no longer touches `Ft8Config` at all. Tuning decode thoroughness per tier is now owned by the **`[decoder]` effort-preset system** (`pancetta/src/coordinator/effort.rs`): `preset_budget_ms(effort, tier)` maps a `DecodeEffort` (`Auto`/`Eco`/`Standard`/`Deep`/`Max`, config `[decoder].effort`) — plus, for `Auto`, the probed `HardwareTier` — to a per-window wall-time budget in ms, published with the decoder config derived from it as one `DecodeWindowConfig` behind one lock (Task 12; PAN-164) that the FT8 decode loop reads once per window. An explicit `[decoder].budget_ms` always overrides the preset. Seeded at coordinator startup (assuming Fast until the tier is known) and re-seeded once the tier resolves (cache hit or background-probe completion, mirroring how `scoped_fast_path` itself gets re-applied). Spec: `docs/superpowers/specs/2026-06-04-hb-216-s2-tier-wiring-design.md` (tier probe) and `docs/superpowers/specs/2026-07-06-decoder-speed-overhaul-design.md` §6.1 (effort presets).

## Dial-change in-flight-window flush (PAN-70, 2026-10-24)

`pancetta/src/coordinator/dsp.rs`: when the dial moves, the shared `operating_frequency_hz` atomic flips but the DSP `ft8_buffer` still holds up to a full window of audio captured on the old dial. Each emitted `DecodeWindow` carries one dial snapshot (`cur_dial_hz`, PAN-67), so any buffered old-dial audio is reported at the new dial: phantom stations after a band change, and frequencies off by the retune delta after an intra-band retune. The original band-switch flush covered only the first case. PAN-70 covers both.

**Policy.** Once per audio batch, after the batch is appended and before the waterfall row or decode window, the DSP reads the dial once and calls `apply_dial_change`. `dial_flush_decision` compares that read with `last_known_dial_hz`, the last nonzero reading:

- A 0 read (no rig, not yet polled, dropped read) keeps the reference and the audio. A→0→A does not flush. A→0→B flushes when B arrives.
- The first nonzero read seeds the reference without flushing.
- Any other inequality flushes, including a 1 Hz move within a band and moves between dials outside the band table. The comparison is exact, with no deadband.
- A flush clears the whole buffer, including the batch that exposed the change, since that batch's acquisition dial is unknown. It also resets the waterfall sample cursor and advances the reference.
- The waterfall noise-floor history (`bin_history`) is cleared only when `is_band_change(old, new)` is true, so an intra-band nudge keeps the floor warm. `is_band_change` itself is unchanged, and so are its band-scoped callers.

**Scope of the guarantee.** It covers transitions the DSP observes between two known readings. Audio batches carry no dial tag, and CAT updates a separate atomic. So audio queued upstream before a retune, CAT readback latency, and an A→B→A move completed between two reads are not detected. Closing those gaps needs capture provenance (timestamps or dial tags on audio batches), which is out of scope. Windows already handed to the decoder keep their snapshot. Already-decoded consumers (PAN-69 stale-band rejection, PAN-146 parity checks) still compare at band granularity. Exact audio invalidation does not make exact-frequency rejection right for them.

**Accepted cost.** Retuning discards audio that might have decoded, and a dial that keeps moving can prevent a full window from accumulating. The policy prefers a truthful dial over keeping audio it would mislabel. The post-flush schedule is unchanged: `next_window_time`, `boundary_anchored_slice` and DT correction are not touched. A deadline that passes while the buffer refills fires as soon as the buffer reaches the window length, as it already did after a band change or at startup. That early window can be shorter than the anchored layout, and its DT is not corrected for it. If flushes continue more than about 2.4 s past a missed deadline, the deadline falls over one slot behind before the buffer refills. The wall-clock-jump guard (PAN-114) then re-anchors it and logs a forward-jump WARN on `pancetta::slot_clock` even though the clock did not jump. It clears only the audio gathered since the last flush. No recovery time is guaranteed. The old claim that a full window rebuilds about 12.64 s later does not hold in general.

**Rejected and deferred.** A 100 Hz tolerance (proposed in the PAN-70 research) was rejected. It keeps mixing for small retunes, and because its reference stays frozen between flushes, two reads 99 Hz either side of it differ by 198 Hz without a flush. No measured CAT readback jitter or operator retune frequency supports any threshold. Hamlib reports integer Hz. If unstable readback ever causes repeated flushes, measure it before adding a deadband. Splitting the buffer into per-dial segments would keep more decodes during frequent tuning, but needs new timing, overlap and emission bookkeeping. Reconsider it only for a stated decode-continuity requirement backed by operating data.

Tests: `dial_flush_tests` (decision table), `dial_change_state_tests` (buffer, cursor, history and reference effects, sequences such as A→B→A and A→0→B) and `dial_change_signal_tests` (real FT8 audio: a complete message on the old dial is never decoded under the new dial; a clean new-dial message decodes after the flush).

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

## Config file watch: live remote-access revocations, everything else restart-required (2026-10-14)

PAN-103. `docs/CONFIG.md` promised that most keys hot-reload within a second, but nothing in the
binary constructed the watcher, so every edit — including revoking a remote client — needed a
restart. The coordinator now starts `pancetta_config::ConfigHotReload` on its `config_path` when
that file's directory exists, re-reads the file once a burst of watcher events settles, and hands the result to
`run_main_loop` → `handle_config_reload` (`pancetta/src/coordinator/config_reload.rs`). Each edit is
measured against the file's previous content, not the running config, because the running config
can also hold values from other config files and `PANCETTA_*` environment variables.

- **Live, never deferred (revocation direction):** the station-agent and WSJT-X UDP consent bits
  (audited `LocalConsentChanged`), `tx_allow_list` removals (sets pruned under the arm lock, armed
  client disarmed `authorization-revoked`, `authz_generation` bumped so the session loop's
  `reconcile_authorizations` evicts the peer and deny-lists its hello `jti` — the PAN-96 path; the
  config edit is now the only static-config bumper), and `station_agent.enabled = false` (session
  and poll tasks removed from `named_task_handles` before abort so the supervisor never counts a
  crash, `fail_safe_disarm_remote_tx`, restart into the inert drain). The TX worker's key-time and
  ≤50 ms playback re-checks do the pending-work invalidation and playback interruption; the TX path
  is unchanged.
- **Consent off gates, it does not disarm.** Re-enabling consent re-allows an arm that is still
  inside its TTL and heartbeat window. Removing the client from `tx_allow_list` is the kill path.
- **Restart-required, said in a diagnostic naming the keys:** allow-list additions,
  `enabled = true`, the agent's transport/identity fields, and every other key. Only the applied
  remote-access fields are written into the shared `config` snapshot. Writing the whole reloaded
  config there was rejected: running code reads that snapshot (the shutdown PTT-off safety net reads
  `rig.interface.enabled`), so a restart-required key would be half-applied. Every edit still goes
  through `classify_config_reload`; C19-latched fields are never written, so they cannot clobber a
  QSO.
- **cqdx-integrated stations:** the authorization poll stays authoritative; a TOML removal is
  evicted at once but a later poll re-admits an edge cqdx still returns.
- The PAN-59 decision above (live rig switch via a TUI command rather than the watcher) stands; the
  watcher now exists but does not reconnect the rig.
- **Deletion is a change; the directory is always watched.** `should_reload` used to drop every
  `Remove` event, so on a backend that reports a deletion only as a removal (Windows), deleting
  `~/.pancetta/pancetta.toml` left the consent and allow-list it granted live. A removal now
  triggers the merged reload, which falls back to the remaining sources; with `--config` the loader
  fails and the last good config stays (falling back to defaults there would silently replace the
  operator's whole configuration). When the file is missing after the settle, the forwarder settles
  once more before loading, so an editor's remove-then-create save is not read as a deletion (a
  spurious revocation would hold until restart, since grants are restart-required). The watcher
  starts whenever the path's directory exists rather than only when the file existed: sampling
  existence after the startup load raced a deployment replacing the file and could leave the
  station unwatched for its lifetime. Plumbing "did the loader read this path" out of the loader
  was rejected as more code for the same result.

## Decoder speed overhaul — budget-governed anytime decoder

PAN-7 added the default-off `Ft8Config::ft8lib_sync_seeds_enabled` research path, which runs the vendored MIT ft8_lib candidate finder on pass-0 audio, translates and re-scores its positions on pancetta's Costas lattice, and unions them before truncation. The design is in `docs/superpowers/specs/2026-08-03-ft8lib-sync-seed-union-design.md`; the sixteen cap-200/cap-400 scorecards are under `research/scorecards/pan7/`; and the verdict is recorded in `research/experiments/2026-08-03-ft8lib-sync-seed-union-declined.md`. Both caps produced exact seeded/control nulls: hard-jt9-rich and curated-hard ΔTP=0 with 95% CI `[0,0]`, Δunverified=0, synth recovery unchanged, and noise stayed at the same 1/1000 baseline (incremental ΔFP=0). Seed-survival counters confirmed that representable ft8_lib positions were already present on the exhaustive native lattice and did not survive as novel candidates. The standing TP gate therefore failed and the flag remains `false`; only a separate negative-dt/slot-edge design warrants reopening.

`pancetta-ft8/src/decoder.rs`, `pancetta/src/coordinator/effort.rs`, `pancetta-config/src/decoder.rs`, `pancetta-tui/src/tui_runner.rs`; spec `docs/superpowers/specs/2026-07-06-decoder-speed-overhaul-design.md`, plan `docs/superpowers/plans/2026-07-06-decoder-speed-overhaul.md`, ledger `.superpowers/sdd/progress.md`, final report `research/experiments/2026-07-06-decoder-speed-overhaul-final.md`: a 16-task subagent-driven-development effort making one decode window an **anytime algorithm** — always produces the same result if allowed to run to completion, but can be stopped early under a wall-clock budget and still return everything decoded so far. **Phase 1 (Tasks 1-7, mechanical/A/B fixes):** lazy BP trajectory + array returns (no per-call 17.4KB alloc on the convergent path), OSD sort-key precompute, flattened contiguous spectrogram storage (kills a triple-`Vec` pointer-chase in the Costas kernel), a piecewise Padé `fast_atanh` (`pade_atanh=true`, Padé for `|x|≤0.95` else `ln` — the *raw* Padé form failed its hard-200 A/B gate and was replaced by the piecewise fallback before shipping), and an f32 real-input FFT + f32 spectrogram (`SpecScalar=f32`, `realfft`). Measured on the profiling harness's one fixture (`cargo run --release -p pancetta-ft8 --example profile_decode -- native 10`, `tests/fixtures/wav/wsjt/210703_133430.wav`): **multi-thread 144.01→107.18 ms/window (-25.6%), single-thread 246.43→189.63 ms/window (-23.1%)** — see `research/experiments/2026-07-06-phase1-checkpoint.md` for the full A/B table. This is *less* than the plan's originally-hoped ~60-100ms/~75% reduction: that bigger estimate assumed the stacked effect under a workload that stresses BP/OSD/FFT harder than this one 8-message fixture does — the corpus-level (hard-200) per-task deltas (Padé -16.0% elapsed, f32 FFT -3.8% elapsed) are consistent with the smaller single-fixture number. One planned flip (Task 6, `costas_half_loop_disabled`) was investigated and **did not ship**: the plan's own text asserted Batch 92 supported flipping it, but Batch 92's actual recorded verdict (`research/experiments/2026-06-12-batch-92.md`) was the opposite (flipping costs real recall, explicit do-not-ship) — Task 6 independently re-confirmed this on hard-200 (Δrec=-58) and correctly declined the flip per the standing "gate fails → no flip" rule. **This corrects the plan document's own factual error for future readers: Batch 92 never supported the flip.** **Phase 2 (Tasks 8-12): `DecodeBudget`/`DecodeBudgetReport` + per-window anytime staging.** Decode work is split into ranked stages, each checkpointed against a `DecodeBudget` (unlimited in tests/CI/research harness by construction — no wall-clock reads on the eval path): **S1-floor** (top-ranked sync candidates, unconditional — same recall as the legacy single-pass decoder), **S2-rest** (remaining candidates, budget-gated), **S3** (BP escalation ladder — a floor-then-continue-to-deep BP retry for candidates that fail at the floor iteration count; ships with `escalation_enabled=false` — its own A/B data showed BP iteration count is a small fraction of total decode cost for this decoder, Costas/FFT dominate, so flipping it wasn't worth it, same data-driven non-flip pattern as Task 6), **S4-cross-cycle**, **S5-multipass**, **S6-joint-pair**, **S7-a7** (the existing recall-boosting stages, now each individually budget-gated rather than always running to completion). The coordinator wires a real wall-clock deadline via a per-window budget (`0` = the protocol ceiling until Phase 3 seeds it), since PAN-164 published together with the decoder config as one `DecodeWindowConfig`. **KNOWN LIMITATION (S3, currently inert since escalation ships disabled):** the BP escalation ladder escalates **inline, in candidate sync-score-rank order** — NOT the plan's originally-envisioned design of collecting all floor-failures, sorting by parity/promise, and escalating the most-likely-to-succeed candidates first. Harmless today (escalation is off and all production paths use `DecodeBudget::unlimited()`), but if ever enabled under real time pressure, a tight budget could be spent on an early low-promise candidate while skipping a later near-certain one — flagged as a TODO, not built to the original spec. **Phase 3 (Tasks 13-15): operator control surface.** `[decoder]` config section (`effort: DecodeEffort` = `Auto`/`Eco`/`Standard`/`Deep`/`Max`, default `Auto`; `budget_ms: Option<u64>` explicit override, always wins over the preset). `preset_budget_ms(effort, tier)` (`pancetta/src/coordinator/effort.rs`) maps preset+tier to a per-window ms budget — `Eco`=1ms (floor-only, deliberately *not* 0/unlimited — a 1ms budget deterministically stops after S1-floor), `Standard`=250ms, `Deep`=1000ms, `Max`=0 (unlimited), `Auto`→ tier-derived (`Slow`=1, `Moderate`=250, `Fast`=1000). This **subsumes and deletes** the old ad-hoc per-tier `Ft8Config` rewrite hack (Fast `mp=2,ldpc=200` / Slow `max_decode_passes=1,max_sync_candidates=150`) — `apply_tier` no longer touches `Ft8Config` at all; tuning decode thoroughness per tier is now entirely the effort-preset system's job. Seeded at coordinator startup (assumes `Fast` before the tier is known) and re-seeded on tier-probe completion (cache hit or background probe). Live operator control via the **`e`** TUI key (`CycleDecodeEffort` → `Eco→Standard→Deep→Max→Auto→Eco`, no optimistic local flip — waits for the coordinator's authoritative `DecodeEffortUpdate` echo before updating the status chip), persisted to `~/.pancetta/tui_state.json`. **KNOWN LIMITATION: config hot-reload does NOT re-seed the effort budget.** Investigated and found genuinely infeasible today, not a corner cut: `pancetta_config::ConfigHotReload`'s file watcher exists but is never constructed anywhere in the coordinator crate — hot-reload is a documented, deliberate no-op (so a reload can never clobber latched QSO state). Only coordinator startup and tier-probe completion seed the budget; a config-file edit to `[decoder]` takes effect only on restart (the `e` key is the only *live* control). **Validation:** full workspace suite green throughout every task; decode counts on every WAV fixture unchanged from the pre-Phase-1 baseline (BIT-EXACT tasks) and A/B-gated on hard-200/hard-1000 (perf tasks) per the standing bootstrap-CI ΔFP≤2×ΔTP rule. **Remaining:** on-air soak (operator-gated, per spec §7.5 — compare `deep` vs `auto` effort sessions' decode counts + telemetry) before declaring the full success criteria met.

### Max-preset multipass headroom (PAN-165, 2026-10-21)

PAN-157's `Max` override (2 passes + time-varying subtraction) was justified by a harness that started a fresh 2000 ms deadline right before the native decode call, while production anchors that deadline at window receipt and spends part of it on waterfall generation, the optional scoped fast-path decode and the ft8_lib call first. The multipass loop in `pancetta-ft8/src/decoder.rs` now starts end-of-pass subtraction, keeps subtracting (checked per message) and starts another pass only while `DecodeBudget::has_time_for(pass_floor_cost)` holds, where `pass_floor_cost` is the costliest start-to-S1-floor prefix this call has already paid; before, `has_time()` let a pass or a whole busy window's subtraction start with a sliver left and run seconds past the deadline. `eval --effort max --budget-anchor receipt [--qso-partner-hz HZ]` reproduces production's pre-native sequence, writes a `window_timing` summary (and `pair_recovery` on synth-pair) into the scorecard, and `DecodeBudgetReport` now reports `passes_run` plus `multipass-subtract` and `multipass-legacy-deadline` stages. Production logs a `window timing:` line per window on the `decode.budget` debug target with the pre-native breakdown, headroom at the native call and overrun. The before/after record is `research/experiments/2026-10-21-pan165-production-anchored-max-timing.md` with `research/scorecards/pan165-*.json`; the Windows MiniPC rerun is operator-gated.

### Eco-preset fine sync (PAN-158, 2026-10-25)

Task W3.3's per-candidate fine time/frequency refinement (`Ft8Config::fine_sync_enabled`) stays default-off in the library, and `apply_effort_overrides` (`pancetta/src/coordinator/effort.rs`) now turns it on exactly when the resolved preset is `Eco` (literal `Eco`, or `Auto` on `Slow` hardware), the protocol is FT8 and the published budget is 1 ms. It is assigned on every publish, so Standard/Deep/Max, Auto on Moderate/Fast, FT4, and any published budget other than 1 ms all clear it. The predicate reads the published budget, not whether it came from an override: a `[decoder] budget_ms` other than 1 keeps Eco off only until a live effort cycle (`cycle_decode_effort`) publishes Eco's own 1 ms, and an explicit `budget_ms = 1` leaves Eco on. The budget condition exists because W3.3b's win came from the floor-only regime: with a budget-gated rest of the pool, one slow candidate starves the others (hard-200, July: Eco +27 TP, 95% CI [+15, +40]; Standard −23). `fine_sync_enabled` is a `DecoderRebuildKey` field, so entering or leaving the regime rebuilds the live decoder at the next window (callsign carry-over unchanged), while a repeated identical publish adopts without a rebuild; busy-lock and failed-rebuild windows keep the whole previous decoder/budget pair, as for every other override. Fresh evidence on today's decoder: `research/experiments/2026-10-25-pan158-eco-fine-sync.md` with `research/scorecards/pan158-eco-*.json`, a receipt-anchored, ft8_lib-merged A/B on the regenerated synth-clean corpus (merged recovery 296 → 321 of 550, +25 with paired 95% CI [+16, +35], 0 → 0 false decodes, elapsed −24.5%) and a regenerated 1000-WAV noise corpus (0 → 0 false positives and decoded files, elapsed −18.8%). Limits: synthetic AWGN single-signal corpora, not the original hard-200 recordings; one x86 host; no MiniPC, Pi or on-air run; and Eco's 1 ms is a scheduling budget, not an end-to-end latency guarantee, because unconditional floor work is not cancelled mid-candidate.

### FT8 impulse-robust LLR knee (PAN-162, PAN-241, 2026-10-27)

hb-256's per-symbol knee (`Ft8Config::impulse_robust_llr`) stays `None` in the library, and `apply_effort_overrides` (`pancetta/src/coordinator/effort.rs`) now sets it to `FT8_IMPULSE_ROBUST_LLR_KNEE` (6.0) whenever the protocol is FT8, at every preset, tier and budget, and to `None` on FT4 and FT2. It is assigned on every publish, so a protocol switch clears or restores it, and it is a `DecoderRebuildKey` field (`impulse_robust_llr_bits`, because `f64` is not `Eq`), so the switch rebuilds the live decoder at the next window. Evidence: PAN-162's pre-registered storm re-measure returned `candidate-k6` (storm plant k=6 +0.465 dB [+0.308, +0.653], AWGN +0.000 dB, off-air ΔTP +1 on the 60 vendored WAVs), and the native-only production gate it triggered passed in PAN-241: synth-clean recovered 285 → 285 of 550 (paired 95% CI [+0.0, +0.0]), elapsed −4.5%; noise_1000 false positives 5 → 5, noise files decoded 5 → 5, elapsed +14.0%. Record: `research/experiments/2026-10-25-pan162-impulse-llr-storm.md` with `research/scorecards/pan162-*.json`. Revert rule (pre-registered, operator-side): remove the coordinator assignment if the station's hard_200/50 ΔTP is below −2, or the storm-day or hard-1000 paired recall CI is significantly negative. Scope: only decoders built from the published `DecodeWindowConfig` (the live FT8 hot loop) get the knee; `--wav` playback (`coordinator/wav_playback.rs`) builds `Ft8Config::default()` and runs without it. Limits: a synthetic Gaussian-burst plant, not recorded lightning; native decoder only, while production merges ft8_lib output; one x86 host; no MiniPC, Pi, on-air or real-storm-day run. The BICM-ID rescue (off in production) does not carry the knee into its per-iteration metrics.

### Open follow-ups (decoder speed overhaul)

- **BP escalation ladder (S3) escalates in candidate rank order, not by promise** — inert today (`escalation_enabled=false`), but if ever flipped on under real time pressure this could waste a tight budget on an early low-promise candidate while skipping a later near-certain one. Fix is a global collect-sort-escalate restructure per the original design, not yet built.
- **Legacy `BudgetTracker` silently caps the pass loop (PAN-165).** `decode_window_with_ap_scoped_partner_impl` still builds `BudgetTracker::new(2000 + 500 × osd_depth)` anchored at call start and checks it before every pass. It is redundant in production, where the receipt-anchored `DecodeBudget` deadline is never later, but it makes `DecodeBudget::unlimited()` harness runs host-dependent (on a 2-core host it cancelled pass 2 in 16 of 18 synth-pair windows). It is now visible as a `multipass-legacy-deadline` stage; candidate for retirement.
- **Effort budget is not re-seeded by config hot-reload.** `seed_effort_budget` only runs at coordinator startup and tier-probe completion; editing `[decoder]` in a running instance's config file has no live effect (restart required). The live `e`-key TUI control is unaffected — it publishes into the `DecodeWindowConfig` directly. Root cause: `pancetta-config`'s hot-reload file watcher is a documented no-op in this codebase today.
- **Budget/config window atomicity (PAN-164, resolved).** Budget and the effort-conditional `Ft8Config` fields publish as one `DecodeWindowConfig` under one lock; the FT8 hot loop reads it once per window and keeps the whole previous state on contention or a failed rebuild; the budget ceiling now comes from the decoder's own protocol, which also closes the FT4→FT8-on-Max mode-switch tear.

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
  (Superseded by PAN-334, 2026-11-09: both are gone.)
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

**Superseded again (2026-09-15):** Mergify's own queue is retired in favor of GitHub's native
`merge_queue` branch-ruleset rule (`Pancetta - Native Merge Queue`, id 23456638, scoped to `main`
only -- the pre-existing "Public - Pancetta" ruleset also covers `release/*`/`releases/*` via
wildcard refs, which GitHub rejects alongside a `merge_queue` rule). `.mergify.yml` is deleted.
`auto-merge-trigger.yml` and `queue-retry-handler.yml` (added in this same cutover, PR #381) now own
admission -- `auto-merge-trigger.yml`'s author allowlist is this addendum's own trusted-author
condition ported onto the native path. **`dependabot-auto-merge.yml` does NOT call `gh pr merge
--auto`** -- it only approves (non-breaking updates) or labels+comments for manual review (breaking
updates); `auto-merge-trigger.yml`'s own allowlist excludes `dependabot[bot]` entirely. A green,
auto-approved Dependabot PR therefore has no automatic merge path at all now -- an operator has to
run `gh pr merge --auto --squash` by hand, same gap already tracked fleet-wide as VAN-710 (vanity's
own Dependabot-admission policy question), not resolved here. The single-collaborator
caveat above (Mergify's `author = thagale` condition needing revisiting once a second trusted
collaborator exists) carries over unchanged to `auto-merge-trigger.yml`'s own allowlist -- same
mechanism, same limitation, different admission path.

### Interrupted native queue recovery (PAN-150, 2026-10-20)

Admission stands down successfully when the native `merge_queue` rule is absent.
Its probe captures the status explicitly, so that behavior does not depend on an
outer Bash conditional suppressing `errexit`. A ruleset read failure still fails
admission, and only a confirmed queue rule permits arming.

A failed merge-group attempt has a different recovery requirement. If the rule
is absent, `queue-retry-handler.yml` fails without arming, consuming the retry
budget, or attaching the handled label. Restore the rule, then rerun the workflow
or redeliver the event. Restoring the rule alone does not schedule recovery.

On second failure, handoff determines pause ownership from the latest label
attachment event after the idempotent label POST. A confirmed human or other
external owner keeps the pause without a handler marker. Unknown ownership
preserves the pause and fails after the disarm checks. For a confirmed
`github-actions[bot]` attachment, handoff preserves any valid bot marker at or
after that attachment, including its original SHA. If readable comment history
contains no such marker, it posts and verifies a new marker before success.
Unreadable provenance or failed publication/readback leaves the event eligible
for rerun. This repairs interrupted handler-owned handoffs without changing the
admission staleness consumer, marker format, or one-retry limit.

Existing orphaned pauses are repaired when an eligible handoff reruns. Historical
attempts already marked handled are not swept automatically. Offline regressions
in `scripts/ci/test-ci-queue-recovery.sh` execute the workflow Bash and jq through
the existing CI test discovery.

PAN-152: the Retry step's fallback, taken when a re-armed head's
`queue-retry-used` label cannot be confirmed, now applies the same ownership
rule. Only once disarm is confirmed and the pause label is visible, and only for
a `github-actions[bot]` attachment, does it post the admission
`queue-needs-human-review sha=` marker and verify that exact comment, so a later
push clears the pause. An external owner keeps the pause with the inert
`no-more-retries` marker. Unknown ownership or an unverified marker fails the
step after the pause is recorded. Pauses already carrying only `no-more-retries`
are not migrated.

Both marker publishers, this fallback and the second-failure handoff, re-read
the latest attachment once the marker comment exists. A maintainer can remove
and re-attach the pause between the ownership read and the comment, and
admission trusts the newest bot marker when it is at or after the latest
attachment. If the attachment changed or is unreadable, the step edits the
marker out (the fallback leaves the inert marker), reads the comment back, and
fails. If that edit cannot be confirmed, or the comment's ID is unknown, the
step only fails and asks for the marker to be removed by hand; a rerun does not
repair it. A fallback pause whose marker was withdrawn stays until removed by
hand. Two races remain open: a re-attachment the timeline does not show yet, and
one within the same second as the comment, because admission compares whole
seconds. Closing them would mean binding the marker to a labeling event in the
admission consumer.

## Partial tables, fatal file-load errors, and an honest `config --validate` (PAN-90, 2026-10-10)

Every documented "just add this block" recipe failed to parse on its own (README's
`[autonomous] enabled = true`, both GUIDE.md GridTracker recipes, CONFIG.md's minimum config):
most config structs had no container-level `#[serde(default)]`, so a table that set one key was
a hard `missing field` error. Worse, the auto-discovery loader swallowed that error, ran on
`Config::default()` (N0CALL), and `pancetta config --validate` printed PASS with exit 0.

- **Partial-table rule.** Every single-instance config struct carries container-level
  `#[serde(default)]` and a `Default` impl. `Config` itself does not (its `Default` mints a random
  `metadata.instance_id`; a file without `[metadata]` must stay `metadata: None`);
  `ConfigMetadata` uses field-level defaults instead. Record types (`[[rig.bookmarks]]`,
  `[[station.antennas]]`, map entries such as `ui.keyboard.shortcuts.<name>`) stay strict.
  Guard: `pancetta-config/tests/partial_tables.rs` parses every table path of
  `Config::default()` as an empty table and requires it to equal the defaults; its two allowlists
  hold at most four entries, each with a reason.
- **Unknown keys at any depth** are load warnings (non-fatal), so a partial table never turns a
  typo into a silent no-op. Detection walks the raw TOML (or the key shape of a raw JSON file)
  against the parsed config serialized to JSON (`ConfigLoader::warn_unknown_keys`, `parse_json`).
- **Fatal file-load errors.** A discovered config *file* that exists but fails to load returns
  `ConfigError::FileLoad { path, .. }`; startup stops (interactive launches get the de-brick
  prompt with the real path). Non-file sources keep warn-and-continue. For a validation failure
  the prompt targets the one discovered file that fails validation on its own, and is not
  offered when no single file can be blamed. The de-brick wizard starts from defaults, so on save
  it always keeps the file it rewrites as `<name>.bak` (or the next free `<name>.bak.N`; created
  exclusively, so a planted symlink or an earlier backup is never followed or overwritten; 0600
  on Unix).
  After the wizard, startup reloads every
  discovered file, so the session starts from what the next start reads (and warns about the
  sections where other files change the wizard's values; the first-run wizard still follows if
  that config has no callsign). If the reload still fails, it warns and runs on the wizard's
  values for this session only.
- **`config --validate` contract.** Loads without any wizard, prints `Config file: <path>` for
  each file read (or the searched directories and "No config file found"), prints warnings, then
  validates. Exit 1 on any load or validation failure. Unknown keys warn but do not fail it.
- **Docs are tested.** `pancetta-config/tests/docs_snippets.rs` loads every fenced ```` ```toml ````
  block and every inline `[section] key = value` span in `README.md` + `docs/*.md` (top level)
  through the real loader; each must load with zero warnings.
- **Search path** deduplicated (keep last occurrence) and documented in `docs/CONFIG.md`.
- `pancetta-config/examples/config.toml` deleted: unreferenced, unparseable, and a duplicate of
  the generated `defaults.toml`.

## In-session clock-skew monitor (PAN-114, 2026-10-10)

`pancetta/src/clock_skew.rs` (SNTP client, thresholds, pure `ClockSkewMonitorCore`),
`pancetta/src/coordinator/clock_monitor.rs` (`spawn_clock_skew_monitor`): before this, the only clock
check was `pancetta doctor`'s one-shot SNTP probe with a 1.0 s threshold, coarser than FT8's ~0.3 s
decode margin (the live capture window spans DT ≈ −0.66…+0.36 s). The SNTP code moved from the
binary-only `doctor.rs` into `pancetta_lib` so the coordinator can use it; doctor imports it back.

- **Cadence.** First probe 30 s after start, then every 30 min. The NTP pool's SNTP client terms
  ([ntppool.org/vendors](https://ntppool.org/vendors)) ask for at most one query per 30 minutes,
  which rules out a fast "re-probe until clear" loop. 30 min of drift at a 50 ppm crystal is
  0.09 s, well below the threshold.
- **Steps.** A `ClockStepDetector` (wall vs monotonic, ≥ 250 ms) on the monitor's 1 s tick detects
  NTP steps and, where the monotonic clock stops in suspend (Linux, macOS), sleep/wake. A step
  clears the warning at once, logs a `clock.step` Warn (at most one per 60 s), and schedules a
  re-check at `max(last_probe + 30 min, step + 30 s)`.
- **Surfacing.** Each `clock.skew` / `clock.step` entry goes to the Diagnostics overlay through
  `emit_diagnostic_full` **and** to `tracing` (`clock skew:` / `clock step:`), because headless mode
  drops the TUI bus receiver. An unreachable server logs one Info entry and shows no chip.
- **Gating.** Started in `ApplicationCoordinator::run` only when `!replay_mode()`, so no SNTP query
  leaves a replay (`--wav` returns before it). Not registered with the supervisor: it is advisory
  and must not trigger restart semantics. Aborted after the main loop exits.
- **No config key.** Always on in live sessions; this avoids a new `merge_with` field. Thresholds
  are shared with doctor: `CLOCK_SKEW_WARN_S` (0.3) and `CLOCK_SKEW_FAIL_S` (1.0). Doctor gained a
  WARN tier for 0.3 s ≤ |offset| < 1.0 s; PASS, FAIL and unreachable texts and the exit code are
  unchanged.

## Research-scan PRs land themselves (PAN-172, 2026-10-11)

- Merge groups skip the five Rust lanes only when their non-empty cumulative diff is
  entirely under `research/notes/`. The path allowlist includes `--no-renames`, so moving
  code into the notes directory cannot hide its deletion. Detection fails closed. Other
  groups run every Rust lane, including Cargo Deny advisories; a note cannot repair an
  advisory already present on main.
- `CI` rejects commit trailer lines on PRs whose title starts with `Weekly research scan `.
  The check covers every commit in the PR and rejects both AI attribution and session lines.
- `research-scan-shepherd.yml` runs at 02:17 and 14:17 UTC. It handles only ready, same-repo,
  notes-only scan PRs into main from the existing trusted authors. It uses the review PAT
  only for thread resolution and arming. It has its own concurrency group and checks that
  Auto-Merge Trigger and Queue Retry Handler are quiet before acting, avoiding eviction
  of a pending run in their shared lock. Stuck scans get a PR comment, a maintainer issue,
  and a failed workflow run.
- The shepherd may clear a handler-attached pause at most twice per head SHA. A pause
  attached by anyone except `github-actions[bot]` remains in force. Exhausting the budget
  produces a report instead of another retry.
- The [routine lifecycle](../routines/pancetta-research-scan.md) is the source for the
  scan-only review policy. Round 1 fixes reasonable findings; rounds 2 and 3 fix P0/P1
  and defer P2 and lower; round 4 and later defer everything without more commits.
  This local override leaves the synced general convergence policy unchanged. The external
  claude.ai routine must receive the documented prompt pointer, daily schedule and Linear
  connector edits before the full lifecycle is operational.

## Network input budgets (PAN-101), 2026-10-14

HTTP response bodies in `pancetta-cqdx`, `pancetta-dx` and the `pancetta pair` client, DX cluster
lines, and relay WebSocket frames are read through a reader that stops at a fixed byte budget, so
none of them is buffered whole before its size is known. This record does not cover the
remote-gateway WebSocket acceptor; its limits are tracked in PAN-99. The budgets are `const`s
beside each client, not config:

- **HTTP bodies.** `pancetta-cqdx` (`client.rs::read_body_capped`) and `pancetta-dx`
  (`body::{bytes_within, text_within, text_prefix}`) read with `Response::chunk()` and stop once the
  budget is passed. A body that is read whole fails when it is over budget, or before reading when
  its declared `Content-Length` is (`CqdxError::ResponseTooLarge`, `DxError::ResponseTooLarge`,
  `PairingError::MalformedResponse` for `pancetta pair`). Two success bodies are read only up to
  their budget and never fail on size: a cqdx `log_qso` 2xx body over 64 KiB is not checked for a
  duplicate marker and counts as logged, and ClubLog's body is read only up to 4 KiB whatever the
  status, with the outcome set by the HTTP status. The cqdx, QRZ Logbook and eQSL error bodies are
  read only up to 4 KiB, so an error message carries at most that much of the body. Budgets: cqdx
  JSON 10 MiB, cqdx `log_qso` 2xx 64 KiB, QRZ XML 64 KiB, QRZ Logbook/eQSL 64 KiB, ClubLog 4 KiB,
  LoTW pages 1 MiB and report 64 MiB, PSKReporter query 16 MiB, `pancetta pair` 64 KiB with a 30 s
  timeout. `pancetta-dx` decodes text the way `Response::text` does (Content-Type charset, BOM
  sniffing).
- **DX cluster.** Telnet lines longer than 4096 bytes (line ending included) are discarded without
  being buffered (`cluster.rs::read_capped_line`). The spot queue holds 1024 and drops new spots when
  full (`DxClusterClient::dropped_spot_count`); the command queue holds 64. The WebSocket branch
  refuses messages over 4096 bytes.
- **Relay WebSocket.** `RealWsConn` sets tungstenite's `max_message_size` and `max_frame_size` to
  relay.v1's `MAX_FRAME_BYTES` (65536), so a larger frame is refused from its header and the session
  closes, the same outcome `parse_frame` already produced.

Rule: do not call `.text()`, `.bytes()` or `.json()` on a `reqwest::Response` in these crates, and
do not dial or accept a WebSocket without a `WebSocketConfig` sized to its protocol. The
remote-gateway acceptor does not follow this rule yet (PAN-99).

## Release workflow pins and token scope (PAN-106, 2026-10-14)

- **Pins.** `.github/workflows/release.yml` pins every action to a full commit SHA, with the
  version in a trailing comment. `dtolnay/rust-toolchain` follows its `stable` branch at a pinned
  SHA (the commit `stable` resolved to when pinned), so a toolchain-action update is an explicit
  pin bump. `supply-chain.yml` still pins an older `dtolnay/rust-toolchain` commit; aligning the
  two is a follow-up.
- **Token.** The workflow-level token is `contents: read`. Only the `release` job, which runs
  `gh release create`, declares `contents: write`; `build` inherits read-only. Both checkouts set
  `persist-credentials: false`; the `ft8_lib` submodule is public and fetches anonymously.
- **Rule.** Add new actions to `release.yml` SHA-pinned, and do not give `build` any write scope.
  The `Supply Chain` pin check only covers newly added `uses:` lines, so it does not catch an
  existing ref being left mutable.
- **Deferred.** Artifact attestation and signing (it needs `id-token: write` and
  `attestations: write` on `build`, so it gets its own decision), and pinning plus permission
  scoping for `demo-assets.yml`, `docs.yml`, `dependabot-auto-merge.yml` and `ci.yml`.

## Semgrep skip visibility and local-ruleset fallback (PAN-107, 2026-10-18)

- `semgrep.yml` is three jobs: `semgrep/mode` reads `SEMGREP_APP_TOKEN` in a
  step and exports `mode=platform|local`; `semgrep/ci` runs only in platform
  mode; `semgrep/local-ruleset` runs a secret-free scan of the vendored
  `.semgrep/secrets.yaml` (`p/secrets`, digest in `.semgrep/README.md`)
  otherwise. A token-absent run therefore reports `semgrep/ci` as natively
  skipped (grey), never green-for-nothing.
- The `secrets` context is not allowed in a job-level `if:` (actionlint errors;
  GitHub fails the workflow) — the decider-job shape is the workaround, same
  producer/consumer idiom as `codex-clean`/`wait-for-codex.yml`.
- Rules: refresh `.semgrep/secrets.yaml` only by PR with the digest updated;
  don't add other packs without re-checking they are clean on `main` (the
  `rust`/`github-actions`/`ci` packs fire 136 accepted-state findings today).
- Do not add an `environment:` block with a branch policy to these jobs: a
  `merge_group` ref (`gh-readonly-queue/main/...`) would miss the policy,
  empty the token, and silently shift merge-queue runs to the local ruleset
  (the `harbor` trap recorded in the pool's backlog-grooming notes).
- Deferred: provisioning `SEMGREP_APP_TOKEN` as a Dependabot secret (CAT-307
  follow-up; after that, Dependabot PRs get platform scans automatically), and
  the same PAN-107 fix in the sibling repos (cqdx/panino/widdershins/coppa).

## WAV recording is opt-in (PAN-125, 2026-10-19)

- `[audio.recording] auto_record` (default `false`) gates both WAV writers:
  the DSP decode-window recorder (`dsp.rs`, `WavRecorder`) and the one-shot
  `raw_48khz_diagnostic.wav` capture in the audio relay (`audio.rs`). One
  function, `resolve_wav_recording`, decides for both. `--replay` and
  `--no-audio` refuse even when enabled (replay joins the `replay_mode()`
  list; closes GH #265).
- `directory` is honored, and pancetta expands a leading `~` itself: the
  config loader never expands `~` inside values (`loader.rs` tests). Empty
  means `~/.pancetta/recordings`.
- The `directory` default stays `~/Documents/Pancetta/Recordings`, and
  `defaults.toml` is unchanged. The station corpus offload (magazzino
  `clients/pancetta-wav-offload.sh`) reads `~/.pancetta/recordings`, so a
  corpus station sets `directory` explicitly. sophon did so on 2026-10-16,
  before this shipped.
- The cap stays a hardcoded 10 GB. The other `[audio.recording]` keys stay
  unread. Restart-required like every non-live key — including across DSP
  supervisor restarts, because the running config snapshot is patched only
  for security fields.

## `--replay` feed waits for the QSO history seed (PAN-42, 2026-10-21)

- `run()` starts the replay feeder (`start_pipeline()`) before the QSO
  component, and both `start_*` calls return once their task is spawned.
  Under `--replay` the QSO task rebuilds a `:memory:` index from the whole
  `qsos.adi` and seeds worked/DX-Hunter/display history before its receiver
  loop, so on a large logbook decodes queued unread (or were aborted by
  replay's self-shutdown). Deferred from PR #293 round 6.
- Fix: `ApplicationCoordinator::qso_consumer_ready` (`watch<bool>`). The QSO
  task's `replay::QsoConsumerReadyGuard` stores `true` immediately before
  the receiver loop, or on drop (startup error, panic, abort), so a dead
  QSO task never hangs a replay. The feeder's `wait_for_qso_consumer` waits
  for it, polling shutdown every 100 ms, then does its slot-boundary
  pre-roll.
- Rejected: reordering `run()` or awaiting the seed in
  `start_qso_component()` (that is also the supervisor's restart path), and
  seeding off the receiver loop (early decodes would be scored without
  history, so results would still depend on logbook size). No timeout cap,
  for the same reason. The flag is never reset, so a Qso restart does not
  re-gate a running feed.
- Live mode is unchanged: live audio cannot be held back. A slow live index
  rebuild can likewise delay the first live decodes; that is a separate gap.

## Exact Rust toolchain pin (PAN-56, 2026-10-21)

- **Pin.** `rust-toolchain.toml` sets an exact `channel` (`1.98.0`, shipped in PR #447), never
  `stable`. rustup's `cargo`/`rustc` proxies read that file on every invocation, ahead of the
  user's default and of whatever toolchain a CI action installed, so a fresh clone, every CI lane
  and a Catalyst cloud run build with the same compiler.
- **Install steps match the pin.** Every `dtolnay/rust-toolchain` step in `ci.yml` (5),
  `docs.yml`, `demo-assets.yml` and `release.yml` uses SHA `686976e1…` (`# stable`) with
  `toolchain: 1.98.0`. Without that input the action installs the floating stable its `@rev`
  names, which the override then ignores: each job downloaded a second toolchain (~8.5 s) and
  `Swatinem/rust-cache` hashed both versions into its key, so the cache missed after every
  upstream stable release.
- **Version rule.** Match the Catalyst runner image: "1.98.0 matches the Catalyst runner image, so
  cloud runs don't download a second toolchain" (`rust-toolchain.toml`). manta MAN-36 instead
  pins whatever `stable` resolved to when its pin landed; do not copy manta's rule here.
- **Bump procedure.** Change `rust-toolchain.toml`'s `channel` and every `toolchain:` input in
  `.github/workflows/` together, in their own PR; `grep -rn 'toolchain: 1\.98\.0'
  .github/workflows/` lists the inputs (`supply-chain.yml`'s step has none yet). `ci.yml`'s
  `changes` filter and `docs.yml`'s push filter both list `rust-toolchain.toml`, so the Rust
  lanes and the docs build re-run on the bump. A missed input still builds with the pinned
  compiler; it only brings the second download back.
- **Rejected.** Floating `stable`: on 2026-10-14 stale local stables (1.94.1, 1.96.1) on macOS 27
  emitted proc-macro dylibs that dyld refused to load. A `scripts/check-rust-toolchain-pin.sh`
  guard like manta's: CI run logs already show the override putting 1.98.0 behind every `cargo`
  call, and the one incident was on local machines a CI guard cannot see. `mise` or another
  cross-language manager: the repo is Rust/cargo only.
- **PAN-106 follow-up status.** This closes the `dtolnay/rust-toolchain` pinning in `ci.yml`,
  `docs.yml` and `demo-assets.yml`. Still deferred: pinning their other actions
  (`actions/checkout`, `Swatinem/rust-cache`), permission scoping for those files and
  `dependabot-auto-merge.yml`, and moving `supply-chain.yml`'s `dtolnay/rust-toolchain` pin
  (`4360b525…`, 2026-08-05) to `686976e1…`.

## Codex gate auto-resolve re-checks the head per thread (PAN-254, 2026-11-01)

- `compute-verdict` in `wait-for-codex.yml` reads `repos/$REPO/pulls/$PR`
  `.head.sha` at the top of every auto-resolve iteration, before the thread's
  `compare` call. A head that differs from the captured `HEAD_SHA` prints one
  `::notice::… mid-loop -- stopping auto-resolve here.` and `break`s, so no later
  thread gets a reply or a resolve. A failed lookup prints a `::warning::` and
  carries on with the captured head, the same fail-open stance as
  `freshness-check`; the next iteration checks again. `freshness-check` still
  runs once before the loop. Both messages are ported verbatim from propagation
  `f8e1243`, so the two files stay diffable. Two lines differ from upstream on
  purpose. The lookup is `LIVE_HEAD_SHA=$(gh api …) || LIVE_HEAD_SHA=""`, not
  upstream's `$(gh api … || echo "")`: on an HTTP error `gh api` still prints
  the JSON error body on stdout, even with `--jq`, so upstream's form reads a
  404 or 502 as a moved head and stops instead of failing open. The loop also
  reads a captured `resolvable_tsv` string through `<<<`, not a pipe from jq:
  with the pipe, `break` hit SIGPIPE (exit 141) under `pipefail` once the TSV
  outgrew the 64 KiB pipe buffer, which failed the step after
  `outcome=success` was written.
- `outcome=success` is written before the loop, so a stop changes no outcome:
  the check-run for the old head is still concluded, and threads left
  unresolved are baseline threads for the new head's own response run.
- Residual window: a push that lands after a thread's check can still let that
  one thread resolve with a reply naming the old head. GitHub has no
  compare-and-set on thread mutations, so a second check before the reply would
  only narrow the window, not close it; not added.
- Offline test: `scripts/ci/test-ci-codex-auto-resolve-head.sh` runs the real
  step body against a mock `gh` (head moves mid-loop, before the first thread,
  never, every lookup failing with a JSON error body, and a head move with 160
  long-path threads). `research-scan-tests` in `ci.yml` picks it
  up by glob. The workflow runs on `issue_comment` from main's copy, so a PR
  cannot exercise its own change to this step.
- Not ported: the other hunk of `f8e1243`, a retry on the request-marker POST
  in the request job. It is a different failure and needs its own ticket and
  test.
- Known gap, not changed here: `freshness-check`'s own retry loop keeps the
  last error body in `CURRENT_HEAD_SHA` after 3 failed lookups, so it marks the
  run stale instead of failing open as its warning says. Same root cause as the
  upstream lookup above; it needs its own ticket and test.

## Dependabot pass-through publishes the required Codex check (PAN-249, 2026-11-01)

PR #399 renamed `wait-for-codex.yml`'s producer job to `Codex review request (producer)` so its
native check-run never shares the name of the managed check, `Codex review clean (gate before
expensive downstream jobs)`. Before the rename, a job skipped by its `if:` published a native
`skipped` check-run under the required name, and Dependabot PRs #353 and #376 merged that way.
After it, the producer's `github.actor != 'dependabot[bot]'` exclusion meant nothing wrote the
managed check for a Dependabot PR. Run 40276929857 (Dependabot PR #525) shows every job skipped,
and ruleset 18627441 kept waiting for the context.

The exclusion's premise was that GitHub withholds Actions secrets from Dependabot runs. GitHub's
[Dependabot on Actions troubleshooting reference](https://docs.github.com/en/code-security/reference/supply-chain-security/troubleshoot-dependabot/dependabot-on-actions)
limits that to `push`, `pull_request`, `pull_request_review` and `pull_request_review_comment`,
and names `pull_request_target` as the way around it. Its one `pull_request_target` exception, a
base ref that Dependabot created, cannot apply here because the producer only runs for PRs into
`main`. Pancetta's own runs for PR #525 agree. CI
run 40276931889 (`pull_request`) logged `Secret source: Dependabot` with `Contents: read`. The
Dependabot Auto-Merge run 40276929798 (`pull_request_target`, same actor) logged
`Secret source: Actions` with `Contents: write` and `PullRequests: write`. The round-8 claim on
PR #399 that the App-token mint fails for Dependabot was a review comment and was never
reproduced in a run.

Decision. The producer admits every non-draft `pull_request_target` event, Dependabot included.
It mints the `hagale-agent` token in the main-only `codex-review` environment, as for any PR.
`create-check` classifies an event as a Dependabot update only when the actor is
`dependabot[bot]`, the PR author is `dependabot[bot]`, and the head repository is this
repository. For that event it upserts the managed check as `completed`/`success` with the run's
numeric `external_id`, and the `Request a review` step skips. A maintainer's push to a
Dependabot branch has a human actor, so it gets an ordinary Codex round. A re-run keeps the
original `github.actor`, so re-running a Dependabot-triggered producer run passes through again,
even if a Codex reply has since failed the check. That follows the bypass policy, and a re-run
needs write access. The check has one
writer, the App, so `codex-review-response` can still find and PATCH it if a later Codex reply
reports findings. The pass-through applies no `codex-clean:<sha12>` label. That is why `ci.yml`'s
`codex-clean` label gate keeps its Dependabot exclusion: polling for a label nobody applies would
wait out its timeout on every Dependabot rebase.

Rejected. A `workflow_run` relay off a `pull_request` no-op: the existing event already has
Actions secrets, the relay's own secret access for a Dependabot-originated chain is unproven
(pancetta's `workflow_run` runs carry `actor: dependabot[bot]`), and it needs separate PR-head
resolution because a `workflow_run`'s `github.sha` is the default-branch tip. A second writer
using `github.token`: `hagale-agent` cannot PATCH a check another app created, so a later Codex
round would post a duplicate. Naming the producer after the managed check: that restores the
native-plus-managed duplicate PR #399 removed.

This is not a secret-free path, and it does not merge Dependabot PRs automatically.
`auto-merge-trigger.yml` still admits only its author allowlist; admission is the separate
policy question in [General auto-merge for trusted-author PRs](#general-auto-merge-for-trusted-author-prs-2026-07-18).
Catalyst automation may still post `@codex review` on a Dependabot PR, and the response job
handles that reply as for any PR.

Evidence status:

- Verified offline here. `scripts/ci/test-ci-codex-gate-dependabot.sh` evaluates the shipped
  `if:` and `env:` expressions and runs the shipped `create-check` script against a stub `gh`.
  It covers admission, the three-part classification, the POST and PATCH writes, the
  `Request a review` skip, stale-head gating and `ci.yml`'s exclusion. Against the pre-PAN-249
  workflow it fails the admission, classification and completed-success cases.
- Sibling evidence. Coppa's producer, with no Dependabot exclusion, minted `hagale-agent` in its
  `codex-review` environment for Dependabot runs 40302227401 and 40302291006, and both heads
  carry a `completed`/`success` managed check. Propagation merged the same design as PRO-18.
- Awaiting rollout. No Dependabot run in pancetta has yet executed a job in the `codex-review`
  environment. On the first Dependabot PR after merge, check the producer's `Set up job` log
  for `Secret source: Actions`, confirm the current PR head carries one `hagale-agent`-owned
  managed check with the producer's numeric run ID, and confirm `Request a review` skipped. If
  the mint fails, the failsafe job tries its own mint and then `github.token` to publish a red
  `Codex review request setup failed` check. A failure caused by missing secrets would also fail
  the failsafe's mint, and `hagale-agent` cannot PATCH the resulting `github.token` check, so
  only a new head clears it. Diagnose that failure before trying another design.

Cross-repo scope: pancetta only. The workflow interface is unchanged, so no `dispensa` contract
applies; `dispensa` was not available to scan from the implementing session. Credenza's template
was not inspected.

## macOS microphone identity: embedded Info.plist and responsibility relaunch (PAN-264, 2026-11-04)

The macOS binary embeds `net.hagale.pancetta` and its microphone usage description in
`__TEXT,__info_plist`. Live station launches and doctor inspect the responsible
process before creating the Tokio runtime. An app launcher keeps its grant.
An already self-responsible process stays in place. Other launchers, including
ssh and ssh-created tmux, cause one disclaimed posix_spawn.

Both private libSystem symbols are loaded through dlsym. Missing symbols, failed
lookups, missing usage description and spawn errors retain the old behavior.
`PANCETTA_MACOS_RELAUNCH=0` also retains it. A child-only environment marker is
removed before threads start and prevents relaunch loops.

The thin parent forwards TERM/HUP through an async-signal-safe handler, ignores
INT/QUIT after spawning, and waits for the child. Signals are blocked across
spawn; SETSIGDEF resets INT/QUIT/TERM/HUP/PIPE and SETSIGMASK clears the child's
mask. Failed spawns restore the parent's handlers and original mask.
Normal exit codes pass through. INT, TERM, HUP, KILL and PIPE are re-raised;
other child signals map to 128 + signal to avoid a second crash report.

Only a self-responsible live station waits for AVFoundation authorization.
It requests once when undetermined, polls every two seconds and reminds every
ten minutes. Denied and restricted states wait with actionable instructions.
Ctrl-C and SIGTERM stop the wait. Doctor never requests authorization explicitly.
No-audio and recorded input paths never relaunch or wait.

SETEXEC would remove the parent, but the station probe verified spawn, not
SETEXEC. An app wrapper and Developer ID signing are separate work. Ad hoc
signatures tie permission to one binary; upgrades may need approval again.
The Mac matrix M1-M8 remains unrun, including the unbundled prompt and capture
during fast user switching. README capability status records that limit.

Sources: the PAN-264 sophon probe; LLDB's PosixSpawnResponsible.h;
Chromium's base/process/launch_mac.cc; Apple TN3127, "Inside Code Signing:
Requirements"; Apple's "Requesting authorization for media capture on macOS";
launchd.plist(5). Operator recovery steps are in docs/RUNBOOK.md.

## Supported-rig table (PAN-271, 2026-11-08)

`pancetta_hamlib::SUPPORTED_RIGS` (`pancetta-hamlib/src/models.rs`) is the one
rig table. The coordinator's `rigctld` spawner (`hamlib_model_id`),
`pancetta doctor`, the TUI rig picker's model check (`model_recognized`, which
accepts or rejects a typed name) and `docs/SUPPORTED_RIGS.md` all read it.
The page is generated by `render_supported_rigs_markdown` and drift-tested by
`supported_rigs_doc_is_current`, the KEYBINDINGS pattern; regenerate with
`PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-hamlib supported_rigs_doc_is_current`.
IC-7300 and IC-705 lead the page; the rest sort by manufacturer and model.

The old coordinator `match` sent seven names to another radio's backend. They
were corrected against upstream Hamlib `include/hamlib/riglist.h` (master,
2026-11-08): FT-991 1035, FT-891 1036, FT-710 1049, FTdx101MP 1044, TS-590S
2031, TS-590SG 2037, TS-890S 2041. `hamlib_ids_match_upstream_riglist` pins
every entry's number, so a new rig fails the test until its author checks
upstream. The dead `RigModelType::hamlib_id()` and `RigModelRegistry`, a second
ID and alias list with no callers, were removed.

A rig capability (CAT, audio, TX) becomes `Tested` only when the repository
records a hardware test, and the entry must name that record in `evidence`
(`tested_requires_evidence`). Today only the FTdx10 is tested.

## Cargo features: hamlib always built, encoder in every pancetta-ft8 build (PAN-331, 2026-11-09)

The `pancetta` crate no longer has a `pancetta-hamlib` feature; the
dependency is mandatory. The hamlib-off build had been broken since at least
GitHub #270 (20 errors then, 77 by PAN-331) because no CI lane built it, and
the default and release builds already included hamlib. Repairing it would
have kept a second build of the TX-safety code (the PAN-115 hard mute takes a
`pancetta_hamlib::Vfo`) that nothing ships. CI's `workspace-check` job now
runs `cargo check -p pancetta --no-default-features`, so the station crate
stays compiled without its default features. Since PAN-334 removed `metrics`
the crate has no default features, so that build equals the default one until
a default feature gains cfg sites. The opt-in `ft2` cfg arms are still built
by no CI lane. Replay mode still starts no Hamlib, so the "no Hamlib route"
paths remain.

`pancetta-ft8` compiles `encoder.rs` and `modulator.rs` in every build.
`transmit` gates only `transmit.rs`, the audio/PTT transmitter, with its
`cpal`/`serialport`/`tokio` stack. Before, the default build (the one manta
uses) compiled out the A7 templates, cross-sequence A7 and the GFSK
subtraction reference, so turning those flags on silently did nothing, and
`ap.rs` carried a private copy of `pack28`. There is no opt-out `encode`
feature: a cfg-gated module forces a cfg at every caller, which is how
`ap.rs` got its copy. Gate only device stacks. The cost is about 56 KB on the
`decode_cycle` binary and no new crate. `ap.rs` now calls
`encoder::pack28` and the fallible `encoder::try_packgrid`.

`serde` stays an opt-in feature. The encoder and modulator derives moved to
`#[cfg_attr(feature = "serde", ...)]` like every other derive in the crate,
and `transmit` enables `serde`.

CI's `ft8-tests` job runs `cargo test -p pancetta-ft8` with default features
(the standalone build) and checks `--no-default-features`; the `transmit`
configuration of every pancetta-ft8 test still runs in `workspace-check`'s
`--features transmit` test step. The default-features test build had been
broken by `examples/snr_calibration.rs` (it imports `Ft8Encoder`) without
anyone noticing, because workspace feature unification turns on
`pancetta-ft8/transmit` for every workspace build. `scripts/check.sh` now
checks `-p pancetta-ft8 --all-targets` on its own for the same reason.

Not done here: 16 `pancetta-ft8/tests/` files keep their `transmit` cfg
gates (whole-file or per-test) although the encoder and modulator they use
are no longer gated. Removing them touches 16 more files, which would take
this pull request past the 40-file limit. Those tests still run in
`workspace-check`'s `--features transmit` step, but the default-features
`ft8-tests` run skips them. The gate removals are in commit `ceff2c0` on the
PAN-331 branch.

## Every listed CLI flag does something (PAN-334, 2026-11-09)

Every flag and subcommand `pancetta --help` lists either works or is gone.

- **`--log-format json`** is built from tracing-subscriber's JSON formatter.
  `init_logging` keeps one text `Option` and one JSON `Option` layer per sink
  (the formatters are different types), and exactly one is `Some`. The log file
  and, with `--headless`, the console write one JSON object per line:
  `timestamp`, `level`, `target`, `threadId`, `fields` (with `message` and the
  event's other fields) and, inside a span, `span` and `spans`. The message
  stays in `fields.message`; flattening would drop the `fields` object and let
  an event field called `level` or `target` repeat a top-level key. Headless
  JSON goes to stderr, so stdout keeps only command output (the `--wav` decode
  lines, the gain-sweep report); journald and launchd capture both streams.
  Headless text stays on stdout and byte-identical. With a JSON console the
  plain `WARNING:` copy of each config-load warning is not printed, because the
  `warn!` already appears as JSON. The panic hook still chains Rust's default
  hook, so a crash prints its plain message after the JSON `panic` event.
- **Metrics are removed** (Tony, 2026-11-07): `--metrics`, `--metrics-port`,
  `init_metrics`, the coordinator's metrics fields and parameters, the
  `metrics` and `metrics-exporter-prometheus` dependencies, and the `metrics`,
  `prometheus` and `tracing-json` features. The exporter was behind an
  off-by-default feature and nothing recorded a metric, so the flags bound
  nothing.
- **`run` is a plain alias** of the bare command. `--callsign`, `--frequency`
  and `--power` were never read; a callsign override would let one process
  identify as a call that differs from its upload accounts, and `--frequency`
  had no config key (the dial is rig state from CAT).
- **`test-audio` lists devices** with or without `--list`. `--device` and
  `--duration` are gone: a capture test would need a PAN-264 macOS relaunch
  decision, and `pancetta doctor` already checks the input level.
- **`benchmark` is gone.** `benchmark-decode` and `cargo bench` cover it.
  `benchmark-decode --format` accepts only `text` and `json`.
- **Run-only flags are rejected elsewhere.** `--audio-device`, `--no-audio`,
  `--wav`, `--replay` and `--test-tx` are `global`, so every subcommand's
  `--help` lists them; `receiving_service_flag_on_subcommand` in `main()` exits
  2 when one is given to a subcommand other than `run`, before the macOS
  responsibility relaunch. `--test-tx-offset` without `--test-tx` exits 2
  from `test_tx_offset_without_test_tx` in `main()`, which runs after clap
  propagates globals; clap's `requires` checks each command level before
  propagation and so rejected `pancetta --test-tx X run --test-tx-offset 900`.
  The gain-sweep guard keeps its own message.
- **Follow-up, not done here:** `--config` on subcommands that never read it
  (`info`, `test-audio`, `benchmark-decode`, `export`, `import`, `setup`).
  `setup` always writes `~/.pancetta/pancetta.toml`, so honouring `--config`
  there needs its own design, and rejecting `--config` elsewhere would break
  shell aliases that pass it to every command.

`pancetta/tests/cli_surface.rs` checks every help screen (found from the live
`--help` output, so a new subcommand is covered), the removed flags' exit 2,
the guard, and both JSON sinks.

## Stable decoder API guard (PAN-380, 2026-11-10)

- **What is stable.** The union of two lists: the cycle API manta calls
  (`CycleDecoder::new`, `CycleDecoder::decode_cycle`, `decode_cycle`,
  `ft8lib_is_available`; dispensa ADR-0017, still `Proposed`) and the window
  API the dispensa Q-0085 answer names (`Ft8Decoder::new(Ft8Config)`,
  `decode_window`). Pinning only one would leave either the ticket or manta's
  real calls unguarded. `pancetta-ft8/README.md` § Stable API and
  compatibility policy is the list consumers read.
- **Only named fields.** Four `DecodedMessage` fields (`text`, `snr_db`,
  `frequency_offset`, `time_offset`) and two `Ft8Config` fields
  (`sample_rate`, `protocol`) are stable. Every other field stays free to
  change, as Q-0085 ("Stable API, and only this") and ADR-0017 say.
- **Hand-written compile-time guard, not `cargo public-api`.**
  `pancetta-ft8/tests/stable_api_guard.rs` coerces each pinned function to
  its fn-pointer type, so drift is an E0308 naming the item, and it runs in
  the existing `ft8-tests` and `workspace-check` jobs on the pinned 1.98.0.
  `cargo public-api` needs a nightly toolchain, a tool build and a pinned
  tool-to-nightly pairing, which the PAN-56 single-toolchain rule argues
  against. `readme_lists_every_pinned_item` keeps the guard and the README
  list in step.
- **`Ft8Config` stays exhaustive.** `#[non_exhaustive]` would forbid
  `Ft8Config { sample_rate, protocol, ..Ft8Config::default() }` from outside
  the crate, so the guard's struct literal fails (E0639) if it is added.
- **`trybuild` rejected.** Proving the guard fails would need a dev-dependency
  to show what CI already shows by failing to compile; validation repeats the
  mutations by hand.

## Windows compiled on every code PR (PAN-286, 2026-11-10)

Before this change only `release.yml` compiled Windows, on a `v*` tag, though
Windows 11 is the maintainer's station platform.

- **Leg and target.** `cross-platform-check` in `.github/workflows/ci.yml` has
  a `windows-latest` leg that checks `x86_64-pc-windows-gnu`, with the same
  runner, toolchain pin, ft8_lib submodule guard and MinGW `gcc --version`
  assert as `release.yml`. MSVC stays out: it cannot compile the VLAs in
  `vendor/ft8_lib/ft8/decode.c` (C2057).
- **Check command.** `cargo check --target x86_64-pc-windows-gnu --workspace
  --exclude pancetta-research --all-targets --features transmit`, the crate
  scope Linux tests. `--all-targets` also compiles `cfg(test)`, `tests/`,
  examples and benches, where three of today's five Windows-only warnings live.
- **Gating.** `cross-platform-check` is in the `CI` aggregator's `needs`, so a
  red Windows leg fails the required `CI` check. `needs` cannot pick one matrix
  leg, so the macOS leg is gating too; a flaky macOS runner now blocks a merge
  until it is re-run. On `push` and `merge_group` the job is `skipped`, which
  the aggregator ignores.
- **Not done.** No tests run on Windows: a bare `cargo test` on
  `windows-latest` uses the MSVC host, and a gnu test run is unverified. No
  `-D warnings`, because the Windows-only warnings would fail it.
- **Follow-ups, not done here:** the `code` path filter misses `.h` files,
  `.gitmodules` and the ft8_lib submodule; `deny.toml` lists
  `x86_64-pc-windows-msvc`, not `-gnu`; `actions/checkout@v7` and
  `Swatinem/rust-cache@v2` in this job stay unpinned (PAN-106).

`scripts/ci/test-ci-windows-lane.sh`, run by the `research-scan-tests` job,
pins the Windows leg, the `ci.needs` entry, the guards and the macOS-only
microphone test.

## DX-cluster reconnect backoff (PAN-288), 2026-11-10

A dropped DX-cluster connection reconnects with capped backoff instead of
spinning the worker on a closed spot channel.

- **The loop lives in `pancetta-dx`.** `DxClusterClient::run_until_shutdown`
  connects, forwards spots, and reconnects on one reused client (`connect()`
  replaces the channels and keeps the dedup map and dropped-spot counter). A
  session ends when `receive_spot()` returns `None`, which covers every way the
  reader stops; `status()` is not used because it stays `Connected` after a
  reader panic. The loop cannot hand the retry to the supervisor: a DxCluster
  task that returns `Ok(())` is not restarted, and an `Err` gets only 5
  restarts per 10 minutes. The shutdown flag is checked at least once a second.
- **Backoff.** Base `reconnect_delay_seconds.max(1)` s, doubled after each
  retry, capped at 10 × base (30, 60, 120, 240, 300, 300 s at the default), no
  jitter. A failed first connect uses the same schedule, so a cluster that is
  down at startup is retried. The delay resets to the base only after a session
  that lasted at least 60 s, so a server that accepts and closes at once is not
  retried at the minimum delay forever.
- **Config.** The worker reads `[network.dx_cluster.connection]`
  `reconnect_delay_seconds` and `timeout_seconds` (clamped to at least 1 s),
  both defaulting to the 30 s it hard-coded before, and always sets
  `auto_reconnect`. `reconnect_attempts`, `auto_connect` and
  `keepalive_interval_seconds` stay unread; `reconnect_attempts` is not a
  give-up limit, because giving up would end spots for the session silently.
- **Time tokens.** `parse_cluster_time` strips the `Z`, requires exactly four
  ASCII digits, and only then reads the hour and minute, so a multibyte
  character cannot land a slice mid-character (the old `&time_str[0..4]` slice
  panicked the reader). `parse_dx_spot` rejects a last token that ends in `Z`
  and is not ASCII, so that line falls through to `ClusterMessage::Raw`.
  ASCII-malformed tokens (`2560Z`, `BUZZ`) keep the `Utc::now()` fallback so
  real lines with odd trailing fields are not dropped.
- **Not done here:** half-open TCP detection (`keepalive_interval_seconds`),
  rotating through `servers`, surviving a non-UTF-8 line without a reconnect,
  and a "reconnecting" state in the TUI.
