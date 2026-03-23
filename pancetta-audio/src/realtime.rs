//! Real-time audio processing engine
//!
//! Provides the core real-time audio callback and processing infrastructure
//! designed to achieve <1ms latency for the Pancetta Week 0 Technical POC.

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, Host, Stream, SupportedStreamConfig,
};
use std::thread;
use std::time::Duration;

use crate::latency::CallbackTimer;
use crate::ringbuffer_comm::{audio_comm_pair, AudioCommShared};

/// Real-time audio processor configuration
#[derive(Debug, Clone)]
pub struct AudioConfig {
    /// Sample rate (typically 44100 or 48000 Hz)
    pub sample_rate: u32,
    /// Buffer size in frames (smaller = lower latency, higher CPU usage)
    pub buffer_size: u32,
    /// Number of input channels
    pub input_channels: u16,
    /// Number of output channels  
    pub output_channels: u16,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000, // 48kHz for professional audio
            buffer_size: 64,    // Ultra-low latency (64 samples = ~1.33ms at 48kHz)
            input_channels: 2,  // Stereo input
            output_channels: 2, // Stereo output
        }
    }
}

/// Real-time audio processor
///
/// Manages the audio stream and provides real-time processing with latency measurement.
pub struct RealtimeAudioProcessor {
    _host: Host,
    input_device: Device,
    output_device: Device,
    config: AudioConfig,
    stream: Option<Stream>,
}

impl RealtimeAudioProcessor {
    /// Create a new real-time audio processor
    ///
    /// # Parameters
    /// - `config`: Audio configuration specifying sample rate, buffer size, etc.
    ///
    /// # Returns
    /// Result containing the processor or an error if audio setup fails
    pub fn new(config: AudioConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let host = cpal::default_host();

        // Get default input and output devices
        let input_device = host
            .default_input_device()
            .ok_or("No input device available")?;
        let output_device = host
            .default_output_device()
            .ok_or("No output device available")?;

        println!("Input device: {}", input_device.name()?);
        println!("Output device: {}", output_device.name()?);

        Ok(Self {
            _host: host,
            input_device,
            output_device,
            config,
            stream: None,
        })
    }

    /// Start the real-time audio processing stream
    ///
    /// # Parameters
    /// - `shared`: Shared atomic state for coordination with main thread
    ///
    /// # Returns
    /// Result indicating success or failure of stream initialization
    pub fn start(&mut self, shared: AudioCommShared) -> Result<(), Box<dyn std::error::Error>> {
        // Get supported stream configurations
        let _input_config = self.get_input_config()?;
        let output_config = self.get_output_config()?;

        println!(
            "Using config - Sample Rate: {}Hz, Buffer Size: {} frames",
            output_config.sample_rate().0,
            self.config.buffer_size
        );

        // Create the real-time audio callback.
        // Phase is captured as a local variable in the closure — no static mut needed.
        let shared_clone = shared.clone();
        let sample_rate = self.config.sample_rate as f32;

        // Create a latency producer for this output stream.
        let (mut latency_producer, _latency_consumer) = audio_comm_pair(64, 256);

        let mut phase: f32 = 0.0;

        let stream = self.output_device.build_output_stream(
            &output_config.into(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                // Start latency measurement
                let timer = CallbackTimer::start();

                // Generate simple test tone (1kHz sine wave)
                let phase_increment = 2.0 * std::f32::consts::PI * 1000.0 / sample_rate;
                for sample in data.iter_mut() {
                    *sample = phase.sin() * 0.1; // Low amplitude test tone
                    phase += phase_increment;
                    if phase >= 2.0 * std::f32::consts::PI {
                        phase -= 2.0 * std::f32::consts::PI;
                    }
                }

                // Record latency measurement
                let latency_ns = timer.elapsed_ns();
                let _ = latency_producer.push_latency(latency_ns);

                // Check for shutdown signal
                if shared_clone.should_stop() {
                    return;
                }
            },
            |err| {
                eprintln!("Audio stream error: {}", err);
            },
            None,
        )?;

        // Start the stream
        stream.play()?;
        self.stream = Some(stream);

        println!("Real-time audio stream started successfully");
        Ok(())
    }

