//! PAN-369 AC3-AC5: the startup tier decision defers a due probe on a
//! loaded host, falls back to the previous same-host tier or Standard, and
//! never measures when a current cache entry exists.
//!
//! Every case drives `initialize_with` (the code `tier::initialize` runs)
//! with a temporary cache path, an injected load reading and a counted
//! probe launcher, so the test machine's own load never matters.

use super::*;
use crate::coordinator::effort::{cycle_decode_effort, seed_effort_budget, DecodeWindowConfig};
use pancetta_ft8::Ft8Config;
use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;
use tempfile::TempDir;

const CPU: &str = "Test CPU 9000";
const CORES: usize = 2;
const VERSION: &str = "9.9.9";

/// Captures formatted log output for the duration of one startup.
#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = LogBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

struct Startup {
    effort: DecodeEffort,
    budget_override: Option<u64>,
    override_: Override,
    load: Option<f64>,
}

impl Startup {
    fn auto(load: Option<f64>) -> Self {
        Self {
            effort: DecodeEffort::Auto,
            budget_override: None,
            override_: Override::None,
            load,
        }
    }
}

struct Outcome {
    scoped_fast_path: bool,
    resolved: HardwareTier,
    window: DecodeWindowConfig,
    current_decode_effort: Arc<AtomicU8>,
    probes: usize,
    load_reads: usize,
    probe_cache_path: Option<PathBuf>,
    logs: String,
}

async fn run_startup(cache_path: &Path, startup: Startup) -> Outcome {
    let logs = LogBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Seeded the way `coordinator/mod.rs` seeds it before `initialize`.
    let mut window = DecodeWindowConfig::new(Ft8Config::default(), 0);
    seed_effort_budget(
        startup.effort,
        startup.budget_override,
        HardwareTier::Fast,
        &mut window,
    );
    let decode_window = Arc::new(tokio::sync::RwLock::new(window));
    let current_decode_effort = Arc::new(AtomicU8::new(startup.effort.as_u8()));
    let resolved_hardware_tier = Arc::new(AtomicU8::new(HardwareTier::Fast.as_u8()));

    let load_reads = AtomicUsize::new(0);
    let probes = AtomicUsize::new(0);
    let probe_cache_path = Mutex::new(None);
    let load = startup.load;

    let scoped = initialize_with(
        StartupHost {
            cpu_model: CPU.to_string(),
            core_count: CORES,
            pancetta_version: VERSION.to_string(),
            cache_path: Some(cache_path.to_path_buf()),
            override_: startup.override_,
        },
        StartupState {
            effort: startup.effort,
            budget_override: startup.budget_override,
            current_decode_effort: current_decode_effort.clone(),
            resolved_hardware_tier: resolved_hardware_tier.clone(),
            decode_window: decode_window.clone(),
        },
        || {
            load_reads.fetch_add(1, Ordering::SeqCst);
            load
        },
        |job| {
            probes.fetch_add(1, Ordering::SeqCst);
            *probe_cache_path.lock().unwrap() = job.cache_path;
        },
    )
    .await;

    let window = decode_window.read().await.clone();
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    Outcome {
        scoped_fast_path: scoped.load(Ordering::Acquire),
        resolved: HardwareTier::from_u8(resolved_hardware_tier.load(Ordering::Acquire)),
        window,
        current_decode_effort,
        probes: probes.load(Ordering::SeqCst),
        load_reads: load_reads.load(Ordering::SeqCst),
        probe_cache_path: probe_cache_path.into_inner().unwrap(),
        logs,
    }
}

fn cache_json(schema: u32, cpu: &str, cores: usize, version: &str, tier: &str) -> String {
    serde_json::json!({
        "schema_version": schema,
        "cpu_model": cpu,
        "core_count": cores,
        "pancetta_version": version,
        "tier": tier,
        "p50_ms": 300,
        "p95_ms": 320,
        "p99_ms": 330,
        "probed_at": "2026-11-18T00:00:00Z",
    })
    .to_string()
}

fn temp_cache(contents: Option<&str>) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("tier_cache.json");
    if let Some(contents) = contents {
        std::fs::write(&path, contents).unwrap();
    }
    (tmp, path)
}

/// The decode window a startup with this effort, override and tier must
/// publish.
fn expected_window(
    effort: DecodeEffort,
    budget_override: Option<u64>,
    tier: HardwareTier,
) -> String {
    let mut window = DecodeWindowConfig::new(Ft8Config::default(), 0);
    seed_effort_budget(effort, budget_override, tier, &mut window);
    format!("{:?}", window.ft8())
}

/// The operator pressing `e` from Max back to Auto, as the TUI relay does:
/// resolved against the tier atomic, ignoring any config override.
fn return_to_auto(outcome: &mut Outcome) -> (DecodeEffort, u64) {
    outcome
        .current_decode_effort
        .store(DecodeEffort::Max.as_u8(), Ordering::Release);
    cycle_decode_effort(
        &outcome.current_decode_effort,
        &mut outcome.window,
        outcome.resolved,
    )
}

