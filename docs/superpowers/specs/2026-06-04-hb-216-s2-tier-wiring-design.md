# hb-216 Session 2 — Hardware-tier classifier coordinator wiring (design)

**Date**: 2026-06-04
**Predecessor**: hb-216 S1 (probe + classifier module shipped 1c40954)
**Status**: Design — ready for implementation

## Goal

Make the hardware-tier classifier shipped in S1 actually mutate runtime
behavior. On the first boot after install (or on hardware swap, or after
a pancetta upgrade), pancetta probes itself, classifies the host into
Fast/Moderate/Slow, persists the result, and applies the per-tier
configuration automatically — so a MiniPC or Pi 4 operator gets the
right decoder tuning without ever setting an env var.

## Non-goals

- Cross-validating tier thresholds against more hardware. The Fast /
  Moderate / Slow boundaries remain the M4-anchored synthetic-baseline
  values from S1; refinement is data-driven and lives in a future batch.
- A pancetta-config schema field for tier override. The env var
  `PANCETTA_SCOPED_FAST_PATH` (already documented) covers operator
  override; adding a config field doubles the surface area for
  questionable benefit.
- TUI surfacing of the tier classification. Logs only for now.

## Decisions (from brainstorming 2026-06-04)

1. **Probe lifecycle**: background after startup. The coordinator does
   not block on the probe; the FT8 thread starts immediately on defaults
   and the tier applies as soon as the probe completes (~2s on Fast,
   ~30s on Slow, **zero** on cache-hit boots after the first).
2. **Activation wire**: `Arc<AtomicBool>` for the scoped-fast-path flag,
   shared between coordinator state and the FT8 hot loop. The existing
   env var read at `coordinator/ft8.rs:159-160` is replaced with an
   atomic load. The env var seeds the atomic at startup.
3. **Cache schema**: JSON at `~/.pancetta/tier_cache.json`, keyed on
   `(cpu_model, core_count, pancetta_version)`. Mismatch triggers
   re-probe; otherwise the cached tier applies immediately.
4. **Operator override**: three-state env var. `PANCETTA_SCOPED_FAST_PATH=1`
   forces on (skips probe activation but probe still runs to populate
   cache); `=0` forces off (still probes for cache); unset = probe decides.

## Architecture

```
                    coordinator startup
                          │
                          ├── 1. seed scoped_fast_path_atomic from env var:
                          │        env=1 → atomic.store(true),  override="force-on"
                          │        env=0 → atomic.store(false), override="force-off"
                          │        unset → atomic.store(false), override=None
                          │
                          ├── 2. cache_load() from ~/.pancetta/tier_cache.json
                          │        key match → tier known
                          │                   → apply (atomic.store unless override)
                          │                   → still spawn probe? NO (cache is authoritative)
                          │        miss / stale / parse error → schedule probe
                          │
                          ├── 3. spawn ft8 / dsp / audio threads
                          │        (FT8 thread gets cloned Arc<AtomicBool>
                          │         + cloned Arc<RwLock<Ft8Config>>)
                          │
                          └── 4. if probe scheduled:
                                  tokio::task::spawn(probe_worker(...))
                                       runs probe_hardware_tier(10)
                                       writes cache (best-effort)
                                       applies tier (atomic.store unless override)
                                       writes Slow-tier preset to Ft8Config
                                              (unless override="force-off")
```

## Components

### NEW: `pancetta/src/coordinator/tier.rs`

Module containing:
- `TierCache` struct (cpu_model, core_count, pancetta_version, tier as
  string, probed_at as RFC 3339 timestamp).
- `load_cache(path: &Path) -> Option<TierCache>` — tolerant of missing /
  malformed files; returns None and logs at `debug!`.
- `save_cache(path: &Path, cache: &TierCache)` — best-effort; logs at
  `warn!` on failure but never panics.
- `current_hardware_key() -> (String, usize)` — reads CPU model + core
  count via `num_cpus::get()` and a platform probe (`sysctl
  -n machdep.cpu.brand_string` on macOS, `/proc/cpuinfo` on Linux,
  `wmic cpu get name` on Windows — first one that succeeds wins; fallback
  to `std::env::consts::ARCH`).
- `cache_path() -> PathBuf` — `~/.pancetta/tier_cache.json`.
- `Override` enum: `ForceOn | ForceOff | None`.
- `parse_override_env() -> Override` — reads `PANCETTA_SCOPED_FAST_PATH`,
  maps `"1"` → `ForceOn`, `"0"` → `ForceOff`, anything else → `None`.
  Returns `None` when the var is unset.
