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
