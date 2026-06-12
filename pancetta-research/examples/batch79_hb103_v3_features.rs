//! Batch 79 — hb-103 v3 candidate: add the two validated features to the
//! content score and measure AUC lift honestly (split-half).
//!
//! Features (both validated as standalone signals, never yet combined):
//!   1. `decode_time_into_window` (Diagnostic V: solo AUC 0.695 inverted —
//!      later decode within the window = more FP-ish)
//!   2. Cross-day callsign trust, K>=1 leave-day-out (Batch 76 / hb-246:
//!      P(TP|trusted)=79.3% vs P(TP|none)=48.7% on the 5/30 scan)
//!
//! Method: decode the corpus, compute v2 score + the two feature values
//! per decode, then grid-search additive weights (w_t, w_c) on the
//! even-indexed WAVs and report AUC on the odd-indexed WAVs (and vice
//! versa). The headline number is the mean held-out ΔAUC of best-v3 over
//! v2 — fit and eval never share WAVs.
//!
//! Corpora: hard_200 (comparability with Batches 64/70: v1=v2=0.9150
//! under ft8_lib truth) and the first 200 slots of raw_530_full
//! (realistic traffic). ft8_lib truth for both.
//!
//! Decision rule: held-out ΔAUC > +0.005 on BOTH corpora → proceed to
//! production wiring consideration; one corpus → weak-proceed, note
//! corpus dependence; neither → measured-no-lift, hb-246 stays shelved
//! as a score feature.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch79_hb103_v3_features

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use pancetta_qso::callsign_continuity::{callsigns_in, CallsignContinuityFilter};
use pancetta_qso::content_score::{content_score_v2_from_features, ContentFeatures};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

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
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return HashSet::new();
    };
    v["decodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["message"].as_str().map(|s| s.to_string()))
        .collect()
}

/// Day key (YYYYMMDD) from a recording path like .../ft8_20260530_183045.wav.
fn day_of(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?;
    let rest = name.strip_prefix("ft8_")?;
    let day = rest.get(..8)?;
    day.chars().all(|c| c.is_ascii_digit()).then(|| day.to_string())
}

/// callsign -> set of days it appears in, over ALL ft8_lib truth files.
fn build_call_days(ws: &Path) -> Result<HashMap<String, HashSet<String>>> {
    let mut out: HashMap<String, HashSet<String>> = HashMap::new();
    let dir = ws.join("research/baselines/ft8");
    for entry in std::fs::read_dir(&dir)? {
        let p = entry?.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(txt) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&txt) else {
            continue;
        };
        let Some(day) = v["wav_path"].as_str().and_then(day_of) else {
            continue;
        };
        for d in v["decodes"].as_array().into_iter().flatten() {
            let Some(msg) = d["message"].as_str() else {
                continue;
            };
            for c in callsigns_in(msg) {
                out.entry(c).or_default().insert(day.clone());
            }
        }
    }
    Ok(out)
}

struct Sample {
    wav_idx: usize,
    is_tp: bool,
    v2: f64,
    time_norm: f64,  // decode_time_into_window seconds, 0 if absent
    trust_frac: f64, // fraction of extracted callsigns trusted (K>=1 leave-day-out)
}

fn auc(tp: &[f64], fp: &[f64]) -> f64 {
    if tp.is_empty() || fp.is_empty() {
        return f64::NAN;
    }
    let mut wins = 0.0f64;
    for t in tp {
        for f in fp {
            if t > f {
                wins += 1.0;
            } else if (t - f).abs() < f64::EPSILON {
                wins += 0.5;
            }
        }
    }
    wins / (tp.len() as f64 * fp.len() as f64)
}

fn auc_of(samples: &[&Sample], score: impl Fn(&Sample) -> f64) -> f64 {
    let tp: Vec<f64> = samples.iter().filter(|s| s.is_tp).map(|s| score(s)).collect();
    let fp: Vec<f64> = samples.iter().filter(|s| !s.is_tp).map(|s| score(s)).collect();
    auc(&tp, &fp)
}

