//! PAN-9 schema-v2 neural-OSD corpus generator.
//!
//! `--tier t0` uses committed fixtures plus local synthetic WAVs and proves
//! the capture pipeline. `--tier t1` recursively mines ~/.pancetta/recordings
//! and is the only tier suitable for training a candidate model.

use anyhow::{Context, Result};
use pancetta_ft8::bp_trajectory_capture as capture;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
struct CorpusRecord {
    schema_version: u32,
    wav: String,
    split_key: String,
    channel_llrs: Vec<f32>,
    trajectory_flat: Vec<f32>,
    final_llrs: Vec<f32>,
    syndrome_counts: Vec<u8>,
    mrb_perm: Option<Vec<u16>>,
    osd_recovered: bool,
    osd_codeword: Option<Vec<u8>>,
    bp_iters_run: u16,
}

fn main() -> Result<()> {
    let mut tier = "t0";
    let mut output = PathBuf::from("research/corpus/neural_osd/schema_v2.jsonl");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--tier" => {
                index += 1;
                tier = args.get(index).context("--tier requires t0 or t1")?;
            }
            "--output" => {
                index += 1;
                output = PathBuf::from(args.get(index).context("--output requires a path")?);
            }
            other => anyhow::bail!("unknown argument {other}"),
        }
        index += 1;
    }
    anyhow::ensure!(matches!(tier, "t0" | "t1"), "--tier must be t0 or t1");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let wavs = wav_pool(&root, tier)?;
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write to a sibling temp file and rename into place only after the
    // full scan, flush, and label checks succeed. A late-file read error,
    // a serialization failure, or the empty-corpus/zero-label checks below
    // would otherwise leave a valid-looking but truncated (prefix-biased)
    // JSONL at `output` — `load_corpus` has no completion marker, so a
    // later training run can't tell a failed mining pass from a real,
    // complete corpus.
    let tmp_output = output.with_extension("jsonl.tmp");
    let mut writer = BufWriter::new(File::create(&tmp_output)?);
    let mut records = 0usize;
    let mut labels = 0usize;
    for wav in &wavs {
        for record in decode(wav, &root)? {
            labels += usize::from(is_trainable_label(&record));
            serde_json::to_writer(&mut writer, &record)?;
            writer.write_all(b"\n")?;
            records += 1;
        }
    }
    writer.flush()?;
    if records == 0 {
        let _ = std::fs::remove_file(&tmp_output);
        anyhow::bail!("tier {tier} wrote zero records from {} WAVs", wavs.len());
    }
    // A record count alone does not make a trainable corpus: `load_corpus` keeps
    // only OSD-recovered rows, so a long T1 run with zero recoveries would exit
    // successfully and publish a corpus that later filters to an empty split.
    // T0 is a pipeline proof, not a training tier, so it only warns.
    if tier == "t1" && labels == 0 {
        let _ = std::fs::remove_file(&tmp_output);
        anyhow::bail!(
            "tier t1 captured {records} records from {} WAVs but zero OSD-recovered \
             labels; the corpus has no trainable positives",
            wavs.len()
        );
    }
    std::fs::rename(&tmp_output, &output)?;
    println!(
        "tier={tier} wavs={} records={records} labeled={labels} output={}",
        wavs.len(),
        output.display()
    );
    if tier == "t0" {
        println!("T0 PIPELINE CHECK ONLY — too little label yield for a shippable model");
        if labels == 0 {
            println!("WARNING: zero OSD-recovered labels — capture pipeline ran but produced no positives");
        }
    }
    Ok(())
}

/// Mirrors `train_rank.py::_iter_samples`'s trainable-row predicate: a
/// record is only kept for training when the OSD-recovered codeword
/// disagrees with the BP hard decision in at least one of the first
/// `K_INFO` (91, the FT8 payload's info-bit count) systematic positions —
/// `osd_recovered && osd_codeword.is_some()` alone (the prior check here)
/// also counts recoveries that only correct parity bits, which
/// `_iter_samples` then discards, so this label-yield count could report a
/// usable T1 corpus that trains on an empty split.
fn is_trainable_label(record: &CorpusRecord) -> bool {
    const K_INFO: usize = 91;
    let Some(codeword) = &record.osd_codeword else {
        return false;
    };
    if !record.osd_recovered || codeword.len() != record.final_llrs.len() {
        return false;
    }
    codeword
        .iter()
        .zip(&record.final_llrs)
        .take(K_INFO)
        .any(|(&bit, &llr)| bit != u8::from(llr < 0.0))
}

