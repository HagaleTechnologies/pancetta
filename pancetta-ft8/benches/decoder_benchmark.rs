//! Performance benchmarks for the FT8 decoder
//!
//! These benchmarks measure the decoder performance under various conditions
//! to ensure it meets real-time requirements.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use pancetta_ft8::{
    Ft8Config, Ft8Decoder, NUM_SYMBOLS, SAMPLE_RATE, SYMBOL_DURATION, WINDOW_SAMPLES,
};
use std::f64::consts::PI;

/// Generate synthetic FT8 signal for benchmarking
fn generate_benchmark_signal(num_signals: usize, base_snr: f32, frequency_spread: f64) -> Vec<f32> {
    let mut samples = vec![0.0f32; WINDOW_SAMPLES];

    // Add noise floor
    for sample in &mut samples {
        *sample = (rand::random::<f32>() - 0.5) * 0.02;
    }

    // Add multiple FT8-like signals
    for signal_idx in 0..num_signals {
        let snr = base_snr - signal_idx as f32 * 2.0; // Decreasing SNR
        let freq_offset = (signal_idx as f64 - num_signals as f64 / 2.0) * frequency_spread;
        let time_offset = (signal_idx as f64 * 0.1) % 1.0; // Staggered timing

        add_ft8_signal(&mut samples, snr, freq_offset, time_offset);
    }

    samples
}

/// Add a single FT8-like signal to the samples
fn add_ft8_signal(samples: &mut [f32], snr_db: f32, frequency_offset: f64, time_offset: f64) {
    let signal_power = 10.0_f64.powf(snr_db as f64 / 10.0);
    let amplitude = signal_power.sqrt() * 0.1;

    let base_freq = 1500.0 + frequency_offset;
    let tone_spacing = 6.25;
    let symbol_samples = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize;
    let start_sample = (time_offset * SAMPLE_RATE as f64) as usize;

    // Generate pseudo-random tone sequence
    let mut tone_seed = (frequency_offset * 1000.0) as u64;

    for symbol_idx in 0..NUM_SYMBOLS {
        // Simple PRNG for tone selection
        tone_seed = tone_seed.wrapping_mul(1103515245).wrapping_add(12345);
        let tone = (tone_seed >> 8) % 8;

        let tone_freq = base_freq + tone as f64 * tone_spacing;
        let symbol_start = start_sample + symbol_idx * symbol_samples;
        let symbol_end = (symbol_start + symbol_samples).min(samples.len());

        for i in symbol_start..symbol_end {
            let t = i as f64 / SAMPLE_RATE as f64;
            let phase = 2.0 * PI * tone_freq * t;
            samples[i] += amplitude as f32 * phase.cos() as f32;
        }
    }
}

/// Benchmark basic decoding performance
fn benchmark_basic_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("basic_decode");

    let config = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(config).unwrap();

    // Test with different signal strengths
    let snr_levels = [-20.0, -15.0, -10.0, -5.0];

    for snr in snr_levels {
        let samples = generate_benchmark_signal(1, snr, 0.0);

        group.bench_with_input(
            BenchmarkId::new("single_signal", format!("{}_dB", snr)),
            &samples,
            |b, samples| {
                b.iter(|| {
                    black_box(decoder.decode_window(black_box(samples)).unwrap());
                });
            },
        );
    }

    group.finish();
}

/// Benchmark decoding with multiple signals
fn benchmark_multiple_signals(c: &mut Criterion) {
    let mut group = c.benchmark_group("multiple_signals");

    let config = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(config).unwrap();

    // Test with different numbers of signals
    let signal_counts = [1, 5, 10, 20, 50];

    for count in signal_counts {
        let samples = generate_benchmark_signal(count, -10.0, 50.0);

        group.bench_with_input(
            BenchmarkId::new("signals", count),
            &samples,
            |b, samples| {
                b.iter(|| {
                    black_box(decoder.decode_window(black_box(samples)).unwrap());
                });
            },
        );
    }

    group.finish();
}

