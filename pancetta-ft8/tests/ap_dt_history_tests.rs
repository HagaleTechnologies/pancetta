#![cfg(feature = "transmit")]
use pancetta_ft8::{
    ap::{ApContext, MyCallAp, RecentCallAp},
    DecodeBudget, Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, InMemoryDtHistory,
};
use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

fn audio() -> Vec<f32> {
    let symbols = Ft8Encoder::new()
        .encode_message("K1ABC W1AW -12", None)
        .unwrap();
    let signal = Ft8Modulator::new(12000, 1000., 1.)
        .unwrap()
        .modulate_symbols(&symbols, 0.)
        .unwrap();
    let mut out = vec![0.; 180000];
    for (i, s) in signal.into_iter().enumerate() {
        if i + 4320 < out.len() {
            out[i + 4320] = s;
        }
    }
    out
}
fn context() -> ApContext {
    ApContext {
        my_call: MyCallAp::new("K1ABC"),
        recent_calls: vec![RecentCallAp::new("W1AW", -10.).unwrap()],
        ..Default::default()
    }
}
fn tuples(messages: Vec<pancetta_ft8::DecodedMessage>) -> Vec<(String, u64, u64, u8)> {
    messages
        .into_iter()
        .map(|m| {
            (
                m.text,
                m.time_offset.to_bits(),
                m.frequency_offset.to_bits(),
                m.ap_level,
            )
        })
        .collect()
}
fn history(at: SystemTime) -> Arc<InMemoryDtHistory> {
    let h = Arc::new(InMemoryDtHistory::default());
    h.record("W1AW", 0.36, at - Duration::from_secs(30));
    h.record("W1AW", 0.36, at - Duration::from_secs(15));
    h
}
fn config(enabled: bool) -> Ft8Config {
    Ft8Config {
        ap_dt_history_enabled: enabled,
        max_decode_passes: 1,
        max_candidates: 50,
        max_sync_candidates: 100,
        osd_depth: None,
        ..Default::default()
    }
}
#[test]
fn default_off_and_absent_prior_preserve_waveform_outputs() {
    let samples = audio();
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
    let mut control = Ft8Decoder::new(config(false)).unwrap();
    let expected = tuples(control.decode_window_with_ap(&samples, &context()).unwrap());
    assert!(expected.iter().any(|m| m.0 == "K1ABC W1AW -12"));
    for (enabled, attach) in [(false, true), (true, false)] {
        let mut d = Ft8Decoder::new(config(enabled))
            .unwrap()
            .with_dt_prior_time(at);
        if attach {
            d = d.with_dt_priors(history(at));
        }
        assert_eq!(
            expected,
            tuples(d.decode_window_with_ap(&samples, &context()).unwrap())
        );
        assert_eq!(d.timing_diagnostics().ap.attempts, 0);
    }
}
#[test]
fn exhausted_budget_and_stale_priors_cannot_launch_retries() {
    let samples = audio();
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
    let mut d = Ft8Decoder::new(config(true))
        .unwrap()
        .with_dt_priors(history(at))
        .with_dt_prior_time(at);
    d.decode_window_with_ap_scoped_partner_budgeted(
        &samples,
        &context(),
        None,
        None,
        DecodeBudget::until(Instant::now()),
    )
    .unwrap();
    assert_eq!(d.timing_diagnostics().ap.attempts, 0);
    let mut stale = Ft8Decoder::new(config(true))
        .unwrap()
        .with_dt_priors(history(at))
        .with_dt_prior_time(at + Duration::from_secs(3600));
    stale.decode_window_with_ap(&samples, &context()).unwrap();
    assert_eq!(stale.timing_diagnostics().ap.eligible_targets, 0);
}
#[test]
fn ft4_ignores_ap_timing_flag_and_combined_ft8_flags_are_compatible() {
    let samples = audio();
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
    let params = pancetta_ft8::ProtocolParams::ft4();
    let symbols = Ft8Encoder::with_protocol(params.clone())
        .encode_message_protocol("CQ W1ABC FN42", None)
        .unwrap();
    let mut ft4_audio =
        Ft8Modulator::with_pulse_shape(12000, 1000., 1., pancetta_ft8::PulseShape::Rectangular)
            .unwrap()
            .modulate_symbols_protocol(&symbols, 0., &params)
            .unwrap();
    ft4_audio.resize(90000, 0.);
    let mut expected = None;
    for enabled in [false, true] {
        let cfg = Ft8Config {
            protocol: pancetta_ft8::Protocol::Ft4,
            ..config(enabled)
        };
        let mut d = Ft8Decoder::new(cfg)
            .unwrap()
            .with_dt_priors(history(at))
            .with_dt_prior_time(at);
        let result = tuples(d.decode_window_with_ap(&ft4_audio, &context()).unwrap());
        assert!(result.iter().any(|m| m.0 == "CQ W1ABC FN42"));
        if let Some(ref old) = expected {
            assert_eq!(old, &result);
        } else {
            expected = Some(result);
        }
        assert_eq!(d.timing_diagnostics().ap.attempts, 0);
    }
    let mut d = Ft8Decoder::new(Ft8Config {
        dt_history_enabled: true,
        ..config(true)
    })
    .unwrap()
    .with_dt_priors(history(at))
    .with_dt_prior_time(at);
    let messages = d.decode_window_with_ap(&samples, &context()).unwrap();
    assert!(messages.iter().any(|m| m.text == "K1ABC W1AW -12"));
    assert!(d.timing_diagnostics().ap.attempts <= 32);
}
#[test]
fn default_off_matches_committed_fixture_with_attached_history() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/wav/basicft8/170923_082000.wav"
    );
    let samples: Vec<f32> = hound::WavReader::open(path)
        .unwrap()
        .samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.)
        .collect();
    let at = SystemTime::now();
    let mut a = Ft8Decoder::new(config(false)).unwrap();
    let mut b = Ft8Decoder::new(config(false))
        .unwrap()
        .with_dt_priors(history(at))
        .with_dt_prior_time(at);
    assert_eq!(
        tuples(a.decode_window_with_ap(&samples, &context()).unwrap()),
        tuples(b.decode_window_with_ap(&samples, &context()).unwrap())
    );
}

