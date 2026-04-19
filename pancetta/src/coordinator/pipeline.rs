use anyhow::Result;
use geographiclib_rs::InverseGeodesic;
use pancetta_audio::{AudioManager, AudioManagerConfig};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::interval;
use tracing::{debug, error, info, span, warn, Level};

use crate::message_bus::{ComponentId, ComponentMessage, MessageType};

impl super::ApplicationCoordinator {
    /// Start the core pipeline with proper point-to-point channels.
    ///
    /// Creates direct crossbeam channels between components:
    ///   audio_tx -> dsp_rx  (raw audio)
    ///   dsp_tx   -> ft8_rx  (processed windows)
    ///   ft8_tx   -> tui_rx  (decoded messages)
    pub(crate) async fn start_pipeline(&mut self) -> Result<()> {
        // Point-to-point channels for the data path
        let (audio_to_dsp_tx, audio_to_dsp_rx) = crossbeam_channel::unbounded::<Vec<f32>>();
        let (dsp_to_ft8_tx, dsp_to_ft8_rx) = crossbeam_channel::unbounded::<Vec<f32>>();
        let (ft8_to_tui_tx, ft8_to_tui_rx) =
            crossbeam_channel::unbounded::<pancetta_ft8::DecodedMessage>();
        let (waterfall_tx, waterfall_rx) = crossbeam_channel::unbounded::<Vec<Vec<f32>>>();

        // Also create message bus channels for control messages (hamlib, autonomous, etc.)
        let (_audio_bus_tx, _audio_bus_rx) =
            self.message_bus.create_channel(ComponentId::Audio).await?;
        let (_dsp_bus_tx, _dsp_bus_rx) = self.message_bus.create_channel(ComponentId::Dsp).await?;
        let (_ft8_bus_tx, _ft8_bus_rx) = self
            .message_bus
            .create_channel(ComponentId::Ft8Decoder)
            .await?;
        let (_tui_bus_tx, tui_bus_rx) = self.message_bus.create_channel(ComponentId::Tui).await?;

        // --- Audio component ---
        self.start_audio_pipeline(audio_to_dsp_tx).await?;

        // --- DSP component ---
        self.start_dsp_pipeline(audio_to_dsp_rx, dsp_to_ft8_tx, waterfall_tx.clone())
            .await?;

        // --- FT8 decoder component ---
        self.start_ft8_pipeline(dsp_to_ft8_rx, ft8_to_tui_tx, waterfall_tx)
            .await?;

        // --- TUI component ---
        if !self.headless {
            self.start_tui_pipeline(ft8_to_tui_rx, tui_bus_rx, waterfall_rx)
                .await?;
        } else {
            // In headless mode, just drain decoded messages and log them
            let shutdown = self.shutdown_signal.clone();
            let handle = tokio::spawn(async move {
                while !shutdown.load(Ordering::Acquire) {
                    match ft8_to_tui_rx.try_recv() {
                        Ok(msg) => {
                            info!(
                                "Decoded: {} (SNR: {:.0}, freq: {:.1} Hz)",
                                msg.text, msg.snr_db, msg.frequency_offset
                            );
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            tokio::task::yield_now().await;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                    }
                }
                Ok(())
            });
            self.named_task_handles.push((ComponentId::Tui, handle));

            // Drain waterfall channel in headless mode to prevent unbounded growth
            let drain_shutdown = self.shutdown_signal.clone();
            let drain_handle = tokio::spawn(async move {
                while !drain_shutdown.load(Ordering::Acquire) {
                    match waterfall_rx.try_recv() {
                        Ok(_) => {} // discard
                        Err(_) => tokio::task::yield_now().await,
                    }
                }
                Ok(())
            });
            self.named_task_handles
                .push((ComponentId::Tui, drain_handle));
        }

        Ok(())
    }

    /// Start audio component with point-to-point output channel
    pub(crate) async fn start_audio_pipeline(
        &mut self,
        audio_to_dsp_tx: crossbeam_channel::Sender<Vec<f32>>,
    ) -> Result<()> {
        if self.no_audio {
            info!("Audio processing disabled");
            return Ok(());
        }

        let span = span!(Level::INFO, "start_audio");
        let _enter = span.enter();

        let use_stub = std::env::var("PANCETTA_STUB_AUDIO").is_ok();

        if use_stub {
            info!("Starting audio component in STUB mode");

            let config = self.config.read().await;
            let sample_rate = config.audio.sample_rate;
            let buffer_size = config.audio.buffer_size as usize;
            drop(config);

            let shutdown = self.shutdown_signal.clone();
            let last_timestamp = self.last_audio_timestamp.clone();

            let handle = tokio::spawn(async move {
                let mut phase = 0.0f32;
                let frequency = 1500.0;
                let buffer_duration_ms = (buffer_size as f64 * 1000.0 / sample_rate as f64) as u64;
                let mut process_interval =
                    interval(Duration::from_millis(buffer_duration_ms.max(5)));

                while !shutdown.load(Ordering::Acquire) {
                    process_interval.tick().await;

                    let mut samples = Vec::with_capacity(buffer_size);
                    for _ in 0..buffer_size {
                        let sample = 0.1 * phase.sin();
                        samples.push(sample);
                        phase += 2.0 * std::f32::consts::PI * frequency / sample_rate as f32;
                        if phase > 2.0 * std::f32::consts::PI {
                            phase -= 2.0 * std::f32::consts::PI;
                        }
                    }

                    {
                        let mut timestamp = last_timestamp.write().await;
                        *timestamp = Some(Instant::now());
                    }

                    if audio_to_dsp_tx.send(samples).is_err() {
                        break;
                    }
                }

                info!("Audio stub stopped");
                Ok(())
            });

            self.named_task_handles.push((ComponentId::Audio, handle));
        } else {
            info!("Starting audio component with real AudioManager");

            let config = self.config.read().await;
            let audio_config = AudioManagerConfig {
                input_device: Some(config.audio.input_device.clone()),
                output_device: Some(config.audio.output_device.clone()),
                sample_rate: config.audio.sample_rate,
                buffer_size: config.audio.buffer_size as usize,
                channels: config.audio.input_channels as u16,
                enable_monitoring: false,
                target_latency_ms: 1.0,
                input_gain_db: config.audio.levels.input_gain_db,
            };
            drop(config);

            let shutdown = self.shutdown_signal.clone();
            let last_timestamp = self.last_audio_timestamp.clone();

            // Audio thread sends samples via a tokio mpsc to an async relay
            let (result_tx, mut result_rx) = tokio::sync::mpsc::channel(100);

            std::thread::spawn(move || {
                let mut audio_manager = match AudioManager::with_config(audio_config) {
                    Ok(manager) => manager,
                    Err(e) => {
                        error!("Failed to create AudioManager: {}", e);
                        return;
                    }
                };

                if let Err(e) = audio_manager.start() {
                    error!("Failed to start audio stream: {}", e);
                    return;
                }

                info!("AudioManager started in dedicated thread");

                loop {
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }

                    match audio_manager.process_audio() {
                        Ok(Some(samples)) => {
                            if result_tx.blocking_send(samples).is_err() {
                                break;
                            }
                        }
                        Ok(None) => {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        Err(e) => {
                            error!("Audio processing error: {}", e);
                        }
                    }
                }

                let _ = audio_manager.stop();
                info!("Audio manager thread stopped");
            });

            // Async relay: tokio mpsc -> crossbeam point-to-point
            let handle = tokio::spawn(async move {
                let mut relay_count: u64 = 0;
                while let Some(samples) = result_rx.recv().await {
                    {
                        let mut timestamp = last_timestamp.write().await;
                        *timestamp = Some(Instant::now());
                    }

                    let len = samples.len();
                    if audio_to_dsp_tx.send(samples).is_err() {
                        info!(
                            "Audio relay: DSP channel closed after {} sends",
                            relay_count
                        );
                        break;
                    }
                    relay_count += 1;
                    if relay_count == 1 {
                        info!("Audio relay: first batch sent ({} samples)", len);
                    } else if relay_count % 1000 == 0 {
                        info!("Audio relay: {} batches sent so far", relay_count);
                    }
                }

                info!("Audio relay task stopped (total: {} batches)", relay_count);
                Ok(())
            });

            self.named_task_handles.push((ComponentId::Audio, handle));
        }

        info!("Audio component started");
        Ok(())
    }

    /// Start DSP pipeline with point-to-point channels
    ///
    /// Simple direct pipeline: resample 48kHz->12kHz on a dedicated thread,
    /// accumulate FT8-sized windows, and send to the decoder.
    pub(crate) async fn start_dsp_pipeline(
        &mut self,
        audio_rx: crossbeam_channel::Receiver<Vec<f32>>,
        dsp_to_ft8_tx: crossbeam_channel::Sender<Vec<f32>>,
        live_waterfall_tx: crossbeam_channel::Sender<Vec<Vec<f32>>>,
    ) -> Result<()> {
        let span = span!(Level::INFO, "start_dsp");
        let _enter = span.enter();

        info!("Starting DSP component");

        let shutdown = self.shutdown_signal.clone();
        let message_count = self.message_count.clone();

        let config = self.config.read().await;
        let input_rate = config.audio.sample_rate;
        let input_channels = config.audio.input_channels as u16;
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

            // FIR low-pass filter for anti-aliased decimation.
            // 65-tap Kaiser-windowed sinc (beta=8, ~80dB stopband attenuation).
            // Cutoff at 0.125 * Nyquist = 6kHz (= 12kHz/2, the decimated Nyquist).
            let fir_len = decimation_factor * 16 + 1; // 65 taps for factor=4
            let beta = 8.0f32; // Kaiser beta for ~80dB stopband
            let fir_coeffs: Vec<f32> = (0..fir_len)
                .map(|i| {
                    let n = i as f32 - (fir_len - 1) as f32 / 2.0;
                    let cutoff = 1.0 / (2.0 * decimation_factor as f32);
                    // Windowed sinc
                    let sinc = if n.abs() < 1e-6 {
                        2.0 * cutoff
                    } else {
                        (2.0 * std::f32::consts::PI * cutoff * n).sin() / (std::f32::consts::PI * n)
                    };
                    // Kaiser window: I0(beta * sqrt(1 - (2i/(N-1) - 1)^2)) / I0(beta)
                    let m = (fir_len - 1) as f32;
                    let x = 2.0 * i as f32 / m - 1.0;
                    let arg = beta * (1.0 - x * x).max(0.0).sqrt();
                    // Approximate I0 (modified Bessel) with series expansion
                    let i0 = |v: f32| -> f32 {
                        let mut sum = 1.0f32;
                        let mut term = 1.0f32;
                        for k in 1..20 {
                            term *= (v / (2.0 * k as f32)) * (v / (2.0 * k as f32));
                            sum += term;
                            if term < 1e-10 {
                                break;
                            }
                        }
                        sum
                    };
                    let window = i0(arg) / i0(beta);
                    sinc * window
                })
                .collect();
            // Normalize filter
            let fir_sum: f32 = fir_coeffs.iter().sum();
            let fir_coeffs: Vec<f32> = fir_coeffs.iter().map(|c| c / fir_sum).collect();

            let mut fir_buffer: Vec<f32> = vec![0.0; fir_len];
            let mut fir_pos: usize = 0;
            let mut decimate_counter: usize = 0;

            let mut ft8_buffer: Vec<f32> = Vec::with_capacity(FT8_WINDOW_SAMPLES * 2);
            let mut window_count: u64 = 0;
            let mut batch_count: u64 = 0;
            let _waiting_for_boundary = true;

            // Live waterfall state
            let mut last_live_wf_samples: usize = 0;
            let mut live_wf_planner = rustfft::FftPlanner::<f32>::new();
            let live_wf_fft = live_wf_planner.plan_fft_forward(2048);

            info!(
                "DSP: {}Hz/{}ch -> {}Hz mono (decimate {}:1, {}-tap FIR), window={}",
                input_rate,
                input_channels,
                FT8_SAMPLE_RATE,
                decimation_factor,
                fir_len,
                FT8_WINDOW_SAMPLES
            );

            // Continuously capture audio -- don't wait for boundaries.
            // FT8 has both even (0/30s) and odd (15/45s) time slots.
            // We send overlapping windows: one at each 15-second mark.
            // The decoder handles time alignment internally via Costas sync.
            let mut next_window_time = {
                let now = chrono::Utc::now();
                let secs = now.timestamp() % 15;
                // Next 15-second boundary
                let wait_secs = if secs == 0 { 0 } else { 15 - secs };
                now + chrono::Duration::seconds(wait_secs)
            };
            info!(
                "DSP: first window at {}",
                next_window_time.format("%H:%M:%S")
            );

            while !shutdown.load(Ordering::Acquire) {
                match audio_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(samples) => {
                        message_count.fetch_add(1, Ordering::Relaxed);
                        batch_count += 1;

                        // Extract mono from interleaved multi-channel.
                        // Use left channel only (channel 0) to avoid phase cancellation
                        // that can occur when averaging L+R from USB audio codecs.
                        let mono: Vec<f32> = if input_channels > 1 {
                            samples
                                .chunks(input_channels as usize)
                                .map(|ch| ch[0])
                                .collect()
                        } else {
                            samples
                        };

                        // Anti-aliased decimation: FIR low-pass + downsample
                        for &sample in &mono {
                            fir_buffer[fir_pos] = sample;
                            fir_pos = (fir_pos + 1) % fir_len;
                            decimate_counter += 1;

                            if decimate_counter >= decimation_factor {
                                decimate_counter = 0;
                                // Apply FIR filter (convolution)
                                let mut sum = 0.0f32;
                                for (j, &coeff) in fir_coeffs.iter().enumerate() {
                                    let idx = (fir_pos + j) % fir_len;
                                    sum += fir_buffer[idx] * coeff;
                                }
                                ft8_buffer.push(sum);
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
                            }
                        }

                        // Send FT8 window when we have enough samples AND
                        // we've reached the next 15-second boundary
                        let now = chrono::Utc::now();
                        if ft8_buffer.len() >= FT8_WINDOW_SAMPLES && now >= next_window_time {
                            // Take the most recent FT8_WINDOW_SAMPLES from the buffer
                            let start = ft8_buffer.len() - FT8_WINDOW_SAMPLES;
                            let window: Vec<f32> = ft8_buffer[start..].to_vec();
                            // Keep some overlap for the next window (retain last 1s worth)
                            let keep = FT8_SAMPLE_RATE; // 12000 samples = 1 second
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
                            if dsp_to_ft8_tx.send(window).is_err() {
                                info!("DSP: FT8 channel closed");
                                return Ok(());
                            }
                            // Schedule next window at the next 15-second boundary
                            let now_resync = chrono::Utc::now();
                            let secs_past = now_resync.timestamp() % 15;
                            let wait_secs = if secs_past == 0 { 15 } else { 15 - secs_past };
                            next_window_time = now_resync + chrono::Duration::seconds(wait_secs);
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

    /// Start FT8 decoder with point-to-point channels
    pub(crate) async fn start_ft8_pipeline(
        &mut self,
        ft8_rx: crossbeam_channel::Receiver<Vec<f32>>,
        ft8_to_tui_tx: crossbeam_channel::Sender<pancetta_ft8::DecodedMessage>,
        waterfall_tx: crossbeam_channel::Sender<Vec<Vec<f32>>>,
    ) -> Result<()> {
        let span = span!(Level::INFO, "start_ft8");
        let _enter = span.enter();

        info!("Starting FT8 component");

        let ft8_config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(ft8_config)?;

        let shutdown = self.shutdown_signal.clone();
        let last_decode_timestamp = self.last_decode_timestamp.clone();
        let message_bus = self.message_bus.clone();
        let self_waterfall_to_auto_tx = self.waterfall_to_auto_tx.clone();

        // Read station callsign for AP decoding before moving into the thread
        let station_callsign = {
            let config = self.config.read().await;
            config.station.callsign.clone()
        };

        // Shared AP state updated by the QSO component
        let active_qso_ap = self.active_qso_ap.clone();

        // Run FT8 decoder on a dedicated thread to avoid tokio starvation
        let handle = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            info!("FT8 decoder thread started");

            // Create persistent AP state for enhanced decoding
            let my_call_ap = pancetta_ft8::MyCallAp::new(&station_callsign);
            if my_call_ap.is_none() {
                warn!(
                    "AP decoding: could not encode station callsign '{}', AP1+ disabled",
                    station_callsign
                );
            } else {
                info!(
                    "AP decoding: station callsign '{}' encoded for AP injection",
                    station_callsign
                );
            }
            let mut recent_pool: Vec<pancetta_ft8::RecentCallAp> = Vec::new();

            while !shutdown.load(Ordering::Acquire) {
                match ft8_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(window) => {
                        info!("FT8 decoder: received window ({} samples)", window.len());

                        // Generate waterfall data
                        let audio_f64: Vec<f64> = window.iter().map(|&s| s as f64).collect();
                        match decoder.generate_waterfall_data(&audio_f64) {
                            Ok(wf) => {
                                let range = wf.max_power - wf.min_power;
                                info!(
                                    "Waterfall: {}x{} matrix, power range {:.1}..{:.1} dB",
                                    wf.power_matrix.len(),
                                    wf.power_matrix.first().map(|r| r.len()).unwrap_or(0),
                                    wf.min_power,
                                    wf.max_power,
                                );
                                let rows: Vec<Vec<f32>> = if range > 0.0 {
                                    wf.power_matrix
                                        .iter()
                                        .map(|row| {
                                            row.iter()
                                                .map(|&p| ((p - wf.min_power) / range) as f32)
                                                .collect()
                                        })
                                        .collect()
                                } else {
                                    wf.power_matrix
                                        .iter()
                                        .map(|row| vec![0.0f32; row.len()])
                                        .collect()
                                };
                                let _ = waterfall_tx.send(rows.clone());
                                if let Some(ref auto_wf_tx) = self_waterfall_to_auto_tx {
                                    let _ = auto_wf_tx.try_send(rows);
                                }
                            }
                            Err(e) => {
                                warn!("Waterfall generation error: {}", e);
                            }
                        }

                        // Build AP context for this decode window
                        let current_qso_ap =
                            active_qso_ap.read().ok().and_then(|guard| guard.clone());
                        let ap_context = pancetta_ft8::ApContext {
                            my_call: my_call_ap.clone(),
                            recent_calls: recent_pool.clone(),
                            active_qso: current_qso_ap,
                        };

                        // Decode FT8 signals with AP-enhanced decoding
                        match decoder.decode_window_with_ap(&window, &ap_context) {
                            Ok(decoded_messages) => {
                                // Update decode timestamp
                                rt.block_on(async {
                                    let mut timestamp = last_decode_timestamp.write().await;
                                    *timestamp = Some(Instant::now());
                                });

                                info!("FT8 decoder: {} messages decoded", decoded_messages.len());

                                for decoded_msg in &decoded_messages {
                                    info!(
                                        "FT8 decoded: {} (SNR: {:.0}, freq: {:.1})",
                                        decoded_msg.text,
                                        decoded_msg.snr_db,
                                        decoded_msg.frequency_offset
                                    );

                                    // Send to TUI via point-to-point channel
                                    if ft8_to_tui_tx.send(decoded_msg.clone()).is_err() {
                                        warn!("TUI channel disconnected");
                                    }

                                    // Forward to other components via message bus (fire-and-forget
                                    // to avoid stalling the decoder thread with block_on)
                                    let auto_msg = ComponentMessage::new(
                                        ComponentId::Ft8Decoder,
                                        ComponentId::Autonomous,
                                        MessageType::DecodedMessage(decoded_msg.clone()),
                                        Instant::now(),
                                    );
                                    let bus1 = message_bus.clone();
                                    rt.spawn(async move {
                                        if let Err(e) = bus1.send_message(auto_msg).await {
                                            debug!("Failed to forward decoded message to Autonomous: {}", e);
                                        }
                                    });

                                    let qso_msg = ComponentMessage::new(
                                        ComponentId::Ft8Decoder,
                                        ComponentId::Qso,
                                        MessageType::DecodedMessage(decoded_msg.clone()),
                                        Instant::now(),
                                    );
                                    let bus2 = message_bus.clone();
                                    rt.spawn(async move {
                                        if let Err(e) = bus2.send_message(qso_msg).await {
                                            debug!(
                                                "Failed to forward decoded message to QSO: {}",
                                                e
                                            );
                                        }
                                    });

                                    let psk_msg = ComponentMessage::new(
                                        ComponentId::Ft8Decoder,
                                        ComponentId::PskReporter,
                                        MessageType::DecodedMessage(decoded_msg.clone()),
                                        Instant::now(),
                                    );
                                    let bus3 = message_bus.clone();
                                    rt.spawn(async move {
                                        if let Err(e) = bus3.send_message(psk_msg).await {
                                            debug!("Failed to forward decoded message to PSKReporter: {}", e);
                                        }
                                    });
                                }

                                // Update AP recent_pool with newly decoded callsigns
                                for msg in &decoded_messages {
                                    if let Some(ref call) = msg.message.from_callsign {
                                        if !recent_pool.iter().any(|r| r.callsign == *call) {
                                            if let Some(ap) =
                                                pancetta_ft8::RecentCallAp::new(call, msg.snr_db)
                                            {
                                                recent_pool.push(ap);
                                            }
                                        }
                                    }
                                }
                                // Keep strongest 20, prune weak entries
                                recent_pool.sort_by(|a, b| {
                                    b.last_snr
                                        .partial_cmp(&a.last_snr)
                                        .unwrap_or(std::cmp::Ordering::Equal)
                                });
                                recent_pool.truncate(20);
                            }
                            Err(e) => {
                                warn!("FT8 decode error: {}", e);
                            }
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        info!("FT8 decoder: input channel disconnected");
                        break;
                    }
                }
            }

            info!("FT8 component stopped");
            Ok(())
        });

        self.named_task_handles
            .push((ComponentId::Ft8Decoder, handle));
        info!("FT8 component started");
        Ok(())
    }

    /// Start TUI component with point-to-point decoded message channel
    pub(crate) async fn start_tui_pipeline(
        &mut self,
        ft8_to_tui_rx: crossbeam_channel::Receiver<pancetta_ft8::DecodedMessage>,
        tui_bus_rx: crossbeam_channel::Receiver<ComponentMessage>,
        waterfall_rx: crossbeam_channel::Receiver<Vec<Vec<f32>>>,
    ) -> Result<()> {
        let span = span!(Level::INFO, "start_tui");
        let _enter = span.enter();

        info!("Starting TUI component");

        let config = self.config.clone();
        let shutdown = self.shutdown_signal.clone();

        // Create TUI message/command channels for the TuiRunner
        let (tui_msg_tx, tui_msg_rx) =
            crossbeam_channel::unbounded::<pancetta_tui::tui_runner::TuiMessage>();
        let (tui_cmd_tx, tui_cmd_rx) =
            crossbeam_channel::unbounded::<pancetta_tui::tui_runner::TuiCommand>();

        // Read initial operating frequency from config (no frequency_mhz field on StationConfig,
        // so default to 20m FT8 = 14.074 MHz; will be updated by FrequencyResponse messages)
        let operating_freq_mhz = 14.074_f64;
        let operating_freq = Arc::new(std::sync::atomic::AtomicU64::new(
            operating_freq_mhz.to_bits(),
        ));
        let operating_freq_relay = operating_freq.clone();

        // Set up station coordinates for distance/bearing calculation
        let station_coords = {
            let config = self.config.read().await;
            pancetta_dx::gridsquare::grid_to_coordinates(&config.station.grid_square).ok()
        };

        // Relay decoded messages from FT8 -> TUI on a dedicated thread
        // (tokio::spawn was causing starvation -- same pattern as DSP/FT8 fixes)
        let relay_shutdown = shutdown.clone();
        let tui_msg_tx_relay = tui_msg_tx.clone();
        let tui_relay_jh = std::thread::Builder::new()
            .name("tui-relay".to_string())
            .spawn(move || {
            let mut ft8_disconnected = false;
            while !relay_shutdown.load(Ordering::Acquire) {
                if !ft8_disconnected {
                    match ft8_to_tui_rx.try_recv() {
                        Ok(decoded_msg) => {
                            let call_sign = decoded_msg.message.from_callsign.clone();
                            let grid_square = decoded_msg.message.grid_square.clone();

                            // Compute distance and bearing if both grids are available
                            let (distance, bearing) = match (&grid_square, &station_coords) {
                                (Some(remote_grid), Some((home_lat, home_lon))) => {
                                    match pancetta_dx::gridsquare::grid_to_coordinates(remote_grid)
                                    {
                                        Ok((remote_lat, remote_lon)) => {
                                            let geod = geographiclib_rs::Geodesic::wgs84();
                                            let (dist_m, azi1, _azi2, _arc) = geod.inverse(
                                                *home_lat, *home_lon, remote_lat, remote_lon,
                                            );
                                            let bearing_deg =
                                                if azi1 < 0.0 { azi1 + 360.0 } else { azi1 };
                                            (Some(dist_m / 1000.0), Some(bearing_deg))
                                        }
                                        Err(_) => (None, None),
                                    }
                                }
                                _ => (None, None),
                            };

                            let tui_decoded = pancetta_tui::DecodedMessageView {
                                timestamp: chrono::Utc::now(),
                                frequency: f64::from_bits(
                                    operating_freq_relay.load(Ordering::Relaxed),
                                ),
                                mode: "FT8".to_string(),
                                snr: decoded_msg.snr_db as i32,
                                delta_time: decoded_msg.time_offset as f32,
                                delta_freq: decoded_msg.frequency_offset as f32,
                                call_sign,
                                grid_square,
                                message: decoded_msg.text.clone(),
                                distance,
                                bearing,
                            };

                            match tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::DecodedMessage(tui_decoded),
                            ) {
                                Ok(()) => info!("TUI relay: forwarded decoded message to TUI channel"),
                                Err(e) => warn!("TUI relay: failed to send to TUI: {}", e),
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {}
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            warn!("FT8 decoder channel disconnected, TUI relay continuing without decode data");
                            ft8_disconnected = true;
                        }
                    }
                }

                // Also drain control messages from the message bus
                match tui_bus_rx.try_recv() {
                    Ok(bus_msg) => {
                        match bus_msg.message_type {
                            MessageType::AutonomousStatus(ref status) => {
                                // Forward as status update for now
                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: "Autonomous".to_string(),
                                        status: status.state.clone(),
                                    },
                                );
                            }
                            MessageType::RigControl(
                                crate::message_bus::RigControlMessage::FrequencyResponse {
                                    vfo,
                                    frequency,
                                },
                            ) => {
                                // Update operating frequency for decoded message enrichment
                                let freq_mhz = frequency as f64 / 1_000_000.0;
                                // Relaxed ordering is fine -- this is a best-effort display value for the TUI
                                operating_freq_relay.store(freq_mhz.to_bits(), Ordering::Relaxed);
                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::FrequencyUpdate {
                                        vfo,
                                        frequency,
                                    },
                                );
                            }
                            MessageType::DxMessage(crate::message_bus::DxMessage::Spot {
                                callsign,
                                frequency,
                                spotter,
                                ..
                            }) => {
                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::DxSpot {
                                        callsign,
                                        frequency,
                                        spotter,
                                    },
                                );
                            }
                            _ => {}
                        }
                    }
                    Err(_) => {}
                }

                // Relay waterfall data from FT8 decoder to TUI
                match waterfall_rx.try_recv() {
                    Ok(rows) => {
                        let _ = tui_msg_tx_relay
                            .send(pancetta_tui::tui_runner::TuiMessage::WaterfallUpdate { rows });
                    }
                    Err(_) => {}
                }

                // Sleep to prevent busy-spinning
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            info!("TUI relay thread stopped");
        }).expect("Failed to spawn TUI relay thread");
        self.tui_relay_handle = Some(tui_relay_jh);

        // Task: relay TUI commands (e.g. SendMessage) to message bus as TransmitRequests
        let cmd_shutdown = self.shutdown_signal.clone();
        let cmd_message_bus = self.message_bus.clone();
        let cmd_operating_freq = operating_freq.clone();
        let cmd_handle = tokio::spawn(async move {
            while !cmd_shutdown.load(Ordering::Acquire) {
                match tui_cmd_rx.try_recv() {
                    Ok(cmd) => match cmd {
                        pancetta_tui::tui_runner::TuiCommand::SendMessage { text } => {
                            info!("TUI SendMessage: '{}'", text);
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Ft8Transmitter,
                                MessageType::TransmitRequest {
                                    message_text: text,
                                    frequency_offset: 1500.0,
                                    qso_id: None,
                                },
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                warn!("Failed to forward TUI command: {}", e);
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::CallStation {
                            callsign,
                            frequency,
                        } => {
                            info!("TUI CallStation: {} at {} Hz", callsign, frequency);
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Qso,
                                MessageType::QsoMessage(crate::message_bus::QsoMessage::StartQso {
                                    callsign,
                                    frequency,
                                }),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                warn!("Failed to forward CallStation command: {}", e);
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::SetFrequency { vfo, frequency } => {
                            info!("TUI SetFrequency: VFO {} -> {} Hz", vfo, frequency);
                            let freq_mhz = frequency as f64 / 1_000_000.0;
                            cmd_operating_freq.store(freq_mhz.to_bits(), Ordering::Relaxed);
                            // Forward to hamlib if available
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Hamlib,
                                MessageType::RigControl(
                                    crate::message_bus::RigControlMessage::SetFrequency {
                                        vfo,
                                        frequency,
                                    },
                                ),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                debug!("Failed to forward SetFrequency to hamlib: {}", e);
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::Quit => {
                            info!("TUI requested application quit");
                            cmd_shutdown.store(true, Ordering::Release);
                            break;
                        }
                        _ => {
                            debug!("Unhandled TUI command: {:?}", cmd);
                        }
                    },
                    Err(crossbeam_channel::TryRecvError::Empty) => {
                        tokio::task::yield_now().await;
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                }
            }
            Ok(())
        });
        self.named_task_handles.push((ComponentId::Tui, cmd_handle));

        // Run the TUI on a blocking task (it takes over the terminal)
        let tui_config_lock = config.read().await;
        let tui_config = pancetta_tui::Config {
            station: pancetta_tui::config::StationConfig {
                call_sign: tui_config_lock.station.callsign.clone(),
                grid_square: tui_config_lock.station.grid_square.clone(),
                power: tui_config_lock.station.power_watts,
                antenna: "Vertical".to_string(),
                rig: tui_config_lock.rig.model.clone(),
                default_frequency: 14.074,
            },
            ui: pancetta_tui::config::UiConfig {
                theme: pancetta_tui::Theme::Dark,
                refresh_rate: 30,
                max_messages: 100,
                show_waterfall: true,
                show_coordinates: true,
                time_format: pancetta_tui::config::TimeFormat::UTC24,
                frequency_format: pancetta_tui::config::FrequencyFormat::MHz,
            },
            audio: pancetta_tui::config::AudioConfig {
                device: Some(tui_config_lock.audio.input_device.clone()),
                sample_rate: tui_config_lock.audio.sample_rate,
                buffer_size: tui_config_lock.audio.buffer_size as usize,
                auto_gain: false,
                gain_level: tui_config_lock.audio.levels.input_gain_db,
            },
            decoder: pancetta_tui::config::DecoderConfig {
                enabled_modes: vec!["FT8".to_string()],
                minimum_snr: -20,
                decode_depth: 3,
                aggressive_decode: true,
                enable_averaging: false,
            },
            bands: pancetta_tui::Config::default().bands,
        };
        drop(tui_config_lock);

        // Start TUI runner in a blocking task so it can own the terminal
        let tui_handle = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                pancetta_tui::tui_runner::run_tui_with_message_bus(
                    tui_config, tui_msg_rx, tui_cmd_tx, shutdown,
                )
                .await
            })
        });

        // Wrap the JoinHandle and ensure shutdown is triggered when TUI exits
        let tui_shutdown = self.shutdown_signal.clone();
        let tui_wrapper = tokio::spawn(async move {
            let result = match tui_handle.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(anyhow::anyhow!("TUI task panicked: {}", e)),
            };
            // Always trigger shutdown when TUI exits (user quit, crash, etc.)
            tui_shutdown.store(true, Ordering::Release);
            result
        });
        self.named_task_handles
            .push((ComponentId::Tui, tui_wrapper));

        info!("TUI component started");
        Ok(())
    }
}