/// Benchmark with different configuration options
fn benchmark_configuration_variants(c: &mut Criterion) {
    let mut group = c.benchmark_group("configuration");

    let samples = generate_benchmark_signal(10, -12.0, 30.0);

    // Default configuration
    let default_config = Ft8Config::default();
    let mut default_decoder = Ft8Decoder::new(default_config).unwrap();

    group.bench_function("default", |b| {
        b.iter(|| {
            black_box(default_decoder.decode_window(black_box(&samples)).unwrap());
        });
    });

    // Single-threaded configuration
    let mut single_thread_config = Ft8Config::default();
    single_thread_config.enable_multithreading = false;
    let mut single_decoder = Ft8Decoder::new(single_thread_config).unwrap();

    group.bench_function("single_thread", |b| {
        b.iter(|| {
            black_box(single_decoder.decode_window(black_box(&samples)).unwrap());
        });
    });

    // Aggressive decoding configuration
    let mut aggressive_config = Ft8Config::default();
    aggressive_config.aggressive_decoding = true;
    aggressive_config.max_candidates = 100;
    aggressive_config.min_snr_db = -25.0;
    let mut aggressive_decoder = Ft8Decoder::new(aggressive_config).unwrap();

    group.bench_function("aggressive", |b| {
        b.iter(|| {
            black_box(
                aggressive_decoder
                    .decode_window(black_box(&samples))
                    .unwrap(),
            );
        });
    });

    // Minimal configuration
    let mut minimal_config = Ft8Config::default();
    minimal_config.max_candidates = 10;
    minimal_config.min_snr_db = -10.0;
    minimal_config.ldpc_iterations = 25;
    let mut minimal_decoder = Ft8Decoder::new(minimal_config).unwrap();

    group.bench_function("minimal", |b| {
        b.iter(|| {
            black_box(minimal_decoder.decode_window(black_box(&samples)).unwrap());
        });
    });

    group.finish();
}

/// Benchmark signal processing components
fn benchmark_signal_processing(c: &mut Criterion) {
    use pancetta_ft8::signal_processing::{BandpassFilter, FftProcessor, WindowFunction};

    let mut group = c.benchmark_group("signal_processing");

    // FFT performance
    let mut fft_processor = FftProcessor::new(4096, WindowFunction::Hann).unwrap();
    let test_data: Vec<f64> = (0..4096).map(|i| (i as f64 / 100.0).sin()).collect();

    group.bench_function("fft_4096", |b| {
        b.iter(|| {
            black_box(fft_processor.fft_real(black_box(&test_data)).unwrap());
        });
    });

    // PSD computation
    group.bench_function("psd_4096", |b| {
        b.iter(|| {
            black_box(
                fft_processor
                    .power_spectral_density(black_box(&test_data))
                    .unwrap(),
            );
        });
    });

    // Bandpass filter performance
    let mut filter = BandpassFilter::new(1500.0, 400.0, 65).unwrap();
    let filter_test_data: Vec<f64> = (0..1000).map(|i| (i as f64 / 10.0).sin()).collect();
    let mut output = vec![0.0; filter_test_data.len()];

    group.bench_function("bandpass_filter", |b| {
        b.iter(|| {
            black_box(
                filter
                    .filter_batch(black_box(&filter_test_data), black_box(&mut output))
                    .unwrap(),
            );
        });
    });

    group.finish();
}

/// Benchmark time synchronization
fn benchmark_time_sync(c: &mut Criterion) {
    use pancetta_ft8::sync::TimeSync;

    let mut group = c.benchmark_group("time_sync");

    let mut time_sync = TimeSync::new().unwrap();
    let audio_data: Vec<f64> = (0..WINDOW_SAMPLES)
        .map(|i| (i as f64 / 1000.0).sin())
        .collect();
    let audio_f32: Vec<f32> = audio_data.iter().map(|&x| x as f32).collect();

    group.bench_function("synchronization", |b| {
        b.iter(|| {
            black_box(
                time_sync
                    .synchronize(
                        black_box(&audio_f32),
                        black_box(std::time::SystemTime::now()),
                    )
                    .unwrap(),
            );
        });
    });

    group.finish();
}

