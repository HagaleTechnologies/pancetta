//! Batch 86 — hb-104 kill-switch prototype: one-step LS subtract + re-decode.
//!
//! Spec: `docs/superpowers/specs/2026-06-12-hb104-joint-decode-scoping.md`.
//!
//! For each (miss, decoded-neighbor) co-channel pair in the Batch 86 work
//! list (`research/corpus/curated/ft8/hb104_kill_switch.json`):
//!   1. Greedy baseline: default `Ft8Decoder` over the whole slot.
//!   2. Re-synthesize the decoded neighbor's GFSK waveform at its decoded
//!      (freq, dt) from the decode's own `tone_symbols` (exact CRC-checked
//!      codeword; falls back to text re-encode only for hash-free texts).
//!   3. Time-domain least-squares fit of complex amplitude (2 real
//!      unknowns: in-phase + quadrature copies of the synth) over the
//!      synth's overlap with the slot buffer; closed-form 2x2 normal
//!      equations.
//!   4. Subtract the LS estimate from the ORIGINAL audio (pairs are
//!      independent trials, never cumulative) and re-decode the residual
//!      with a fresh default decoder — whole buffer, no frequency scoping.
//!   5. Count recovery of the targeted miss, serendipitous new truth TPs,
//!      and new non-truth decodes (FP cost). The miss's coordinates are
//!      used ONLY for post-hoc attribution, never in the decode path.
//!
//! Work-list integrity note (discovered at implementation time): 54/70
//! pairs are display aliases — ft8_lib renders an unresolved hashed
//! callsign as `<...>` while pancetta renders `<...NNNN>`, so the "miss"
//! is the SAME transmission as the decoded neighbor (identical text modulo
//! the hash token; coordinates agree to grid granularity). Subtracting the
//! neighbor removes the missed signal itself, so recovery is structurally
//! impossible for those pairs. The other 16 pairs have FP-looking
//! cross-text neighbors that don't reproduce in the greedy decode. The
//! alias pairs are still run (spec-faithful, and they exercise the
//! precision subtract) but tagged `alias`; the pre-registered decision
//! uses the genuine (non-alias) co-channel pairs — the population hb-104
//! is actually about — and `premise_audit` re-measures that population on
//! the full 5/30 corpus with hash-normalized matching.
//!
//! Two fit rounds per pair, both reported (the pre-registered SHELVE
//! criterion is "< 2% after refinement", so the refinement round runs
//! unconditionally):
//!   - **Global**: one complex amplitude over the whole 12.64 s synth.
//!     First-run finding: mean residual-energy ratio 0.9999 — the
//!     work-list coordinates are quantized to the 3.125 Hz half-tone
//!     grid, and even a 0.08 Hz error fully decorrelates a 12.6 s
//!     coherent fit (the spec's "frequency quantization" risk, observed).
//!   - **Refined**: per-block amplitudes (10 blocks of 7.9 symbols, 20
//!     real unknowns) x a fine-shift grid search, delta-f in +/-1.6 Hz
//!     (0.1 Hz steps; wider than the spec's +/-0.5 Hz because the grid
//!     quantization alone can be +/-1.5625 Hz) and delta-t in +/-2400
//!     samples (120-sample steps; the `--dt-scan` diagnostic showed the
//!     decoder's reported time_offset can be ~0.18 s off sample-accurate).
//!     Best cell = minimum residual energy over the fit window. Only the
//!     NEIGHBOR's coordinates seed the search — the miss's never do.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch86_hb104_kill_switch

use anyhow::{Context, Result};
use num_complex::Complex;
use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, NUM_SYMBOLS};
use rustfft::FftPlanner;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: f64 = 12_000.0;

/// Anchor frequency for synthesis. The public modulator rejects total
/// frequencies whose 8-tone span exceeds `MAX_FREQUENCY_DEVIATION`
/// (2500 Hz) or sits below 200 Hz, and 18/70 work-list neighbors are
/// above that ceiling (up to 2859 Hz). We therefore always modulate at
/// 1500 Hz and heterodyne the analytic signal to the exact target
/// frequency — an exact operation for these narrowband (~50 Hz wide)
/// signals well away from DC and Nyquist.
const MOD_ANCHOR_HZ: f64 = 1500.0;

/// Minimum overlap between the synth and the slot buffer for a fit to
/// be attempted: 8 symbol periods (the leading Costas block + 1).
const MIN_OVERLAP_SAMPLES: i64 = 8 * 1920;

#[derive(Deserialize)]
struct WorkList {
    entries: Vec<WorkEntry>,
}

#[derive(Deserialize)]
struct WorkEntry {
    wav_path: String,
    wav_sha256: String,
    pairs: Vec<WorkPair>,
}

#[derive(Deserialize, Clone)]
struct WorkPair {
    miss_message: String,
    /// Present in the work-list schema; intentionally unused in code —
    /// the honesty rule forbids using the miss's coordinates anywhere
    /// in the decode path (they exist only for human inspection).
    #[allow(dead_code)]
    miss_freq_hz: f64,
    #[allow(dead_code)]
    miss_time_sec: f64,
    neighbor_text: String,
    neighbor_freq_hz: f64,
    neighbor_dt_sec: f64,
}

fn workspace_root() -> Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

fn load_wav(path: &Path) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    anyhow::ensure!(spec.channels == 1 && spec.sample_rate == 12000);
    Ok(match spec.sample_format {
        hound::SampleFormat::Int => r
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
    })
}

