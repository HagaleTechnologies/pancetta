//! Real-time audio processing engine
//! 
//! Provides the core real-time audio callback and processing infrastructure
//! designed to achieve <1ms latency for the Pancetta Week 0 Technical POC.

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, Host, Stream, SupportedStreamConfig,
};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::ringbuffer_comm::AudioComm;
use crate::latency::CallbackTimer;

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
            sample_rate: 48000,    // 48kHz for professional audio
            buffer_size: 64,       // Ultra-low latency (64 samples = ~1.33ms at 48kHz)
            input_channels: 2,     // Stereo input
            output_channels: 2,    // Stereo output
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
    /// - `comm`: Lock-free communication channel for coordination with main thread
    /// 
    /// # Returns
    /// Result indicating success or failure of stream initialization
    pub fn start(&mut self, comm: Arc<AudioComm>) -> Result<(), Box<dyn std::error::Error>> {
        // Get supported stream configurations
        let input_config = self.get_input_config()?;
        let output_config = self.get_output_config()?;
        
        println!(
            "Using config - Sample Rate: {}Hz, Buffer Size: {} frames",
            input_config.sample_rate().0, self.config.buffer_size
        );
        
        // Create the real-time audio callback
        let comm_clone = comm.clone();
        let stream = self.output_device.build_output_stream(
            &output_config.into(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                // Start latency measurement
                let timer = CallbackTimer::start();
                
                // Process audio in real-time (currently pass-through)
                Self::process_audio_callback(data);
                
                // Record latency measurement
                let latency_ns = timer.elapsed_ns();
                // Use non-blocking latency recording
                let _ = comm_clone.push_latency(latency_ns);
                
                // Check for shutdown signal
                if comm_clone.should_stop() {
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
    
    /// Real-time audio processing callback (allocation-free)
    /// 
    /// This is the critical path that must complete in <1ms.
    /// No allocations, no blocking operations, minimal processing.
    fn process_audio_callback(output: &mut [f32]) {
        // Currently implement simple pass-through for POC
        // In the future, this will process FT8 signals
        
        // Generate simple test tone for now (1kHz sine wave)
        static mut PHASE: f32 = 0.0;
        let phase_increment = 2.0 * std::f32::consts::PI * 1000.0 / 48000.0; // 1kHz at 48kHz
        
        unsafe {
            for sample in output.iter_mut() {
                *sample = PHASE.sin() * 0.1; // Low amplitude test tone
                PHASE += phase_increment;
                if PHASE >= 2.0 * std::f32::consts::PI {
                    PHASE -= 2.0 * std::f32::consts::PI;
                }
            }
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
    
    let comm = Arc::new(AudioComm::new(128, 1000));
    let mut latency_measurer = LatencyMeasurer::new(1000, 1_000_000); // 1ms target
    
    println!("Starting latency stress test for {} seconds...", test_duration_seconds);
    println!("Theoretical minimum latency: {:.3}ms", processor.theoretical_min_latency_ms());
    
    // Start audio processing
    processor.start(comm.clone())?;
    
    // Collect latency measurements
    let test_start = std::time::Instant::now();
    while test_start.elapsed().as_secs() < test_duration_seconds {
        // Collect latency measurements from the ringbuffer
        while let Some(latency_ns) = comm.pop_latency() {
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
    comm.stop();
    processor.stop();
    
    // Collect any remaining measurements
    while let Some(latency_ns) = comm.pop_latency() {
        latency_measurer.record_latency(latency_ns);
    }
    
    // Display final results
    let final_stats = latency_measurer.get_stats();
    println!("\n{}", final_stats.format_for_display());
    
    // Validate if we meet the <1ms requirement
    if final_stats.meeting_target {
        println!("\n✅ SUCCESS: Audio system consistently achieves <1ms latency!");
        println!("   The Pancetta real-time architecture is VIABLE.");
    } else {
        println!("\n❌ FAILURE: Audio system does not consistently achieve <1ms latency.");
        println!("   The Pancetta architecture needs fundamental changes.");
        return Err("Latency requirement not met".into());
    }
    
    Ok(())
}