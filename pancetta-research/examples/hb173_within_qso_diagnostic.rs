//! hb-173 within-QSO context graph — Session-1 feasibility / kill-switch
//! diagnostic.
//!
//! Spawned 2026-06-01 from cross-time ideation T1 (the within-QSO
//! template-injection mechanism). Before specifying the production wiring,
//! we need an empirical answer to: **what fraction of the decoded messages
//! in our test corpus are downstream turns of QSOs whose upstream turn is
//! also decoded?**
//!
//! That fraction is the *target population* for the mechanism's recall
//! lift: only the second-and-later turns of a structurally identifiable
//! QSO can benefit from a pair-conditional AP template injected by the
//! within-QSO context table. If the population is too small, the prior
//! coverage is not large enough to justify the cross-slot plumbing work
//! (substantially more invasive than a single-callsign a7 table; see
//! `docs/superpowers/specs/2026-05-31-hb-048-a7-design.md` §126-132).
//!
//! ## What this diagnostic measures
//!
//! On `hard-200` (and, by extension, `hard-1000` if `--full` is passed):
//!
//! 1. Group all WAVs into **chronological sessions** by parsing the
//!    `ft8_YYYYMMDD_HHMMSS.wav` filename. Two WAVs are in the same session
//!    when their timestamps are ≤ `SESSION_GAP_S` (default 30 s) apart.
//!    Each session is a sequence of contiguous 15-second slots.
//!
//! 2. Load each WAV's **jt9 baseline truth set** (from
//!    `research/baselines/ft8/<sha>.json`). Each truth is a message of
//!    the form `<TO> <FROM> <PAYLOAD>` (directed) or `CQ <FROM> ...` (CQ),
//!    with a frequency and per-slot DT offset.
//!
//! 3. For each session, identify **in-QSO continuations** — a truth at
//!    slot N that is a downstream turn of a QSO whose upstream turn
//!    decoded at slot M < N. The matching rule (deliberately conservative
//!    to avoid spurious pair matches):
//!
//!    - The **callsign pair** `{from, to}` of the slot-N message equals
//!      the pair of some earlier slot-M message in the same session
//!      (order ignored — the responder swaps places between turns).
//!    - The two messages are within `FREQ_TOL_HZ` (default 30 Hz, mirroring
//!      WSJT-X's typical "same QSO" frequency window — a7 uses 2 Hz which
//!      is tighter, but real QSOs sometimes drift slightly across the
//!      exchange).
//!    - Slot M is within `MAX_QSO_SLOTS` (default 8) of slot N. A typical
//!      QSO is 4-7 turns; 8 slots gives slack for late RR73 / 73 turns.
//!    - At least one DIRECTED (non-CQ) message in the pair history must
//!      already exist — otherwise we'd be matching two unrelated CQs.
//!
//! 4. Report:
//!    - `% of truths that are in-QSO continuations` (the headline)
//!    - per-WAV breakdown
//!    - distribution of "QSO depth" (turn index within QSO) of
//!      continuations
//!    - distribution of upstream → downstream slot gap (1 slot, 2 slots, …)
//!
//! ## Kill switch
//!
//! **PROCEED to Session 2** iff `≥ 10%` of truths on hard-200 are in-QSO
//! continuations (matching the spec bar in `research/ideation/2026-06-01-
//! cross-time.md` §T1 "Kill-switch sketch" — 8% there, rounded to 10% in
//! the bank entry; we use the tighter bank-entry threshold to be
//! conservative).
//!
//! **SHELVE** iff `< 10%`. The mechanism's target population is too small
//! to justify the cross-slot QSO-state plumbing.
//!
//! ## Why this is honest about coverage
//!
//! We use the jt9 baseline truth set (not pancetta's own decodes) to
//! identify QSO continuations. This is intentional:
//!
//!   - The target population is "the prior would help if the upstream
//!     turn was decoded". If pancetta decoded the upstream turn (which it
//!     usually does — pancetta recovers ~80%+ on these WAVs), the
//!     downstream turn is a candidate for the boost. Using truth as proxy
//!     for "decodable" is a slight upper bound on coverage.
//!
//!   - The DIAGNOSTIC ESTIMATE is *coverage*, not *recall lift*. The
//!     actual recall lift in production is bounded by (a) coverage × (b)
//!     fraction of covered downstream turns that pancetta currently
//!     misses. We measure (a) here and defer (b) to Session 2 (which
//!     will need pancetta-vs-jt9 diff data per QSO turn).
//!
//! ## Run
//!
//! ```text
//! cargo run --release -p pancetta-research --example hb173_within_qso_diagnostic
//! cargo run --release -p pancetta-research --example hb173_within_qso_diagnostic -- --full
//! ```
//!
//! `--full` adds hard-1000 to the report (slower; ~5 s on M2 for I/O only,
//! no decoder is invoked).

