//! PAN-358: the `clock.step` wording depends on whether the slot clock's
//! event is a resync (1 s or more).

use super::tests::Sim;

#[test]
fn sub_second_step_diagnostic_says_slot_timing_kept() {
    let mut sim = Sim::new();
    sim.probe_at(100, Ok(0.0));
    sim.jump_ms(500);
    let diag = sim
        .tick_at(200)
        .step_diag
        .expect("a sub-second step reports");
    assert_eq!(
        diag.text,
        "system clock jumped +0.5 s (sleep/wake or NTP step) — under 1 s, slot timing kept"
    );
    sim.jump_ms(60_000);
    let diag = sim.tick_at(300).step_diag.expect("a resync reports");
    assert_eq!(
        diag.text,
        "system clock jumped +60.0 s (sleep/wake or NTP step) — slot timing re-anchored to UTC"
    );
}