#[test]
fn v2_diagnostics_distinguish_unused_empty_and_hit_lookups() {
    use pancetta_ft8::{DtPrior, DtPriorLookup};
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Counting {
        queries: AtomicU64,
        populated: bool,
    }
    impl DtPriorLookup for Counting {
        fn prior(&self, _: &str) -> Option<DtPrior> {
            None
        }
        fn priors_near_freq(&self, _: f64, _: f64) -> Vec<DtPrior> {
            self.queries.fetch_add(1, Ordering::Relaxed);
            if self.populated {
                vec![DtPrior {
                    median_dt: 0.36,
                    iqr: 10.,
                    sighting_count: 2,
                }]
            } else {
                vec![]
            }
        }
    }
    let samples = audio();
    let mut outputs = Vec::new();
    for (enabled, populated, observe) in [
        (false, true, false),
        (true, false, false),
        (true, true, false),
        (false, true, true),
    ] {
        let h = Arc::new(Counting {
            queries: AtomicU64::new(0),
            populated,
        });
        let mut d = Ft8Decoder::new(Ft8Config {
            dt_history_enabled: enabled,
            ..config(false)
        })
        .unwrap()
        .with_dt_priors(h.clone());
        if observe {
            d = d.with_dt_history_diagnostics();
        }
        outputs.push(tuples(
            d.decode_window_with_ap(&samples, &context()).unwrap(),
        ));
        let diag = d.timing_diagnostics();
        assert_eq!(diag.v2_queries, h.queries.load(Ordering::Relaxed));
        if enabled || observe {
            assert!(diag.v2_queries > 0);
            if populated {
                assert_eq!(diag.v2_queries, diag.v2_hits);
            } else {
                assert_eq!(diag.v2_hits, 0);
                assert_eq!(diag.v2_queries, diag.v2_without_prior);
            }
        } else {
            assert_eq!(diag.v2_queries, 0);
        }
    }
    // Empty priors and all-containing windows cannot alter the decisions.
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(outputs[1], outputs[2]);
    assert_eq!(outputs[2], outputs[3]);
}

#[test]
fn single_and_multiple_workers_use_the_same_timing_queue() {
    let samples = audio();
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
    let mut outcomes = Vec::new();
    for workers in [1, 4] {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()
            .unwrap();
        outcomes.push(pool.install(|| {
            let mut d = Ft8Decoder::new(config(true))
                .unwrap()
                .with_dt_priors(history(at))
                .with_dt_prior_time(at);
            let messages = tuples(d.decode_window_with_ap(&samples, &context()).unwrap());
            let diag = d.timing_diagnostics();
            (messages, diag.ap.attempts, diag.ap.searched_rows)
        }));
    }
    assert_eq!(outcomes[0], outcomes[1]);
}
