// PAN-381 AC9: an external (WSJT-X UDP) decode goes through the exact
// production view conversion a native decode takes, so it is scored and
// enriched identically. The two views differ only in the `external` tag
// and, for an FT4 external decode, in the mode its WSJT-X glyph named.
use super::*;
use crate::coordinator::pipeline::{DecodeOrigin, RelayedDecode};
use crate::priority_evaluator::CachedStationLookup;
use pancetta_config::OperatingMode;
use std::collections::HashSet;

/// A lookup with K1ABC worked on 20m and JA marked needed and ATNO, so
/// `worked_before`, `needed` and `atno` all carry real values.
fn seeded_lookup() -> CachedStationLookup {
    let lookup = CachedStationLookup::new();
    lookup.record_worked("K1ABC", "20m");
    lookup.update_needed_dxcc(HashSet::from(["JA".to_string()]));
    lookup.update_needed_atno(HashSet::from(["JA".to_string()]));
    lookup
}

fn relayed(text: &str, dial_hz: u64, origin: DecodeOrigin) -> RelayedDecode {
    let mut message = pancetta_ft8::DecodedMessage::from_ft8lib(text, 1302.0, -5.0, 0);
    message.time_offset = 0.2;
    RelayedDecode {
        message,
        dial_hz,
        origin,
    }
}

/// Run the relay's own conversion with fresh per-relay caches, the way
/// `directed_decode_table` does, and pancetta's protocol mode FT8 (0).
fn view(lookup: &CachedStationLookup, relayed: &RelayedDecode) -> pancetta_tui::DecodedMessageView {
    let scorer = pancetta_qso::PriorityScorer::new(
        pancetta_qso::priority::PriorityWeights::default(),
        Box::new(lookup.clone()),
    );
    let mut last_grids = RelayCache::default();
    let mut last_rank_inputs = RelayCache::default();
    decoded_view_for_relay(
        relayed,
        &mut RelayDecodeCtx {
            last_grids: &mut last_grids,
            last_rank_inputs: &mut last_rank_inputs,
            station_coords: Some((35.0, -97.0)),
            lookup,
            scorer: &scorer,
            our_callsign: "K5ARH",
            protocol_mode: 0,
        },
    )
}

/// Every scoring and enrichment field the plan's AC9 names must match.
fn assert_scored_alike(
    native: &pancetta_tui::DecodedMessageView,
    external: &pancetta_tui::DecodedMessageView,
) {
    assert_eq!(
        native.priority_score, external.priority_score,
        "priority_score"
    );
    assert_eq!(
        native.priority_factors, external.priority_factors,
        "priority_factors"
    );
    assert_eq!(native.needed, external.needed, "needed");
    assert_eq!(native.atno, external.atno, "atno");
    assert_eq!(
        native.worked_before, external.worked_before,
        "worked_before"
    );
    assert_eq!(native.band_needed, external.band_needed, "band_needed");
    assert_eq!(native.grid_square, external.grid_square, "grid_square");
    assert_eq!(native.distance, external.distance, "distance");
    assert_eq!(native.bearing, external.bearing, "bearing");
    assert_eq!(
        native.is_directed_at_us, external.is_directed_at_us,
        "is_directed_at_us"
    );
    // The frame's own fields reach both views unchanged.
    assert_eq!(native.call_sign, external.call_sign);
    assert_eq!(native.message, external.message);
    assert_eq!(native.snr, external.snr);
    assert_eq!(native.delta_freq, external.delta_freq);
    assert_eq!(native.delta_time, external.delta_time);
    assert_eq!(native.frequency, external.frequency);
}

#[test]
fn external_decode_scores_like_native() {
    let lookup = seeded_lookup();
    let ext = DecodeOrigin::WsjtxUdp {
        mode: OperatingMode::Ft8,
    };
    for text in [
        "CQ K1ABC FN42",
        "CQ DX JA1ABC PM95",
        "K5ARH W1ABC FN31",
        "JA2EJP N4BP 73",
    ] {
        let native = view(&lookup, &relayed(text, 14_074_000, DecodeOrigin::Native));
        let external = view(&lookup, &relayed(text, 14_074_000, ext));
        assert_scored_alike(&native, &external);
        assert!(!native.external, "{text}: native decode is not tagged");
        assert!(
            external.external,
            "{text}: WSJT-X decode is tagged external"
        );
        assert_eq!(native.mode, "FT8");
        assert_eq!(external.mode, "FT8");
    }

    // The seeds took effect, so the comparison above covers real values,
    // not two empty views.
    let k1 = view(&lookup, &relayed("CQ K1ABC FN42", 14_074_000, ext));
    assert!(k1.worked_before, "K1ABC was worked on 20m");
    assert!(k1.priority_score.is_some());
    assert!(k1.priority_factors.is_some());
    assert_eq!(k1.grid_square.as_deref(), Some("FN42"));
    assert!(k1.distance.is_some() && k1.bearing.is_some());
    let ja = view(&lookup, &relayed("CQ DX JA1ABC PM95", 14_074_000, ext));
    assert!(ja.needed && ja.atno, "JA is seeded needed and ATNO");
    let directed = view(&lookup, &relayed("K5ARH W1ABC FN31", 14_074_000, ext));
    assert!(directed.is_directed_at_us);
}

