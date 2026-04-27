//! DSP component startup.
//!
//! Reads 48 kHz audio batches from the audio relay, decimates to 12 kHz
//! mono via simple subsampling (FT8 sits well below the 6 kHz Nyquist),
//! and emits FT8-aligned 12.64-second windows aligned to UTC slot
//! boundaries (:00/:15/:30/:45 + 13 s for headroom).
//!
//! In parallel, computes a 1 Hz waterfall via a 2048-point Hann-windowed
//! FFT and an audio-level RMS meter — both forwarded to the TUI.
//!
//! Validation captures: when `WavRecorder::new` succeeds, every full
//! window is written to `~/.pancetta/recordings/ft8_*.wav` for off-line
//! decoder comparison. Disk usage is capped at 10 GB; oldest files are
//! evicted when the cap is hit.

use anyhow::Result;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::{debug, info, span, warn, Level};

use crate::message_bus::ComponentId;

/// Maximum total disk space for WAV recordings (bytes).
const WAV_RECORDING_MAX_BYTES: u64 = 10 * 1024 * 1024 * 1024; // 10 GB

/// Manage WAV recording of 12kHz mono FT8 windows for decoder validation.
///
/// Writes one WAV file per FT8 window (12.64 seconds @ 12kHz mono i16).
/// Each file is ~303 KB.  When total usage exceeds [`WAV_RECORDING_MAX_BYTES`],
/// the oldest files are deleted to make room.
struct WavRecorder {
    dir: std::path::PathBuf,
    total_bytes: u64,
}

impl WavRecorder {
    fn new() -> Result<Self> {
        let dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".pancetta")
            .join("recordings");
        std::fs::create_dir_all(&dir)?;

        // Sum existing recording sizes
        let total_bytes = Self::dir_size(&dir);
        info!(
            "WAV recorder: dir={}, existing={:.1} MB, cap={:.1} GB",
            dir.display(),
            total_bytes as f64 / 1_048_576.0,
            WAV_RECORDING_MAX_BYTES as f64 / 1_073_741_824.0,
        );
        Ok(Self { dir, total_bytes })
    }

    fn dir_size(dir: &std::path::Path) -> u64 {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0)
    }

    /// Write a 12kHz mono f32 window to a timestamped WAV file.
    fn write_window(&mut self, samples: &[f32], timestamp: &chrono::DateTime<chrono::Utc>) {
        // Enforce cap by deleting oldest files
        while self.total_bytes >= WAV_RECORDING_MAX_BYTES {
            if !self.delete_oldest() {
                warn!("WAV recorder: cannot free space, skipping write");
                return;
            }
        }

        let filename = format!("ft8_{}.wav", timestamp.format("%Y%m%d_%H%M%S"));
        let path = self.dir.join(&filename);

        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 12000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        match hound::WavWriter::create(&path, spec) {
            Ok(mut writer) => {
                for &s in samples {
                    let _ = writer.write_sample((s * i16::MAX as f32) as i16);
                }
                if let Err(e) = writer.finalize() {
                    warn!("WAV recorder: finalize error: {}", e);
                    return;
                }
                let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                self.total_bytes += file_size;
                debug!(
                    "WAV recorded: {} ({:.0} KB, total {:.1} MB)",
                    filename,
                    file_size as f64 / 1024.0,
                    self.total_bytes as f64 / 1_048_576.0,
                );
            }
            Err(e) => {
                warn!("WAV recorder: create error: {}", e);
            }
        }
    }

    /// Delete the oldest WAV file to free space. Returns false if no files remain.
    fn delete_oldest(&mut self) -> bool {
        let mut entries: Vec<_> = std::fs::read_dir(&self.dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map_or(false, |ext| ext == "wav"))
            .collect();

        entries.sort_by_key(|e| e.file_name());

        if let Some(oldest) = entries.first() {
            let size = oldest.metadata().map(|m| m.len()).unwrap_or(0);
            if std::fs::remove_file(oldest.path()).is_ok() {
                self.total_bytes = self.total_bytes.saturating_sub(size);
                info!(
                    "WAV recorder: deleted {} ({:.0} KB) to free space",
                    oldest.file_name().to_string_lossy(),
                    size as f64 / 1024.0,
                );
                return true;
            }
        }
        false
    }
}

