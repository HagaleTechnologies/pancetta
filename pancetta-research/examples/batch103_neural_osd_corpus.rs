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
    let mut writer = BufWriter::new(File::create(&output)?);
    let mut records = 0usize;
    let mut labels = 0usize;
    for wav in &wavs {
        for record in decode(wav, &root)? {
            labels += usize::from(record.osd_recovered && record.osd_codeword.is_some());
            serde_json::to_writer(&mut writer, &record)?;
            writer.write_all(b"\n")?;
            records += 1;
        }
    }
    writer.flush()?;
    anyhow::ensure!(
        records > 0,
        "tier {tier} wrote zero records from {} WAVs",
        wavs.len()
    );
    println!(
        "tier={tier} wavs={} records={records} labeled={labels} output={}",
        wavs.len(),
        output.display()
    );
    if tier == "t0" {
        println!("T0 PIPELINE CHECK ONLY — too little label yield for a shippable model");
    }
    Ok(())
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
        let home = std::env::var_os("HOME").context("HOME is unset")?;
        collect_wavs(
            &PathBuf::from(home).join(".pancetta/recordings"),
            &mut files,
        )?;
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
    anyhow::ensure!(
        spec.channels == 1 && spec.sample_rate == 12_000,
        "{} is not 12 kHz mono",
        wav.display()
    );
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|v| v.map(|v| v as f32 / 32768.0))
            .collect::<std::result::Result<_, _>>()?,
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<std::result::Result<_, _>>()?,
    };
    let mut config = pancetta_ft8::Ft8Config::default();
    config.osd_depth = Some(1);
    config.neural_osd_enabled = false;
    let mut decoder = pancetta_ft8::Ft8Decoder::new(config)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    capture::enable_local();
    decoder
        .decode_window(&samples)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    capture::disable_local();
    let relative = wav
        .strip_prefix(root)
        .unwrap_or(wav)
        .to_string_lossy()
        .into_owned();
    Ok(capture::drain_local()
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