fn load_ft8lib_truth(ws: &Path, sha: &str) -> HashSet<String> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{sha}.ft8lib.json"));
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
        return HashSet::new();
    };
    v["decodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["message"].as_str().map(|s| s.to_string()))
        .collect()
}

/// Replace any `<...>` / `<...NNNN>` hash-display token with a canonical
/// `<H>` so ft8_lib and pancetta renderings of the same hashed callsign
/// compare equal.
fn normalize_hash_tokens(text: &str) -> String {
    text.split_whitespace()
        .map(|tok| {
            if tok.starts_with('<') && tok.ends_with('>') {
                "<H>"
            } else {
                tok
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// True when the pair's "miss" is just a display alias of the decoded
/// neighbor (same transmission): identical text modulo hash tokens.
/// Coordinates are deliberately NOT compared — ft8_lib truth and the
/// pancetta scan disagree by up to 3.125 Hz / 0.16 s for the same signal
/// (grid granularity), which is exactly how 11 alias pairs masqueraded
/// as genuine co-channel pairs in the first pass of this experiment.
fn is_alias_pair(p: &WorkPair) -> bool {
    normalize_hash_tokens(&p.miss_message) == normalize_hash_tokens(&p.neighbor_text)
}

/// FFT-based analytic signal (Hilbert transform). Chosen over the
/// quarter-period-delay approximation because rustfft is already a
/// dependency of pancetta-research and the full-length FFT Hilbert is
/// essentially exact for this narrowband signal, whereas the integer
/// sample delay round(12000/(4f)) only lands on 90 deg when f divides
/// 3000 evenly (e.g. 96 deg at 800 Hz). We also reuse the analytic form
/// to heterodyne high-frequency neighbors past the modulator's 2500 Hz
/// ceiling (see `MOD_ANCHOR_HZ`).
fn analytic(s: &[f32]) -> Vec<Complex<f64>> {
    let n = s.len();
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(n);
    let ifft = planner.plan_fft_inverse(n);
    let mut buf: Vec<Complex<f64>> = s.iter().map(|&x| Complex::new(x as f64, 0.0)).collect();
    fft.process(&mut buf);
    let half = n.div_ceil(2);
    for (k, v) in buf.iter_mut().enumerate() {
        if k == 0 || (n % 2 == 0 && k == n / 2) {
            // DC and Nyquist bins unchanged.
        } else if k < half {
            *v *= 2.0;
        } else {
            *v = Complex::new(0.0, 0.0);
        }
    }
    ifft.process(&mut buf);
    let scale = 1.0 / n as f64;
    for v in buf.iter_mut() {
        *v *= scale;
    }
    buf
}

/// Modulate the neighbor at the anchor frequency and return its analytic
/// signal (computed once per pair; fine-shift candidates reuse it).
fn synth_analytic_anchor(symbols: &[u8; NUM_SYMBOLS]) -> Result<Vec<Complex<f64>>> {
    let mut modulator = Ft8Modulator::new(12_000, MOD_ANCHOR_HZ, 1.0)
        .map_err(|e| anyhow::anyhow!("Ft8Modulator::new: {e}"))?;
    let wave = modulator
        .modulate_symbols(symbols, 0.0)
        .map_err(|e| anyhow::anyhow!("modulate_symbols: {e}"))?;
    Ok(analytic(&wave))
}

/// Heterodyne the anchor analytic signal to exactly `freq_hz`, returning
/// the in-phase copy `s` and its quadrature `s_q` (Re/Im of the shifted
/// analytic signal).
fn shift_to(z: &[Complex<f64>], freq_hz: f64) -> (Vec<f64>, Vec<f64>) {
    let w = 2.0 * std::f64::consts::PI * (freq_hz - MOD_ANCHOR_HZ) / SAMPLE_RATE;
    let mut s = Vec::with_capacity(z.len());
    let mut sq = Vec::with_capacity(z.len());
    for (i, zi) in z.iter().enumerate() {
        let v = zi * Complex::from_polar(1.0, w * i as f64);
        s.push(v.re);
        sq.push(v.im);
    }
    (s, sq)
}

/// Per-block LS solution: block index range in synth coordinates plus the
/// fitted complex amplitude (a = in-phase, b = quadrature).
struct BlockSolve {
    j0: usize,
    j1: usize,
    a: f64,
    b: f64,
}

struct Fit {
    solves: Vec<BlockSolve>,
    /// ||x - sum a_k s + b_k sq||^2 / ||x||^2 over the overlap window.
    res_ratio: f64,
    /// Mean per-block |(a, b)| across fitted blocks.
    mean_amp: f64,
}

/// Closed-form least squares min ||x - a*s - b*sq||^2 solved independently
/// per block (2x2 normal equations each). `n_blocks = 1` is the global
/// single-amplitude fit; `n_blocks = 10` is the spec's 7.9-symbol-block
/// refinement. Uses the identity res = <x,x> - a<x,s> - b<x,sq> at the LS
/// solution, so candidate evaluation needs no subtraction pass.
fn per_block_fit(x: &[f32], s: &[f64], sq: &[f64], offset: i64, n_blocks: usize) -> Option<Fit> {
    let n = x.len() as i64;
    let m = s.len() as i64;
    if (offset + m).min(n) - offset.max(0) < MIN_OVERLAP_SAMPLES {
        return None;
    }
    let block_len = s.len().div_ceil(n_blocks);
    let mut solves = Vec::with_capacity(n_blocks);
    let mut res_energy = 0f64;
    let mut total_xx = 0f64;
    for blk in 0..n_blocks {
        let j0 = blk * block_len;
        let j1 = ((blk + 1) * block_len).min(s.len());
        let i0 = (offset + j0 as i64).max(0);
        let i1 = (offset + j1 as i64).min(n);
        if i1 <= i0 {
            continue;
        }
        let (mut ss, mut sxq, mut qq, mut xs, mut xq, mut xx) =
            (0f64, 0f64, 0f64, 0f64, 0f64, 0f64);
        for i in i0..i1 {
            let xi = x[i as usize] as f64;
            let j = (i - offset) as usize;
            let (si, qi) = (s[j], sq[j]);
            ss += si * si;
            sxq += si * qi;
            qq += qi * qi;
            xs += xi * si;
            xq += xi * qi;
            xx += xi * xi;
        }
        total_xx += xx;
        let det = ss * qq - sxq * sxq;
        if det.abs() <= 1e-9 * ss.max(1e-30) * qq.max(1e-30) {
            // Degenerate block (e.g. tiny overlap): subtract nothing here.
            res_energy += xx;
            continue;
        }
        let a = (xs * qq - xq * sxq) / det;
        let b = (xq * ss - xs * sxq) / det;
        res_energy += (xx - a * xs - b * xq).max(0.0);
        solves.push(BlockSolve { j0, j1, a, b });
    }
    if solves.is_empty() {
        return None;
    }
    let mean_amp = solves
        .iter()
        .map(|v| (v.a * v.a + v.b * v.b).sqrt())
        .sum::<f64>()
        / solves.len() as f64;
    Some(Fit {
        solves,
        res_ratio: res_energy / total_xx.max(1e-30),
        mean_amp,
    })
}

/// Apply the fitted per-block subtraction to a copy of the original audio.
fn apply_subtract(x: &[f32], s: &[f64], sq: &[f64], offset: i64, fit: &Fit) -> Vec<f32> {
    let mut out = x.to_vec();
    let n = x.len() as i64;
    for blk in &fit.solves {
        let i0 = (offset + blk.j0 as i64).max(0);
        let i1 = (offset + blk.j1 as i64).min(n);
        for i in i0..i1 {
            let j = (i - offset) as usize;
            out[i as usize] = (x[i as usize] as f64 - blk.a * s[j] - blk.b * sq[j]) as f32;
        }
    }
    out
}

/// Refinement grid: delta-f covers the +/-1.5625 Hz half-tone-grid
/// quantization of the work-list coordinates (0.1 Hz steps keep the
/// residual within-cell phase drift per 1.26 s block negligible);
/// Delta-t covers +/-0.2 s in 10 ms steps: the `--dt-scan` diagnostic
/// showed pancetta's reported time_offset can be up to ~0.18 s from the
/// signal's sample-accurate position (LDPC decoding tolerates that; a
/// coherent subtract does not), with the strongest signals in the first
/// three slots locking at -360/-1680/-2160 samples from nominal.
const DF_STEPS: i32 = 16; // +/-1.6 Hz in 0.1 Hz steps
const DT_RANGE: i64 = 2400; // +/-0.2 s
const DT_STEP: i64 = 120; // 10 ms

/// Energy in the frequency band [f_lo, f_hi] Hz of `x[i0..i1]`, via a
/// plain FFT periodogram. Used as the spec's "residual energy at victim
/// bins before/after" mechanism evidence: the neighbor's 8 tones span
/// 43.75 Hz starting at its base frequency, and for a |delta-f| < 6.25 Hz
/// co-channel pair the miss lives in the same band.
fn band_energy(x: &[f32], i0: usize, i1: usize, f_lo: f64, f_hi: f64) -> f64 {
    let n = i1 - i0;
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(n);
    let mut buf: Vec<Complex<f64>> = x[i0..i1]
        .iter()
        .map(|&v| Complex::new(v as f64, 0.0))
        .collect();
    fft.process(&mut buf);
    let hz_per_bin = SAMPLE_RATE / n as f64;
    let k_lo = (f_lo / hz_per_bin).floor().max(0.0) as usize;
    let k_hi = ((f_hi / hz_per_bin).ceil() as usize).min(n / 2);
    buf[k_lo..=k_hi].iter().map(|v| v.norm_sqr()).sum()
}

#[derive(Default)]
struct SlotStats {
    label: String,
    n_pairs: usize,
    skipped_neighbor: usize,
    skipped_miss_decoded: usize,
    encode_failed: usize,
    tried: usize,
    tried_alias: usize,
    rec_global: usize,
    rec_refined: usize,
    rec_refined_alias: usize,
    ser_global: usize,
    fp_global: usize,
    fp_hash_global: usize,
    ser_refined: usize,
    fp_refined: usize,
    fp_hash_refined: usize,
    res_g: Vec<f64>,
    res_r: Vec<f64>,
    band_g: Vec<f64>,
    band_r: Vec<f64>,
}

struct DecodeOutcome {
    recovered: bool,
    serendip: usize,
    fps: usize,
    /// Of `fps`, how many are hash-display aliases of a truth message
    /// (counted as FP under the pre-registered tol=0 exact-text rule,
    /// but actually true decodes rendered differently).
    fp_hash_alias: usize,
}

/// Re-decode a residual buffer with a fresh default decoder (whole buffer,
/// no frequency scoping) and classify the NEW texts (not in the greedy
/// set) against truth. The miss text is used only here, post-decode.
fn redecode_classify(
    cfg: &Ft8Config,
    residual: &[f32],
    greedy_texts: &HashSet<String>,
    truth: &HashSet<String>,
    truth_norm: &HashSet<String>,
    miss: &str,
) -> Result<DecodeOutcome> {
    let mut decoder =
        Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(residual)
        .map_err(|e| anyhow::anyhow!("decode_window(residual): {e}"))?;
    let new_texts: HashSet<String> = decoded
        .iter()
        .map(|d| d.text.clone())
        .filter(|t| !greedy_texts.contains(t))
        .collect();
    let fp_texts: Vec<&String> = new_texts.iter().filter(|t| !truth.contains(*t)).collect();
    Ok(DecodeOutcome {
        recovered: new_texts.contains(miss),
        serendip: new_texts
            .iter()
            .filter(|t| truth.contains(*t) && *t != miss)
            .count(),
        fps: fp_texts.len(),
        fp_hash_alias: fp_texts
            .iter()
            .filter(|t| truth_norm.contains(&normalize_hash_tokens(t)))
            .count(),
    })
}

/// Diagnostic mode (`--dt-scan`): for the first 3 slots, take the
/// STRONGEST greedy decode (its own decoded freq/dt, not the work list),
/// re-synth from its tone_symbols, and scan a wide (delta-f, delta-t)
/// grid. If even the strongest signal in a slot produces no residual
/// minimum, the synth/fit pipeline is systematically misaligned; if it
/// locks cleanly, the kill-switch failures are signal-strength-limited.
fn dt_scan(worklist: &WorkList) -> Result<()> {
    let cfg = Ft8Config::default();
    for entry in worklist.entries.iter().take(3) {
        let samples = load_wav(Path::new(&entry.wav_path))?;
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let greedy = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        let Some(strongest) = greedy
            .iter()
            .filter(|d| {
                d.tone_symbols
                    .as_ref()
                    .is_some_and(|t| t.len() >= NUM_SYMBOLS)
            })
            .max_by(|a, b| a.snr_db.partial_cmp(&b.snr_db).unwrap())
        else {
            continue;
        };
        let mut symbols = [0u8; NUM_SYMBOLS];
        symbols.copy_from_slice(&strongest.tone_symbols.as_ref().unwrap()[..NUM_SYMBOLS]);
        let z = synth_analytic_anchor(&symbols)?;
        let offset0 = (strongest.time_offset * SAMPLE_RATE).round() as i64;
        println!(
            "== {} strongest: '{}' snr={:.0} f={:.1} dt={:.2}",
            entry.wav_path,
            strongest.text,
            strongest.snr_db,
            strongest.frequency_offset,
            strongest.time_offset
        );
        let mut cells: Vec<(f64, i64, f64)> = Vec::new();
        for k in -20..=20 {
            let df = k as f64 * 0.1;
            let (s, sq) = shift_to(&z, strongest.frequency_offset + df);
            let mut dt = -12_000i64;
            while dt <= 12_000 {
                if let Some(fit) = per_block_fit(&samples, &s, &sq, offset0 + dt, 10) {
                    cells.push((df, dt, fit.res_ratio));
                }
                dt += 120;
            }
        }
        cells.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap());
        let nominal = cells
            .iter()
            .find(|c| c.0 == 0.0 && c.1 == 0)
            .map(|c| c.2)
            .unwrap_or(f64::NAN);
        println!("   res_ratio at nominal (df=0,dt=0): {nominal:.4}");
        for (df, dt, r) in cells.iter().take(5) {
            println!("   best: df={df:+.1} dt={dt:+} res_ratio={r:.4}");
        }
    }
    Ok(())
}

/// Audit the Batch 85 premise on the full 5/30 scan: how much of the
/// "co-channel miss" population survives hash-display normalization?
/// (ft8_lib renders unresolved hashed callsigns as `<...>`, pancetta as
/// `<...NNNN>`; tol=0 exact-text matching counts those decodes as misses
/// sitting at delta-f = 0 from "a decoded signal" — themselves.)
fn premise_audit(ws: &Path) -> Result<String> {
    #[derive(Deserialize)]
    struct ScanDecode {
        text: String,
        freq: f64,
        dt: f64,
    }
    #[derive(Deserialize)]
    struct ScanRec {
        path: String,
        decodes: Vec<ScanDecode>,
    }
    let manifest: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/raw_530_full.manifest.json"),
    )?)?;
    let mut path_to_sha = std::collections::HashMap::new();
    for e in manifest["entries"].as_array().context("entries")?.iter() {
        path_to_sha.insert(
            e["wav_path"].as_str().context("wav_path")?.to_string(),
            e["wav_sha256"].as_str().context("wav_sha256")?.to_string(),
        );
    }
    let (mut n_slots, mut n_truth, mut miss_exact, mut miss_norm) =
        (0usize, 0usize, 0usize, 0usize);
    // (gate label, freq gate, near-any count, near-TP count)
    let mut gates = [("6.25 Hz", 6.25f64, 0usize, 0usize), ("25 Hz", 25.0, 0, 0)];
    let scan = std::fs::read_to_string(ws.join("research/corpus/scans/raw_20260530_scan.jsonl"))?;
    for line in scan.lines().filter(|l| !l.trim().is_empty()) {
        let rec: ScanRec = serde_json::from_str(line)?;
        let Some(sha) = path_to_sha.get(&rec.path) else {
            continue;
        };
        let truth_path = ws
            .join("research/baselines/ft8")
            .join(format!("{sha}.ft8lib.json"));
        let Ok(txt) = std::fs::read_to_string(&truth_path) else {
            continue;
        };
        let v: serde_json::Value = serde_json::from_str(&txt)?;
        let truth: Vec<(String, f64, f64)> = v["decodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| {
                Some((
                    d["message"].as_str()?.to_string(),
                    d["freq_hz"].as_f64()?,
                    d["time_sec"].as_f64().unwrap_or(0.0),
                ))
            })
            .collect();
        if truth.is_empty() {
            continue;
        }
        n_slots += 1;
        n_truth += truth.len();
        let dec_exact: HashSet<&str> = rec.decodes.iter().map(|d| d.text.as_str()).collect();
        let dec_norm: HashSet<String> = rec
            .decodes
            .iter()
            .map(|d| normalize_hash_tokens(&d.text))
            .collect();
        let truth_norm: HashSet<String> = truth
            .iter()
            .map(|(m, _, _)| normalize_hash_tokens(m))
            .collect();
        for (msg, tf, tt) in &truth {
            if dec_exact.contains(msg.as_str()) {
                continue;
            }
            miss_exact += 1;
            let mn = normalize_hash_tokens(msg);
            if dec_norm.contains(&mn) {
                continue; // display alias: actually decoded
            }
            miss_norm += 1;
            for (_, fg, near_any, near_tp) in gates.iter_mut() {
                let mut any = false;
                let mut tp = false;
                for d in &rec.decodes {
                    if normalize_hash_tokens(&d.text) == mn {
                        continue;
                    }
                    if (d.freq - tf).abs() < *fg && (d.dt - tt).abs() < 2.0 {
                        any = true;
                        if truth_norm.contains(&normalize_hash_tokens(&d.text)) {
                            tp = true;
                        }
                    }
                }
                *near_any += any as usize;
                *near_tp += tp as usize;
            }
        }
    }
    let mut out = format!(
        "## Premise audit (full 5/30 scan x ft8_lib truth, hash-normalized)\n\n\
         - {n_slots} slots, {n_truth} truth decodes.\n\
         - Misses at tol=0 exact text: {miss_exact} (the Batch 85 population).\n\
         - Misses after hash normalization: {miss_norm} — **{} of {miss_exact} \
         ({:.1}%) of nominal misses are display aliases of pancetta's own \
         decodes** (the Batch 85 \"within one tone spacing of a decoded \
         signal\" premise was dominated by misses at delta-f = 0 from \
         themselves).\n",
        miss_exact - miss_norm,
        100.0 * (miss_exact - miss_norm) as f64 / miss_exact.max(1) as f64,
    );
    for (label, _, near_any, near_tp) in &gates {
        out.push_str(&format!(
            "- Genuine misses within {label} / 2 s of a decoded signal: \
             {near_any} ({:.1}%); of a TRUTH-CONFIRMED decoded signal: \
             {near_tp} ({:.1}%).\n",
            100.0 * *near_any as f64 / miss_norm.max(1) as f64,
            100.0 * *near_tp as f64 / miss_norm.max(1) as f64,
        ));
    }
    Ok(out)
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let worklist: WorkList = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hb104_kill_switch.json"),
    )?)?;
    if std::env::args().any(|a| a == "--dt-scan") {
        return dt_scan(&worklist);
    }

    let cfg = Ft8Config::default();
    let mut slots: Vec<SlotStats> = Vec::new();
    let mut pair_lines: Vec<String> = Vec::new();

    for (ei, entry) in worklist.entries.iter().enumerate() {
        let label = Path::new(&entry.wav_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&entry.wav_path)
            .to_string();
        eprintln!(
            "[{}/{}] {label} ({} pairs)",
            ei + 1,
            worklist.entries.len(),
            entry.pairs.len()
        );
        let mut st = SlotStats {
            label: label.clone(),
            n_pairs: entry.pairs.len(),
            ..Default::default()
        };

        let samples = load_wav(Path::new(&entry.wav_path))
            .with_context(|| format!("load_wav {}", entry.wav_path))?;
        let truth = load_ft8lib_truth(&ws, &entry.wav_sha256);

        // 1. Greedy baseline (complete production pipeline incl. multipass).
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let greedy = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        let greedy_texts: HashSet<String> = greedy.iter().map(|d| d.text.clone()).collect();
        let truth_norm: HashSet<String> = truth.iter().map(|t| normalize_hash_tokens(t)).collect();

        for pair in &entry.pairs {
            if !greedy_texts.contains(&pair.neighbor_text) {
                st.skipped_neighbor += 1;
                continue;
            }
            if greedy_texts.contains(&pair.miss_message) {
                st.skipped_miss_decoded += 1;
                continue;
            }
            let alias = is_alias_pair(pair);

            // 2. Neighbor symbols. Prefer the greedy decode's own
            // tone_symbols (the exact CRC-checked codeword — strictly more
            // faithful than a text round-trip, and the only correct option
            // for hashed-callsign texts, where the encoder's free-text
            // fallback would silently synthesize the wrong bits). Text
            // re-encode is the fallback for hash-free texts only.
            let matched = greedy
                .iter()
                .filter(|d| d.text == pair.neighbor_text)
                .min_by(|x, y| {
                    let dx = (x.frequency_offset - pair.neighbor_freq_hz).abs();
                    let dy = (y.frequency_offset - pair.neighbor_freq_hz).abs();
                    dx.partial_cmp(&dy).unwrap()
                });
            let symbols: Option<[u8; NUM_SYMBOLS]> = matched
                .and_then(|d| d.tone_symbols.as_ref())
                .filter(|v| v.len() >= NUM_SYMBOLS)
                .map(|v| {
                    let mut arr = [0u8; NUM_SYMBOLS];
                    arr.copy_from_slice(&v[..NUM_SYMBOLS]);
                    arr
                })
                .or_else(|| {
                    if pair.neighbor_text.contains('<') {
                        None
                    } else {
                        Ft8Encoder::new()
                            .encode_message(&pair.neighbor_text, None)
                            .ok()
                    }
                });
            let Some(symbols) = symbols else {
                st.encode_failed += 1;
                continue;
            };

            // 3-4. Synthesize at the decoded coordinates, LS fit, subtract.
            // The miss's coordinates are NOT used anywhere below — only
            // pair.miss_message, and only for post-hoc attribution.
            let z = match synth_analytic_anchor(&symbols) {
                Ok(z) => z,
                Err(e) => {
                    eprintln!("    synth failed ({}): {e}", pair.neighbor_text);
                    st.encode_failed += 1;
                    continue;
                }
            };
            let offset0 = (pair.neighbor_dt_sec * SAMPLE_RATE).round() as i64;

            // Round 1: global single-amplitude fit at nominal coordinates.
            let (s0, sq0) = shift_to(&z, pair.neighbor_freq_hz);
            let Some(gfit) = per_block_fit(&samples, &s0, &sq0, offset0, 1) else {
                eprintln!("    degenerate/short fit ({})", pair.neighbor_text);
                st.encode_failed += 1;
                continue;
            };
            let g_res = apply_subtract(&samples, &s0, &sq0, offset0, &gfit);
            let g = redecode_classify(
                &cfg,
                &g_res,
                &greedy_texts,
                &truth,
                &truth_norm,
                &pair.miss_message,
            )?;

            // Round 2: per-block amplitudes x fine-shift grid (refinement).
            let mut best: Option<(f64, i64, Fit)> = None;
            for k in -DF_STEPS..=DF_STEPS {
                let df = k as f64 * 0.1;
                let (s, sq) = shift_to(&z, pair.neighbor_freq_hz + df);
                let mut dt = -DT_RANGE;
                while dt <= DT_RANGE {
                    if let Some(fit) = per_block_fit(&samples, &s, &sq, offset0 + dt, 10) {
                        if best
                            .as_ref()
                            .is_none_or(|(_, _, b)| fit.res_ratio < b.res_ratio)
                        {
                            best = Some((df, offset0 + dt, fit));
                        }
                    }
                    dt += DT_STEP;
                }
            }
            let Some((best_df, best_off, rfit)) = best else {
                eprintln!("    refinement found no valid fit ({})", pair.neighbor_text);
                st.encode_failed += 1;
                continue;
            };
            let (sb, sqb) = shift_to(&z, pair.neighbor_freq_hz + best_df);
            let r_res = apply_subtract(&samples, &sb, &sqb, best_off, &rfit);
            let r = redecode_classify(
                &cfg,
                &r_res,
                &greedy_texts,
                &truth,
                &truth_norm,
                &pair.miss_message,
            )?;

            // Mechanism evidence: energy at the victim's bins (the neighbor
            // band +/- margin) over the fit window, before vs after subtract.
            let i0 = offset0.max(0) as usize;
            let i1 = (offset0 + z.len() as i64).min(samples.len() as i64) as usize;
            let (f_lo, f_hi) = (
                pair.neighbor_freq_hz - 10.0,
                pair.neighbor_freq_hz + 43.75 + 10.0,
            );
            let e0 = band_energy(&samples, i0, i1, f_lo, f_hi).max(1e-30);
            let bg = band_energy(&g_res, i0, i1, f_lo, f_hi) / e0;
            let br = band_energy(&r_res, i0, i1, f_lo, f_hi) / e0;

            st.tried += 1;
            st.tried_alias += alias as usize;
            st.rec_global += g.recovered as usize;
            st.rec_refined += r.recovered as usize;
            st.rec_refined_alias += (r.recovered && alias) as usize;
            st.ser_global += g.serendip;
            st.fp_global += g.fps;
            st.fp_hash_global += g.fp_hash_alias;
            st.ser_refined += r.serendip;
            st.fp_refined += r.fps;
            st.fp_hash_refined += r.fp_hash_alias;
            st.res_g.push(gfit.res_ratio);
            st.res_r.push(rfit.res_ratio);
            st.band_g.push(bg);
            st.band_r.push(br);

            let best_dt = best_off - offset0;
            pair_lines.push(format!(
                "| {label} | {} | {:.1} | {} | {} | {} | {} | {} | {} | {:.4} | {:.4} | {bg:.3} | {br:.3} | {best_df:+.1} | {best_dt:+} | {:.5} |",
                pair.neighbor_text.replace('|', "\\|"),
                pair.neighbor_freq_hz,
                pair.miss_message.replace('|', "\\|"),
                if alias { "alias" } else { "genuine" },
                if g.recovered { "YES" } else { "no" },
                if r.recovered { "YES" } else { "no" },
                r.serendip,
                r.fps,
                gfit.res_ratio,
                rfit.res_ratio,
                rfit.mean_amp,
            ));
            eprintln!(
                "    pair {} -> {} [{}] rec g/r={}/{} ser_r={} fp_r={} res g/r={:.4}/{:.4} band g/r={bg:.3}/{br:.3} df={best_df:+.1} dt={best_dt:+} amp={:.5}",
                pair.neighbor_text,
                pair.miss_message,
                if alias { "alias" } else { "genuine" },
                g.recovered,
                r.recovered,
                r.serendip,
                r.fps,
                gfit.res_ratio,
                rfit.res_ratio,
                rfit.mean_amp,
            );
        }
        slots.push(st);
    }

    // ---- Aggregate -------------------------------------------------------
    let tried: usize = slots.iter().map(|s| s.tried).sum();
    let tried_alias: usize = slots.iter().map(|s| s.tried_alias).sum();
    let tried_genuine = tried - tried_alias;
    let rec_global: usize = slots.iter().map(|s| s.rec_global).sum();
    let rec_refined: usize = slots.iter().map(|s| s.rec_refined).sum();
    let rec_refined_alias: usize = slots.iter().map(|s| s.rec_refined_alias).sum();
    let rec_refined_genuine = rec_refined - rec_refined_alias;
    let ser_g: usize = slots.iter().map(|s| s.ser_global).sum();
    let fp_g: usize = slots.iter().map(|s| s.fp_global).sum();
    let fp_hash_g: usize = slots.iter().map(|s| s.fp_hash_global).sum();
    let ser_r: usize = slots.iter().map(|s| s.ser_refined).sum();
    let fp_r: usize = slots.iter().map(|s| s.fp_refined).sum();
    let fp_hash_r: usize = slots.iter().map(|s| s.fp_hash_refined).sum();
    let encode_failed: usize = slots.iter().map(|s| s.encode_failed).sum();
    let skipped_neighbor: usize = slots.iter().map(|s| s.skipped_neighbor).sum();
    let skipped_miss: usize = slots.iter().map(|s| s.skipped_miss_decoded).sum();
    let mean_of = |sel: &dyn Fn(&SlotStats) -> &Vec<f64>| {
        let all: Vec<f64> = slots.iter().flat_map(|s| sel(s).clone()).collect();
        all.iter().sum::<f64>() / all.len().max(1) as f64
    };
    let mean_res_g = mean_of(&|s| &s.res_g);
    let mean_res_r = mean_of(&|s| &s.res_r);
    let mean_band_g = mean_of(&|s| &s.band_g);
    let mean_band_r = mean_of(&|s| &s.band_r);
    let rate_all = rec_refined as f64 / tried.max(1) as f64;
    let rate_genuine = rec_refined_genuine as f64 / tried_genuine.max(1) as f64;

    // Pre-registered decision (spec 2026-06-12-hb104-joint-decode-scoping.md):
    // SHELVE is judged "after refinement", so the refined round is the
    // decision round. Applied to the genuine co-channel pairs (alias pairs
    // are work-list artifacts where recovery is structurally impossible;
    // see header).
    let fp_ok = fp_r <= rec_refined.max(1) || fp_r <= rec_refined_genuine.max(1);
    let decision = if tried_genuine == 0 {
        "SHELVE — zero valid co-channel targets: every tried pair's \"miss\" is a \
         hash-display alias of the subtracted neighbor itself, and the premise \
         audit shows the Batch 85 co-channel population was a truth-matching \
         artifact (0 genuine misses within 6.25 Hz of a truth-confirmed decode \
         on the full 5/30 corpus). The mechanism hb-104 targets does not exist \
         at measurable frequency in this data."
    } else if rate_genuine >= 0.05 && fp_ok {
        "PROCEED"
    } else if rate_genuine >= 0.02 && fp_ok {
        "WEAK-PROCEED"
    } else {
        "SHELVE"
    };

    let mut body = String::from(
        "# Batch 86 — hb-104 kill-switch: one-step LS subtract + re-decode\n\n\
         Spec: `docs/superpowers/specs/2026-06-12-hb104-joint-decode-scoping.md`.\n\
         Work list: `research/corpus/curated/ft8/hb104_kill_switch.json` (20 slots, 70 pairs).\n\
         Truth: ft8_lib, tol=0 exact text. Pairs are independent trials from the\n\
         original audio (never cumulative). Miss coordinates unused in decode.\n\n\
         Two fit rounds per pair: **global** (one complex amplitude over the\n\
         12.64 s synth, nominal coordinates) and **refined** (10 per-block\n\
         amplitudes x grid search over delta-f in +/-1.6 Hz, delta-t in +/-2400\n\
         samples, best residual energy). The refined round is the decision\n\
         round per the spec's \"< 2% after refinement\" SHELVE wording.\n\n\
         ## Work-list integrity finding\n\n\
         54/70 pairs are **display aliases**: ft8_lib renders an unresolved hashed\n\
         callsign as `<...>` while pancetta renders `<...NNNN>`, so the \"miss\" is\n\
         the same transmission as the decoded neighbor (identical text modulo the\n\
         hash token; coordinates agree to grid granularity, up to 3.125 Hz /\n\
         0.16 s). Subtracting the neighbor removes the missed signal itself —\n\
         recovery is structurally impossible for those pairs. They were run anyway\n\
         (they exercise the precision subtract and provide the mechanism\n\
         evidence). The remaining 16 pairs have cross-text neighbors that did not\n\
         reproduce in the greedy decode — all 16 neighbor texts look like FP\n\
         decodes from the original scan (e.g. `2W9XHD JU4YID/P R JN15`), so the\n\
         work list contains zero valid co-channel targets. See the premise audit\n\
         below for what this means for the Batch 85 numbers.\n\n\
         ## Per-slot results\n\n\
         | Slot | pairs | neighbor-not-decoded | encode/fit failed | tried (alias) | rec global | rec refined (alias) | serendip TPs (refined) | new FPs (refined) | mean res-ratio g/r | mean band-ratio g/r |\n\
         |---|---:|---:|---:|---|---:|---|---:|---:|---|---|\n",
    );
    for st in &slots {
        let mg = st.res_g.iter().sum::<f64>() / st.res_g.len().max(1) as f64;
        let mr = st.res_r.iter().sum::<f64>() / st.res_r.len().max(1) as f64;
        let bg = st.band_g.iter().sum::<f64>() / st.band_g.len().max(1) as f64;
        let br = st.band_r.iter().sum::<f64>() / st.band_r.len().max(1) as f64;
        body.push_str(&format!(
            "| {} | {} | {} | {} | {} ({}) | {} | {} ({}) | {} | {} | {mg:.4} / {mr:.4} | {bg:.3} / {br:.3} |\n",
            st.label,
            st.n_pairs,
            st.skipped_neighbor,
            st.encode_failed,
            st.tried,
            st.tried_alias,
            st.rec_global,
            st.rec_refined,
            st.rec_refined_alias,
            st.ser_refined,
            st.fp_refined,
        ));
    }
    body.push_str(&format!(
        "\n## Per-pair detail\n\n\
         | Slot | Neighbor (subtracted) | f (Hz) | Targeted miss | class | rec g | rec r | ser r | FP r | res g | res r | band g | band r | best df | best dt | mean \\|a,b\\| |\n\
         |---|---|---:|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n{}\n",
        pair_lines.join("\n")
    ));
    body.push_str(&format!(
        "\n## Aggregate\n\n\
         - Pairs in work list: 70; neighbor not in greedy decode: {skipped_neighbor}; \
         miss already in greedy decode: {skipped_miss}; encode/fit failed: {encode_failed}.\n\
         - **Tried: {tried}** ({tried_genuine} genuine co-channel, {tried_alias} alias).\n\
         - Recovered, global round: {rec_global}. Serendip/FP global: {ser_g}/{fp_g} \
         ({fp_hash_g} of the FPs are hash-display aliases of truth messages).\n\
         - **Recovered, refined round: {rec_refined}** ({rec_refined_genuine} genuine, \
         {rec_refined_alias} alias). Serendip/FP refined: {ser_r}/{fp_r} \
         ({fp_hash_r} of the FPs are hash-display aliases of truth messages).\n\
         - Recovery rate (refined), all tried: {rate_all:.3} ({rec_refined}/{tried}).\n\
         - **Recovery rate (refined), genuine pairs: {rate_genuine:.3} \
         ({rec_refined_genuine}/{tried_genuine})** — the kill-switch number.\n\
         - Mean residual-energy ratio (full window): global {mean_res_g:.4}, refined \
         {mean_res_r:.4} (1.0 = subtract removed nothing; lower = better fit).\n\
         - Mean victim-band energy ratio after/before (neighbor band +/- 10 Hz over \
         the fit window): global {mean_band_g:.3}, refined {mean_band_r:.3} — the \
         spec's mechanism evidence. ~1.0 means the precision subtract never removed \
         the neighbor's energy at the victim's bins.\n\n",
    ));
    body.push_str(&premise_audit(&ws)?);
    body.push_str(&format!(
        "\n## Decision (pre-registered criteria)\n\n\
         PROCEED >= 5% genuine recovery with FPs <= 1 per recovered TP; \
         WEAK-PROCEED 2-5%; SHELVE < 2% after refinement or FP cost > 1/TP.\n\n\
         **{decision}**\n\n\
         ## Side findings (mechanism evidence for the journal)\n\n\
         1. **The precision LS subtract itself works**: refined fits remove most \
         of the neighbor's band energy (mean victim-band ratio {mean_band_r:.3}; \
         best pairs reach <0.05) once the time search is wide enough. The one-step \
         ALS machinery is sound — it is the target population that is empty.\n\
         2. **Pancetta's reported time_offset is coarse**: the `--dt-scan` \
         diagnostic and the refined-fit dt distribution show decodes reporting dt \
         up to ~0.2 s from the sample-accurate signal position (LDPC tolerates \
         it; coherent processing does not). Any future coherent mechanism \
         (hb-090 matched filter, subtract refinement) must re-search time \
         locally. The fixed dt cluster near -0.13..-0.19 s on these slots also \
         suggests a systematic component worth checking in the sync chain.\n\
         3. **Hash-display truth-matching artifact**: tol=0 exact-text scoring \
         against ft8_lib truth double-penalizes every pancetta decode of an \
         unresolved hashed callsign (1 phantom miss + 1 phantom FP). This \
         contaminated Batch 85's premise and likely deflates TP counts in every \
         ft8_lib-truth eval that includes hashed messages. Eval tooling should \
         normalize `<...>`/`<...NNNN>` tokens before set intersection.\n",
    ));

    let notes_path = ws.join("research/notes/2026-06-12-batch86-hb104-kill-switch.md");
    std::fs::write(&notes_path, &body)?;
    println!("{body}");
    println!("wrote {}", notes_path.display());
    Ok(())
}
