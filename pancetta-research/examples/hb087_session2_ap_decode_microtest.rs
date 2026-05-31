//! hb-087 Session 2 — per-truth AP-decode micro-test (V3 doctrine refinement).
//!
//! Spawned 2026-05-31 as the Session 2 → Session 3 kill-switch. Session 1
//! measured 23.6% callsign coverage on missed truths in the top-20 worst
//! hard-200 WAVs (above the 20% PROCEED gate). But coverage alone
//! doesn't imply *decodability* — that's the V3 SHELVE doctrine.
//! This micro-test asks the next question:
//!
//!   When a prior-covered missed truth has its callsign injected as an
//!   AP hint, does the production AP path rescue it?
//!
//! ## Method
//!
//! 1. Decode top-20 hard-200 WAVs with production config (AP off).
//!    Collect the set of decoded messages per WAV. Extract the
//!    `recent_this_wav` callsign set from those decodes (mirrors
//!    Session 1 + pancetta-qso's rolling-window aggregation).
//! 2. Build a `CallsignPriorSet` per WAV with operator=K5ARH +
//!    recent_this_wav + bundled-common.
//! 3. Load each WAV's jt9 baseline truth. Identify missed truths
//!    (truth message not present in production decodes).
//! 4. For each missed truth, extract its callsigns and check if ANY
//!    of them is in `iter_unique(64)`. If so, it's "prior-covered" —
//!    a Session 3 candidate.
//! 5. Pick the FIRST 10 prior-covered missed truths across all WAVs
//!    (or all if fewer than 10 are available).
//! 6. For each picked truth:
//!    a. Build a singleton `ApContext { recent_calls: [truth_callsign] }`.
//!    This is the SAME ApContext structure the production AP path uses,
//!    so we exercise both AP1 (called-position) and AP2 (caller-position)
//!    internally via the existing `par_try_ldpc_with_recent_only` /
//!    `par_try_ldpc_with_ap` paths — both inject AP bits at the right
//!    positions for the prior callsign.
//!    b. Re-decode the WAV with `decode_window_with_ap(samples, &ctx)`.
//!    c. Check whether the truth message appears in the AP-on decode set
//!    but NOT in the AP-off baseline.
//!    d. Record the source-of-prior (operator / recent / bundled) so we
//!    can see which source carried the rescue.
//! 7. Report N/10. **Kill switch: ≥3/10 = PROCEED to Session 3.
//!    <3/10 = SHELVE.**
//!
//! ## Faithfulness note (relative to the spec's idealised test)
//!
//! The original Session 2 spec envisioned extracting LLRs at the truth's
//! known (freq_bin, time_step) directly from the post-multipass residual
//! and running LDPC manually. That requires touching pancetta-ft8
//! decoder internals (residual spectrogram is a private struct), which
//! is out of scope for Session 2 ("DO NOT touch production decoder").
//!
//! Instead we exercise the production AP path end-to-end:
//! `decode_window_with_ap` already runs AP-constrained LDPC against the
//! residual after multipass + V1 (see `par_try_ldpc_with_recent_only`
//! at pancetta-ft8/src/decoder.rs:~3715, where the my_call-less recent-
//! caller path injects each recent callsign at both bits 0-27 and bits
//! 28-55, runs LDPC + CRC + plausibility, and returns matches).
//!
//! Differences vs the idealised test:
//!   - We let Costas sync_search choose positions, not the truth's
//!     known coordinates. So the test does NOT fully bypass Costas;
//!     hb-087's full mechanism (which DOES bypass Costas at known
//!     residual positions) might rescue MORE truths than this test
//!     measures. The micro-test result is therefore a LOWER BOUND on
//!     the Session 3 mechanism's potential.
//!   - We exercise the FULL ApContext path (with my_call=None,
//!     active_qso=None, recent_calls=singleton), which exactly matches
//!     how Session 3's `callsign_prior_residual_pass` would invoke AP
//!     injection per-prior-callsign per-position.
//!
//! This means the kill-switch interpretation is:
//!   ≥3/10 rescue → AP+LDPC machinery can recover prior-covered
//!     marginal residuals. Session 3's residual-position enumeration
//!     should extend this capability to truths Costas misses.
//!   <3/10 rescue → AP injection itself doesn't rescue these
//!     residuals. Session 3's mechanism would inherit this weakness;
//!     SHELVE.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example hb087_session2_ap_decode_microtest

