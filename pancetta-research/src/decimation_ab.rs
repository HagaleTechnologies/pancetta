//! PAN-320 decimation A/B: replay identical recorded 48 kHz captures
//! through the naive subsampler and the unity-gain 31-tap FIR, and report
//! merged decode counts plus SNR distributions for both arms.
//!
//! Local research tooling only — `pancetta-research` is excluded from CI by
//! construction. The binary (`src/bin/decimation_ab.rs`) is the only entry
//! point operators use:
//!
//! ```text
//! cargo run --release -p pancetta-research --bin decimation-ab \
//!   -- --manifest /data/pan320/captures.json --output /tmp/pan320-result.json
//! ```
//!
//! Guarantees this module implements (see `tests/decimation_ab.rs`):
//!
//! * Both arms consume the *same* validated channel-0 raw48 samples —
//!   channel 0 alone feeds both arms, matching the live DSP (which
//!   extracts the left channel); channels are never averaged.
//! * Fresh, identically-configured `Ft8Decoder` state per arm per capture;
//!   no cross-file soft-combining state.
//! * The same FT8 prefix is decoded from each arm's 15 s tile (13.5 s for
//!   lead 0.5, 14 s for lead 1.0); the FIR arm's reported per-message
//!   timing is compensated for its causal 15-input-sample group delay.
//! * The strict count decision: `fir31` becomes the default **iff** its
//!   total merged decode count is strictly greater. A tie keeps `subsample`
//!   regardless of SNR. Missing inputs keep `subsample` with a pending
//!   status.
//! * Malformed or incomplete corpora are refused before either arm
//! decodes anything; the output report is written atomically only after a
//! successful run, and a failed run never creates or replaces it.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

pub use pancetta_dsp::decimation::DecimationFilter;

/// Manifest schema version this tool consumes.
pub const MANIFEST_VERSION: u32 = 1;

/// Exactly 15 s at 48 kHz — one raw48 tile. Anything else is invalid,
/// including a truncated tail (an incomplete corpus is not a zero-decode
/// experiment).
pub const CAPTURE_FRAMES: usize = 720_000;

pub const CAPTURE_RATE_HZ: u32 = 48_000;

/// Station capture conditions the ticket requires (at least one file per
/// condition). The condition label comes from the station-supplied
/// manifest; a `_tx` filename suffix is provenance, not proof.
pub const CONDITIONS: [&str; 3] = ["quiet", "busy", "tx_desense"];

/// Lead-in before the slot boundary, in seconds, that a capture may carry.
pub const ALLOWED_LEAD_SECONDS: [f64; 2] = [0.5, 1.0];

