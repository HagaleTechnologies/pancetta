//! Decode one FT8/FT4 cycle from a WAV file — the manta-facing usage
//! example (PAN-176).
//!
//! Usage:
//!
//! ```text
//! cargo run -p pancetta-ft8 --example decode_cycle -- <12 kHz mono WAV> [--ft4]
//! ```
//!
//! The WAV must be 12 kHz mono 16-bit; FT8 cycles are 15 s (180 000 frames),
//! FT4 cycles 7.5 s (90 000 frames), with frame 0 at the slot boundary.
//! `hound` is a dev-dependency of this crate, so this example never enters
//! the normal dependency graph of a consumer.

use pancetta_ft8::{decode_cycle, Protocol, SAMPLE_RATE};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or_else(|| {
            format!(
                "usage: decode_cycle <12 kHz mono WAV> [--ft4]\n\
                 example: cargo run -p pancetta-ft8 --example decode_cycle -- \
                 pancetta-ft8/tests/fixtures/wav/jtdx/190227_155815.wav"
            )
        })?
        .clone();
    let protocol = if args.iter().any(|a| a == "--ft4") {
        Protocol::Ft4
    } else {
        Protocol::Ft8
    };

    let reader = hound::WavReader::open(&path)?;
    let spec = reader.spec();
    if spec.channels != 1 || spec.sample_rate != SAMPLE_RATE {
        return Err(format!(
            "{path}: expected mono {SAMPLE_RATE} Hz WAV, got {} channels at {} Hz",
            spec.channels, spec.sample_rate
        )
        .into());
    }
    let samples: Vec<f32> = reader
        .into_samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0)
        .collect();
    eprintln!(
        "decoding {} samples ({:.1} s) as {:?}",
        samples.len(),
        samples.len() as f64 / SAMPLE_RATE as f64,
        protocol
    );

    for m in decode_cycle(&samples, protocol)? {
        println!(
            "[{:>5.1} dB] {} ({} Hz, dt {:.2}s)",
            m.snr_db, m.text, m.frequency_offset, m.time_offset
        );
    }
    Ok(())
}
