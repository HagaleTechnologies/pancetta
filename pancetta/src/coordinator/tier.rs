//! Hardware-tier auto-classification at coordinator startup (hb-216 S2).
//!
//! Wires the [`pancetta_ft8::tier_probe`] classifier into pancetta's
//! boot path so a fresh install on a MiniPC or Pi 4 gets the right
//! decoder tuning automatically — without the operator setting
//! `PANCETTA_SCOPED_FAST_PATH` by hand.
//!
//! ## Lifecycle
//!
//! 1. [`initialize`] seeds an `Arc<AtomicBool>` from the env var (operator
//!    override), checks the on-disk cache, and either applies the cached
//!    tier directly or spawns a background probe.
//! 2. The atomic is handed to the FT8 hot loop; reading it is a single
//!    relaxed load per window iteration.
//! 3. `initialize` and the background probe worker both also re-seed the
//!    `decode_effort_budget_ms` atomic (decoder-speed-overhaul Task 14, see
//!    `effort.rs`) once the tier is known — this **replaces** the old
//!    Slow-tier `Ft8Config` numeric-field rewrite (`max_decode_passes=1` +
//!    `max_sync_candidates=150`) that used to live in [`apply_tier`]: the
//!    `[decoder]` effort-preset system now owns tuning decode thoroughness
//!    for the Slow tier via the budget atomic instead of mutating
//!    `Ft8Config` fields directly. `scoped_fast_path` handling is
//!    unaffected by this change — it remains a separate, still-valid
//!    mechanism.
//!
//! ## Override matrix
//!
//! | env var | probe result | atomic final | Ft8Config preset |
//! |---------|--------------|--------------|------------------|
//! | unset   | Fast         | false        | none (defaults; preset retired Batch 83) |
//! | unset   | Moderate     | true         | none (defaults)  |
//! | unset   | Slow         | true         | none (defaults; Slow-tier preset retired Task 14 — see `effort.rs`) |
//! | `"1"`   | (any)        | true         | none (operator chose) |
//! | `"0"`   | (any)        | false        | none (operator chose) |
//!
//! The Fast preset trades wall-clock for sensitivity:
//! - Batch 36 B1: `max_decode_passes=2` (+32 TPs / hard-200, 2.0× WC)
//! - Batch 41: `ldpc_iterations=200` (+16 TPs / hard-200; early-termination
//!   means avg iter count rises modestly)
//!
//! Hosts with the compute budget pay it; Moderate/Slow stay at the default.
//!
//! See `docs/superpowers/specs/2026-06-04-hb-216-s2-tier-wiring-design.md`
//! and `docs/superpowers/specs/2026-07-06-decoder-speed-overhaul-design.md`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use pancetta_config::DecodeEffort;
use pancetta_ft8::tier_probe::{recommend_actions, HardwareTier};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::effort::seed_effort_budget;

const CACHE_SCHEMA_VERSION: u32 = 1;
const ENV_OVERRIDE: &str = "PANCETTA_SCOPED_FAST_PATH";

/// Operator-supplied override of the scoped-fast-path flag.
///
/// `ForceOn` and `ForceOff` short-circuit the probe's decision; `None`
/// means "trust the probe."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Override {
    ForceOn,
    ForceOff,
    None,
}

impl Override {
    fn as_str(&self) -> &'static str {
        match self {
            Override::ForceOn => "force-on",
            Override::ForceOff => "force-off",
            Override::None => "none",
        }
    }
}

/// Parse the override env var.
///
/// `"1"` → ForceOn, `"0"` → ForceOff, anything else (including absent)
/// → None. Pure function on the supplied value; the production caller
/// reads `std::env::var(ENV_OVERRIDE)` and feeds the result here.
pub(crate) fn parse_override(value: Option<&str>) -> Override {
    match value {
        Some("1") => Override::ForceOn,
        Some("0") => Override::ForceOff,
        _ => Override::None,
    }
}

/// On-disk cache record. JSON at `~/.pancetta/tier_cache.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TierCache {
    pub schema_version: u32,
    pub cpu_model: String,
    pub core_count: usize,
    pub pancetta_version: String,
    pub tier: String,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
    pub probed_at: String,
}

