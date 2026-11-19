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
//!    tier directly or spawns a background probe. PAN-369: before a due
//!    probe it reads the 1-minute load average; above the logical core
//!    count it defers measurement to a later startup and applies the
//!    previous same-host tier, or Moderate (Auto at the Standard 250 ms
//!    budget) when there is none, without touching the cache.
//! 2. The atomic is handed to the FT8 hot loop; reading it is a single
//!    relaxed load per window iteration.
//! 3. `initialize` and the background probe worker both also re-publish
//!    the decode budget (decoder-speed-overhaul Task 14, see `effort.rs`)
//!    into the coordinator's `DecodeWindowConfig` once the tier is known — this **replaces** the old
//!    Slow-tier `Ft8Config` numeric-field rewrite (`max_decode_passes=1` +
//!    `max_sync_candidates=150`) that used to live in [`apply_tier`]: the
//!    `[decoder]` effort-preset system now owns tuning decode thoroughness
//!    for the Slow tier via the published budget instead of mutating
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
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use pancetta_config::DecodeEffort;
use pancetta_ft8::tier_probe::{recommend_actions, HardwareTier};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::effort::{seed_effort_budget, DecodeWindowConfig};

/// PAN-369: schema 2 marks a busy-window measurement. Schema 1 (the
/// synthetic-CQ probe) is read only as a same-host fallback tier for a
/// deferred startup and is replaced by the next successful probe.
const CACHE_SCHEMA_VERSION: u32 = 2;
const LEGACY_CACHE_SCHEMA_VERSION: u32 = 1;
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

/// Default cache path: `<data-dir>/tier_cache.json` (PAN-376: `--data-dir`,
/// else `~/.pancetta`). Returns None if no data directory can be located.
pub(crate) fn default_cache_path() -> Option<PathBuf> {
    pancetta_core::data_dir::try_current().map(|d| d.join("tier_cache.json"))
}

/// Read and parse the cache file once. Returns `None` on any error:
/// missing file, malformed JSON, or a schema this build does not know
/// (anything but the current or legacy schema). All non-trivial failures
/// log at `debug!`; [`evaluate_cache`] decides what a parsed entry is
/// worth.
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
    if cache.schema_version != CACHE_SCHEMA_VERSION
        && cache.schema_version != LEGACY_CACHE_SCHEMA_VERSION
    {
        debug!(
            "tier cache: unknown schema version {} (expected {} or legacy {}); ignoring",
            cache.schema_version, CACHE_SCHEMA_VERSION, LEGACY_CACHE_SCHEMA_VERSION
        );
        return None;
    }
    Some(cache)
}

/// What the on-disk cache is worth at startup (PAN-369).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheVerdict {
    /// A current busy-window measurement for this host and build: apply it,
    /// no measurement is due.
    Current(HardwareTier),
    /// A legacy or version-stale entry for this same CPU and core count: a
    /// measurement is due, and this tier stands in only if it is deferred.
    Fallback(HardwareTier),
    /// No usable entry (missing, malformed, unknown schema or tier, other
    /// host): a measurement is due with no previous tier.
    Miss,
}

/// Classify a parsed cache entry against this host and build.
pub(crate) fn evaluate_cache(
    cache: Option<&TierCache>,
    cpu_model: &str,
    core_count: usize,
    pancetta_version: &str,
) -> CacheVerdict {
    let Some(cache) = cache else {
        return CacheVerdict::Miss;
    };
    if cache.cpu_model != cpu_model || cache.core_count != core_count {
        return CacheVerdict::Miss;
    }
    let Some(tier) = cache.parse_tier() else {
        debug!("tier cache: unknown tier string '{}'", cache.tier);
        return CacheVerdict::Miss;
    };
    match cache.schema_version {
        CACHE_SCHEMA_VERSION if cache.pancetta_version == pancetta_version => {
            CacheVerdict::Current(tier)
        }
        CACHE_SCHEMA_VERSION | LEGACY_CACHE_SCHEMA_VERSION => CacheVerdict::Fallback(tier),
        _ => CacheVerdict::Miss,
    }
}

/// Parse the 1-minute field of a `/proc/loadavg` line. `None` for a
/// missing, malformed, negative or non-finite value.
pub(crate) fn parse_loadavg(contents: &str) -> Option<f64> {
    let load: f64 = contents.split_whitespace().next()?.parse().ok()?;
    valid_load(load)
}

