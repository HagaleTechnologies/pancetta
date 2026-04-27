//! TX hardware validation tool.
//!
//! Encodes an FT8 message, waits for slot boundary, and plays audio out the
//! default output device.  Use `--ptt` to key the radio via rigctld.
//!
//! Usage:
//!   cargo run --example tx_test
//!   cargo run --example tx_test -- --ptt
//!   cargo run --example tx_test -- --message "CQ K5ARH EM10" --freq-offset 1200

use chrono::Utc;
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use pancetta_ft8::{Ft8Encoder, Ft8Modulator, SAMPLE_RATE as FT8_SAMPLE_RATE};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// RAII guard that releases PTT on drop. Defense in depth:
/// - Drop runs on normal exit, panic unwind, or early return.
/// - SIGINT/SIGTERM are turned into a flag the main loop checks, then drops the guard.
/// - An independent watchdog thread force-releases PTT if the deadline elapses,
///   which catches main-thread hangs (e.g., CoreAudio stuck) that drop() can't.
/// - SIGKILL / abort cannot be intercepted; PTT will leak in those cases.
struct PttGuard {
    active: Arc<AtomicBool>,
}

impl PttGuard {
    fn engage(max_secs: f64) -> Result<Self, Box<dyn std::error::Error>> {
        rigctld_ptt(true)?;
        let active = Arc::new(AtomicBool::new(true));
        let active_wd = active.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs_f64(max_secs));
            if active_wd.swap(false, Ordering::SeqCst) {
                eprintln!(
                    "\nPTT WATCHDOG: max TX time {:.1}s exceeded — forcing PTT OFF",
                    max_secs
                );
                if let Err(e) = rigctld_ptt(false) {
                    eprintln!("watchdog: PTT release failed: {}", e);
                }
            }
        });
        Ok(Self { active })
    }
}

impl Drop for PttGuard {
    fn drop(&mut self) {
        // swap-to-false makes this idempotent with the watchdog: whichever
        // path reaches here first owns the release; the other becomes a no-op.
        if self.active.swap(false, Ordering::SeqCst) {
            eprintln!("PTT OFF (guard drop)");
            if let Err(e) = rigctld_ptt(false) {
                eprintln!("PTT release failed in drop: {}", e);
            }
        }
    }
}

#[derive(Parser)]
#[command(name = "tx_test", about = "FT8 TX hardware validation")]
struct Args {
    /// FT8 message to transmit
    #[arg(long, default_value = "K5ARH RR73")]
    message: String,

    /// Audio frequency offset in Hz (within FT8 passband)
    #[arg(long, default_value_t = 1500.0)]
    freq_offset: f64,

    /// TX power level (0.0 - 1.0)
    #[arg(long, default_value_t = 0.5)]
    power: f64,

    /// Output audio device name (substring match, case-insensitive)
    #[arg(long)]
    device: Option<String>,

    /// Enable PTT via rigctld (localhost:4532)
    #[arg(long)]
    ptt: bool,

    /// Skip waiting for FT8 slot boundary
    #[arg(long)]
    immediate: bool,

    /// Hard ceiling on PTT keying time (seconds). A watchdog thread forces
    /// PTT OFF when this elapses, even if the main thread is stuck. FT8
    /// transmissions are 12.64s and slots are 15s, so 14s is a safe default.
    #[arg(long, default_value_t = 14.0)]
    max_tx_secs: f64,
}

