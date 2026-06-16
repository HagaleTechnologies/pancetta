//! Adversarial tests for compound-callsign equivalence (catalog C18 / peer D4).
//!
//! A station may appear with a compound callsign (`EA8/G8BCG`, `G8BCG/P`,
//! `K1ABC/R`, `VK9/W1XYZ/MM`) and later in the SAME QSO appear as its base call
//! (`G8BCG`), or vice versa — it is the same operator. WSJT-X/JTDX stall here
//! because sender-verification compares the *displayed* call against the latched
//! partner. pancetta must NOT stall: a compound call and its base are treated as
//! the same station for matching an established QSO's partner.
//!
//! These tests assert two things:
//!   1. POSITIVE — a mid-QSO compound↔base change still advances/completes ONE
//!      QSO (both directions: compound-first and base-first).
//!   2. NEGATIVE — genuinely different near-miss calls (`K5ARG` vs `K5ARH`,
//!      `G8BCH` vs `G8BCG`) do NOT false-match and do NOT advance the QSO.
//!
//! Plus a direct table-driven unit test of `callsigns_match` / `base_callsign`.
//!
//! Run: `cargo test -p pancetta-qso --test adversarial_compound_calls`.

use pancetta_qso::sim::Sim;
use pancetta_qso::{base_callsign, callsigns_match};

const US: &str = "K5ARH";
const GRID: &str = "EM10";
const FREQ: f64 = 1500.0;

async fn sim() -> Sim {
    Sim::new(US, Some(GRID)).await
}

