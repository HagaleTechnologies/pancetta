//! PAN-385: the autonomous band scheduler, driven through the operator.
//!
//! Every test drives virtual UTC through `feed_decoded_messages_on_dial_at`
//! and `decide_at_time`, as the coordinator does once per slot.

use super::*;
use crate::band_scheduler::{BandHopReason, BandOpportunity, BandPlanEntry, OpportunitySnapshot};
use jiff::SignedDuration;
use pancetta_core::time::UtcTimestamp;
use pancetta_core::utc_window::UtcWindow;
use pancetta_core::Band;
use std::collections::HashSet;

/// Score 0.5 for every call; tier `PerBandDxccNew` for the configured
/// calls, `Standard` otherwise.
struct NeedEvaluator {
    needed: HashSet<String>,
}

impl NeedEvaluator {
    fn new(needed: &[&str]) -> Self {
        Self {
            needed: needed.iter().map(|c| c.to_string()).collect(),
        }
    }
}

impl DxEvaluator for NeedEvaluator {
    fn evaluate_cq(&self, _: &str, _: Option<&str>, _: i8, _: f64) -> f64 {
        0.5
    }

    fn evaluate_cq_tiered(
        &self,
        callsign: &str,
        _: Option<&str>,
        _: i8,
        _: f64,
    ) -> Option<TieredScore> {
        let tier = if self.needed.contains(callsign) {
            PriorityTier::PerBandDxccNew
        } else {
            PriorityTier::Standard
        };
        Some(TieredScore {
            tier,
            secondary: 0.5,
        })
    }
}

fn entries(list: &[(u64, &str)]) -> Vec<BandEntry> {
    list.iter()
        .enumerate()
        .map(|(i, (dial, name))| BandEntry {
            dial_frequency: *dial,
            band_name: name.to_string(),
            priority: i as u32 + 1,
        })
        .collect()
}

fn window(utc: &str, bands: &[Band]) -> BandPlanEntry {
    BandPlanEntry {
        window: utc.parse::<UtcWindow>().expect("valid window"),
        bands: bands.to_vec(),
    }
}

fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> UtcTimestamp {
    UtcTimestamp::from_ymd_hms(y, mo, d, h, mi, s).unwrap()
}

fn secs(n: i64) -> SignedDuration {
    SignedDuration::from_secs(n)
}

fn cq(call: &str, grid: &str) -> DecodedMessageInfo {
    DecodedMessageInfo {
        callsign: Some(call.to_string()),
        frequency_hz: 1500.0,
        snr: -10,
        message_text: format!("CQ {call} {grid}"),
        slot_parity: None,
        confidence: None,
        time_offset_s: None,
        decode_origin: None,
        captured_dial_hz: None,
    }
}

/// Two CQs the evaluator tiers `Standard`.
fn two_cqs() -> Vec<DecodedMessageInfo> {
    vec![cq("W2AAA", "FN31"), cq("W3BBB", "FM19")]
}

fn snap(as_of: UtcTimestamp, counts: &[(Band, u32, u32)]) -> OpportunitySnapshot {
    OpportunitySnapshot {
        as_of,
        bands: counts
            .iter()
            .map(|(band, needed, atno)| BandOpportunity {
                band: *band,
                needed_entities: *needed,
                atno_entities: *atno,
            })
            .collect(),
    }
}

fn hop_of(actions: &[OperatorAction]) -> Option<(u64, BandHopReason)> {
    actions.iter().find_map(|a| match a {
        OperatorAction::ChangeBand {
            dial_frequency,
            reason,
        } => Some((*dial_frequency, reason.clone())),
        _ => None,
    })
}

struct Harness {
    op: AutonomousOperator,
    eval: NeedEvaluator,
    dial: u64,
}

impl Harness {
    /// `K1ABC` at `grid`, hopping on, `hop_threshold = 20`, never pouncing
    /// or calling CQ by itself; `tweak` adjusts the config further.
    fn new(
        bands: &[(u64, &str)],
        plan: Vec<BandPlanEntry>,
        grid: Option<&str>,
        tweak: impl FnOnce(&mut AutonomousConfig),
    ) -> Self {
        let mut config = AutonomousConfig {
            enabled: true,
            slot_parity: SlotParityConfig::Even,
            min_dx_score: 0.99,
            cq_after_idle_cycles: 100_000,
            band_hopping: BandHoppingConfig {
                enabled: true,
                hop_threshold: 20,
                bands: entries(bands),
                plan,
                ..Default::default()
            },
            ..Default::default()
        };
        config.listen_cycle.initial_interval = 1000;
        tweak(&mut config);
        Self {
            op: AutonomousOperator::new(config, "K1ABC".into(), grid.map(str::to_string)),
            eval: NeedEvaluator::new(&["VP8PJ"]),
            dial: 0,
        }
    }

    fn slot(
        &mut self,
        now: UtcTimestamp,
        messages: &[DecodedMessageInfo],
        snapshot: Option<OpportunitySnapshot>,
    ) -> Vec<OperatorAction> {
        if snapshot.is_some() {
            self.op.update_band_opportunities(snapshot);
        }
        self.op
            .feed_decoded_messages_on_dial_at(messages, &self.eval, self.dial, now);
        self.op.decide_at_time(now)
    }
}

const AC1_BANDS: &[(u64, &str)] = &[(14_074_000, "20m"), (18_100_000, "17m")];

/// 2026-06-21 15:00 UTC: an even slot, no grey-line at FN42.
fn t0() -> UtcTimestamp {
    utc(2026, 6, 21, 15, 0, 0)
}

fn ac1_snapshot(now: UtcTimestamp) -> OpportunitySnapshot {
    snap(now, &[(Band::Band17m, 2, 0), (Band::Band20m, 0, 0)])
}

