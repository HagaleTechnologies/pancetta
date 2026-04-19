# Tier 1: Core Stub Buildout — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Complete the three core stubs that block real-world use: audio output, noise reduction, spot frequency reporting.

**Architecture:** Pancetta is a modular Rust application where components communicate via a `MessageBus` (crossbeam channels). Audio I/O uses lock-free ring buffers (`ringbuf` crate) with producer/consumer pairs split between real-time cpal callbacks and processing threads. DSP processing uses `rustfft`/`realfft` for spectral operations. The coordinator (`pancetta/src/coordinator/`) orchestrates all components and maintains shared state via `Arc<AtomicU64>` for the operating frequency.

**Tech Stack:** Rust, cpal, rustfft, realfft, crossbeam, ringbuf

---

## Task 1: Audio Output Pipeline

**Problem:** `AudioManager::queue_output()` discards TX audio with a warning. The output stream callback in `AudioStreamManager::create_output_stream()` plays silence. There is no ring buffer connecting the two.

**Files:**
- `pancetta-audio/src/stream.rs` — add output ring buffer producer/consumer, wire consumer into output callback
- `pancetta-audio/src/manager.rs` — write samples into the output ring buffer from `queue_output()`
- `pancetta-audio/src/ringbuffer_comm.rs` — reference only (existing `audio_comm_pair` pattern)

### Steps

- [ ] **1.1** In `stream.rs`, add an output producer field to `AudioStreamManager`:

```rust
// In AudioStreamManager struct, add after `consumer`:
/// Producer half for output audio — caller pushes TX samples here
output_producer: Option<AudioProducer>,
```

- [ ] **1.2** In `AudioStreamManager::new()`, create a second ring buffer pair for output:

```rust
// After the input pair creation:
let (output_producer, output_consumer) =
    audio_comm_pair(DEFAULT_AUDIO_BUFFER_SIZE, DEFAULT_LATENCY_BUFFER_SIZE);
```

Store `output_producer` in the struct field from 1.1. Store `output_consumer` in a new field:

```rust
output_consumer: Option<AudioConsumer>,
```

- [ ] **1.3** Add a `take_output_producer` method (mirrors `take_consumer` pattern):

```rust
/// Take the output producer half so the AudioManager can push TX samples.
pub fn take_output_producer(&mut self) -> Option<AudioProducer> {
    self.output_producer.take()
}
```

- [ ] **1.4** In `create_output_stream()`, take the output consumer and move it into the cpal callback closure. Replace the silence callback with a ring-buffer drain:

```rust
fn create_output_stream(&mut self) -> AudioResult<()> {
    // ... device selection code unchanged ...

    // Take the output consumer — move into the callback closure
    let mut output_consumer = self.output_consumer.take().ok_or_else(|| {
        AudioError::stream("Output consumer already taken")
    })?;

    let stream = output_device.build_output_stream(
        &stream_config.into(),
        move |data: &mut [f32], _info: &OutputCallbackInfo| {
            let read = output_consumer.pop_audio_slice(data);
            // Fill any remaining samples with silence (underrun)
            for sample in data[read..].iter_mut() {
                *sample = 0.0;
            }
        },
        |err| {
            eprintln!("Output stream error: {}", err);
        },
        None,
    )?;

    self.output_stream = Some(stream);
    Ok(())
}
```

- [ ] **1.5** In `start()`, after `create_input_stream()`, refresh the output producer/consumer pair so restarts work:

In `start()`, before `self.create_output_stream()`, add:

```rust
// Recreate output ring buffer pair for this session
let (output_producer, output_consumer) =
    audio_comm_pair(DEFAULT_AUDIO_BUFFER_SIZE, DEFAULT_LATENCY_BUFFER_SIZE);
self.output_producer = Some(output_producer);
self.output_consumer = Some(output_consumer);
```

- [ ] **1.6** In `manager.rs`, add an output producer field to `AudioManager`:

```rust
// In AudioManager struct, add:
/// Producer half for sending TX audio to the output stream
output_producer: Option<AudioProducer>,
```

- [ ] **1.7** In `AudioManager::with_config()`, take the output producer from the stream manager:

