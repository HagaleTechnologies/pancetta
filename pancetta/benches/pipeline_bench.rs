//! Performance benchmarks for the audio pipeline
//!
//! Run with: cargo bench

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use std::time::Duration;

/// Benchmark FT8 decoder with various signal conditions
fn bench_ft8_decoder(c: &mut Criterion) {
    let mut group = c.benchmark_group("ft8_decoder");
    group.measurement_time(Duration::from_secs(10));

    // Create decoder
    let config = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(config).unwrap();

    // Test with different signal conditions
    for snr in [-10, 0, 10].iter() {
        let samples = generate_test_signal(*snr as f32);

        group.throughput(Throughput::Elements(samples.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("decode_window", format!("SNR_{}dB", snr)),
            &samples,
            |b, samples| {
                b.iter(|| {
                    let _ = decoder.decode_window(black_box(samples));
                });
            },
        );
    }

    group.finish();
}

/// Benchmark DSP pipeline processing
fn bench_dsp_pipeline(c: &mut Criterion) {
    use pancetta_dsp::factory;

    let mut group = c.benchmark_group("dsp_pipeline");
    group.measurement_time(Duration::from_secs(5));

    // Create pipeline
    let (mut pipeline, input_tx, output_rx) = factory::create_ft8_pipeline().unwrap();

    // Start pipeline
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let _ = pipeline.start().await;
        });
    });

    // Benchmark different buffer sizes
    for buffer_size in [256, 512, 1024, 2048].iter() {
        let samples = vec![0.1_f32; *buffer_size];

        group.throughput(Throughput::Elements(*buffer_size as u64));
        group.bench_with_input(
            BenchmarkId::new("process_buffer", buffer_size),
            &samples,
            |b, samples| {
                b.iter(|| {
                    let _ = input_tx.try_send(samples.clone());
                    // Try to receive (may timeout if pipeline is saturated)
                    let _ = output_rx.try_recv();
                });
            },
        );
    }

    group.finish();
}

/// Benchmark message bus throughput
fn bench_message_bus(c: &mut Criterion) {
    use pancetta_lib::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};
    use std::time::Instant;

    let mut group = c.benchmark_group("message_bus");

    let runtime = tokio::runtime::Runtime::new().unwrap();

    runtime.block_on(async {
        let message_bus = MessageBus::new(10000).unwrap();
        let (tx, rx) = message_bus
            .create_channel(ComponentId::Audio)
            .await
            .unwrap();

        // Benchmark different message sizes
        for size in [128, 512, 1024, 4096].iter() {
            let samples = vec![0.1_f32; *size];
            let message = ComponentMessage::new(
                ComponentId::Audio,
                ComponentId::Dsp,
                MessageType::AudioData(samples.clone()),
                Instant::now(),
            );

            group.throughput(Throughput::Elements(*size as u64));
            group.bench_with_input(
                BenchmarkId::new("send_receive", size),
                &message,
                |b, message| {
                    b.iter(|| {
                        let _ = tx.try_send(message.clone());
                        let _ = rx.try_recv();
                    });
                },
            );
        }
    });

    group.finish();
}

/// Benchmark audio buffer operations
fn bench_audio_buffer(c: &mut Criterion) {
    use pancetta_dsp::factory;

    let mut group = c.benchmark_group("audio_buffer");

    let buffer = factory::create_audio_buffer(48000.0, 1.0).unwrap();

    // Benchmark write operations
    for size in [256, 512, 1024].iter() {
        let samples = vec![0.1_f32; *size];

        group.throughput(Throughput::Elements(*size as u64));
        group.bench_with_input(BenchmarkId::new("write", size), &samples, |b, samples| {
            b.iter(|| {
                let _ = buffer.write(black_box(samples));
            });
        });
    }

    // Benchmark read operations
    for size in [256, 512, 1024].iter() {
        // Pre-fill buffer
        let samples = vec![0.1_f32; *size * 10];
        let _ = buffer.write(&samples);

        // `read` fills a caller-provided slice (returns the count read).
        let mut out = vec![0.0_f32; *size];
        group.throughput(Throughput::Elements(*size as u64));
        group.bench_function(BenchmarkId::new("read", size), |b| {
            b.iter(|| {
                let _ = buffer.read(black_box(&mut out));
            });
        });
    }

    group.finish();
}

/// Generate test signal for benchmarking
fn generate_test_signal(snr_db: f32) -> Vec<f32> {
    let mut samples = vec![0.0_f32; 151680]; // FT8 window size

    // Generate simple test signal
    let frequency = 1500.0; // FT8 center frequency
    let sample_rate = 12000.0;
    let amplitude = 0.1 * 10.0_f32.powf(snr_db / 20.0);

    for (i, sample) in samples.iter_mut().enumerate() {
        let t = i as f32 / sample_rate;
        *sample = amplitude * (2.0 * std::f32::consts::PI * frequency * t).sin();

        // Add simple noise
        if snr_db < 20.0 {
            let noise_level = 0.1 * 10.0_f32.powf(-snr_db / 20.0);
            *sample += noise_level * ((i as f32 * 0.123).sin() * 0.5);
        }
    }

    samples
}

criterion_group!(
    benches,
    bench_ft8_decoder,
    bench_dsp_pipeline,
    bench_message_bus,
    bench_audio_buffer
);
criterion_main!(benches);