fn main() {
    let args = Args::parse();

    println!("FT8 TX Test");
    println!("===========");
    println!("Message:     \"{}\"", args.message);
    println!("Freq offset: {} Hz", args.freq_offset);
    println!("Power:       {:.1}%", args.power * 100.0);
    println!(
        "PTT:         {}",
        if args.ptt { "ENABLED" } else { "disabled" }
    );
    println!();

    // --- Encode ---
    let mut encoder = Ft8Encoder::new();
    let symbols = match encoder.encode_message(&args.message, None) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Encode error: {}", e);
            std::process::exit(1);
        }
    };
    println!("Encoded {} symbols", symbols.len());

    // --- Modulate at 12 kHz ---
    // base_frequency is the center of our signal; frequency_offset is additional shift.
    // We put everything in base_frequency and use 0.0 offset.
    let mut modulator = Ft8Modulator::new(FT8_SAMPLE_RATE, args.freq_offset, args.power)
        .expect("modulator creation");
    let samples_12k = modulator
        .modulate_symbols(&symbols, 0.0)
        .expect("modulation");
    let duration_secs = samples_12k.len() as f64 / FT8_SAMPLE_RATE as f64;

    let rms = (samples_12k.iter().map(|s| s * s).sum::<f32>() / samples_12k.len() as f32).sqrt();
    let peak = samples_12k.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
    println!(
        "Modulated {} samples ({:.2}s @ {} Hz)",
        samples_12k.len(),
        duration_secs,
        FT8_SAMPLE_RATE
    );
    println!("Peak: {:.4} ({:.1} dBFS)", peak, 20.0 * peak.log10());
    println!("RMS:  {:.4} ({:.1} dBFS)", rms, 20.0 * rms.log10());
    println!();

    // --- Resample 12 kHz -> 48 kHz (linear interpolation) ---
    let output_rate: u32 = 48000;
    let ratio = output_rate as f64 / FT8_SAMPLE_RATE as f64;
    let out_len = (samples_12k.len() as f64 * ratio) as usize;
    let mut samples_48k = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 / ratio;
        let idx = src_pos as usize;
        let frac = (src_pos - idx as f64) as f32;
        let s0 = samples_12k[idx.min(samples_12k.len() - 1)];
        let s1 = samples_12k[(idx + 1).min(samples_12k.len() - 1)];
        samples_48k.push(s0 + (s1 - s0) * frac);
    }
    println!(
        "Resampled to {} samples ({:.2}s @ {} Hz)",
        samples_48k.len(),
        samples_48k.len() as f64 / output_rate as f64,
        output_rate
    );

    // --- Open output device ---
    let host = cpal::default_host();
    let default_out_name = host.default_output_device().and_then(|d| d.name().ok());

    println!("\nAll devices (cpal sees):");
    if let Ok(devices) = host.devices() {
        for d in devices {
            let name = d.name().unwrap_or_else(|_| "??".into());
            let in_count = d.supported_input_configs().map(|c| c.count()).unwrap_or(0);
            let out_count = d.supported_output_configs().map(|c| c.count()).unwrap_or(0);
            let is_default_out = default_out_name.as_deref() == Some(name.as_str()) && out_count > 0;
            println!(
                "  {:50}  in={}  out={}{}",
                name,
                in_count,
                out_count,
                if is_default_out { "  <-- DEFAULT OUT" } else { "" }
            );
        }
    }

    // Select output device. With --device, search ALL devices (not just
    // output_devices) because some USB audio codecs don't enumerate output
    // configs via cpal but can still be force-opened with a known config.
    // Match is case-insensitive and whitespace-normalized so that
    // "USB AUDIO CODEC" matches the actual "USB AUDIO  CODEC" (double-space).
    let device = if let Some(ref name) = args.device {
        let needle = normalize(name);
        let mut matches: Vec<_> = host
            .devices()
            .expect("Failed to enumerate devices")
            .filter(|d| normalize(&d.name().unwrap_or_default()).contains(&needle))
            .collect();
        if matches.is_empty() {
            eprintln!("No device matching '{}'", name);
            std::process::exit(1);
        }
        // Prefer the device that actually has output configs.
        matches.sort_by_key(|d| {
            let out = d.supported_output_configs().map(|c| c.count()).unwrap_or(0);
            std::cmp::Reverse(out)
        });
        matches.remove(0)
    } else {
        host.default_output_device()
            .expect("No default output device")
    };
    let dev_name = device.name().unwrap_or_else(|_| "unknown".into());
    println!("\nUsing output: {}", dev_name);

    let stream_config = cpal::StreamConfig {
        channels: 1,
        sample_rate: cpal::SampleRate(output_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    // --- Build the output stream NOW (before slot wait) ---
    // Stream construction is non-trivial; doing it early keeps it out of the
    // timing-critical path. cpal's CoreAudio backend invokes the callback
    // even before .play(), so we gate sample emission with a `started` flag:
    // the callback writes silence (and does NOT advance the cursor) until
    // we flip the flag at the precise audio-start instant.
    let samples = Arc::new(samples_48k);
    let cursor = Arc::new(Mutex::new(0usize));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));

    let samples_cb = samples.clone();
    let cursor_cb = cursor.clone();
    let done_cb = done.clone();
    let started_cb = started.clone();

    let stream = device
        .build_output_stream(
            &stream_config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                if !started_cb.load(Ordering::Relaxed) {
                    for sample in data.iter_mut() {
                        *sample = 0.0;
                    }
                    return;
                }
                let mut pos = cursor_cb.lock().unwrap();
                for sample in data.iter_mut() {
                    if *pos < samples_cb.len() {
                        *sample = samples_cb[*pos];
                        *pos += 1;
                    } else {
                        *sample = 0.0;
                        done_cb.store(true, Ordering::Relaxed);
                    }
                }
            },
            |err| eprintln!("Output stream error: {}", err),
            None,
        )
        .expect("Failed to build output stream");

    // Start the stream immediately so CoreAudio's IO unit is running; the
    // callback emits silence until we flip `started`.
    stream.play().expect("Failed to start playback");

    // --- Signal handler: release PTT on Ctrl-C / SIGTERM ---
    // Set up BEFORE engaging PTT so a kill between here and `engage()`
    // returning still has a path to release if it raced.
    let term_flag = Arc::new(AtomicBool::new(false));
    if args.ptt {
        signal_hook::flag::register(signal_hook::consts::SIGINT, term_flag.clone())
            .expect("register SIGINT");
        signal_hook::flag::register(signal_hook::consts::SIGTERM, term_flag.clone())
            .expect("register SIGTERM");
    }

    // --- Compute precise FT8 timing ---
    // FT8 convention: a transmission's audio begins 0.5s into a 15s slot and
    // runs for 12.64s. We aim for the receiver to see DT≈0, so we need to:
    //   1) align to the slot boundary with sub-second precision
    //   2) engage PTT a couple hundred ms early so the relay is settled when
    //      audio starts (instead of clipping the leading sync symbols)
    //   3) call stream.play() at exactly slot+500ms
    const PRE_ROLL_NS: i64 = 500_000_000; // FT8 leading silence
    const PTT_LEAD_NS: i64 = 200_000_000; // engage PTT this much before audio
    const SLOT_NS: i64 = 15_000_000_000;
    const MIN_LEAD_NS: i64 = 1_000_000_000; // require at least 1s of headroom

    let (target_audio_start, target_ptt_engage) = if args.immediate {
        let now = std::time::Instant::now();
        (now, now)
    } else {
        let now_utc_ns = Utc::now()
            .timestamp_nanos_opt()
            .expect("system clock out of i64 ns range");
        let mut target_slot_ns = ((now_utc_ns / SLOT_NS) + 1) * SLOT_NS;
        while target_slot_ns - now_utc_ns < MIN_LEAD_NS {
            target_slot_ns += SLOT_NS;
        }
        let target_audio_ns = target_slot_ns + PRE_ROLL_NS;
        let target_ptt_ns = target_audio_ns - PTT_LEAD_NS;

        let now_inst = std::time::Instant::now();
        let audio_in = Duration::from_nanos((target_audio_ns - now_utc_ns) as u64);
        let ptt_in = Duration::from_nanos((target_ptt_ns - now_utc_ns) as u64);

        let target_slot_secs = target_slot_ns / 1_000_000_000;
        println!(
            "\nWaiting for FT8 slot at :{:02} ({}s lead) — audio starts at slot+0.5s",
            target_slot_secs % 60,
            (target_audio_ns - now_utc_ns) / 1_000_000_000
        );
        (now_inst + audio_in, now_inst + ptt_in)
    };

    // --- PTT ON (slightly before audio start, for relay settle) ---
    // ptt_start MUST be measured at the actual PTT engage time, not before
    // the slot-wait sleep — otherwise the soft-cap check downstream sees an
    // inflated elapsed time and cuts playback mid-transmission.
    let ptt_start;
    let _ptt_guard = if args.ptt {
        let now = std::time::Instant::now();
        if target_ptt_engage > now {
            std::thread::sleep(target_ptt_engage - now);
        }
        ptt_start = std::time::Instant::now();
        println!(
            "PTT ON  at {}  (watchdog: {:.1}s)",
            Utc::now().format("%H:%M:%S%.3f UTC"),
            args.max_tx_secs
        );
        match PttGuard::engage(args.max_tx_secs) {
            Ok(g) => Some(g),
            Err(e) => {
                eprintln!("PTT ON failed: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        ptt_start = std::time::Instant::now();
        None
    };

    // --- Sleep precisely until target audio start, then unmute the stream ---
    let now = std::time::Instant::now();
    if target_audio_start > now {
        std::thread::sleep(target_audio_start - now);
    }
    started.store(true, Ordering::Release);
    println!(
        "Audio  at {}  ({} samples, {:.2}s)",
        Utc::now().format("%H:%M:%S%.3f UTC"),
        samples.len(),
        duration_secs
    );

    // Wait for playback to complete (with a small margin) OR a termination
    // signal OR the soft TX-time cap. On any of those, we abort the stream
    // early; the PttGuard drop will release PTT cleanly. The watchdog inside
    // PttGuard is the hard cap that fires even if this loop is stuck.
    let playback_ms = (duration_secs * 1000.0) as u64 + 500;
    let soft_max = Duration::from_secs_f64(args.max_tx_secs - 0.5);
    let start = std::time::Instant::now();
    while !done.load(std::sync::atomic::Ordering::Relaxed) {
        if term_flag.load(Ordering::Relaxed) {
            println!("\nSignal received — stopping playback, releasing PTT");
            break;
        }
        if args.ptt && ptt_start.elapsed() >= soft_max {
            println!(
                "\nSoft TX cap reached ({:.1}s) — stopping playback",
                soft_max.as_secs_f64()
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
        if start.elapsed() > Duration::from_millis(playback_ms) {
            println!("Playback timeout — forcing stop");
            break;
        }
    }

    // Let the tail settle, then drop the stream
    std::thread::sleep(Duration::from_millis(100));
    drop(stream);

    // PTT release happens automatically via _ptt_guard's Drop impl on
    // function return (or panic, or signal-driven early exit).

    let final_pos = *cursor.lock().unwrap();
    println!(
        "\nDone. Played {}/{} samples ({:.2}s)",
        final_pos,
        samples.len(),
        final_pos as f64 / output_rate as f64
    );
}

/// Normalize device name for matching: lowercase + collapse whitespace.
fn normalize(s: &str) -> String {
    s.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Send PTT command to rigctld via TCP (localhost:4532).
/// Protocol: send "T 1\n" for ON, "T 0\n" for OFF, read response.
fn rigctld_ptt(on: bool) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpStream;

    let mut stream =
        TcpStream::connect_timeout(&"127.0.0.1:4532".parse().unwrap(), Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let cmd = if on { "T 1\n" } else { "T 0\n" };
    stream.write_all(cmd.as_bytes())?;

    let mut reader = BufReader::new(&stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;

    let response = response.trim();
    if response != "RPRT 0" {
        return Err(format!("rigctld error: {}", response).into());
    }
    Ok(())
}
