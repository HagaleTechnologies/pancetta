//! Parallel decode configuration and budget management.

use std::time::{Duration, Instant};

/// Controls decode parallelism and resource budget.
#[derive(Debug, Clone)]
pub struct DecodeConfig {
    /// Maximum candidates on pass 0
    pub max_candidates_pass0: usize,
    /// Maximum candidates on passes 1+
    pub max_candidates_pass_n: usize,
    /// Maximum decode passes (including signal subtraction)
    pub max_decode_passes: usize,
    /// Parallelism strategy
    pub parallelism: Parallelism,
    /// Hard wall-clock budget in milliseconds. Decode stops when exceeded.
    pub budget_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub enum Parallelism {
    /// Single-threaded decode (for debugging, testing, low-power devices)
    Serial,
    /// Rayon work-stealing thread pool
    Rayon {
        /// Max threads (None = use all available cores)
        max_threads: Option<usize>,
    },
}

impl Default for DecodeConfig {
    fn default() -> Self {
        Self {
            max_candidates_pass0: 100,
            max_candidates_pass_n: 40,
            max_decode_passes: 3,
            parallelism: Parallelism::Rayon { max_threads: None },
            budget_ms: 2000,
        }
    }
}

impl DecodeConfig {
    /// Create a serial config (for testing/debugging)
    pub fn serial() -> Self {
        Self {
            parallelism: Parallelism::Serial,
            ..Default::default()
        }
    }
}

/// Budget tracker — checks if we've exceeded our time allocation.
#[derive(Debug, Clone)]
pub struct BudgetTracker {
    deadline: Instant,
}

impl BudgetTracker {
    pub fn new(budget_ms: u64) -> Self {
        Self {
            deadline: Instant::now() + Duration::from_millis(budget_ms),
        }
    }

    /// Returns true if we've exceeded the budget.
    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Remaining time in milliseconds.
    pub fn remaining_ms(&self) -> u64 {
        self.deadline
            .checked_duration_since(Instant::now())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_tracker_not_expired() {
        let tracker = BudgetTracker::new(10_000);
        assert!(!tracker.expired());
        assert!(tracker.remaining_ms() > 9_000);
    }

    #[test]
    fn test_budget_tracker_zero_budget() {
        let tracker = BudgetTracker::new(0);
        // May or may not be expired depending on timing
        assert!(tracker.remaining_ms() == 0);
    }

    #[test]
    fn test_decode_config_default() {
        let config = DecodeConfig::default();
        assert_eq!(config.max_candidates_pass0, 100);
        assert_eq!(config.max_candidates_pass_n, 40);
        assert_eq!(config.budget_ms, 2000);
        assert!(matches!(config.parallelism, Parallelism::Rayon { .. }));
    }

    #[test]
    fn test_decode_config_serial() {
        let config = DecodeConfig::serial();
        assert!(matches!(config.parallelism, Parallelism::Serial));
    }
}