const AC1_TEXT: &str = "Band hop 20m -> 17m: needed activity; 2 needed entities spotted on 17m, \
                        0 on 20m; no needed caller heard on 20m for 10 min";

/// Runs AC1's feed from `t0` through `t0 + 15 * last` and returns the slot
/// index of the first hop.
fn first_hop_slot(
    h: &mut Harness,
    last: i64,
    snapshot: impl Fn(UtcTimestamp) -> Option<OpportunitySnapshot>,
) -> Option<(i64, u64, BandHopReason)> {
    for k in 0..=last {
        let now = t0() + secs(15 * k);
        let actions = h.slot(now, &two_cqs(), snapshot(now));
        if let Some((dial, reason)) = hop_of(&actions) {
            return Some((k, dial, reason));
        }
    }
    None
}

#[test]
fn pan385_scenario1_needed_activity_hops_to_17m() {
    let mut h = Harness::new(AC1_BANDS, vec![], Some("FN42"), |_| {});
    h.dial = 14_074_000;
    for k in 0..40 {
        let now = t0() + secs(15 * k);
        let actions = h.slot(now, &two_cqs(), Some(ac1_snapshot(now)));
        assert!(hop_of(&actions).is_none(), "no hop at t0 + {} s", 15 * k);
    }
    let now = t0() + secs(600);
    let actions = h.slot(now, &two_cqs(), Some(ac1_snapshot(now)));
    let (dial, reason) = hop_of(&actions).expect("needed hop at t0 + 600 s");
    assert_eq!(dial, 18_100_000);
    assert_eq!(reason.why_text(), AC1_TEXT);
}

#[test]
fn pan385_needed_caller_resets_the_idle_clock() {
    let mut h = Harness::new(AC1_BANDS, vec![], Some("FN42"), |_| {});
    h.dial = 14_074_000;
    let mut first = None;
    for k in 0..=60 {
        let now = t0() + secs(15 * k);
        let mut msgs = two_cqs();
        if k == 20 {
            msgs.push(cq("VP8PJ", "GD18"));
        }
        let actions = h.slot(now, &msgs, Some(ac1_snapshot(now)));
        if let Some((dial, reason)) = hop_of(&actions) {
            first = Some((k, dial, reason));
            break;
        }
    }
    let (k, dial, reason) = first.expect("a hop after the idle clock restarts");
    assert_eq!(
        k * 15,
        900,
        "needed caller at t0 + 300 s moves the hop to t0 + 900 s"
    );
    assert_eq!(dial, 18_100_000);
    assert_eq!(reason.why_text(), AC1_TEXT);
}

#[test]
fn pan385_needed_hop_needs_fresh_strictly_better_spots() {
    // Positive control: the AC1 snapshot hops at t0 + 600 s.
    let mut h = Harness::new(AC1_BANDS, vec![], Some("FN42"), |_| {});
    h.dial = 14_074_000;
    let hop = first_hop_slot(&mut h, 40, |now| Some(ac1_snapshot(now)));
    assert_eq!(hop.map(|(k, d, _)| (k, d)), Some((40, 18_100_000)));

    let cases: Vec<(&str, TweakFn, SnapFn)> = vec![
        (
            "all-zero snapshot",
            Box::new(|_| {}),
            Box::new(|now| Some(snap(now, &[(Band::Band17m, 0, 0), (Band::Band20m, 0, 0)]))),
        ),
        (
            "snapshot older than 600 s",
            Box::new(|_| {}),
            Box::new(|now| Some(ac1_snapshot(now - secs(601)))),
        ),
        (
            "needed_activity = false",
            Box::new(|c| c.band_hopping.needed_activity = false),
            Box::new(|now| Some(ac1_snapshot(now))),
        ),
        (
            "not strictly better",
            Box::new(|_| {}),
            Box::new(|now| Some(snap(now, &[(Band::Band17m, 2, 0), (Band::Band20m, 2, 0)]))),
        ),
        (
            "no snapshot ever pushed",
            Box::new(|_| {}),
            Box::new(|_| None),
        ),
    ];
    for (name, tweak, snapshot) in cases {
        let mut h = Harness::new(AC1_BANDS, vec![], Some("FN42"), tweak);
        h.dial = 14_074_000;
        let hop = first_hop_slot(&mut h, 40, snapshot);
        assert!(hop.is_none(), "{name}: no hop by t0 + 600 s, got {hop:?}");
    }
}

type SnapFn = Box<dyn Fn(UtcTimestamp) -> Option<OpportunitySnapshot>>;
type TweakFn = Box<dyn Fn(&mut AutonomousConfig)>;

#[test]
fn pan385_atno_outranks_band_fill_count() {
    let bands = &[
        (14_074_000, "20m"),
        (18_100_000, "17m"),
        (21_074_000, "15m"),
    ];
    let mut h = Harness::new(bands, vec![], Some("FN42"), |_| {});
    h.dial = 14_074_000;
    let hop = first_hop_slot(&mut h, 40, |now| {
        Some(snap(
            now,
            &[
                (Band::Band15m, 1, 1),
                (Band::Band17m, 3, 0),
                (Band::Band20m, 0, 0),
            ],
        ))
    });
    let (k, dial, reason) = hop.expect("needed hop");
    assert_eq!(k, 40);
    assert_eq!(dial, 21_074_000);
    assert_eq!(reason.to_band(), Some(Band::Band15m));
}

fn scenario2_plan() -> Vec<BandPlanEntry> {
    vec![
        window("00:00-06:00", &[Band::Band40m, Band::Band80m]),
        window("06:00-24:00", &[Band::Band20m, Band::Band17m]),
    ]
}