- `apply_tier(tier, override, scoped_fast_path: &AtomicBool, ft8_config: &RwLock<Ft8Config>)`
  — sets the atomic per tier+override and rewrites `Ft8Config` if Slow.
- `initialize(ft8_config: Arc<RwLock<Ft8Config>>) -> Arc<AtomicBool>` —
  the orchestrator the coordinator calls during `new()`. Seeds the
  atomic from env, checks the cache, and spawns the probe-worker if
  needed. Returns the atomic that gets handed to the FT8 thread.
- Unit tests for cache round-trip, key construction, env-var parsing,
  and tier-application logic with all override states.

### MODIFIED: `pancetta/src/coordinator/mod.rs`

`ApplicationCoordinator` gains two new fields:

```rust
pub(crate) scoped_fast_path: Arc<AtomicBool>,
pub(crate) ft8_config: Arc<RwLock<Ft8Config>>,
```

`new()` calls `tier::initialize(ft8_config.clone())` and stores the
returned atomic. The `Ft8Config` is initialized to `Ft8Config::default()`
and exposed via `RwLock` so the probe-worker can rewrite it for Slow
tier before the FT8 thread reads it.

### MODIFIED: `pancetta/src/coordinator/ft8.rs`

Two changes:

1. Replace the env-var read at line 159 with an atomic load:

   ```rust
   let scoped_fast_path = self.scoped_fast_path.clone();  // captured into thread
   // ... in hot loop:
   let scoped_fast_path_enabled =
       scoped_fast_path.load(std::sync::atomic::Ordering::Relaxed);
   ```

2. Replace the local `Ft8Config::default()` at line 40 with a read of
   the shared `RwLock`. Re-read at the top of each window iteration; if
   the config has changed since the last decode, rebuild the decoder.
   The compare-key is a tuple of mutable fields
   (`max_decode_passes`, `osd_depth`), not full structural equality
   (Ft8Config doesn't impl PartialEq).

### NEW: `pancetta-research/examples/tier_probe.rs` — already exists from S1.

Already produces the right shape; no change.

## Cache schema (concrete)

```json
{
  "schema_version": 1,
  "cpu_model": "Apple M4",
  "core_count": 10,
  "pancetta_version": "0.1.0",
  "tier": "fast",
  "p50_ms": 210,
  "p95_ms": 213,
  "p99_ms": 213,
  "probed_at": "2026-06-04T12:34:56Z"
}
```

`schema_version: 1` lets a future S3 evolve the schema without breaking
load on an old cache file — a mismatched schema version is treated as a
miss and triggers re-probe.

## Per-tier `Ft8Config` mutation

For **Slow** tier, write into the `RwLock<Ft8Config>`:

```rust
config.max_decode_passes = 1;   // skip multipass — kills the bimodal tail
config.osd_depth = Some(1);     // cheaper OSD fallback
```

For Fast and Moderate, leave the config at defaults.

The FT8 hot loop re-reads the config at the top of each window
iteration (cheap: `RwLock::read()` of a small struct, hit or miss in
~µs). If the active config's `(max_decode_passes, osd_depth)` tuple
differs from the value used to build the current decoder, the decoder
is rebuilt. This happens **at most once** per process lifetime in
practice (probe lands once, writes once, decoder rebuilds once on the
next window).

## Logging

Three log lines at INFO level over the boot:

1. At startup, immediately after seeding:
   `tier: env override = force-on | force-off | none`
2. After cache load:
   `tier: cache hit (Apple M4, 10 cores, v0.1.0) → fast` OR
   `tier: cache miss/stale, scheduling background probe`
3. After probe-worker completes:
   `tier: probe complete (Apple M4, 10 cores) p50=210ms p95=213ms p99=213ms → fast, no recommendations`
   OR (when applying):
   `tier: probe complete (Intel N100, 4 cores) p50=620ms p95=890ms → moderate, enabling scoped fast-path`
   OR:
   `tier: probe complete (Cortex-A72, 4 cores) p50=1200ms p95=2100ms → slow, enabling scoped fast-path + reducing max_decode_passes to 1 + osd_depth to Some(1)`

## Override interaction matrix

| env var | probe result | atomic final | Ft8Config slow preset? |
|---|---|---|---|
| unset | Fast      | false | no  |
| unset | Moderate  | true  | no  |
| unset | Slow      | true  | yes |
| `=1`  | (any)     | true  | no¹ |
| `=0`  | (any)     | false | no  |

¹ `=1` is "force on the existing knob"; we don't *also* rewrite
`max_decode_passes` on the operator's behalf when they've explicitly
flagged in. If the operator wants the full Slow preset, they leave the
env var unset and let the probe decide.

## Testing

**Unit tests** (in `coordinator/tier.rs::tests`):

1. `parse_override_env` — three cases (`"1"`, `"0"`, missing).
2. `cache_round_trip` — write `TierCache` to a `tempfile::NamedTempFile`,
   read back, compare.
3. `cache_load_handles_missing_file` → `None`.
4. `cache_load_handles_malformed_json` → `None`, logs at `debug`.
5. `cache_load_handles_schema_mismatch` (`schema_version: 999`) → `None`.
6. `apply_tier_fast_with_no_override` → atomic stays false, config
   untouched.
7. `apply_tier_moderate_with_no_override` → atomic true, config
   untouched.
8. `apply_tier_slow_with_no_override` → atomic true, config gets slow
   preset.
9. `apply_tier_slow_with_force_off_override` → atomic false, config
   untouched (operator override wins).
10. `apply_tier_fast_with_force_on_override` → atomic true (env wins),
    config untouched.

**Integration smoke** (in `pancetta/tests/`):

11. `tier_initialize_with_cache_hit_skips_probe` — pre-populate a cache
    file in a temp HOME, call a test-only entry point that runs the
    initialize logic, verify the atomic reflects the cached tier and
    no probe runs (use a `did_probe_run` test hook).
12. `tier_initialize_with_no_cache_spawns_probe` — empty temp HOME,
    verify probe-worker spawns. Don't await the probe (it's slow and
    flaky on CI by design — but this test is local-only / not CI, and
    is `#[ignore]` in standard runs).

