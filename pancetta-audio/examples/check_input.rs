//! Quick audio input diagnostic: capture 3 seconds from default input,
//! report RMS and peak levels to verify the device is delivering signal.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn main() {
    let host = cpal::default_host();

    // List all input devices
    println!("=== Input Devices ===");
    if let Ok(devices) = host.input_devices() {
        for d in devices {
            let name = d.name().unwrap_or_else(|_| "??".into());
            let is_default = host
                .default_input_device()
                .map(|dd| dd.name().ok() == d.name().ok())
                .unwrap_or(false);
            println!(
                "  {} {}",
                name,
                if is_default { " <-- DEFAULT" } else { "" }
            );
        }
    }

    let device = host
        .default_input_device()
        .expect("No default input device");
    let name = device.name().unwrap_or_else(|_| "unknown".into());
    println!("\nUsing: {}", name);

    let config = cpal::StreamConfig {
        channels: 2,
        sample_rate: cpal::SampleRate(48000),
        buffer_size: cpal::BufferSize::Default,
    };

    let samples: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let samples_clone = samples.clone();
    let start = Instant::now();

    let stream = device
        .build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if start.elapsed() < Duration::from_secs(3) {
                    samples_clone.lock().unwrap().extend_from_slice(data);
                }
            },
            |err| eprintln!("Stream error: {}", err),
            None,
        )
        .expect("Failed to build input stream");

    stream.play().expect("Failed to start stream");
    println!("Capturing 3 seconds...");
    std::thread::sleep(Duration::from_secs(4));
    drop(stream);

    let data = samples.lock().unwrap();
    let n = data.len();
    if n == 0 {
        println!("ERROR: No samples captured!");
        return;
    }

    let rms = (data.iter().map(|s| s * s).sum::<f32>() / n as f32).sqrt();
    let peak = data.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
    let nonzero = data.iter().filter(|&&s| s != 0.0).count();

    println!("\n=== Results ===");
    println!("Samples: {} ({:.2}s stereo)", n, n as f64 / (48000.0 * 2.0));
    println!(
        "Non-zero: {} ({:.1}%)",
        nonzero,
        nonzero as f64 / n as f64 * 100.0
    );
    println!(
        "RMS: {:.6} ({:.1} dBFS)",
        rms,
        if rms > 0.0 {
            20.0 * rms.log10()
        } else {
            -999.0
        }
    );
    println!(
        "Peak: {:.6} ({:.1} dBFS)",
        peak,
        if peak > 0.0 {
            20.0 * peak.log10()
        } else {
            -999.0
        }
    );

    // Per-channel
    let left: Vec<f32> = data.iter().step_by(2).copied().collect();
    let right: Vec<f32> = data.iter().skip(1).step_by(2).copied().collect();
    let l_rms = (left.iter().map(|s| s * s).sum::<f32>() / left.len() as f32).sqrt();
    let r_rms = (right.iter().map(|s| s * s).sum::<f32>() / right.len() as f32).sqrt();
    let l_nz = left.iter().filter(|&&s| s != 0.0).count();
    let r_nz = right.iter().filter(|&&s| s != 0.0).count();
    println!("\nL ch: RMS={:.6} nonzero={}", l_rms, l_nz);
    println!("R ch: RMS={:.6} nonzero={}", r_rms, r_nz);

    // First 20 samples
    println!("\nFirst 20 samples: {:?}", &data[..20.min(n)]);
}