use anyhow::Context;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Two WAVs are in the same chronological session if their timestamps
/// differ by ≤ this many seconds.
const SESSION_GAP_S: i64 = 30;

/// Audio-frequency tolerance (Hz) for "same QSO". WSJT-X a7 uses 2 Hz;
/// we use a more permissive 30 Hz here because in a real QSO the
/// responder is usually within 50 Hz of the caller (operator habit:
/// "answer up 200 Hz from CQ"). 30 Hz captures the staying-in-place
/// pattern while excluding accidental same-pair matches at different
/// pile-up locations.
const FREQ_TOL_HZ: f64 = 30.0;

/// Maximum number of slots between two turns of a putative QSO. Typical
/// QSO is 4-7 turns at 15s/turn; 8-slot window gives slack for delayed
/// turns due to QSB or operator pause.
const MAX_QSO_SLOTS: usize = 8;

/// Decision threshold (% of truths that are in-QSO continuations).
const PROCEED_THRESHOLD_PCT: f64 = 10.0;

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

/// Extract a (year, month, day, hour, minute, second) UTC timestamp
/// from a filename of the form `ft8_YYYYMMDD_HHMMSS.wav`. Returns Unix
/// seconds (assumes the timestamp IS UTC, matching pancetta's recorder).
fn parse_wav_timestamp(path: &str) -> Option<i64> {
    let base = Path::new(path).file_stem()?.to_str()?;
    // ft8_YYYYMMDD_HHMMSS
    let parts: Vec<&str> = base.split('_').collect();
    if parts.len() != 3 || parts[0] != "ft8" {
        return None;
    }
    if parts[1].len() != 8 || parts[2].len() != 6 {
        return None;
    }
    let y: i32 = parts[1][0..4].parse().ok()?;
    let mo: u32 = parts[1][4..6].parse().ok()?;
    let d: u32 = parts[1][6..8].parse().ok()?;
    let h: u32 = parts[2][0..2].parse().ok()?;
    let mi: u32 = parts[2][2..4].parse().ok()?;
    let s: u32 = parts[2][4..6].parse().ok()?;
    let dt = chrono::NaiveDate::from_ymd_opt(y, mo, d)?.and_hms_opt(h, mi, s)?;
    Some(dt.and_utc().timestamp())
}

/// A parsed jt9 truth decode.
#[derive(Clone, Debug)]
struct Truth {
    /// Canonical sender callsign (FROM token, base call only, no /P /M).
    from: String,
    /// Canonical receiver callsign (TO token if directed; None for CQ).
    to: Option<String>,
    /// Audio frequency in Hz.
    freq_hz: f64,
    /// Raw message text.
    #[allow(dead_code)]
    message: String,
    /// True iff this is a CQ-shaped message (no fixed receiver).
    #[allow(dead_code)]
    is_cq: bool,
}

