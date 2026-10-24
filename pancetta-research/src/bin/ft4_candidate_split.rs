//! Classify jt9-truth misses at the native FT4 candidate/decode boundary.
use anyhow::{bail, ensure, Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder, Protocol};
use pancetta_research::{
    candidate_split::*,
    synth::{SynthEntry, SynthManifest},
    Mode,
};
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

fn observe(
    workspace: &Path,
    baseline: &Path,
    e: &SynthEntry,
    config: &Ft8Config,
    cap: Option<usize>,
) -> Result<(WavObservation, String)> {
    let path = workspace.join(&e.wav_path);
    let bytes = std::fs::read(&path)?;
    let sha = format!("{:x}", Sha256::digest(&bytes));
    let (jt9_recovered, identity) = jt9_recovered_from_cache(baseline, &sha, &e.encoded_message)?;
    // Same PCM scaling as src/decoder.rs's native WAV loader.
    let mut reader = hound::WavReader::new(std::io::Cursor::new(bytes))?;
    let spec = reader.spec();
    ensure!(
        spec.channels == 1
            && spec.sample_rate == 12000
            && spec.bits_per_sample == 16
            && spec.sample_format == hound::SampleFormat::Int,
        "expected 12 kHz mono 16-bit PCM"
    );
    let samples = reader
        .samples::<i16>()
        .map(|s| s.map(|v| v as f32 / 32768.0))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut decoder = Ft8Decoder::new(config.clone())?;
    let (messages, records) = decoder.decode_window_with_candidate_dump(&samples)?;
    let pancetta_recovered = messages.iter().any(|d| d.text.contains(&e.encoded_message));
    let truth = TruthPosition::from_ft4_synth_entry(e);
    let cap_lifted = if let Some(cap) = cap.filter(|_| {
        jt9_recovered
            && !pancetta_recovered
            && classify_miss(&truth, &records, PRIMARY_GATE) == MissLabel::NoCandidateProduced
    }) {
        let mut decoder = Ft8Decoder::new(Ft8Config {
            max_sync_candidates: cap,
            ..config.clone()
        })?;
        let (messages, records) = decoder.decode_window_with_candidate_dump(&samples)?;
        Some(CapLiftedRecheck::from_records(
            cap,
            &truth,
            &records,
            messages.iter().any(|d| d.text.contains(&e.encoded_message)),
        ))
    } else {
        None
    };
    Ok((
        WavObservation {
            wav_path: e.wav_path.display().to_string(),
            wav_sha256: sha,
            snr_db: e.snr_db,
            message: e.encoded_message.clone(),
            truth,
            jt9_recovered,
            pancetta_recovered,
            records,
            max_sync_candidates: config.max_sync_candidates,
            cap_lifted,
        },
        identity,
    ))
}
fn main() -> Result<()> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("workspace root")?;
    let mut manifest = PathBuf::from("research/corpus/synth/manifests/ft4_clean.manifest.json");
    let mut baseline = PathBuf::from("research/baselines/ft4");
    let mut output = None;
    let mut cap = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            println!("Usage: ft4-candidate-split [--manifest PATH] [--baseline-dir PATH] --output PATH [--cap-lifted-recheck N]");
            return Ok(());
        }
        match arg.as_str() {
            "--manifest" => manifest = args.next().context("--manifest requires PATH")?.into(),
            "--baseline-dir" => {
                baseline = args.next().context("--baseline-dir requires PATH")?.into()
            }
            "--output" => {
                output = Some(PathBuf::from(
                    args.next().context("--output requires PATH")?,
                ))
            }
            "--cap-lifted-recheck" => {
                cap = Some(
                    args.next()
                        .context("--cap-lifted-recheck requires N")?
                        .parse::<usize>()?,
                )
            }
            _ => bail!("unknown argument {arg}"),
        }
    }
    let output = workspace.join(output.context("--output is required")?);
    let manifest = workspace.join(manifest);
    let baseline_path = workspace.join(&baseline);
    let m = SynthManifest::load(&manifest)?;
    ensure!(
        m.config.mode == Mode::Ft4,
        "ft4-candidate-split requires an FT4 manifest"
    );
    ensure!(!m.entries.is_empty(), "FT4 manifest is empty");
    let config = Ft8Config {
        protocol: Protocol::Ft4,
        ..Default::default()
    };
    if let Some(n) = cap {
        ensure!(
            n > config.max_sync_candidates,
            "--cap-lifted-recheck must exceed the default cap {}",
            config.max_sync_candidates
        );
    }
    let done = AtomicUsize::new(0);
    let results: Vec<_> = m
        .entries
        .par_iter()
        .map(|e| {
            let result = observe(workspace, &baseline_path, e, &config, cap)
                .with_context(|| format!("WAV {}", e.wav_path.display()));
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(50) || n == m.entries.len() {
                eprintln!("processed {n}/{} WAVs", m.entries.len());
            }
            result
        })
        .collect::<Result<Vec<_>>>()?;
    let (observations, identities): (Vec<_>, Vec<_>) = results.into_iter().unzip();
    let truth = TruthProvenance {
        source: "jt9".into(),
        predicate: "jt9 decode text contains manifest encoded_message".into(),
        baseline_dir: baseline.display().to_string(),
        decoder_identities: identities
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    };
    let corpus = synth_corpus_provenance("synth-ft4", workspace, &manifest)?;
    let score = CandidateSplitScorecard::build(
        &observations,
        DecoderProvenance::native(&config),
        truth,
        corpus,
    )?;
    score.save(&output)?;
    let score = CandidateSplitScorecard::load(&output)?;
    eprintln!(
        "pass-0 candidate list at cap: {}/{} WAVs",
        observations
            .iter()
            .filter(
                |o| o.records.iter().filter(|c| c.pass == 0).count() == config.max_sync_candidates
            )
            .count(),
        observations.len()
    );
    println!("PAN-154 FT4 candidate split: native decoder, Ft8Config::default() + Protocol::Ft4, unlimited budget");
    println!(
        "corpus {} ({} WAVs), truth = jt9, gate +/-10.5 Hz / +/-0.096 s\n",
        score.corpus.manifest, score.totals.wavs
    );
    println!("{}\n\n{}", score.summary_table(), score.total_line());
    let t = &score.totals;
    println!("sensitivity gate +/-20.9 Hz / +/-0.16 s: no candidate produced {}, candidate produced, decode failed {} ({} labels changed)",
        t.sensitivity_no_candidate_produced,t.sensitivity_candidate_produced_decode_failed,t.labels_changed_by_sensitivity_gate);
    if let Some(c) = &score.cap_lifted_recheck {
        println!("cap-lifted recheck (max_sync_candidates={}) of {} no-candidate misses: {} present above min_sync_score, cut by the max_sync_candidates cap; {} absent above min_sync_score; {} truth recovered",
            c.max_sync_candidates,c.rechecked,c.present_cut_by_cap,c.absent,c.truth_recovered);
        if c.inconclusive > 0 {
            println!("cap-lifted recheck inconclusive for {} misses: the raised cap still binds; raise --cap-lifted-recheck to determine absence above min_sync_score", c.inconclusive);
        }
    }
    let g = &score.gate_calibration;
    println!("gate calibration: {} recovered truths, nearest candidate max |df| {:.2} Hz, max |dt| {:.3} s",g.recovered_truths,g.max_abs_df_hz,g.max_abs_dt_s);
    let n = &score.non_jt9_truth_misses;
    println!("pancetta-only recoveries (jt9 missed): {}; manifest misses outside jt9 truth: {} ({} no candidate produced, {} candidate produced, decode failed)",
        t.pancetta_only_recoveries,n.total,n.no_candidate_produced,n.candidate_produced_decode_failed);
    Ok(())
}