/// The decode prefix taken from each arm's 15 s tile: lead + 13 s (the
/// production decode phase past the slot start).
pub fn decode_prefix_samples(lead_seconds: f64) -> usize {
    ((lead_seconds + 13.0) * 12_000.0) as usize
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaptureEntry {
    /// Path, resolved relative to the manifest's directory.
    pub path: String,
    /// SHA-256 of the WAV file, 64 lowercase hex digits.
    pub sha256: String,
    /// Station-supplied condition: one of [`CONDITIONS`].
    pub condition: String,
    /// Lead-in before the slot boundary: 0.5 or 1.0 seconds.
    pub lead_seconds: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaptureManifest {
    pub version: u32,
    pub captures: Vec<CaptureEntry>,
}

/// One loaded, hash-verified capture: channel 0 only, f32 in [−1, 1).
#[derive(Debug, Clone)]
pub struct LoadedCapture {
    pub entry: CaptureEntry,
    pub channel_count: u16,
    pub channel0: Vec<f32>,
}

/// Parse and structurally validate a manifest (no file I/O).
pub fn validate_manifest(manifest: &CaptureManifest) -> Result<()> {
    if manifest.version != MANIFEST_VERSION {
        bail!(
            "unsupported manifest version {} (expected {MANIFEST_VERSION})",
            manifest.version
        );
    }
    if manifest.captures.is_empty() {
        bail!("manifest lists no captures");
    }
    for cond in CONDITIONS {
        if !manifest.captures.iter().any(|c| c.condition == cond) {
            bail!("manifest has no capture for condition '{cond}'");
        }
    }
    let mut seen_paths = std::collections::BTreeSet::new();
    let mut seen_hashes = std::collections::BTreeSet::new();
    for entry in &manifest.captures {
        if entry.path.trim().is_empty() {
            bail!("capture entry with an empty path");
        }
        if !seen_paths.insert(entry.path.clone()) {
            bail!("duplicate capture path {}", entry.path);
        }
        if entry.sha256.len() != 64 || !entry.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("capture {} has a malformed sha256", entry.path);
        }
        if !seen_hashes.insert(entry.sha256.to_ascii_lowercase()) {
            bail!(
                "duplicate capture hash for {} (two entries point at one file)",
                entry.path
            );
        }
        if !CONDITIONS.contains(&entry.condition.as_str()) {
            bail!(
                "capture {} has unknown condition '{}' (expected one of {CONDITIONS:?})",
                entry.path,
                entry.condition
            );
        }
        if !ALLOWED_LEAD_SECONDS.contains(&entry.lead_seconds) {
            bail!(
                "capture {} has lead_seconds {} (expected one of {ALLOWED_LEAD_SECONDS:?})",
                entry.path,
                entry.lead_seconds
            );
        }
    }
    Ok(())
}

/// Read one capture WAV: 48 kHz PCM16, positive channel count, exactly
/// [`CAPTURE_FRAMES`] frames, SHA-256 matching the manifest. Channel 0
/// alone is returned — the same channel the live DSP extracts.
pub fn load_capture(manifest_dir: &Path, entry: &CaptureEntry) -> Result<LoadedCapture> {
    let path = manifest_dir.join(&entry.path);
    let bytes = std::fs::read(&path).with_context(|| format!("cannot read {}", entry.path))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hash = format!("{:x}", hasher.finalize());
    if hash != entry.sha256.to_ascii_lowercase() {
        bail!(
            "{}: sha256 mismatch (manifest {}, file {})",
            entry.path,
            entry.sha256,
            hash
        );
    }
    let reader = hound::WavReader::new(std::io::Cursor::new(&bytes))
        .with_context(|| format!("{}: cannot parse WAV", entry.path))?;
    let spec = reader.spec();
    if spec.sample_rate != CAPTURE_RATE_HZ {
        bail!(
            "{}: sample rate {} Hz, expected {CAPTURE_RATE_HZ} Hz",
            entry.path,
            spec.sample_rate
        );
    }
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        bail!(
            "{}: format {:?}/{}-bit, expected PCM16",
            entry.path,
            spec.sample_format,
            spec.bits_per_sample
        );
    }
    if spec.channels == 0 {
        bail!("{}: zero channels", entry.path);
    }
    let channel_count = spec.channels;
    let samples: Vec<i16> = reader
        .into_samples::<i16>()
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("{}: truncated PCM data", entry.path))?;
    let frames = samples.len() / channel_count as usize;
    if frames != CAPTURE_FRAMES {
        bail!(
            "{}: {frames} frames, expected exactly {CAPTURE_FRAMES} (15 s at 48 kHz)",
            entry.path
        );
    }
    // Channel 0 only. Same scale for both arms; never resample or
    // normalize per arm; never average channels.
    let channel0: Vec<f32> = samples
        .chunks(channel_count as usize)
        .map(|frame| frame[0] as f32 / 32768.0)
        .collect();
    Ok(LoadedCapture {
        entry: entry.clone(),
        channel_count,
        channel0,
    })
}

// ---------------------------------------------------------------------------
// Decimation arms
// ---------------------------------------------------------------------------

/// The naive production subsampler: counter starts at 0, every 4th sample
/// retained (indices 3, 7, 11, …) — byte-for-byte the coordinator's
/// default loop.
pub fn subsample_arm(input: &[f32]) -> Vec<f32> {
    let mut out = Vec::with_capacity(input.len() / 4);
    let mut counter = 0usize;
    for &s in input {
        counter += 1;
        if counter >= 4 {
            counter = 0;
            out.push(s);
        }
    }
    out
}