fn wav_pool(root: &Path, tier: &str) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if tier == "t0" {
        collect_wavs(
            &root.join("pancetta-ft8/tests/fixtures/wav/basicft8"),
            &mut files,
        )?;
        collect_wavs(
            &root.join("pancetta-ft8/tests/fixtures/wav/wsjt"),
            &mut files,
        )?;
        collect_wavs(&root.join("research/corpus/synth/wavs"), &mut files)?;
    } else {
        // `$HOME` is commonly unset on the native Windows 11 operator
        // target even though a real home directory exists (USERPROFILE);
        // `dirs::home_dir()` resolves it portably instead of requiring the
        // Unix environment variable.
        let home = dirs::home_dir().context("could not resolve the home directory")?;
        collect_wavs(&home.join(".pancetta/recordings"), &mut files)?;
        // `~/.pancetta/recordings` also holds curated fixture subdirectories
        // that are signal-free or synthetic by construction (e.g. noise_1000,
        // the noise-corpus dir the ship-gate's noise manifest points at). Any
        // CRC-valid OSD decode from those is a false positive being fed back
        // in as a training-corpus ground-truth label, poisoning T1's
        // positives even though the production decoder would reject the
        // message. Drop anything under a known non-signal directory.
        files.retain(|path| {
            !path
                .components()
                .any(|component| component.as_os_str() == "noise_1000")
        });
    }
    files.sort();
    files.dedup();
    anyhow::ensure!(!files.is_empty(), "tier {tier} found no WAV files");
    Ok(files)
}

fn collect_wavs(dir: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_wavs(&path, output)?;
        } else if path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("wav"))
        {
            output.push(path);
        }
    }
    Ok(())
}

