//! Wall-clock decode budget (Phase 2: "anytime decoder" plumbing).
//!
//! [`DecodeBudget`] is a deadline threaded through a decode window so later
//! stages can check whether there's still time to do low-value optional
//! work. Today it gates the optional S2-S7 candidate stages, end-of-pass
//! subtraction and every multipass pass after the first (PAN-165: the last
//! two via [`DecodeBudget::has_time_for`]). `DecodeBudget::unlimited()` (used
//! by every existing entry point) must remain the default so tests, CI, and
//! the research harness are unaffected and fully deterministic.

use std::time::{Duration, Instant};

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

    /// Deadline this budget expires at (`None` = unlimited).
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Time left before the deadline (`None` = unlimited; `Some(ZERO)` once expired).
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// True when at least `needed` remains before the deadline. Unlimited budgets
    /// always have time. `has_time_for(Duration::ZERO) == has_time()`.
    #[inline]
    pub fn has_time_for(&self, needed: Duration) -> bool {
        match self.deadline {
            None => true,
            Some(d) => Instant::now().checked_add(needed).is_some_and(|t| t < d),
        }
    }
}

/// Per-window telemetry filled in by the stage driver.
#[derive(Debug, Clone, Default)]
pub struct DecodeBudgetReport {
    /// (stage label, elapsed ms, items done, items skipped)
    pub stages: Vec<(&'static str, u32, u32, u32)>,
    /// Set when the budget ran out before all optional work completed.
    pub budget_exhausted: bool,
    /// Multipass-loop iterations that started (passed every pass gate).
    /// `0` only for reports not produced by a decode call.
    pub passes_run: u32,
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

    #[test]
    fn unlimited_has_no_deadline_and_has_time_for_anything() {
        let b = DecodeBudget::unlimited();
        assert!(b.deadline().is_none());
        assert!(b.remaining().is_none());
        assert!(b.has_time_for(Duration::from_secs(3600)));
    }

    #[test]
    fn has_time_for_compares_needed_against_remaining() {
        let b = DecodeBudget::until(Instant::now() + Duration::from_secs(3600));
        assert!(b.has_time_for(Duration::from_secs(1)));
        assert!(!b.has_time_for(Duration::from_secs(7200)));
        assert!(b.remaining().unwrap() > Duration::from_secs(3500));
    }

    #[test]
    fn expired_budget_has_zero_remaining_and_no_time_for_zero() {
        let b = DecodeBudget::until(Instant::now() - Duration::from_millis(1));
        assert_eq!(b.remaining(), Some(Duration::ZERO));
        assert!(!b.has_time_for(Duration::ZERO));
        assert_eq!(b.has_time_for(Duration::ZERO), b.has_time());
    }

    #[test]
    fn has_time_for_never_panics_on_overflow() {
        let b = DecodeBudget::until(Instant::now() + Duration::from_secs(60));
        assert!(!b.has_time_for(Duration::MAX));
    }
}