/// Parse an FT8 truth message into (FROM, TO?, is_cq). Conservative —
/// returns None when the structure is unrecognized (e.g., free-text,
/// telemetry).
fn parse_message(message: &str) -> Option<(String, Option<String>, bool)> {
    let upper = message.to_uppercase();
    let tokens: Vec<&str> = upper.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    if tokens[0] == "CQ" {
        // CQ [MODIFIER] FROM [GRID]
        let mut idx = 1;
        if idx < tokens.len() && is_cq_modifier(tokens[idx]) {
            idx += 1;
        }
        if idx >= tokens.len() {
            return None;
        }
        let from = canonical_call(tokens[idx])?;
        Some((from, None, true))
    } else {
        // TO FROM PAYLOAD
        if tokens.len() < 2 {
            return None;
        }
        let to = canonical_call(tokens[0])?;
        let from = canonical_call(tokens[1])?;
        Some((from, Some(to), false))
    }
}

fn is_cq_modifier(t: &str) -> bool {
    matches!(t, "DX" | "NA" | "SA" | "EU" | "AS" | "AF" | "OC" | "QRP")
        || t.chars().all(|c| c.is_ascii_digit())
        || (t.len() <= 3 && t.chars().all(|c| c.is_ascii_alphabetic()))
}

/// Canonical callsign: strip portable suffixes (/P /M /MM …) and verify
/// it looks like a real call (has both letter and digit).
fn canonical_call(t: &str) -> Option<String> {
    let len = t.len();
    if !(3..=10).contains(&len) {
        return None;
    }
    let mut has_digit = false;
    let mut has_alpha = false;
    for c in t.chars() {
        if c.is_ascii_digit() {
            has_digit = true;
        } else if c.is_ascii_alphabetic() {
            has_alpha = true;
        } else if c != '/' {
            return None;
        }
    }
    if !(has_digit && has_alpha) {
        return None;
    }
    Some(t.split('/').next().unwrap_or(t).to_string())
}

/// Unordered callsign pair used as a QSO-history key.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Pair(String, String);

impl Pair {
    fn new(a: &str, b: &str) -> Self {
        if a <= b {
            Pair(a.to_string(), b.to_string())
        } else {
            Pair(b.to_string(), a.to_string())
        }
    }
}

/// One slot of one session.
struct SlotRecord {
    /// Slot index within the session (0-based).
    slot_idx: usize,
    /// Truths decoded by jt9 for this slot.
    truths: Vec<Truth>,
    /// Short SHA prefix for reporting.
    sha_short: String,
}

/// One chronologically-contiguous session.
struct Session {
    /// Slot 0 timestamp (Unix seconds).
    #[allow(dead_code)]
    start_unix: i64,
    slots: Vec<SlotRecord>,
}

/// Load and parse one WAV's jt9 baseline file. Returns (Truths, sha_short).
fn load_baseline_truths(baseline_path: &Path, sha: &str) -> anyhow::Result<Vec<Truth>> {
    let baseline: Value = serde_json::from_str(&std::fs::read_to_string(baseline_path)?)?;
    let mut truths: Vec<Truth> = Vec::new();
    let _ = sha;
    if let Some(arr) = baseline["decodes"].as_array() {
        for d in arr {
            let msg = d.get("message").and_then(|v| v.as_str()).unwrap_or("");
            let freq = d.get("freq_hz").and_then(|v| v.as_f64()).unwrap_or(0.0);
            if msg.is_empty() {
                continue;
            }
            let Some((from, to, is_cq)) = parse_message(msg) else {
                continue;
            };
            truths.push(Truth {
                from,
                to,
                freq_hz: freq,
                message: msg.trim().to_string(),
                is_cq,
            });
        }
    }
    Ok(truths)
}

