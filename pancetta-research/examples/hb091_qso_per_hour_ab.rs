//! hb-091 Session 3c — QSO/hour A/B simulation.
//!
//! Translates the S3 mechanism diagnostic's empirical decoder wall-clock
//! distribution (M4 Mac Mini, hard-200) into an operational QSO/hour
//! estimate for the "with scoped fast-path" vs "without" arms, under
//! three propagation/fade scenarios.
//!
//! ## Why a simulation
//!
//! The S3 latency profile (`decode_latency_profile.rs`, 2026-06-04)
//! shows full-decode p95=2132ms / p99=2332ms BUSTS pancetta's
//! 2000ms slot budget (DSP fires at t=13.0; next-slot TX boundary at
//! t=15.0). When the decoder busts, TX is late on the next slot;
//! pancetta's TX scheduler skip-ahead-cursors the leading samples (up
//! to `tx_late_max_ms=8s` per coordinator/tx.rs), truncating the
//! transmitted audio's leading symbols. Partner's decoder loses
//! leading sync information → reduced decode success at their end →
//! some legs require retry → fewer QSOs/hour.
//!
//! Scoped fast-path p99=866ms reliably stays inside the slot budget.
//! No truncation, no missed-leg penalty.
//!
//! The simulation closes the loop: empirical wall-clock → tx-late →
//! truncation penalty → partner decode success rate → QSO/hour.
//!
//! ## Method
//!
//! For each arm (`full_only`, `scoped_fast_path`):
//!   1. For each of N_TRIALS=2000 QSO trials:
//!      a. For each of 4 legs (CQ→resp, resp→R+report,
//!         R+report→RR73, RR73→73):
//!         - Sample decode_ms from arm's empirical distribution
//!           (linear interpolation between percentiles)
//!         - tx_late_ms = max(0, decode_ms - 2000)
//!         - partner_decode_prob = 1 - penalty(tx_late_ms, scenario)
//!         - If RNG draw fails the prob: leg failed, +15s slot retry,
//!           re-roll. Up to MAX_RETRIES=3 before abandoning the QSO.
//!         - On success: +15s slot, advance to next leg
//!      b. Total QSO time = sum of slot waits (incl. retries).
//!      c. Successful QSO = all 4 legs completed within retry budget.
//!   2. Aggregate: QSOs/hour = 3600 / mean(total_qso_time_s).
//!
//! Three fade scenarios:
//!   - `no_fade`: partner decode 100% regardless of tx-late
//!   - `moderate`: tx_late in [100,500)ms → 30% penalty; ≥500ms → 70%
//!   - `heavy`: tx_late in [50,300)ms → 50% penalty; ≥300ms → 90%
//!
//! Decision rule (S3 deployment gate):
//!   - PROCEED-default-on if (qsos_per_hour scoped) / (qsos_per_hour full)
//!     ≥ 1.10 in ANY scenario.
//!   - SHELVE if no scenario clears the +10% bar. Primitive + config
//!     flag stay; production fast-path off by default.
//!
//! ## Empirical distribution (M4 Mac Mini, hard-200, 2026-06-04)
//!
//! From `decode_latency_profile.rs` full run:
//!
//! | percentile | full ms | scoped ms |
//! |---|---|---|
//! | p50 | 862 | 329 |
//! | p90 | 1980 | 605 |
//! | p95 | 2132 | 712 |
//! | p99 | 2332 | 866 |
//! | max | 2446 | 917 |
//! | min | ~300 | ~298 |
//!
//! ## Caveats
//!
//! - The truncation→partner-decode model is heuristic; real-world
//!   propagation effects (QSB, multipath) compound differently.
//! - Slot retry counts are bounded to 3; real ops may give up sooner
//!   or persist longer.
//! - The full distribution is bimodal (easy WAVs cluster near 600ms,
//!   hard WAVs at 1500-2400ms); linear interpolation between
//!   percentiles smooths over this. Result: simulation may slightly
//!   over-estimate the "easy" arm's busts and under-estimate the
//!   "hard" arm's. Bounded by max=2446 anyway.
//! - On lower-tier hardware (MiniPC, ARMv7, etc.) the full
//!   distribution shifts right → bigger relative gain from scoped.
//!   This sim represents the M4 reference; future MiniPC tier would
//!   re-run with empirical numbers from that machine.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example hb091_qso_per_hour_ab

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

