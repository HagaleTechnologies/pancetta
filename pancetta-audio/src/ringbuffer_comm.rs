//! Lock-free ringbuffer communication for real-time audio threads
//!
//! Provides zero-allocation, lock-free communication between the main thread
//! and the real-time audio callback thread.
//!
//! The ring buffer carries raw `f32` samples rather than heap-allocated
//! `AudioSample` structs, so the audio callback never touches the allocator.

use atomic::{Atomic, Ordering};
use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapRb,
};
use std::sync::Arc;

/// Size constraints for real-time audio processing
pub const DEFAULT_AUDIO_BUFFER_SIZE: usize = 8192; // ~170ms at 48kHz mono – enough to absorb jitter
pub const DEFAULT_LATENCY_BUFFER_SIZE: usize = 256; // Enough for several seconds of latency measurements
pub const MAX_AUDIO_BUFFER_SIZE: usize = 65536; // Hard limit to prevent excessive memory usage

/// Buffer size for TX audio output. A full FT8 transmission is 12.64s of
/// 48kHz mono = 606,720 samples. The output ring buffer must hold a complete
/// transmission so `queue_output` can push the entire waveform in one call
/// without dropping samples (the cpal callback drains in real time). Sized
/// with ~3 seconds of cushion above the 12.64s TX duration.
pub const OUTPUT_AUDIO_BUFFER_SIZE: usize = 786_432; // ~16s at 48kHz mono

/// Shared atomic state accessible from both producer and consumer sides.
#[derive(Clone)]
pub struct AudioCommShared {
    /// Atomic flag for clean shutdown
    pub should_stop: Arc<Atomic<bool>>,
    /// Atomic flag set when the audio stream reports an error (e.g. device disconnect)
    pub stream_error: Arc<Atomic<bool>>,
    /// Atomic counter for dropped samples (individual f32 values)
    pub dropped_samples: Arc<Atomic<u64>>,
    /// Atomic counter for processed samples (individual f32 values)
    pub processed_samples: Arc<Atomic<u64>>,
}

impl AudioCommShared {
    fn new() -> Self {
        Self {
            should_stop: Arc::new(Atomic::new(false)),
            stream_error: Arc::new(Atomic::new(false)),
            dropped_samples: Arc::new(Atomic::new(0)),
            processed_samples: Arc::new(Atomic::new(0)),
        }
    }

    /// Signal the audio thread to stop processing
    pub fn stop(&self) {
        self.should_stop.store(true, Ordering::Release);
    }

    /// Check if the audio thread should stop processing
    pub fn should_stop(&self) -> bool {
        self.should_stop.load(Ordering::Acquire)
    }

    /// Set the stream error flag (called from the audio error callback)
    pub fn set_stream_error(&self) {
        self.stream_error.store(true, Ordering::Release);
    }

    /// Check whether the audio stream has reported an error (e.g. device disconnect)
    pub fn has_stream_error(&self) -> bool {
        self.stream_error.load(Ordering::Acquire)
    }

    /// Get the number of dropped samples
    pub fn dropped_samples(&self) -> u64 {
        self.dropped_samples.load(Ordering::Relaxed)
    }

    /// Get the number of processed samples
    pub fn processed_samples(&self) -> u64 {
        self.processed_samples.load(Ordering::Relaxed)
    }

    /// Get statistics about dropped vs processed samples
    pub fn get_drop_rate(&self) -> f64 {
        let dropped = self.dropped_samples.load(Ordering::Relaxed) as f64;
        let processed = self.processed_samples.load(Ordering::Relaxed) as f64;
        let total = dropped + processed;

        if total > 0.0 {
            (dropped / total) * 100.0
        } else {
            0.0
        }
    }

    /// Reset counters (for testing)
    pub fn reset_counters(&self) {
        self.dropped_samples.store(0, Ordering::Relaxed);
        self.processed_samples.store(0, Ordering::Relaxed);
    }
}

/// Producer half of the audio communication channel.
///
/// Owned by the real-time audio callback thread. All operations are lock-free.
pub struct AudioProducer {
    /// Lock-free producer for raw f32 audio samples
    audio_producer: <HeapRb<f32> as Split>::Prod,
    /// Lock-free producer for latency measurements
    latency_producer: <HeapRb<u64> as Split>::Prod,
    /// Shared atomic state
    pub shared: AudioCommShared,
}