const SCENARIO2_BANDS: &[(u64, &str)] = &[(14_074_000, "20m"), (7_076_000, "40m")];

#[test]
fn pan385_scenario2_plan_window_entry_moves_within_one_hop_interval() {
    let mut h = Harness::new(SCENARIO2_BANDS, scenario2_plan(), Some("FN42"), |_| {});
    h.dial = 14_074_000;
    let a = h.slot(utc(2026, 11, 16, 23, 59, 30), &two_cqs(), None);
    assert!(
        hop_of(&a).is_none(),
        "first evaluation already on a window band"
    );
    let a = h.slot(utc(2026, 11, 16, 23, 59, 45), &two_cqs(), None);
    assert!(hop_of(&a).is_none());
    let a = h.slot(utc(2026, 11, 17, 0, 0, 0), &two_cqs(), None);
    let (dial, reason) = hop_of(&a).expect("window entry hops 0 s after it starts");
    assert_eq!(dial, 7_076_000, "the configured 40m entry's dial");
    assert_eq!(
        reason.why_text(),
        "Band hop 20m -> 40m: UTC plan window 00:00-06:00 (40m, 80m)"
    );

    h.dial = 7_076_000;
    for k in 1..8 {
        let a = h.slot(utc(2026, 11, 17, 0, 0, 0) + secs(15 * k), &two_cqs(), None);
        assert!(
            hop_of(&a).is_none(),
            "no hop after the move was carried out"
        );
    }
    let a = h.slot(utc(2026, 11, 17, 5, 59, 45), &two_cqs(), None);
    assert!(hop_of(&a).is_none());
    let a = h.slot(utc(2026, 11, 17, 6, 0, 0), &two_cqs(), None);
    let (dial, reason) = hop_of(&a).expect("second window entry hops");
    assert_eq!(dial, 14_074_000);
    assert_eq!(
        reason.why_text(),
        "Band hop 40m -> 20m: UTC plan window 06:00-24:00 (20m, 17m)"
    );
}

#[test]
fn pan385_plan_band_dial_prefers_a_configured_entry() {
    // A configured 40m entry wins over the FT8 table's 7 074 000.
    let mut h = Harness::new(
        SCENARIO2_BANDS,
        vec![window("00:00-06:00", &[Band::Band40m])],
        Some("FN42"),
        |_| {},
    );
    h.dial = 14_074_000;
    let a = h.slot(utc(2026, 11, 17, 3, 0, 0), &two_cqs(), None);
    assert_eq!(hop_of(&a).map(|(d, _)| d), Some(7_076_000));

    // No 80m entry: the FT8 table dial.
    let mut h = Harness::new(
        SCENARIO2_BANDS,
        vec![window("00:00-06:00", &[Band::Band80m])],
        Some("FN42"),
        |_| {},
    );
    h.dial = 14_074_000;
    let a = h.slot(utc(2026, 11, 17, 3, 0, 0), &two_cqs(), None);
    let (dial, reason) = hop_of(&a).expect("plan hop to 80m");
    assert_eq!(dial, 3_573_000);
    assert_eq!(
        reason.why_text(),
        "Band hop 20m -> 80m: UTC plan window 00:00-06:00 (80m)"
    );
}

const DEFAULT_BANDS: &[(u64, &str)] = &[(14_074_000, "20m"), (7_074_000, "40m")];

#[test]
fn pan385_plan_entry_is_edge_triggered() {
    let plan = vec![window("00:00-06:00", &[Band::Band40m, Band::Band80m])];
    let start = utc(2026, 11, 17, 3, 0, 0);

    let mut h = Harness::new(DEFAULT_BANDS, plan.clone(), Some("FN42"), |_| {});
    h.dial = 14_074_000;
    let a = h.slot(start, &two_cqs(), None);
    assert_eq!(
        hop_of(&a).map(|(d, r)| (d, r.to_band())),
        Some((7_074_000, Some(Band::Band40m))),
        "first evaluation inside the window on 20m hops to 40m"
    );
    h.dial = 7_074_000;
    let a = h.slot(start + secs(15), &two_cqs(), None);
    assert!(hop_of(&a).is_none());
    // Manual QSY to 20m at 03:30: left alone until the next window entry.
    h.dial = 14_074_000;
    let mut now = utc(2026, 11, 17, 3, 30, 0);
    while now <= utc(2026, 11, 17, 5, 59, 45) {
        let a = h.slot(now, &two_cqs(), None);
        assert!(
            hop_of(&a).is_none(),
            "no plan hop after a manual QSY at {now}"
        );
        now += secs(15);
    }

    let mut h = Harness::new(DEFAULT_BANDS, plan, Some("FN42"), |_| {});
    h.dial = 3_573_000;
    let a = h.slot(start, &two_cqs(), None);
    assert!(hop_of(&a).is_none(), "already on 80m, a window band");
}

