use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::message_bus::ComponentId;

/// Reconnect/restart backoff (capped) — same shape as
/// `station_agent/mod.rs`'s `RECONNECT_BACKOFF_MIN`/`MAX`.
pub(crate) const RESTART_BACKOFF_MIN: Duration = Duration::from_secs(2);
pub(crate) const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// How many restarts a component gets before the supervisor gives up and
/// leaves it `DegradeOnly` (spec §4.3): 5 restarts per rolling 10-minute
/// window.
const MAX_RESTARTS_PER_WINDOW: usize = 5;
const RESTART_WINDOW: Duration = Duration::from_secs(600);

/// Per-component restart bookkeeping: restart timestamps (for the rolling
/// window) and the current backoff duration (reset to `RESTART_BACKOFF_MIN`
/// whenever the window has no restarts in it).
#[derive(Debug, Default)]
pub(crate) struct RestartBudget {
    attempts: HashMap<ComponentId, Vec<Instant>>,
    backoff: HashMap<ComponentId, Duration>,
}

impl RestartBudget {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Prune attempts older than `RESTART_WINDOW`, then report whether
    /// another restart is allowed right now for `id`.
    pub(crate) fn may_restart(&mut self, id: ComponentId, now: Instant) -> bool {
        let attempts = self.attempts.entry(id).or_default();
        attempts.retain(|&t| now.duration_since(t) < RESTART_WINDOW);
        attempts.len() < MAX_RESTARTS_PER_WINDOW
    }

    /// Record a restart attempt at `now` and return the backoff to sleep
    /// before actually restarting (doubles each call within the window,
    /// resets to `RESTART_BACKOFF_MIN` once the window has aged out).
    pub(crate) fn record_attempt_and_backoff(&mut self, id: ComponentId, now: Instant) -> Duration {
        let attempts = self.attempts.entry(id).or_default();
        attempts.retain(|&t| now.duration_since(t) < RESTART_WINDOW);
        attempts.push(now);
        let next = if attempts.len() <= 1 {
            RESTART_BACKOFF_MIN
        } else {
            let prev = *self.backoff.get(&id).unwrap_or(&RESTART_BACKOFF_MIN);
            (prev * 2).min(RESTART_BACKOFF_MAX)
        };
        self.backoff.insert(id, next);
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_five_restarts_in_window() {
        let mut budget = RestartBudget::new();
        let base = Instant::now();
        for _ in 0..5 {
            assert!(budget.may_restart(ComponentId::Qso, base));
            budget.record_attempt_and_backoff(ComponentId::Qso, base);
        }
        assert!(!budget.may_restart(ComponentId::Qso, base));
    }

    #[test]
    fn window_expiry_resets_the_budget() {
        let mut budget = RestartBudget::new();
        let base = Instant::now();
        for _ in 0..5 {
            budget.record_attempt_and_backoff(ComponentId::Qso, base);
        }
        assert!(!budget.may_restart(ComponentId::Qso, base));
        let later = base + Duration::from_secs(601);
        assert!(budget.may_restart(ComponentId::Qso, later));
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let mut budget = RestartBudget::new();
        let base = Instant::now();
        let b1 = budget.record_attempt_and_backoff(ComponentId::Qso, base);
        assert_eq!(b1, RESTART_BACKOFF_MIN);
        let b2 = budget.record_attempt_and_backoff(ComponentId::Qso, base + Duration::from_secs(1));
        assert_eq!(b2, RESTART_BACKOFF_MIN * 2);
        // Keep doubling past the cap — must clamp, never exceed MAX.
        for i in 3..10 {
            let b =
                budget.record_attempt_and_backoff(ComponentId::Qso, base + Duration::from_secs(i));
            assert!(b <= RESTART_BACKOFF_MAX);
        }
    }

    #[test]
    fn components_have_independent_budgets() {
        let mut budget = RestartBudget::new();
        let base = Instant::now();
        for _ in 0..5 {
            budget.record_attempt_and_backoff(ComponentId::Qso, base);
        }
        assert!(!budget.may_restart(ComponentId::Qso, base));
        assert!(budget.may_restart(ComponentId::DxCluster, base));
    }
}
