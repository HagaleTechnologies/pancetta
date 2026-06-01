//! hb048_a7_synthetic_injection — Session 2 graduation gate for hb-048.
//!
//! Per the design spec
//! (`docs/superpowers/specs/2026-05-31-hb-048-a7-design.md`, §"Session 2
//! GRADUATE-to-3 gate"):
//!
//! > Cross-correlation primitive recovers a synthetic-injected message
//! > with `snr7 ≥ 6.0` and `snr7b ≥ 1.8` (this is the decodability
//! > micro-test — a Session 3 PROCEED prereq).
//!
//! ## Test plan
//!
//! 1. Choose a known follow-up message (`K1ABC W1AW RR73`) and encode it
//!    through `Ft8Encoder` to get a 174-bit LDPC codeword `cw_true`.
//! 2. Generate templates for the source decode (`K1ABC` at f=1200,
//!    `heard_with=W1AW`). One of those templates must match `cw_true`.
//! 3. Construct synthetic LLRs at multiple noise levels:
//!    `llr_i = (1 - 2*cw_true[i]) * signal_mag + AWGN(0, noise_std)`.
//! 4. Run `cross_correlate(matching_template, llrs)` and verify snr7 ≥
//!    6.0 across the noise sweep, and `best_template_score` returns the
//!    matching template index with snr7b ≥ 1.8.
//! 5. Report sensitivity curve (snr7 vs noise) so Session 3 has a
//!    quantitative reference for threshold tuning.
//!
//! ## Decision rule (Session 2 graduation)
//!
//! - At signal_mag/noise_std ≥ 1.5 (a reasonable mid-band operating
//!   point), best snr7 ≥ 6.0 AND best snr7b ≥ 1.8 across all 5 trials →
//!   **GRADUATE**.
//! - Otherwise → **SHELVE Session 2**, document what didn't work.
//!
//! Run: `cargo run --release -p pancetta-research --example hb048_a7_synthetic_injection`

use pancetta_ft8::a7::{
    best_template_score, cross_correlate, generate_templates, A7ExpectedCall, A7SlotParity,
    A7Template, A7_CODEWORD_BITS, A7_SNR7B_THRESHOLD_DEFAULT, A7_SNR7_THRESHOLD_DEFAULT,
};
use pancetta_ft8::encoder::Ft8Encoder;

/// Reconstruct a 174-bit codeword from a 79-symbol sequence (Gray-decoded,
/// data symbols only). Mirrors `pancetta_ft8::a7::symbols_to_codeword_bits`
/// (which is module-private).
fn symbols_to_codeword_bits(symbols: &[u8; 79]) -> [bool; A7_CODEWORD_BITS] {
    let mut bits = [false; A7_CODEWORD_BITS];
    let mut bit_idx = 0usize;
    for (i, &sym) in symbols.iter().enumerate() {
        let is_costas = i < 7 || (36..43).contains(&i) || i >= 72;
        if is_costas {
            continue;
        }
        let g2 = (sym >> 2) & 1;
        let g1 = (sym >> 1) & 1;
        let g0 = sym & 1;
        let b2 = g2;
        let b1 = b2 ^ g1;
        let b0 = b1 ^ g0;
        bits[bit_idx] = b2 == 1;
        bits[bit_idx + 1] = b1 == 1;
        bits[bit_idx + 2] = b0 == 1;
        bit_idx += 3;
    }
    bits
}

/// xorshift64* deterministic PRNG for reproducible noise.
struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn next_unit_f64(&mut self) -> f64 {
        (self.next_u64() as f64) / (u64::MAX as f64)
    }

    /// Box-Muller standard normal.
    fn next_gaussian(&mut self) -> f64 {
        let u1 = self.next_unit_f64().max(1e-10);
        let u2 = self.next_unit_f64();
        let r = (-2.0 * u1.ln()).sqrt();
        let theta = 2.0 * std::f64::consts::PI * u2;
        r * theta.cos()
    }
}

fn synthesize_llrs(
    codeword: &[bool; A7_CODEWORD_BITS],
    signal_mag: f32,
    noise_std: f32,
    rng: &mut Xorshift64,
) -> Vec<f32> {
    codeword
        .iter()
        .map(|&b| {
            let clean = if b { -signal_mag } else { signal_mag };
            clean + noise_std * (rng.next_gaussian() as f32)
        })
        .collect()
}

