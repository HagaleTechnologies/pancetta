# TX Hardware Validation Tool

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a standalone TX test tool that validates the audio output + PTT hardware path end-to-end, independent of the coordinator/message bus.

**Architecture:** A single example binary (`pancetta/examples/tx_test.rs`) that uses `pancetta-ft8` to encode+modulate an FT8 message and `cpal` directly to play it out the USB AUDIO CODEC. Bypasses the coordinator, message bus, and `AudioManager` entirely — minimal moving parts so failures point at hardware. Optional `--ptt` flag connects to rigctld for PTT control.

**Tech Stack:** Rust, cpal, pancetta-ft8 (encoder + modulator), clap (CLI), chrono (slot timing), tokio (for hamlib async PTT)

**Known Bug Found During Research:** The coordinator's `AudioStreamManager::start()` only creates the output stream when `enable_monitoring: true` (stream.rs:183), but the coordinator sets `enable_monitoring: false` (pipeline.rs:352). This means `queue_output()` pushes samples into a ring buffer with no consumer. This must be fixed separately after we validate the hardware path with the standalone tool.

---

### Task 1: Create the TX test binary with encoding and audio output

**Files:**
- Create: `pancetta/examples/tx_test.rs`

- [ ] **Step 1: Write the example binary**

```rust
//! TX hardware validation tool.
//!
//! Encodes an FT8 message, waits for slot boundary, and plays audio out the
//! default output device.  Use `--ptt` to key the radio via rigctld.
//!
//! Usage:
//!   cargo run --example tx_test
//!   cargo run --example tx_test -- --ptt
//!   cargo run --example tx_test -- --message "CQ K1ABC EM10" --freq-offset 1200

use chrono::Utc;
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use pancetta_ft8::{Ft8Encoder, Ft8Modulator, SAMPLE_RATE as FT8_SAMPLE_RATE};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "tx_test", about = "FT8 TX hardware validation")]
struct Args {
    /// FT8 message to transmit
    #[arg(long, default_value = "K1ABC RR73")]
    message: String,

    /// Audio frequency offset in Hz (within FT8 passband)
    #[arg(long, default_value_t = 1500.0)]
    freq_offset: f64,

    /// TX power level (0.0 - 1.0)
    #[arg(long, default_value_t = 0.5)]
    power: f64,

    /// Enable PTT via rigctld (localhost:4532)
    #[arg(long)]
    ptt: bool,

    /// Skip waiting for FT8 slot boundary
    #[arg(long)]
    immediate: bool,
}

fn main() {
    let args = Args::parse();

    println!("FT8 TX Test");
    println!("===========");
    println!("Message:     \"{}\"", args.message);
    println!("Freq offset: {} Hz", args.freq_offset);
    println!("Power:       {:.1}%", args.power * 100.0);
    println!("PTT:         {}", if args.ptt { "ENABLED" } else { "disabled" });
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
    let mut modulator =
        Ft8Modulator::new(FT8_SAMPLE_RATE, args.freq_offset, args.power).expect("modulator creation");
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

    // --- Resample 12 kHz → 48 kHz (linear interpolation) ---
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
    println!("\nOutput devices:");
    if let Ok(devices) = host.output_devices() {
        for d in devices {
            let name = d.name().unwrap_or_else(|_| "??".into());
            let is_default = host
                .default_output_device()
                .map(|dd| dd.name().ok() == d.name().ok())
                .unwrap_or(false);
            println!("  {}{}", name, if is_default { " <-- DEFAULT" } else { "" });
        }
    }

    let device = host
        .default_output_device()
        .expect("No default output device");
    let dev_name = device.name().unwrap_or_else(|_| "unknown".into());
    println!("\nUsing output: {}", dev_name);

    let stream_config = cpal::StreamConfig {
        channels: 1,
        sample_rate: cpal::SampleRate(output_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    // --- Wait for FT8 slot boundary ---
    if !args.immediate {
        let now = Utc::now();
        let secs_in_slot = now.timestamp() % 15;
        let wait = if secs_in_slot == 0 { 0 } else { 15 - secs_in_slot };
        if wait > 0 {
            println!(
                "\nWaiting {}s for next FT8 slot (:{:02})...",
                wait,
                (now.timestamp() + wait) % 60
            );
            std::thread::sleep(Duration::from_secs(wait as u64));
        }
        println!("Slot started at {}", Utc::now().format("%H:%M:%S UTC"));
    }

    // --- PTT ON ---
    if args.ptt {
        println!("PTT ON");
        if let Err(e) = rigctld_ptt(true) {
            eprintln!("PTT ON failed: {}", e);
            std::process::exit(1);
        }
        // TX settle delay
        std::thread::sleep(Duration::from_millis(100));
    }

    // --- Play audio ---
    let samples = Arc::new(samples_48k);
    let cursor = Arc::new(Mutex::new(0usize));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let samples_cb = samples.clone();
    let cursor_cb = cursor.clone();
    let done_cb = done.clone();

    let stream = device
        .build_output_stream(
            &stream_config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut pos = cursor_cb.lock().unwrap();
                for sample in data.iter_mut() {
                    if *pos < samples_cb.len() {
                        *sample = samples_cb[*pos];
                        *pos += 1;
                    } else {
                        *sample = 0.0;
                        done_cb.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            },
            |err| eprintln!("Output stream error: {}", err),
            None,
        )
        .expect("Failed to build output stream");

    stream.play().expect("Failed to start playback");
    println!("Playing audio...");

    // Wait for playback to complete (with a small margin)
    let playback_ms = (duration_secs * 1000.0) as u64 + 500;
    let start = std::time::Instant::now();
    while !done.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(10));
        if start.elapsed() > Duration::from_millis(playback_ms) {
            println!("Playback timeout — forcing stop");
            break;
        }
    }

    // Let the tail settle
    std::thread::sleep(Duration::from_millis(100));
    drop(stream);

    // --- PTT OFF ---
    if args.ptt {
        println!("PTT OFF");
        if let Err(e) = rigctld_ptt(false) {
            eprintln!("PTT OFF failed: {}", e);
        }
    }

    let final_pos = *cursor.lock().unwrap();
    println!(
        "\nDone. Played {}/{} samples ({:.2}s)",
        final_pos,
        samples.len(),
        final_pos as f64 / output_rate as f64
    );
}

/// Send PTT command to rigctld via TCP (localhost:4532).
/// Protocol: send "T 1\n" for ON, "T 0\n" for OFF, read response.
fn rigctld_ptt(on: bool) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpStream;

    let mut stream = TcpStream::connect_timeout(
        &"127.0.0.1:4532".parse().unwrap(),
        Duration::from_secs(2),
    )?;
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
```