#[test]
fn pan385_window_restricts_needed_and_low_activity_hops() {
    let plan = vec![window("00:00-06:00", &[Band::Band40m, Band::Band80m])];
    let bands = &[(14_074_000, "20m"), (18_100_000, "17m"), (7_074_000, "40m")];

    // Needed activity picks only among the window's bands.
    let mut h = Harness::new(bands, plan.clone(), Some("FN42"), |_| {});
    h.dial = 7_074_000;
    let start = utc(2026, 11, 17, 2, 50, 0);
    let snapshot = |now| {
        snap(
            now,
            &[
                (Band::Band17m, 3, 0),
                (Band::Band80m, 1, 0),
                (Band::Band40m, 0, 0),
            ],
        )
    };
    for k in 0..40 {
        let now = start + secs(15 * k);
        let a = h.slot(now, &two_cqs(), Some(snapshot(now)));
        assert!(hop_of(&a).is_none(), "no hop at {now}");
    }
    let now = utc(2026, 11, 17, 3, 0, 0);
    let a = h.slot(now, &two_cqs(), Some(snapshot(now)));
    let (dial, reason) = hop_of(&a).expect("needed hop at 03:00");
    assert_eq!(dial, 3_573_000);
    assert_eq!(reason.to_band(), Some(Band::Band80m), "never 17m");

    // Low activity inside the window rotates 40m -> 80m -> 40m.
    let mut h = Harness::new(bands, plan, Some("FN42"), |_| {});
    h.dial = 7_074_000;
    let start = utc(2026, 11, 17, 1, 0, 0);
    let mut hops = Vec::new();
    for k in 0..80 {
        let a = h.slot(start + secs(15 * k), &[], None);
        if let Some((dial, reason)) = hop_of(&a) {
            assert!(
                matches!(reason, BandHopReason::LowActivity { .. }),
                "{reason:?}"
            );
            hops.push(dial);
            h.dial = dial;
        }
    }
    assert!(hops.len() >= 3, "hops: {hops:?}");
    assert_eq!(&hops[..3], &[3_573_000, 7_074_000, 3_573_000]);
    assert!(hops.iter().all(|d| *d == 3_573_000 || *d == 7_074_000));
}

#[test]
fn pan385_hop_waits_for_the_exchange_to_end() {
    // Plan entry waits.
    let mut h = Harness::new(SCENARIO2_BANDS, scenario2_plan(), Some("FN42"), |_| {});
    h.dial = 14_074_000;
    let a = h.slot(utc(2026, 11, 16, 23, 59, 30), &two_cqs(), None);
    assert!(hop_of(&a).is_none());
    h.op.set_exchange_in_progress(true);
    let midnight = utc(2026, 11, 17, 0, 0, 0);
    for k in 0..10 {
        let a = h.slot(midnight + secs(15 * k), &two_cqs(), None);
        assert!(
            hop_of(&a).is_none(),
            "no hop while an exchange is in progress"
        );
    }
    h.op.set_exchange_in_progress(false);
    let a = h.slot(midnight + secs(150), &two_cqs(), None);
    assert_eq!(hop_of(&a).map(|(d, _)| d), Some(7_076_000));

    // The AC1 needed hop waits the same way.
    let mut h = Harness::new(AC1_BANDS, vec![], Some("FN42"), |_| {});
    h.dial = 14_074_000;
    for k in 0..=46 {
        let now = t0() + secs(15 * k);
        h.op.set_exchange_in_progress((35..=45).contains(&k));
        let a = h.slot(now, &two_cqs(), Some(ac1_snapshot(now)));
        if k < 46 {
            assert!(hop_of(&a).is_none(), "no hop at t0 + {} s", 15 * k);
        } else {
            let (dial, reason) = hop_of(&a).expect("hop once the exchange ends");
            assert_eq!(dial, 18_100_000);
            assert!(reason
                .why_text()
                .starts_with("Band hop 20m -> 17m: needed activity"));
        }
    }
}

#[test]
fn pan385_unexecuted_hop_is_retried_next_slot() {
    // Needed hop.
    let mut h = Harness::new(AC1_BANDS, vec![], Some("FN42"), |_| {});
    h.dial = 14_074_000;
    let hop = first_hop_slot(&mut h, 40, |now| Some(ac1_snapshot(now)));
    assert_eq!(hop.map(|(k, d, _)| (k, d)), Some((40, 18_100_000)));
    let now = t0() + secs(615);
    let a = h.slot(now, &two_cqs(), Some(ac1_snapshot(now)));
    let (dial, reason) = hop_of(&a).expect("dial still on 20m: the hop is emitted again");
    assert_eq!(dial, 18_100_000);
    assert_eq!(reason.why_text(), AC1_TEXT);

    // Plan hop.
    let mut h = Harness::new(SCENARIO2_BANDS, scenario2_plan(), Some("FN42"), |_| {});
    h.dial = 14_074_000;
    h.slot(utc(2026, 11, 16, 23, 59, 45), &two_cqs(), None);
    let a = h.slot(utc(2026, 11, 17, 0, 0, 0), &two_cqs(), None);
    assert_eq!(hop_of(&a).map(|(d, _)| d), Some(7_076_000));
    let a = h.slot(utc(2026, 11, 17, 0, 0, 15), &two_cqs(), None);
    assert_eq!(
        hop_of(&a).map(|(d, _)| d),
        Some(7_076_000),
        "not carried out: emitted again"
    );
    h.dial = 7_076_000;
    let a = h.slot(utc(2026, 11, 17, 0, 0, 30), &two_cqs(), None);
    assert!(hop_of(&a).is_none(), "carried out: no further hop");
}