    /// Stop the audio processing stream
    pub fn stop(&mut self) {
        if let Some(stream) = self.stream.take() {
            drop(stream);
            println!("Audio stream stopped");
        }
    }

    /// Get the optimal input configuration
    fn get_input_config(&self) -> Result<SupportedStreamConfig, Box<dyn std::error::Error>> {
        let supported_configs: Vec<_> = self.input_device.supported_input_configs()?.collect();

        // Find configuration matching our requirements
        for config in supported_configs {
            if config.channels() == self.config.input_channels
                && config.min_sample_rate().0 <= self.config.sample_rate
                && config.max_sample_rate().0 >= self.config.sample_rate
            {
                return Ok(config.with_sample_rate(cpal::SampleRate(self.config.sample_rate)));
            }
        }

        Err("No suitable input configuration found".into())
    }

    /// Get the optimal output configuration
    fn get_output_config(&self) -> Result<SupportedStreamConfig, Box<dyn std::error::Error>> {
        let supported_configs: Vec<_> = self.output_device.supported_output_configs()?.collect();

        // Find configuration matching our requirements
        for config in supported_configs {
            if config.channels() == self.config.output_channels
                && config.min_sample_rate().0 <= self.config.sample_rate
                && config.max_sample_rate().0 >= self.config.sample_rate
            {
                return Ok(config.with_sample_rate(cpal::SampleRate(self.config.sample_rate)));
            }
        }

        Err("No suitable output configuration found".into())
    }

    /// Get the theoretical minimum latency for the current configuration
    pub fn theoretical_min_latency_ms(&self) -> f64 {
        (self.config.buffer_size as f64 / self.config.sample_rate as f64) * 1000.0
    }
}

impl Drop for RealtimeAudioProcessor {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Run a latency stress test
///
/// Tests the audio system under various conditions to validate <1ms latency.
pub fn run_latency_stress_test(
    processor: &mut RealtimeAudioProcessor,
    test_duration_seconds: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::latency::LatencyMeasurer;

    let (_producer, mut consumer) = audio_comm_pair(8192, 1000);
    let shared = consumer.shared.clone();
    let mut latency_measurer = LatencyMeasurer::new(1000, 1_000_000); // 1ms target

    println!(
        "Starting latency stress test for {} seconds...",
        test_duration_seconds
    );
    println!(
        "Theoretical minimum latency: {:.3}ms",
        processor.theoretical_min_latency_ms()
    );

    // Start audio processing
    processor.start(shared.clone())?;

    // Collect latency measurements
    let test_start = std::time::Instant::now();
    while test_start.elapsed().as_secs() < test_duration_seconds {
        // Collect latency measurements from the ringbuffer
        while let Some(latency_ns) = consumer.pop_latency() {
            latency_measurer.record_latency(latency_ns);
        }

        // Print periodic updates
        if test_start.elapsed().as_secs() % 5 == 0 && latency_measurer.measurement_count() > 0 {
            let stats = latency_measurer.get_stats();
            println!(
                "Progress: {}s - Avg: {:.3}ms, Max: {:.3}ms, Excessive: {:.1}%",
                test_start.elapsed().as_secs(),
                stats.average_ms,
                stats.max_ms,
                stats.excessive_percentage
            );
        }

        thread::sleep(Duration::from_millis(100));
    }

    // Stop audio and collect final measurements
    shared.stop();
    processor.stop();

    // Collect any remaining measurements
    while let Some(latency_ns) = consumer.pop_latency() {
        latency_measurer.record_latency(latency_ns);
    }

    // Display final results
    let final_stats = latency_measurer.get_stats();
    println!("\n{}", final_stats.format_for_display());

    // Validate if we meet the <1ms requirement
    if final_stats.meeting_target {
        println!("\nSUCCESS: Audio system consistently achieves <1ms latency!");
        println!("   The Pancetta real-time architecture is VIABLE.");
    } else {
        println!("\nFAILURE: Audio system does not consistently achieve <1ms latency.");
        println!("   The Pancetta architecture needs fundamental changes.");
        return Err("Latency requirement not met".into());
    }

    Ok(())
}