impl super::ApplicationCoordinator {
    /// Start DSP pipeline with point-to-point channels
    ///
    /// Simple direct pipeline: resample 48kHz->12kHz on a dedicated thread,
    /// accumulate FT8-sized windows, and send to the decoder.
    pub(crate) async fn start_dsp_pipeline(
        &mut self,
        audio_rx: crossbeam_channel::Receiver<Vec<f32>>,
        dsp_to_ft8_tx: crossbeam_channel::Sender<Vec<f32>>,
        live_waterfall_tx: crossbeam_channel::Sender<Vec<Vec<f32>>>,
        audio_level_tx: crossbeam_channel::Sender<f32>,
        health_dsp_windows: Arc<std::sync::atomic::AtomicU64>,
        health_last_rms: Arc<std::sync::atomic::AtomicU32>,
    ) -> Result<()> {
        let span = span!(Level::INFO, "start_dsp");
        let _enter = span.enter();

        info!("Starting DSP component");

        let shutdown = self.shutdown_signal.clone();
        let message_count = self.message_count.clone();

        let config = self.config.read().await;
        let input_rate = config.audio.sample_rate;
        let _input_channels = config.audio.input_channels as u16;
        drop(config);

        let handle = tokio::task::spawn_blocking(move || {
            // FT8 timing: transmissions start at 0/15/30/45 second marks.
            // We need 12.64 seconds of audio at 12kHz = 151,680 samples per window.
            // We align window capture to UTC 15-second boundaries for best decode.
            let decimation_factor = (input_rate / 12000) as usize;
            if input_rate as usize != decimation_factor * 12000 {
                return Err(anyhow::anyhow!(
                    "Audio sample rate {} Hz is not evenly divisible by 12000 Hz. \
                     Supported rates: 12000, 24000, 48000, 96000.",
                    input_rate
                ));
            }
            const FT8_SAMPLE_RATE: usize = 12000;
            const FT8_WINDOW_SECONDS: f64 = 12.64;
            const FT8_WINDOW_SAMPLES: usize =
                (FT8_SAMPLE_RATE as f64 * FT8_WINDOW_SECONDS) as usize; // 151,680

            let mut decimate_counter: usize = 0;

            let mut ft8_buffer: Vec<f32> = Vec::with_capacity(FT8_WINDOW_SAMPLES * 2);
            let mut window_count: u64 = 0;
            let mut batch_count: u64 = 0;
            let _waiting_for_boundary = true;

            // WAV recorder for decoder validation
            let mut wav_recorder = match WavRecorder::new() {
                Ok(r) => Some(r),
                Err(e) => {
                    warn!("WAV recorder disabled: {}", e);
                    None
                }
            };

            // Live waterfall state
            let mut last_live_wf_samples: usize = 0;
            let mut live_wf_planner = rustfft::FftPlanner::<f32>::new();
            let live_wf_fft = live_wf_planner.plan_fft_forward(2048);

            info!(
                "DSP: {}Hz -> {}Hz mono (decimate {}:1, subsample), window={}",
                input_rate, FT8_SAMPLE_RATE, decimation_factor, FT8_WINDOW_SAMPLES
            );

            // Continuously capture audio -- don't wait for boundaries.
            // FT8 has both even (0/30s) and odd (15/45s) time slots.
            // We send overlapping windows: one at each 15-second mark.
            // The decoder handles time alignment internally via Costas sync.
            // Schedule decode at 13s past the slot start (message ends at 12.64s).
            // Slots start at :00/:15/:30/:45, so decode at :13/:28/:43/:58.
            let mut next_window_time =
                pancetta_core::slot::next_phase(chrono::Utc::now(), chrono::Duration::seconds(13));
            info!(
                "DSP: first window at {}",
                next_window_time.format("%H:%M:%S")
            );

            while !shutdown.load(Ordering::Acquire) {
                match audio_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(samples) => {
                        message_count.fetch_add(1, Ordering::Relaxed);
                        batch_count += 1;

                        // Extract left channel from interleaved stereo.
                        // cpal delivers interleaved [L, R, L, R, ...] where
                        // the right channel is near-silent on the FTdx10 USB codec.
                        let mono: Vec<f32> = if _input_channels > 1 {
                            samples
                                .chunks(_input_channels as usize)
                                .map(|ch| ch[0])
                                .collect()
                        } else {
                            samples
                        };

                        // One-time diagnostic: log first batch stats
                        if batch_count == 1 {
                            let rms = (mono.iter().map(|s| s * s).sum::<f32>() / mono.len() as f32)
                                .sqrt();
                            info!(
                                "DSP first batch: {} samples, RMS={:.6}, first 5 values: {:?}",
                                mono.len(),
                                rms,
                                &mono[..5.min(mono.len())]
                            );
                        }

                        // Decimate by taking every Nth sample (simple subsampling).
                        // FT8 signals occupy 0–3 kHz, well below the 6 kHz Nyquist
                        // of the 12 kHz target rate, so anti-alias filtering is
                        // unnecessary and the previous 65-tap FIR was attenuating
                        // signals (ft8_lib decoded 86 from naive decimation vs 1
                        // from the FIR output on the same live audio).
                        for &sample in &mono {
                            decimate_counter += 1;
                            if decimate_counter >= decimation_factor {
                                decimate_counter = 0;
                                ft8_buffer.push(sample);
                            }
                        }

                        // Live waterfall: emit one spectrum row per second using rustfft.
                        // We keep a simple sample counter to trigger every ~1 second.
                        const LIVE_WF_INTERVAL: usize = 12000; // 1 second at 12kHz
                        const LIVE_WF_FFT_SIZE: usize = 2048;
                        if ft8_buffer.len() >= LIVE_WF_FFT_SIZE {
                            let samples_since_last =
                                ft8_buffer.len().saturating_sub(last_live_wf_samples);
                            if samples_since_last >= LIVE_WF_INTERVAL {
                                last_live_wf_samples = ft8_buffer.len();

                                let wf_start = ft8_buffer.len() - LIVE_WF_FFT_SIZE;
                                let wf_slice = &ft8_buffer[wf_start..];

                                // Use rustfft for a quick spectrum
                                let mut input: Vec<rustfft::num_complex::Complex<f32>> = wf_slice
                                    .iter()
                                    .enumerate()
                                    .map(|(i, &s)| {
                                        // Apply Hann window
                                        let w = 0.5
                                            * (1.0
                                                - (2.0 * std::f32::consts::PI * i as f32
                                                    / LIVE_WF_FFT_SIZE as f32)
                                                    .cos());
                                        rustfft::num_complex::Complex::new(s * w, 0.0)
                                    })
                                    .collect();
                                live_wf_fft.process(&mut input);

                                // Extract 0-3000 Hz bins and convert to dB
                                let freq_res = FT8_SAMPLE_RATE as f32 / LIVE_WF_FFT_SIZE as f32;
                                let bin_end = (3000.0 / freq_res) as usize;
                                let bin_end = bin_end.min(LIVE_WF_FFT_SIZE / 2);

                                let powers: Vec<f32> = (0..=bin_end)
                                    .map(|i| {
                                        10.0 * (input[i].norm_sqr() / LIVE_WF_FFT_SIZE as f32
                                            + 1e-12)
                                            .log10()
                                    })
                                    .collect();

                                let min_p = powers.iter().cloned().fold(f32::MAX, f32::min);
                                let max_p = powers.iter().cloned().fold(f32::MIN, f32::max);
                                let range = (max_p - min_p).max(1.0);
                                let row: Vec<f32> =
                                    powers.iter().map(|&p| (p - min_p) / range).collect();
                                let _ = live_waterfall_tx.try_send(vec![row]);

                                // Send audio level (RMS) to TUI — once per second
                                let rms = (wf_slice.iter().map(|s| s * s).sum::<f32>()
                                    / wf_slice.len() as f32)
                                    .sqrt();
                                let _ = audio_level_tx.try_send(rms);
                            }
                        }

                        // Decode as early as possible after the FT8 message completes.
                        // FT8 messages are 12.64s long, starting at :00/:15/:30/:45.
                        // We trigger at 13s past the slot start (0.36s after message
                        // end) to maximize response time for QSO management.
                        const IDEAL_SAMPLES: usize = FT8_SAMPLE_RATE * 15; // 180000
                        let now = chrono::Utc::now();
                        if ft8_buffer.len() >= FT8_WINDOW_SAMPLES && now >= next_window_time {
                            let send_len = ft8_buffer.len().min(IDEAL_SAMPLES);
                            let start = ft8_buffer.len() - send_len;
                            let window: Vec<f32> = ft8_buffer[start..].to_vec();
                            // Keep overlap for next window — retain enough for the
                            // full 15s ideal window at the next boundary
                            let keep = IDEAL_SAMPLES;
                            if ft8_buffer.len() > keep {
                                ft8_buffer.drain(..ft8_buffer.len() - keep);
                                last_live_wf_samples = ft8_buffer.len();
                            }
                            window_count += 1;
                            let rms = (window.iter().map(|s| s * s).sum::<f32>()
                                / window.len() as f32)
                                .sqrt();
                            info!(
                                "DSP: FT8 window #{} (RMS={:.4}) at {}",
                                window_count,
                                rms,
                                now.format("%H:%M:%S.%3f")
                            );

                            // Record window to WAV for decoder validation
                            if let Some(ref mut recorder) = wav_recorder {
                                recorder.write_window(&window, &now);
                            }

                            if dsp_to_ft8_tx.send(window).is_err() {
                                info!("DSP: FT8 channel closed");
                                return Ok(());
                            }
                            health_dsp_windows.fetch_add(1, Ordering::Relaxed);
                            health_last_rms.store(rms.to_bits(), Ordering::Relaxed);
                            // Schedule next decode at 13s into the next slot
                            next_window_time = pancetta_core::slot::next_phase(
                                chrono::Utc::now(),
                                chrono::Duration::seconds(13),
                            );
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        info!(
                            "DSP: audio channel disconnected after {} batches",
                            batch_count
                        );
                        break;
                    }
                }
            }

            info!(
                "DSP stopped ({} batches, {} windows sent)",
                batch_count, window_count
            );
            Ok(())
        });

        self.named_task_handles.push((ComponentId::Dsp, handle));
        info!("DSP component started");
        Ok(())
    }
}