#[test]
fn external_mode_comes_from_wsjtx_glyph() {
    let lookup = seeded_lookup();
    let native = view(
        &lookup,
        &relayed("CQ K1ABC FN42", 14_080_000, DecodeOrigin::Native),
    );
    let ft4 = view(
        &lookup,
        &relayed(
            "CQ K1ABC FN42",
            14_080_000,
            DecodeOrigin::WsjtxUdp {
                mode: OperatingMode::Ft4,
            },
        ),
    );
    // pancetta's own protocol mode is FT8 (protocol_mode 0); the external
    // decode shows the FT4 its WSJT-X `+` glyph named.
    assert_eq!(native.mode, "FT8");
    assert_eq!(ft4.mode, "FT4");
    assert!(ft4.external);
    assert!(!native.external);
    // Mode is display only: scoring is unchanged.
    assert_scored_alike(&native, &ft4);
}

#[test]
fn dial_band_scopes_external_like_native() {
    let lookup = seeded_lookup();
    let ext = DecodeOrigin::WsjtxUdp {
        mode: OperatingMode::Ft8,
    };
    for (dial_hz, worked) in [(14_074_000, true), (7_074_000, false)] {
        let native = view(
            &lookup,
            &relayed("CQ K1ABC FN42", dial_hz, DecodeOrigin::Native),
        );
        let external = view(&lookup, &relayed("CQ K1ABC FN42", dial_hz, ext));
        assert_eq!(
            native.worked_before, worked,
            "native worked_before at {dial_hz} Hz"
        );
        assert_eq!(
            external.worked_before, worked,
            "external worked_before at {dial_hz} Hz"
        );
        assert_eq!(external.frequency, dial_hz as f64 / 1_000_000.0);
        assert_scored_alike(&native, &external);
    }
}

/// The DX Hunter rescores a QSO outcome sends for `call`, as
/// `(external, score)` pairs sorted native first.
fn refreshed_scores(
    scorer: &pancetta_qso::PriorityScorer,
    last_rank_inputs: &RelayCache<(String, bool), RankInputs>,
    call: &str,
) -> Vec<(bool, u32)> {
    let mut scores: Vec<(bool, u32)> = outcome_rank_refresh(scorer, last_rank_inputs, call)
        .into_iter()
        .map(|refresh| match refresh {
            pancetta_tui::tui_runner::TuiMessage::DxRankRefresh {
                score, external, ..
            } => (external, score),
            other => panic!("expected a DxRankRefresh, got {other:?}"),
        })
        .collect();
    scores.sort();
    scores
}

/// Feed decodes through the relay's conversion with shared caches, as the
/// relay loop does across frames, and return its rank-input memory.
fn rank_inputs_after(
    lookup: &CachedStationLookup,
    scorer: &pancetta_qso::PriorityScorer,
    decodes: &[RelayedDecode],
) -> RelayCache<(String, bool), RankInputs> {
    let mut last_grids = RelayCache::default();
    let mut last_rank_inputs = RelayCache::default();
    for relayed in decodes {
        decoded_view_for_relay(
            relayed,
            &mut RelayDecodeCtx {
                last_grids: &mut last_grids,
                last_rank_inputs: &mut last_rank_inputs,
                station_coords: Some((35.0, -97.0)),
                lookup,
                scorer,
                our_callsign: "K5ARH",
                protocol_mode: 0,
            },
        );
    }
    last_rank_inputs
}

