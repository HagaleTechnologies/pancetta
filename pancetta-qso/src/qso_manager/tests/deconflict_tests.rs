use super::deconflict_offset;

const LO: f64 = 300.0;
const HI: f64 = 2700.0;
const SEP: f64 = 75.0;

#[test]
fn deconflict_clear_candidate_returned_unchanged() {
    // 1500 Hz is well away from 800 Hz — must come back untouched.
    let result = deconflict_offset(1500.0, &[800.0], SEP, LO, HI);
    assert_eq!(result, 1500.0, "clear candidate must be returned unchanged");
}

#[test]
fn deconflict_collision_nudges_at_least_min_sep() {
    // Candidate sits exactly on an occupied offset — must move by ≥ min_sep.
    let result = deconflict_offset(1500.0, &[1500.0], SEP, LO, HI);
    assert!(
        (LO..=HI).contains(&result),
        "result {result} must be within [{LO}, {HI}]"
    );
    assert!(
        (result - 1500.0).abs() >= SEP,
        "result {result} must be ≥ {SEP} Hz from the occupied offset 1500.0"
    );
}

#[test]
fn deconflict_near_collision_within_min_sep() {
    // Candidate 1520 Hz is only 20 Hz from occupied 1500 Hz (< 75 Hz sep) — must move.
    let result = deconflict_offset(1520.0, &[1500.0], SEP, LO, HI);
    assert!(
        (LO..=HI).contains(&result),
        "result {result} must be within [{LO}, {HI}]"
    );
    assert!(
        (result - 1500.0).abs() >= SEP,
        "result {result} must be ≥ {SEP} Hz from 1500.0 (was only 20 Hz away)"
    );
}

#[test]
fn deconflict_bracketed_gap_finds_clear_slot() {
    // Candidate 1500 Hz is bracketed by 1400 and 1600 Hz; the 100 Hz gap
    // between them is narrower than 2 * min_sep so neither side clears
    // within the bracket — the search must escape to a clear slot elsewhere.
    let result = deconflict_offset(1500.0, &[1400.0, 1600.0], SEP, LO, HI);
    assert!(
        (LO..=HI).contains(&result),
        "result {result} must be within [{LO}, {HI}]"
    );
    assert!(
        (result - 1400.0).abs() >= SEP,
        "result {result} must be ≥ {SEP} Hz from 1400.0"
    );
    assert!(
        (result - 1600.0).abs() >= SEP,
        "result {result} must be ≥ {SEP} Hz from 1600.0"
    );
}

#[test]
fn deconflict_empty_occupied_clamps_candidate() {
    // No occupied offsets; candidate 2900 is above HI — must clamp to HI.
    let result = deconflict_offset(2900.0, &[], SEP, LO, HI);
    assert_eq!(
        result, HI,
        "out-of-range candidate with no occupied must clamp to hi"
    );
}

#[test]
fn deconflict_deterministic() {
    // Same inputs twice must produce identical outputs.
    let occupied = [800.0_f64, 1200.0, 1800.0];
    let a = deconflict_offset(1500.0, &occupied, SEP, LO, HI);
    let b = deconflict_offset(1500.0, &occupied, SEP, LO, HI);
    assert_eq!(a, b, "deconflict_offset must be deterministic");
}