impl TierCache {
    fn parse_tier(&self) -> Option<HardwareTier> {
        match self.tier.as_str() {
            "fast" => Some(HardwareTier::Fast),
            "moderate" => Some(HardwareTier::Moderate),
            "slow" => Some(HardwareTier::Slow),
            _ => None,
        }
    }
}

/// Default cache path: `~/.pancetta/tier_cache.json`. Returns None if
/// the home directory cannot be located.
pub(crate) fn default_cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".pancetta").join("tier_cache.json"))
}

/// Read and parse the cache file. Returns `None` on any error: missing
/// file, malformed JSON, mismatched schema version. All non-trivial
/// failures log at `debug!`.
pub(crate) fn load_cache(path: &Path) -> Option<TierCache> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            debug!("tier cache: read failed at {}: {}", path.display(), e);
            return None;
        }
    };
    let cache: TierCache = match serde_json::from_slice(&bytes) {
        Ok(c) => c,
        Err(e) => {
            debug!("tier cache: parse failed at {}: {}", path.display(), e);
            return None;
        }
    };
    if cache.schema_version != CACHE_SCHEMA_VERSION {
        debug!(
            "tier cache: schema version {} != expected {}; re-probing",
            cache.schema_version, CACHE_SCHEMA_VERSION
        );
        return None;
    }
    Some(cache)
}

/// Write the cache atomically (temp + rename). Best-effort: failures
/// log at `warn!` but do not propagate. Creates parent directory if
/// missing.
pub(crate) fn save_cache(path: &Path, cache: &TierCache) {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!("tier cache: failed to create {}: {}", parent.display(), e);
            return;
        }
    }
    let json = match serde_json::to_vec_pretty(cache) {
        Ok(j) => j,
        Err(e) => {
            warn!("tier cache: serialize failed: {}", e);
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, &json) {
        warn!("tier cache: write to {} failed: {}", tmp.display(), e);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        warn!(
            "tier cache: rename {} → {} failed: {}",
            tmp.display(),
            path.display(),
            e
        );
    }
}

/// Best-effort host identification: CPU model + logical core count.
///
/// CPU model is platform-probed (sysctl on macOS, /proc/cpuinfo on
/// Linux, wmic on Windows). On failure, falls back to
/// `std::env::consts::ARCH`. Core count comes from `num_cpus::get()`.
pub(crate) fn current_hardware_key() -> (String, usize) {
    let core_count = num_cpus::get();
    let cpu_model = detect_cpu_model().unwrap_or_else(|| std::env::consts::ARCH.to_string());
    (cpu_model, core_count)
}

#[cfg(target_os = "macos")]
fn detect_cpu_model() -> Option<String> {
    let output = std::process::Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(target_os = "linux")]