fn build_sessions(manifest_path: &Path, baselines_dir: &Path) -> anyhow::Result<Vec<Session>> {
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(manifest_path)?)?;
    let entries = manifest["entries"]
        .as_array()
        .context("manifest entries not array")?;

    // (timestamp_unix, sha) for every WAV in the tier that has both a
    // timestamp-parseable filename AND a baseline file on disk.
    let mut wavs: Vec<(i64, String)> = Vec::new();
    for e in entries {
        let path = e["wav_path"].as_str().unwrap_or("");
        let sha = e["wav_sha256"].as_str().unwrap_or("");
        if path.is_empty() || sha.is_empty() {
            continue;
        }
        let Some(ts) = parse_wav_timestamp(path) else {
            continue;
        };
        if !baselines_dir.join(format!("{sha}.json")).exists() {
            continue;
        }
        wavs.push((ts, sha.to_string()));
    }
    wavs.sort_by_key(|(ts, _)| *ts);
    wavs.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);

    // Cluster into sessions.
    let mut sessions: Vec<Session> = Vec::new();
    let mut cur_start: Option<i64> = None;
    let mut cur_prev: Option<i64> = None;
    let mut cur_slots: Vec<SlotRecord> = Vec::new();
    for (ts, sha) in wavs.into_iter() {
        let new_session = match cur_prev {
            Some(prev) => (ts - prev).abs() > SESSION_GAP_S,
            None => true,
        };
        if new_session && !cur_slots.is_empty() {
            sessions.push(Session {
                start_unix: cur_start.unwrap(),
                slots: std::mem::take(&mut cur_slots),
            });
            cur_start = None;
        }
        if cur_start.is_none() {
            cur_start = Some(ts);
        }
        let slot_idx = cur_slots.len();
        let baseline_path = baselines_dir.join(format!("{sha}.json"));
        let truths = load_baseline_truths(&baseline_path, &sha).unwrap_or_default();
        cur_slots.push(SlotRecord {
            slot_idx,
            truths,
            sha_short: sha[..8].to_string(),
        });
        cur_prev = Some(ts);
    }
    if !cur_slots.is_empty() {
        sessions.push(Session {
            start_unix: cur_start.unwrap(),
            slots: cur_slots,
        });
    }
    Ok(sessions)
}

#[derive(Default, Clone)]
struct TierStats {
    n_wavs: usize,
    n_sessions: usize,
    n_multi_slot_sessions: usize,
    total_truths: usize,
    n_continuations: usize,
    /// For each gap (slot_n - slot_m), how many continuations had that gap.
    gap_histogram: HashMap<usize, usize>,
    /// For each (pair turn index 1..MAX_QSO_SLOTS), how many continuations.
    depth_histogram: HashMap<usize, usize>,
    /// Per-WAV breakdown for the top of the report.
    per_wav: Vec<(String, usize, usize)>, // (sha_short, truths, continuations)
}