- [ ] **Step 2: Build and verify it compiles**

Run: `cargo build --example tx_test`
Expected: Compiles without errors.

- [ ] **Step 3: Run without PTT to verify encoding and device enumeration**

Run: `cargo run --example tx_test -- --immediate`
Expected: Output shows encoded symbols, modulated samples, output devices listed, audio plays (may be silence if output goes to USB AUDIO CODEC — that's fine, we're verifying the code path works).

- [ ] **Step 4: Commit**

```bash
git add pancetta/examples/tx_test.rs
git commit -m "feat: standalone TX hardware validation tool"
```

---

### Task 2: Fix output stream not created in coordinator

**Files:**
- Modify: `pancetta-audio/src/stream.rs:182-185`

The output stream is only created when `enable_monitoring` is true. TX audio needs an output stream regardless. The fix: always create the output stream when TX might be used (which is always in a ham radio station).

- [ ] **Step 1: Change the output stream to always be created**

In `pancetta-audio/src/stream.rs`, replace the conditional:

```rust
        // Create output stream if monitoring is enabled
        if self.config.enable_monitoring {
            self.create_output_stream()?;
        }
```

with:

```rust
        // Always create output stream — needed for TX audio playback.
        // When not transmitting, the callback simply outputs silence (zero-fill).
        self.create_output_stream()?;
```

- [ ] **Step 2: Build and run tests**

Run: `cargo build -p pancetta-audio && cargo test -p pancetta-audio`
Expected: Compiles and tests pass. (Output stream now always created.)

- [ ] **Step 3: Commit**

```bash
git add pancetta-audio/src/stream.rs
git commit -m "fix: always create output stream for TX audio playback"
```

---

### Task 3: Clean up the check_input diagnostic example

**Files:**
- Keep: `pancetta-audio/examples/check_input.rs` (already created earlier in this session)

- [ ] **Step 1: Verify check_input builds alongside tx_test**

Run: `cargo build --example check_input -p pancetta-audio && cargo build --example tx_test`
Expected: Both compile.

- [ ] **Step 2: Commit check_input if not already committed**

```bash
git add pancetta-audio/examples/check_input.rs
git commit -m "feat: audio input diagnostic tool for device validation"
```

---

### Task 4: End-to-end hardware validation (manual, from VNC/tmux session)

This is the manual validation sequence. Run from a VNC session or tmux started from the GUI (for macOS microphone/audio permissions).

- [ ] **Step 1: Verify audio output device is the USB AUDIO CODEC**

Run: `cargo run --example tx_test -- --immediate`
Expected: Output lists "USB AUDIO CODEC" as an output device. Audio plays.

Check the FTdx10's ALC meter — if audio is arriving via USB, the ALC meter should deflect. If it doesn't:
- Check FTdx10 Menu 063 (`USB MOD LEVEL`) — should be non-zero
- Check FTdx10 Menu 064 (`DATA MODE`) — should be DATA-USB/USB

- [ ] **Step 2: Test with slot alignment (no PTT)**

Run: `cargo run --example tx_test`
Expected: Waits for next :00/:15/:30/:45 boundary, then plays. Confirm ALC deflects at the right time.

- [ ] **Step 3: Test with PTT (on-air transmission)**

Requires rigctld running: `rigctld -m 1042 -r /dev/cu.usbserial-01A6218A0 -s 38400 &`

Run: `cargo run --example tx_test -- --ptt`
Expected:
- "PTT ON" printed
- Radio keys up (TX indicator on FTdx10)
- Audio plays for ~12.64 seconds
- "PTT OFF" printed
- Radio unkeys

Verify on a second receiver or WSJT-X instance that the FT8 signal is decodable.

- [ ] **Step 4: Test with a CQ message**

Run: `cargo run --example tx_test -- --ptt --message "CQ K1ABC EM10"`
Expected: Same as above, but the decoded message should read "CQ K1ABC EM10" on a monitoring receiver.
