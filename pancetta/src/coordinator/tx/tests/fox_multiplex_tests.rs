//! PAN-386: Fox streams on 60 Hz slots and type 0.1 multiplexing at encode
//! time.

use super::fox_multiplex::{parse_fox_part, plan_fox_frames, FoxPart};
use super::*;
use crate::message_bus::{TransmitRequestItem, TxOrigin};
use pancetta_ft8::{Ft8Config, Ft8Decoder, Protocol, ProtocolParams, WINDOW_SAMPLES};
use std::collections::HashSet;

fn item(text: &str, hz: f64, qso: &str) -> TransmitRequestItem {
    TransmitRequestItem {
        message_text: text.to_string(),
        frequency_offset: hz,
        qso_id: Some(qso.to_string()),
        origin: TxOrigin::Local,
        remote_client_key_id: None,
        attempt_id: None,
    }
}

fn fox_set(ids: &[&str]) -> HashSet<String> {
    ids.iter()
        .map(|id| super::super::active_tx_qso_key(id))
        .collect()
}

fn encode(
    items: &[TransmitRequestItem],
    protocol: Protocol,
    fox_ids: &[&str],
) -> MultiTxEncodeOutcome {
    let set = std::sync::RwLock::new(fox_set(fox_ids));
    let mut enc = Ft8Encoder::new();
    let params = ProtocolParams::from_protocol(protocol);
    let is_fox = fox_stream_predicate(&set);
    let outcome =
        encode_and_modulate_multi_tx_with_fox(&mut enc, protocol, &params, items, &is_fox);
    outcome
}

/// Decode a bundle with a Hound context for `fox`, through both production
/// decoders. Returns `(frequency, text)` per decoder.
fn decode_both(samples: &[f32], fox: &str) -> [Vec<(f64, String)>; 2] {
    let mut audio = samples.to_vec();
    audio.resize(WINDOW_SAMPLES, 0.0);
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();
    decoder.set_hound_foxes(&[fox.to_string()]);
    let native = decoder
        .decode_window(&audio)
        .unwrap()
        .into_iter()
        .map(|m| (m.frequency_offset, m.text))
        .collect();
    let reference = decoder
        .decode_window_ft8lib_with_context(&audio, Protocol::Ft8)
        .into_iter()
        .map(|m| (m.frequency_offset, m.text))
        .collect();
    [native, reference]
}

fn find_near<'a>(frames: &'a [(f64, String)], hz: f64, tol: f64) -> Vec<&'a str> {
    frames
        .iter()
        .filter(|(f, _)| (f - hz).abs() <= tol)
        .map(|(_, t)| t.as_str())
        .collect()
}

#[test]
fn parse_fox_part_reads_rr73_and_report_texts() {
    assert_eq!(
        parse_fox_part("K1ABC KH7Z RR73"),
        Some(FoxPart::Rr73 {
            hound: "K1ABC".into(),
            fox: "KH7Z".into()
        })
    );
    assert_eq!(
        parse_fox_part("W9XYZ KH7Z -8"),
        Some(FoxPart::Report {
            hound: "W9XYZ".into(),
            fox: "KH7Z".into(),
            db: -8
        })
    );
    assert_eq!(
        parse_fox_part("W9XYZ <KH1/KH7Z> +5"),
        Some(FoxPart::Report {
            hound: "W9XYZ".into(),
            fox: "KH1/KH7Z".into(),
            db: 5
        })
    );
    for text in [
        "CQ KH7Z BK29",
        "W9XYZ KH7Z R-08",
        "W9XYZ KH7Z 73",
        "W9XYZ KH7Z FN31",
        "W9XYZ KH7Z -8 extra",
        "W9XYZ KH7Z",
    ] {
        assert_eq!(parse_fox_part(text), None, "{text}");
    }
}