fn analyze_tier(label: &str, sessions: &[Session]) -> TierStats {
    let mut stats = TierStats {
        n_sessions: sessions.len(),
        n_wavs: sessions.iter().map(|s| s.slots.len()).sum(),
        n_multi_slot_sessions: sessions.iter().filter(|s| s.slots.len() > 1).count(),
        ..Default::default()
    };

    // For each session, walk slots in chronological order. Maintain a
    // map from Pair → Vec<(slot_idx, freq_hz, directed)> recording every
    // prior sighting. A new sighting is a "continuation" if the pair has
    // a prior sighting within MAX_QSO_SLOTS, within FREQ_TOL_HZ, and the
    // pair history contains at least one DIRECTED message.
    for session in sessions {
        let mut pair_history: HashMap<Pair, Vec<(usize, f64, bool)>> = HashMap::new();

        for slot in &session.slots {
            let mut slot_continuations: usize = 0;
            // Build the "current slot's pair sightings" before exposing
            // them to subsequent truths (so a truth doesn't match
            // ITSELF via the same-slot history; in practice slots are
            // independent, but be explicit).
            let mut new_entries: Vec<(Pair, (usize, f64, bool))> = Vec::new();

            for truth in &slot.truths {
                stats.total_truths += 1;
                // Only consider DIRECTED messages as candidate
                // continuations (a CQ is by definition the START of a
                // QSO, not a downstream turn). CQs DO contribute to
                // pair_history when their FROM appears later as a TO
                // of a directed reply.
                let is_continuation = if let Some(to) = &truth.to {
                    let pair = Pair::new(&truth.from, to);
                    let history = pair_history.get(&pair);
                    let mut found = None;
                    if let Some(prior_sightings) = history {
                        // Require at least one DIRECTED prior sighting.
                        // (A solo CQ from FROM does not yet establish a
                        // QSO with TO — TO might just be a coincident
                        // other CQ.)
                        let has_directed = prior_sightings.iter().any(|(_, _, dir)| *dir);
                        if has_directed {
                            // Find the most-recent prior sighting within
                            // MAX_QSO_SLOTS and FREQ_TOL_HZ.
                            for (m_idx, f_hz, _dir) in prior_sightings.iter().rev() {
                                let gap = slot.slot_idx.saturating_sub(*m_idx);
                                if gap == 0 {
                                    continue;
                                }
                                if gap > MAX_QSO_SLOTS {
                                    break;
                                }
                                if (truth.freq_hz - *f_hz).abs() <= FREQ_TOL_HZ {
                                    let depth = prior_sightings.len(); // 1-based "this is turn (depth+1)"
                                    found = Some((gap, depth));
                                    break;
                                }
                            }
                        }
                    }
                    if let Some((gap, depth)) = found {
                        stats.n_continuations += 1;
                        slot_continuations += 1;
                        *stats.gap_histogram.entry(gap).or_insert(0) += 1;
                        *stats.depth_histogram.entry(depth + 1).or_insert(0) += 1;
                        true
                    } else {
                        false
                    }
                } else {
                    // CQ is never a downstream turn.
                    false
                };
                let _ = is_continuation;

                // Record this sighting for future slots. CQs use a
                // sentinel pair (FROM, FROM) to avoid mixing them into
                // arbitrary pair histories. We DO accumulate directed
                // sightings as primary evidence.
                if let Some(to) = &truth.to {
                    let pair = Pair::new(&truth.from, to);
                    new_entries.push((pair, (slot.slot_idx, truth.freq_hz, true)));
                }
                // (CQ sightings are intentionally NOT recorded in
                // pair_history. They become part of a pair only when a
                // directed reply names the CQ-er.)
            }

            for (pair, entry) in new_entries {
                pair_history.entry(pair).or_default().push(entry);
            }

            stats.per_wav.push((
                slot.sha_short.clone(),
                slot.truths.len(),
                slot_continuations,
            ));
        }
    }
    let _ = label;
    stats
}

fn print_tier_report(label: &str, stats: &TierStats) {
    println!("\n=== hb-173 within-QSO continuation — tier {label} ===");
    println!(
        "  WAVs: {}    sessions: {} (multi-slot: {})    total truths: {}",
        stats.n_wavs, stats.n_sessions, stats.n_multi_slot_sessions, stats.total_truths,
    );
    let pct = if stats.total_truths == 0 {
        0.0
    } else {
        100.0 * stats.n_continuations as f64 / stats.total_truths as f64
    };
    println!(
        "  In-QSO continuations: {} ({:.2}% of truths)",
        stats.n_continuations, pct,
    );

    println!("\n  Continuation depth (which turn of the QSO this is):");
    let mut depths: Vec<(usize, usize)> = stats
        .depth_histogram
        .iter()
        .map(|(k, v)| (*k, *v))
        .collect();
    depths.sort_by_key(|(k, _)| *k);
    for (depth, n) in depths {
        let pct_of_cont = if stats.n_continuations == 0 {
            0.0
        } else {
            100.0 * n as f64 / stats.n_continuations as f64
        };
        println!("    turn {depth:>2}: {n:>5} ({pct_of_cont:>5.1}%)");
    }

    println!("\n  Slot gap (slots between upstream turn and downstream continuation):");
    let mut gaps: Vec<(usize, usize)> = stats.gap_histogram.iter().map(|(k, v)| (*k, *v)).collect();
    gaps.sort_by_key(|(k, _)| *k);
    for (gap, n) in gaps {
        let pct_of_cont = if stats.n_continuations == 0 {
            0.0
        } else {
            100.0 * n as f64 / stats.n_continuations as f64
        };
        println!("    gap {gap:>2} slots: {n:>5} ({pct_of_cont:>5.1}%)");
    }
}

