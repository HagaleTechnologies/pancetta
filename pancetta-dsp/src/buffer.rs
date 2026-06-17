use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, warn};

#[derive(Debug, Error)]
pub enum BufferError {
    #[error("Buffer overflow: dropped {count} samples")]
    Overflow { count: usize },
    #[error("Buffer underflow: insufficient data available")]
    Underflow,
    #[error("Invalid buffer size: {size}")]
    InvalidSize { size: usize },
}

pub type Result<T> = std::result::Result<T, BufferError>;

/// High-performance ring buffer for continuous audio streaming
/// Uses a simple VecDeque with mutex for thread safety
pub struct AudioRingBuffer {
    /// Internal buffer using VecDeque
    buffer: Arc<Mutex<VecDeque<f32>>>,
    /// Sample rate of the audio data
    sample_rate: f32,
    /// Maximum capacity
    capacity: usize,
    /// Processing statistics
    stats: Arc<Mutex<BufferStats>>,
}

#[derive(Debug, Clone, Default)]
pub struct BufferStats {
    pub total_samples_written: u64,
    pub total_samples_read: u64,
    pub overflow_count: u64,
    pub underflow_count: u64,
    pub peak_occupancy: usize,
    pub current_occupancy: usize,
}

impl AudioRingBuffer {
    /// Create a new ring buffer with specified capacity
    pub fn new(sample_rate: f32, max_latency: f32) -> Result<Self> {
        let capacity = (sample_rate * max_latency) as usize;

        if capacity == 0 {
            return Err(BufferError::InvalidSize { size: capacity });
        }

        debug!(
            "Creating audio ring buffer: sample_rate={}, max_latency={}s, capacity={}",
            sample_rate, max_latency, capacity
        );

        Ok(Self {
            buffer: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            sample_rate,
            capacity,
            stats: Arc::new(Mutex::new(BufferStats::default())),
        })
    }

    /// Write audio samples to the buffer
    pub fn write(&self, samples: &[f32]) -> Result<usize> {
        let mut buffer = self.buffer.lock();
        let available_space = self.capacity - buffer.len();

        if samples.len() > available_space {
            // Buffer overflow - drop oldest samples
            let overflow_count = samples.len() - available_space;
            warn!(
                "Audio buffer overflow: dropping {} samples ({:.2}ms)",
                overflow_count,
                overflow_count as f32 / self.sample_rate * 1000.0
            );

            // Remove old samples to make space
            for _ in 0..overflow_count {
                buffer.pop_front();
            }

            // Update statistics
            {
                let mut stats = self.stats.lock();
                stats.overflow_count += 1;
            }
        }

        // Write samples to buffer
        buffer.extend(samples.iter());
        let written = samples.len();

        // Update statistics
        {
            let mut stats = self.stats.lock();
            stats.total_samples_written += written as u64;
            stats.current_occupancy = buffer.len();
            if stats.current_occupancy > stats.peak_occupancy {
                stats.peak_occupancy = stats.current_occupancy;
            }
        }

        debug!("Wrote {} samples to buffer", written);
        Ok(written)
    }

    /// Read audio samples from the buffer
    pub fn read(&self, output: &mut [f32]) -> Result<usize> {
        let mut buffer = self.buffer.lock();
        let available_samples = buffer.len();

        if output.len() > available_samples {
            // Buffer underflow
            debug!(
                "Audio buffer underflow: requested {} samples, only {} available",
                output.len(),
                available_samples
            );

            // Update statistics
            {
                let mut stats = self.stats.lock();
                stats.underflow_count += 1;
            }

            return Err(BufferError::Underflow);
        }

        // Read samples from buffer
        for sample in output.iter_mut() {
            if let Some(s) = buffer.pop_front() {
                *sample = s;
            } else {
                break;
            }
        }
        let read = output.len();

        // Update statistics
        {
            let mut stats = self.stats.lock();
            stats.total_samples_read += read as u64;
            stats.current_occupancy = buffer.len();
        }

        debug!("Read {} samples from buffer", read);
        Ok(read)
    }

