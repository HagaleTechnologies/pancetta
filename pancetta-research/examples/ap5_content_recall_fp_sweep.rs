//! ap5_content_recall_fp_sweep — Task 7 (`docs/superpowers/plans/
//! 2026-07-25-ap-content-decoding.md`) eval harness for Ap5 (content-AP)
//! per `docs/ap-decoding-design.md` §4A/§4B.
//!
//! ## Carry-forward from Task 6 (READ FIRST — this is why the harness runs
//! ## in two phases, pilot before full sweep)
//!
//! Task 6's implementer searched real audio via 3 methodologies and found
//! **no natural window where the existing AP0-AP4 ladder fails but Ap5
//! (content injection) succeeds** — the rescue test that proves Ap5's
//! *wiring* had to structurally disable AP2-4 (empty `recent_calls`,
//! `active_qso: None`) to isolate a clean win. That is legitimate proof the
//! wiring works, but NOT proof Ap5 buys real recall over the FULL ladder.
//!
//! This harness's job, in order:
//! 1. **Pilot** (`run_pilot`, always runs): the SAME measurement — recall
//!    with the FULL AP0-4+Cq ladder always enabled on both sides, content-AP
//!    (Ap5) off vs on — at a handful of SNR points, swept DEEPER than the
//!    §4A spec range (down to -46 dB, not just -24..-6) specifically to
//!    answer "can this corruption methodology construct a rescue window AT
//!    ALL", not just "within the nominal operating range". If it can't
//!    across enough trials to be conclusive, THAT is the headline result —
//!    reported prominently. The full §4A sweep only runs if the pilot finds
//!    real rescue cases, or if `--full` is passed explicitly to force it.
//! 2. **Full sweep** (`run_full_sweep`, `--full`): the official §4A SNR
//!    range (-24..-6 dB, 2 dB steps), recall + both false-decode decoy
//!    protocols, plus the knob-vector tradeoff curve.
//! 3. **Corpus A/B** (§4B) — NOT built. See "Corpus A/B" doc section below
//!    for why, honestly, rather than fabricating a result.
//!
//! ## A load-bearing caveat about the knob vector
//!
//! Per `pancetta-ft8/src/decoder.rs`'s `Ft8Config::ap_llr_magnitude` and
//! `::min_ap_decode_confidence` doc comments (Task 4 scope note): those two
//! fields are config SURFACE ONLY as of this task — `inject_ap_llrs` and
//! `try_ldpc_with_ap` still read their own hardcoded consts
//! (`AP_LLR_MAGNITUDE = 15.0`, `MIN_AP_DECODE_CONFIDENCE = 0.55`), not these
//! config fields (confirmed by grep: no call site outside a default-value
//! test reads `.ap_llr_magnitude` or `.min_ap_decode_confidence`). Sweeping
//! `ap_llr_magnitude` therefore MUST show zero effect on outcomes — this
//! harness still sweeps it (to confirm/demonstrate that directly) but flags
//! it explicitly rather than reporting a silent no-op as a real finding.
//! Only `content_ap_enabled`, `min_content_ap_confidence`,
//! `max_ap_hypotheses`, and `max_ap_qsos` are actually wired to the Ap5
//! decode loop today.
//!
//! ## Corpus A/B (§4B) — documented gap, not built
//!
//! Not implemented in this harness. Two independent reasons, either one
//! sufficient alone: (1) the `ft8_lib` vendored submodule is not checked
//! out in this worktree (`git submodule status` shows a `-` prefix, and
//! `cargo build` prints "ft8_lib C sources not found ... building WITHOUT
//! the C decoder (ft8lib_stub, degraded decode recall)") — this blocks the
//! ft8_lib-truth precision proxy that §4B's spec calls for, a pre-existing
//! environment gap unrelated to this task (per the task brief). (2)
//! Reconstructing the live `ApContext` (partner call + QSO progress) from a
//! recorded decode stream — §4B's own spec text says "context reconstructed
//! from the decode stream" — is a nontrivial QSO-state-machine replay,
//! disproportionate to build for a question the synthetic sweep (§4A)
//! already answers directly with ground truth, especially given the pilot
//! below is expected (per Task 6) to make the ship-gate answer moot before
//! §4B would even run. If the pilot instead finds real recall lift, §4B
//! becomes worth building as a follow-on — flagged, not fabricated.
//!
//! ## Method (§4A)
//!
//! Mirrors `hb048_a7_synthetic_injection.rs`'s deterministic-seed
//! sweep-table convention and `batch30_snr_recall_curve.rs`'s SNR-sweep /
//! WSJT-X-2500-Hz-reference-bandwidth convention; reuses
//! `pancetta_research::synth::add_awgn_2500hz_ref` rather than
//! reimplementing AWGN (per this task's instruction to reuse an existing
//! helper if one exists).
//!
//! Two QSO stages are exercised — the two content-hypothesis-set shapes per
//! `docs/ap-decoding-design.md` §1's table: `WaitingForReport` (24
//! report-value hypotheses) and `WaitingForConfirmation` (3 RR73/RRR/73
//! hypotheses). For each stage, at each SNR point:
//! - Encode + modulate the partner's TRUE next message, add AWGN at the
//!   target SNR (WSJT-X 2500 Hz reference-bandwidth convention, whole
//!   12.64s window).
//! - Build the `ApContext` the coordinator would actually have (correct
//!   partner call, correct progress, the real enumerated hypothesis set,
//!   plus a small decoy `recent_calls` pool for AP2 realism).
//! - Decode the SAME noisy audio once with `content_ap_enabled: false`
//!   (full AP0-4 + Cq ladder, no Ap5) and once with `content_ap_enabled:
//!   true` (same ladder + Ap5). AP-off failing while AP-on recovers the
//!   true text via `ap_level == 6` is a genuine rescue case.
//! - Two false-decode decoy protocols over the SAME noisy audio / a
//!   noise-only buffer: (i) wrong-context injection — wrong partner call,
//!   and separately wrong QSO stage (wrong hypothesis set) — over the TRUE
//!   audio; (ii) noise-only injection — the real AP context asserted over
//!   pure AWGN, no signal at all.
//!
//! ## Run
//!
//! ```text
//! cargo run --release -p pancetta-research --example ap5_content_recall_fp_sweep -- --help
//! cargo run --release -p pancetta-research --example ap5_content_recall_fp_sweep          # pilot only (default)
//! cargo run --release -p pancetta-research --example ap5_content_recall_fp_sweep -- --full
//! ```
//!
//! Env vars (all optional):
//! - `AP5_SWEEP_PILOT_TRIALS` (default 60): trials per (SNR, stage) pair in the pilot.
//! - `AP5_SWEEP_PILOT_SNRS` (default "-18,-22,-26,-30,-34,-38,-42,-46"): pilot SNR points, dB —
//!   deliberately deeper than the official §4A range, see module doc.
//! - `AP5_SWEEP_FULL_TRIALS` (default 60): trials per (SNR, stage) pair in the full sweep.
//! - `AP5_SWEEP_FULL_SNRS` (default "-24,-22,-20,-18,-16,-14,-12,-10,-8,-6"): full sweep SNR
//!   points (the official §4A spec range).