fn collect_corpus(
    ws: &Path,
    label: &str,
    entries: &[Value],
    call_days: &HashMap<String, HashSet<String>>,
    trust: &CallsignContinuityFilter,
) -> Result<Vec<Sample>> {
    let cfg = Ft8Config::default();
    let mut samples = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let day = day_of(wav_path);
        let samples_wav = match load_wav(Path::new(wav_path)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let truth = load_ft8lib_truth(ws, sha);
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples_wav)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        for d in &decoded {
            let cf = d.confidence_features.as_ref();
            let feat = ContentFeatures {
                text: &d.text,
                confidence: d.confidence,
                snr_db: d.snr_db,
                time_offset: d.time_offset,
                bp_iterations_used: cf.and_then(|c| c.bp_iterations_used),
                osd_depth_used: cf.and_then(|c| c.osd_depth_used),
                nharderrs: cf.and_then(|c| c.nharderrs),
                min_llr_magnitude: cf.and_then(|c| c.min_llr_magnitude),
                decode_time_frac: None,
            };
            let v2 = content_score_v2_from_features(feat, trust);
            let time_norm = d
                .decode_time_into_window
                .map(|t| t.as_secs_f64())
                .unwrap_or(0.0);
            let calls = callsigns_in(&d.text);
            let trust_frac = if calls.is_empty() {
                0.0
            } else {
                let hits = calls
                    .iter()
                    .filter(|c| {
                        call_days.get(*c).is_some_and(|days| {
                            days.iter().any(|dd| Some(dd) != day.as_ref())
                        })
                    })
                    .count();
                hits as f64 / calls.len() as f64
            };
            samples.push(Sample {
                wav_idx: i,
                is_tp: truth.contains(&d.text),
                v2,
                time_norm,
                trust_frac,
            });
        }
        if (i + 1) % 50 == 0 {
            eprintln!("    [{label} {}/{}]", i + 1, entries.len());
        }
    }
    Ok(samples)
}