/// Codex review on PR #700: the TUI ignores a WSJT-X decode for a row
/// pancetta heard itself, so the QSO-outcome rescore of that row must keep
/// the native band, grid and SNR. Before the fix, one per-call slot held the
/// newest decode, so a later external decode on another band replaced the
/// native inputs and the 20m-worked K1ABC was rescored as unworked on 40m.
/// Now each origin keeps its own inputs, and each rescore names its origin
/// so the TUI applies it only to a row of that origin.
#[test]
fn external_decode_keeps_native_rank_inputs() {
    let lookup = seeded_lookup();
    let scorer = pancetta_qso::PriorityScorer::new(
        pancetta_qso::priority::PriorityWeights::default(),
        Box::new(lookup.clone()),
    );
    let ext = DecodeOrigin::WsjtxUdp {
        mode: OperatingMode::Ft8,
    };
    let native_20m = relayed("CQ K1ABC FN42", 14_074_000, DecodeOrigin::Native);
    let mut external_40m = relayed("CQ K1ABC FN31", 7_074_000, ext);
    external_40m.message.snr_db = 12.0;

    let native_only = refreshed_scores(
        &scorer,
        &rank_inputs_after(&lookup, &scorer, std::slice::from_ref(&native_20m)),
        "K1ABC",
    );
    let external_only = refreshed_scores(
        &scorer,
        &rank_inputs_after(&lookup, &scorer, std::slice::from_ref(&external_40m)),
        "K1ABC",
    );
    let [(false, native_score)] = native_only[..] else {
        panic!("one native rescore expected, got {native_only:?}");
    };
    let [(true, external_score)] = external_only[..] else {
        panic!("one external rescore expected, got {external_only:?}");
    };
    // The two inputs score differently (worked on 20m, not on 40m), so the
    // equalities below are each origin's own inputs surviving.
    assert_ne!(native_score, external_score);

    // Either order: the native rescore still uses the native 20m inputs, and
    // the external rescore the WSJT-X 40m inputs.
    for decodes in [
        [native_20m.clone(), external_40m.clone()],
        [external_40m.clone(), native_20m.clone()],
    ] {
        assert_eq!(
            refreshed_scores(
                &scorer,
                &rank_inputs_after(&lookup, &scorer, &decodes),
                "K1ABC"
            ),
            vec![(false, native_score), (true, external_score)],
            "an external decode replaced a native row's rank inputs"
        );
    }

    // A later external decode still updates the external inputs.
    let mut external_20m = relayed("CQ K1ABC FN42", 14_074_000, ext);
    external_20m.message.snr_db = -15.0;
    let latest = rank_inputs_after(&lookup, &scorer, &[external_40m, external_20m]);
    let inputs = latest
        .get(&("K1ABC".to_owned(), true))
        .expect("external K1ABC cached");
    assert_eq!(inputs.dial_mhz, 14.074);
    assert_eq!(inputs.snr_db, -15.0);
    assert!(latest.get(&("K1ABC".to_owned(), false)).is_none());
}

/// Feed decodes through the relay's conversion with one shared grid and
/// rank-input memory, as the relay loop does across frames (PAN-434), and
/// return every view plus the rank-input memory.
fn views_after(
    lookup: &CachedStationLookup,
    decodes: &[RelayedDecode],
) -> (
    Vec<pancetta_tui::DecodedMessageView>,
    RelayCache<(String, bool), RankInputs>,
) {
    let scorer = pancetta_qso::PriorityScorer::new(
        pancetta_qso::priority::PriorityWeights::default(),
        Box::new(lookup.clone()),
    );
    let mut last_grids = RelayCache::default();
    let mut last_rank_inputs = RelayCache::default();
    let views = decodes
        .iter()
        .map(|relayed| {
            decoded_view_for_relay(
                relayed,
                &mut RelayDecodeCtx {
                    last_grids: &mut last_grids,
                    last_rank_inputs: &mut last_rank_inputs,
                    station_coords: Some((35.0, -97.0)),
                    lookup,
                    scorer: &scorer,
                    our_callsign: "K5ARH",
                    protocol_mode: 0,
                },
            )
        })
        .collect();
    (views, last_rank_inputs)
}

const WSJTX_FT8: DecodeOrigin = DecodeOrigin::WsjtxUdp {
    mode: OperatingMode::Ft8,
};

/// The grid, distance (km) and bearing (degrees) a view shows, rounded.
fn located(view: &pancetta_tui::DecodedMessageView) -> (Option<&str>, Option<i64>, Option<i64>) {
    (
        view.grid_square.as_deref(),
        view.distance.map(|d| d.round() as i64),
        view.bearing.map(|b| b.round() as i64),
    )
}

/// PAN-434 ticket scenario 1: pancetta heard `CQ K1ABC FN42`, WSJT-X then
/// sent `CQ K1ABC FN31` on the same band, and pancetta decodes a grid-less
/// RR73. The native row keeps pancetta's FN42 with its distance and bearing.
#[test]
fn wsjtx_grid_does_not_fill_a_native_decode() {
    let lookup = seeded_lookup();
    let (views, _) = views_after(
        &lookup,
        &[
            relayed("CQ K1ABC FN42", 14_074_000, DecodeOrigin::Native),
            relayed("CQ K1ABC FN31", 14_074_000, WSJTX_FT8),
            relayed("K5ARH K1ABC RR73", 14_074_000, DecodeOrigin::Native),
        ],
    );
    let [native_cq, wsjtx_cq, rr73] = &views[..] else {
        panic!("three views expected, got {}", views.len());
    };
    // The two CQs locate the station differently, so the RR73's match with
    // the native CQ is pancetta's own grid surviving.
    assert_eq!(located(native_cq), (Some("FN42"), Some(2397), Some(62)));
    assert_eq!(located(wsjtx_cq), (Some("FN31"), Some(2213), Some(64)));
    assert!(!rr73.external, "the RR73 is pancetta's own decode");
    assert_eq!(rr73.grid_square.as_deref(), Some("FN42"));
    assert_eq!(rr73.distance, native_cq.distance);
    assert_eq!(rr73.bearing, native_cq.bearing);
    assert_ne!(rr73.distance, wsjtx_cq.distance);
    assert_ne!(rr73.bearing, wsjtx_cq.bearing);
}

