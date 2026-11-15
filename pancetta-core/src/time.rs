//! UTC source shared by scheduling and QSO management.
use chrono::{DateTime, Utc};
use std::sync::Arc;

/// Thread-safe UTC source; production uses the system clock.
pub type UtcClock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Construct the production UTC source.
pub fn system_utc_clock() -> UtcClock {
    Arc::new(Utc::now)
}

/// A reproducible even-slot origin for simulations.
pub fn slot_grid_origin() -> DateTime<Utc> {
    DateTime::from_timestamp(1_767_225_600, 0).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn system_utc_clock_advances() {
        let clock = system_utc_clock();
        let first = clock();
        std::thread::sleep(std::time::Duration::from_millis(1));
        assert!(clock() > first);
    }
    #[test]
    fn slot_grid_origin_is_phase_zero() {
        let origin = slot_grid_origin();
        assert_eq!(origin.timestamp_millis().rem_euclid(30_000), 0);
        assert_eq!(
            crate::slot::SlotParity::of(origin),
            crate::slot::SlotParity::Even
        );
    }
}