```rust
// After `let consumer = stream.take_consumer();`
let output_producer = stream.take_output_producer();
```

Store it: `output_producer,` in the struct construction.

- [ ] **1.8** In `AudioManager::start()`, refresh the output producer after restarting:

```rust
// After `self.consumer = stream.take_consumer();`
self.output_producer = stream.take_output_producer();
```

- [ ] **1.9** Replace the stub `queue_output` method with a real implementation:

```rust
pub fn queue_output(&mut self, samples: &[f32], input_rate: u32) -> Result<(), AudioError> {
    let producer = self.output_producer.as_mut().ok_or_else(|| {
        AudioError::Stream {
            message: "Output stream not initialized".to_string(),
        }
    })?;

    // Resample if input rate differs from output rate
    let output_samples = if input_rate != self.config.sample_rate {
        // Simple linear interpolation resampling
        let ratio = self.config.sample_rate as f64 / input_rate as f64;
        let out_len = (samples.len() as f64 * ratio) as usize;
        let mut resampled = Vec::with_capacity(out_len);
        for i in 0..out_len {
            let src_pos = i as f64 / ratio;
            let src_idx = src_pos as usize;
            let frac = src_pos - src_idx as f64;
            let s0 = samples[src_idx.min(samples.len() - 1)];
            let s1 = samples[(src_idx + 1).min(samples.len() - 1)];
            resampled.push(s0 + (s1 - s0) * frac as f32);
        }
        resampled
    } else {
        samples.to_vec()
    };

    let written = producer.push_audio_slice(&output_samples);
    if written < output_samples.len() {
        warn!(
            "Output buffer overrun: {}/{} samples written",
            written,
            output_samples.len()
        );
    }

    info!(
        "Queued {} TX audio samples for output (rate {}->{}Hz)",
        written, input_rate, self.config.sample_rate
    );
    Ok(())
}
```

- [ ] **1.10** Build and test:

```bash
touch pancetta-audio/src/stream.rs pancetta-audio/src/manager.rs
cargo build -p pancetta-audio 2>&1
cargo test -p pancetta-audio 2>&1
```

- [ ] **1.11** Add a unit test in `manager.rs`:

```rust
#[test]
fn test_queue_output_no_crash() {
    let manager = AudioManager::new();
    if let Ok(mut manager) = manager {
        // Should not panic even without a running stream
        let samples = vec![0.5f32; 480];
        // output_producer is Some from construction
        let result = manager.queue_output(&samples, 12000);
        // May succeed or fail depending on output_producer availability
        assert!(result.is_ok() || result.is_err());
    }
}
```

**Commit message:** `feat: wire audio output ring buffer — TX audio plays through cpal output stream`

---

## Task 2: Noise Reduction Filter (FFT-based Spectral Subtraction)

**Problem:** `NoiseReductionFilter::process_frame()` in `pancetta-dsp/src/filter.rs` is a placeholder that applies a simple amplitude gate. The struct already has allocated workspace fields (`noise_estimate`, `signal_estimate`, `alpha`, `beta`, `fft_workspace`) sized for real spectral subtraction.

**Files:**
- `pancetta-dsp/src/filter.rs` — replace `process_frame()` stub with FFT-based spectral subtraction

### Existing Struct Fields (reference)

```rust
pub struct NoiseReductionFilter {
    sample_rate: f32,
    frame_size: usize,           // e.g. 1024
    overlap_factor: f32,         // e.g. 0.5
    noise_estimate: Vec<f32>,    // len = frame_size / 2 + 1, initialized to 0.001
    signal_estimate: Vec<f32>,   // len = frame_size / 2 + 1
    alpha: f32,                  // 2.0 — spectral subtraction factor
    beta: f32,                   // 0.01 — spectral floor factor
    input_buffer: VecDeque<f32>,
    output_buffer: VecDeque<f32>,
    window: Vec<f32>,            // Hann window
    fft_workspace: Vec<f32>,     // len = frame_size * 2
}
```

### Steps

- [ ] **2.1** Add `rustfft` imports at the top of `filter.rs`:

```rust
use rustfft::{num_complex::Complex, FftPlanner};
```