fn valid_load(load: f64) -> Option<f64> {
    (load.is_finite() && load >= 0.0).then_some(load)
}

/// The host's 1-minute load average from `/proc/loadavg`, or `None` when
/// the reading fails. In a container this is the host-wide load, compared
/// against the container's logical cores.
#[cfg(target_os = "linux")]
pub(crate) fn one_minute_load() -> Option<f64> {
    parse_loadavg(&std::fs::read_to_string("/proc/loadavg").ok()?)
}

/// The host's 1-minute load average from `getloadavg(3)`, or `None` when
/// the call fails.
#[cfg(target_os = "macos")]
pub(crate) fn one_minute_load() -> Option<f64> {
    let mut loads = [0.0f64; 1];
    // SAFETY: `loads` is a valid, writable buffer of exactly one f64, and
    // `getloadavg` writes at most `nelem` (1) entries into it.
    let written = unsafe { libc::getloadavg(loads.as_mut_ptr(), 1) };
    if written == 1 {
        valid_load(loads[0])
    } else {
        None
    }
}

/// Windows and other platforms have no 1-minute load average: always
/// unavailable, so the probe runs without a load check.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn one_minute_load() -> Option<f64> {
    None
}

/// The load check that gates a due probe (PAN-369).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum LoadCheck {
    /// Load at or below the core count: measure.
    Proceed,
    /// No reading: measure anyway.
    Unavailable,
    /// Load strictly above the core count: defer measurement.
    Defer { load: f64, cores: usize },
}

/// Compare a 1-minute load reading with the logical core count. Only a load
/// strictly above the core count defers; equal load still measures.
pub(crate) fn check_load(load: Option<f64>, cores: usize) -> LoadCheck {
    match load {
        None => LoadCheck::Unavailable,
        Some(load) if load > cores as f64 => LoadCheck::Defer { load, cores },
        Some(_) => LoadCheck::Proceed,
    }
}

/// The station-log line for a deferred probe.
pub(crate) fn deferral_message(load: f64, cores: usize, previous: Option<HardwareTier>) -> String {
    let using = match previous {
        Some(tier) => format!("previous tier {}", tier.as_str()),
        None => "Standard (no previous tier)".to_string(),
    };
    format!(
        "tier probe deferred: 1-minute load {load:.2} exceeds {cores} logical cores; using {using}"
    )
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
/// published decode budget instead. This function no longer
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

/// Whether a tier-probe completion should re-seed the decode budget
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
/// Also re-publishes the decode budget (decoder-speed-overhaul Task 14)
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
/// Everything the background probe worker needs (PAN-369: one value so the
/// startup decision can hand it to an injected launcher in tests).
pub(crate) struct ProbeJob {
    pub cpu_model: String,
    pub core_count: usize,
    pub pancetta_version: String,
    pub cache_path: Option<PathBuf>,
    pub override_: Override,
    pub scoped_fast_path: Arc<AtomicBool>,
    pub budget_override: Option<u64>,
    pub current_decode_effort: Arc<AtomicU8>,
    pub resolved_hardware_tier: Arc<AtomicU8>,
    pub decode_window: Arc<tokio::sync::RwLock<DecodeWindowConfig>>,
}

fn spawn_probe_worker(job: ProbeJob) {
    let ProbeJob {
        cpu_model,
        core_count,
        pancetta_version,
        cache_path,
        override_,
        scoped_fast_path,
        budget_override,
        current_decode_effort,
        resolved_hardware_tier,
        decode_window,
    } = job;
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

        // PAN-156 rounds 1-5 review findings, all versions of the same root
        // cause: this used to be two DIFFERENT critical sections (one per
        // if/else branch, gated on an outer `live_effort` snapshot), each
        // trusting that snapshot instead of re-reading fresh state under
        // the lock. Collapsed into a single, UNCONDITIONAL critical
        // section below that always re-reads `current_decode_effort`
        // fresh AFTER acquiring the config lock.
        //
        // PAN-156 round-8 review finding: collapsing the CONFIG decision
        // wasn't enough on its own — the BUDGET reseed (`seed_effort_
        // budget`) was still gated on the stale outer `live_effort`
        // snapshot from `probe_completion_should_reseed_budget`, so an
        // operator cycling Max→Auto in the gap between that snapshot and
        // the lock being acquired could get the fresh tier's Ft8Config
        // override paired with a budget computed for a DIFFERENT
        // (earlier) tier that was never reseeded — e.g. Auto+Slow's
        // overrides with the Fast-tier 1000ms budget instead of 1ms.
        // Reseed the budget from the SAME fresh `live_effort_at_write` /
        // `result.tier` pair used for the config override, in this same
        // critical section, so budget and config can never disagree
        // about which effort/tier they're for.
        //
        // PAN-164: the budget and the overrides derived from it publish as
        // one `DecodeWindowConfig` change under this one write guard, and
        // the FT8 hot loop reads both with one `try_read()`, so no write
        // ordering inside this section matters any more.
        tokio::runtime::Handle::current().block_on(async {
            let mut window_guard = decode_window.write().await;
            let live_effort_at_write =
                DecodeEffort::from_u8(current_decode_effort.load(Ordering::Acquire));
            if live_effort_at_write == DecodeEffort::Auto {
                seed_effort_budget(
                    DecodeEffort::Auto,
                    budget_override,
                    result.tier,
                    &mut window_guard,
                );
                info!(
                    "tier probe: decode budget re-published for {} tier (Auto)",
                    result.tier.as_str()
                );
            } else {
                debug!(
                    "tier probe: skipped Ft8Config override re-application and budget \
                     reseed — operator cycled to {live_effort_at_write:?} while the \
                     config lock was being acquired; tier is still recorded for a \
                     future Auto cycle"
                );
            }
            resolved_hardware_tier.store(result.tier.as_u8(), Ordering::Release);
        });
    });
}