impl AudioProducer {
    /// Push a slice of raw f32 samples into the ring buffer (lock-free, no allocation).
    ///
    /// Returns the number of samples actually written. Samples that don't fit
    /// are counted as dropped.
    pub fn push_audio_slice(&mut self, data: &[f32]) -> usize {
        let written = self.audio_producer.push_slice(data);
        let dropped = data.len() - written;
        if dropped > 0 {
            self.shared
                .dropped_samples
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }
        self.shared
            .processed_samples
            .fetch_add(written as u64, Ordering::Relaxed);
        written
    }

    /// Try to push a latency measurement (lock-free, non-blocking)
    pub fn push_latency(&mut self, latency_ns: u64) -> Result<(), u64> {
        self.latency_producer.try_push(latency_ns)
    }
}

/// Consumer half of the audio communication channel.
///
/// Owned by the processing thread. All operations are lock-free.
pub struct AudioConsumer {
    /// Lock-free consumer for raw f32 audio samples
    audio_consumer: <HeapRb<f32> as Split>::Cons,
    /// Lock-free consumer for latency measurements
    latency_consumer: <HeapRb<u64> as Split>::Cons,
    /// Shared atomic state
    pub shared: AudioCommShared,
}

impl AudioConsumer {
    /// Pop up to `buf.len()` raw f32 samples from the ring buffer.
    ///
    /// Returns the number of samples actually read.
    pub fn pop_audio_slice(&mut self, buf: &mut [f32]) -> usize {
        self.audio_consumer.pop_slice(buf)
    }

    /// Get the number of available audio samples (individual f32 values) in the buffer.
    pub fn audio_samples_available(&self) -> usize {
        self.audio_consumer.occupied_len()
    }

    /// Get the audio buffer capacity.
    pub fn audio_buffer_capacity(&self) -> usize {
        self.audio_consumer.capacity().get()
    }

    /// Get the number of available latency measurements in the buffer.
    pub fn latency_measurements_available(&self) -> usize {
        self.latency_consumer.occupied_len()
    }

    /// Try to pop a latency measurement (lock-free, non-blocking)
    pub fn pop_latency(&mut self) -> Option<u64> {
        self.latency_consumer.try_pop()
    }

    /// Get buffer usage statistics
    pub fn get_buffer_stats(&self) -> BufferStats {
        let audio_occupied = self.audio_consumer.occupied_len();
        let audio_capacity = self.audio_consumer.capacity().get();
        let latency_occupied = self.latency_consumer.occupied_len();

        BufferStats {
            audio_buffer_used: audio_occupied,
            audio_buffer_capacity: audio_capacity,
            audio_buffer_usage_percent: if audio_capacity > 0 {
                (audio_occupied as f64 / audio_capacity as f64) * 100.0
            } else {
                0.0
            },
            latency_buffer_used: latency_occupied,
            dropped_samples: self.shared.dropped_samples.load(Ordering::Relaxed),
            processed_samples: self.shared.processed_samples.load(Ordering::Relaxed),
        }
    }
}

/// Create a matched pair of lock-free audio producer and consumer.
///
/// # Parameters
/// - `audio_buffer_size`: Size of the audio sample ring buffer in f32 values
/// - `latency_buffer_size`: Size of the latency measurement ring buffer
pub fn audio_comm_pair(
    audio_buffer_size: usize,
    latency_buffer_size: usize,
) -> (AudioProducer, AudioConsumer) {
    let audio_rb = HeapRb::<f32>::new(audio_buffer_size);
    let (audio_prod, audio_cons) = audio_rb.split();

    let latency_rb = HeapRb::<u64>::new(latency_buffer_size);
    let (latency_prod, latency_cons) = latency_rb.split();

    let shared = AudioCommShared::new();

    let producer = AudioProducer {
        audio_producer: audio_prod,
        latency_producer: latency_prod,
        shared: shared.clone(),
    };

    let consumer = AudioConsumer {
        audio_consumer: audio_cons,
        latency_consumer: latency_cons,
        shared,
    };

    (producer, consumer)
}

