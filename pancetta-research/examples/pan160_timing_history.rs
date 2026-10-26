//! PAN-160 paired timing-history experiment. All arms run serially.
use anyhow::Context;
use pancetta_ft8::{
    ap::{ApContext, MyCallAp, RecentCallAp},
    Ft8Config, InMemoryDtHistory,
};
use pancetta_research::synth::{
    add_awgn_2500hz_ref, modulate_message_at, place_in_slot, signal_rms,
};
use pancetta_research::{
    chrono_replay::{
        compare_timing_slots, preflight_timing_history, ChronoReplayManifest, TimingSlotResult,
    },
    DecoderUnderTest, Ft8Decoder,
};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

const TARGET: &str = "K1ABC W1AW -12";
const OTHER: &str = "CQ K2XYZ FN42";
fn context() -> ApContext {
    ApContext {
        my_call: MyCallAp::new("K1ABC"),
        recent_calls: vec![RecentCallAp::new("W1AW", -15.).unwrap()],
        ..Default::default()
    }
}
fn config(enabled: bool) -> Ft8Config {
    Ft8Config {
        ap_dt_history_enabled: enabled,
        max_decode_passes: 1,
        max_candidates: 50,
        max_sync_candidates: 100,
        ldpc_iterations: 30,
        osd_depth: None,
        ..Default::default()
    }
}
fn wrapper(enabled: bool) -> Ft8Decoder {
    Ft8Decoder::with_default_config()
        .with_ap_dt_history(enabled)
        .with_ap_context(context())
        .with_max_passes(1)
        .with_max_candidates(50)
        .with_max_sync_candidates(100)
        .with_ldpc_iterations(30)
        .with_osd_depth(None)
}
fn at(slot: usize) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + slot as u64 * 15)
}
fn audio(seed: u64, snr: f64, dt: f64, noise_only: bool, second: bool) -> anyhow::Result<Vec<f32>> {
    let signal = modulate_message_at(TARGET, 1000.)?;
    let mut samples = if noise_only {
        vec![0.; 180000]
    } else {
        place_in_slot(&signal, dt, 0., 180000)
    };
    if second && !noise_only {
        let other = place_in_slot(&modulate_message_at(OTHER, 1600.)?, 0.8, 0., 180000);
        for (s, o) in samples.iter_mut().zip(other) {
            *s += o * 0.5;
        }
    }
    add_awgn_2500hz_ref(&mut samples, signal_rms(&signal), snr, seed);
    Ok(samples)
}
fn hash(samples: &[f32]) -> String {
    let mut h = Sha256::new();
    for s in samples {
        h.update(s.to_le_bytes());
    }
    format!("{:x}", h.finalize())
}
fn write_wav(path: &Path, samples: &[f32]) -> anyhow::Result<()> {
    let mut w = hound::WavWriter::create(
        path,
        hound::WavSpec {
            channels: 1,
            sample_rate: 12000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )?;
    for s in samples {
        w.write_sample(*s)?;
    }
    w.finalize()?;
    Ok(())
}
fn save(out: &Path, name: &str, value: &serde_json::Value) -> anyhow::Result<()> {
    std::fs::create_dir_all(out)?;
    std::fs::write(out.join(name), serde_json::to_vec_pretty(value)?)?;
    Ok(())
}
fn calibration(out: &Path, fine: bool) -> anyhow::Result<()> {
    let mut records = Vec::new();
    // Calibration seeds are disjoint from held-out session and noise seeds.
    let seeds = if fine { 20..=25 } else { 1..=12 };
    let snrs = if fine {
        vec![-19., -19.5, -20., -20.5, -21.]
    } else {
        vec![-12., -15., -18., -21.]
    };
    let offsets = if fine {
        [0.32, 0.4, 0.48]
    } else {
        [0.28, 0.36, 0.44]
    };
    let filename = if fine {
        "calibration-fine.json"
    } else {
        "calibration.json"
    };
    for seed in seeds {
        for &snr in &snrs {
            for dt in offsets {
                let samples = audio(seed, snr, dt, false, false)?;
                let history = Arc::new(InMemoryDtHistory::default());
                history.record("W1AW", dt, at(0));
                history.record("W1AW", dt, at(1));
                let mut arms = Vec::new();
                for enabled in [false, true] {
                    let mut d = pancetta_ft8::Ft8Decoder::new(config(enabled))?
                        .with_dt_priors(history.clone())
                        .with_dt_prior_time(at(2));
                    let start = Instant::now();
                    let decoded = d.decode_window_with_ap(&samples, &context())?;
                    arms.push(serde_json::json!({"enabled":enabled, "messages":decoded.iter().map(|m| &m.text).collect::<Vec<_>>(),
                        "diagnostics":d.timing_diagnostics(), "elapsed":start.elapsed().as_secs_f64()}));
                }
                let rescued = !arms[0]["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m == TARGET)
                    && arms[1]["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|m| m == TARGET);
                eprintln!(
                    "calibration seed={seed} snr={snr} dt={dt} rescued={rescued} attempts={}",
                    arms[1]["diagnostics"]["ap"]["attempts"]
                );
                records.push(serde_json::json!({"seed":seed,"snr":snr,"dt":dt,"audio_sha256":hash(&samples),"arms":arms,"rescued":rescued}));
                if rescued {
                    save(
                        out,
                        filename,
                        &serde_json::json!({"version":1,"config":config(false),"records":records}),
                    )?;
                    return Ok(());
                }
            }
        }
    }
    save(
        out,
        filename,
        &serde_json::json!({"version":1,"config":config(false),"records":records}),
    )
}
fn portable(out: &Path) -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let mut summaries = Vec::new();
    let mut sessions = Vec::new();
    for varying in [false, true] {
        for seed in if varying {
            1100..1110u64
        } else {
            1000..1010u64
        } {
            let decoders = [wrapper(false), wrapper(true)];
            let mut arms = [Vec::new(), Vec::new()];
            for slot in 0..10 {
                let dt = 0.36
                    + if varying {
                        [0., 0.04, -0.04, 0.08, -0.08][slot % 5]
                    } else {
                        0.
                    };
                let snr = if slot < 2 {
                    -6.
                } else {
                    -19. - (slot % 3) as f64
                };
                let samples = audio(seed * 100 + slot as u64, snr, dt, false, true)?;
                let path = temp.path().join("slot.wav");
                write_wav(&path, &samples)?;
                for arm in 0..2 {
                    let d = &decoders[arm];
                    d.set_slot_time(at(slot));
                    let eligible = d.eligible_timing_calls(&["W1AW".into()], at(slot));
                    let start = Instant::now();
                    let decoded = d.decode_wav(&path)?;
                    arms[arm].push(TimingSlotResult {
                        slot,
                        audio_sha256: hash(&samples),
                        truth: vec![TARGET.into(), OTHER.into()],
                        decoded: decoded
                            .into_iter()
                            .filter(|m| {
                                !pancetta_qso::callsign_continuity::has_high_risk_fp_pattern(
                                    &m.message,
                                )
                            })
                            .map(|m| m.message)
                            .collect(),
                        eligible,
                        elapsed_seconds: start.elapsed().as_secs_f64(),
                        diagnostics: serde_json::to_value(d.timing_diagnostics())?,
                    });
                }
            }
            let summary = compare_timing_slots(&arms[0], &arms[1], 10);
            let name = format!(
                "synthetic-{}-{seed}.json",
                if varying { "moderate" } else { "stable" }
            );
            sessions.push(serde_json::json!({"version":1,"dataset":"synthetic-ground-truth","seed":seed,"varying":varying,
                "generator":{"slots":10,"warmup":2,"target":TARGET,"other":OTHER,"base_dt":0.36,"snr_weak":[-19.,-20.,-21.],"noise_seed":"seed*100+slot","audio_hash":"f32 little-endian samples"},
                "clock":"UNIX 1700000000 + slot*15s","filter":"native acceptance then reject high-risk patterns",
                "configs":[decoders[0].config_snapshot(),decoders[1].config_snapshot()],"arms":arms,"comparison":summary}));
            eprintln!("{name}: {summary}");
            summaries.push(summary);
        }
    }
    save(
        out,
        "synthetic-sessions.json",
        &serde_json::json!({"version":1,"sessions":sessions}),
    )?;
    // Bootstrap whole independent sessions, never individual correlated slots.
    use rand::{RngExt, SeedableRng};
    let deltas: Vec<_> = summaries
        .iter()
        .map(|s| s["recall_delta"].as_f64().unwrap())
        .collect();
    let mut rng = rand::rngs::StdRng::seed_from_u64(0xb007);
    let mut boot: Vec<f64> = (0..1000)
        .map(|_| {
            (0..deltas.len())
                .map(|_| deltas[rng.random_range(0..deltas.len())])
                .sum::<f64>()
                / deltas.len() as f64
        })
        .collect();
    boot.sort_by(f64::total_cmp);
    save(
        out,
        "synthetic-summary.json",
        &serde_json::json!({"version":1,"sessions":summaries,"session_bootstrap_seed":45063,"bootstrap_n":1000,"mean_delta_ci95":[boot[25],boot[974]]}),
    )
}
fn controls(out: &Path) -> anyhow::Result<()> {
    let mut records = Vec::new();
    for kind in [
        "no-context",
        "one-sighting",
        "stale",
        "wrong-sender",
        "shifted-dt",
        "noise-only",
    ] {
        let count = if kind == "noise-only" { 100 } else { 1 };
        for seed in 2000..2000 + count {
            let noise = kind == "noise-only";
            let samples = audio(
                seed,
                -15.,
                if kind == "shifted-dt" { 1.2 } else { 0.36 },
                noise,
                false,
            )?;
            let mut arms = Vec::new();
            for enabled in [false, true] {
                let h = Arc::new(InMemoryDtHistory::default());
                let call = if kind == "wrong-sender" {
                    "K2XYZ"
                } else {
                    "W1AW"
                };
                h.record(call, 0.36, at(0));
                if kind != "one-sighting" {
                    h.record(call, 0.36, at(1));
                }
                let mut d = pancetta_ft8::Ft8Decoder::new(config(enabled))?
                    .with_dt_priors(h)
                    .with_dt_prior_time(if kind == "stale" { at(200) } else { at(2) });
                let ctx = if kind == "no-context" {
                    ApContext::default()
                } else {
                    context()
                };
                let start = Instant::now();
                let messages = d.decode_window_with_ap(&samples, &ctx)?;
                arms.push(serde_json::json!({"enabled":enabled,"messages":messages.iter().map(|m|&m.text).collect::<Vec<_>>(),
                    "diagnostics":d.timing_diagnostics(),"elapsed_seconds":start.elapsed().as_secs_f64()}));
            }
            records.push(serde_json::json!({"kind":kind,"seed":seed,"audio_sha256":hash(&samples),"arms":arms}));
        }
    }
    save(
        out,
        "controls.json",
        &serde_json::json!({"version":1,"config":config(false),"records":records,"noise_seed_range":[2000,2099],"noise_truth":[],"noise_accepted_messages_are_false_positives":true}),
    )
}
fn original(args: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.len() >= 3,
        "usage: pan160_timing_history MANIFEST BASELINES OUTPUT [MEDIA]"
    );
    let mut manifest = ChronoReplayManifest::load(&args[0])?;
    let media = args.get(3).map(PathBuf::from);
    anyhow::ensure!(
        manifest.entries.len() == 33,
        "mini33 requires exactly 33 original slots"
    );
    let problems = preflight_timing_history(&manifest.entries, media.as_deref(), args[1].as_ref());
    let out = PathBuf::from(&args[2]);
    save(
        &out,
        "preflight.json",
        &serde_json::json!({"version":1,"dataset":"mini33-original",
        "status":if problems.is_empty(){"ready"}else{"blocked_missing_inputs"},"slots":manifest.entries.len(),
        "completed_slots":0,"problems":problems,"truth_recovered":null,"warm_slots":null}),
    )?;
    anyhow::ensure!(
        problems.is_empty(),
        "blocked_missing_inputs: {} rejected inputs",
        problems.len()
    );
    for entry in &mut manifest.entries {
        if let Some(dir) = &media {
            entry.wav_path = dir.join(entry.wav_path.file_name().context("missing basename")?);
        }
    }
    for mechanism in ["general", "ap-only"] {
        let mut arms = [Vec::new(), Vec::new()];
        let mut configs = Vec::new();
        for arm in 0..2 {
            let mut d = Ft8Decoder::with_default_config();
            if mechanism == "general" {
                if arm == 1 {
                    d = d
                        .with_dt_history(0.2, 3.)
                        .with_dt_history_freq_window_hz(25.);
                } else {
                    d = d.with_dt_history_control();
                }
            } else {
                d = d.with_ap_dt_history(arm == 1);
            }
            let (d, _) = d.with_chrono_replay(100);
            configs.push(d.config_snapshot());
            for (slot, entry) in manifest.entries.iter().enumerate() {
                let cache: pancetta_research::BaselineCache = serde_json::from_slice(
                    &std::fs::read(Path::new(&args[1]).join(format!("{}.json", entry.wav_sha256)))?,
                )?;
                d.set_slot_time(
                    chrono::DateTime::parse_from_rfc3339(&entry.wav_timestamp)?
                        .with_timezone(&chrono::Utc)
                        .into(),
                );
                let replay_at: SystemTime =
                    chrono::DateTime::parse_from_rfc3339(&entry.wav_timestamp)?
                        .with_timezone(&chrono::Utc)
                        .into();
                let eligible = d.eligible_timing_calls(&d.chrono_replay_snapshot(), replay_at);
                let start = Instant::now();
                let mut decoded = d.decode_wav(&entry.wav_path)?;
                if mechanism == "ap-only" {
                    decoded.retain(|m| {
                        !pancetta_qso::callsign_continuity::has_high_risk_fp_pattern(&m.message)
                    });
                }
                let diag = d.timing_diagnostics();
                arms[arm].push(TimingSlotResult {
                    slot,
                    audio_sha256: entry.wav_sha256.clone(),
                    truth: cache
                        .decodes
                        .into_iter()
                        .map(|m| m.message.trim().to_string())
                        .collect(),
                    decoded: decoded
                        .into_iter()
                        .map(|m| m.message.trim().to_string())
                        .collect(),
                    eligible: if mechanism == "ap-only" {
                        eligible
                    } else if diag.v2_hits > 0 {
                        vec!["near-frequency-hit".into()]
                    } else {
                        vec![]
                    },
                    elapsed_seconds: start.elapsed().as_secs_f64(),
                    diagnostics: serde_json::to_value(diag)?,
                });
            }
        }
        save(
            &out,
            &format!("mini33-{mechanism}.json"),
            &serde_json::json!({"version":1,"dataset":"mini33-original","mechanism":mechanism,
        "clock":if mechanism=="general" {"legacy V2 wall-clock recording, no lookup-time expiry"} else {"manifest slot timestamp"},
        "control_observes_v2_without_filtering":mechanism=="general", "configs":configs,
        "comparison":compare_timing_slots(&arms[0],&arms[1],manifest.entries.len()),"arms":arms}),
        )?;
    }
    Ok(())
}
fn supplemental(out: &Path) -> anyhow::Result<()> {
    let mut arms = [Vec::new(), Vec::new()];
    for arm in 0..2 {
        let (d, _) = Ft8Decoder::with_default_config()
            .with_ap_dt_history(arm == 1)
            .with_chrono_replay(100);
        for (slot, suffix) in ["00", "15", "30", "45"].iter().enumerate() {
            let path = PathBuf::from(format!(
                "pancetta-ft8/tests/fixtures/wav/basicft8/170923_0820{suffix}.wav"
            ));
            let bytes = std::fs::read(&path)?;
            let time: SystemTime =
                chrono::DateTime::parse_from_rfc3339(&format!("2017-09-23T08:20:{suffix}Z"))?
                    .with_timezone(&chrono::Utc)
                    .into();
            d.set_slot_time(time);
            let eligible = d.eligible_timing_calls(&d.chrono_replay_snapshot(), time);
            let start = Instant::now();
            let decoded = d.decode_wav(&path)?;
            arms[arm].push(serde_json::json!({"slot":slot,"path":path,"audio_sha256":format!("{:x}",Sha256::digest(bytes)),
                "eligible":eligible,"decoded":decoded.iter().map(|m|&m.message).collect::<Vec<_>>(),"diagnostics":d.timing_diagnostics(),"elapsed_seconds":start.elapsed().as_secs_f64(),"truth":null}));
        }
    }
    let exercised = arms[1]
        .iter()
        .any(|s| s["diagnostics"]["ap"]["attempts"].as_u64().unwrap_or(0) > 0);
    save(
        out,
        "supplemental.json",
        &serde_json::json!({"version":1,"dataset":"four-slot-supplemental-real",
        "status":if exercised {"measured"} else {"inconclusive_no_eligible_priors"},
        "configs":[Ft8Config::default(),Ft8Config {ap_dt_history_enabled:true,..Default::default()}],
        "clock":"2017-09-23T08:20:00Z + slot*15s", "truth":null,"arms":arms}),
    )
}
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() == 2 {
        let out = Path::new(&args[1]);
        return match args[0].as_str() {
            "calibrate" => calibration(out, false),
            "calibrate-fine" => calibration(out, true),
            "portable" => portable(out),
            "controls" => controls(out),
            "supplemental" => supplemental(out),
            _ => anyhow::bail!("unknown mode"),
        };
    }
    original(&args)
}