// ---------------------------------------------------------------- AC3

#[tokio::test]
async fn loaded_startup_defers_due_probe() {
    for load in [3.25, 2.01] {
        let (_tmp, path) = temp_cache(None);
        let outcome = run_startup(&path, Startup::auto(Some(load))).await;
        assert_eq!(
            outcome.probes, 0,
            "load {load} above {CORES} cores must not probe"
        );
        assert_eq!(outcome.load_reads, 1);
        let line = format!(
            "tier probe deferred: 1-minute load {load:.2} exceeds {CORES} logical cores; \
             using Standard (no previous tier)"
        );
        assert!(
            outcome.logs.contains(&line),
            "missing `{line}` in:\n{}",
            outcome.logs
        );
    }
}

#[tokio::test]
async fn equal_load_allows_probe() {
    for load in [2.0, 1.99, 0.0] {
        let (_tmp, path) = temp_cache(None);
        let outcome = run_startup(&path, Startup::auto(Some(load))).await;
        assert_eq!(
            outcome.probes, 1,
            "load {load} at or below {CORES} cores must probe"
        );
        assert_eq!(outcome.probe_cache_path.as_deref(), Some(path.as_path()));
        assert!(
            !outcome.logs.contains("tier probe deferred"),
            "{}",
            outcome.logs
        );
        // Nothing resolved yet: the window keeps its startup seed until the
        // worker finishes.
        assert_eq!(outcome.resolved, HardwareTier::Fast);
    }
}

// ---------------------------------------------------------------- AC4

#[tokio::test]
async fn deferred_startup_reuses_same_host_tier() {
    // Legacy schema-1 entry, and a version-stale schema-2 entry: both are
    // same-host previous tiers.
    for (schema, version, tier, budget, scoped) in [
        (1, VERSION, "slow", 1, true),
        (2, "0.0.1", "fast", 1000, false),
    ] {
        let contents = cache_json(schema, CPU, CORES, version, tier);
        let (_tmp, path) = temp_cache(Some(&contents));
        let mut outcome = run_startup(&path, Startup::auto(Some(3.25))).await;
        let expected_tier = match tier {
            "slow" => HardwareTier::Slow,
            _ => HardwareTier::Fast,
        };

        assert_eq!(outcome.probes, 0);
        assert_eq!(outcome.resolved, expected_tier);
        assert_eq!(outcome.window.budget_ms(), budget);
        assert_eq!(
            format!("{:?}", outcome.window.ft8()),
            expected_window(DecodeEffort::Auto, None, expected_tier)
        );
        assert_eq!(outcome.scoped_fast_path, scoped);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            contents,
            "cache rewritten"
        );
        let line = format!(
            "tier probe deferred: 1-minute load 3.25 exceeds 2 logical cores; using previous tier {tier}"
        );
        assert!(
            outcome.logs.contains(&line),
            "missing `{line}` in:\n{}",
            outcome.logs
        );

        assert_eq!(return_to_auto(&mut outcome), (DecodeEffort::Auto, budget));
    }
}

#[tokio::test]
async fn deferred_startup_defaults_to_standard() {
    let (_tmp, path) = temp_cache(None);
    let mut outcome = run_startup(&path, Startup::auto(Some(3.25))).await;

    assert_eq!(outcome.probes, 0);
    assert_eq!(outcome.resolved, HardwareTier::Moderate);
    assert_eq!(outcome.window.budget_ms(), 250);
    assert_eq!(
        format!("{:?}", outcome.window.ft8()),
        expected_window(DecodeEffort::Standard, None, HardwareTier::Moderate),
        "Auto on a deferred startup must behave as Standard"
    );
    assert!(outcome.scoped_fast_path);
    assert!(
        !path.exists(),
        "a deferred startup must not write a measurement"
    );

    assert_eq!(return_to_auto(&mut outcome), (DecodeEffort::Auto, 250));
}

#[tokio::test]
async fn deferred_startup_respects_overrides() {
    // Config budget override applies at startup...
    let (_tmp, path) = temp_cache(None);
    let mut outcome = run_startup(
        &path,
        Startup {
            budget_override: Some(700),
            ..Startup::auto(Some(3.25))
        },
    )
    .await;
    assert_eq!(outcome.probes, 0);
    assert_eq!(outcome.window.budget_ms(), 700);
    // ...and the `e` key still takes precedence over it.
    assert_eq!(return_to_auto(&mut outcome), (DecodeEffort::Auto, 250));

    // An explicit effort keeps its own preset.
    let outcome = run_startup(
        &path,
        Startup {
            effort: DecodeEffort::Deep,
            ..Startup::auto(Some(3.25))
        },
    )
    .await;
    assert_eq!(outcome.window.budget_ms(), 1000);
    assert_eq!(outcome.resolved, HardwareTier::Moderate);

    // PANCETTA_SCOPED_FAST_PATH=0 beats the Moderate fallback.
    let outcome = run_startup(
        &path,
        Startup {
            override_: Override::ForceOff,
            ..Startup::auto(Some(3.25))
        },
    )
    .await;
    assert!(!outcome.scoped_fast_path);
    assert!(!path.exists());
}