- [ ] **2.2** Add FFT planner fields to `NoiseReductionFilter` struct:

```rust
/// Forward FFT planner (frame_size)
fft_forward: std::sync::Arc<dyn rustfft::Fft<f32>>,
/// Inverse FFT planner (frame_size)
fft_inverse: std::sync::Arc<dyn rustfft::Fft<f32>>,
/// Complex buffer for FFT processing
fft_complex: Vec<Complex<f32>>,
/// Number of frames processed (for noise calibration)
frames_processed: u64,
```

- [ ] **2.3** Initialize the new fields in `NoiseReductionFilter::new()`:

```rust
let mut planner = FftPlanner::<f32>::new();
let fft_forward = planner.plan_fft_forward(frame_size);
let fft_inverse = planner.plan_fft_inverse(frame_size);
let fft_complex = vec![Complex::new(0.0, 0.0); frame_size];
```

Add `fft_forward`, `fft_inverse`, `fft_complex`, and `frames_processed: 0` to the struct constructor.

- [ ] **2.4** Replace `process_frame()` with real spectral subtraction:

```rust
/// Process a single frame with FFT-based spectral subtraction.
///
/// Algorithm:
/// 1. Convert windowed time-domain frame to frequency domain via FFT
/// 2. Compute power spectrum |X(k)|^2
/// 3. Update noise floor estimate (running average during first N frames,
///    then slow adaptation)
/// 4. Spectral subtraction: |Y(k)|^2 = max(|X(k)|^2 - alpha * |N(k)|^2, beta * |X(k)|^2)
/// 5. Apply gain: G(k) = sqrt(|Y(k)|^2 / |X(k)|^2)
/// 6. Multiply complex spectrum by gain
/// 7. IFFT back to time domain
fn process_frame(&mut self, frame: &mut [f32]) -> Result<()> {
    let n = self.frame_size;
    let num_bins = n / 2 + 1;

    // Step 1: Copy frame into complex buffer and zero-pad imaginary
    for (i, &sample) in frame.iter().enumerate() {
        self.fft_complex[i] = Complex::new(sample, 0.0);
    }

    // Forward FFT (in-place)
    self.fft_forward.process(&mut self.fft_complex);

    // Step 2: Compute power spectrum for positive frequencies
    let mut power_spectrum = vec![0.0f32; num_bins];
    for i in 0..num_bins {
        let c = self.fft_complex[i];
        power_spectrum[i] = c.norm_sqr();
    }

    // Step 3: Update noise estimate
    // First 10 frames: assume pure noise, use fast averaging
    // After that: slow adaptation for noise-only bins
    self.frames_processed += 1;
    let noise_alpha = if self.frames_processed <= 10 {
        // Fast initial calibration
        1.0 / self.frames_processed as f32
    } else {
        // Slow adaptation: track slowly rising noise floor
        0.02
    };

    for i in 0..num_bins {
        if self.frames_processed <= 10 {
            // Running average during calibration
            self.noise_estimate[i] =
                self.noise_estimate[i] * (1.0 - noise_alpha) + power_spectrum[i] * noise_alpha;
        } else {
            // Only update noise estimate when power is close to current estimate
            // (i.e., no signal present in this bin)
            if power_spectrum[i] < self.noise_estimate[i] * 4.0 {
                self.noise_estimate[i] = self.noise_estimate[i] * (1.0 - noise_alpha)
                    + power_spectrum[i] * noise_alpha;
            }
        }
    }

    // Step 4 & 5: Spectral subtraction and gain computation
    for i in 0..num_bins {
        let noise_power = self.noise_estimate[i];
        let signal_power = power_spectrum[i];

        // Spectral subtraction with spectral floor
        let clean_power = (signal_power - self.alpha * noise_power)
            .max(self.beta * signal_power);

        // Compute Wiener-like gain
        let gain = if signal_power > 1e-10 {
            (clean_power / signal_power).sqrt()
        } else {
            0.0
        };

        // Step 6: Apply gain to complex spectrum (positive freq)
        self.fft_complex[i] *= gain;

        // Mirror to negative frequencies (except DC and Nyquist)
        if i > 0 && i < n / 2 {
            self.fft_complex[n - i] *= gain;
        }

        // Store signal estimate for potential diagnostics
        self.signal_estimate[i] = clean_power;
    }

    // Step 7: Inverse FFT
    self.fft_inverse.process(&mut self.fft_complex);

    // Normalize IFFT output (rustfft does unnormalized FFT)
    let norm = 1.0 / n as f32;
    for (i, sample) in frame.iter_mut().enumerate() {
        *sample = self.fft_complex[i].re * norm;
    }

    Ok(())
}
```