/// Benchmark memory allocation patterns
fn benchmark_memory_usage(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory");

    let config = Ft8Config::default();

    // Test decoder creation overhead
    group.bench_function("decoder_creation", |b| {
        b.iter(|| {
            black_box(Ft8Decoder::new(black_box(config.clone())).unwrap());
        });
    });

    // Test with varying complexity
    let samples_simple = vec![0.0f32; WINDOW_SAMPLES]; // Silent
    let samples_complex = generate_benchmark_signal(25, -15.0, 40.0);

    let mut decoder = Ft8Decoder::new(config).unwrap();

    group.bench_function("decode_silent", |b| {
        b.iter(|| {
            black_box(decoder.decode_window(black_box(&samples_simple)).unwrap());
        });
    });

    group.bench_function("decode_complex", |b| {
        b.iter(|| {
            black_box(decoder.decode_window(black_box(&samples_complex)).unwrap());
        });
    });

    group.finish();
}

/// Benchmark real-time performance scenarios
fn benchmark_realtime_scenarios(c: &mut Criterion) {
    let mut group = c.benchmark_group("realtime");

    let config = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(config).unwrap();

    // Typical amateur radio conditions
    let weak_signals = generate_benchmark_signal(3, -18.0, 75.0);
    let strong_signals = generate_benchmark_signal(8, -8.0, 50.0);
    let contest_conditions = generate_benchmark_signal(15, -12.0, 25.0);

    group.bench_function("weak_signals", |b| {
        b.iter(|| {
            black_box(decoder.decode_window(black_box(&weak_signals)).unwrap());
        });
    });

    group.bench_function("strong_signals", |b| {
        b.iter(|| {
            black_box(decoder.decode_window(black_box(&strong_signals)).unwrap());
        });
    });

    group.bench_function("contest_conditions", |b| {
        b.iter(|| {
            black_box(
                decoder
                    .decode_window(black_box(&contest_conditions))
                    .unwrap(),
            );
        });
    });

    group.finish();
}

/// Benchmark throughput (messages decoded per second)
fn benchmark_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput");
    group.sample_size(10); // Fewer samples for longer benchmarks

    let config = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(config).unwrap();

    // Generate multiple windows of data
    let num_windows = 10;
    let mut windows = Vec::new();

    for i in 0..num_windows {
        let signals_per_window = 5 + (i % 3); // Varying load
        let base_snr = -15.0 + (i as f32 * 2.0); // Varying SNR
        windows.push(generate_benchmark_signal(
            signals_per_window,
            base_snr,
            60.0,
        ));
    }

    group.bench_function("sustained_throughput", |b| {
        b.iter(|| {
            for window in &windows {
                black_box(decoder.decode_window(black_box(window)).unwrap());
            }
        });
    });

    group.finish();
}

/// Custom measurement for real-time factor
fn benchmark_realtime_factor(c: &mut Criterion) {
    use criterion::measurement::WallTime;
    use std::time::{Duration, Instant};

    let mut group = c.benchmark_group("realtime_factor");

    let config = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(config).unwrap();

    let samples = generate_benchmark_signal(10, -12.0, 40.0);

    group.bench_function("realtime_factor", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();

            for _ in 0..iters {
                black_box(decoder.decode_window(black_box(&samples)).unwrap());
            }

            let processing_time = start.elapsed();
            let audio_duration = Duration::from_secs_f64(12.64 * iters as f64);

            let realtime_factor = processing_time.as_secs_f64() / audio_duration.as_secs_f64();

            println!("Real-time factor: {:.2}x", realtime_factor);

            processing_time
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    benchmark_basic_decode,
    benchmark_multiple_signals,
    benchmark_configuration_variants,
    benchmark_signal_processing,
    benchmark_time_sync,
    benchmark_memory_usage,
    benchmark_realtime_scenarios,
    benchmark_throughput,
    benchmark_realtime_factor
);

criterion_main!(benches);
