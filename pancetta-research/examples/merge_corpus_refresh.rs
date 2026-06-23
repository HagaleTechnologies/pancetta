//! merge_corpus_refresh — build the refreshed hard_200 and new wild_100
//! manifests from the all_wavs_scored.json produced by
//! `score_all_today` + the existing hard_200 manifest.
//!
//! Steps:
//!   1. Read research/corpus/surveys/2026-05-30/all_wavs_scored.json
//!   2. Read research/corpus/curated/ft8/hard_200.manifest.json
//!   3. today_top100 = top 100 of all_wavs_scored by interest_score
//!   4. existing_top100 = top 100 of existing manifest by interest_score
//!      (the existing manifest already stores interest_score per entry)
//!   5. merged = today_top100 ++ existing_top100, dedup by wav_sha256
//!      (today's WAVs win on collision — should never happen since today's
//!      WAVs are a different recording dir prefix, but defensive)
//!   6. Archive the OLD hard_200.manifest.json under
//!      `history/hard_200.<generated_at_date>.manifest.json`
//!   7. Write the new hard_200.manifest.json (overwriting in place)
//!   8. Build wild_100 from all_wavs_scored using stratified-by-hour
//!      sampling with a deterministic seed (rand StdRng), write
//!      wild_100.manifest.json.
//!
//! Intermediate outputs:
//!   - research/corpus/surveys/2026-05-30/today_top100.entries.json
//!   - research/corpus/surveys/2026-05-30/existing_top100.entries.json
//!
//! Run:
//!   cargo run --release -p pancetta-research --example merge_corpus_refresh

