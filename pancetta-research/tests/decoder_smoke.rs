//! Smoke test: Ft8Decoder over a known fixture decodes at least one message.

use pancetta_research::decoder::{DecoderUnderTest, Ft8Decoder};
use std::path::PathBuf;

fn fixture(rel: &str) -> PathBuf {
    let workspace = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(workspace)
        .parent()
        .unwrap()
        .join("pancetta-ft8/tests/fixtures/wav")
        .join(rel)
}

#[test]
fn ft8_decoder_finds_at_least_one_decode_in_generated_cq() {
    let path = fixture("generated/ft8_cq.wav");
    assert!(path.exists(), "fixture missing: {}", path.display());

    let decoder = Ft8Decoder::with_default_config();
    let decodes = decoder.decode_wav(&path).expect("decode should not error");
    assert!(
        !decodes.is_empty(),
        "expected at least one decode in {}, got 0",
        path.display(),
    );
    // The generated CQ fixture should produce a CQ message.
    assert!(
        decodes.iter().any(|d| d.message.contains("CQ")),
        "expected a CQ decode, got: {:?}",
        decodes.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

#[test]
fn ft8_decoder_config_snapshot_is_json_object() {
    let decoder = Ft8Decoder::with_default_config();
    let snap = decoder.config_snapshot();
    assert!(
        snap.is_object(),
        "config snapshot should be a JSON object, got: {snap:?}"
    );
}

#[test]
fn ft8_decoder_identity_includes_version() {
    let decoder = Ft8Decoder::with_default_config();
    let id = decoder.identity();
    assert!(id.starts_with("pancetta-ft8@"));
}

/// PAN-7: the flag must be reachable from the research harness AND land in
/// every scorecard's `ConfigInfo.decoder`, or the ship gate is unmeasurable.
/// hb-228 shipped with neither a builder nor an eval flag and was measurable
/// only through a bespoke example — do not repeat that.
#[test]
fn ft8lib_sync_seeds_builder_reaches_config_snapshot() {
    let off = Ft8Decoder::with_default_config();
    assert_eq!(
        off.config_snapshot()["ft8lib_sync_seeds_enabled"].as_bool(),
        Some(false),
        "the PAN-7 flag must appear in the config snapshot and default false"
    );

    let on = Ft8Decoder::with_default_config().with_ft8lib_sync_seeds_enabled(true);
    assert_eq!(
        on.config_snapshot()["ft8lib_sync_seeds_enabled"].as_bool(),
        Some(true),
        "the research builder must mutate the wrapped Ft8Config"
    );
}