/// Coordinator-startup entry point.
///
/// Reads the override env var, checks the on-disk cache, and either
/// applies the cached tier synchronously or schedules a background
/// probe. Returns the `Arc<AtomicBool>` that the FT8 hot loop reads.
///
/// Also re-publishes the decode budget (decoder-speed-overhaul Task 14)
/// once the tier is known — on the synchronous cache-hit path here, or
/// asynchronously from the background probe worker (see
/// [`spawn_probe_worker`]). The caller is expected to have already seeded
/// `decode_window` with an assumed tier before calling this (see
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
pub(crate) async fn initialize(
    effort: DecodeEffort,
    budget_override: Option<u64>,
    current_decode_effort: Arc<AtomicU8>,
    resolved_hardware_tier: Arc<AtomicU8>,
    decode_window: Arc<tokio::sync::RwLock<DecodeWindowConfig>>,
) -> Arc<AtomicBool> {
    let env_value = std::env::var(ENV_OVERRIDE).ok();
    let override_ = parse_override(env_value.as_deref());
    info!("tier: env override = {}", override_.as_str());

    let (cpu_model, core_count) = current_hardware_key();
    let host = StartupHost {
        cpu_model,
        core_count,
        pancetta_version: env!("CARGO_PKG_VERSION").to_string(),
        cache_path: default_cache_path(),
        override_,
    };
    initialize_with(
        host,
        StartupState {
            effort,
            budget_override,
            current_decode_effort,
            resolved_hardware_tier,
            decode_window,
        },
        one_minute_load,
        spawn_probe_worker,
    )
    .await
}

/// Host and build identity, cache location and env override that
/// [`initialize`] reads from the real machine (PAN-369: injected in tests).
pub(crate) struct StartupHost {
    pub cpu_model: String,
    pub core_count: usize,
    pub pancetta_version: String,
    pub cache_path: Option<PathBuf>,
    pub override_: Override,
}

/// The coordinator state a startup tier decision publishes into.
pub(crate) struct StartupState {
    pub effort: DecodeEffort,
    pub budget_override: Option<u64>,
    pub current_decode_effort: Arc<AtomicU8>,
    pub resolved_hardware_tier: Arc<AtomicU8>,
    pub decode_window: Arc<tokio::sync::RwLock<DecodeWindowConfig>>,
}