fn evaluate(label: &str, samples: &[Sample], body: &mut String) {
    // Standardize time_norm to [0,1] by corpus max so weights are comparable.
    let tmax = samples
        .iter()
        .map(|s| s.time_norm)
        .fold(0.0f64, f64::max)
        .max(1e-9);

    // Production-feasible alternative: normalize by the SLOT's own max
    // decode time (the score consumer sees the whole slot; no
    // hardware-dependent absolute scale needed).
    let mut slot_max: HashMap<usize, f64> = HashMap::new();
    for s in samples {
        let e = slot_max.entry(s.wav_idx).or_insert(1e-9);
        if s.time_norm > *e {
            *e = s.time_norm;
        }
    }
    let slot_frac = |s: &Sample| s.time_norm / slot_max[&s.wav_idx];

    let grid: Vec<f64> = vec![
        -8.0, -4.0, -2.0, -1.0, -0.5, -0.25, -0.1, 0.0, 0.1, 0.25, 0.5, 1.0,
    ];
    let halves: [(Vec<&Sample>, Vec<&Sample>); 2] = {
        let even: Vec<&Sample> = samples.iter().filter(|s| s.wav_idx % 2 == 0).collect();
        let odd: Vec<&Sample> = samples.iter().filter(|s| s.wav_idx % 2 == 1).collect();
        [(even.clone(), odd.clone()), (odd, even)]
    };

    let mut held_out_v2 = Vec::new();
    let mut held_out_v3 = Vec::new();
    let mut held_out_v3s = Vec::new();
    let mut chosen: Vec<(f64, f64)> = Vec::new();
    let mut chosen_s: Vec<f64> = Vec::new();
    for (train, test) in &halves {
        let mut best = (0.0f64, 0.0f64, f64::MIN);
        for &wt in &grid {
            for &wc in &grid {
                let a = auc_of(train, |s| s.v2 + wt * (s.time_norm / tmax) + wc * s.trust_frac);
                if a > best.2 {
                    best = (wt, wc, a);
                }
            }
        }
        let (wt, wc, _) = best;
        chosen.push((wt, wc));
        held_out_v2.push(auc_of(test, |s| s.v2));
        held_out_v3.push(auc_of(test, |s| s.v2 + wt * (s.time_norm / tmax) + wc * s.trust_frac));

        // Slot-max-normalized variant, time feature only (trust is dead).
        let mut best_s = (0.0f64, f64::MIN);
        for &wt in &grid {
            let a = auc_of(train, |s| s.v2 + wt * slot_frac(s));
            if a > best_s.1 {
                best_s = (wt, a);
            }
        }
        chosen_s.push(best_s.0);
        held_out_v3s.push(auc_of(test, |s| s.v2 + best_s.0 * slot_frac(s)));
    }

    let all: Vec<&Sample> = samples.iter().collect();
    let solo_time = auc_of(&all, |s| -s.time_norm); // inverted per Diagnostic V
    let solo_trust = auc_of(&all, |s| s.trust_frac);
    let v2_all = auc_of(&all, |s| s.v2);
    let n_tp = samples.iter().filter(|s| s.is_tp).count();
    let n_fp = samples.len() - n_tp;
    let mv2 = (held_out_v2[0] + held_out_v2[1]) / 2.0;
    let mv3 = (held_out_v3[0] + held_out_v3[1]) / 2.0;

    let mv3s = (held_out_v3s[0] + held_out_v3s[1]) / 2.0;
    body.push_str(&format!(
        "## {label}\n\n\
         {n_tp} TPs / {n_fp} FPs. Full-set AUCs: v2={v2_all:.4}, solo decode_time (inverted)={solo_time:.4}, solo trust_frac={solo_trust:.4}\n\n\
         | Fold | v2 held-out AUC | best-v3 (corpus-max norm) | (w_time, w_trust) | v3 slot-max norm | w_time |\n|---|---:|---:|---|---:|---|\n\
         | even→odd | {:.4} | {:.4} | ({:+.2}, {:+.2}) | {:.4} | {:+.2} |\n\
         | odd→even | {:.4} | {:.4} | ({:+.2}, {:+.2}) | {:.4} | {:+.2} |\n\n\
         **Mean held-out ΔAUC: corpus-max v3 {:+.4} | slot-max v3 {:+.4}**\n\n",
        held_out_v2[0], held_out_v3[0], chosen[0].0, chosen[0].1, held_out_v3s[0], chosen_s[0],
        held_out_v2[1], held_out_v3[1], chosen[1].0, chosen[1].1, held_out_v3s[1], chosen_s[1],
        mv3 - mv2, mv3s - mv2,
    ));
    println!(
        "{label}: held-out ΔAUC corpus-max {:+.4} | slot-max {:+.4} (v2 {mv2:.4})",
        mv3 - mv2,
        mv3s - mv2
    );
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    eprintln!("building cross-day trust DB from all ft8_lib truth files…");
    let call_days = build_call_days(&ws)?;
    eprintln!("trust DB: {} unique callsigns", call_days.len());

    let mut body = String::from(
        "# Batch 79 — hb-103 v3 features (decode_time + cross-day trust), split-half AUC\n\n",
    );

    for (label, manifest_name, take) in [
        ("hard_200", "hard_200.manifest.json", usize::MAX),
        ("raw_530 subset-200", "raw_530_full.manifest.json", 200usize),
    ] {
        let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
            ws.join("research/corpus/curated/ft8").join(manifest_name),
        )?)?;
        let entries: Vec<Value> = manifest["entries"]
            .as_array()
            .context("entries")?
            .iter()
            .take(take)
            .cloned()
            .collect();
        eprintln!("---- {label}: {} entries ----", entries.len());

        // v2's existing trust input: continuity filter seeded from the
        // corpus's own truth callsigns (mirrors Batch 70 exactly).
        let mut trust = CallsignContinuityFilter::new(4000);
        let mut all_calls: HashSet<String> = HashSet::new();
        for entry in &entries {
            let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
            for t in load_ft8lib_truth(&ws, sha) {
                for c in callsigns_in(&t) {
                    all_calls.insert(c);
                }
            }
        }
        trust.extend_from_iter(all_calls.iter());

        let samples = collect_corpus(&ws, label, &entries, &call_days, &trust)?;
        evaluate(label, &samples, &mut body);
    }

    let notes_path = ws.join("research/notes/2026-06-11-batch79-hb103-v3.md");
    std::fs::write(&notes_path, &body)?;
    println!("wrote {}", notes_path.display());
    Ok(())
}