/// A low-activity hop the executor waited on (rig keyed) is retried the
/// next slot once the coordinator reports it with `band_hop_waited`,
/// instead of waiting out the two-slot settle and another `hop_threshold`
/// of silence. Covers the round-robin over `bands` and the round-robin
/// inside an active plan window.
#[test]
fn pan385_waited_low_activity_hop_is_retried_next_slot() {
    fn first_low_activity_hop(h: &mut Harness, start: UtcTimestamp) -> Option<(i64, u64)> {
        (0..30).find_map(|k| {
            hop_of(&h.slot(start + secs(15 * k), &[], None)).map(|(dial, reason)| {
                assert!(
                    matches!(reason, BandHopReason::LowActivity { .. }),
                    "{reason:?}"
                );
                (k, dial)
            })
        })
    }

    // Round-robin over `bands`.
    for waited in [false, true] {
        let mut h = Harness::new(DEFAULT_BANDS, vec![], Some("FN42"), |_| {});
        h.dial = 14_074_000;
        assert_eq!(first_low_activity_hop(&mut h, t0()), Some((19, 7_074_000)));
        if waited {
            h.op.band_hop_waited();
        }
        // The dial still shows 20m: the hop was not carried out.
        let next = hop_of(&h.slot(t0() + secs(15 * 20), &[], None));
        if waited {
            let (dial, reason) = next.expect("waited hop retried next slot");
            assert_eq!(dial, 7_074_000);
            assert_eq!(reason.to_band(), Some(Band::Band40m));
            assert!(matches!(reason, BandHopReason::LowActivity { .. }));
        } else {
            assert!(next.is_none(), "not reported as waited: settling");
        }
    }

    // Round-robin inside the active window.
    let plan = vec![window("00:00-06:00", &[Band::Band40m, Band::Band80m])];
    let bands = &[(14_074_000, "20m"), (18_100_000, "17m"), (7_074_000, "40m")];
    let mut h = Harness::new(bands, plan, Some("FN42"), |_| {});
    h.dial = 7_074_000;
    let start = utc(2026, 11, 17, 1, 0, 0);
    assert_eq!(first_low_activity_hop(&mut h, start), Some((19, 3_573_000)));
    h.op.band_hop_waited();
    let (dial, reason) = hop_of(&h.slot(start + secs(15 * 20), &[], None))
        .expect("waited window hop retried next slot");
    assert_eq!(dial, 3_573_000);
    assert!(matches!(reason, BandHopReason::LowActivity { .. }));
}

/// With the dial unknown, a waited plan hop must not leave the scheduler
/// assuming the station reached the window's band: the next slot emits the
/// plan move again.
#[test]
fn pan385_waited_plan_hop_with_unknown_dial_is_retried_next_slot() {
    for waited in [false, true] {
        let mut h = Harness::new(SCENARIO2_BANDS, scenario2_plan(), Some("FN42"), |_| {});
        h.slot(utc(2026, 11, 16, 23, 59, 45), &two_cqs(), None);
        let a = h.slot(utc(2026, 11, 17, 0, 0, 0), &two_cqs(), None);
        assert_eq!(hop_of(&a).map(|(d, _)| d), Some(7_076_000));
        if waited {
            h.op.band_hop_waited();
        }
        let next = hop_of(&h.slot(utc(2026, 11, 17, 0, 0, 15), &two_cqs(), None));
        if waited {
            assert_eq!(
                next.map(|(d, r)| (d, r.why_text())),
                Some((
                    7_076_000,
                    "Band hop 20m -> 40m: UTC plan window 00:00-06:00 (40m, 80m)".to_string()
                )),
                "waited: the plan move is still owed"
            );
        } else {
            assert!(
                next.is_none(),
                "not reported as waited: assumed carried out"
            );
        }
    }
}

#[test]
fn pan385_needed_hop_dwell_is_one_hop_interval() {
    let bands = &[
        (14_074_000, "20m"),
        (18_100_000, "17m"),
        (21_074_000, "15m"),
    ];
    // A 60 s idle requirement isolates the dwell from the idle clock.
    let mut h = Harness::new(bands, vec![], Some("FN42"), |c| {
        c.band_hopping.needed_idle_secs = 60;
    });
    h.dial = 14_074_000;
    let first = |now| {
        snap(
            now,
            &[
                (Band::Band17m, 2, 0),
                (Band::Band15m, 0, 0),
                (Band::Band20m, 0, 0),
            ],
        )
    };
    let hop = first_hop_slot(&mut h, 10, |now| Some(first(now)));
    assert_eq!(hop.map(|(k, d, _)| (k * 15, d)), Some((60, 18_100_000)));

    // Carried out; a better band appears.
    h.dial = 18_100_000;
    let better = |now| {
        snap(
            now,
            &[
                (Band::Band15m, 3, 0),
                (Band::Band17m, 2, 0),
                (Band::Band20m, 0, 0),
            ],
        )
    };
    let mut next = None;
    for k in 5..=30 {
        let now = t0() + secs(15 * k);
        let a = h.slot(now, &two_cqs(), Some(better(now)));
        if let Some((dial, _)) = hop_of(&a) {
            next = Some((15 * k, dial));
            break;
        }
    }
    assert_eq!(
        next,
        Some((360, 21_074_000)),
        "no needed hop until 300 s after the last one"
    );

    // A plan entry 60 s after a needed hop still hops at once.
    let bands = &[(14_074_000, "20m"), (18_100_000, "17m"), (7_074_000, "40m")];
    let mut h = Harness::new(
        bands,
        vec![window("00:10-06:00", &[Band::Band40m])],
        Some("FN42"),
        |_| {},
    );
    h.dial = 14_074_000;
    let start = utc(2026, 11, 16, 23, 59, 0);
    let snapshot = |now| snap(now, &[(Band::Band17m, 2, 0), (Band::Band20m, 0, 0)]);
    let mut needed_at = None;
    for k in 0..=44 {
        let now = start + secs(15 * k);
        let a = h.slot(now, &two_cqs(), Some(snapshot(now)));
        if let Some((dial, reason)) = hop_of(&a) {
            if needed_at.is_none() {
                assert_eq!(dial, 18_100_000);
                assert!(matches!(reason, BandHopReason::NeededActivity { .. }));
                needed_at = Some(now);
                h.dial = dial;
            } else {
                assert_eq!(now, utc(2026, 11, 17, 0, 10, 0));
                assert_eq!(dial, 7_074_000);
                assert!(matches!(reason, BandHopReason::PlanWindow { .. }));
                return;
            }
        }
    }
    panic!("plan entry did not hop (needed hop at {needed_at:?})");
}