/// PAN-434: the WSJT-X grid stays out of the native score and the native
/// rank inputs a QSO-outcome rescore uses. FN31 is already worked on 20m and
/// FN42 is not, so a leaked FN31 would turn `grid_new_on_band` off.
#[test]
fn wsjtx_grid_stays_out_of_native_rank_inputs() {
    let lookup = seeded_lookup();
    lookup.record_worked_grid("FN31", "20m");
    let native_cq = relayed("CQ K1ABC FN42", 14_074_000, DecodeOrigin::Native);
    let native_rr73 = relayed("K5ARH K1ABC RR73", 14_074_000, DecodeOrigin::Native);
    let (views, rank_inputs) = views_after(
        &lookup,
        &[
            native_cq.clone(),
            relayed("CQ K1ABC FN31", 14_074_000, WSJTX_FT8),
            native_rr73.clone(),
        ],
    );
    let native_inputs = rank_inputs
        .get(&("K1ABC".to_owned(), false))
        .expect("native K1ABC rank inputs cached");
    assert_eq!(native_inputs.grid.as_deref(), Some("FN42"));
    let external_inputs = rank_inputs
        .get(&("K1ABC".to_owned(), true))
        .expect("external K1ABC rank inputs cached");
    assert_eq!(external_inputs.grid.as_deref(), Some("FN31"));

    let rr73 = views.last().expect("RR73 view");
    let factors = rr73.priority_factors.as_ref().expect("RR73 is scored");
    assert!(factors.grid_new_on_band, "FN42 is still new on 20m");

    // The same RR73 in a session without the WSJT-X decode scores alike.
    let (control, _) = views_after(&lookup, &[native_cq, native_rr73]);
    let control_rr73 = control.last().expect("control RR73 view");
    assert_eq!(rr73.priority_score, control_rr73.priority_score);
    assert_eq!(rr73.priority_factors, control_rr73.priority_factors);
}

/// PAN-434: neither origin borrows the other's grid, and each keeps its own.
#[test]
fn wsjtx_decode_does_not_borrow_a_native_grid() {
    let lookup = seeded_lookup();

    // A grid-less WSJT-X frame does not borrow pancetta's grid.
    let (views, _) = views_after(
        &lookup,
        &[
            relayed("CQ K1ABC FN42", 14_074_000, DecodeOrigin::Native),
            relayed("K5ARH K1ABC RR73", 14_074_000, WSJTX_FT8),
        ],
    );
    let wsjtx_rr73 = views.last().expect("WSJT-X RR73 view");
    assert!(wsjtx_rr73.external);
    assert_eq!(located(wsjtx_rr73), (None, None, None));

    // A grid-less native frame does not borrow WSJT-X's grid.
    let (views, _) = views_after(
        &lookup,
        &[
            relayed("CQ K1ABC FN31", 14_074_000, WSJTX_FT8),
            relayed("K5ARH K1ABC RR73", 14_074_000, DecodeOrigin::Native),
        ],
    );
    let native_rr73 = views.last().expect("native RR73 view");
    assert!(!native_rr73.external);
    assert_eq!(located(native_rr73), (None, None, None));

    // A grid-less WSJT-X frame keeps the grid WSJT-X itself sent.
    let (views, _) = views_after(
        &lookup,
        &[
            relayed("CQ K1ABC FN42", 14_074_000, DecodeOrigin::Native),
            relayed("CQ K1ABC FN31", 14_074_000, WSJTX_FT8),
            relayed("K5ARH K1ABC RR73", 14_074_000, WSJTX_FT8),
        ],
    );
    let [_, wsjtx_cq, wsjtx_rr73] = &views[..] else {
        panic!("three views expected, got {}", views.len());
    };
    assert!(wsjtx_rr73.external);
    assert_eq!(wsjtx_rr73.grid_square.as_deref(), Some("FN31"));
    assert_eq!(wsjtx_rr73.distance, wsjtx_cq.distance);
    assert_eq!(wsjtx_rr73.bearing, wsjtx_cq.bearing);
    assert!(wsjtx_rr73.distance.is_some() && wsjtx_rr73.bearing.is_some());
}