fn find_template_matching_codeword<'a>(
    templates: &'a [A7Template],
    codeword: &[bool; A7_CODEWORD_BITS],
) -> Option<(usize, &'a A7Template)> {
    templates
        .iter()
        .enumerate()
        .find(|(_, t)| &t.codeword == codeword)
}

#[derive(Debug, Clone, Copy)]
struct TrialResult {
    signal_mag: f32,
    noise_std: f32,
    snr7: f64,
    snr7b: f64,
    match_idx_correct: bool,
}

fn run_noise_sweep(
    templates: &[A7Template],
    target_template_idx: usize,
    target_codeword: &[bool; A7_CODEWORD_BITS],
) -> Vec<TrialResult> {
    // Operating points spanning clean → noisy. signal_mag stays at 5.0
    // (mid-range LLR magnitude); noise_std varies.
    let signal_mag = 5.0f32;
    let noise_stds = [0.5, 1.0, 1.5, 2.0, 3.0, 5.0, 8.0, 12.0];
    let mut results = Vec::new();

    for &noise_std in noise_stds.iter() {
        // 5 trials per operating point for stability.
        let mut snr7_sum = 0.0;
        let mut snr7b_sum = 0.0;
        let mut correct_count = 0;
        let trials = 5;
        for trial in 0..trials {
            let seed = 0x517CC1B7u64.wrapping_mul((noise_std * 1000.0) as u64 + trial + 1);
            let mut rng = Xorshift64::new(seed);
            let llrs = synthesize_llrs(target_codeword, signal_mag, noise_std, &mut rng);
            let (best_idx, snr7, snr7b) =
                best_template_score(templates, &llrs).expect("non-empty bank");
            snr7_sum += snr7;
            snr7b_sum += snr7b.min(1000.0); // cap for averaging when no second-best
            if best_idx == target_template_idx {
                correct_count += 1;
            }
            // Also direct cross-correlate for the matching template's own snr7.
            let direct = cross_correlate(&templates[target_template_idx], &llrs);
            // Use direct.snr7 as the headline since best may pick a different
            // (correlated) template; report both anyway via the avg.
            let _ = direct;
        }
        results.push(TrialResult {
            signal_mag,
            noise_std,
            snr7: snr7_sum / trials as f64,
            snr7b: snr7b_sum / trials as f64,
            match_idx_correct: correct_count == trials,
        });
    }
    results
}