/// The startup tier decision behind [`initialize`], with the load reading
/// and the probe launcher injected (PAN-369).
///
/// A current cache entry applies directly; `read_load` is not called. When
/// a measurement is due, `read_load` runs once: a 1-minute load strictly
/// above `host.core_count` defers the probe (nothing launched, the cache
/// untouched) and applies the previous same-host tier from a legacy or
/// version-stale entry, else Moderate, the tier at which Auto uses the
/// Standard 250 ms budget. Otherwise `launch_probe` receives the job.
pub(crate) async fn initialize_with(
    host: StartupHost,
    state: StartupState,
    read_load: impl FnOnce() -> Option<f64>,
    launch_probe: impl FnOnce(ProbeJob),
) -> Arc<AtomicBool> {
    let scoped_fast_path = Arc::new(AtomicBool::new(false));

    // Seed the atomic from env immediately. The probe (if any) will
    // re-apply after measurement, but only if override == None.
    if host.override_ == Override::ForceOn {
        scoped_fast_path.store(true, Ordering::Release);
    }

    let cached: Option<TierCache> = host.cache_path.as_deref().and_then(load_cache);
    let verdict = evaluate_cache(
        cached.as_ref(),
        &host.cpu_model,
        host.core_count,
        &host.pancetta_version,
    );

    let previous = match verdict {
        CacheVerdict::Current(tier) => {
            let summary = apply_startup_tier(tier, host.override_, &scoped_fast_path, &state).await;
            info!(
                "tier: cache hit (cpu='{}', cores={}, v{}) → {} — {}",
                host.cpu_model,
                host.core_count,
                host.pancetta_version,
                tier.as_str(),
                summary
            );
            return scoped_fast_path;
        }
        CacheVerdict::Fallback(tier) => {
            info!(
                "tier: cache stale (earlier probe schema or version changed; previous tier {}), measurement due",
                tier.as_str()
            );
            Some(tier)
        }
        CacheVerdict::Miss if cached.is_some() => {
            info!("tier: cache stale (host changed or entry unusable), measurement due");
            None
        }
        CacheVerdict::Miss => {
            info!("tier: no cache, measurement due");
            None
        }
    };

    match check_load(read_load(), host.core_count) {
        LoadCheck::Defer { load, cores } => {
            warn!("{}", deferral_message(load, cores, previous));
            let tier = previous.unwrap_or(HardwareTier::Moderate);
            let summary = apply_startup_tier(tier, host.override_, &scoped_fast_path, &state).await;
            info!(
                "tier: deferred startup applied {} — {}",
                tier.as_str(),
                summary
            );
            return scoped_fast_path;
        }
        LoadCheck::Unavailable => {
            info!("tier probe: 1-minute load unavailable; proceeding without load check");
        }
        LoadCheck::Proceed => {}
    }

    info!("tier: scheduling background probe");
    launch_probe(ProbeJob {
        cpu_model: host.cpu_model,
        core_count: host.core_count,
        pancetta_version: host.pancetta_version,
        cache_path: host.cache_path,
        override_: host.override_,
        scoped_fast_path: scoped_fast_path.clone(),
        budget_override: state.budget_override,
        current_decode_effort: state.current_decode_effort,
        resolved_hardware_tier: state.resolved_hardware_tier,
        decode_window: state.decode_window,
    });

    scoped_fast_path
}

/// Publish a startup tier (cache hit or deferred probe): flip the scoped
/// fast path, seed the decode window from the startup effort and budget
/// override, and record the tier for a later live `Auto` cycle. Runs before
/// the TUI accepts input, so the startup effort is the live one.
async fn apply_startup_tier(
    tier: HardwareTier,
    override_: Override,
    scoped_fast_path: &AtomicBool,
    state: &StartupState,
) -> String {
    let summary = apply_tier(tier, override_, scoped_fast_path).await;
    {
        let mut window_guard = state.decode_window.write().await;
        seed_effort_budget(state.effort, state.budget_override, tier, &mut window_guard);
    }
    state
        .resolved_hardware_tier
        .store(tier.as_u8(), Ordering::Release);
    summary
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
        // thoroughness via the published decode budget instead (see
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
                "probe completion must not re-seed the published budget when the \
                 operator has cycled to {cycled:?} — doing so would silently \
                 revert their live choice (final-review Finding 1)"
            );
        }
    }

    // PAN-369: the startup decision itself is exercised through
    // `initialize_with` in `tests/load_guard_tests.rs` with an injected
    // load reading, temporary cache path and counted probe launcher.
    //
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

#[cfg(test)]
#[path = "tier/tests/load_guard_tests.rs"]
mod load_guard_tests;
