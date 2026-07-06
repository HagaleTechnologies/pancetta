//! Profiling harness for measuring decoder performance.
//!
//! Decodes a representative 12.64-second window repeatedly and reports per-window
//! wall time, with support for ablation studies via environment variables.
//!
//! Usage:
//!   cargo run --release -p pancetta-ft8 --example profile_decode -- [native|native-fresh|ft8lib] [iters]
//!
//! See `README.md` Profiling section for details.

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("native");
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(50);

    let path = format!(
        "{}/tests/fixtures/wav/wsjt/210703_133430.wav",
        env!("CARGO_MANIFEST_DIR")
    );
    let reader = hound::WavReader::open(&path).expect("open wav");
    let spec = reader.spec();
    assert_eq!(spec.channels, 1);
    let mut samples: Vec<f32> = reader
        .into_samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0)
        .collect();
    // Match production: decode exactly one WINDOW_SAMPLES window
    samples.resize(pancetta_ft8::WINDOW_SAMPLES, 0.0);
    eprintln!("mode={mode} iters={iters} samples={}", samples.len());

    match mode {
        "ft8lib" => {
            // warmup
            let msgs = pancetta_ft8::Ft8Decoder::decode_window_ft8lib(&samples);
            eprintln!("ft8lib decoded {} messages", msgs.len());
            let w0 = Instant::now();
            let mut total = 0usize;
            for _ in 0..iters {
                total += pancetta_ft8::Ft8Decoder::decode_window_ft8lib(&samples).len();
            }
            let wall = w0.elapsed().as_secs_f64();
            eprintln!(
                "ft8lib: {:.2} ms/window wall, {} msgs total",
                wall * 1e3 / iters as f64,
                total
            );
        }
        "native-fresh" => {
            // fresh decoder per window: no cross-window state accumulation
            let mut total = 0usize;
            let w0 = Instant::now();
            for i in 0..iters {
                let t = Instant::now();
                let mut decoder =
                    pancetta_ft8::Ft8Decoder::new(pancetta_ft8::Ft8Config::default()).unwrap();
                let n = decoder.decode_window(&samples).unwrap_or_default().len();
                total += n;
                eprintln!(
                    "  iter {i}: {:.1} ms, {n} msgs",
                    t.elapsed().as_secs_f64() * 1e3
                );
            }
            let wall = w0.elapsed().as_secs_f64();
            eprintln!(
                "native-fresh: {:.2} ms/window wall, {} msgs total",
                wall * 1e3 / iters as f64,
                total
            );
        }
        _ => {
            let mut config = pancetta_ft8::Ft8Config::default();
            // Ablation overrides via env vars (evidence-gathering only)
            let env_usize = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<usize>().ok());
            let env_flag = |k: &str| std::env::var(k).ok().map(|v| v == "1");
            if let Some(v) = env_usize("ABL_LDPC_ITERS") {
                config.ldpc_iterations = v;
            }
            if let Some(v) = env_usize("ABL_SYNC_CANDS") {
                config.max_sync_candidates = v;
            }
            if let Some(v) = env_usize("ABL_MULTIPASS") {
                config.coherent_multipass_iterations = v as u8;
            }
            if let Some(v) = env_flag("ABL_CROSS_CYCLE") {
                config.cross_cycle_averaging = v;
                config.cross_cycle_coherent = v;
            }
            if let Some(v) = env_flag("ABL_JOINT_PAIR") {
                config.joint_pair_retry = v;
            }
            let mut decoder = pancetta_ft8::Ft8Decoder::new(config).unwrap();
            // warmup
            let msgs = decoder.decode_window(&samples).unwrap_or_default();
            eprintln!("native decoded {} messages", msgs.len());
            let w0 = Instant::now();
            let mut total = 0usize;
            for i in 0..iters {
                let t = Instant::now();
                let n = decoder.decode_window(&samples).unwrap_or_default().len();
                total += n;
                eprintln!(
                    "  iter {i}: {:.1} ms, {n} msgs",
                    t.elapsed().as_secs_f64() * 1e3
                );
            }
            let wall = w0.elapsed().as_secs_f64();
            eprintln!(
                "native: {:.2} ms/window wall, {} msgs total",
                wall * 1e3 / iters as f64,
                total
            );
        }
    }
}