/// Buffer usage statistics
#[derive(Debug, Clone)]
pub struct BufferStats {
    /// Number of audio samples currently in buffer
    pub audio_buffer_used: usize,
    /// Total audio buffer capacity
    pub audio_buffer_capacity: usize,
    /// Audio buffer usage as percentage
    pub audio_buffer_usage_percent: f64,
    /// Number of latency measurements currently in buffer
    pub latency_buffer_used: usize,
    /// Total number of dropped samples
    pub dropped_samples: u64,
    /// Total number of processed samples
    pub processed_samples: u64,
}

impl BufferStats {
    /// Check if the audio buffer is nearly full (>80%)
    pub fn is_audio_buffer_nearly_full(&self) -> bool {
        self.audio_buffer_usage_percent > 80.0
    }

    /// Check if there are buffer overruns happening
    pub fn has_buffer_overruns(&self) -> bool {
        self.dropped_samples > 0
    }

    /// Get the sample drop rate as a percentage
    pub fn drop_rate_percent(&self) -> f64 {
        let total = self.dropped_samples + self.processed_samples;
        if total > 0 {
            (self.dropped_samples as f64 / total as f64) * 100.0
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_comm_creation() {
        let (_producer, consumer) =
            audio_comm_pair(DEFAULT_AUDIO_BUFFER_SIZE, DEFAULT_LATENCY_BUFFER_SIZE);
        assert_eq!(consumer.audio_samples_available(), 0);
        assert_eq!(consumer.latency_measurements_available(), 0);
        assert!(!consumer.shared.should_stop());
    }

    #[test]
    fn test_audio_sample_transfer() {
        let (mut producer, mut consumer) =
            audio_comm_pair(DEFAULT_AUDIO_BUFFER_SIZE, DEFAULT_LATENCY_BUFFER_SIZE);

        let data = [0.1f32, 0.2, 0.3, 0.4];
        let written = producer.push_audio_slice(&data);
        assert_eq!(written, 4);
        assert_eq!(consumer.audio_samples_available(), 4);

        let mut buf = [0.0f32; 4];
        let read = consumer.pop_audio_slice(&mut buf);
        assert_eq!(read, 4);
        assert_eq!(buf, data);
        assert_eq!(consumer.audio_samples_available(), 0);
    }

    #[test]
    fn test_buffer_overflow() {
        let (mut producer, consumer) = audio_comm_pair(4, DEFAULT_LATENCY_BUFFER_SIZE);

        // Write more samples than capacity
        let data = [0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6];
        let written = producer.push_audio_slice(&data);

        // Ring buffer capacity is 4, so at most 4 written
        assert!(written <= 4);
        let stats = consumer.get_buffer_stats();
        assert!(stats.dropped_samples > 0);
    }

    #[test]
    fn test_latency_measurement() {
        let (mut producer, mut consumer) =
            audio_comm_pair(DEFAULT_AUDIO_BUFFER_SIZE, DEFAULT_LATENCY_BUFFER_SIZE);

        let latency_ns = 500_000u64; // 0.5ms
        assert!(producer.push_latency(latency_ns).is_ok());
        assert_eq!(consumer.latency_measurements_available(), 1);

        let received = consumer.pop_latency().unwrap();
        assert_eq!(received, latency_ns);
        assert_eq!(consumer.latency_measurements_available(), 0);
    }

    #[test]
    fn test_stop_signal() {
        let (producer, consumer) =
            audio_comm_pair(DEFAULT_AUDIO_BUFFER_SIZE, DEFAULT_LATENCY_BUFFER_SIZE);

        assert!(!consumer.shared.should_stop());
        producer.shared.stop();
        assert!(consumer.shared.should_stop());
    }

    #[test]
    fn test_buffer_stats() {
        let (mut producer, consumer) = audio_comm_pair(10, DEFAULT_LATENCY_BUFFER_SIZE);

        let stats = consumer.get_buffer_stats();
        assert_eq!(stats.audio_buffer_capacity, 10);
        assert_eq!(stats.audio_buffer_used, 0);
        assert_eq!(stats.audio_buffer_usage_percent, 0.0);

        // Push 9 samples (90%)
        let data = [1.0f32; 9];
        producer.push_audio_slice(&data);

        let stats = consumer.get_buffer_stats();
        assert_eq!(stats.audio_buffer_used, 9);
        assert_eq!(stats.audio_buffer_usage_percent, 90.0);
        assert!(stats.is_audio_buffer_nearly_full());
    }
}