#[test]
fn pan385_hop_drops_old_band_cq_candidates() {
    fn run(needed_activity: bool) -> Vec<OperatorAction> {
        let mut h = Harness::new(AC1_BANDS, vec![], Some("FN42"), |c| {
            c.min_dx_score = 0.3;
            c.band_hopping.hop_threshold = 1000;
            c.band_hopping.needed_activity = needed_activity;
        });
        h.dial = 14_074_000;
        for k in 0..40 {
            let now = t0() + secs(15 * k);
            let a = h.slot(now, &[], Some(ac1_snapshot(now)));
            assert!(hop_of(&a).is_none());
        }
        let now = t0() + secs(600);
        h.slot(now, &[cq("W1AW", "FN42")], Some(ac1_snapshot(now)))
    }
    let pounced = |a: &[OperatorAction]| {
        a.iter().any(|x| {
            matches!(x, OperatorAction::Transmit { message_text, .. } if message_text.starts_with("W1AW"))
        })
    };

    let hop_slot = run(true);
    assert_eq!(hop_of(&hop_slot).map(|(d, _)| d), Some(18_100_000));
    assert!(!pounced(&hop_slot), "no old-band pounce: {hop_slot:?}");

    let control = run(false);
    assert!(hop_of(&control).is_none());
    assert!(
        control.iter().any(|x| matches!(
            x,
            OperatorAction::Transmit { message_text, .. } if message_text == "W1AW K1ABC FN42"
        )),
        "control slot answers W1AW: {control:?}"
    );
}

#[test]
fn pan385_low_activity_hop_keeps_its_dial_sequence() {
    let mut h = Harness::new(DEFAULT_BANDS, vec![], Some("FN42"), |_| {});
    let mut hops = Vec::new();
    for k in 0..50 {
        let a = h.slot(t0() + secs(15 * k), &[], None);
        if let Some((dial, reason)) = hop_of(&a) {
            hops.push((k, dial, reason.why_text()));
        }
    }
    assert!(hops.len() >= 2, "{hops:?}");
    assert_eq!(hops[0].0, 19, "the 20th silent slot hops");
    assert_eq!(hops[0].1, 7_074_000);
    assert_eq!(
        hops[0].2,
        "Band hop 20m -> 40m: low activity; no decodes for 20 slots"
    );
    assert_eq!(hops[1].1, 14_074_000);
    assert_eq!(
        hops[1].2,
        "Band hop 40m -> 20m: low activity; no decodes for 20 slots"
    );
}

#[test]
fn pan385_low_activity_index_follows_the_live_dial() {
    let mut h = Harness::new(DEFAULT_BANDS, vec![], Some("FN42"), |_| {});
    h.dial = 7_074_000;
    for k in 0..30 {
        let a = h.slot(t0() + secs(15 * k), &[], None);
        if let Some((dial, reason)) = hop_of(&a) {
            assert_eq!(dial, 14_074_000, "rig on 40m: the first hop goes to 20m");
            assert_eq!(
                reason.why_text(),
                "Band hop 40m -> 20m: low activity; no decodes for 20 slots"
            );
            return;
        }
    }
    panic!("no low-activity hop");
}

/// Two entries on one band (FT8 and FT4 dials on 20m) must not trap the
/// round-robin: the live dial on the second 20m entry leaves the index
/// there, so the next low-activity hop reaches 40m.
#[test]
fn pan385_low_activity_rotation_passes_two_entries_on_one_band() {
    let bands = &[(14_074_000, "20m"), (14_080_000, "20m"), (7_074_000, "40m")];
    let mut h = Harness::new(bands, vec![], Some("FN42"), |_| {});
    h.dial = 14_074_000;
    let mut dials = Vec::new();
    for k in 0..80 {
        let a = h.slot(t0() + secs(15 * k), &[], None);
        if let Some((dial, _)) = hop_of(&a) {
            // The coordinator carried the hop out: the next live dial shows it.
            h.dial = dial;
            dials.push(dial);
        }
    }
    assert!(dials.len() >= 2, "{dials:?}");
    assert_eq!(&dials[..2], &[14_080_000, 7_074_000], "{dials:?}");
}

#[test]
fn pan385_grey_line_breaks_ties_toward_low_bands() {
    let bands = &[(14_074_000, "20m"), (18_100_000, "17m"), (7_074_000, "40m")];
    let tie = |now| {
        snap(
            now,
            &[
                (Band::Band17m, 1, 0),
                (Band::Band40m, 1, 0),
                (Band::Band20m, 0, 0),
            ],
        )
    };
    let zero = |now| {
        snap(
            now,
            &[
                (Band::Band17m, 0, 0),
                (Band::Band40m, 0, 0),
                (Band::Band20m, 0, 0),
            ],
        )
    };
    // Ten minutes on 20m ending at `at`; returns the hop at `at`, if any.
    let run = |grid: Option<&str>,
               grey_line: bool,
               at: UtcTimestamp,
               snapshot: &dyn Fn(UtcTimestamp) -> OpportunitySnapshot|
     -> Option<(u64, BandHopReason)> {
        let mut h = Harness::new(bands, vec![], grid, |c| {
            c.band_hopping.grey_line = grey_line;
        });
        h.dial = 14_074_000;
        let start = at - secs(600);
        for k in 0..40 {
            let now = start + secs(15 * k);
            let a = h.slot(now, &two_cqs(), Some(snapshot(now)));
            assert!(hop_of(&a).is_none(), "no hop before {at}");
        }
        hop_of(&h.slot(at, &two_cqs(), Some(snapshot(at))))
    };
    let sunrise = utc(2026, 6, 21, 9, 20, 0);
    let afternoon = utc(2026, 6, 21, 15, 0, 0);

    let (dial, reason) = run(Some("FN42"), true, sunrise, &tie).expect("hop at 09:20");
    assert_eq!(dial, 7_074_000);
    assert!(
        reason
            .why_text()
            .ends_with("; grey-line sunrise favours 40m"),
        "{}",
        reason.why_text()
    );

    let (dial, reason) = run(Some("FN42"), true, afternoon, &tie).expect("hop at 15:00");
    assert_eq!(dial, 18_100_000, "list order without grey-line");
    assert!(!reason.why_text().contains("grey-line"));

    let (dial, _) = run(Some("FN42"), false, sunrise, &tie).expect("hop, grey_line off");
    assert_eq!(dial, 18_100_000);

    let (dial, _) = run(None, true, sunrise, &tie).expect("hop, no grid");
    assert_eq!(dial, 18_100_000);

    assert!(run(Some("FN42"), true, sunrise, &zero).is_none());
}