const N_TRIALS: usize = 2000;
const LEGS_PER_QSO: usize = 4;
const MAX_RETRIES_PER_LEG: usize = 3;
const SLOT_S: f64 = 15.0;
const SLOT_BUDGET_MS: f64 = 2000.0;
const SEED: u64 = 091_2026_06_04;

#[derive(Debug, Clone, Copy)]
struct EmpiricalDist {
    /// Sorted (percentile, value) pairs. p in [0.0, 1.0].
    points: &'static [(f64, f64)],
}

impl EmpiricalDist {
    /// Sample by inverse-CDF via linear interpolation between percentile points.
    fn sample(&self, rng: &mut impl Rng) -> f64 {
        let u: f64 = rng.gen();
        let p = &self.points;
        // Find bracket [p_lo, p_hi] containing u
        for w in p.windows(2) {
            let (p_lo, v_lo) = w[0];
            let (p_hi, v_hi) = w[1];
            if u <= p_hi {
                if (p_hi - p_lo).abs() < f64::EPSILON {
                    return v_hi;
                }
                let t = (u - p_lo) / (p_hi - p_lo);
                return v_lo + t * (v_hi - v_lo);
            }
        }
        p.last().map(|x| x.1).unwrap_or(0.0)
    }
}

// M4 Mac Mini empirical distributions (hard-200, 2026-06-04).
const FULL_DIST: EmpiricalDist = EmpiricalDist {
    points: &[
        (0.00, 300.0),
        (0.50, 862.0),
        (0.90, 1980.0),
        (0.95, 2132.0),
        (0.99, 2332.0),
        (1.00, 2446.0),
    ],
};

const SCOPED_DIST: EmpiricalDist = EmpiricalDist {
    points: &[
        (0.00, 298.0),
        (0.50, 329.0),
        (0.90, 605.0),
        (0.95, 712.0),
        (0.99, 866.0),
        (1.00, 917.0),
    ],
};

#[derive(Debug, Clone, Copy)]
enum FadeScenario {
    NoFade,
    Moderate,
    Heavy,
}