/// AC8: RR73 to K1ABC and a report to W9XYZ air as one type 0.1 frame at the
/// report Hound's slot; the CQ and a second report stay standard frames.
#[test]
fn fox_rr73_and_report_share_one_type01_frame_at_the_report_slot() {
    let items = vec![
        item("CQ KH7Z BK29", 300.0, "q-cq"),
        item("K1ABC KH7Z RR73", 360.0, "q-k1abc"),
        item("W9XYZ KH7Z -8", 420.0, "q-w9xyz"),
        item("JA1CCC KH7Z -12", 480.0, "q-ja1ccc"),
    ];
    let outcome = encode(
        &items,
        Protocol::Ft8,
        &["q-cq", "q-k1abc", "q-w9xyz", "q-ja1ccc"],
    );
    let samples = outcome.samples.as_ref().expect("Fox bundle modulates");
    assert!(outcome.encode_failed.is_empty());
    assert_eq!(
        outcome.item_texts,
        items
            .iter()
            .map(|i| i.message_text.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        outcome.encoded_qso_ids,
        items.iter().map(|i| i.qso_id.clone()).collect::<Vec<_>>()
    );
    assert_eq!(
        outcome
            .encoded_items
            .iter()
            .map(|i| i.frequency_offset)
            .collect::<Vec<_>>(),
        vec![300.0, 360.0, 420.0, 480.0],
        "every item keeps its own slot downstream"
    );

    for frames in decode_both(samples, "KH7Z") {
        assert_eq!(frames.len(), 3, "exactly three frames on air: {frames:?}");
        assert_eq!(
            find_near(&frames, 300.0, 4.0),
            vec!["CQ KH7Z BK29"],
            "{frames:?}"
        );
        assert_eq!(
            find_near(&frames, 420.0, 4.0),
            vec!["K1ABC RR73; W9XYZ <KH7Z> -08"],
            "{frames:?}"
        );
        assert_eq!(
            find_near(&frames, 480.0, 4.0),
            vec!["JA1CCC KH7Z -12"],
            "{frames:?}"
        );
        assert!(find_near(&frames, 360.0, 20.0).is_empty(), "{frames:?}");
    }
}

/// With no Fox-admitted item the Fox-aware encode is the legacy encode: one
/// standard frame per item, summed by `modulate_multi_tx`.
#[test]
fn ordinary_items_never_multiplex() {
    let items = vec![
        item("CQ KH7Z BK29", 300.0, "q-cq"),
        item("K1ABC KH7Z RR73", 375.0, "q-k1abc"),
        item("W9XYZ KH7Z -8", 450.0, "q-w9xyz"),
        item("JA1CCC KH7Z -12", 525.0, "q-ja1ccc"),
    ];
    let outcome = encode(&items, Protocol::Ft8, &[]);
    let samples = outcome.samples.expect("75 Hz spacing modulates");

    let params = ProtocolParams::from_protocol(Protocol::Ft8);
    let mut enc = Ft8Encoder::new();
    let symbols: Vec<Vec<u8>> = items
        .iter()
        .map(|i| enc.encode_message(&i.message_text, None).unwrap().to_vec())
        .collect();
    let legacy_items: Vec<_> = symbols
        .iter()
        .zip(&items)
        .map(|(s, i)| pancetta_ft8::MultiTxItem {
            symbols: s.as_slice(),
            frequency_offset: i.frequency_offset - 200.0,
            params: &params,
        })
        .collect();
    let legacy = pancetta_ft8::modulate_multi_tx(&legacy_items, 12000, 200.0, 0.5).unwrap();
    assert_eq!(samples, legacy, "byte-identical to the legacy encode");

    // And the 60 Hz Fox geometry is refused for ordinary items, as today.
    let tight = vec![
        item("K1ABC KH7Z RR73", 360.0, "q-k1abc"),
        item("W9XYZ KH7Z -8", 420.0, "q-w9xyz"),
    ];
    assert!(encode(&tight, Protocol::Ft8, &[]).samples.is_err());
}

#[test]
fn ft4_never_multiplexes() {
    let items = vec![
        item("K1ABC KH7Z RR73", 360.0, "q-k1abc"),
        item("W9XYZ KH7Z -8", 600.0, "q-w9xyz"),
    ];
    let is_fox = |_: &str| true;
    let plan = plan_fox_frames(&items, Protocol::Ft4, &is_fox);
    assert_eq!(plan.len(), 2);
    assert!(plan.iter().all(|f| f.dxpedition.is_none()));
    assert_eq!(plan[0].item_indices, vec![0]);
    assert_eq!(plan[1].item_indices, vec![1]);

    let outcome = encode(&items, Protocol::Ft4, &["q-k1abc", "q-w9xyz"]);
    assert_eq!(outcome.encoded_items.len(), 2, "per-item encode path runs");

    // Same result as the legacy per-item FT4 encode and default-guard sum.
    let params = ProtocolParams::from_protocol(Protocol::Ft4);
    let mut enc = Ft8Encoder::new();
    let symbols: Vec<Vec<u8>> = items
        .iter()
        .map(|i| enc.encode_message_protocol(&i.message_text, None).unwrap())
        .collect();
    let legacy_items: Vec<_> = symbols
        .iter()
        .zip(&items)
        .map(|(s, i)| pancetta_ft8::MultiTxItem {
            symbols: s.as_slice(),
            frequency_offset: i.frequency_offset - 200.0,
            params: &params,
        })
        .collect();
    let legacy = pancetta_ft8::modulate_multi_tx(&legacy_items, 12000, 200.0, 0.5)
        .map_err(|e| e.to_string());
    assert_eq!(outcome.samples, legacy);
}

#[test]
fn different_fox_calls_never_pair() {
    let items = vec![
        item("K1ABC KH7Z RR73", 360.0, "q-k1abc"),
        item("W9XYZ KH6Z -8", 420.0, "q-w9xyz"),
    ];
    let is_fox = |_: &str| true;
    let plan = plan_fox_frames(&items, Protocol::Ft8, &is_fox);
    assert_eq!(plan.len(), 2);
    assert!(plan.iter().all(|f| f.dxpedition.is_none()));
}

#[test]
fn ordinary_items_are_never_planned_as_a_pair() {
    let items = vec![
        item("K1ABC KH7Z RR73", 360.0, "q-k1abc"),
        item("W9XYZ KH7Z -8", 420.0, "q-w9xyz"),
    ];
    // Only one of the two is Fox-admitted.
    let is_fox = |id: &str| id == "q-k1abc";
    let plan = plan_fox_frames(&items, Protocol::Ft8, &is_fox);
    assert_eq!(plan.len(), 2);
    assert!(plan.iter().all(|f| f.dxpedition.is_none()));
}

#[test]
fn nonstandard_hound_falls_back_to_two_standard_frames() {
    let items = vec![
        item("K1ABC/P KH7Z RR73", 360.0, "q-k1abc"),
        item("W9XYZ KH7Z -8", 420.0, "q-w9xyz"),
    ];
    let outcome = encode(&items, Protocol::Ft8, &["q-k1abc", "q-w9xyz"]);
    assert_eq!(outcome.encoded_items.len(), 2);
    assert!(outcome.encode_failed.is_empty());
    let samples = outcome.samples.expect("two standard Fox frames modulate");
    let [native, _] = decode_both(&samples, "KH7Z");
    assert_eq!(
        find_near(&native, 360.0, 4.0),
        vec!["K1ABC/P KH7Z RR73"],
        "{native:?}"
    );
    assert_eq!(
        find_near(&native, 420.0, 4.0),
        vec!["W9XYZ KH7Z -08"],
        "{native:?}"
    );
    assert!(native.iter().all(|(_, t)| !t.contains(';')), "{native:?}");
}

#[test]
fn pairs_by_order_and_leaves_the_extra_rr73_standalone() {
    let items = vec![
        item("K1ABC KH7Z RR73", 360.0, "q-a"),
        item("W9XYZ KH7Z -8", 420.0, "q-b"),
        item("JA1CCC KH7Z RR73", 480.0, "q-c"),
    ];
    let is_fox = |_: &str| true;
    let plan = plan_fox_frames(&items, Protocol::Ft8, &is_fox);
    assert_eq!(plan.len(), 2);
    assert_eq!(plan[0].item_indices, vec![0, 1]);
    assert_eq!(plan[0].frequency_offset, 420.0);
    assert_eq!(plan[1].item_indices, vec![2]);
    assert!(plan[1].dxpedition.is_none());

    let outcome = encode(&items, Protocol::Ft8, &["q-a", "q-b", "q-c"]);
    assert_eq!(outcome.encoded_items.len(), 3);
    let samples = outcome.samples.expect("modulates");
    let [native, reference] = decode_both(&samples, "KH7Z");
    for frames in [native, reference] {
        assert_eq!(
            find_near(&frames, 420.0, 4.0),
            vec!["K1ABC RR73; W9XYZ <KH7Z> -08"],
            "{frames:?}"
        );
        assert_eq!(
            find_near(&frames, 480.0, 4.0),
            vec!["JA1CCC KH7Z RR73"],
            "{frames:?}"
        );
        assert!(find_near(&frames, 360.0, 20.0).is_empty(), "{frames:?}");
    }
}

#[test]
fn fox_spacing_never_applies_to_an_ordinary_frame() {
    let items = vec![
        item("CQ KH7Z BK29", 300.0, "q-cq"),
        item("W1AW K5ARH EM12", 360.0, "q-ordinary"),
    ];
    let outcome = encode(&items, Protocol::Ft8, &["q-cq"]);
    let err = outcome.samples.expect_err("an ordinary frame keeps 75 Hz");
    assert!(
        err.contains("Frequency separation 60.0 Hz too small (minimum 75.0 Hz)"),
        "{err}"
    );
}

fn entry(qso: &str, hz: f64) -> CoalesceEntry {
    CoalesceEntry {
        attempt_id: None,
        message_text: format!("{qso} KH7Z -8"),
        frequency_offset: hz,
        qso_id: Some(qso.to_string()),
        tx_parity: None,
        origin: TxOrigin::Local,
        remote_client_key_id: None,
    }
}

fn retained_ids(outcome: &CoalesceOutcome) -> Vec<String> {
    outcome
        .retained
        .iter()
        .filter_map(|e| e.qso_id.clone())
        .collect()
}

#[test]
fn coalescer_keeps_fox_streams_60_hz_apart() {
    let fox = fox_set(&["a", "b", "c"]);
    let outcome = coalesce_transmit_requests_with_fox_priority(
        vec![entry("a", 300.0), entry("b", 360.0), entry("c", 420.0)],
        1,
        |_| true,
        |key| fox.contains(key),
    );
    assert_eq!(retained_ids(&outcome), vec!["a", "b", "c"]);
    assert_eq!(outcome.freq_excluded, 0);
}

#[test]
fn coalescer_still_excludes_ordinary_streams_within_75_hz() {
    let outcome = coalesce_transmit_requests_with_fox_priority(
        vec![entry("a", 300.0), entry("b", 360.0), entry("c", 420.0)],
        MAX_RETAINED_TX_STREAMS as u32,
        |_| true,
        |_| false,
    );
    assert_eq!(retained_ids(&outcome), vec!["a", "c"]);
    assert_eq!(outcome.freq_excluded, 1);
}

#[test]
fn coalescer_excludes_an_ordinary_stream_60_hz_from_a_fox_stream() {
    let fox = fox_set(&["a"]);
    let outcome = coalesce_transmit_requests_with_fox_priority(
        vec![entry("a", 300.0), entry("b", 360.0)],
        MAX_RETAINED_TX_STREAMS as u32,
        |_| true,
        |key| fox.contains(key),
    );
    assert_eq!(retained_ids(&outcome), vec!["a"]);
    assert_eq!(outcome.freq_excluded, 1);
}