use anyhow::Result;
use pancetta_ft8::ap::enumerate_a8_expected_texts;
use pancetta_ft8::{
    ApContext, DecodedMessage, Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, MyCallAp, QsoAp,
    QsoApProgress, RecentCallAp, WINDOW_SAMPLES,
};
use pancetta_research::synth::{add_awgn_2500hz_ref, signal_rms};
use std::time::Instant;

const MY_CALL: &str = "K1ABC";
const DX_CALL: &str = "W1AW";
const WRONG_DX_CALL: &str = "W9XYZ";
const DECOY_CALLS: [&str; 3] = ["N0AAA", "VE3XYZ", "G4ABC"];

/// Default knob vector — matches `Ft8Config::default()`'s Ap5 knobs
/// exactly, so the "default operating point" row of every table below is
/// directly comparable to shipped behavior if `content_ap_enabled` were
/// ever flipped.
const DEFAULT_KNOBS: Knobs = Knobs {
    min_content_ap_confidence: 0.60,
    max_ap_hypotheses: 8,
    ap_llr_magnitude: 15.0,
};

#[derive(Clone, Copy, Debug)]
struct Knobs {
    min_content_ap_confidence: f32,
    max_ap_hypotheses: usize,
    ap_llr_magnitude: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    WaitingForReport,
    WaitingForConfirmation,
}

impl Stage {
    const ALL: [Stage; 2] = [Stage::WaitingForReport, Stage::WaitingForConfirmation];

