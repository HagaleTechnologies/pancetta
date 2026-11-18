use super::*;

fn assert_db(baseline: &[f32], adjusted: &[f32], expected_db: f32) {
    assert_eq!(baseline.len(), adjusted.len());
    let baseline_power: f64 = baseline.iter().map(|s| f64::from(*s).powi(2)).sum();
    let adjusted_power: f64 = adjusted.iter().map(|s| f64::from(*s).powi(2)).sum();
    let measured = 10.0 * (adjusted_power / baseline_power).log10();
    assert!(
        (measured - f64::from(expected_db)).abs() < 0.02,
        "{measured} dB"
    );
    let gain = 10.0_f32.powf(expected_db / 20.0);
    for (before, after) in baseline.iter().zip(adjusted) {
        assert!((before * gain - after).abs() < 0.00001);
    }
}

#[test]
fn drive_down_changes_only_the_next_ft8_buffer() {
    let drive = TxDrive::new(0.0, 0.0);
    let baseline = [0.0, 0.25, -0.5, 0.5];
    let mut queued = baseline;
    let first_key_db = drive.get();
    apply(&mut queued, first_key_db, drive.ceiling_db(), false);
    assert_eq!(queued, baseline);
    drive.adjust(-1);
    assert_eq!(queued, baseline);
    let mut next = baseline;
    apply(&mut next, drive.get(), drive.ceiling_db(), false);
    assert_db(&baseline, &next, -1.0);
}

#[test]
fn drive_limits_preserve_default_buffers_and_positive_headroom() {
    let drive = TxDrive::new(0.0, 0.0);
    assert_eq!(drive.adjust(1), 0.0);
    let original = vec![0.0, 0.5, -0.5, 0.95];
    let mut unchanged = original.clone();
    apply(&mut unchanged, drive.get(), drive.ceiling_db(), true);
    assert_eq!(unchanged, original);
    for _ in 0..40 {
        drive.adjust(-1);
    }
    assert_eq!(drive.get(), -20.0);
    assert_eq!(drive.adjust(-1), -20.0);
    let boosted = TxDrive::new(5.0, 5.0);
    for (baseline, bundle) in [(0.5, false), (0.95, true)] {
        let mut output = [baseline, -baseline];
        apply(&mut output, boosted.get(), boosted.ceiling_db(), bundle);
        assert!(output.iter().all(|s| s.is_finite()));
        assert!(output
            .iter()
            .all(|s| s.abs() <= 10.0_f32.powf(-1.0 / 20.0) + 0.00001));
    }
}

#[test]
fn configured_minus_six_scales_single_bundle_and_rekey() {
    let drive = TxDrive::new(-6.0, 0.0);
    let baseline = vec![0.0, 0.1, 0.5, -0.5, -0.2];
    for bundle in [false, true] {
        let mut first = baseline.clone();
        apply(&mut first, drive.get(), drive.ceiling_db(), bundle);
        assert_db(&baseline, &first, -6.0);
        drive.adjust(1);
        let mut rekey = baseline.clone();
        apply(&mut rekey, drive.get(), drive.ceiling_db(), bundle);
        assert_db(&baseline, &rekey, -5.0);
        drive.adjust(-1);
    }
}

#[test]
fn tune_uses_the_same_drive() {
    let baseline: Vec<f32> = (0..12000)
        .map(|i| ((i as f64) * 2.0 * std::f64::consts::PI * 1500.0 / 12000.0).sin() as f32 * 0.5)
        .collect();
    let mut output = baseline.clone();
    apply(&mut output, -3.0, 0.0, false);
    assert_db(&baseline, &output, -3.0);
}

#[test]
fn saved_drive_is_used_by_the_first_buffer_after_restart() {
    let mut config = pancetta_config::Config::default();
    config.audio.levels.output_gain_db = -4.0;
    let fresh = TxDrive::from_config(&config);
    let mut output = [0.5, -0.5];
    apply(&mut output, fresh.get(), fresh.ceiling_db(), false);
    assert_db(&[0.5, -0.5], &output, -4.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_drive_scales_real_worker_audio() {
    use crate::coordinator::tx::pan115_worker_gate_tests::{request, worker_harness_with_config};
    use crate::message_bus::{ComponentId, MessageType};
    use std::time::Duration;

    async fn capture(db: f32) -> Vec<f32> {
        let mut config = pancetta_config::Config::default();
        config.audio.levels.output_gain_db = db;
        let harness = worker_harness_with_config(config).await;
        let (_, receiver) = harness
            .coordinator
            .message_bus
            .get_or_create_channel(ComponentId::Audio)
            .await
            .unwrap();
        request(&harness, "CQ K5ARH EM10").await;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Ok(message) = receiver.try_recv() {
                    if let MessageType::AudioOutput { samples, .. } = message.message_type {
                        return samples;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker emitted TX audio")
    }

    let (baseline, driven) = tokio::join!(capture(0.0), capture(-6.0));
    let peak = |samples: &[f32]| {
        samples
            .iter()
            .map(|sample| sample.abs())
            .fold(0.0_f32, f32::max)
    };
    let ratio_db = 20.0 * (peak(&driven) / peak(&baseline)).log10();
    assert!(
        (ratio_db + 6.0).abs() < 0.02,
        "worker audio delta {ratio_db} dB"
    );
}
