//! PAN-369 AC1: the startup probe times the live station's busy-window work
//! (overlapped reference + native passes, join, production merge) on the
//! embedded off-air recording, not a synthetic CQ in silence.

use super::*;
use crate::{
    ap::ApContext, merge, DecodeBudget, DecodedMessage, Ft8Config, Ft8Decoder, Protocol,
    WINDOW_SAMPLES,
};

/// Collapse whitespace so formatting differences between backends cannot
/// hide (or fake) an ordering difference.
fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn texts(messages: &[DecodedMessage]) -> Vec<String> {
    messages.iter().map(|m| normalized(&m.text)).collect()
}

#[test]
fn busy_window_is_a_production_sized_off_air_window() {
    let window = busy_window_samples().expect("embedded busy window parses");
    assert_eq!(window.len(), WINDOW_SAMPLES);
    assert!(
        window
            .iter()
            .all(|s| s.is_finite() && (-1.0..1.0).contains(s)),
        "PCM16 normalized by 32768 must stay inside [-1, 1)"
    );
    // Off-air, not silence: the recording carries real band noise and
    // signals across the whole window.
    let rms = (window.iter().map(|s| s * s).sum::<f32>() / window.len() as f32).sqrt();
    assert!(rms > 1e-3, "busy window RMS {rms} looks like silence");
}

#[cfg(not(ft8lib_stub))]
#[test]
fn busy_window_matches_production_merge() {
    let window = busy_window_samples().expect("embedded busy window parses");
    assert_eq!(window.len(), WINDOW_SAMPLES);

    // The function the probe times, once.
    let probed = decode_busy_window_once(&window).expect("busy window decodes");
    assert!(probed.elapsed > Duration::ZERO);

    // The same backends, run independently and sequentially on the same
    // samples, then the production merge and hash-twin filter.
    let reference_decoder = Ft8Decoder::new(Ft8Config::default()).expect("reference decoder");
    let (expected_reference, _grid) =
        reference_decoder.decode_window_ft8lib_with_context_and_capture(&window, Protocol::Ft8);
    let mut native_decoder = Ft8Decoder::new(Ft8Config::default()).expect("native decoder");
    let (expected_native, _report) = native_decoder
        .decode_window_with_ap_scoped_partner_budgeted(
            &window,
            &ApContext::default(),
            None,
            None,
            DecodeBudget::unlimited(),
        )
        .expect("native decode");
    let merged = merge::merge_ft8lib_first(expected_reference.clone(), expected_native.clone());
    let (expected_delivered, _) = merge::drop_unresolved_hash_twins(merged.messages, Protocol::Ft8);

    // A busy band: both backends decode something, and the overlapped run
    // produced what each backend produces alone.
    assert!(
        !probed.reference.is_empty(),
        "reference pass decoded nothing"
    );
    assert!(!probed.native.is_empty(), "native pass decoded nothing");
    assert!(
        probed.delivered.len() >= 10,
        "busy window delivered only {} decodes",
        probed.delivered.len()
    );
    assert_eq!(texts(&probed.reference), texts(&expected_reference));
    assert_eq!(texts(&probed.native), texts(&expected_native));

    // Final output: same texts in the same order as the production merge.
    assert_eq!(texts(&probed.delivered), texts(&expected_delivered));

    // Shared decodes keep the reference pass's metadata (merge is
    // ft8_lib-first), and at least one of them differs from what the native
    // pass reported, so a native-only path cannot pass by matching texts.
    let mut shared_with_distinct_metadata = 0usize;
    for delivered in &probed.delivered {
        let text = normalized(&delivered.text);
        let Some(reference) = expected_reference
            .iter()
            .find(|m| normalized(&m.text) == text)
        else {
            continue;
        };
        assert_eq!(
            delivered.frequency_offset, reference.frequency_offset,
            "{text}: delivered frequency is not the reference pass's"
        );
        assert_eq!(
            delivered.time_offset, reference.time_offset,
            "{text}: delivered DT is not the reference pass's"
        );
        if let Some(native) = expected_native.iter().find(|m| normalized(&m.text) == text) {
            if native.frequency_offset != reference.frequency_offset
                || native.time_offset != reference.time_offset
            {
                shared_with_distinct_metadata += 1;
            }
        }
    }
    assert!(
        shared_with_distinct_metadata >= 1,
        "no shared decode carries reference metadata distinct from native; \
         the test cannot tell the merged pipeline from a native-only path"
    );
}
