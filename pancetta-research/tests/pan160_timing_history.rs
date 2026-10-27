use pancetta_research::chrono_replay::{compare_timing_slots, TimingSlotResult};
use std::path::Path;

#[test]
fn persisted_pairs_recompute_from_message_identities() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../research/scorecards/pan160");
    let records: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("synthetic-sessions.json")).unwrap())
            .unwrap();
    let mut sessions = 0;
    for record in records["sessions"].as_array().unwrap() {
        let a: Vec<TimingSlotResult> = serde_json::from_value(record["arms"][0].clone()).unwrap();
        let b: Vec<TimingSlotResult> = serde_json::from_value(record["arms"][1].clone()).unwrap();
        assert_eq!(compare_timing_slots(&a, &b, 10), record["comparison"]);
        assert!(a.iter().any(|s| !s.eligible.is_empty()));
        assert!(b
            .iter()
            .all(|s| s.diagnostics["ap"]["attempts"].as_u64().unwrap() <= 32));
        sessions += 1;
    }
    assert_eq!(sessions, 20);
}

#[cfg(feature = "research-eval")]
#[test]
fn generated_prior_slots_reach_real_retry_path_and_preserve_arm_independence() {
    use pancetta_ft8::ap::{ApContext, MyCallAp, RecentCallAp};
    use pancetta_research::{synth::*, DecoderUnderTest, Ft8Decoder};
    use std::time::{Duration, SystemTime};
    let temp = tempfile::tempdir().unwrap();
    let make = |enabled| {
        Ft8Decoder::with_default_config()
            .with_ap_dt_history(enabled)
            .with_max_passes(1)
            .with_max_candidates(50)
            .with_max_sync_candidates(100)
            .with_ldpc_iterations(30)
            .with_osd_depth(None)
            .with_ap_context(ApContext {
                my_call: MyCallAp::new("K1ABC"),
                recent_calls: vec![RecentCallAp::new("W1AW", -15.).unwrap()],
                ..Default::default()
            })
    };
    let a = make(false);
    let b = make(true);
    let mut attempts = 0;
    for slot in 0..10 {
        let signal = modulate_message_at("K1ABC W1AW -12", 1000.).unwrap();
        let mut audio = place_in_slot(&signal, 0.36, 0., 180000);
        let other = place_in_slot(
            &modulate_message_at("CQ K2XYZ FN42", 1600.).unwrap(),
            0.8,
            0.,
            180000,
        );
        for (s, o) in audio.iter_mut().zip(other) {
            *s += o * 0.5;
        }
        add_awgn_2500hz_ref(
            &mut audio,
            signal_rms(&signal),
            if slot < 2 {
                -6.
            } else {
                -19. - (slot % 3) as f64
            },
            100000 + slot,
        );
        let path = temp.path().join("slot.wav");
        let mut w = hound::WavWriter::create(
            &path,
            hound::WavSpec {
                channels: 1,
                sample_rate: 12000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )
        .unwrap();
        for s in audio {
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1700000000 + 15 * slot);
        let calls = vec!["W1AW".into()];
        if slot == 2 {
            assert_eq!(a.eligible_timing_calls(&calls, at), calls);
            assert_eq!(b.eligible_timing_calls(&calls, at), calls);
        }
        for d in [&a, &b] {
            d.set_slot_time(at);
            d.decode_wav(&path).unwrap();
        }
        assert_eq!(a.timing_diagnostics().ap.attempts, 0);
        attempts += b.timing_diagnostics().ap.attempts;
    }
    assert!(
        attempts > 0,
        "a real learned prior must cause actual LDPC retries"
    );
}