fn print_per_wav_top(label: &str, stats: &TierStats, top_n: usize) {
    // Show top-N WAVs by continuation count, then bottom-N by truths.
    let mut rows = stats.per_wav.clone();
    rows.sort_by(|a, b| b.2.cmp(&a.2));
    println!("\n  Top {top_n} WAVs by continuation count ({label}):");
    println!(
        "    {:>9} {:>7} {:>9} {:>6}",
        "sha", "truths", "continues", "%"
    );
    for r in rows.iter().take(top_n) {
        let pct = if r.1 == 0 {
            0.0
        } else {
            100.0 * r.2 as f64 / r.1 as f64
        };
        println!("    {:>9} {:>7} {:>9} {:>5.1}%", r.0, r.1, r.2, pct);
    }
}

fn main() -> anyhow::Result<()> {
    let ws = workspace_root()?;
    let baselines_dir = ws.join("research/baselines/ft8");
    let full = std::env::args().any(|a| a == "--full");

    println!(
        "hb-173 within-QSO continuation diagnostic\n  baselines: {}",
        baselines_dir.display()
    );

    let mut tier_results: Vec<(String, TierStats)> = Vec::new();

    // --- hard-200 ---
    let h200 = ws.join("research/corpus/curated/ft8/hard_200.manifest.json");
    eprintln!("loading hard-200 sessions…");
    let sessions = build_sessions(&h200, &baselines_dir)?;
    let stats = analyze_tier("hard-200", &sessions);
    print_tier_report("hard-200", &stats);
    print_per_wav_top("hard-200", &stats, 15);
    tier_results.push(("hard-200".into(), stats));

    if full {
        let h1000 = ws.join("research/corpus/curated/ft8/hard_1000.manifest.json");
        eprintln!("loading hard-1000 sessions…");
        let sessions = build_sessions(&h1000, &baselines_dir)?;
        let stats = analyze_tier("hard-1000", &sessions);
        print_tier_report("hard-1000", &stats);
        tier_results.push(("hard-1000".into(), stats));
    }

    // Coverage sanity: how many distinct callsigns we saw, how many
    // sessions were single-slot (no QSO context possible).
    println!("\n=== Diagnostic sanity ===");
    for (label, stats) in &tier_results {
        let single_slot = stats.n_sessions.saturating_sub(stats.n_multi_slot_sessions);
        println!(
            "  {label}: {} sessions, {} single-slot ({:.0}%)",
            stats.n_sessions,
            single_slot,
            if stats.n_sessions == 0 {
                0.0
            } else {
                100.0 * single_slot as f64 / stats.n_sessions as f64
            }
        );
    }

    // --- Verdict ---
    let h200_stats = &tier_results
        .iter()
        .find(|(l, _)| l == "hard-200")
        .unwrap()
        .1;
    let h200_pct = if h200_stats.total_truths == 0 {
        0.0
    } else {
        100.0 * h200_stats.n_continuations as f64 / h200_stats.total_truths as f64
    };
    println!("\n=== Decision ===");
    println!(
        "  PROCEED threshold: in-QSO continuations ≥ {:.0}% of hard-200 truths.",
        PROCEED_THRESHOLD_PCT,
    );
    println!(
        "  Observed: {:.2}% of hard-200 truths are in-QSO continuations.",
        h200_pct,
    );
    let verdict = if h200_pct >= PROCEED_THRESHOLD_PCT {
        format!(
            "PROCEED — {:.2}% of decoded messages are structural continuations of an \
             identifiable QSO. Pair-conditional AP templates have a real target population; \
             write Session 2 implementation.",
            h200_pct,
        )
    } else {
        format!(
            "SHELVE — only {:.2}% of decoded messages are within-QSO continuations. The \
             cross-slot QSO-state plumbing is heavy for so small a target population; \
             revisit if eval harness gains a chronological-replay tier (which would let \
             us measure the FULL slot stream, not the curated WAVs).",
            h200_pct,
        )
    };
    println!("\n  Verdict: {verdict}");

    // For HashSet usage (silence dead-code warning if not referenced
    // elsewhere) — we use HashSet implicitly via HashMap above.
    let _ = HashSet::<String>::new();

    Ok(())
}