// =====================================================================
// 1. POSITIVE — compound FIRST, then bare base mid-QSO.
//    We call EA8/G8BCG; the DX opens as EA8/G8BCG, then sends its report and
//    RR73 as the bare base G8BCG. The QSO must complete as ONE QSO, no stall.
// =====================================================================
#[tokio::test]
async fn compound_first_then_base_completes() {
    let mut sim = sim().await;

    // Slot 0: DX is CQing as a compound (prefix-portable) call; we call them.
    sim.inject_decode("CQ EA8/G8BCG IL18", FREQ, -9.0, 0.1);
    sim.call_station("EA8/G8BCG", FREQ).await;
    sim.tick().await; // we send our grid: "EA8/G8BCG K5ARH EM10"

    // Slot 1: DX returns our call as the compound, no report yet — we step to
    // sending our report (stuck-at-grid arm).
    sim.inject_decode("K5ARH EA8/G8BCG", FREQ, -9.0, 0.1);
    sim.tick().await;

    // Slot 2: DX now signs with the BARE BASE call and sends our report. This is
    // the C18 case: from = G8BCG, but our latched partner is EA8/G8BCG. Must
    // still match and advance.
    sim.inject_decode("K5ARH G8BCG -11", FREQ, -9.0, 0.1);
    sim.tick().await;

    // Slot 3: DX closes with RR73, still as the bare base.
    sim.inject_decode("K5ARH G8BCG RR73", FREQ, -9.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // The QSO completed — and is logged under the MOST-COMPLETE form seen
    // (the compound carries DX info; we never downgrade to the bare base).
    tl.assert_completed_with("EA8/G8BCG");
    tl.assert_not_completed_with("G8BCG");
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// 2. POSITIVE — bare base FIRST, then a /P suffix-portable mid-QSO.
//    We call G8BCG; the DX later signs G8BCG/P. The QSO must complete and the
//    logged call upgrades to the more-complete /P form.
// =====================================================================
#[tokio::test]
async fn base_first_then_portable_suffix_completes() {
    let mut sim = sim().await;

    // Slot 0: DX CQs as the bare base; we call them as the bare base.
    sim.inject_decode("CQ G8BCG IO81", FREQ, -7.0, 0.1);
    sim.call_station("G8BCG", FREQ).await;
    sim.tick().await; // "G8BCG K5ARH EM10"

    // Slot 1: DX returns our call as a SUFFIX-portable compound, no report — step
    // grid -> report. from = G8BCG/P vs latched G8BCG must match.
    sim.inject_decode("K5ARH G8BCG/P", FREQ, -7.0, 0.1);
    sim.tick().await;

    // Slot 2: DX sends our report as /P.
    sim.inject_decode("K5ARH G8BCG/P -05", FREQ, -7.0, 0.1);
    sim.tick().await;

    // Slot 3: DX closes as /P.
    sim.inject_decode("K5ARH G8BCG/P RR73", FREQ, -7.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // Completed as ONE QSO; logged under the upgraded more-complete /P form.
    tl.assert_completed_with("G8BCG/P");
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// 3. POSITIVE (CQer flow) — we CQ; a station answers as a compound, then
//    rogers as the bare base. One QSO, completes.
// =====================================================================
#[tokio::test]
async fn cqer_caller_compound_then_base_completes() {
    let mut sim = sim().await;

    sim.cq(FREQ).await;
    sim.tick().await; // "CQ K5ARH EM10"

    // A station answers our CQ as a compound (prefix-portable) call.
    sim.inject_decode("K5ARH VP2/W1XYZ FK87", FREQ, -10.0, 0.1);
    sim.tick().await; // we auto-send our report to the latched compound

    // They roger our report as the BARE BASE call — must still match + advance.
    sim.inject_decode("K5ARH W1XYZ R-09", FREQ, -10.0, 0.1);
    sim.tick().await;

    // They close as the bare base.
    sim.inject_decode("K5ARH W1XYZ 73", FREQ, -10.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // Logged under the most-complete form (the opening compound).
    tl.assert_completed_with("VP2/W1XYZ");
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// 4. NEGATIVE — a near-miss base call must NOT false-match.
//    We call K5ARH-the-partner... actually we (US) are K5ARH, so use a partner
//    K5XYZ and have K5XYA (one letter off) try to advance the QSO.
// =====================================================================
#[tokio::test]
async fn near_miss_callsign_does_not_match() {
    let mut sim = sim().await;

    // We call K5XYZ.
    sim.inject_decode("CQ K5XYZ EM00", FREQ, -8.0, 0.1);
    sim.call_station("K5XYZ", FREQ).await;
    sim.tick().await; // RespondingToCq, sent our grid

    // A genuinely DIFFERENT station one letter off (K5XYA) sends us a report on
    // the same frequency, addressed to us. This must NOT advance the K5XYZ QSO —
    // K5XYA and K5XYZ are different stations, not a compound of each other.
    sim.inject_decode("K5ARH K5XYA -12", FREQ, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_not_completed_with("K5XYZ");
    tl.assert_not_completed_with("K5XYA");
    // We never advanced to sending our R-report to anyone.
    tl.assert_not_transmitted_contains("K5ARH R");
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// 5. NEGATIVE — G8BCG vs G8BCH (last letter differs) must NOT match
//    even though both are bare base calls of the same length/shape.
// =====================================================================
#[tokio::test]
async fn last_letter_differs_does_not_match() {
    let mut sim = sim().await;

    sim.inject_decode("CQ G8BCG IO81", FREQ, -8.0, 0.1);
    sim.call_station("G8BCG", FREQ).await;
    sim.tick().await;

    // G8BCH (last letter H, not G) tries to advance our G8BCG QSO. No match.
    sim.inject_decode("K5ARH G8BCH -12", FREQ, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_not_completed_with("G8BCG");
    tl.assert_not_completed_with("G8BCH");
    tl.assert_not_transmitted_contains("K5ARH R");
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// 6. UNIT — direct table test of base_callsign / callsigns_match.
// =====================================================================
#[test]
fn callsigns_match_table() {
    // base_callsign extraction.
    assert_eq!(base_callsign("G8BCG"), "G8BCG");
    assert_eq!(base_callsign("G8BCG/P"), "G8BCG");
    assert_eq!(base_callsign("EA8/G8BCG"), "G8BCG");
    assert_eq!(base_callsign("VK9/W1XYZ/MM"), "W1XYZ");
    assert_eq!(base_callsign("K1ABC/R"), "K1ABC");
    assert_eq!(base_callsign("K1ABC/4"), "K1ABC"); // bare-digit reassignment suffix
    assert_eq!(base_callsign("g8bcg/p"), "G8BCG"); // case-insensitive
    assert_eq!(base_callsign("  G8BCG/P  "), "G8BCG"); // trimmed

    // EQUIVALENT pairs — must match.
    let equiv = [
        ("G8BCG", "G8BCG/P"),
        ("G8BCG", "EA8/G8BCG"),
        ("G8BCG/P", "EA8/G8BCG"),
        ("EA8/G8BCG", "G8BCG"),
        ("K1ABC", "K1ABC/R"),
        ("K1ABC", "K1ABC/4"),
        ("W1XYZ", "VK9/W1XYZ/MM"),
        ("g8bcg", "G8BCG"),     // case only
        ("G8BCG ", " G8BCG/P"), // whitespace + suffix
    ];
    for (a, b) in equiv {
        assert!(
            callsigns_match(a, b),
            "expected {a:?} and {b:?} to be the SAME station (compound/base)"
        );
        assert!(callsigns_match(b, a), "callsigns_match must be symmetric");
    }

    // NON-EQUIVALENT pairs — must NOT match (conservative).
    let diff = [
        ("K5ARH", "K5ARG"),         // last letter
        ("G8BCG", "G8BCH"),         // last letter
        ("G8BCG", "G8BCG2"),        // different base
        ("EA8/G8BCG", "EA8/G8BCH"), // same prefix, different base
        ("G8BCG/P", "G8BCH/P"),     // same suffix, different base
        ("K1ABC", "K2ABC"),         // digit differs
        ("W1XYZ", "W1XYW"),         // last letter
        ("EA8/G8BCG", "EA8/W1XYZ"), // genuinely different home calls
    ];
    for (a, b) in diff {
        assert!(
            !callsigns_match(a, b),
            "expected {a:?} and {b:?} to be DIFFERENT stations (no false match)"
        );
    }

    // An empty call never relaxes into matching a real station.
    assert!(!callsigns_match("", "G8BCG"));
    assert!(!callsigns_match("G8BCG", ""));
    assert!(callsigns_match("", ""));
}
