//! High-precision latency measurement for real-time audio
//! 
//! Provides tools to measure and analyze audio callback latency with nanosecond precision.
//! Critical for proving <1ms latency requirement for the Pancetta project.

use instant::Instant;
use std::collections::VecDeque;

/// High-precision latency measurement utilities
pub struct LatencyMeasurer {
    /// Ring buffer for storing recent latency measurements (nanoseconds)
    measurements: VecDeque<u64>,
    /// Maximum number of measurements to keep in memory
    max_measurements: usize,
    /// Running sum for efficient average calculation
    running_sum: u64,
    /// Count of measurements that exceeded the target latency
    excessive_latency_count: usize,
    /// Target latency threshold in nanoseconds (1ms = 1,000,000ns)
    target_latency_ns: u64,
}

impl LatencyMeasurer {
    /// Create a new latency measurer
    /// 
    /// # Parameters
    /// - `max_measurements`: Maximum number of measurements to keep (for rolling statistics)
    /// - `target_latency_ns`: Target latency threshold in nanoseconds (1ms = 1,000,000ns)
    pub fn new(max_measurements: usize, target_latency_ns: u64) -> Self {
        Self {
            measurements: VecDeque::with_capacity(max_measurements),
            max_measurements,
            running_sum: 0,
            excessive_latency_count: 0,
            target_latency_ns,
        }
    }
    
    /// Record a new latency measurement
    /// 
    /// # Parameters
    /// - `latency_ns`: Latency measurement in nanoseconds
    pub fn record_latency(&mut self, latency_ns: u64) {
        // Remove oldest measurement if at capacity
        if self.measurements.len() >= self.max_measurements {
            if let Some(old_measurement) = self.measurements.pop_front() {
                self.running_sum -= old_measurement;
                if old_measurement > self.target_latency_ns {
                    self.excessive_latency_count = self.excessive_latency_count.saturating_sub(1);
                }
            }
        }
        
        // Add new measurement
        self.measurements.push_back(latency_ns);
        self.running_sum += latency_ns;
        
        if latency_ns > self.target_latency_ns {
            self.excessive_latency_count += 1;
        }
    }
    
    /// Get the average latency in nanoseconds
    pub fn average_latency_ns(&self) -> Option<u64> {
        if self.measurements.is_empty() {
            None
        } else {
            Some(self.running_sum / self.measurements.len() as u64)
        }
    }
    
    /// Get the average latency in milliseconds
    pub fn average_latency_ms(&self) -> Option<f64> {
        self.average_latency_ns().map(|ns| ns as f64 / 1_000_000.0)
    }
    
    /// Get the minimum latency in nanoseconds
    pub fn min_latency_ns(&self) -> Option<u64> {
        self.measurements.iter().min().copied()
    }
    
    /// Get the maximum latency in nanoseconds
    pub fn max_latency_ns(&self) -> Option<u64> {
        self.measurements.iter().max().copied()
    }
    
    /// Get the minimum latency in milliseconds
    pub fn min_latency_ms(&self) -> Option<f64> {
        self.min_latency_ns().map(|ns| ns as f64 / 1_000_000.0)
    }
    
    /// Get the maximum latency in milliseconds
    pub fn max_latency_ms(&self) -> Option<f64> {
        self.max_latency_ns().map(|ns| ns as f64 / 1_000_000.0)
    }
    
    /// Get the percentage of measurements that exceeded the target latency
    pub fn excessive_latency_percentage(&self) -> f64 {
        if self.measurements.is_empty() {
            0.0
        } else {
            (self.excessive_latency_count as f64 / self.measurements.len() as f64) * 100.0
        }
    }
    
    /// Get the total number of measurements recorded
    pub fn measurement_count(&self) -> usize {
        self.measurements.len()
    }
    
    /// Check if the system is consistently meeting the latency target
    /// 
    /// Returns true if less than 1% of measurements exceed the target latency
    pub fn is_meeting_target(&self) -> bool {
        self.excessive_latency_percentage() < 1.0
    }
    
    /// Get comprehensive latency statistics
    pub fn get_stats(&self) -> LatencyStats {
        LatencyStats {
            count: self.measurement_count(),
            average_ns: self.average_latency_ns().unwrap_or(0),
            average_ms: self.average_latency_ms().unwrap_or(0.0),
            min_ns: self.min_latency_ns().unwrap_or(0),
            min_ms: self.min_latency_ms().unwrap_or(0.0),
            max_ns: self.max_latency_ns().unwrap_or(0),
            max_ms: self.max_latency_ms().unwrap_or(0.0),
            excessive_percentage: self.excessive_latency_percentage(),
            meeting_target: self.is_meeting_target(),
            target_latency_ns: self.target_latency_ns,
            target_latency_ms: self.target_latency_ns as f64 / 1_000_000.0,
        }
    }
}

/// Comprehensive latency statistics
#[derive(Debug, Clone)]
pub struct LatencyStats {
    /// Total number of measurements
    pub count: usize,
    /// Average latency in nanoseconds
    pub average_ns: u64,
    /// Average latency in milliseconds
    pub average_ms: f64,
    /// Minimum latency in nanoseconds
    pub min_ns: u64,
    /// Minimum latency in milliseconds
    pub min_ms: f64,
    /// Maximum latency in nanoseconds
    pub max_ns: u64,
    /// Maximum latency in milliseconds
    pub max_ms: f64,
    /// Percentage of measurements exceeding target
    pub excessive_percentage: f64,
    /// Whether the system is meeting the latency target
    pub meeting_target: bool,
    /// Target latency threshold in nanoseconds
    pub target_latency_ns: u64,
    /// Target latency threshold in milliseconds
    pub target_latency_ms: f64,
}

impl LatencyStats {
    /// Format the statistics for console output
    pub fn format_for_display(&self) -> String {
        format!(
            "Latency Statistics (Target: {:.3}ms):\n\
             • Measurements: {}\n\
             • Average: {:.3}ms ({} ns)\n\
             • Range: {:.3}ms - {:.3}ms\n\
             • Excessive: {:.1}% (>{:.3}ms)\n\
             • Meeting Target: {}",
            self.target_latency_ms,
            self.count,
            self.average_ms,
            self.average_ns,
            self.min_ms,
            self.max_ms,
            self.excessive_percentage,
            self.target_latency_ms,
            if self.meeting_target { "✅ YES" } else { "❌ NO" }
        )
    }
}

/// Timer for measuring callback execution time
/// 
/// Used within the audio callback to measure processing latency
pub struct CallbackTimer {
    start_time: Instant,
}

impl CallbackTimer {
    /// Start timing a callback
    pub fn start() -> Self {
        Self {
            start_time: Instant::now(),
        }
    }
    
    /// Get the elapsed time in nanoseconds since the timer was started
    pub fn elapsed_ns(&self) -> u64 {
        self.start_time.elapsed().as_nanos() as u64
    }
    
    /// Get the elapsed time in milliseconds since the timer was started
    pub fn elapsed_ms(&self) -> f64 {
        self.elapsed_ns() as f64 / 1_000_000.0
    }
}