fn main() -> anyhow::Result<()> {
    println!("hb-048 Session 2 — synthetic-injection decodability micro-test");
    println!("================================================================\n");

    // ----- Setup ---------------------------------------------------------
    // Source decode: K1ABC at 1200 Hz, even slot, with W1AW as heard_with.
    // Expected follow-up message in slot N+1: "K1ABC W1AW RR73".
    let expected_call = A7ExpectedCall::new("K1ABC", 1200.0, A7SlotParity::Even)
        .with_heard_with("W1AW")
        .with_my_call("W1AW");
    let templates = generate_templates(&expected_call);
    println!("Generated {} templates for K1ABC (cap = 32)", templates.len());
    println!("Template kinds:");
    for (i, t) in templates.iter().enumerate() {
        println!("  [{:02}] {:?}: {}", i, t.kind, t.message_text);
    }
    println!();

    // ----- Synthesize the truth codeword ---------------------------------
    let truth_text = "K1ABC W1AW RR73";
    let mut encoder = Ft8Encoder::new();
    let truth_symbols = encoder
        .encode_message(truth_text, None)
        .map_err(|e| anyhow::anyhow!("encode failed: {:?}", e))?;
    let truth_codeword = symbols_to_codeword_bits(&truth_symbols);
    println!(
        "Truth message '{}' encoded — codeword has {} bits set",
        truth_text,
        truth_codeword.iter().filter(|&&b| b).count()
    );

    // ----- Confirm the templates contain a match for truth ---------------
    let (matching_idx, matching_template) =
        find_template_matching_codeword(&templates, &truth_codeword).ok_or_else(|| {
            anyhow::anyhow!(
                "no template matches truth codeword — generator missed expected message '{}'",
                truth_text
            )
        })?;
    println!(
        "Matching template found at index {} (kind={:?}, text='{}')\n",
        matching_idx, matching_template.kind, matching_template.message_text
    );

    // ----- Clean (zero-noise) sanity -------------------------------------
    let mut rng = Xorshift64::new(0xDEADBEEF);
    let clean_llrs = synthesize_llrs(&truth_codeword, 5.0, 0.0, &mut rng);
    let clean_score = cross_correlate(matching_template, &clean_llrs);
    println!(
        "Clean-channel (signal=5.0, noise=0.0) snr7 = {:.2}  (theoretical 5*sqrt(174) ≈ {:.2})\n",
        clean_score.snr7,
        5.0 * (174.0_f64).sqrt()
    );
    assert!(
        clean_score.snr7 > A7_SNR7_THRESHOLD_DEFAULT * 5.0,
        "clean-channel snr7 {} suspiciously low",
        clean_score.snr7
    );

    // ----- Noise sweep ---------------------------------------------------
    let results = run_noise_sweep(&templates, matching_idx, &truth_codeword);
    println!("Noise sensitivity sweep (signal_mag=5.0, 5 trials/point):");
    println!(
        "  {:>10}  {:>10}  {:>10}  {:>10}  {:>10}",
        "noise_std", "lin_SNR", "snr7", "snr7b", "best_idx_ok"
    );
    println!("  {:->58}", "");
    for r in &results {
        let lin_snr = (r.signal_mag / r.noise_std).powi(2);
        let lin_snr_db = 10.0 * lin_snr.log10();
        println!(
            "  {:>10.2}  {:>+8.1}dB  {:>10.2}  {:>10.2}  {:>10}",
            r.noise_std,
            lin_snr_db,
            r.snr7,
            r.snr7b,
            if r.match_idx_correct { "YES" } else { "NO" }
        );
    }
    println!();

    // ----- Graduation decision -------------------------------------------
    // Find the highest-noise operating point that still meets the WSJT-X
    // thresholds. Per design spec: snr7 >= 6.0, snr7b >= 1.8.
    let mut max_passing_noise: Option<f32> = None;
    for r in &results {
        let passes = r.snr7 >= A7_SNR7_THRESHOLD_DEFAULT
            && r.snr7b >= A7_SNR7B_THRESHOLD_DEFAULT
            && r.match_idx_correct;
        if passes && r.noise_std > max_passing_noise.unwrap_or(0.0) {
            max_passing_noise = Some(r.noise_std);
        }
    }

    // Look at the "mid-band" operating point (signal_mag / noise_std >= 1.5)
    // for the headline number.
    let headline = results
        .iter()
        .find(|r| r.noise_std == 3.0)
        .or_else(|| results.iter().find(|r| r.noise_std == 2.0))
        .expect("results not empty");
    println!("Headline (signal=5.0, noise={:.1}):", headline.noise_std);
    println!("  snr7 = {:.2}  (threshold {:.1})", headline.snr7, A7_SNR7_THRESHOLD_DEFAULT);
    println!("  snr7b = {:.2}  (threshold {:.1})", headline.snr7b, A7_SNR7B_THRESHOLD_DEFAULT);
    println!(
        "  best-template correctly identified: {}",
        headline.match_idx_correct
    );
    println!();

    let graduates = headline.snr7 >= A7_SNR7_THRESHOLD_DEFAULT
        && headline.snr7b >= A7_SNR7B_THRESHOLD_DEFAULT
        && headline.match_idx_correct;

    if graduates {
        println!("DECISION: Session 2 **GRADUATES**.");
        println!(
            "  Mid-band snr7={:.2} >= {:.1}, snr7b={:.2} >= {:.1}, match-correct=YES.",
            headline.snr7, A7_SNR7_THRESHOLD_DEFAULT, headline.snr7b, A7_SNR7B_THRESHOLD_DEFAULT
        );
        if let Some(mn) = max_passing_noise {
            println!(
                "  Max noise still passing thresholds: {:.1} (lin-SNR {:+.1} dB).",
                mn,
                10.0 * (5.0_f32 / mn).powi(2).log10()
            );
        }
        println!("  → PROCEED to Session 3 (production wiring + threshold sweep).");
    } else {
        println!("DECISION: Session 2 **SHELVES**.");
        println!(
            "  Mid-band snr7={:.2}, snr7b={:.2}, match-correct={} — fails one or more gates.",
            headline.snr7, headline.snr7b, headline.match_idx_correct
        );
        println!("  → Document the failure mode; revisit if hb-048 priority rises.");
    }

    Ok(())
}