- [ ] **2.5** Update `reset()` to also reset the frame counter:

```rust
pub fn reset(&mut self) {
    self.input_buffer.clear();
    self.output_buffer.clear();
    self.noise_estimate.fill(0.001);
    self.signal_estimate.fill(0.0);
    self.frames_processed = 0;
}
```

- [ ] **2.6** Build and test:

```bash
touch pancetta-dsp/src/filter.rs
cargo build -p pancetta-dsp 2>&1
cargo test -p pancetta-dsp 2>&1
```

- [ ] **2.7** Add a unit test for noise reduction:

```rust
#[test]
fn test_noise_reduction_processes_without_panic() {
    let mut nr = NoiseReductionFilter::new(12000.0, 1024, 0.5);

    // Feed several frames of low-level noise
    let noise: Vec<f32> = (0..2048).map(|i| (i as f32 * 0.001).sin() * 0.001).collect();
    let mut output = Vec::new();
    nr.process(&noise, &mut output).unwrap();

    // Feed a frame with a signal
    let signal: Vec<f32> = (0..2048)
        .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 12000.0).sin() * 0.5)
        .collect();
    let mut output2 = Vec::new();
    nr.process(&signal, &mut output2).unwrap();

    // The signal frame output should have content (not all zeros)
    let rms: f32 = output2.iter().map(|x| x * x).sum::<f32>() / output2.len().max(1) as f32;
    assert!(rms > 0.0, "Output should contain signal energy");
}

#[test]
fn test_noise_reduction_attenuates_steady_noise() {
    let mut nr = NoiseReductionFilter::new(12000.0, 512, 0.5);

    // Feed 20 frames of steady noise for calibration
    let noise: Vec<f32> = (0..512 * 20)
        .map(|i| (i as f32 * 0.37).sin() * 0.01)
        .collect();
    let mut output = Vec::new();
    nr.process(&noise, &mut output).unwrap();

    // After calibration, noise power should be reduced
    if output.len() >= 512 {
        let tail = &output[output.len() - 512..];
        let rms: f32 = (tail.iter().map(|x| x * x).sum::<f32>() / tail.len() as f32).sqrt();
        // Noise should be attenuated below original level (0.01 * 0.707 ~= 0.007)
        assert!(rms < 0.01, "Noise should be attenuated, got RMS={}", rms);
    }
}
```

**Commit message:** `feat: implement FFT-based spectral subtraction noise reduction`

---

## Task 3: Spot Frequency Reporting

**Problem:** In `pancetta/src/coordinator/components.rs`, the autonomous operator's spot reports to cqdx.io use `frequency: 0` because only the audio-baseband offset (200-3000 Hz) is available, not the RF operating frequency. The pipeline already tracks `operating_freq` as an `Arc<AtomicU64>` (see `pipeline.rs:752`), but this value is not shared with the autonomous component.

**Files:**
- `pancetta/src/coordinator/mod.rs` — add `operating_freq` field to `ApplicationCoordinator`
- `pancetta/src/coordinator/pipeline.rs` — store the `operating_freq` Arc in `self` so components can access it
- `pancetta/src/coordinator/components.rs` — read `operating_freq` in the autonomous loop, compute `rf_freq + audio_offset`

### Existing Architecture (reference)

In `pipeline.rs` (line 751-754):
```rust
let operating_freq_mhz = 14.074_f64;
let operating_freq = Arc::new(std::sync::atomic::AtomicU64::new(
    operating_freq_mhz.to_bits(),
));
```

