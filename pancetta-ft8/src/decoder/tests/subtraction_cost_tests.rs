use super::*;
use crate::WINDOW_SAMPLES;
#[cfg(not(debug_assertions))]
use std::time::Instant;

fn decoder() -> Ft8Decoder {
    Ft8Decoder::new(Ft8Config::default()).unwrap()
}

#[test]
fn hilbert_with_cached_plans_is_bit_identical_to_fresh_planner() {
    let decoder = decoder();
    for n in [151_680, 101, 103] {
        let signal: Vec<f64> = (0..n).map(|i| (i as f64 * 0.01).sin()).collect();
        let old = Ft8Decoder::hilbert_quadrature(&signal);
        let cached = Ft8Decoder::hilbert_quadrature_with_plans(
            &signal,
            &decoder.hilbert_fft,
            &decoder.hilbert_ifft,
        );
        assert!(old
            .iter()
            .zip(cached)
            .all(|(a, b)| a.to_bits() == b.to_bits()));
    }
}

#[test]
fn fast_gfsk_reference_matches_modulator_reference() {
    let decoder = decoder();
    let pp = ProtocolParams::ft8();
    for frequency in [200.0, 1000.0, 2900.0] {
        for seed in [0, 1, 2] {
            let symbols: Vec<u8> = (0..79).map(|i| ((i * 3 + seed) % 8) as u8).collect();
            let slow =
                Ft8Decoder::generate_gfsk_reference(&symbols, &pp, SAMPLE_RATE, frequency, 0.0);
            let fast =
                decoder.generate_gfsk_reference_fast(&symbols, &pp, SAMPLE_RATE, frequency, 0.0);
            assert_eq!(slow.is_err(), fast.is_err());
            if let (Ok(slow), Ok(fast)) = (slow, fast) {
                let max = slow
                    .samples
                    .iter()
                    .zip(fast.samples)
                    .map(|(a, b)| (a - b).abs())
                    .fold(0.0_f32, f32::max);
                assert!(max <= 1e-6, "frequency={frequency} max={max}");
            }
        }
    }
}

#[test]
fn fast_gfsk_trajectory_matches_convolution() {
    let decoder = decoder();
    let pp = ProtocolParams::ft8();
    let symbols: Vec<u8> = (0..79).map(|i| ((i * 3) % 8) as u8).collect();
    let modulator = crate::Ft8Modulator::with_pulse_shape(
        SAMPLE_RATE,
        1000.0,
        0.5,
        crate::PulseShape::Gaussian {
            bt: crate::modulator::DEFAULT_BT,
        },
    )
    .unwrap();
    let expected = modulator.build_frequency_trajectory_generic(
        &symbols,
        1000.0,
        pp.tone_spacing,
        pp.samples_per_symbol(SAMPLE_RATE),
    );
    let actual = decoder.fast_gfsk_trajectory(&symbols, &pp, SAMPLE_RATE, 1000.0);
    let max = expected
        .iter()
        .zip(actual)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0_f64, f64::max);
    assert!(max <= 1e-9, "max trajectory difference {max}");
}

#[test]
fn rotated_iq_pair_matches_direct_synthesis() {
    let symbols: Vec<u8> = (0..79).map(|i| ((i * 3) % 8) as u8).collect();
    for ramp in [false, true] {
        let generate = |frequency| {
            if ramp {
                Ft8Decoder::generate_cpfsk_iq_ramped(&symbols, frequency, 1920, 120)
            } else {
                Ft8Decoder::generate_cpfsk_iq(&symbols, frequency, 1920, TONE_SPACING)
            }
        };
        let nominal = generate(1000.0);
        for delta in [-6.25, -1.5, -1.0, -0.5, 0.5, 1.0, 1.5, 6.25] {
            let rotated = Ft8Decoder::rotate_iq_pair(&nominal.0, &nominal.1, delta, SAMPLE_RATE);
            let direct = generate(1000.0 + delta);
            let max = rotated
                .0
                .iter()
                .zip(direct.0.iter())
                .chain(rotated.1.iter().zip(direct.1.iter()))
                .map(|(a, b)| (a - b).abs())
                .fold(0.0_f64, f64::max);
            assert!(max <= 1e-6, "ramp={ramp} delta={delta} max={max}");
        }
    }
}