use anyhow::Context;
use pancetta_ft8::ap::{ApContext, RecentCallAp};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use pancetta_research::callsign_priors::{CallsignPriorSet, PriorSourceMask};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

const OPERATOR_CALLSIGN: &str = "K5ARH";
const PRIOR_SET_CAP: usize = 64;
const MICROTEST_N_TARGET: usize = 10;
const PROCEED_THRESHOLD: usize = 3;
const SLOT_S: f64 = 15.0;
/// Cap picks per WAV so the micro-test sample isn't dominated by a single
/// noisy WAV. With 20 WAVs available and N=10 target, cap=1 is the
/// tightest spread (one truth per WAV until 10 WAVs are exhausted); we
/// fall back to cap=2 if fewer than 10 WAVs contribute prior-covered
/// truths.
const PER_WAV_PICK_CAP: usize = 1;
const PER_WAV_PICK_CAP_FALLBACK: usize = 2;

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

fn load_wav(path: &PathBuf) -> anyhow::Result<Vec<f32>> {
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

/// Bare-callsign tokens from an FT8 message. Mirrors
/// `pancetta-qso::callsign_continuity::callsigns_in` (also inlined in
/// Session 1's diagnostic).
fn callsigns_in(message: &str) -> Vec<String> {
    let upper = message.to_uppercase();
    let tokens: Vec<&str> = upper.split_whitespace().collect();
    let mut out = Vec::new();
    let mut idx = 0;
    if idx < tokens.len() && tokens[idx] == "CQ" {
        idx += 1;
        if idx < tokens.len() && is_cq_modifier(tokens[idx]) {
            idx += 1;
        }
    }
    for t in tokens.iter().skip(idx).take(2) {
        if looks_like_callsign(t) {
            let bare = t.split('/').next().unwrap_or(t);
            out.push(bare.to_string());
        }
    }
    out
}

fn is_cq_modifier(t: &str) -> bool {
    matches!(t, "DX" | "NA" | "SA" | "EU" | "AS" | "AF" | "OC" | "QRP")
        || t.chars().all(|c| c.is_ascii_digit())
        || (t.len() <= 3 && t.chars().all(|c| c.is_ascii_alphabetic()))
}

fn looks_like_callsign(t: &str) -> bool {
    let len = t.len();
    if !(3..=10).contains(&len) {
        return false;
    }
    let mut has_digit = false;
    let mut has_alpha = false;
    for c in t.chars() {
        if c.is_ascii_digit() {
            has_digit = true;
        } else if c.is_ascii_alphabetic() {
            has_alpha = true;
        } else if c != '/' {
            return false;
        }
    }
    has_digit && has_alpha
}

/// Loose truth-match: WSJT-X-style overlap (either string contains the
/// other). Mirrors the matcher used in Session 1's diagnostic and
/// `ap_recovery_ceiling.rs`.
fn message_matches(decoded: &str, truth: &str) -> bool {
    let d = decoded.trim();
    let t = truth.trim();
    d == t || d.contains(t) || t.contains(d)
}

/// A single missed-truth candidate selected for the micro-test.
#[derive(Debug, Clone)]
struct PickedTruth {
    wav_sha_short: String,
    truth_message: String,
    /// First callsign extracted from the truth message that is in the
    /// prior set.
    matched_callsign: String,
    /// Which source provided the prior. Operator > recent > cqdx > bundled.
    source: PriorSourceMask,
}

/// Result of running the AP micro-test on one picked truth.
#[derive(Debug, Clone)]
struct MicrotestResult {
    picked: PickedTruth,
    ap_decoded_truth: bool,
    ap_total_decodes: usize,
    ap_off_total_decodes: usize,
}

fn main() -> anyhow::Result<()> {
    let ws = workspace_root()?;

    let main_json: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/scorecards/main.json"),
    )?)?;
    let top20_hashes: Vec<String> = main_json["tiers"]["curated-hard-200"]["per_wav_top_failures"]
        .as_array()
        .context("per_wav_top_failures not array")?
        .iter()
        .take(20)
        .map(|f| f["wav_hash"].as_str().unwrap().to_string())
        .collect();

    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let mut path_by_sha: HashMap<String, String> = HashMap::new();
    for e in manifest["entries"].as_array().context("no entries")? {
        path_by_sha.insert(
            e["wav_sha256"].as_str().unwrap().to_string(),
            e["wav_path"].as_str().unwrap().to_string(),
        );
    }

    let cfg = Ft8Config::default();

    // Pass 1: decode all top-20 WAVs with AP off, identify missed truths,
    // filter to prior-covered, pick the first MICROTEST_N_TARGET across WAVs.
    eprintln!(
        "=== Pass 1: production decode (AP off) + prior-coverage filter on top-{} hard-200 WAVs ===",
        top20_hashes.len(),
    );

    struct WavWorkingSet {
        sha_short: String,
        samples: Vec<f32>,
        ap_off_decoded: HashSet<String>,
        prior_set: CallsignPriorSet,
        missed_truths: Vec<String>,
    }
    let mut working_sets: Vec<WavWorkingSet> = Vec::new();

    for (idx, sha) in top20_hashes.iter().enumerate() {
        let Some(wav_path_str) = path_by_sha.get(sha) else {
            eprintln!("  [{idx:2}] sha {} not in manifest — skip", &sha[..8]);
            continue;
        };
        let wav_path = PathBuf::from(wav_path_str);
        let baseline_path = ws.join(format!("research/baselines/ft8/{sha}.json"));

        let baseline: Value = serde_json::from_str(&std::fs::read_to_string(&baseline_path)?)?;
        let truths: Vec<String> = baseline["decodes"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|d| Some(d.get("message")?.as_str()?.trim().to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let samples = match load_wav(&wav_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  [{idx:2}] WAV load failed for {}: {e}", &sha[..8]);
                continue;
            }
        };

        let mut decoder = Ft8Decoder::new(cfg.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new failed: {e}"))?;
        let ap_off_decoded_msgs = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        let ap_off_decoded: HashSet<String> = ap_off_decoded_msgs
            .iter()
            .map(|d| d.text.trim().to_string())
            .collect();

        // Build the recent-this-WAV prior set from the AP-off pass-1 decodes.
        let mut recent_set: HashSet<String> = HashSet::new();
        for m in &ap_off_decoded {
            for c in callsigns_in(m) {
                recent_set.insert(c);
            }
        }
        let recent_vec: Vec<String> = recent_set.into_iter().collect();
        let prior_set = CallsignPriorSet::from_session1_pool(Some(OPERATOR_CALLSIGN), recent_vec);

        // Identify missed truths.
        let mut missed: Vec<String> = Vec::new();
        for t in &truths {
            let matched = ap_off_decoded.iter().any(|d| message_matches(d, t));
            if !matched {
                missed.push(t.clone());
            }
        }

        eprintln!(
            "  [{idx:2}] {} truth={} rec_off={} missed={} prior_size={}",
            &sha[..8],
            truths.len(),
            ap_off_decoded.len().min(truths.len()), // pancetta may decode extras
            missed.len(),
            prior_set.iter_unique(PRIOR_SET_CAP).len(),
        );

        working_sets.push(WavWorkingSet {
            sha_short: sha[..8].to_string(),
            samples,
            ap_off_decoded,
            prior_set,
            missed_truths: missed,
        });
        let _ = SLOT_S; // silence unused-const if we later reduce reuse
    }

    // Pick prior-covered missed truths, capped per-WAV so the sample is
    // diverse across WAVs (not dominated by one noisy WAV). Two-pass
    // pick: first take ≤PER_WAV_PICK_CAP per WAV; if that didn't fill
    // MICROTEST_N_TARGET, top up with ≤PER_WAV_PICK_CAP_FALLBACK per WAV.
    // The spec's "first N across all WAVs" wording is preserved in spirit
    // — we sweep WAVs in baseline-ranked order — but a hard per-WAV cap
    // prevents single-WAV monoculture.
    //
    // Also skip prior-covered truths whose matched callsign can't be
    // encoded for AP injection (e.g. tokens that look like callsigns but
    // fail pack28, such as grid squares like "FM18" that have
    // letter-letter-digit-digit shape). The micro-test result must be
    // counted only over truths the mechanism CAN attempt.
    let pick_truths_with_cap =
        |cap: usize, exclude: &HashSet<(String, String)>| -> Vec<PickedTruth> {
            let mut picks: Vec<PickedTruth> = Vec::new();
            for ws_item in &working_sets {
                let prior_unique: HashSet<String> = ws_item
                    .prior_set
                    .iter_unique(PRIOR_SET_CAP)
                    .into_iter()
                    .collect();
                let mut this_wav_picks = 0usize;
                for tm in &ws_item.missed_truths {
                    if this_wav_picks >= cap {
                        break;
                    }
                    let calls = callsigns_in(tm);
                    let Some(matched) = calls.iter().find(|c| prior_unique.contains(*c)) else {
                        continue;
                    };
                    // Skip if this (wav, truth) was already picked at the
                    // tighter cap; needed for the top-up second pass.
                    if exclude.contains(&(ws_item.sha_short.clone(), tm.clone())) {
                        continue;
                    }
                    // Skip if the matched callsign can't be encoded for AP.
                    if RecentCallAp::new(matched, 0.0).is_none() {
                        continue;
                    }
                    let source = ws_item.prior_set.source_of(matched);
                    picks.push(PickedTruth {
                        wav_sha_short: ws_item.sha_short.clone(),
                        truth_message: tm.clone(),
                        matched_callsign: matched.clone(),
                        source,
                    });
                    this_wav_picks += 1;
                    if picks.len() >= MICROTEST_N_TARGET {
                        return picks;
                    }
                }
            }
            picks
        };

    let mut picks = pick_truths_with_cap(PER_WAV_PICK_CAP, &HashSet::new());
    if picks.len() < MICROTEST_N_TARGET {
        // Top up at the fallback cap (avoids picking the same (wav, truth)
        // we already have).
        let already: HashSet<(String, String)> = picks
            .iter()
            .map(|p| (p.wav_sha_short.clone(), p.truth_message.clone()))
            .collect();
        let topup = pick_truths_with_cap(PER_WAV_PICK_CAP_FALLBACK, &already);
        for p in topup {
            if picks.len() >= MICROTEST_N_TARGET {
                break;
            }
            picks.push(p);
        }
    }

    if picks.is_empty() {
        println!(
            "\nNo prior-covered missed truths found in the top-20 — \
             nothing to micro-test. SHELVE by default (no leverage)."
        );
        return Ok(());
    }
    eprintln!(
        "\n=== Pass 2: per-truth AP-decode micro-test on {} picked truth(s) ===",
        picks.len()
    );

    // Map sha_short -> &WavWorkingSet for quick re-lookup; we need the
    // raw samples + ap_off_decoded reference set during the AP-on re-decode.
    let mut by_sha: HashMap<&str, &WavWorkingSet> = HashMap::new();
    for w in &working_sets {
        by_sha.insert(w.sha_short.as_str(), w);
    }

    let mut results: Vec<MicrotestResult> = Vec::with_capacity(picks.len());
    for (i, pick) in picks.iter().enumerate() {
        let Some(ws_item) = by_sha.get(pick.wav_sha_short.as_str()) else {
            eprintln!(
                "  ! sha {} dropped from working set — skip",
                &pick.wav_sha_short
            );
            continue;
        };

        // Singleton AP context: my_call=None (research / no operator
        // activity), active_qso=None, recent_calls=[truth callsign].
        // This exercises pancetta-ft8's hb-043 my_call-less path which
        // injects the recent call at BOTH bits 0-27 AND bits 28-55 per
        // candidate, then runs LDPC+CRC+plausibility. The decoder loops
        // this AP-attempt over every Costas candidate and every
        // multipass residual position internally.
        let recent_ap = match RecentCallAp::new(&pick.matched_callsign, 0.0) {
            Some(r) => r,
            None => {
                eprintln!(
                    "  [{i:2}] callsign {} failed RecentCallAp::new — skip pick",
                    &pick.matched_callsign,
                );
                continue;
            }
        };
        let ctx = ApContext {
            my_call: None,
            recent_calls: vec![recent_ap],
            active_qso: None,
        };

        let mut decoder = Ft8Decoder::new(cfg.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new failed: {e}"))?;
        let ap_on_decoded_msgs = decoder
            .decode_window_with_ap(&ws_item.samples, &ctx)
            .map_err(|e| anyhow::anyhow!("decode_window_with_ap: {e}"))?;
        let ap_on_msgs: HashSet<String> = ap_on_decoded_msgs
            .iter()
            .map(|d| d.text.trim().to_string())
            .collect();

        // Did the truth appear in AP-on?
        let truth_in_ap_on = ap_on_msgs
            .iter()
            .any(|d| message_matches(d, &pick.truth_message));
        // Did the truth ALREADY appear in AP-off? (sanity — shouldn't,
        // because we filtered to MISSED truths; but ap_recovery_ceiling
        // shows AP-on sometimes finds things AP-off found differently.)
        let truth_in_ap_off = ws_item
            .ap_off_decoded
            .iter()
            .any(|d| message_matches(d, &pick.truth_message));

        // Rescue test: AP-on must surface a truth that AP-off missed.
        let rescued = truth_in_ap_on && !truth_in_ap_off;

        eprintln!(
            "  [{i:2}] WAV {} truth=\"{}\" call={} src={} ap_off_total={} ap_on_total={} rescued={}",
            &pick.wav_sha_short,
            pick.truth_message,
            pick.matched_callsign,
            pick.source.label(),
            ws_item.ap_off_decoded.len(),
            ap_on_msgs.len(),
            rescued,
        );

        results.push(MicrotestResult {
            picked: pick.clone(),
            ap_decoded_truth: rescued,
            ap_total_decodes: ap_on_msgs.len(),
            ap_off_total_decodes: ws_item.ap_off_decoded.len(),
        });
    }

    // ========= Report =========
    println!("\n=== hb-087 Session 2 — AP-decode micro-test ===");
    println!("\nPer-truth results:");
    println!(
        "  {:>3} {:>9} {:>12} {:>10} {:>10} {:>10} {:>9}",
        "#", "wav", "callsign", "src", "ap_off", "ap_on", "rescued",
    );
    for (i, r) in results.iter().enumerate() {
        println!(
            "  {:>3} {:>9} {:>12} {:>10} {:>10} {:>10} {:>9}",
            i,
            r.picked.wav_sha_short,
            r.picked.matched_callsign,
            r.picked.source.label(),
            r.ap_off_total_decodes,
            r.ap_total_decodes,
            if r.ap_decoded_truth { "YES" } else { "no" },
        );
    }
    println!("\nPer-truth detail (truth message strings):");
    for (i, r) in results.iter().enumerate() {
        println!(
            "  [{i:2}] wav={} call={} src={} rescued={} truth=\"{}\"",
            r.picked.wav_sha_short,
            r.picked.matched_callsign,
            r.picked.source.label(),
            if r.ap_decoded_truth { "YES" } else { "no" },
            r.picked.truth_message,
        );
    }

    let rescued_count = results.iter().filter(|r| r.ap_decoded_truth).count();
    let attempted = results.len();

    // Source-of-prior breakdown.
    let mut by_src_attempted: HashMap<&str, usize> = HashMap::new();
    let mut by_src_rescued: HashMap<&str, usize> = HashMap::new();
    for r in &results {
        let s = r.picked.source.label();
        *by_src_attempted.entry(s).or_insert(0) += 1;
        if r.ap_decoded_truth {
            *by_src_rescued.entry(s).or_insert(0) += 1;
        }
    }

    println!("\nSource-of-prior breakdown (rescued / attempted):");
    for src in ["operator", "recent", "cqdx", "bundled"] {
        let att = by_src_attempted.get(src).copied().unwrap_or(0);
        if att == 0 {
            continue;
        }
        let res = by_src_rescued.get(src).copied().unwrap_or(0);
        println!("  {:>10}: {res}/{att}", src);
    }

    println!(
        "\nMicro-test aggregate: {} rescued out of {} attempted (target N={}).",
        rescued_count, attempted, MICROTEST_N_TARGET,
    );
    println!(
        "\nKill switch: ≥{} of {} rescued → PROCEED to Session 3. \
         <{} → SHELVE.",
        PROCEED_THRESHOLD, MICROTEST_N_TARGET, PROCEED_THRESHOLD,
    );

    if rescued_count >= PROCEED_THRESHOLD && attempted >= PROCEED_THRESHOLD {
        println!(
            "\nVerdict: PROCEED — {} of {} rescued. The AP+LDPC machinery \
             does rescue prior-covered marginal residuals on the production \
             path. Session 3 should extend this to truths whose Costas \
             sync_search misses (the bypass-Costas leg of the mechanism).",
            rescued_count, attempted,
        );
    } else if attempted < PROCEED_THRESHOLD {
        println!(
            "\nVerdict: INDETERMINATE — only {} truths attempted (< {} kill-switch \
             floor). Either expand the corpus or relax the prior-coverage filter \
             before deciding.",
            attempted, PROCEED_THRESHOLD,
        );
    } else {
        println!(
            "\nVerdict: SHELVE — only {} of {} rescued (need ≥{}). Adding \
             callsign priors via the production AP path does NOT materially \
             rescue these marginal residuals. Session 3's bypass-Costas \
             mechanism would inherit this weakness; the structural assumption \
             (that AP-injection at known callsign positions rescues sub-\
             threshold signals) is not supported by this micro-test.",
            rescued_count, attempted, PROCEED_THRESHOLD,
        );
    }

    Ok(())
}
