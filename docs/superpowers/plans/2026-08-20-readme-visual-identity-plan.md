# README & Visual Identity Overhaul Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give pancetta a `--replay <wav-dir>` demo mode that drives the full TUI pipeline from a directory of WAV captures, use it to record VHS demo GIFs and screenshots, and rewrite the README around those assets.

**Architecture:** `--replay` slots in as a third `start_audio_pipeline` branch (alongside the existing real-`AudioManager` and `PANCETTA_STUB_AUDIO` branches), feeding resampled WAV audio into the same `audio_to_dsp_tx` channel the stub already uses — no changes to DSP, FT8, QSO, or TUI code. A small `replay.rs` module holds the pure, testable file-listing/resampling logic; the async pacing loop mirrors the existing stub's `tokio::time::interval` pattern. VHS `.tape` scripts then drive the built binary in replay mode to produce the demo assets, and the README is rewritten around them with detail pushed into `docs/`.

**Tech Stack:** Rust (existing `pancetta` binary crate), `hound` (already a dependency) for WAV I/O, `tempfile` (already a dev-dependency) for tests, `assert_cmd`/`predicates` (already dev-dependencies) for CLI-level integration tests, VHS (`charmbracelet/vhs`) for terminal recording, GitHub Actions for CI regeneration.

**Spec:** `docs/superpowers/specs/2026-08-20-readme-visual-identity-design.md`

## Global Constraints