    fn label(self) -> &'static str {
        match self {
            Stage::WaitingForReport => "WaitingForReport",
            Stage::WaitingForConfirmation => "WaitingForConfirmation",
        }
    }

    fn progress(self) -> QsoApProgress {
        match self {
            Stage::WaitingForReport => QsoApProgress::WaitingForReport,
            Stage::WaitingForConfirmation => QsoApProgress::WaitingForConfirmation,
        }
    }

    fn other(self) -> Stage {
        match self {
            Stage::WaitingForReport => Stage::WaitingForConfirmation,
            Stage::WaitingForConfirmation => Stage::WaitingForReport,
        }
    }

    /// The TRUE next message the partner sends at this stage. Must be a
    /// member of `enumerate_a8_expected_texts`'s own output for this stage
    /// — asserted in `main` — so the harness measures a genuinely
    /// enumerable hypothesis, not an out-of-band message AP could never
    /// have hit regardless of noise.
    fn truth_text(self, my: &str, dx: &str) -> String {
        match self {
            Stage::WaitingForReport => format!("{my} {dx} -08"),
            Stage::WaitingForConfirmation => format!("{my} {dx} RR73"),
        }
    }
}

fn build_ap_context(my: &str, dx: &str, stage: Stage) -> ApContext {
    let my_call = MyCallAp::new(my).expect("my call encodes");
    let texts = enumerate_a8_expected_texts(my, dx, stage.progress());
    let mut qso = QsoAp::new(dx, stage.progress()).expect("dx call encodes");
    qso = qso.with_expected_texts(texts);
    let recent_calls = DECOY_CALLS
        .iter()
        .filter_map(|c| RecentCallAp::new(c, -10.0))
        .collect();
    ApContext {
        my_call: Some(my_call),
        recent_calls,
        active_qso: Some(qso.clone()),
        active_qsos: vec![qso],
    }
}

/// Encode + modulate `text`, zero-padded to a full window. Returns the
/// padded samples plus the CLEAN (pre-padding) signal RMS, which
/// `add_awgn_2500hz_ref` requires be measured before zero-padding dilutes
/// it (see that function's doc comment).
fn modulate(text: &str) -> (Vec<f32>, f64) {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder.encode_message(text, None).expect("encode");
    let mut modulator = Ft8Modulator::new_default().expect("modulator");
    let mut tx = modulator.modulate_symbols(&symbols, 0.0).expect("modulate");
    let rms = signal_rms(&tx);
    tx.resize(WINDOW_SAMPLES, 0.0);
    (tx, rms)
}

fn cfg_off() -> Ft8Config {
    Ft8Config {
        content_ap_enabled: false,
        ..Ft8Config::default()
    }
}

fn cfg_on(k: Knobs) -> Ft8Config {
    Ft8Config {
        content_ap_enabled: true,
        min_content_ap_confidence: k.min_content_ap_confidence,
        max_ap_hypotheses: k.max_ap_hypotheses,
        ap_llr_magnitude: k.ap_llr_magnitude,
        ..Ft8Config::default()
    }
}

fn decode(cfg: Ft8Config, samples: &[f32], ctx: &ApContext) -> Vec<DecodedMessage> {
    let mut decoder = Ft8Decoder::new(cfg).expect("decoder construction");
    decoder
        .decode_window_with_ap(samples, ctx)
        .expect("decode_window_with_ap")
}

// ---------------------------------------------------------------------------
// Recall measurement (AP-off vs AP-on, same noisy audio)
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy)]
struct RecallOutcome {
    off_hit: bool,
    on_hit: bool,
    /// `on_hit` AND the surviving decode's `ap_level == 6` (a genuine Ap5
    /// hit, not a coincidental AP0-4/Cq hit that also would have survived
    /// with `content_ap_enabled: false`).
    on_via_ap5: bool,
}

impl RecallOutcome {
    /// AP0-4 (+Cq) failed, but the SAME noisy audio was recovered once Ap5
    /// joined the ladder — the exact case the carry-forward asks the pilot
    /// to hunt for.
    fn is_rescue(&self) -> bool {
        !self.off_hit && self.on_hit
    }
}

