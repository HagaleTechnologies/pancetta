//! Wall-clock decode budget (Phase 2: "anytime decoder" plumbing).
//!
//! [`DecodeBudget`] is a deadline threaded through a decode window so later
//! stages can check whether there's still time to do low-value optional
//! work. This module is pure plumbing (Task 8 of the decoder-speed-overhaul
//! plan): nothing in the decode pipeline consults [`DecodeBudget::has_time`]
//! yet — that starts in Task 9. `DecodeBudget::unlimited()` (used by every
//! existing entry point) must remain the default so tests, CI, and the
//! research harness are unaffected and fully deterministic.

use std::time::Instant;

/// Wall-clock budget for one decode window. `deadline == None` is unlimited
/// (tests, research harness, `max` preset). Checked BETWEEN work items only.
#[derive(Debug, Clone, Copy)]
pub struct DecodeBudget {
    deadline: Option<Instant>,
}

impl DecodeBudget {
    /// No deadline: `has_time()` always returns `true`. This is the only
    /// budget used today (every existing decode entry point is
    /// byte-identical because of it).
    pub fn unlimited() -> Self {
        Self { deadline: None }
    }

    /// A budget that expires at `deadline`.
    pub fn until(deadline: Instant) -> Self {
        Self {
            deadline: Some(deadline),
        }
    }

    /// True when work may continue.
    #[inline]
    pub fn has_time(&self) -> bool {
        self.deadline.map(|d| Instant::now() < d).unwrap_or(true)
    }
}

/// Per-window telemetry filled in by the stage driver.
#[derive(Debug, Clone, Default)]
pub struct DecodeBudgetReport {
    /// (stage label, elapsed ms, items done, items skipped)
    pub stages: Vec<(&'static str, u32, u32, u32)>,
    /// Set when the budget ran out before all optional work completed.
    pub budget_exhausted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_always_has_time() {
        assert!(DecodeBudget::unlimited().has_time());
    }

    #[test]
    fn expired_deadline_has_no_time() {
        let b = DecodeBudget::until(Instant::now() - std::time::Duration::from_millis(1));
        assert!(!b.has_time());
    }
}