impl FadeScenario {
    fn name(&self) -> &'static str {
        match self {
            FadeScenario::NoFade => "no-fade",
            FadeScenario::Moderate => "moderate",
            FadeScenario::Heavy => "heavy",
        }
    }

    /// Probability of partner-side decode FAILURE given our TX-late truncation in ms.
    fn fail_prob(&self, tx_late_ms: f64) -> f64 {
        match self {
            FadeScenario::NoFade => 0.0,
            FadeScenario::Moderate => {
                if tx_late_ms < 100.0 {
                    0.0
                } else if tx_late_ms < 500.0 {
                    0.30
                } else {
                    0.70
                }
            }
            FadeScenario::Heavy => {
                if tx_late_ms < 50.0 {
                    0.0
                } else if tx_late_ms < 300.0 {
                    0.50
                } else {
                    0.90
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Arm {
    FullOnly,
    ScopedFastPath,
}

impl Arm {
    fn name(&self) -> &'static str {
        match self {
            Arm::FullOnly => "full only",
            Arm::ScopedFastPath => "scoped fast-path",
        }
    }

    fn sample_decode_ms(&self, rng: &mut impl Rng) -> f64 {
        match self {
            Arm::FullOnly => FULL_DIST.sample(rng),
            Arm::ScopedFastPath => SCOPED_DIST.sample(rng),
        }
    }
}

/// Simulate one leg; return (slots_consumed, succeeded).
fn simulate_leg(arm: Arm, fade: FadeScenario, rng: &mut impl Rng) -> (u32, bool) {
    let mut slots = 1u32; // the partner-TX slot we're decoding
    for _attempt in 0..=MAX_RETRIES_PER_LEG {
        let decode_ms = arm.sample_decode_ms(rng);
        let tx_late_ms = (decode_ms - SLOT_BUDGET_MS).max(0.0);
        let fail_prob = fade.fail_prob(tx_late_ms);
        if rng.gen::<f64>() >= fail_prob {
            // partner decoded our TX successfully → leg done
            // We consume one MORE slot for our TX itself
            return (slots + 1, true);
        }
        // partner missed our TX: +1 slot for our re-TX next cycle
        slots += 2; // their wait + our retry
    }
    (slots, false)
}

/// Simulate one QSO; return total slots consumed and whether all legs succeeded.
fn simulate_qso(arm: Arm, fade: FadeScenario, rng: &mut impl Rng) -> (u32, bool) {
    let mut total_slots = 0u32;
    for _ in 0..LEGS_PER_QSO {
        let (slots, ok) = simulate_leg(arm, fade, rng);
        total_slots += slots;
        if !ok {
            return (total_slots, false);
        }
    }
    (total_slots, true)
}

fn qsos_per_hour(arm: Arm, fade: FadeScenario, rng: &mut impl Rng) -> (f64, f64) {
    let mut total_slots: u64 = 0;
    let mut successful = 0u64;
    for _ in 0..N_TRIALS {
        let (slots, ok) = simulate_qso(arm, fade, rng);
        total_slots += slots as u64;
        if ok {
            successful += 1;
        }
    }
    let total_time_s = total_slots as f64 * SLOT_S;
    let qsos_per_hour = if total_time_s > 0.0 {
        3600.0 * successful as f64 / total_time_s
    } else {
        0.0
    };
    let success_rate = successful as f64 / N_TRIALS as f64;
    (qsos_per_hour, success_rate)
}

fn main() {
    println!("== hb-091 Session 3c — QSO/hour A/B (M4 Mac Mini distributions) ==");
    println!("  N_TRIALS:        {N_TRIALS} QSO simulations per (arm, scenario)");
    println!("  legs per QSO:    {LEGS_PER_QSO}");
    println!("  max retries/leg: {MAX_RETRIES_PER_LEG}");
    println!("  slot budget:     {SLOT_BUDGET_MS:.0}ms (DSP fires at t=13.0, next slot at t=15.0)");
    println!();

    let scenarios = [
        FadeScenario::NoFade,
        FadeScenario::Moderate,
        FadeScenario::Heavy,
    ];
    let arms = [Arm::FullOnly, Arm::ScopedFastPath];

    println!("{:-<78}", "");
    println!(
        "{:<12} {:<18} {:>10} {:>14} {:>14}",
        "scenario", "arm", "succ_rate", "QSOs/hour", "vs full"
    );
    println!("{:-<78}", "");

    let mut decision_proceed = false;
    let mut best_ratio: f64 = 0.0;

    for scenario in scenarios {
        let mut rng_full = StdRng::seed_from_u64(SEED);
        let mut rng_scoped = StdRng::seed_from_u64(SEED);

        let (qph_full, sr_full) = qsos_per_hour(Arm::FullOnly, scenario, &mut rng_full);
        let (qph_scoped, sr_scoped) = qsos_per_hour(Arm::ScopedFastPath, scenario, &mut rng_scoped);

        let ratio = if qph_full > 0.0 {
            qph_scoped / qph_full
        } else {
            f64::INFINITY
        };

        println!(
            "{:<12} {:<18} {:>10.2} {:>14.2} {:>14}",
            scenario.name(),
            arms[0].name(),
            sr_full,
            qph_full,
            "—"
        );
        println!(
            "{:<12} {:<18} {:>10.2} {:>14.2} {:>13.2}x",
            "",
            arms[1].name(),
            sr_scoped,
            qph_scoped,
            ratio
        );

        if ratio >= 1.10 {
            decision_proceed = true;
        }
        if ratio > best_ratio {
            best_ratio = ratio;
        }
    }

    println!("{:-<78}", "");
    println!();
    println!("== Decision (S3 deployment gate) ==");
    println!("  PROCEED-default-on if ratio >= 1.10 in ANY scenario.");
    println!("  best ratio observed: {:.2}x", best_ratio);
    println!();
    println!(
        "AUTO-DECISION: {}",
        if decision_proceed {
            "PROCEED-default-on"
        } else {
            "SHELVE (keep primitive + env-var gate)"
        }
    );
}
