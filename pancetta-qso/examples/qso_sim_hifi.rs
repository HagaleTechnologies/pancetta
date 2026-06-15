//! High-fidelity QSO simulation demo.
//!
//! Unlike `examples/qso_sim.rs` (which injects perfect decoded *text*), this
//! sandbox injects transmitted **signals**: each scenario specifies a message +
//! SNR + fading, and the harness runs it through the *real* pancetta-ft8
//! pipeline — encode → modulate → apply fading → add calibrated AWGN →
//! `Ft8Decoder::decode_window`. At low SNR or under deep fading the message can
//! MISS entirely, exactly as on a real band. Whatever the decoder actually
//! recovers (its text, freq, and measured SNR) is what drives the QSO engine.
//!
//! Run with:
//!
//! ```bash
//! cargo run -p pancetta-qso --features sim-hifi --example qso_sim_hifi
//! ```
//!
//! It runs three scenarios and prints, for each, the per-slot
//! decoded-vs-missed timeline and whether the QSO completed (or how it ended).

use pancetta_qso::sim::{FadingProfile, Sim};

/// Helper: a strong, no-fading signal that should always decode.
const STRONG_SNR: f32 = 6.0;

#[tokio::main]
async fn main() {
    println!("\n############################################################");
    println!("# High-fidelity QSO sim — real encode -> noise/fading -> decode");
    println!("############################################################");

    scenario_strong_always_decodes().await;
    scenario_marginal_some_miss().await;
    scenario_fading_drop_then_return().await;

    println!("\nAll high-fidelity scenarios ran. (decoded-vs-missed shown above)\n");
}

/// A strong DX answers our manual call; every slot decodes; the QSO completes.
async fn scenario_strong_always_decodes() {
    println!("\n=== Scenario 1: STRONG signal, no fading — reliably decodes ===");
    let mut sim = Sim::new("K5ARH", Some("EM10")).await.with_hifi_seed(1);
    let dx = "VB7F";
    let freq = 1200.0;

    // Operator calls the DX (manual keep-call).
    sim.call_station(dx, freq).await;

    // DX answers with our-call/their-call/grid, then report, then RR73 — all
    // strong, all decoded — interleaved with our keep-call/auto-reply slots.
    sim.inject_signal(
        &format!("K5ARH {dx} EM73"),
        freq,
        STRONG_SNR,
        FadingProfile::None,
    );
    sim.tick().await;

    sim.inject_signal(
        &format!("K5ARH {dx} -12"),
        freq,
        STRONG_SNR,
        FadingProfile::None,
    );
    sim.tick().await;

    sim.inject_signal(
        &format!("K5ARH {dx} RR73"),
        freq,
        STRONG_SNR,
        FadingProfile::None,
    );
    sim.tick().await;

    sim.tick_n(2).await;

    let tl = sim.into_timeline();
    print!("{tl}");
    println!(
        "  -> signals decoded: {}, missed: {}; completed with {dx}: {}",
        tl.signals_decoded(),
        tl.signals_missed(),
        tl.completed_with(dx),
    );
}

/// A marginal DX near the decode floor: some slots decode, some MISS. The QSO
/// engine keep-calls through the misses; whether it completes depends on which
/// slots survive, so we just report the outcome.
async fn scenario_marginal_some_miss() {
    println!("\n=== Scenario 2: MARGINAL signal (-20..-24 dB) — some slots MISS ===");
    let mut sim = Sim::new("K5ARH", Some("EM10")).await.with_hifi_seed(7);
    let dx = "VB7F";
    let freq = 1500.0;

    sim.call_station(dx, freq).await;

    // The DX tries to answer our call across several slots at marginal SNR.
    // Some will decode, some will be lost in the noise. The keep-call re-arm
    // fires every slot regardless.
    for snr in [-20.0f32, -24.0, -22.0, -20.0, -23.0] {
        let msg = format!("K5ARH {dx} EM73");
        let out = sim.inject_signal(&msg, freq, snr, FadingProfile::None);
        println!(
            "    slot {}: requested {:+.0} dB -> {}",
            out.slot,
            out.requested_snr_db,
            if out.decoded {
                format!(
                    "DECODED (measured {:+.0} dB)",
                    out.measured_snr_db.unwrap_or(0.0)
                )
            } else {
                "MISSED".to_string()
            }
        );
        sim.tick().await;
    }
    sim.tick_n(2).await;

    let tl = sim.into_timeline();
    print!("{tl}");
    println!(
        "  -> signals decoded: {}, missed: {} (marginal regime is probabilistic; misses handled gracefully)",
        tl.signals_decoded(),
        tl.signals_missed(),
    );
}

/// A fading DX: present and strong for the first part, then drops out (deep
/// dropout fade), then returns strong. The dropout slot likely MISSES; the
/// return slot decodes again. Demonstrates the engine surviving a fade.
async fn scenario_fading_drop_then_return() {
    println!("\n=== Scenario 3: FADING signal — drops then returns ===");
    let mut sim = Sim::new("K5ARH", Some("EM10")).await.with_hifi_seed(3);
    let dx = "VB7F";
    let freq = 1000.0;

    // Operator calls the DX (manual keep-call), so the keep-call re-arm carries
    // us across any fade-induced misses.
    sim.call_station(dx, freq).await;

    // Slot A: the DX answers with grid — strong, clean — decodes.
    let a = sim.inject_signal(
        &format!("K5ARH {dx} EM73"),
        freq,
        STRONG_SNR,
        FadingProfile::None,
    );
    report_signal("A (clean grid)", &a);
    sim.tick().await;

    // Slot B: deep dropout — signal gone for the back 70% of the frame; the
    // decoder loses sync and this slot MISSES. The keep-call re-arm still fires.
    let b = sim.inject_signal(
        &format!("K5ARH {dx} -12"),
        freq,
        STRONG_SNR,
        FadingProfile::Dropout { fraction: 0.7 },
    );
    report_signal("B (dropout 70%)", &b);
    sim.tick().await;

    // Slot C: the DX returns, strong again, re-sending its report — decodes;
    // the exchange resumes from where the fade interrupted it.
    let c = sim.inject_signal(
        &format!("K5ARH {dx} -12"),
        freq,
        STRONG_SNR,
        FadingProfile::None,
    );
    report_signal("C (returned)", &c);
    sim.tick().await;

    // Slot D: the DX rogers — strong — and the QSO closes.
    let d = sim.inject_signal(
        &format!("K5ARH {dx} RR73"),
        freq,
        STRONG_SNR,
        FadingProfile::None,
    );
    report_signal("D (RR73)", &d);
    sim.tick().await;

    sim.tick_n(2).await;

    let tl = sim.into_timeline();
    print!("{tl}");
    println!(
        "  -> signals decoded: {}, missed: {}; completed with {dx}: {}",
        tl.signals_decoded(),
        tl.signals_missed(),
        tl.completed_with(dx),
    );
}

fn report_signal(label: &str, out: &pancetta_qso::sim::SignalOutcome) {
    println!(
        "    slot {} {label}: {}",
        out.slot,
        if out.decoded {
            format!(
                "DECODED -> {} (measured {:+.0} dB)",
                out.decoded_text.as_deref().unwrap_or(""),
                out.measured_snr_db.unwrap_or(0.0)
            )
        } else {
            "MISSED".to_string()
        }
    );
}