    /// Try to read samples without blocking
    // rationale: the index `i` pairs each `pop_front` with its output slot and the
    // early `break` is load-bearing; an iterator rewrite would obscure that.
    #[allow(clippy::needless_range_loop)]
    pub fn try_read(&self, output: &mut [f32]) -> usize {
        let mut buffer = self.buffer.lock();
        let available = buffer.len().min(output.len());

        if available > 0 {
            for i in 0..available {
                if let Some(sample) = buffer.pop_front() {
                    output[i] = sample;
                } else {
                    break;
                }
            }

            // Update statistics
            {
                let mut stats = self.stats.lock();
                stats.total_samples_read += available as u64;
                stats.current_occupancy = buffer.len();
            }

            available
        } else {
            0
        }
    }

    /// Get the current buffer occupancy
    pub fn len(&self) -> usize {
        self.buffer.lock().len()
    }

    /// Check if buffer is empty
    pub fn is_empty(&self) -> bool {
        self.buffer.lock().is_empty()
    }

    /// Get buffer capacity
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Get current latency in seconds
    pub fn latency(&self) -> f32 {
        self.len() as f32 / self.sample_rate
    }

    /// Get buffer statistics
    pub fn stats(&self) -> BufferStats {
        self.stats.lock().clone()
    }

    /// Reset buffer statistics
    pub fn reset_stats(&self) {
        let mut stats = self.stats.lock();
        *stats = BufferStats::default();
    }

    /// Clear the buffer
    pub fn clear(&self) {
        let mut buffer = self.buffer.lock();
        buffer.clear();

        let mut stats = self.stats.lock();
        stats.current_occupancy = 0;
    }

    /// Get available space in samples
    pub fn available_space(&self) -> usize {
        self.capacity - self.len()
    }

    /// Check if buffer has enough samples for reading
    pub fn has_samples(&self, count: usize) -> bool {
        self.len() >= count
    }
}

/// Window extractor for FT8 signal processing
pub struct WindowExtractor {
    /// Sample rate of input audio
    sample_rate: f32,
    /// Window duration in seconds (12.64s for FT8)
    window_duration: f32,
    /// Overlap factor (typically 0.5 for 50% overlap)
    overlap_factor: f32,
    /// Samples per window
    window_size: usize,
    /// Step size between windows
    step_size: usize,
    /// Buffer for accumulating samples
    accumulator: Vec<f32>,
    /// Current position in accumulator
    position: usize,
}

impl WindowExtractor {
    /// Create a new window extractor
    pub fn new(sample_rate: f32, window_duration: f32, overlap_factor: f32) -> Self {
        let window_size = (sample_rate * window_duration) as usize;
        let step_size = (window_size as f32 * (1.0 - overlap_factor)) as usize;

        debug!(
            "Creating window extractor: sample_rate={}, window_duration={}s, window_size={}, step_size={}",
            sample_rate, window_duration, window_size, step_size
        );

        Self {
            sample_rate,
            window_duration,
            overlap_factor,
            window_size,
            step_size,
            accumulator: vec![0.0; window_size],
            position: 0,
        }
    }

    /// Create FT8-optimized window extractor
    pub fn new_ft8(sample_rate: f32) -> Self {
        Self::new(sample_rate, 12.64, 0.5)
    }

    /// Process audio samples and extract windows when ready
    pub fn process<F>(&mut self, input: &[f32], mut window_callback: F)
    where
        F: FnMut(&[f32]),
    {
        let mut input_pos = 0;

        while input_pos < input.len() {
            let remaining_in_window = self.window_size - self.position;
            let remaining_in_input = input.len() - input_pos;
            let to_copy = remaining_in_window.min(remaining_in_input);

            self.accumulator[self.position..self.position + to_copy]
                .copy_from_slice(&input[input_pos..input_pos + to_copy]);

            self.position += to_copy;
            input_pos += to_copy;

            if self.position >= self.window_size {
                window_callback(&self.accumulator);

                if self.step_size < self.window_size {
                    self.accumulator.copy_within(self.step_size.., 0);
                    self.position = self.window_size - self.step_size;
                } else {
                    self.position = 0;
                }
            }
        }
    }

    /// Get window size in samples
    pub fn window_size(&self) -> usize {
        self.window_size
    }

    /// Get step size in samples
    pub fn step_size(&self) -> usize {
        self.step_size
    }

    /// Get current position in accumulator
    pub fn position(&self) -> usize {
        self.position
    }

    /// Check if a complete window is ready
    pub fn is_window_ready(&self) -> bool {
        self.position >= self.window_size
    }
}