#[test]
fn pan385_plan_entry_before_the_first_dial_is_judged_on_the_first_dial() {
    // The configured fallback band (20m) is in the window, but the rig is
    // really on 40m: the first live dial must still owe the window a move.
    let plan = vec![window("06:00-24:00", &[Band::Band20m, Band::Band17m])];
    let mut h = Harness::new(DEFAULT_BANDS, plan.clone(), Some("FN42"), |_| {});
    let entry = utc(2026, 11, 17, 6, 0, 0);
    h.dial = 0;
    let a = h.slot(entry, &two_cqs(), None);
    assert!(hop_of(&a).is_none(), "fallback 20m is in the window");
    h.dial = 7_074_000;
    let mut moved = None;
    for k in 1..=20 {
        let a = h.slot(entry + secs(15 * k), &two_cqs(), None);
        if let Some((dial, reason)) = hop_of(&a) {
            moved = Some((k, dial, reason));
            break;
        }
    }
    let (k, dial, reason) = moved.expect("a plan hop within one hop interval");
    assert_eq!(k, 1, "the first live dial decides");
    assert_eq!(dial, 14_074_000);
    assert!(
        matches!(reason, BandHopReason::PlanWindow { .. }),
        "{reason:?}"
    );

    // The first live dial on a window band owes nothing, and a later manual
    // QSY inside the window is still left alone.
    let mut h = Harness::new(DEFAULT_BANDS, plan, Some("FN42"), |_| {});
    h.dial = 0;
    assert!(hop_of(&h.slot(entry, &two_cqs(), None)).is_none());
    h.dial = 18_100_000;
    assert!(hop_of(&h.slot(entry + secs(15), &two_cqs(), None)).is_none());
    h.dial = 7_074_000;
    for k in 2..40 {
        let a = h.slot(entry + secs(15 * k), &two_cqs(), None);
        assert!(hop_of(&a).is_none(), "no hop after a manual QSY (k = {k})");
    }
}

#[test]
fn pan385_window_rotation_skips_duplicate_bands() {
    // `[40m, 40m, 80m]` passes validation (duplicates stay legal); the quiet
    // rotation must still reach 80m and come back.
    let plan = vec![window(
        "00:00-06:00",
        &[Band::Band40m, Band::Band40m, Band::Band80m],
    )];
    let bands = &[(14_074_000, "20m"), (7_074_000, "40m")];
    let mut h = Harness::new(bands, plan, Some("FN42"), |_| {});
    h.dial = 7_074_000;
    let start = utc(2026, 11, 17, 1, 0, 0);
    let mut hops = Vec::new();
    for k in 0..80 {
        let a = h.slot(start + secs(15 * k), &[], None);
        if let Some((dial, _)) = hop_of(&a) {
            hops.push(dial);
            h.dial = dial;
        }
    }
    assert!(hops.len() >= 3, "hops: {hops:?}");
    assert_eq!(&hops[..3], &[3_573_000, 7_074_000, 3_573_000]);
}

#[test]
fn pan452_window_rotation_reaches_a_band_after_a_non_adjacent_duplicate() {
    // `[40m, 80m, 40m, 20m]` passes validation. The quiet rotation runs
    // over the distinct bands in first-occurrence order, 40m -> 80m -> 20m,
    // so 20m is reached (before PAN-452 it alternated 80m and 40m).
    let plan = vec![window(
        "00:00-06:00",
        &[Band::Band40m, Band::Band80m, Band::Band40m, Band::Band20m],
    )];
    let bands = &[(14_074_000, "20m"), (7_074_000, "40m")];
    let mut h = Harness::new(bands, plan, Some("FN42"), |_| {});
    h.dial = 7_074_000;
    let start = utc(2026, 11, 17, 1, 0, 0);
    let mut hops = Vec::new();
    for k in 0..120 {
        let a = h.slot(start + secs(15 * k), &[], None);
        if let Some((dial, reason)) = hop_of(&a) {
            assert!(
                matches!(reason, BandHopReason::LowActivity { .. }),
                "{reason:?}"
            );
            hops.push(dial);
            h.dial = dial;
        }
    }
    assert!(hops.len() >= 4, "hops: {hops:?}");
    assert_eq!(&hops[..4], &[3_573_000, 14_074_000, 7_074_000, 3_573_000]);
}

#[test]
fn pan452_window_with_one_distinct_band_never_hops() {
    let plan = vec![window("00:00-06:00", &[Band::Band40m, Band::Band40m])];
    let bands = &[(14_074_000, "20m"), (7_074_000, "40m")];
    let mut h = Harness::new(bands, plan, Some("FN42"), |_| {});
    h.dial = 7_074_000;
    let start = utc(2026, 11, 17, 1, 0, 0);
    for k in 0..120 {
        let a = h.slot(start + secs(15 * k), &[], None);
        assert!(hop_of(&a).is_none(), "no hop at slot {k}");
    }
}