// ---------------------------------------------------------------- AC5

#[tokio::test]
async fn current_cache_bypasses_load_and_probe() {
    let contents = cache_json(CACHE_SCHEMA_VERSION, CPU, CORES, VERSION, "fast");
    let (_tmp, path) = temp_cache(Some(&contents));
    let outcome = run_startup(&path, Startup::auto(Some(64.0))).await;

    assert_eq!(outcome.load_reads, 0, "a current cache needs no load check");
    assert_eq!(outcome.probes, 0);
    assert_eq!(outcome.resolved, HardwareTier::Fast);
    assert_eq!(outcome.window.budget_ms(), 1000);
    assert!(outcome.logs.contains("tier: cache hit"), "{}", outcome.logs);
    assert!(!outcome.logs.contains("tier probe deferred"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
}

#[tokio::test]
async fn legacy_cache_is_fallback_only() {
    // Same-host legacy and version-stale entries never count as a cache
    // hit: below the threshold they are remeasured, and the startup itself
    // leaves the old bytes alone (only a successful probe replaces them).
    for contents in [
        cache_json(LEGACY_CACHE_SCHEMA_VERSION, CPU, CORES, VERSION, "fast"),
        cache_json(CACHE_SCHEMA_VERSION, CPU, CORES, "0.0.1", "fast"),
    ] {
        let (_tmp, path) = temp_cache(Some(&contents));
        let outcome = run_startup(&path, Startup::auto(Some(1.5))).await;
        assert_eq!(outcome.load_reads, 1);
        assert_eq!(
            outcome.probes, 1,
            "stale entry must be remeasured: {contents}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
    }

    // Entries that supply no fallback: malformed, unknown future schema,
    // another host, an unknown tier. Under load they default to Standard...
    let no_fallback = [
        "not json at all".to_string(),
        cache_json(CACHE_SCHEMA_VERSION + 1, CPU, CORES, VERSION, "slow"),
        cache_json(CACHE_SCHEMA_VERSION, "Other CPU", CORES, VERSION, "slow"),
        cache_json(LEGACY_CACHE_SCHEMA_VERSION, CPU, CORES + 2, VERSION, "slow"),
        cache_json(CACHE_SCHEMA_VERSION, CPU, CORES, VERSION, "turbo"),
    ];
    for contents in &no_fallback {
        let (_tmp, path) = temp_cache(Some(contents));
        let outcome = run_startup(&path, Startup::auto(Some(3.25))).await;
        assert_eq!(outcome.probes, 0);
        assert_eq!(outcome.resolved, HardwareTier::Moderate, "{contents}");
        assert!(
            outcome.logs.contains("using Standard (no previous tier)"),
            "{contents}:\n{}",
            outcome.logs
        );
        assert_eq!(&std::fs::read_to_string(&path).unwrap(), contents);
    }
    // ...and on a valid below-threshold reading they are measured.
    for contents in &no_fallback {
        let (_tmp, path) = temp_cache(Some(contents));
        let outcome = run_startup(&path, Startup::auto(Some(0.5))).await;
        assert_eq!(outcome.probes, 1, "{contents}");
    }
}

#[tokio::test]
async fn missing_load_allows_probe() {
    let (_tmp, path) = temp_cache(None);
    let outcome = run_startup(&path, Startup::auto(None)).await;
    assert_eq!(outcome.load_reads, 1);
    assert_eq!(outcome.probes, 1);
    assert!(
        outcome
            .logs
            .contains("tier probe: 1-minute load unavailable; proceeding without load check"),
        "{}",
        outcome.logs
    );

    // Malformed readings are unavailable, never a load.
    for malformed in ["", "garbage", "-1.0 0.5 0.5", "NaN 1 1", "inf 1 1", "-inf"] {
        assert_eq!(parse_loadavg(malformed), None, "{malformed:?}");
        let (_tmp, path) = temp_cache(None);
        let outcome = run_startup(&path, Startup::auto(parse_loadavg(malformed))).await;
        assert_eq!(outcome.probes, 1, "{malformed:?}");
    }
    assert_eq!(parse_loadavg("3.25 1.10 0.50 2/345 6789\n"), Some(3.25));
    assert_eq!(check_load(Some(2.0), 2), LoadCheck::Proceed);
    assert_eq!(
        check_load(Some(2.5), 2),
        LoadCheck::Defer {
            load: 2.5,
            cores: 2
        }
    );
    assert_eq!(check_load(None, 2), LoadCheck::Unavailable);
}
