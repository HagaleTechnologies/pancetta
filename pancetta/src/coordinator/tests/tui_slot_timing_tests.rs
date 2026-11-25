//! PAN-477: the TUI's slot tables (title chip and QSO timing row) are a copy
//! of the DSP's timing, because `pancetta-tui` holds no `SlotClock` and does
//! not depend on `pancetta-ft8`. This pins the copy to `derive_dsp_timing`
//! for every protocol the build has, so it cannot drift.

use super::derive_dsp_timing;
use pancetta_ft8::ProtocolParams;
use pancetta_tui::ui::slot_clock::{decode_phase_ns, slot_period_ns};

#[test]
fn tui_slot_tables_match_dsp_timing() {
    let cases = [
        ("FT8", ProtocolParams::ft8()),
        ("FT4", ProtocolParams::ft4()),
        #[cfg(feature = "ft2")]
        ("FT2", ProtocolParams::ft2()),
    ];
    for (mode, params) in cases {
        let timing = derive_dsp_timing(&params);
        assert_eq!(slot_period_ns(mode), timing.slot_ns, "{mode} slot length");
        assert_eq!(
            decode_phase_ns(mode),
            timing.decode_phase.as_nanos() as i64,
            "{mode} decode phase"
        );
    }
}