#[test]
fn pan385_plan_hop_held_by_an_exchange_is_reported_as_waiting() {
    let mut h = Harness::new(SCENARIO2_BANDS, scenario2_plan(), Some("FN42"), |_| {});
    h.dial = 14_074_000;
    let a = h.slot(utc(2026, 11, 16, 23, 59, 30), &two_cqs(), None);
    assert!(hop_of(&a).is_none());
    assert!(h.op.take_held_band_hop().is_none());
    h.op.set_exchange_in_progress(true);
    let midnight = utc(2026, 11, 17, 0, 0, 0);
    let a = h.slot(midnight, &two_cqs(), None);
    assert!(
        hop_of(&a).is_none(),
        "no ChangeBand while a partner answered"
    );
    let held = h.op.take_held_band_hop().expect("the window move is held");
    assert!(h.op.take_held_band_hop().is_none(), "taken once");
    assert!(matches!(held, BandHopReason::PlanWindow { .. }), "{held:?}");
    assert_eq!(held.to_band(), Some(Band::Band40m));
    assert_eq!(
        held.waiting_text("QSO with K1ABC in progress"),
        "Band hop 20m -> 40m waiting: QSO with K1ABC in progress"
    );
    h.op.set_exchange_in_progress(false);
    let a = h.slot(midnight + secs(15), &two_cqs(), None);
    assert!(
        hop_of(&a).is_some(),
        "the held move runs once the exchange ends"
    );
    assert!(h.op.take_held_band_hop().is_none());
}

#[test]
fn pan447_no_hop_while_fox_mode_is_on() {
    // Plan entry: no hop for as long as Fox mode is on.
    let mut h = Harness::new(SCENARIO2_BANDS, scenario2_plan(), Some("FN42"), |_| {});
    h.dial = 14_074_000;
    let a = h.slot(utc(2026, 11, 16, 23, 59, 30), &two_cqs(), None);
    assert!(hop_of(&a).is_none());
    h.op.set_fox_mode(true);
    let midnight = utc(2026, 11, 17, 0, 0, 0);
    for k in 0..10 {
        let a = h.slot(midnight + secs(15 * k), &two_cqs(), None);
        assert!(hop_of(&a).is_none(), "no hop while Fox mode is on");
    }
    h.op.set_fox_mode(false);
    let a = h.slot(midnight + secs(150), &two_cqs(), None);
    assert_eq!(hop_of(&a).map(|(d, _)| d), Some(7_076_000));

    // The AC1 needed-activity hop (due at t0 + 600 s) never fires while
    // Fox mode is on, and fires on the first slot after it ends.
    let mut h = Harness::new(AC1_BANDS, vec![], Some("FN42"), |_| {});
    h.dial = 14_074_000;
    h.op.set_fox_mode(true);
    for k in 0..=60 {
        let now = t0() + secs(15 * k);
        let a = h.slot(now, &two_cqs(), Some(ac1_snapshot(now)));
        assert!(
            hop_of(&a).is_none(),
            "no hop at t0 + {} s while Fox mode is on",
            15 * k
        );
        assert!(
            h.op.take_held_band_hop().is_none(),
            "a needed-activity hop is not reported as held"
        );
    }
    h.op.set_fox_mode(false);
    let now = t0() + secs(15 * 61);
    let a = h.slot(now, &two_cqs(), Some(ac1_snapshot(now)));
    let (dial, reason) = hop_of(&a).expect("hop once Fox mode ends");
    assert_eq!(dial, 18_100_000);
    assert!(reason
        .why_text()
        .starts_with("Band hop 20m -> 17m: needed activity"));
}

#[test]
fn pan447_plan_hop_held_by_fox_mode_runs_once_it_ends() {
    let mut h = Harness::new(SCENARIO2_BANDS, scenario2_plan(), Some("FN42"), |_| {});
    h.dial = 14_074_000;
    let a = h.slot(utc(2026, 11, 16, 23, 59, 30), &two_cqs(), None);
    assert!(hop_of(&a).is_none());
    assert!(h.op.take_held_band_hop().is_none());
    h.op.set_fox_mode(true);
    let midnight = utc(2026, 11, 17, 0, 0, 0);
    for k in 0..4 {
        let a = h.slot(midnight + secs(15 * k), &two_cqs(), None);
        assert!(hop_of(&a).is_none(), "no ChangeBand while Fox mode is on");
        let held = h.op.take_held_band_hop().expect("the window move is held");
        assert!(h.op.take_held_band_hop().is_none(), "taken once");
        assert!(matches!(held, BandHopReason::PlanWindow { .. }), "{held:?}");
        assert_eq!(held.to_band(), Some(Band::Band40m));
        assert_eq!(
            held.waiting_text("Fox mode is on"),
            "Band hop 20m -> 40m waiting: Fox mode is on"
        );
    }
    h.op.set_fox_mode(false);
    let a = h.slot(midnight + secs(60), &two_cqs(), None);
    assert_eq!(
        hop_of(&a).map(|(d, _)| d),
        Some(7_076_000),
        "the held move runs on the first slot after Fox mode ends"
    );
    assert!(h.op.take_held_band_hop().is_none());
}

#[test]
fn pan385_needed_hop_needs_a_known_current_band_count() {
    // 20m's needed set has not loaded, so the snapshot has no 20m entry: a
    // tie read as 0 on 20m must not become a hop.
    let mut h = Harness::new(AC1_BANDS, vec![], Some("FN42"), |_| {});
    h.dial = 14_074_000;
    let partial = |now| Some(snap(now, &[(Band::Band17m, 2, 0)]));
    assert!(first_hop_slot(&mut h, 60, partial).is_none());
}