- No changes to DSP, FT8 decode, QSO engine, autonomous operator, or TX-arm gating logic — this work only adds a new audio *source* and *documents/demonstrates* existing invariants (`docs/superpowers/specs/2026-08-20-readme-visual-identity-design.md`, "Out of scope").
- Reuse existing helpers (`resample_linear` in `util.rs`, the WAV-read/mono-mix pattern in `wav_playback.rs`, the real-time pacing pattern in `audio.rs`'s stub branch) rather than duplicating logic.
- README target length ~1,200 words (spec, Phase 2). Displaced content moves to `docs/TROUBLESHOOTING.md` and `docs/PROVENANCE.md`, not deleted.
- Author attribution: K5ARH, per spec.
- `cargo test --workspace --features transmit` and `cargo clippy --workspace --features transmit` must stay green after every task (per `AGENTS.md` build/test invariants).
- Branch `docs/readme-visual-identity` already created and checked out; work continues there. Commit after every task per this plan's step lists.

---

## File Structure

| File | Responsibility |
|---|---|
| `pancetta/src/coordinator/wav_playback.rs` | Modified: `read_wav_mono` extracted as a standalone, reusable function; `run_wav_playback` calls it instead of inlining the read/mix logic. |
| `pancetta/src/coordinator/replay.rs` | New. Pure functions `list_wav_files_sorted` and `load_replay_samples` (file discovery + read + resample, fully unit-testable, no async/timing), plus the async `start_replay_pipeline` method that paces the loaded samples onto `audio_to_dsp_tx`. |
| `pancetta/src/coordinator/audio.rs` | Modified: `start_audio_pipeline` gains an `else if` branch that calls `start_replay_pipeline` when `self.replay_path` is set. |
| `pancetta/src/coordinator/mod.rs` | Modified: new `replay_path: Option<PathBuf>` field, `mod replay;` declaration, constructor parameter threaded through `ApplicationCoordinator::new`. |
| `pancetta/src/main.rs` | Modified: new `--replay <PATH>` CLI flag, threaded into the `ApplicationCoordinator::new` call. |
| `pancetta/tests/replay_mode.rs` | New. Subprocess integration test: `--replay` against a small synthetic WAV directory runs the full pipeline and exits cleanly. |
| `assets/demo-wav/` | New. Copy of the existing `pancetta-ft8/tests/fixtures/wav/basicft8/` sequential captures (170923_082000 → _082045), used as VHS recording input — decoupled from test fixtures so it doesn't drift if fixtures change. |
| `.tapes/demo.tape` | New. VHS script for the hero GIF. |
| `.tapes/README.md` | New. One-paragraph note on installing VHS and re-running tapes. |
| `.github/workflows/demo-assets.yml` | New. Regenerates GIFs from `.tapes/*.tape` on push to files that affect the TUI, via `charmbracelet/vhs-action`. |
| `docs/TROUBLESHOOTING.md` | New. Troubleshooting section moved out of the README. |
| `docs/PROVENANCE.md` | New. Provenance/clean-room essay moved out of the README. |
| `README.md` | Rewritten per spec Phase 2 structure. |

---

## Task 1: Extract `read_wav_mono` from `wav_playback.rs`

**Files:**
- Modify: `pancetta/src/coordinator/wav_playback.rs`
- Test: same file, `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `pub(crate) fn read_wav_mono(path: &std::path::Path) -> anyhow::Result<(Vec<f32>, u32)>` — returns `(mono_samples, native_sample_rate)`. Later tasks (Task 2) import this as `super::wav_playback::read_wav_mono`.

- [ ] **Step 1: Write the failing test**

Add to the existing `#[cfg(test)] mod tests` block in `pancetta/src/coordinator/wav_playback.rs` (alongside the existing `int_sample_max_val` tests):

```rust
    #[test]
    fn read_wav_mono_mixes_stereo_and_reports_native_rate() {
        use std::io::Cursor;

        // Build a tiny 2-channel, 16-bit, 8000 Hz WAV in memory: left=1.0, right=-1.0
        // for every sample, so mono mix-down must produce 0.0 at every sample.
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stereo.wav");
        {
            let mut writer = hound::WavWriter::create(&path, spec).unwrap();
            for _ in 0..100 {
                writer.write_sample(i16::MAX).unwrap();
                writer.write_sample(i16::MIN + 1).unwrap();
            }
            writer.finalize().unwrap();
        }

        let (mono, rate) = read_wav_mono(&path).unwrap();
        assert_eq!(rate, 8000);
        assert_eq!(mono.len(), 100);
        for s in mono {
            assert!(s.abs() < 0.001, "expected near-zero mix-down, got {s}");
        }
        let _ = Cursor::new(Vec::<u8>::new()); // silence unused-import if hound changes later
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta --lib coordinator::wav_playback::tests::read_wav_mono_mixes_stereo_and_reports_native_rate`
Expected: FAIL with "cannot find function `read_wav_mono`"

- [ ] **Step 3: Extract the function and update `run_wav_playback` to use it**

Replace the top of `run_wav_playback` (the `hound::WavReader::open` through the mono mix-down, currently lines ~33-70) so the file becomes:

```rust
use anyhow::Result;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use std::path::{Path, PathBuf};
use tracing::{debug, info};

use super::util::resample_linear;

/// Compute the integer-PCM normalization scale (`2^(bits-1)`) for a WAV's
/// `bits_per_sample`, validating it first.
///
/// `bits_per_sample` comes straight out of an attacker-controlled WAV header.
/// Rejecting out-of-range values up front avoids the `bits - 1` u16 underflow
/// (panic in debug / wrap in release) and the subsequent oversized
/// `1i64 << (bits - 1)` shift that the naive expression would hit on a
/// malformed `0` or absurdly large field. Valid PCM widths (8/16/24/32) are
/// unaffected.
fn int_sample_max_val(bits_per_sample: u16) -> Result<f32> {
    if !(1..=32).contains(&bits_per_sample) {
        anyhow::bail!(
            "Unsupported WAV bits_per_sample: {} (expected 1..=32)",
            bits_per_sample
        );
    }
    Ok((1i64 << (bits_per_sample - 1)) as f32)
}

/// Read a WAV file, mixing to mono if it's multi-channel. Returns the mono
/// samples at the file's own native sample rate (no resampling here) plus
/// that rate, so callers can resample to whatever target they need.
pub(crate) fn read_wav_mono(path: &Path) -> Result<(Vec<f32>, u32)> {
    let reader = hound::WavReader::open(path)
        .map_err(|e| anyhow::anyhow!("Failed to open WAV file {}: {}", path.display(), e))?;

    let spec = reader.spec();
    info!(
        "WAV: {} channels, {} Hz, {:?}, {} bits",
        spec.channels, spec.sample_rate, spec.sample_format, spec.bits_per_sample
    );

    let raw_samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let max_val = int_sample_max_val(spec.bits_per_sample)?;
            reader
                .into_samples::<i32>()
                .filter_map(|s| s.ok())
                .map(|s| s as f32 / max_val)
                .collect()
        }
        hound::SampleFormat::Float => reader
            .into_samples::<f32>()
            .filter_map(|s| s.ok())
            .collect(),
    };

    let mono_samples: Vec<f32> = if spec.channels > 1 {
        let ch = spec.channels as usize;
        raw_samples
            .chunks(ch)
            .map(|frame| frame.iter().sum::<f32>() / ch as f32)
            .collect()
    } else {
        raw_samples
    };

    Ok((mono_samples, spec.sample_rate))
}

impl super::ApplicationCoordinator {
    /// Run WAV playback mode: read file, decode, print results, exit.
    pub(crate) async fn run_wav_playback(&self, wav_path: PathBuf) -> Result<()> {
        info!("WAV playback mode: {}", wav_path.display());

        let (mono_samples, native_rate) = read_wav_mono(&wav_path)?;
        info!("Read {} mono samples", mono_samples.len());

        // Resample to 12 kHz if needed
        let target_rate = pancetta_ft8::SAMPLE_RATE;
        let samples_12k: Vec<f32> = if native_rate != target_rate {
            info!("Resampling from {} Hz to {} Hz", native_rate, target_rate);
            resample_linear(&mono_samples, native_rate, target_rate)
        } else {
            mono_samples
        };

        let total_samples = samples_12k.len();
        let duration_s = total_samples as f64 / target_rate as f64;
        info!(
            "Audio ready: {} samples ({:.2}s) at {} Hz",
            total_samples, duration_s, target_rate
        );

        // Create FT8 decoder
        let ft8_config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(ft8_config)?;

        let window_size = pancetta_ft8::WINDOW_SAMPLES; // 151680 (12.64s @ 12 kHz)

        let mut all_decoded = Vec::new();
        let mut offset = 0usize;
        let step = window_size / 2;

        while offset + window_size <= total_samples {
            let window = &samples_12k[offset..offset + window_size];
            match decoder.decode_window(window) {
                Ok(messages) => {
                    for msg in &messages {
                        let freq_hz = msg.frequency_offset;
                        let snr = msg.snr_db;
                        let dt = msg.time_offset;
                        let text = &msg.text;

                        let slot_time = offset as f64 / target_rate as f64;
                        let mins = (slot_time / 60.0) as u32;
                        let secs = (slot_time % 60.0) as u32;
                        let conf = msg.confidence;
                        let ap = msg.ap_level;
                        println!(
                            "{:02}:{:02}  {:>+4.0} {:>6.1} {:>+5.1}  conf={:.2} ap={}  {}",
                            mins, secs, snr, freq_hz, dt, conf, ap, text
                        );
                    }
                    all_decoded.extend(messages);
                }
                Err(e) => {
                    debug!("Decode error at offset {}: {}", offset, e);
                }
            }
            offset += step;
        }

        println!(
            "\n--- Decoded {} messages from {} ---",
            all_decoded.len(),
            wav_path.display()
        );

        Ok(())
    }
}
```

Keep the existing `#[cfg(test)] mod tests` block (the three `int_sample_max_val` tests) below this, with the new `read_wav_mono_mixes_stereo_and_reports_native_rate` test added to it — add `use super::read_wav_mono;` to that block's imports alongside the existing `use super::int_sample_max_val;`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta --lib coordinator::wav_playback`
Expected: PASS (4 tests: the new one plus the 3 existing `int_sample_max_val` tests)

- [ ] **Step 5: Verify `--wav` behavior is unchanged**

Run: `cargo build -p pancetta && ./target/debug/pancetta --wav pancetta-ft8/tests/fixtures/wav/basicft8/170923_082000.wav`
Expected: same decode output style as before the refactor (WSJT-X-style lines + a `--- Decoded N messages from ... ---` summary)

- [ ] **Step 6: Commit**

```bash
git add pancetta/src/coordinator/wav_playback.rs
git commit -m "refactor(pancetta): extract read_wav_mono from run_wav_playback"
```

---

## Task 2: `replay.rs` — pure file-discovery and sample-loading functions

**Files:**
- Create: `pancetta/src/coordinator/replay.rs`
- Modify: `pancetta/src/coordinator/mod.rs:39` (add `mod replay;` between `mod remote_gateway;` and `mod restart_budget;`)

**Interfaces:**
- Consumes: `super::wav_playback::read_wav_mono(path: &Path) -> Result<(Vec<f32>, u32)>` (Task 1), `super::util::resample_linear(input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32>` (existing).
- Produces: `pub(crate) fn list_wav_files_sorted(dir: &Path) -> Result<Vec<PathBuf>>` and `pub(crate) fn load_replay_samples(dir: &Path, target_rate: u32) -> Result<Vec<f32>>`, both used by Task 3's `start_replay_pipeline`.

- [ ] **Step 1: Write the failing tests**

Create `pancetta/src/coordinator/replay.rs`:

```rust
use anyhow::Result;
use std::path::{Path, PathBuf};

use super::util::resample_linear;
use super::wav_playback::read_wav_mono;

/// List `.wav` files in `dir`, sorted by filename. Sequential-capture
/// filenames (`170923_082000.wav`, `170923_082015.wav`, ...) sort
/// chronologically this way; callers rely on that order to replay a
/// directory as a continuous recording.
pub(crate) fn list_wav_files_sorted(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        anyhow::bail!("--replay path is not a directory: {}", dir.display());
    }

    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("Failed to read replay directory {}: {}", dir.display(), e))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("wav"))
                .unwrap_or(false)
        })
        .collect();

    if files.is_empty() {
        anyhow::bail!("No .wav files found in replay directory: {}", dir.display());
    }

    files.sort();
    Ok(files)
}

/// Read every `.wav` file in `dir` (in filename order), mix each to mono,
/// resample each to `target_rate`, and concatenate into one continuous
/// sample stream -- as if the directory were one long recording.
pub(crate) fn load_replay_samples(dir: &Path, target_rate: u32) -> Result<Vec<f32>> {
    let files = list_wav_files_sorted(dir)?;

    let mut all_samples = Vec::new();
    for path in &files {
        let (mono, native_rate) = read_wav_mono(path)?;
        let resampled = if native_rate != target_rate {
            resample_linear(&mono, native_rate, target_rate)
        } else {
            mono
        };
        all_samples.extend(resampled);
    }

    Ok(all_samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_test_wav(path: &Path, sample_rate: u32, n_samples: usize) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..n_samples {
            let v = ((i % 100) as i16) - 50;
            writer.write_sample(v).unwrap();
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn list_wav_files_sorted_orders_by_filename_and_ignores_non_wav() {
        let dir = tempfile::tempdir().unwrap();
        write_test_wav(&dir.path().join("170923_082030.wav"), 12000, 10);
        write_test_wav(&dir.path().join("170923_082000.wav"), 12000, 10);
        write_test_wav(&dir.path().join("170923_082015.wav"), 12000, 10);
        std::fs::write(dir.path().join("notes.txt"), b"ignore me").unwrap();

        let files = list_wav_files_sorted(dir.path()).unwrap();
        let names: Vec<&str> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "170923_082000.wav",
                "170923_082015.wav",
                "170923_082030.wav",
            ]
        );
    }

    #[test]
    fn list_wav_files_sorted_rejects_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_wav_files_sorted(dir.path()).is_err());
    }

    #[test]
    fn list_wav_files_sorted_rejects_non_directory_path() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("not_a_dir.wav");
        write_test_wav(&file_path, 12000, 10);
        assert!(list_wav_files_sorted(&file_path).is_err());
    }

    #[test]
    fn load_replay_samples_concatenates_files_in_order_and_resamples() {
        let dir = tempfile::tempdir().unwrap();
        // Native rate 8000 Hz; request 12000 Hz target so resampling is exercised.
        write_test_wav(&dir.path().join("a_first.wav"), 8000, 80);
        write_test_wav(&dir.path().join("b_second.wav"), 8000, 80);

        let samples = load_replay_samples(dir.path(), 12000).unwrap();

        // 80 samples @ 8000 Hz resampled to 12000 Hz -> 120 samples each (linear
        // resampler truncates via integer division: 80 * 12000 / 8000 = 120).
        assert_eq!(samples.len(), 240);
    }
}
```

- [ ] **Step 2: Register the module**

In `pancetta/src/coordinator/mod.rs`, change:

```rust
mod remote_gateway;
mod restart_budget;
```

to:

```rust
mod remote_gateway;
mod replay;
mod restart_budget;
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p pancetta --lib coordinator::replay`
Expected: PASS (4 tests)

- [ ] **Step 4: Commit**

```bash
git add pancetta/src/coordinator/replay.rs pancetta/src/coordinator/mod.rs
git commit -m "feat(pancetta): add replay.rs file-discovery and sample-loading helpers"
```

---

## Task 3: `start_replay_pipeline` — feed loaded samples into the pipeline at real-time cadence

**Files:**
- Modify: `pancetta/src/coordinator/replay.rs` (add the async method)
- Modify: `pancetta/src/coordinator/audio.rs:67-125` (add the `else if` branch)

**Interfaces:**
- Consumes: `load_replay_samples(dir: &Path, target_rate: u32) -> Result<Vec<f32>>` (Task 2); `super::pipeline::forward_or_drop_async`, `super::pipeline::DECODE_FORWARD_TIMEOUT`, `super::pipeline::ForwardOutcome` (existing, already used by the stub branch in `audio.rs`); `self.config` (`Arc<RwLock<Config>>`, existing field, `.read().await` gives `audio.sample_rate: u32` and `audio.buffer_size: u32`); `self.shutdown_signal: Arc<AtomicBool>` (existing field).
- Produces: `pub(crate) async fn start_replay_pipeline(&mut self, replay_dir: PathBuf, audio_to_dsp_tx: crossbeam_channel::Sender<Vec<f32>>) -> Result<()>`, called from Task 4's CLI wiring via `audio.rs`.

- [ ] **Step 1: Add the async pacing method to `replay.rs`**

Append to `pancetta/src/coordinator/replay.rs` (after `load_replay_samples`, before the `#[cfg(test)]` block):

```rust
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::interval;
use tracing::{info, warn};

/// Grace period after the last replay chunk is fed before triggering
/// shutdown -- long enough for in-flight FT8 decode windows to finish,
/// short enough to keep `--replay` usable in CI and for VHS recordings.
const REPLAY_GRACE_PERIOD: Duration = Duration::from_secs(5);

impl super::ApplicationCoordinator {
    /// Feed every `.wav` file in `replay_dir` (in filename order) into the
    /// pipeline as if it were live audio: resampled to the configured audio
    /// sample rate, chunked to the configured buffer size, and paced in
    /// real time via the same `audio_to_dsp_tx` channel a real `cpal::Device`
    /// or `PANCETTA_STUB_AUDIO` would use. Once the directory is exhausted,
    /// waits [`REPLAY_GRACE_PERIOD`] then triggers the same graceful
    /// shutdown Ctrl+C uses, so `--replay` is a finite, self-terminating
    /// mode suitable for scripted demos and automated tests.
    pub(crate) async fn start_replay_pipeline(
        &mut self,
        replay_dir: PathBuf,
        audio_to_dsp_tx: crossbeam_channel::Sender<Vec<f32>>,
    ) -> Result<()> {
        self.audio_path_supervised = false;

        let config = self.config.read().await;
        let sample_rate = config.audio.sample_rate;
        let buffer_size = config.audio.buffer_size as usize;
        drop(config);

        info!(
            "Starting audio component in REPLAY mode: {}",
            replay_dir.display()
        );
        let samples = load_replay_samples(&replay_dir, sample_rate)?;
        info!(
            "Replay: loaded {} samples ({:.1}s) at {} Hz",
            samples.len(),
            samples.len() as f64 / sample_rate as f64,
            sample_rate
        );

        let shutdown = self.shutdown_signal.clone();
        let last_timestamp = self.last_audio_timestamp.clone();

        let handle = tokio::spawn(async move {
            let buffer_duration_ms = (buffer_size as f64 * 1000.0 / sample_rate as f64) as u64;
            let mut process_interval = interval(Duration::from_millis(buffer_duration_ms.max(5)));

            let mut offset = 0usize;
            while offset < samples.len() && !shutdown.load(Ordering::Acquire) {
                process_interval.tick().await;

                let end = (offset + buffer_size).min(samples.len());
                let chunk = samples[offset..end].to_vec();
                offset = end;

                last_timestamp.store(super::now_epoch_ms(), Ordering::Relaxed);

                match super::pipeline::forward_or_drop_async(
                    &audio_to_dsp_tx,
                    chunk,
                    super::pipeline::DECODE_FORWARD_TIMEOUT,
                )
                .await
                {
                    super::pipeline::ForwardOutcome::Sent => {}
                    super::pipeline::ForwardOutcome::Dropped => {
                        warn!("Replay: DSP stage not draining -- dropped one batch");
                    }
                    super::pipeline::ForwardOutcome::Disconnected => break,
                }
            }

            info!(
                "Replay complete -- waiting {:?} before shutdown",
                REPLAY_GRACE_PERIOD
            );
            tokio::time::sleep(REPLAY_GRACE_PERIOD).await;
            shutdown.store(true, Ordering::Release);
            info!("Replay: triggered shutdown");

            Ok(())
        });

        self.named_task_handles
            .push((crate::message_bus::ComponentId::Audio, handle));

        Ok(())
    }
}
```

- [ ] **Step 2: Add the `replay_path` branch to `audio.rs`**

In `pancetta/src/coordinator/audio.rs`, change the branching from:

```rust
        let use_stub = std::env::var("PANCETTA_STUB_AUDIO").is_ok();

        if use_stub {
```

to:

```rust
        let use_stub = std::env::var("PANCETTA_STUB_AUDIO").is_ok();

        if let Some(replay_dir) = self.replay_path.clone() {
            return self.start_replay_pipeline(replay_dir, audio_to_dsp_tx).await;
        }

        if use_stub {
```

(This keeps the existing `if use_stub { ... } else { ... real AudioManager ... }` below completely unchanged -- `replay_path` is checked first and returns early, so it never falls into either the stub or real branch. `self.replay_path` doesn't exist yet; it's added in Task 4.)

- [ ] **Step 3: Confirm it doesn't compile yet (expected -- `replay_path` field is Task 4)**

Run: `cargo build -p pancetta 2>&1 | grep replay_path`
Expected: an error naming `replay_path` as an unknown field on `ApplicationCoordinator` -- confirms the wiring in this task is correct and only the field/constructor plumbing (Task 4) is missing.

- [ ] **Step 4: Commit**

```bash
git add pancetta/src/coordinator/replay.rs pancetta/src/coordinator/audio.rs
git commit -m "feat(pancetta): add start_replay_pipeline real-time WAV-directory feeder"
```

---

## Task 4: CLI plumbing — `--replay <PATH>`

**Files:**
- Modify: `pancetta/src/main.rs` (Cli struct + `ApplicationCoordinator::new` call site)
- Modify: `pancetta/src/coordinator/mod.rs` (struct field + constructor parameter)

**Interfaces:**
- Produces: `Cli.replay: Option<PathBuf>`; `ApplicationCoordinator::new(..., replay_path: Option<PathBuf>, ...)`; `ApplicationCoordinator.replay_path: Option<PathBuf>` (read by Task 3's `audio.rs` branch).

- [ ] **Step 1: Add the CLI flag**

In `pancetta/src/main.rs`, add after the existing `wav` field (around line 106):

```rust
    /// WAV file to decode (enables playback mode — decodes and exits)
    #[arg(long, global = true)]
    wav: Option<PathBuf>,

    /// Directory of sequential WAV captures to replay through the full
    /// pipeline (audio → DSP → FT8 → QSO → TUI) at real-time cadence, as if
    /// it were live audio. Files are read in filename order. Exits on its
    /// own a few seconds after the last file is exhausted. Unlike --wav,
    /// this runs the complete pipeline (TUI, QSO engine, priority scoring),
    /// not just the decoder -- intended for demos and scripted recordings.
    #[arg(long, global = true)]
    replay: Option<PathBuf>,
```

- [ ] **Step 2: Thread it into the constructor call**

In `pancetta/src/main.rs`, change the `ApplicationCoordinator::new(...)` call (around line 368) from:

```rust
    let coordinator = ApplicationCoordinator::new(
        config,
        cli.audio_device,
        cli.no_audio,
        cli.headless,
        cli.metrics,
        cli.metrics_port,
        cli.wav,
        cli.test_tx,
        cli.test_tx_offset,
        shutdown.clone(),
        config_warnings,
    )
    .await?;
```

to:

```rust
    let coordinator = ApplicationCoordinator::new(
        config,
        cli.audio_device,
        cli.no_audio,
        cli.headless,
        cli.metrics,
        cli.metrics_port,
        cli.wav,
        cli.replay,
        cli.test_tx,
        cli.test_tx_offset,
        shutdown.clone(),
        config_warnings,
    )
    .await?;
```

- [ ] **Step 3: Add the field and constructor parameter in `coordinator/mod.rs`**

In `pancetta/src/coordinator/mod.rs`, change the struct field (around line 684-685) from:

```rust
    /// WAV file playback path (if set, runs in playback mode)
    wav_path: Option<PathBuf>,
```

to:

```rust
    /// WAV file playback path (if set, runs in playback mode)
    wav_path: Option<PathBuf>,

    /// Directory of sequential WAV captures to replay through the full
    /// pipeline at real-time cadence (see `--replay` in `main.rs`). Checked
    /// by `start_audio_pipeline` (`audio.rs`) ahead of the stub/real-device
    /// branches.
    pub(crate) replay_path: Option<PathBuf>,
```

Then change the constructor signature (around line 1279-1291) from:

```rust
    pub async fn new(
        config: Config,
        audio_device: Option<String>,
        no_audio: bool,
        headless: bool,
        enable_metrics: bool,
        metrics_port: u16,
        wav_path: Option<PathBuf>,
        test_tx: Option<String>,
        test_tx_offset: f64,
        shutdown_signal: Arc<AtomicBool>,
        config_warnings: Vec<String>,
    ) -> Result<Self> {
```

to:

```rust
    pub async fn new(
        config: Config,
        audio_device: Option<String>,
        no_audio: bool,
        headless: bool,
        enable_metrics: bool,
        metrics_port: u16,
        wav_path: Option<PathBuf>,
        replay_path: Option<PathBuf>,
        test_tx: Option<String>,
        test_tx_offset: f64,
        shutdown_signal: Arc<AtomicBool>,
        config_warnings: Vec<String>,
    ) -> Result<Self> {
```

Finally, find where `wav_path` is placed into the `coordinator` struct literal near the end of `new()` (search for `wav_path,` inside the `Ok(Self { ... })` / `let coordinator = Self { ... }` construction) and add `replay_path,` immediately after it, e.g.:

```rust
            wav_path,
            replay_path,
```

- [ ] **Step 4: Fix the other call site(s), if any**

Run: `cargo build -p pancetta 2>&1 | head -60`

If any other call to `ApplicationCoordinator::new` exists (e.g. in an integration test under `pancetta/tests/`), add `None` (or the relevant `Option<PathBuf>`) as the new argument in the same position. Repeat build until it succeeds.

- [ ] **Step 5: Verify it builds and `--help` documents the new flag**

Run: `cargo build -p pancetta && ./target/debug/pancetta --help | grep -A2 -- --replay`
Expected: the `--replay` flag and its help text appear.

- [ ] **Step 6: Run the full workspace test suite**

Run: `cargo test --workspace --features transmit`
Expected: PASS, no regressions.

- [ ] **Step 7: Commit**

```bash
git add pancetta/src/main.rs pancetta/src/coordinator/mod.rs
git commit -m "feat(pancetta): wire --replay <dir> CLI flag through to the coordinator"
```

---

## Task 5: Integration test for `--replay`

**Files:**
- Create: `pancetta/tests/replay_mode.rs`

**Interfaces:**
- Consumes: the built `pancetta` binary via `assert_cmd::Command::cargo_bin("pancetta")` (existing pattern, see `pancetta/src/main.rs`'s `test_cli_help`/`test_cli_version`/`test_cli_test_audio_list_runs`).

- [ ] **Step 1: Write the failing test**

Create `pancetta/tests/replay_mode.rs`:

```rust
//! `--replay <dir>` integration test: points the real binary at a tiny
//! synthetic WAV directory and confirms it runs the full pipeline and
//! exits cleanly on its own once the input is exhausted (see
//! `REPLAY_GRACE_PERIOD` in `coordinator/replay.rs`).

use assert_cmd::Command;
use std::time::Duration;

fn write_short_wav(path: &std::path::Path, sample_rate: u32, seconds: f32) {
    let n_samples = (sample_rate as f32 * seconds) as usize;
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..n_samples {
        let t = i as f32 / sample_rate as f32;
        let v = (0.2 * (2.0 * std::f32::consts::PI * 440.0 * t).sin() * i16::MAX as f32) as i16;
        writer.write_sample(v).unwrap();
    }
    writer.finalize().unwrap();
}

#[test]
fn replay_mode_runs_full_pipeline_and_exits_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    write_short_wav(&dir.path().join("a_first.wav"), 12000, 1.0);
    write_short_wav(&dir.path().join("b_second.wav"), 12000, 1.0);

    let mut cmd = Command::cargo_bin("pancetta").unwrap();
    cmd.args(["--headless", "--replay"])
        .arg(dir.path())
        .timeout(Duration::from_secs(30));

    cmd.assert().success();
}

#[test]
fn replay_mode_rejects_empty_directory() {
    let dir = tempfile::tempdir().unwrap();

    let mut cmd = Command::cargo_bin("pancetta").unwrap();
    cmd.args(["--headless", "--replay"])
        .arg(dir.path())
        .timeout(Duration::from_secs(15));

    cmd.assert().failure();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta --test replay_mode`
Expected: FAIL (binary doesn't yet exit on its own within the timeout, or the empty-directory case doesn't yet propagate as a process failure) -- confirms the test exercises real behavior rather than passing vacuously.

Note: if this unexpectedly passes already, check that `start_replay_pipeline`'s error return (Task 3, `load_replay_samples` failing on an empty directory) actually surfaces as a non-zero exit code from `main()` -- `main.rs`'s top-level error handling (the `match result { Err(e) => ... }` block seen around line 393) should already turn a returned `Err` into a non-zero exit; if it doesn't, that's a real gap to fix as part of this task, not a test to weaken.

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p pancetta --test replay_mode`
Expected: PASS (2 tests). The success case should finish in roughly 1s (audio) + 5s (`REPLAY_GRACE_PERIOD`) + process startup overhead -- comfortably inside the 30s timeout.

- [ ] **Step 4: Commit**

```bash
git add pancetta/tests/replay_mode.rs
git commit -m "test(pancetta): add --replay integration test"
```

---

## Task 6: Demo WAV assets + VHS tape script

**Files:**
- Create: `assets/demo-wav/170923_082000.wav`, `assets/demo-wav/170923_082015.wav`, `assets/demo-wav/170923_082030.wav`, `assets/demo-wav/170923_082045.wav` (copies)
- Create: `assets/demo-wav/README.md`
- Create: `.tapes/demo.tape`
- Create: `.tapes/README.md`

**Interfaces:**
- Consumes: the `pancetta --replay assets/demo-wav --headless=false` binary built in Task 4.
- Produces: `assets/demo.gif` (the hero GIF referenced by Task 9's README rewrite).

- [ ] **Step 1: Copy the demo WAV sequence**

```bash
mkdir -p assets/demo-wav
cp pancetta-ft8/tests/fixtures/wav/basicft8/170923_082000.wav assets/demo-wav/
cp pancetta-ft8/tests/fixtures/wav/basicft8/170923_082015.wav assets/demo-wav/
cp pancetta-ft8/tests/fixtures/wav/basicft8/170923_082030.wav assets/demo-wav/
cp pancetta-ft8/tests/fixtures/wav/basicft8/170923_082045.wav assets/demo-wav/
```

- [ ] **Step 2: Document provenance**

Create `assets/demo-wav/README.md`:

```markdown
# Demo WAV sequence

Four consecutive 15-second FT8 captures (2017-09-23, 08:20:00–08:20:45 UTC),
copied from `pancetta-ft8/tests/fixtures/wav/basicft8/`. Kept as a separate
copy here (rather than referencing the test fixtures directly) so the demo
recording doesn't change if the decoder test corpus does.

Used as input to `pancetta --replay assets/demo-wav` when recording the
README's demo GIFs (see `.tapes/`).
```

- [ ] **Step 3: Install VHS**

Run: `brew install vhs` (macOS; see https://github.com/charmbracelet/vhs for other platforms)

If VHS cannot be installed in the current environment, stop here and hand off: commit the `.tape` script from Step 4 below, note in the PR description that the GIF still needs to be rendered once (by the maintainer, or by CI once Task 8's workflow lands), and skip to Task 7.

- [ ] **Step 4: Write the tape script**

Create `.tapes/demo.tape`:

```
# Hero demo GIF for the README. Run from the repo root:
#   vhs .tapes/demo.tape
# Produces assets/demo.gif.

Output assets/demo.gif

Set Shell "bash"
Set FontSize 16
Set Width 1200
Set Height 700
Set Padding 20
Set Theme "Dracula"

Type "cargo run --release -p pancetta -- --replay assets/demo-wav"
Enter

# Give the pipeline a moment to spin up and start decoding.
Sleep 3s
Wait+Screen /DE/ 20s

# Let a few decode cycles and priority scoring play out on screen.
Sleep 8s

# Hold on the final state before the tape ends (the process exits on its
# own ~5s after the replay directory is exhausted -- see
# REPLAY_GRACE_PERIOD in coordinator/replay.rs).
Sleep 5s
```

Note: adjust the `Wait+Screen` pattern once you've seen a real run's on-screen text (it should match something the TUI actually prints once decoding starts, e.g. a station callsign or "DE" from a decoded CQ) -- the placeholder `/DE/` above wasn't pixel-verified against a live run because VHS isn't available in every environment.

Create `.tapes/README.md`:

```markdown
# Demo recordings

`.tape` scripts for [VHS](https://github.com/charmbracelet/vhs). To
re-render after a TUI change:

```bash
brew install vhs   # once
vhs .tapes/demo.tape
```

Output lands in `assets/`. CI regenerates these automatically on TUI
changes (see `.github/workflows/demo-assets.yml`) -- these are not required
to be re-run by hand for every PR.
```

- [ ] **Step 5: Record and inspect the GIF**

Run: `cd /path/to/repo && vhs .tapes/demo.tape`
Expected: `assets/demo.gif` is created. Open it and confirm it shows: the command starting, decodes appearing, and a QSO/priority-scoring view visible before the recording ends. Adjust the `Sleep`/`Wait+Screen` timings in `.tapes/demo.tape` and re-run until it does, then re-run once more to confirm the final version.

- [ ] **Step 6: Commit**

```bash
git add assets/demo-wav .tapes assets/demo.gif
git commit -m "feat: add --replay demo WAV assets and VHS hero-GIF tape"
```

(If Step 3/5 couldn't run in this environment, omit `assets/demo.gif` from this commit and note in the commit message that it's pending a render pass.)

---

## Task 7: Screenshots and per-feature GIFs

**Files:**
- Create: `assets/screenshot-waterfall.png`, `assets/screenshot-priority.png`, `assets/screenshot-qso.png` (or however many distinct TUI views are worth showing -- see Step 1)
- Create: `assets/pskreporter.png`
- Create: `.tapes/feature-priority.tape`, `.tapes/feature-multitx.tape` (or fewer/more, per Step 2)

**Interfaces:**
- Consumes: the same `pancetta --replay assets/demo-wav` binary as Task 6.

- [ ] **Step 1: Capture static screenshots**

Run `pancetta --replay assets/demo-wav` in a real terminal (or `vhs` with an `Output screenshot.png` tape) and capture 2-4 distinct TUI activity views (per `docs/DECISIONS/` or `wiki/pages/tui.md` for what the activity views are called) -- at minimum, the waterfall/decode view and a view showing priority scores. Save as PNG under `assets/`.

- [ ] **Step 2: Record 1-2 short per-feature GIFs**

Following the same tape-script pattern as `.tapes/demo.tape` (Task 6), record one GIF specifically showing priority-based station selection (a decoded CQ getting a visible priority score, then being worked) and, if `--features transmit` multi-stream TX is visually distinguishable in the TUI, one showing two simultaneous TX streams. Keep each to 5-10 seconds per the spec's per-feature-GIF guidance.

- [ ] **Step 3: Capture the PSKReporter proof screenshot**

This one is not reproducible from `--replay` (it requires the existing real on-air PSKReporter spot history for K5ARH's FTdx10 validation runs, per the design spec). Take a screenshot of https://pskreporter.info filtered to K5ARH showing real spots and save as `assets/pskreporter.png`. This step needs Tony (real account/session, not something to script) -- flag it as a manual follow-up if it can't be done in this session.

- [ ] **Step 4: Commit**

```bash
git add assets/*.png .tapes/feature-*.tape
git commit -m "feat: add TUI screenshots and per-feature demo GIFs"
```

---

## Task 8: CI regeneration workflow

**Files:**
- Create: `.github/workflows/demo-assets.yml`

**Interfaces:**
- Consumes: `.tapes/*.tape` (Task 6, 7), `charmbracelet/vhs-action`.

- [ ] **Step 1: Write the workflow**

Create `.github/workflows/demo-assets.yml`:

```yaml
name: Regenerate demo assets

on:
  pull_request:
    paths:
      - "pancetta-tui/**"
      - ".tapes/**"
      - "assets/demo-wav/**"

jobs:
  vhs:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable

      - name: Build pancetta
        run: cargo build --release -p pancetta

      - name: Render demo tape
        uses: charmbracelet/vhs-action@v2
        with:
          path: ".tapes/demo.tape"

      - name: Check for changed assets
        id: diff
        run: |
          if ! git diff --quiet -- assets/demo.gif; then
            echo "changed=true" >> "$GITHUB_OUTPUT"
          fi

      - name: Commit updated GIF
        if: steps.diff.outputs.changed == 'true'
        uses: stefanzweifel/git-auto-commit-action@v5
        with:
          commit_message: "chore: regenerate demo.gif [skip ci]"
          file_pattern: assets/demo.gif
```

- [ ] **Step 2: Verify the workflow is syntactically valid**

Run: `cat .github/workflows/demo-assets.yml | python3 -c "import yaml,sys; yaml.safe_load(sys.stdin)"`
Expected: no output (valid YAML), exit code 0.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/demo-assets.yml
git commit -m "ci: regenerate demo.gif automatically on TUI changes"
```

Note: this workflow needs a real PR (not this local branch) to prove it actually fires and commits correctly -- verify it after this branch's PR is open, as part of normal CI observation, not as a local test step.

---

## Task 9: README rewrite

**Files:**
- Create: `docs/TROUBLESHOOTING.md`
- Create: `docs/PROVENANCE.md`
- Modify: `README.md`

**Interfaces:**
- Consumes: `assets/demo.gif`, `assets/screenshot-*.png`, `assets/pskreporter.png` (Tasks 6-7), the logo mark chosen from the exploration canvas (spec, Phase 2 -- pick before this task starts).

- [ ] **Step 1: Move the troubleshooting section out**

Create `docs/TROUBLESHOOTING.md` containing the current README's "Troubleshooting" section verbatim (the four `###` subsections: "Audio init failed", "No decodes appear", "Call X failed: duplicate QSO", "rigctld won't connect") plus a one-line header (`# Troubleshooting`) and a back-link to the README.

- [ ] **Step 2: Move the provenance essay out**

Create `docs/PROVENANCE.md` containing the current README's "Provenance & clean-room methodology" section verbatim, plus a `# Provenance & Clean-Room Methodology` header and a back-link to the README.

- [ ] **Step 3: Rewrite `README.md`**

Replace `README.md` end-to-end with the structure from the design spec (Phase 2):

1. Logo/wordmark (from the chosen exploration-canvas mark) + tagline + "K5ARH" attribution line.
2. Minimal badges: CI status, license. (Do not add a crates.io badge -- pancetta isn't published to crates.io.)
3. Hero GIF (`assets/demo.gif`), placed within the first ~20 words of body text per the spec's research findings.
4. Pain-point pitch: running FT8 seriously today means WSJT-X + a separate logger + GridTracker + a cluster client, alt-tabbed; pancetta is one binary, one terminal.
5. "Why pancetta" bullets: the +11.6% decode benchmark (link to `docs/decoder-comparison.md`), the priority engine, multi-stream TX, headless/Pi-class operation, `pancetta doctor`.
6. Control-operator/autonomy framing: pancetta's autonomous operator always has a present, interruptible control operator (`Shift+Q` emergency stop, the fail-closed TX-arm gate, drop-stale-TX) -- link `docs/fcc-part97-compliance.md`.
7. PSKReporter screenshot (`assets/pskreporter.png`) as on-air proof.
8. Compressed quick start (condense the existing 4-step Quick Start -- keep the exact commands, drop the surrounding prose explaining each step to a sentence or less).
9. "Why not (yet)" honesty section: pre-1.0 status, which crates are stubs/scaffolded (per `AGENTS.md`'s workspace table), platforms not yet validated.
10. Documentation links section (existing list, add `docs/TROUBLESHOOTING.md` and `docs/PROVENANCE.md`).
11. Tightened Acknowledgments (keep attribution to K1JT/K9AN, YL3JG, WSJT-X; drop the "What's specifically novel" paragraph -- that content duplicates `FEATURES.md`).
12. License section (unchanged).

Do not include the full workspace crate table -- replace it with one sentence and a link to `docs/ARCHITECTURE.md`.

Target ~1,200 words total (spec, Phase 2). Prerequisites table, per-command CLI reference table, and decode-effort-control table may stay (they're compact and reference material, not prose bulk) -- the word-count target is about cutting prose, not deleting every table.

- [ ] **Step 4: Verify all internal links resolve**

Run:
```bash
grep -oE '\[.*\]\((docs/[^)]+|\./[^)]+|[A-Z_-]+\.md)\)' README.md | grep -oE '\((.*)\)' | tr -d '()' | while read -r f; do
  [ -f "$f" ] || echo "BROKEN LINK: $f"
done
```
Expected: no output (every linked file exists).

- [ ] **Step 5: Verify word count is in range**

Run: `sed 's/```[^`]*```//g' README.md | wc -w`
Expected: roughly 1,000-1,500 (spec target ~1,200; this is a sanity check, not a hard gate -- use judgment if a table pushes it slightly over).

- [ ] **Step 6: Commit**

```bash
git add README.md docs/TROUBLESHOOTING.md docs/PROVENANCE.md
git commit -m "docs: rewrite README around demo assets, push detail into docs/"
```

---

## Self-Review

**1. Spec coverage:**
- Phase 1 replay mode: Tasks 1-5. ✅
- Phase 1 visual assets (VHS tooling, hero GIF, per-feature GIFs, screenshots, PSKReporter capture, CI regeneration): Tasks 6-8. ✅
- Phase 2 README rewrite (structure, displaced content, logo): Task 9. ✅
- Phase 3 (distribution): intentionally *not* broken into TDD tasks here -- the spec says "decide item by item," and turning `cargo-dist` setup, an `awesome-hamradio` PR, groups.io posting, and Mastodon posting into speculative step-by-step tasks now would be planning work nobody asked for yet. See "Phase 3 — Next Steps (not yet planned)" below instead.

**2. Placeholder scan:** No TBD/TODO markers. Task 6 Step 3 and Task 7 Step 3 both have explicit conditional fallback instructions (VHS unavailable; PSKReporter needs Tony's real session) rather than hand-waving past an environment limitation -- these are real constraints of the execution environment, not deferred design decisions.

**3. Type consistency:** `read_wav_mono(path: &Path) -> Result<(Vec<f32>, u32)>` (Task 1) is called identically in Task 2's `load_replay_samples`. `list_wav_files_sorted(dir: &Path) -> Result<Vec<PathBuf>>` and `load_replay_samples(dir: &Path, target_rate: u32) -> Result<Vec<f32>>` (Task 2) match their use in Task 3's `start_replay_pipeline`. The `replay_path: Option<PathBuf>` field (Task 4) matches its check in Task 3's `audio.rs` branch (`self.replay_path.clone()`) and its constructor parameter position (added right after `wav_path` in both the `new()` signature and the `main.rs` call site, keeping every later positional argument's index consistent between the two edits).

---

Plan complete and saved to `docs/superpowers/plans/2026-08-20-readme-visual-identity-plan.md`.

## Phase 3 — Next Steps (not yet planned)

Per the spec, these are decided individually, not as a bundle. Each would get its own short design-and-plan pass when picked:

- **`cargo-dist` prebuilt release binaries** — biggest adoption lever for non-Rust-fluent hams. Needs its own scoping pass (which platforms, code-signing on macOS, whether Windows needs the hamlib DLL bundled).
- **PR to `DD5HT/awesome-hamradio`** — small, no plan needed, just do it once the README is live.
- **groups.io group + posts** — a Tony action (account/identity), not an engineering task.
- **Mastodon presence** — same.
- **Headless-Pi demo for KM4ACK/Temporarily Offline** — could reuse this plan's VHS assets, or need real Pi hardware footage; worth a short follow-up conversation once Phase 1/2 land.
