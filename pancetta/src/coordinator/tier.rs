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
//! 3. The [`Ft8Config`] handed in is wrapped in `Arc<RwLock<_>>`; the
//!    probe worker rewrites it once with the Slow-tier preset if the
//!    host classifies that way.
//!
//! ## Override matrix
//!
//! | env var | probe result | atomic final | Slow preset applied? |
//! |---------|--------------|--------------|----------------------|
//! | unset   | Fast         | false        | no                   |
//! | unset   | Moderate     | true         | no                   |
//! | unset   | Slow         | true         | yes                  |
//! | `"1"`   | (any)        | true         | no (operator chose)  |
//! | `"0"`   | (any)        | false        | no (operator chose)  |
//!
//! See `docs/superpowers/specs/2026-06-04-hb-216-s2-tier-wiring-design.md`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use pancetta_ft8::tier_probe::{recommend_actions, HardwareTier};
use pancetta_ft8::Ft8Config;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

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
/// override: `ForceOn`/`ForceOff` short-circuit both the atomic and the
/// config preset.
///
/// Returns a short human-readable description of what changed, suitable
/// for logging.
pub(crate) async fn apply_tier(
    tier: HardwareTier,
    override_: Override,
    scoped_fast_path: &AtomicBool,
    ft8_config: &RwLock<Ft8Config>,
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

    // Slow-preset rewrite, skipped on any operator override.
    let config_change = if override_ == Override::None && tier == HardwareTier::Slow {
        let mut cfg = ft8_config.write().await;
        cfg.max_decode_passes = 1;
        cfg.osd_depth = Some(1);
        " + Ft8Config slow preset (max_decode_passes=1, osd_depth=Some(1))"
    } else {
        ""
    };

    format!(
        "scoped_fast_path={} ({}){}",
        atomic_value, atomic_reason, config_change
    )
}

/// Spawn the background probe worker. Runs `probe_hardware_tier(10)`,
/// persists the cache, and applies the tier to runtime state. All
/// failures log and return — never panic.
fn spawn_probe_worker(
    cpu_model: String,
    core_count: usize,
    pancetta_version: String,
    cache_path: Option<PathBuf>,
    override_: Override,
    scoped_fast_path: Arc<AtomicBool>,
    ft8_config: Arc<RwLock<Ft8Config>>,
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
            &ft8_config,
        ));
        info!("tier probe: applied — {}", summary);
    });
}

/// Coordinator-startup entry point.
///
/// Reads the override env var, checks the on-disk cache, and either
/// applies the cached tier synchronously or schedules a background
/// probe. Returns the `Arc<AtomicBool>` that the FT8 hot loop reads.
///
/// `ft8_config` is the shared decoder config the FT8 thread reads on
/// each window iteration; the probe worker may rewrite it (Slow-tier
/// preset) before the FT8 thread sees the next window.
pub(crate) async fn initialize(ft8_config: Arc<RwLock<Ft8Config>>) -> Arc<AtomicBool> {
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
                let summary = apply_tier(tier, override_, &scoped_fast_path, &ft8_config).await;
                info!(
                    "tier: cache hit (cpu='{}', cores={}, v{}) → {} — {}",
                    cpu_model,
                    core_count,
                    pancetta_version,
                    tier.as_str(),
                    summary
                );
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
            ft8_config,
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
    async fn apply_tier_fast_no_override_leaves_config_alone() {
        let atomic = AtomicBool::new(false);
        let cfg = RwLock::new(Ft8Config::default());
        let before = cfg.read().await.osd_depth;
        apply_tier(HardwareTier::Fast, Override::None, &atomic, &cfg).await;
        assert!(!atomic.load(Ordering::Acquire));
        assert_eq!(cfg.read().await.osd_depth, before);
    }

    #[tokio::test]
    async fn apply_tier_moderate_no_override_sets_atomic_only() {
        let atomic = AtomicBool::new(false);
        let cfg = RwLock::new(Ft8Config::default());
        let before = cfg.read().await.osd_depth;
        apply_tier(HardwareTier::Moderate, Override::None, &atomic, &cfg).await;
        assert!(atomic.load(Ordering::Acquire));
        assert_eq!(cfg.read().await.osd_depth, before);
    }

    #[tokio::test]
    async fn apply_tier_slow_no_override_writes_preset() {
        let atomic = AtomicBool::new(false);
        let cfg = RwLock::new(Ft8Config::default());
        apply_tier(HardwareTier::Slow, Override::None, &atomic, &cfg).await;
        assert!(atomic.load(Ordering::Acquire));
        let c = cfg.read().await;
        assert_eq!(c.max_decode_passes, 1);
        assert_eq!(c.osd_depth, Some(1));
    }

    #[tokio::test]
    async fn apply_tier_slow_with_force_off_does_not_change_config() {
        let atomic = AtomicBool::new(true); // pre-set to verify it gets cleared
        let cfg = RwLock::new(Ft8Config::default());
        let before_osd = cfg.read().await.osd_depth;
        apply_tier(HardwareTier::Slow, Override::ForceOff, &atomic, &cfg).await;
        assert!(!atomic.load(Ordering::Acquire));
        assert_eq!(cfg.read().await.osd_depth, before_osd);
    }

    #[tokio::test]
    async fn apply_tier_fast_with_force_on_sets_atomic() {
        let atomic = AtomicBool::new(false);
        let cfg = RwLock::new(Ft8Config::default());
        let before_osd = cfg.read().await.osd_depth;
        apply_tier(HardwareTier::Fast, Override::ForceOn, &atomic, &cfg).await;
        assert!(atomic.load(Ordering::Acquire));
        assert_eq!(cfg.read().await.osd_depth, before_osd);
    }

    #[test]
    fn current_hardware_key_returns_nonempty() {
        let (cpu, cores) = current_hardware_key();
        assert!(!cpu.is_empty());
        assert!(cores >= 1);
    }
}