fn decode(wav: &Path, root: &Path) -> Result<Vec<CorpusRecord>> {
    let mut reader =
        hound::WavReader::open(wav).with_context(|| format!("open {}", wav.display()))?;
    let spec = reader.spec();
    // The live recorder also writes non-decoder-window WAVs into the same
    // directory tree — e.g. `raw_48khz_diagnostic.wav`
    // (pancetta/src/coordinator/audio.rs), 48 kHz stereo, written once
    // after ~90s of operation. Aborting the whole T1 scan over one such
    // file (potentially after already processing every alphabetically
    // earlier ft8_* recording) is worse than just skipping it.
    if spec.channels != 1 || spec.sample_rate != 12_000 {
        eprintln!(
            "skip {} — {} ch @ {} Hz, not 12 kHz mono; not a decoder-window recording",
            wav.display(),
            spec.channels,
            spec.sample_rate
        );
        return Ok(Vec::new());
    }
    let samples: Vec<f32> = match spec.sample_format {
        // Decode integer PCM at its declared width. The recursive T1 scan over
        // ~/.pancetta/recordings routinely meets 24- and 32-bit captures, which
        // `samples::<i16>()` rejects as too wide — aborting the whole corpus run
        // — while narrower PCM would silently get a fixed 16-bit scale.
        hound::SampleFormat::Int => {
            let bits = spec.bits_per_sample;
            anyhow::ensure!(
                (1..=32).contains(&bits),
                "{} declares an unsupported integer bit depth of {bits}",
                wav.display()
            );
            let scale = (1i64 << (bits - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|v| v.map(|v| v as f32 / scale))
                .collect::<std::result::Result<_, _>>()?
        }
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<std::result::Result<_, _>>()?,
    };
    // The live recorder (pancetta/src/coordinator/dsp.rs) writes every
    // active mode's windows into the same ~/.pancetta/recordings directory
    // under the same `ft8_*.wav` naming — there's no mode marker in the
    // filename to filter on. But each mode's window length differs (FT8's
    // WINDOW_SAMPLES; FT4's slot is half that, FT2 shorter still), so a
    // recording physically too short to contain a full FT8 window can only
    // be a non-FT8 capture; padding and decoding it as FT8 anyway lets any
    // CRC-valid collision get accepted as a training label. Skip it rather
    // than abort the whole T1 scan over one wrong-mode file.
    if samples.len() < pancetta_ft8::WINDOW_SAMPLES {
        eprintln!(
            "skip {} — {} samples, shorter than one FT8 window ({}); likely a non-FT8 recording",
            wav.display(),
            samples.len(),
            pancetta_ft8::WINDOW_SAMPLES
        );
        return Ok(Vec::new());
    }
    let mut config = pancetta_ft8::Ft8Config::default();
    // Capture truth at OSD depth 2, not the depth-1 the production A/B arms
    // actually run at. A depth-1-only capture never produces osd_codeword
    // for a BP failure that needs 2+ flips, so _iter_samples silently drops
    // it — conditioning the training set on exactly the easy cases the
    // depth-only baseline (arm B) already solves. This only affects label
    // mining here; eval.rs's A/B arms configure their own osd_depth
    // independently and are unaffected.
    //
    // Deliberately NOT depth 3: `research/experiments/2026-05-24-osd3-followup.md`
    // measured OSD-2 -> OSD-3 adding ~284 novel decodes on hard-200 with
    // ZERO recall gain (~275 estimated CRC-14 collisions, not real
    // recoveries) — OSD-3 is the LDPC codeword-neighborhood width where
    // spurious CRC-14-valid collisions become common enough to matter, and
    // capture has no independent oracle to catch one and reject it before
    // it's recorded as a training label. OSD-2 carries the same class of
    // risk at a much lower, already production-precedented rate. Mining
    // depth-3 truth safely needs validating recovered codewords against an
    // external oracle (synthetic ground truth, or per-WAV jt9 cross-check)
    // — real work, out of scope here; left for a follow-up, not attempted
    // as a quick depth bump.
    config.osd_depth = Some(2);
    config.neural_osd_enabled = false;
    let mut decoder = pancetta_ft8::Ft8Decoder::new(config)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    capture::enable_global();
    // `decode_window` requires exactly WINDOW_SAMPLES (12.64s @ 12kHz), but
    // FT8's actual cycle is a 15s slot — the message occupies the first
    // ~12.64s of each slot, the remaining ~2.36s is dead air/guard time, not
    // more signal. Stepping by WINDOW_SAMPLES (rather than the 15s slot
    // period) drifts the window earlier into each successive slot by that
    // same ~2.36s, so only the first slot of a multi-slot recording is ever
    // presented at its real boundary, and a plain 15s single-slot file gets
    // a second, meaningless decode of its silent tail. Stride by
    // SAMPLES_PER_SLOT (matches `Jt9Decoder`'s own file-chunking in
    // `decoder.rs`) and truncate/pad only the in-slot window to
    // WINDOW_SAMPLES for the actual decode call.
    const SLOT_SECONDS: usize = 15;
    let samples_per_slot = SLOT_SECONDS * pancetta_ft8::SAMPLE_RATE as usize;
    let mut decode_error = None;
    for slot in samples.chunks(samples_per_slot) {
        let mut window = vec![0.0f32; pancetta_ft8::WINDOW_SAMPLES];
        let n = slot.len().min(pancetta_ft8::WINDOW_SAMPLES);
        window[..n].copy_from_slice(&slot[..n]);
        if let Err(error) = decoder.decode_window(&window) {
            decode_error = Some(anyhow::anyhow!(error.to_string()));
            break;
        }
    }
    capture::disable_global();
    if let Some(error) = decode_error {
        return Err(error);
    }
    let relative = wav
        .strip_prefix(root)
        .unwrap_or(wav)
        .to_string_lossy()
        .into_owned();
    Ok(capture::drain_global()
        .into_iter()
        .map(|sample| CorpusRecord {
            schema_version: capture::CAPTURE_SCHEMA_VERSION,
            split_key: relative.clone(),
            wav: relative.clone(),
            channel_llrs: sample.channel_llrs.to_vec(),
            trajectory_flat: sample.trajectory.into_iter().flatten().collect(),
            final_llrs: sample.final_llrs.to_vec(),
            syndrome_counts: sample.syndrome_counts.to_vec(),
            mrb_perm: sample.mrb_perm.map(|value| value.to_vec()),
            osd_recovered: sample.osd_recovered,
            osd_codeword: sample.osd_codeword.map(|value| value.to_vec()),
            bp_iters_run: sample.bp_iters_run,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t0_pool_contains_committed_fixtures() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        assert!(!wav_pool(&root, "t0").unwrap().is_empty());
    }

    #[test]
    fn t0_fixture_runs_capture_pipeline_and_emits_schema_v2_records() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let fixture = wav_pool(&root, "t0").unwrap().remove(0);
        let records = decode(&fixture, &root).unwrap();
        assert!(!records.is_empty(), "fixture capture must not be empty");
        assert!(records
            .iter()
            .all(|record| record.schema_version == capture::CAPTURE_SCHEMA_VERSION));
        assert!(records
            .iter()
            .all(|record| record.syndrome_counts.len() == 174));
    }

    #[test]
    fn schema_v2_record_round_trips_permutation_and_syndrome() {
        let record = CorpusRecord {
            schema_version: 2,
            wav: "x.wav".into(),
            split_key: "20m/20260808/x.wav".into(),
            channel_llrs: vec![0.0; 174],
            trajectory_flat: vec![0.0; 25 * 174],
            final_llrs: vec![0.0; 174],
            syndrome_counts: vec![3; 174],
            mrb_perm: Some((0..174).collect()),
            osd_recovered: true,
            osd_codeword: Some(vec![0; 174]),
            bp_iters_run: 25,
        };
        let decoded: CorpusRecord =
            serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
        assert_eq!(decoded.schema_version, 2);
        assert_eq!(decoded.syndrome_counts, vec![3; 174]);
        assert_eq!(decoded.mrb_perm.unwrap(), (0..174).collect::<Vec<_>>());
    }
}
