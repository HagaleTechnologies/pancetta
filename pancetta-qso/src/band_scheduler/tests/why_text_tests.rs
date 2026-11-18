//! PAN-385 AC8: every Diagnostics text a band hop can produce.

use super::*;
use pancetta_core::Band;

fn w(text: &str) -> UtcWindow {
    text.parse().expect("window")
}

#[test]
fn why_text_table() {
    let needed = BandHopReason::NeededActivity {
        from: Some(Band::Band20m),
        to: Band::Band17m,
        to_needed: 2,
        to_atno: 0,
        from_needed: 0,
        idle_mins: 10,
        grey_line: None,
    };
    assert_eq!(
        needed.why_text(),
        "Band hop 20m -> 17m: needed activity; 2 needed entities spotted on 17m, 0 on 20m; no needed caller heard on 20m for 10 min"
    );

    let one_atno = BandHopReason::NeededActivity {
        from: Some(Band::Band20m),
        to: Band::Band15m,
        to_needed: 1,
        to_atno: 1,
        from_needed: 0,
        idle_mins: 10,
        grey_line: None,
    };
    assert_eq!(
        one_atno.why_text(),
        "Band hop 20m -> 15m: needed activity; 1 needed entity spotted on 15m (1 ATNO), 0 on 20m; no needed caller heard on 20m for 10 min"
    );

    let needed_grey = BandHopReason::NeededActivity {
        from: Some(Band::Band20m),
        to: Band::Band40m,
        to_needed: 1,
        to_atno: 0,
        from_needed: 0,
        idle_mins: 10,
        grey_line: Some("sunrise"),
    };
    assert_eq!(
        needed_grey.why_text(),
        "Band hop 20m -> 40m: needed activity; 1 needed entity spotted on 40m, 0 on 20m; no needed caller heard on 20m for 10 min; grey-line sunrise favours 40m"
    );

    let plan = BandHopReason::PlanWindow {
        from: Some(Band::Band20m),
        to: Band::Band40m,
        window: w("00:00-06:00"),
        bands: vec![Band::Band40m, Band::Band80m],
        grey_line: None,
    };
    assert_eq!(
        plan.why_text(),
        "Band hop 20m -> 40m: UTC plan window 00:00-06:00 (40m, 80m)"
    );

    let plan_grey = BandHopReason::PlanWindow {
        from: Some(Band::Band17m),
        to: Band::Band40m,
        window: w("22:00-02:00"),
        bands: vec![Band::Band20m, Band::Band40m],
        grey_line: Some("sunset"),
    };
    assert_eq!(
        plan_grey.why_text(),
        "Band hop 17m -> 40m: UTC plan window 22:00-02:00 (20m, 40m); grey-line sunset favours 40m"
    );

    let low = BandHopReason::LowActivity {
        from: Some(Band::Band20m),
        to: Some(Band::Band40m),
        silent_slots: 20,
    };
    assert_eq!(
        low.why_text(),
        "Band hop 20m -> 40m: low activity; no decodes for 20 slots"
    );

    let to_17 = BandHopReason::NeededActivity {
        from: Some(Band::Band20m),
        to: Band::Band17m,
        to_needed: 2,
        to_atno: 0,
        from_needed: 0,
        idle_mins: 10,
        grey_line: None,
    };
    assert_eq!(
        to_17.waiting_text("rig is keyed"),
        "Band hop 20m -> 17m waiting: rig is keyed"
    );
    assert_eq!(
        to_17.waiting_text("QSO with K1ABC in progress"),
        "Band hop 20m -> 17m waiting: QSO with K1ABC in progress"
    );
    assert_eq!(
        to_17.waiting_text("2 QSOs in progress"),
        "Band hop 20m -> 17m waiting: 2 QSOs in progress"
    );

    let unknown_from = BandHopReason::LowActivity {
        from: None,
        to: Some(Band::Band40m),
        silent_slots: 20,
    };
    assert_eq!(
        unknown_from.why_text(),
        "Band hop unknown -> 40m: low activity; no decodes for 20 slots"
    );
    assert_eq!(needed.from_band(), Some(Band::Band20m));
    assert_eq!(needed.to_band(), Some(Band::Band17m));
}