fn detect_cpu_model() -> Option<String> {
    let content = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("model name") {
            if let Some((_, value)) = rest.split_once(':') {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn detect_cpu_model() -> Option<String> {
    let output = std::process::Command::new("wmic")
        .args(["cpu", "get", "name", "/value"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("Name=") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn detect_cpu_model() -> Option<String> {
    None
}

/// Apply a tier classification to runtime state. Honors operator
/// override: `ForceOn`/`ForceOff` short-circuit the atomic.
///
/// Returns a short human-readable description of what changed, suitable
/// for logging.
///
/// Prior to decoder-speed-overhaul Task 14, this also rewrote
/// `Ft8Config.max_decode_passes`/`max_sync_candidates` on the Slow tier.
/// That rewrite is retired — the `[decoder]` effort-preset system
/// (`effort.rs`) now owns tuning decode thoroughness per tier via the
/// `decode_effort_budget_ms` atomic instead. This function no longer
/// touches `Ft8Config` at all; `scoped_fast_path` handling is unchanged.
pub(crate) async fn apply_tier(
    tier: HardwareTier,
    override_: Override,
    scoped_fast_path: &AtomicBool,
) -> String {
    // Atomic flip.
    let (atomic_value, atomic_reason) = match override_ {
        Override::ForceOn => (true, "env override (force-on)"),
        Override::ForceOff => (false, "env override (force-off)"),
        Override::None => match tier {
            HardwareTier::Fast => (false, "fast tier (defaults)"),
            HardwareTier::Moderate => (true, "moderate tier"),
            HardwareTier::Slow => (true, "slow tier"),
        },
    };
    scoped_fast_path.store(atomic_value, Ordering::Release);

    format!("scoped_fast_path={} ({})", atomic_value, atomic_reason)
}

/// Whether a tier-probe completion should re-seed `decode_effort_budget_ms`
/// given the operator's LIVE decode-effort preset (final-review Finding 1).
///
/// Pure decision function, factored out of [`spawn_probe_worker`] so the
/// race-guard logic is unit-testable without spawning a real hardware
/// probe: `Auto` is the only preset whose resolved budget depends on the
/// tier (see `effort::preset_budget_ms`), so re-seeding is only correct —
/// and only necessary — when the live preset is still `Auto`. A `false`
/// here means the operator has explicitly cycled to a non-Auto preset
/// since startup and that choice must not be clobbered.
fn probe_completion_should_reseed_budget(live_effort: DecodeEffort) -> bool {
    live_effort == DecodeEffort::Auto
}

/// Spawn the background probe worker. Runs `probe_hardware_tier(10)`,
/// persists the cache, and applies the tier to runtime state. All
/// failures log and return — never panic.
///
/// Also re-seeds `decode_effort_budget_ms` (decoder-speed-overhaul Task 14)
/// with the now-known tier via [`seed_effort_budget`], honoring
/// `budget_override` when set, and stores the resolved tier into
/// `resolved_hardware_tier` (decoder-speed-overhaul Task 15) so a later live
/// `Auto` decode-effort cycle resolves the correct tier-derived budget.
///
/// **Race guard (final-review Finding 1):** this runs in the background,
/// concurrently with the live TUI, so the operator may have already
/// pressed `e` (cycling `current_decode_effort` away from `Auto`) before
/// this completes. The budget re-seed reads the LIVE `current_decode_effort`
/// atomic (not a value captured at coordinator startup) and only
/// re-resolves/re-applies the tier-derived budget when that live value is
/// still `Auto` — `Auto` is the only preset whose resolved budget actually
/// depends on the tier (see `preset_budget_ms`), so this never clobbers an
/// operator's explicit non-Auto choice, and is a no-op change of behavior
/// for the untouched-`Auto` case.
#[allow(clippy::too_many_arguments)]
fn spawn_probe_worker(
    cpu_model: String,
    core_count: usize,
    pancetta_version: String,
    cache_path: Option<PathBuf>,
    override_: Override,
    scoped_fast_path: Arc<AtomicBool>,
    budget_override: Option<u64>,
    decode_effort_budget_ms: Arc<AtomicU64>,
    current_decode_effort: Arc<AtomicU8>,
    resolved_hardware_tier: Arc<AtomicU8>,
) {
    tokio::task::spawn_blocking(move || {
        let result = match pancetta_ft8::tier_probe::probe_hardware_tier(10) {
            Ok(r) => r,
            Err(e) => {
                warn!("tier probe: failed ({}); leaving defaults active", e);
                return;
            }
        };

        let recs = recommend_actions(result.tier);
        let recs_summary = if recs.is_empty() {
            "no recommendations".to_string()
        } else {
            recs.iter().map(|r| r.key).collect::<Vec<_>>().join(", ")
        };
        info!(
            "tier probe: complete (cpu='{}', cores={}) p50={}ms p95={}ms p99={}ms → {} ({})",
            cpu_model,
            core_count,
            result.p50.as_millis(),
            result.p95.as_millis(),
            result.p99.as_millis(),
            result.tier.as_str(),
            recs_summary
        );

        if let Some(path) = cache_path {
            let cache = TierCache {
                schema_version: CACHE_SCHEMA_VERSION,
                cpu_model: cpu_model.clone(),
                core_count,
                pancetta_version,
                tier: result.tier.as_str().to_string(),
                p50_ms: result.p50.as_millis() as u64,
                p95_ms: result.p95.as_millis() as u64,
                p99_ms: result.p99.as_millis() as u64,
                probed_at: chrono::Utc::now().to_rfc3339(),
            };
            save_cache(&path, &cache);
        }

        let summary = tokio::runtime::Handle::current().block_on(apply_tier(
            result.tier,
            override_,
            &scoped_fast_path,
        ));
        info!("tier probe: applied — {}", summary);

        // Finding 1: only re-seed the budget atomic if the operator hasn't
        // already cycled `e` away from `Auto` while this probe was running.
        // Non-Auto presets resolve to the same budget regardless of tier
        // (see `preset_budget_ms`), so skipping them here changes nothing
        // for the untouched case and avoids clobbering an explicit choice.
        let live_effort = DecodeEffort::from_u8(current_decode_effort.load(Ordering::Acquire));
        if probe_completion_should_reseed_budget(live_effort) {
            seed_effort_budget(
                DecodeEffort::Auto,
                budget_override,
                result.tier,
                &decode_effort_budget_ms,
            );
            info!(
                "tier probe: decode_effort_budget_ms re-seeded for {} tier (Auto)",
                result.tier.as_str()
            );
        } else {
            debug!(
                "tier probe: decode_effort_budget_ms NOT re-seeded — operator has already \
                 cycled decode effort to {:?}; tier is still recorded for a future Auto cycle",
                live_effort
            );
        }
        resolved_hardware_tier.store(result.tier.as_u8(), Ordering::Release);
    });
}

/// Coordinator-startup entry point.
///
/// Reads the override env var, checks the on-disk cache, and either
/// applies the cached tier synchronously or schedules a background
/// probe. Returns the `Arc<AtomicBool>` that the FT8 hot loop reads.
///
/// Also re-seeds `decode_effort_budget_ms` (decoder-speed-overhaul Task 14)
/// once the tier is known — on the synchronous cache-hit path here, or
/// asynchronously from the background probe worker (see
/// [`spawn_probe_worker`]). The caller is expected to have already seeded
/// `decode_effort_budget_ms` with an assumed tier before calling this (see
/// `coordinator/mod.rs`), since a cache-miss means the real tier isn't
/// known until the probe completes.
///
/// Also stores the resolved tier into `resolved_hardware_tier`
/// (decoder-speed-overhaul Task 15), same seed-then-reconcile shape, so the
/// TUI's live `e` decode-effort cycle can resolve `Auto` against the real
/// tier without re-probing.
///
/// `current_decode_effort` is the live atomic the TUI's `e` keybinding
/// cycles (`effort::cycle_decode_effort`); the background probe worker
/// (see [`spawn_probe_worker`]) reads it at re-seed time so an operator
/// choice made before the probe completes is never silently clobbered
/// (final-review Finding 1). The synchronous cache-hit path below has no
/// such race (it runs before the TUI can accept input), so it seeds from
/// the startup `effort` value directly, same as before.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn initialize(
    effort: DecodeEffort,
    budget_override: Option<u64>,
    decode_effort_budget_ms: Arc<AtomicU64>,
    current_decode_effort: Arc<AtomicU8>,
    resolved_hardware_tier: Arc<AtomicU8>,
) -> Arc<AtomicBool> {
    let scoped_fast_path = Arc::new(AtomicBool::new(false));

    let env_value = std::env::var(ENV_OVERRIDE).ok();
    let override_ = parse_override(env_value.as_deref());
    info!("tier: env override = {}", override_.as_str());

    // Seed the atomic from env immediately. The probe (if any) will
    // re-apply after measurement, but only if override == None.
    if override_ == Override::ForceOn {
        scoped_fast_path.store(true, Ordering::Release);
    }

    let (cpu_model, core_count) = current_hardware_key();
    let pancetta_version = env!("CARGO_PKG_VERSION").to_string();
    let cache_path = default_cache_path();

    let cached: Option<TierCache> = cache_path.as_deref().and_then(load_cache);

    let need_probe = match &cached {
        Some(c)
            if c.cpu_model == cpu_model
                && c.core_count == core_count
                && c.pancetta_version == pancetta_version =>
        {
            if let Some(tier) = c.parse_tier() {
                let summary = apply_tier(tier, override_, &scoped_fast_path).await;
                info!(
                    "tier: cache hit (cpu='{}', cores={}, v{}) → {} — {}",
                    cpu_model,
                    core_count,
                    pancetta_version,
                    tier.as_str(),
                    summary
                );
                seed_effort_budget(effort, budget_override, tier, &decode_effort_budget_ms);
                resolved_hardware_tier.store(tier.as_u8(), Ordering::Release);
                false
            } else {
                debug!("tier cache: unknown tier string '{}', re-probing", c.tier);
                true
            }
        }
        Some(_) => {
            info!("tier: cache stale (host or version changed), scheduling background probe");
            true
        }
        None => {
            info!("tier: no cache, scheduling background probe");
            true
        }
    };

    if need_probe {
        spawn_probe_worker(
            cpu_model,
            core_count,
            pancetta_version,
            cache_path,
            override_,
            scoped_fast_path.clone(),
            budget_override,
            decode_effort_budget_ms,
            current_decode_effort,
            resolved_hardware_tier,
        );
    }

    scoped_fast_path
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_cache(tier: &str) -> TierCache {
        TierCache {
            schema_version: CACHE_SCHEMA_VERSION,
            cpu_model: "Apple M4".to_string(),
            core_count: 10,
            pancetta_version: "0.1.0".to_string(),
            tier: tier.to_string(),
            p50_ms: 210,
            p95_ms: 213,
            p99_ms: 213,
            probed_at: "2026-06-04T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn parse_override_one_is_force_on() {
        assert_eq!(parse_override(Some("1")), Override::ForceOn);
    }

    #[test]
    fn parse_override_zero_is_force_off() {
        assert_eq!(parse_override(Some("0")), Override::ForceOff);
    }

    #[test]
    fn parse_override_missing_is_none() {
        assert_eq!(parse_override(None), Override::None);
    }

    #[test]
    fn parse_override_garbage_is_none() {
        assert_eq!(parse_override(Some("yes")), Override::None);
        assert_eq!(parse_override(Some("")), Override::None);
    }

    #[test]
    fn cache_round_trip_preserves_fields() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("tier_cache.json");
        let original = make_cache("fast");
        save_cache(&path, &original);
        let loaded = load_cache(&path).expect("loaded");
        assert_eq!(loaded.cpu_model, "Apple M4");
        assert_eq!(loaded.tier, "fast");
        assert_eq!(loaded.p95_ms, 213);
    }

    #[test]
    fn cache_load_missing_file_is_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("does_not_exist.json");
        assert!(load_cache(&path).is_none());
    }

    #[test]
    fn cache_load_malformed_json_is_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("malformed.json");
        std::fs::write(&path, b"not json at all").unwrap();
        assert!(load_cache(&path).is_none());
    }

    #[test]
    fn cache_load_schema_version_mismatch_is_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("future_schema.json");
        std::fs::write(
            &path,
            br#"{"schema_version":999,"cpu_model":"x","core_count":1,"pancetta_version":"x","tier":"fast","p50_ms":1,"p95_ms":1,"p99_ms":1,"probed_at":"x"}"#,
        )
        .unwrap();
        assert!(load_cache(&path).is_none());
    }

    #[tokio::test]
    async fn apply_tier_fast_no_override_clears_atomic() {
        // Batch 83: the Batch 36/41 Fast preset (mp=2, ldpc=200) was
        // retired — under ft8_lib truth it bought +24..+57 TPs for
        // +142..+387 FPs at 2.6-3.9x decode time, strictly dominated
        // by the ldpc=300 recall lever. Fast tier now runs defaults.
        let atomic = AtomicBool::new(true); // pre-set to verify it gets cleared
        apply_tier(HardwareTier::Fast, Override::None, &atomic).await;
        assert!(!atomic.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn apply_tier_fast_with_force_off_leaves_atomic_clear() {
        // Operator override always wins over the tier-driven decision.
        let atomic = AtomicBool::new(false);
        apply_tier(HardwareTier::Fast, Override::ForceOff, &atomic).await;
        assert!(!atomic.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn apply_tier_moderate_no_override_sets_atomic() {
        let atomic = AtomicBool::new(false);
        apply_tier(HardwareTier::Moderate, Override::None, &atomic).await;
        assert!(atomic.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn apply_tier_slow_no_override_sets_atomic() {
        // decoder-speed-overhaul Task 14: the old Slow-tier `Ft8Config`
        // rewrite (max_decode_passes=1, max_sync_candidates=150) is
        // retired — `apply_tier` now only flips `scoped_fast_path` for the
        // Slow tier; the `[decoder]` effort-preset system owns decode
        // thoroughness via `decode_effort_budget_ms` instead (see
        // `effort.rs`).
        let atomic = AtomicBool::new(false);
        apply_tier(HardwareTier::Slow, Override::None, &atomic).await;
        assert!(atomic.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn apply_tier_slow_with_force_off_clears_atomic() {
        let atomic = AtomicBool::new(true); // pre-set to verify it gets cleared
        apply_tier(HardwareTier::Slow, Override::ForceOff, &atomic).await;
        assert!(!atomic.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn apply_tier_fast_with_force_on_sets_atomic() {
        let atomic = AtomicBool::new(false);
        apply_tier(HardwareTier::Fast, Override::ForceOn, &atomic).await;
        assert!(atomic.load(Ordering::Acquire));
    }

    #[test]
    fn current_hardware_key_returns_nonempty() {
        let (cpu, cores) = current_hardware_key();
        assert!(!cpu.is_empty());
        assert!(cores >= 1);
    }

    // ------------------------------------------------------------------
    // final-review Finding 1: tier-probe completion must not clobber a
    // live operator decode-effort choice.
    // ------------------------------------------------------------------

    #[test]
    fn probe_completion_reseeds_when_live_effort_is_auto() {
        // The default/untouched state: `Auto`, and the whole point of a
        // tier-probe re-seed is to resolve `Auto` against the now-known
        // tier. This must stay true, or the tier probe becomes inert.
        assert!(probe_completion_should_reseed_budget(DecodeEffort::Auto));
    }

    #[test]
    fn probe_completion_never_reseeds_when_operator_cycled_away_from_auto() {
        // Simulates the operator pressing `e` during the brief
        // cache-miss probe window: `current_decode_effort` now holds a
        // non-Auto preset. The probe completing must NOT silently
        // revert it — this is the exact bug described in Finding 1.
        for cycled in [
            DecodeEffort::Eco,
            DecodeEffort::Standard,
            DecodeEffort::Deep,
            DecodeEffort::Max,
        ] {
            assert!(
                !probe_completion_should_reseed_budget(cycled),
                "probe completion must not re-seed the budget atomic when the \
                 operator has cycled to {cycled:?} — doing so would silently \
                 revert their live choice (final-review Finding 1)"
            );
        }
    }

    // Note: `tier::initialize`/`spawn_probe_worker` themselves are not
    // exercised end-to-end here — `initialize` reads/writes the real
    // `~/.pancetta/tier_cache.json` (via `default_cache_path`) and
    // `spawn_probe_worker` runs the genuine `probe_hardware_tier` hardware
    // timing probe on a `spawn_blocking` thread, so driving them from a
    // unit test would have real side effects on the developer's machine
    // and non-deterministic timing — the existing test suite already
    // avoids calling `initialize` directly for the same reason. The
    // plumbing was verified by code inspection instead: `initialize` now
    // takes `current_decode_effort: Arc<AtomicU8>` and forwards it
    // unchanged into `spawn_probe_worker` (see the `spawn_probe_worker(...)`
    // call at the end of `initialize`), which reads it live at re-seed
    // time via `probe_completion_should_reseed_budget`, tested above.
}