fn run_recall_trial(stage: Stage, snr_db: f64, seed: u64, knobs: Knobs) -> RecallOutcome {
    let truth = stage.truth_text(MY_CALL, DX_CALL);
    let (clean, rms) = modulate(&truth);
    let mut noisy = clean;
    add_awgn_2500hz_ref(&mut noisy, rms, snr_db, seed);
    let ctx = build_ap_context(MY_CALL, DX_CALL, stage);

    let off = decode(cfg_off(), &noisy, &ctx);
    let off_hit = off.iter().any(|m| m.text == truth);

    let on = decode(cfg_on(knobs), &noisy, &ctx);
    let on_hit = on.iter().any(|m| m.text == truth);
    let on_via_ap5 = on.iter().any(|m| m.text == truth && m.ap_level == 6);

    RecallOutcome {
        off_hit,
        on_hit,
        on_via_ap5,
    }
}

// ---------------------------------------------------------------------------
// False-decode protocols (i) wrong-context, (ii) noise-only
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy)]
struct FalseDecodeOutcome {
    /// AP-off, no context at all — baseline spurious-decode indicator.
    baseline_false: bool,
    /// Protocol (i-a): correct stage, WRONG partner call, over TRUE audio.
    wrong_call_false: bool,
    /// Protocol (i-b): correct partner call, WRONG stage (wrong hypothesis
    /// set), over TRUE audio.
    wrong_stage_false: bool,
}

fn run_wrong_context_trial(stage: Stage, snr_db: f64, seed: u64, knobs: Knobs) -> FalseDecodeOutcome {
    let truth = stage.truth_text(MY_CALL, DX_CALL);
    let (clean, rms) = modulate(&truth);
    let mut noisy = clean;
    add_awgn_2500hz_ref(&mut noisy, rms, snr_db, seed);

    let baseline = decode(cfg_off(), &noisy, &ApContext::default());
    let baseline_false = baseline.iter().any(|m| m.text != truth);

    let ctx_wrong_call = build_ap_context(MY_CALL, WRONG_DX_CALL, stage);
    let on_a = decode(cfg_on(knobs), &noisy, &ctx_wrong_call);
    let wrong_call_false = on_a.iter().any(|m| m.text != truth);

    let ctx_wrong_stage = build_ap_context(MY_CALL, DX_CALL, stage.other());
    let on_b = decode(cfg_on(knobs), &noisy, &ctx_wrong_stage);
    let wrong_stage_false = on_b.iter().any(|m| m.text != truth);

    FalseDecodeOutcome {
        baseline_false,
        wrong_call_false,
        wrong_stage_false,
    }
}

#[derive(Default, Clone, Copy)]
struct NoiseOnlyOutcome {
    baseline_false: bool,
    on_false: bool,
}

fn run_noise_only_trial(
    stage: Stage,
    snr_db: f64,
    seed: u64,
    ref_rms: f64,
    knobs: Knobs,
) -> NoiseOnlyOutcome {
    let mut buf = vec![0.0f32; WINDOW_SAMPLES];
    add_awgn_2500hz_ref(&mut buf, ref_rms, snr_db, seed);
    let ctx = build_ap_context(MY_CALL, DX_CALL, stage);

    let baseline = decode(cfg_off(), &buf, &ApContext::default());
    let baseline_false = !baseline.is_empty();

    let on = decode(cfg_on(knobs), &buf, &ctx);
    let on_false = !on.is_empty();

    NoiseOnlyOutcome {
        baseline_false,
        on_false,
    }
}

// ---------------------------------------------------------------------------
// Pilot
// ---------------------------------------------------------------------------

fn parse_env_f64_list(var: &str, default: &str) -> Vec<f64> {
    std::env::var(var)
        .unwrap_or_else(|_| default.to_string())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect()
}