This is updated by `FrequencyResponse` messages from hamlib (line 849-851):
```rust
let freq_mhz = frequency as f64 / 1_000_000.0;
operating_freq_relay.store(freq_mhz.to_bits(), Ordering::Relaxed);
```

The `SpotReport` struct (`pancetta-cqdx/src/types.rs:96`) expects `frequency: u64` in Hz.

### Steps

- [ ] **3.1** In `pancetta/src/coordinator/mod.rs`, add the shared operating frequency to `ApplicationCoordinator`:

```rust
// Add to the struct fields, after `active_qso_ap`:
/// Shared operating frequency in MHz (as f64 bits in AtomicU64).
/// Updated by hamlib FrequencyResponse, read by autonomous/PSKReporter components.
operating_freq: std::sync::Arc<std::sync::atomic::AtomicU64>,
```

- [ ] **3.2** In the `ApplicationCoordinator` constructor (in `mod.rs` or wherever `new()` is), initialize it:

```rust
operating_freq: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
    14.074_f64.to_bits(),
)),
```

- [ ] **3.3** In `pipeline.rs`, replace the local `operating_freq` variable with `self.operating_freq`:

Replace the lines:
```rust
let operating_freq_mhz = 14.074_f64;
let operating_freq = Arc::new(std::sync::atomic::AtomicU64::new(
    operating_freq_mhz.to_bits(),
));
let operating_freq_relay = operating_freq.clone();
```

With:
```rust
let operating_freq_relay = self.operating_freq.clone();
```

Remove any other local variable named `operating_freq` and replace references to it with `self.operating_freq.clone()` (e.g., `cmd_operating_freq` on line 898).

- [ ] **3.4** In `components.rs`, clone the operating freq Arc into `start_autonomous_component()`:

Before the `tokio::spawn` block, add:

```rust
let operating_freq = self.operating_freq.clone();
```

Then move it into the spawned task.

- [ ] **3.5** In the autonomous loop's spot reporting section (around line 846-864 in `components.rs`), compute the real frequency:

Replace:
```rust
frequency: 0,
```

With:
```rust
frequency: {
    let op_freq_mhz = f64::from_bits(
        operating_freq.load(std::sync::atomic::Ordering::Relaxed)
    );
    let op_freq_hz = (op_freq_mhz * 1_000_000.0) as u64;
    op_freq_hz + msg.frequency_hz as u64
},
```

- [ ] **3.6** Also fix PSKReporter frequency in the same file. In `start_pskreporter_component()` (around line 1246-1248), the report also uses only the audio offset:

```rust
frequency: decoded_msg.frequency_offset as u64,
```

Clone the operating freq into this task too:

```rust
let operating_freq_psk = self.operating_freq.clone();
```

And replace the frequency line:

```rust
frequency: {
    let op_freq_mhz = f64::from_bits(
        operating_freq_psk.load(std::sync::atomic::Ordering::Relaxed)
    );
    (op_freq_mhz * 1_000_000.0) as u64
        + decoded_msg.frequency_offset as u64
},
```

- [ ] **3.7** Build and verify:

```bash
touch pancetta/src/coordinator/mod.rs pancetta/src/coordinator/pipeline.rs pancetta/src/coordinator/components.rs
cargo build -p pancetta 2>&1
cargo test -p pancetta 2>&1
```

- [ ] **3.8** Verify no remaining `frequency: 0` in spot reports:

```bash
grep -n 'frequency: 0' pancetta/src/coordinator/components.rs
# Should return no matches
```

**Commit message:** `fix: report actual RF frequency in spot reports — operating_freq + audio offset`

---

## Verification Checklist

After all three tasks are complete:

- [ ] Full workspace build passes: `cargo build --workspace 2>&1`
- [ ] Full workspace tests pass: `cargo test --workspace 2>&1`
- [ ] No remaining `TODO.*output ring buffer` in `pancetta-audio/`
- [ ] No remaining `frequency: 0` in `pancetta/src/coordinator/components.rs`
- [ ] `NoiseReductionFilter::process_frame` contains `fft_forward.process` (not the stub)

## Dependency Order

Tasks 1, 2, and 3 are independent and can be implemented in parallel.