use anyhow::Context;
use chrono::Utc;
use pancetta_research::curated::{CuratedEntry, CuratedManifest, ScoreBreakdown};
use rand::seq::SliceRandom;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ScoredWav {
    wav_path: String,
    wav_sha256: String,
    file_bytes: u64,
    interest_score: f64,
    score_breakdown: ScoreBreakdown,
    slot_hhmmss: String,
    error: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf();
    let survey_dir = workspace.join("research/corpus/surveys/2026-05-30");
    let curated_dir = workspace.join("research/corpus/curated/ft8");
    let history_dir = curated_dir.join("history");
    std::fs::create_dir_all(&history_dir)?;

    // === Step 1: Load all_wavs_scored.json. ===
    let scored_path = survey_dir.join("all_wavs_scored.json");
    let scored_text = std::fs::read_to_string(&scored_path)
        .with_context(|| format!("reading {}", scored_path.display()))?;
    let mut scored: Vec<ScoredWav> = serde_json::from_str(&scored_text)?;
    let n_scored_total = scored.len();
    scored.retain(|s| s.error.is_none() && !s.interest_score.is_nan() && !s.wav_sha256.is_empty());
    println!(
        "Loaded {} scored WAVs ({} after error filter)",
        n_scored_total,
        scored.len()
    );
    scored.sort_by(|a, b| {
        b.interest_score
            .partial_cmp(&a.interest_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // === Step 2: Load existing hard_200 manifest. ===
    let hard_200_path = curated_dir.join("hard_200.manifest.json");
    let existing_manifest = CuratedManifest::load(&hard_200_path)?;
    let existing_date_iso = existing_manifest.generated_at.clone();
    println!(
        "Loaded existing hard_200 manifest: {} entries, generated_at={}",
        existing_manifest.entries.len(),
        existing_date_iso,
    );

    // === Step 3: today_top100 ===
    let today_top100: Vec<CuratedEntry> = scored
        .iter()
        .take(100)
        .map(|s| CuratedEntry {
            wav_path: PathBuf::from(&s.wav_path),
            wav_sha256: s.wav_sha256.clone(),
            interest_score: s.interest_score,
            score_breakdown: s.score_breakdown.clone(),
        })
        .collect();
    println!(
        "today_top100: scores [{:.3} .. {:.3}], decode_count [{} .. {}]",
        today_top100.last().map(|e| e.interest_score).unwrap_or(0.0),
        today_top100
            .first()
            .map(|e| e.interest_score)
            .unwrap_or(0.0),
        today_top100
            .iter()
            .map(|e| e.score_breakdown.pancetta_decode_count)
            .min()
            .unwrap_or(0),
        today_top100
            .iter()
            .map(|e| e.score_breakdown.pancetta_decode_count)
            .max()
            .unwrap_or(0),
    );
    std::fs::write(
        survey_dir.join("today_top100.entries.json"),
        serde_json::to_string_pretty(&today_top100)?,
    )?;

    // === Step 4: existing_top100 ===
    let mut existing_sorted = existing_manifest.entries.clone();
    existing_sorted.sort_by(|a, b| {
        b.interest_score
            .partial_cmp(&a.interest_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let existing_top100: Vec<CuratedEntry> = existing_sorted.into_iter().take(100).collect();
    println!(
        "existing_top100: scores [{:.3} .. {:.3}]",
        existing_top100
            .last()
            .map(|e| e.interest_score)
            .unwrap_or(0.0),
        existing_top100
            .first()
            .map(|e| e.interest_score)
            .unwrap_or(0.0),
    );
    std::fs::write(
        survey_dir.join("existing_top100.entries.json"),
        serde_json::to_string_pretty(&existing_top100)?,
    )?;

    // === Step 5: merge dedup by wav_sha256 (today wins on collision). ===
    let mut merged: Vec<CuratedEntry> = Vec::with_capacity(200);
    let mut seen: HashSet<String> = HashSet::new();
    let mut dedup_count = 0usize;
    for e in today_top100.iter().chain(existing_top100.iter()) {
        if seen.insert(e.wav_sha256.clone()) {
            merged.push(e.clone());
        } else {
            dedup_count += 1;
        }
    }
    println!(
        "Merged hard_200: {} entries, {} deduplications",
        merged.len(),
        dedup_count
    );

    // === Step 6: Archive old manifest. ===
    // Extract YYYY-MM-DD from existing generated_at ISO 8601.
    let archive_date = existing_date_iso.split('T').next().unwrap_or("unknown");
    let archive_path = history_dir.join(format!("hard_200.{archive_date}.manifest.json"));
    if !archive_path.exists() {
        std::fs::copy(&hard_200_path, &archive_path)?;
        println!("Archived old hard_200 → {}", archive_path.display());
    } else {
        println!(
            "Archive exists at {}, skipping copy",
            archive_path.display()
        );
    }

    // === Step 7: Write new hard_200 manifest. ===
    let new_hard_200 = CuratedManifest {
        schema_version: CuratedManifest::CURRENT_SCHEMA_VERSION,
        label: "hard_200".to_string(),
        generated_at: Utc::now().to_rfc3339(),
        scoring_decoder: existing_manifest.scoring_decoder.clone(),
        entries: merged,
    };
    new_hard_200.save(&hard_200_path)?;
    println!(
        "Wrote new hard_200.manifest.json: {} entries",
        new_hard_200.entries.len()
    );

    // === Step 8: wild_100 stratified-by-hour from today's WAVs. ===
    // Strategy: group scored entries by hour (parsed from slot_hhmmss[0..2]).
    // Allocate per-hour count proportional to that hour's share of total.
    // Fix shortfall by drawing extras from the largest hour.
    let mut by_hour: BTreeMap<String, Vec<ScoredWav>> = BTreeMap::new();
    for s in scored.iter() {
        if s.slot_hhmmss.len() < 2 {
            continue;
        }
        let hour = s.slot_hhmmss[..2].to_string();
        by_hour.entry(hour).or_default().push(s.clone());
    }
    let total_in_hours: usize = by_hour.values().map(|v| v.len()).sum();
    println!(
        "wild_100 stratification: {} hours, {} WAVs total in hours",
        by_hour.len(),
        total_in_hours
    );

    let target = 100;
    let mut quotas: BTreeMap<String, usize> = BTreeMap::new();
    let mut allocated = 0usize;
    for (h, v) in by_hour.iter() {
        let q = ((v.len() as f64 / total_in_hours as f64) * target as f64).round() as usize;
        let q = q.min(v.len());
        quotas.insert(h.clone(), q);
        allocated += q;
    }
    // Fix shortfall / overshoot.
    while allocated < target {
        // Add one to the hour with the largest remaining headroom.
        let pick = by_hour
            .iter()
            .map(|(h, v)| (h.clone(), v.len(), quotas.get(h).copied().unwrap_or(0)))
            .filter(|(_, total, used)| used < total)
            .max_by_key(|(_, total, used)| total - used)
            .map(|(h, _, _)| h);
        if let Some(h) = pick {
            *quotas.entry(h).or_insert(0) += 1;
            allocated += 1;
        } else {
            break;
        }
    }
    while allocated > target {
        // Remove from the hour with the largest quota.
        let pick = quotas
            .iter()
            .filter(|(_, q)| **q > 0)
            .max_by_key(|(_, q)| **q)
            .map(|(h, _)| h.clone());
        if let Some(h) = pick {
            *quotas.entry(h).or_insert(0) -= 1;
            allocated -= 1;
        } else {
            break;
        }
    }
    println!(
        "wild_100 hour quotas: {:?} (sum={})",
        quotas,
        quotas.values().sum::<usize>()
    );

    // Deterministic seed so manifest is reproducible.
    let mut rng = rand::rngs::StdRng::seed_from_u64(2026_05_31);
    let mut wild_entries: Vec<CuratedEntry> = Vec::with_capacity(100);
    for (h, hour_wavs) in by_hour.iter() {
        let q = quotas.get(h).copied().unwrap_or(0);
        if q == 0 {
            continue;
        }
        let mut shuffled = hour_wavs.clone();
        shuffled.shuffle(&mut rng);
        for s in shuffled.iter().take(q) {
            wild_entries.push(CuratedEntry {
                wav_path: PathBuf::from(&s.wav_path),
                wav_sha256: s.wav_sha256.clone(),
                interest_score: s.interest_score,
                score_breakdown: s.score_breakdown.clone(),
            });
        }
    }
    println!("wild_100: {} entries", wild_entries.len());

    let wild_manifest = CuratedManifest {
        schema_version: CuratedManifest::CURRENT_SCHEMA_VERSION,
        label: "wild_100".to_string(),
        generated_at: Utc::now().to_rfc3339(),
        scoring_decoder: existing_manifest.scoring_decoder.clone(),
        entries: wild_entries,
    };
    let wild_path = curated_dir.join("wild_100.manifest.json");
    wild_manifest.save(&wild_path)?;
    println!("Wrote {}", wild_path.display());

    Ok(())
}