fn parse_env_usize(var: &str, default: usize) -> usize {
    std::env::var(var)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Returns total rescue count found across the whole pilot (the number
/// that determines whether the full sweep is worth running).
fn run_pilot() -> usize {
    let trials: usize = parse_env_usize("AP5_SWEEP_PILOT_TRIALS", 60);
    let snrs = parse_env_f64_list(
        "AP5_SWEEP_PILOT_SNRS",
        "-18,-22,-26,-30,-34,-38,-42,-46",
    );

    println!("## Phase 1 — Pilot (carry-forward from Task 6)");
    println!(
        "  {} trials per (SNR, stage); SNR points (dB, deliberately deeper than the official",
        trials
    );
    println!(
        "  §4A range -24..-6, hunting for ANY window the corruption method can construct): {:?}",
        snrs
    );
    println!("  Full AP0-4+Cq ladder enabled on BOTH sides; only content_ap_enabled differs.\n");

    let mut total_rescues = 0usize;
    let mut total_trials = 0usize;

    println!(
        "  {:<24} {:>7} {:>7} {:>9} {:>9} {:>9}",
        "stage", "snr_db", "trials", "off_hit%", "on_hit%", "rescues"
    );
    println!("  {:-<70}", "");
    for stage in Stage::ALL {
        for &snr in &snrs {
            let mut off_hits = 0usize;
            let mut on_hits = 0usize;
            let mut rescues = 0usize;
            for t in 0..trials {
                let seed = pilot_seed(stage, snr, t);
                let outcome = run_recall_trial(stage, snr, seed, DEFAULT_KNOBS);
                if outcome.off_hit {
                    off_hits += 1;
                }
                if outcome.on_hit {
                    on_hits += 1;
                }
                if outcome.is_rescue() {
                    rescues += 1;
                    println!(
                        "    ** RESCUE ** stage={:?} snr={:.1}dB trial={} seed={:#x} on_via_ap5={}",
                        stage, snr, t, seed, outcome.on_via_ap5
                    );
                }
            }
            println!(
                "  {:<24} {:>+6.1} {:>7} {:>8.1}% {:>8.1}% {:>9}",
                stage.label(),
                snr,
                trials,
                100.0 * off_hits as f64 / trials as f64,
                100.0 * on_hits as f64 / trials as f64,
                rescues
            );
            total_rescues += rescues;
            total_trials += trials;
        }
    }

    println!();
    println!(
        "PILOT VERDICT: {} genuine rescue cases (AP-off fails, AP-on recovers) out of {} trials \
         across {} SNR points x {} stages.",
        total_rescues,
        total_trials,
        snrs.len(),
        Stage::ALL.len()
    );
    if total_rescues == 0 {
        println!(
            "  → ZERO rescue cases. This confirms Task 6's real-audio finding synthetically: at \
             this decoder's default LDPC/OSD strength, once the AP0-4 ladder's 56 known callsign \
             bits are injected, the codeword is essentially always recovered (or sync itself has \
             already failed) regardless of the extra 19 content bits Ap5 adds — there is no \
             SNR band, deep or shallow, where content-AP provides additional recall. This IS the \
             primary result; see the journal entry."
        );
    } else {
        println!("  → Non-zero rescue cases found — proceeding to the full §4A sweep is warranted.");
    }

    total_rescues
}

fn pilot_seed(stage: Stage, snr_db: f64, trial: usize) -> u64 {
    let stage_tag: u64 = match stage {
        Stage::WaitingForReport => 0x5741_4954, // "WAIT"
        Stage::WaitingForConfirmation => 0x434f_4e46, // "CONF"
    };
    let snr_bits = (snr_db * 100.0) as i64 as u64;
    0x4150_3521_u64 // "AP5!"
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ stage_tag
        ^ snr_bits.wrapping_mul(0x1000_0001)
        ^ (trial as u64).wrapping_mul(0x1_0000_0001)
}

// ---------------------------------------------------------------------------
// Full sweep (§4A step 2 recall + false-decode, step 3 knob tradeoff)
// ---------------------------------------------------------------------------

fn run_full_sweep() {
    let trials: usize = parse_env_usize("AP5_SWEEP_FULL_TRIALS", 60);
    let snrs = parse_env_f64_list(
        "AP5_SWEEP_FULL_SNRS",
        "-24,-22,-20,-18,-16,-14,-12,-10,-8,-6",
    );

    println!("\n## Phase 2 — Full §4A sweep (official range -24..-6 dB, 2 dB steps)");
    println!("  {} trials per (SNR, stage)\n", trials);

    println!("### Recall: AP-off vs AP-on (default knobs)");
    println!(
        "  {:<24} {:>7} {:>7} {:>9} {:>9} {:>9}",
        "stage", "snr_db", "trials", "off_hit%", "on_hit%", "rescues"
    );
    println!("  {:-<70}", "");
    let mut total_rescues = 0usize;
    for stage in Stage::ALL {
        for &snr in &snrs {
            let mut off_hits = 0usize;
            let mut on_hits = 0usize;
            let mut rescues = 0usize;
            for t in 0..trials {
                let seed = pilot_seed(stage, snr, t).wrapping_add(0xF0F0_F0F0);
                let outcome = run_recall_trial(stage, snr, seed, DEFAULT_KNOBS);
                off_hits += outcome.off_hit as usize;
                on_hits += outcome.on_hit as usize;
                rescues += outcome.is_rescue() as usize;
            }
            println!(
                "  {:<24} {:>+6.1} {:>7} {:>8.1}% {:>8.1}% {:>9}",
                stage.label(),
                snr,
                trials,
                100.0 * off_hits as f64 / trials as f64,
                100.0 * on_hits as f64 / trials as f64,
                rescues
            );
            total_rescues += rescues;
        }
    }
    println!("  Total rescues across full-range sweep: {total_rescues}");

    println!("\n### False-decode protocol (i): wrong-context injection over TRUE audio");
    println!(
        "  {:<24} {:>7} {:>7} {:>10} {:>14} {:>15}",
        "stage", "snr_db", "trials", "baseline_fp", "wrong_call_fp", "wrong_stage_fp"
    );
    println!("  {:-<80}", "");
    for stage in Stage::ALL {
        for &snr in &snrs {
            let mut baseline = 0usize;
            let mut wrong_call = 0usize;
            let mut wrong_stage = 0usize;
            for t in 0..trials {
                let seed = pilot_seed(stage, snr, t).wrapping_add(0x0BAD_C0DE);
                let o = run_wrong_context_trial(stage, snr, seed, DEFAULT_KNOBS);
                baseline += o.baseline_false as usize;
                wrong_call += o.wrong_call_false as usize;
                wrong_stage += o.wrong_stage_false as usize;
            }
            println!(
                "  {:<24} {:>+6.1} {:>7} {:>9}/{trials} {:>12}/{trials} {:>13}/{trials}",
                stage.label(),
                snr,
                trials,
                baseline,
                wrong_call,
                wrong_stage
            );
        }
    }

    println!("\n### False-decode protocol (ii): noise-only injection");
    println!(
        "  {:<24} {:>7} {:>7} {:>12} {:>10}",
        "stage", "snr_db", "trials", "baseline_fp", "on_fp"
    );
    println!("  {:-<60}", "");
    for stage in Stage::ALL {
        let (_, ref_rms) = modulate(&stage.truth_text(MY_CALL, DX_CALL));
        for &snr in &snrs {
            let mut baseline = 0usize;
            let mut on = 0usize;
            for t in 0..trials {
                let seed = pilot_seed(stage, snr, t).wrapping_add(0x900D_F00D);
                let o = run_noise_only_trial(stage, snr, seed, ref_rms, DEFAULT_KNOBS);
                baseline += o.baseline_false as usize;
                on += o.on_false as usize;
            }
            println!(
                "  {:<24} {:>+6.1} {:>7} {:>10}/{trials} {:>8}/{trials}",
                stage.label(),
                snr,
                trials,
                baseline,
                on
            );
        }
    }

    // Step 3: knob-vector tradeoff curve, at a fixed representative SNR
    // point (the middle of the official range).
    let repr_snr = snrs[snrs.len() / 2];
    println!(
        "\n### Knob tradeoff curve (representative SNR = {:.1} dB, WaitingForConfirmation stage, {} trials/knob-point)",
        repr_snr, trials
    );
    println!(
        "  NOTE: ap_llr_magnitude is config-surface-only in this codebase version (see module \
         doc) — expect zero effect on outcome; included to confirm that directly."
    );
    let knob_variants: Vec<(&str, Knobs)> = vec![
        ("default", DEFAULT_KNOBS),
        (
            "min_content_ap_confidence=0.45 (looser)",
            Knobs {
                min_content_ap_confidence: 0.45,
                ..DEFAULT_KNOBS
            },
        ),
        (
            "min_content_ap_confidence=0.80 (stricter)",
            Knobs {
                min_content_ap_confidence: 0.80,
                ..DEFAULT_KNOBS
            },
        ),
        (
            "max_ap_hypotheses=3 (tighter budget)",
            Knobs {
                max_ap_hypotheses: 3,
                ..DEFAULT_KNOBS
            },
        ),
        (
            "max_ap_hypotheses=16 (looser budget)",
            Knobs {
                max_ap_hypotheses: 16,
                ..DEFAULT_KNOBS
            },
        ),
        (
            "ap_llr_magnitude=8.0 (weaker prior, should be no-op)",
            Knobs {
                ap_llr_magnitude: 8.0,
                ..DEFAULT_KNOBS
            },
        ),
        (
            "ap_llr_magnitude=25.0 (stronger prior, should be no-op)",
            Knobs {
                ap_llr_magnitude: 25.0,
                ..DEFAULT_KNOBS
            },
        ),
    ];
    println!(
        "  {:<45} {:>9} {:>9} {:>10} {:>10}",
        "knob variant", "on_hit%", "rescues", "wrongcall_fp", "noise_fp"
    );
    println!("  {:-<90}", "");
    let stage = Stage::WaitingForConfirmation;
    let (_, ref_rms) = modulate(&stage.truth_text(MY_CALL, DX_CALL));
    for (label, knobs) in knob_variants {
        let mut on_hits = 0usize;
        let mut rescues = 0usize;
        let mut wrong_call_fp = 0usize;
        let mut noise_fp = 0usize;
        for t in 0..trials {
            let seed = pilot_seed(stage, repr_snr, t).wrapping_add(0xC0FF_EE00);
            let r = run_recall_trial(stage, repr_snr, seed, knobs);
            on_hits += r.on_hit as usize;
            rescues += r.is_rescue() as usize;
            let fc = run_wrong_context_trial(stage, repr_snr, seed, knobs);
            wrong_call_fp += fc.wrong_call_false as usize;
            let no = run_noise_only_trial(stage, repr_snr, seed, ref_rms, knobs);
            noise_fp += no.on_false as usize;
        }
        println!(
            "  {:<45} {:>8.1}% {:>9} {:>9}/{trials} {:>8}/{trials}",
            label,
            100.0 * on_hits as f64 / trials as f64,
            rescues,
            wrong_call_fp,
            noise_fp
        );
    }
}

fn print_help() {
    println!("ap5_content_recall_fp_sweep — Task 7 Ap5 (content-AP) eval harness\n");
    println!("USAGE:");
    println!("  cargo run --release -p pancetta-research --example ap5_content_recall_fp_sweep -- [FLAGS]\n");
    println!("FLAGS:");
    println!("  --help    Print this help and exit.");
    println!(
        "  --full    Run the full §4A sweep (official -24..-6 dB range, false-decode protocols,"
    );
    println!("            knob tradeoff curve) in addition to the pilot. Without this flag,");
    println!("            only the pilot runs, and the full sweep is skipped automatically if");
    println!("            the pilot finds zero rescue cases (per the Task 6 carry-forward).\n");
    println!("ENV VARS (see module doc comment for full list/defaults):");
    println!("  AP5_SWEEP_PILOT_TRIALS, AP5_SWEEP_PILOT_SNRS, AP5_SWEEP_FULL_TRIALS, AP5_SWEEP_FULL_SNRS");
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    let force_full = args.iter().any(|a| a == "--full");

    println!("# Task 7 — Ap5 content-AP recall/false-decode-vs-SNR eval harness\n");

    // Sanity check: the truth texts used throughout this harness must
    // actually be members of enumerate_a8_expected_texts's own output for
    // their stage, or the harness would be measuring an unreachable
    // hypothesis.
    for stage in Stage::ALL {
        let truth = stage.truth_text(MY_CALL, DX_CALL);
        let enumerated = enumerate_a8_expected_texts(MY_CALL, DX_CALL, stage.progress());
        assert!(
            enumerated.contains(&truth),
            "harness bug: truth_text {truth:?} for stage {stage:?} is not in \
             enumerate_a8_expected_texts's own output {enumerated:?}"
        );
    }

    let start = Instant::now();
    let rescues = run_pilot();
    println!("\n(pilot elapsed: {:.1}s)", start.elapsed().as_secs_f64());

    if rescues > 0 || force_full {
        if rescues == 0 && force_full {
            println!(
                "\n--full forced despite zero pilot rescues — running the full sweep anyway for \
                 completeness/documentation."
            );
        }
        let full_start = Instant::now();
        run_full_sweep();
        println!(
            "\n(full sweep elapsed: {:.1}s)",
            full_start.elapsed().as_secs_f64()
        );
    } else {
        println!(
            "\nSkipping the full §4A sweep — the pilot found zero rescue cases across a deep \
             SNR sweep, which conclusively answers the ship-gate question (no) before the rest \
             of the sweep would add information. Pass --full to force it anyway."
        );
    }

    println!("\n(total elapsed: {:.1}s)", start.elapsed().as_secs_f64());
    Ok(())
}
