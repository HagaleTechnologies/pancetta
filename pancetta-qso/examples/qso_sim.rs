//! Runnable QSO simulation sandbox.
//!
//! A "vehicle" for the durable [`pancetta_qso::sim`] harness: it authors a few
//! real-world scenarios, drives the **real** QSO engine through them with a
//! virtual band + virtual clock (no audio, no rig, no waiting), and prints each
//! one's human-readable slot-by-slot timeline (`slot | RX | TX | events`).
//!
//! Use it as a fast sandbox to try new ideas against the engine:
//!
//! ```text
//! cargo run -p pancetta-qso --example qso_sim
//! ```
//!
//! For the asserted, permanent catalog see `tests/qso_scenarios.rs`.

use pancetta_core::ResponseStep;
use pancetta_qso::sim::{Scenario, Sim, SimAction};

const US: &str = "K5ARH";
const GRID: &str = "EM10";

#[tokio::main]
async fn main() {
    println!("=== Pancetta QSO Simulation Sandbox ===");
    println!("Station: {US} @ {GRID}  (virtual band + virtual clock; no audio/rig)\n");

    clean_answer_imperative().await;
    fade_then_complete_imperative().await;
    scripted_clean_cq().await;
    crowded_band_tx_selection().await;

    println!("All sandbox scenarios ran to completion.");
}

/// Scenario A — we call a CQer and run a clean exchange to completion, written
/// in the imperative style (reads like an on-air log).
async fn clean_answer_imperative() {
    banner("A. Clean answer — we call VB7F's CQ and complete the QSO");
    let mut sim = Sim::new(US, Some(GRID)).await;

    // Slot 0: VB7F is calling CQ; operator calls them.
    sim.inject_decode("CQ VB7F DO33", 1500.0, -8.0, 0.1);
    sim.call_station("VB7F", 1500.0).await;
    sim.tick().await;

    // Slot 1: their report → we auto-send our R-report.
    sim.inject_decode("K5ARH VB7F -12", 1500.0, -8.0, 0.1);
    sim.tick().await;

    // Slot 2: their RR73 → we complete + send our 73.
    sim.inject_decode("K5ARH VB7F RR73", 1500.0, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    print!("{tl}");
    println!("  => completed with VB7F: {}\n", tl.completed_with("VB7F"));
}

/// Scenario B — the DX fades for a slot mid-QSO; keep-calling bridges the gap
/// and the QSO still completes.
async fn fade_then_complete_imperative() {
    banner("B. Weak/fade — VB7F drops a slot; keep-call bridges it to completion");
    let mut sim = Sim::new(US, Some(GRID)).await;

    sim.inject_decode("CQ VB7F DO33", 1500.0, -20.0, 0.4);
    sim.call_station("VB7F", 1500.0).await;
    sim.tick().await;

    // Fade: nothing decoded this slot.
    sim.tick().await;

    sim.inject_decode("K5ARH VB7F -22", 1500.0, -22.0, 0.5);
    sim.tick().await;

    // Fade again — we keep-call our R-report.
    sim.tick().await;

    sim.inject_decode("K5ARH VB7F RR73", 1500.0, -21.0, 0.5);
    sim.tick().await;

    let tl = sim.into_timeline();
    print!("{tl}");
    println!(
        "  => completed with VB7F despite fades: {}\n",
        tl.completed_with("VB7F")
    );
}

/// Scenario C — same clean exchange but authored declaratively as a [`Scenario`]
/// script and run with [`Sim::run_scenario`]; here WE are the CQer.
async fn scripted_clean_cq() {
    banner("C. Scripted — we CQ, W5XO answers, run to completion (declarative)");
    let sim = Sim::new(US, Some(GRID)).await;

    let scenario = Scenario::new("clean-cq")
        .at(0, vec![SimAction::Cq { freq_hz: 1200.0 }])
        .at(
            1,
            vec![SimAction::Inject {
                text: "K5ARH W5XO EM12".to_string(),
                freq_hz: 1200.0,
                snr_db: -10.0,
                dt: 0.1,
            }],
        )
        .at(
            2,
            vec![SimAction::Inject {
                text: "K5ARH W5XO R-09".to_string(),
                freq_hz: 1200.0,
                snr_db: -10.0,
                dt: 0.1,
            }],
        )
        .at(
            3,
            vec![SimAction::Inject {
                text: "K5ARH W5XO 73".to_string(),
                freq_hz: 1200.0,
                snr_db: -10.0,
                dt: 0.1,
            }],
        );

    let tl = sim.run_scenario(&scenario).await;
    print!("{tl}");
    println!("  => completed with W5XO: {}\n", tl.completed_with("W5XO"));
    // Demonstrate the context-aware step selector too.
    let _ = ResponseStep::Grid;
}

/// Scenario D — a crowded band: the allocator must steer our TX clear of the
/// occupied region.
async fn crowded_band_tx_selection() {
    banner("D. Crowded band — allocator picks a clear TX offset");
    let mut sim = Sim::new(US, Some(GRID)).await;

    let crowd: Vec<(&str, f64)> = vec![
        ("CQ A1AA AA00", 700.0),
        ("CQ B2BB BB11", 900.0),
        ("CQ C3CC CC22", 1100.0),
        ("CQ D4DD DD33", 1300.0),
        ("CQ E5EE EE44", 1500.0),
        ("CQ F6FF FF55", 1700.0),
    ];
    sim.inject_crowd(&crowd);
    sim.tick().await;

    let chosen = sim.choose_tx_offset(None).await;
    print!("{}", sim.timeline());
    println!(
        "  => allocator chose TX offset {:.0} Hz (crowd at 700–1700 Hz)\n",
        chosen
    );
}

fn banner(title: &str) {
    println!("------------------------------------------------------------");
    println!("{title}");
    println!("------------------------------------------------------------");
}