#[test]
fn search_grid_picks_the_same_best_on_fixtures() {
    let mut decoder = decoder();
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/wav/wsjt/210703_133430.wav"
    );
    let reader = hound::WavReader::open(path).unwrap();
    let mut audio: Vec<f32> = reader
        .into_samples::<i16>()
        .map(|x| x.unwrap() as f32 / 32768.0)
        .collect();
    audio.resize(WINDOW_SAMPLES, 0.0);
    for message in decoder.decode_window(&audio).unwrap() {
        let Some(symbols) = message.tone_symbols else {
            continue;
        };
        let nominal =
            Ft8Decoder::generate_cpfsk_iq(&symbols, message.frequency_offset, 1920, TONE_SPACING);
        let nominal_time = (message.time_offset * SAMPLE_RATE as f64) as isize;
        let mut old_best = (0.0, 0, 0);
        let mut new_best = (0.0, 0, 0);
        for di in -3..=3 {
            let freq = message.frequency_offset + di as f64 * 0.5;
            let old = Ft8Decoder::generate_cpfsk_iq(&symbols, freq, 1920, TONE_SPACING);
            let new =
                Ft8Decoder::rotate_iq_pair(&nominal.0, &nominal.1, di as f64 * 0.5, SAMPLE_RATE);
            let prefix = Ft8Decoder::reference_prefix(&new.0, &new.1);
            for dt in -4..=4 {
                let try_time = nominal_time + dt as isize * 120;
                let start = try_time.max(0) as usize;
                let offset = (start as isize - try_time) as usize;
                let len = (symbols.len() * 1920 - offset).min(audio.len().saturating_sub(start));
                if len == 0 {
                    continue;
                }
                let old_energy =
                    Ft8Decoder::correlation_energy(&audio, start, &old.0, &old.1, offset, len);
                let new_energy = Ft8Decoder::correlation_energy_prefix(
                    &audio, start, &new.0, &new.1, offset, len, &prefix,
                );
                if old_energy > old_best.0 {
                    old_best = (old_energy, di, dt);
                }
                if new_energy > new_best.0 {
                    new_best = (new_energy, di, dt);
                }
            }
        }
        assert_eq!(
            (old_best.1, old_best.2),
            (new_best.1, new_best.2),
            "{}",
            message.text
        );
    }
}

#[cfg(not(debug_assertions))]
#[test]
fn subtract_with_sidelobes_max_shape_cost_ceiling() {
    let mut decoder = Ft8Decoder::new(Ft8Config {
        max_decode_passes: 2,
        time_varying_subtraction_enabled: true,
        ..Ft8Config::default()
    })
    .unwrap();
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/wav/wsjt/210703_133430.wav"
    );
    let reader = hound::WavReader::open(path).unwrap();
    let mut audio: Vec<f32> = reader
        .into_samples::<i16>()
        .map(|x| x.unwrap() as f32 / 32768.0)
        .collect();
    audio.resize(WINDOW_SAMPLES, 0.0);
    let messages = decoder.decode_window(&audio).unwrap();
    let msg = messages.first().expect("fixture decode");
    let mut times = Vec::new();
    for _ in 0..5 {
        let mut copy = audio.clone();
        let start = Instant::now();
        decoder.subtract_with_sidelobes(&mut copy, msg);
        times.push(start.elapsed().as_millis());
    }
    times.sort();
    eprintln!("PAN-370 median subtraction: {} ms", times[2]);
    assert!(times[2] <= 50, "median subtraction {} ms", times[2]);
}