/// The opt-in FIR arm: `pancetta-dsp`'s 31-tap unity-gain decimator, fresh
/// zero-initialized history (the documented convention; the first-sample
/// transient is part of the contract).
pub fn fir31_arm(input: &[f32]) -> Vec<f32> {
    let mut fir = DecimationFilter::new();
    let mut out = Vec::with_capacity(input.len() / 4);
    fir.process(input, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

/// One decode record, arm-independent. `frequency_hz` is the deterministic
/// secondary tie break when duplicate texts carry equal SNR.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DecodeRecord {
    pub text: String,
    pub snr_db: f32,
    pub frequency_hz: f64,
    pub time_offset_s: f64,
}

/// Nearest-rank quantile: the p-th percentile of `values` (sorted) is the
/// element at index `ceil(p/100 * n) - 1` (nearest-rank method; n=0 → None).
pub fn nearest_rank_quantile(sorted: &[f32], p: f64) -> Option<f32> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    let rank = rank.clamp(1, sorted.len());
    Some(sorted[rank - 1])
}

/// SNR distribution over finite values, nearest-rank quantiles. Nonfinite
/// values are counted separately and excluded from the quantiles only.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SnrDistribution {
    pub n: usize,
    pub nonfinite: usize,
    pub min: Option<f64>,
    pub p10: Option<f64>,
    pub median: Option<f64>,
    pub p90: Option<f64>,
    pub max: Option<f64>,
}

impl SnrDistribution {
    pub fn from_snrs(snrs: &[f32]) -> Self {
        let finite: Vec<f32> = snrs.iter().copied().filter(|s| s.is_finite()).collect();
        let mut sorted = finite.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        Self {
            n: finite.len(),
            nonfinite: snrs.len() - finite.len(),
            min: sorted.first().map(|&v| v as f64),
            p10: nearest_rank_quantile(&sorted, 10.0).map(|v| v as f64),
            median: nearest_rank_quantile(&sorted, 50.0).map(|v| v as f64),
            p90: nearest_rank_quantile(&sorted, 90.0).map(|v| v as f64),
            max: sorted.last().map(|&v| v as f64),
        }
    }
}