Test #12 stays `#[ignore]` and runs only when explicitly invoked
(`cargo test -p pancetta -- --ignored`), matching pancetta's existing
convention for slow/integration tests.

**Coordinator wide** (not new tests, just keep green):

13. `cargo test --workspace --features transmit` must remain green.
14. `cargo test -p pancetta-ft8` must remain green.

## Rollout / risk

This change is **default-active on first boot** for any operator who
upgrades. The risks are:

- **Probe failure** (couldn't encode synth signal, etc.): treat as a
  no-op — log at `warn!`, leave defaults active, do not retry this
  boot. Cache is *not* written. Next boot will try again.
- **Cache corruption**: handled as a miss → re-probe. No operator
  action required.
- **Misclassification on first boot**: the probe runs a single
  10-iteration synthetic-decode burst. False-positive Slow classification
  triggers the slow preset (`max_decode_passes = 1`, scoped fast-path
  on), which reduces decode coverage. Mitigation: operator sets
  `PANCETTA_SCOPED_FAST_PATH=0` to force-off and delete the cache file;
  next boot re-probes.
- **`num_cpus` / cpuinfo-probe variance** (e.g., a CPU with a noisy
  brand string field, big.LITTLE differential core count between boots):
  could cause unnecessary re-probes. Acceptable — background probe is
  cheap.

## Compatibility

- **No breaking changes** to public APIs of any crate.
- The env var is still honored exactly as before (it now seeds an
  atomic instead of being read in the hot loop; behavior identical
  from the operator's POV).
- A pre-existing cache file from S1 (none exist) would be a forward
  schema; we handle that as a miss.

## File touch list

| Path | Action |
|---|---|
| `pancetta/src/coordinator/tier.rs` | NEW |
| `pancetta/src/coordinator/mod.rs` | add fields + init call |
| `pancetta/src/coordinator/ft8.rs` | atomic load + config re-read |
| `pancetta/Cargo.toml` | add `dirs` if not present (for `~/.pancetta/`) |
| `research/experiments/2026-06-04-hb-216-session2.md` | NEW (journal) |
| `research/hypothesis_bank.md` | mark hb-216 SESSION-2-COMPLETE |
| `CLAUDE.md` | add "Hardware tier auto-classification" bullet under Known Gaps → flip to a positive feature |
| `docs/superpowers/specs/2026-06-04-hb-216-s2-tier-wiring-design.md` | THIS doc |

## Acceptance criteria

1. Fresh boot on M4 (no cache) → background probe runs, lands Fast,
   cache written, no behavior change. Logs reflect the classification.
2. Second boot on M4 → cache hit, no probe, defaults active.
3. Manual cache forgery (write a Slow record) → boot applies Slow
   preset (`max_decode_passes=1`, scoped fast-path on). Boot with
   `PANCETTA_SCOPED_FAST_PATH=0` overrides this.
4. All unit + integration tests pass.
5. `cargo fmt`, `cargo clippy --workspace --features transmit
   -- -D warnings` clean.
6. `cargo test --workspace --features transmit` green.