/// Deduplicate decode records by message text within one arm, keeping the
/// highest-SNR decode; frequency is the deterministic secondary tie break.
/// (The decoder's own within-window merge already ran — this is the
/// per-capture, per-arm uniqueness the merged-decode count is defined on.)
pub fn dedupe_by_text(records: Vec<DecodeRecord>) -> Vec<DecodeRecord> {
    let mut best: BTreeMap<String, DecodeRecord> = BTreeMap::new();
    for rec in records {
        let replace = match best.get(&rec.text) {
            None => true,
            Some(cur) => {
                // Strictly higher SNR wins; equal SNR → the higher
                // frequency wins (deterministic tie break).
                rec.snr_db > cur.snr_db
                    || (rec.snr_db == cur.snr_db && rec.frequency_hz > cur.frequency_hz)
            }
        };
        if replace {
            best.insert(rec.text.clone(), rec);
        }
    }
    best.into_values().collect()
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

pub const REPORT_SCHEMA_VERSION: u32 = 1;
pub const STATUS_COMPLETE: &str = "complete";
pub const STATUS_PENDING: &str = "pending_station_captures";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArmResult {
    pub merged_decodes: usize,
    pub snr: SnrDistribution,
    /// Unique decode texts, kept for audit (sorted).
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaptureComparison {
    pub path: String,
    pub sha256: String,
    pub condition: String,
    pub lead_seconds: f64,
    pub subsample: ArmResult,
    pub fir31: ArmResult,
    /// Texts decoded only by the FIR arm (survivor-bias guard).
    pub fir_only: Vec<String>,
    /// Texts decoded only by the subsample arm.
    pub subsample_only: Vec<String>,
    /// FIR minus subsample snr_db for texts present in both arms.
    pub paired_snr_delta: Option<f64>,
    pub paired_n: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Decision {
    pub selected_default: String,
    pub subsample_total: Option<u64>,
    pub fir31_total: Option<u64>,
    pub decode_delta: Option<i64>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecimationAbReport {
    pub schema_version: u32,
    pub status: String,
    /// True when the corpus was synthesized by tests rather than recorded
    /// by the station; synthetic totals never count toward station
    /// qualification.
    pub synthetic: bool,
    pub decision: Decision,
    pub manifest: Option<CaptureManifest>,
    pub decoder_settings: serde_json::Value,
    pub filter_coefficients: Vec<f64>,
    pub decimation: serde_json::Value,
    pub captures: Vec<CaptureComparison>,
    /// The strict count decision applied to the totals above. Pure
    /// function of (subsample_total, fir31_total): fir31 iff strictly
    /// greater; ties and losses keep subsample; missing evidence keeps
    /// subsample with a pending status.
    pub per_condition: BTreeMap<String, serde_json::Value>,
}

/// The strict count decision (PAN-320): the FIR becomes the default only
/// on a *strictly* larger merged decode total. A tie keeps subsample no
/// matter how much better the FIR's SNR distribution is — the ticket
/// requires "whichever decodes more".
pub fn strict_count_decision(subsample_total: Option<u64>, fir31_total: Option<u64>) -> Decision {
    match (subsample_total, fir31_total) {
        (Some(sub), Some(fir)) => {
            let delta = fir as i64 - sub as i64;
            if delta > 0 {
                Decision {
                    selected_default: "fir31".to_string(),
                    subsample_total: Some(sub),
                    fir31_total: Some(fir),
                    decode_delta: Some(delta),
                    reason: format!(
                        "fir31 decoded {fir} merged messages vs {sub} for subsample \
                         (strictly more)"
                    ),
                }
            } else {
                Decision {
                    selected_default: "subsample".to_string(),
                    subsample_total: Some(sub),
                    fir31_total: Some(fir),
                    decode_delta: Some(delta),
                    reason: format!(
                        "subsample retained: fir31 delta {delta} (a tie or loss keeps \
                         subsample regardless of SNR)"
                    ),
                }
            }
        }
        _ => Decision {
            selected_default: "subsample".to_string(),
            subsample_total: None,
            fir31_total: None,
            decode_delta: None,
            reason: "missing station capture totals: qualification pending, \
                     subsample retained"
                .to_string(),
        },
    }
}

/// A pending report: no station corpus was available. Totals are null
/// (never fabricated zeros), the default stays `subsample`, and the
/// reason names what is missing.
pub fn pending_report(reason: &str) -> DecimationAbReport {
    DecimationAbReport {
        schema_version: REPORT_SCHEMA_VERSION,
        status: STATUS_PENDING.to_string(),
        synthetic: false,
        decision: strict_count_decision(None, None),
        manifest: None,
        decoder_settings: serde_json::json!({
            "note": "not run: no station captures",
        }),
        filter_coefficients: pancetta_dsp::decimation::design_fir31_taps().to_vec(),
        decimation: serde_json::json!({
            "input_rate_hz": 48_000,
            "output_rate_hz": 12_000,
            "subsample": "counter starts 0, retains indices 3, 7, 11, ...",
            "fir31": "31-tap Kaiser low-pass, unity DC gain, cutoff 6 kHz midpoint",
            "fir31_group_delay_input_samples": 15,
        }),
        captures: Vec::new(),
        per_condition: BTreeMap::new(),
    }
    .with_pending_reason(reason)
}

impl DecimationAbReport {
    fn with_pending_reason(mut self, reason: &str) -> Self {
        self.decision.reason = reason.to_string();
        self
    }
}

// ---------------------------------------------------------------------------
// Comparison core (decoder-injectable for tests)
// ---------------------------------------------------------------------------

/// Decode one 12 kHz window into records. Production uses
/// `Ft8Decoder::new(Ft8Config::default())` + `decode_window`; tests inject
/// deterministic callbacks.
pub trait DecodeArm {
    fn decode(&mut self, window: &[f32]) -> Vec<DecodeRecord>;
}

/// The production arm: a fresh decoder per arm per capture, default
/// config, unlimited budget, no cross-file state.
pub struct Ft8DecoderArm {
    #[allow(dead_code)]
    settings: serde_json::Value,
}

impl Ft8DecoderArm {
    pub fn new() -> Self {
        Self {
            settings: serde_json::to_value(pancetta_ft8::Ft8Config::default())
                .unwrap_or(serde_json::Value::Null),
        }
    }

    pub fn settings(&self) -> &serde_json::Value {
        &self.settings
    }
}

impl Default for Ft8DecoderArm {
    fn default() -> Self {
        Self::new()
    }
}

impl DecodeArm for Ft8DecoderArm {
    fn decode(&mut self, window: &[f32]) -> Vec<DecodeRecord> {
        let mut decoder = pancetta_ft8::Ft8Decoder::new(pancetta_ft8::Ft8Config::default())
            .expect("Ft8Decoder::new with default config");
        let messages = decoder
            .decode_window(window)
            .expect("decode_window on a well-formed window");
        messages
            .into_iter()
            .map(|m| DecodeRecord {
                text: m.text,
                snr_db: m.snr_db,
                frequency_hz: m.frequency_offset,
                time_offset_s: m.time_offset,
            })
            .collect()
    }
}

/// Compare both arms over the loaded captures and build the report. The
/// decode callback is invoked per arm per capture over the *same* prefix
/// of each arm's decimated stream; `synthetic` labels test-generated
/// corpora (station totals must never come from synthetic input).
pub fn compare_captures<D: DecodeArm>(
    captures: &[LoadedCapture],
    decode: &mut D,
    synthetic: bool,
) -> DecimationAbReport {
    let mut comparisons = Vec::with_capacity(captures.len());
    let mut sub_total: u64 = 0;
    let mut fir_total: u64 = 0;
    let mut per_condition: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut cond_sub: BTreeMap<String, u64> = BTreeMap::new();
    let mut cond_fir: BTreeMap<String, u64> = BTreeMap::new();

    for capture in captures {
        let prefix = decode_prefix_samples(capture.entry.lead_seconds);
        let sub_stream = subsample_arm(&capture.channel0);
        let fir_stream = fir31_arm(&capture.channel0);

        // Both arms decode the same-length prefix of their own stream.
        // The FIR arm's per-message timing is compensated for its causal
        // 15-input-sample (0.3125 ms) group delay so reported offsets use
        // one effective-end convention.
        let sub_records: Vec<DecodeRecord> = {
            let mut r = decode.decode(&sub_stream[..prefix]);
            r.sort_by(|a, b| a.text.cmp(&b.text));
            r
        };
        let mut fir_records = decode.decode(&fir_stream[..prefix]);
        for rec in &mut fir_records {
            rec.time_offset_s -= 15.0 / 48_000.0;
        }

        let sub_unique = dedupe_by_text(sub_records);
        let fir_unique = dedupe_by_text(fir_records);

        let sub_snrs: Vec<f32> = sub_unique.iter().map(|r| r.snr_db).collect();
        let fir_snrs: Vec<f32> = fir_unique.iter().map(|r| r.snr_db).collect();

        let sub_texts: std::collections::BTreeSet<&String> =
            sub_unique.iter().map(|r| &r.text).collect();
        let fir_texts: std::collections::BTreeSet<&String> =
            fir_unique.iter().map(|r| &r.text).collect();

        let fir_only: Vec<String> = fir_texts
            .difference(&sub_texts)
            .map(|t| (*t).clone())
            .collect();
        let sub_only: Vec<String> = sub_texts
            .difference(&fir_texts)
            .map(|t| (*t).clone())
            .collect();

        // Paired SNR delta for texts present in both arms.
        let mut deltas: Vec<f64> = Vec::new();
        for s in &sub_unique {
            if let Some(f) = fir_unique.iter().find(|r| r.text == s.text) {
                deltas.push(f.snr_db as f64 - s.snr_db as f64);
            }
        }
        deltas.sort_by(|a, b| a.total_cmp(b));
        let paired_snr_delta =
            nearest_rank_quantile(&deltas.iter().map(|d| *d as f32).collect::<Vec<_>>(), 50.0)
                .map(|d| d as f64);

        sub_total += sub_unique.len() as u64;
        fir_total += fir_unique.len() as u64;
        *cond_sub.entry(capture.entry.condition.clone()).or_default() += sub_unique.len() as u64;
        *cond_fir.entry(capture.entry.condition.clone()).or_default() += fir_unique.len() as u64;

        let sub_arm = ArmResult {
            merged_decodes: sub_unique.len(),
            snr: SnrDistribution::from_snrs(&sub_snrs),
            messages: sub_unique.iter().map(|r| r.text.clone()).collect(),
        };
        let fir_arm = ArmResult {
            merged_decodes: fir_unique.len(),
            snr: SnrDistribution::from_snrs(&fir_snrs),
            messages: fir_unique.iter().map(|r| r.text.clone()).collect(),
        };

        comparisons.push(CaptureComparison {
            path: capture.entry.path.clone(),
            sha256: capture.entry.sha256.clone(),
            condition: capture.entry.condition.clone(),
            lead_seconds: capture.entry.lead_seconds,
            subsample: sub_arm,
            fir31: fir_arm,
            fir_only,
            subsample_only: sub_only,
            paired_snr_delta,
            paired_n: deltas.len(),
        });
    }

    for cond in CONDITIONS {
        per_condition.insert(
            cond.to_string(),
            serde_json::json!({
                "subsample": cond_sub.get(cond).copied().unwrap_or(0),
                "fir31": cond_fir.get(cond).copied().unwrap_or(0),
            }),
        );
    }

    let mut decision = strict_count_decision(Some(sub_total), Some(fir_total));
    if synthetic {
        decision.reason = format!(
            "synthetic corpus: {} (synthetic totals never select the shipped default)",
            decision.reason
        );
    }

    DecimationAbReport {
        schema_version: REPORT_SCHEMA_VERSION,
        status: STATUS_COMPLETE.to_string(),
        synthetic,
        decision,
        manifest: None,
        decoder_settings: serde_json::to_value(pancetta_ft8::Ft8Config::default())
            .unwrap_or(serde_json::Value::Null),
        filter_coefficients: pancetta_dsp::decimation::design_fir31_taps().to_vec(),
        decimation: serde_json::json!({
            "input_rate_hz": 48_000,
            "output_rate_hz": 12_000,
            "channel": "channel_0",
            "subsample": "counter starts 0, retains indices 3, 7, 11, ...",
            "fir31": "31-tap Kaiser low-pass, unity DC gain, cutoff 6 kHz midpoint",
            "fir31_group_delay_input_samples": 15,
            "prefix": "lead + 13 s (13.5 s for lead 0.5, 14 s for lead 1.0)",
            "history": "zero-initialized FIR history; first-sample transient kept",
        }),
        captures: comparisons,
        per_condition,
    }
}

/// Full pipeline over a manifest file: load + verify every capture before
/// decoding either arm; return the finished report. Errors name the
/// offending path and reason.
pub fn run_manifest(manifest_path: &Path) -> Result<DecimationAbReport> {
    let text = std::fs::read_to_string(manifest_path)
        .map_err(|e| anyhow!("cannot read manifest {}: {e}", manifest_path.display()))?;
    let manifest: CaptureManifest = serde_json::from_str(&text).map_err(|e| {
        anyhow!(
            "manifest {} is not valid JSON: {e}",
            manifest_path.display()
        )
    })?;
    validate_manifest(&manifest)
        .with_context(|| format!("manifest {} failed validation", manifest_path.display()))?;
    let dir = manifest_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut captures = Vec::with_capacity(manifest.captures.len());
    for entry in &manifest.captures {
        captures.push(load_capture(dir, entry).with_context(|| format!("capture {}", entry.path))?);
    }
    let mut arm = Ft8DecoderArm::new();
    let _ = arm.settings();
    let mut report = compare_captures(&captures, &mut arm, false);
    report.manifest = Some(manifest);
    Ok(report)
}

/// Write the report atomically: a temp file in the output's directory,
/// then rename over the destination. Only called after a successful run —
/// a failure never creates or replaces the output.
pub fn write_report_atomic(report: &DecimationAbReport, output: &Path) -> Result<()> {
    let json = serde_json::to_string_pretty(report)?;
    let dir = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp",
        output
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("report")
    ));
    std::fs::write(&tmp, json.as_bytes())?;
    std::fs::rename(&tmp, output)?;
    Ok(())
}

/// SHA-256 hex of a file (corpus preparation helper).
pub fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}
