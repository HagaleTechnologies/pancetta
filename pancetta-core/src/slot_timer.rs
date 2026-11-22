//! Slot deadline timer on the shared [`SlotClock`](crate::SlotClock)
//! (PAN-358).
//!
//! A [`SlotTimer`] tracks "the next instant `offset` past a slot boundary"
//! from [`SlotReading`]s. It keeps PAN-72 Fix B (the due check comes before
//! a mode-mismatch recompute) and PAN-133 (recover one new-mode boundary
//! crossed between polls), and adds the PAN-358 rules:
//!
//! - a resync event (a wall-clock step of 1 s or more) re-anchors the
//!   deadline to the first phase after the event's `observed_at`, without
//!   firing, so every consumer lands in the same post-step slot however late
//!   it polls;
//! - a deadline more than one period late with no step re-anchors without
//!   firing ([`Reanchor::Lagged`]);
//! - a boundary fires at most once between resyncs, even after a small
//!   backward drift.
//!
//! [`SlotTimer::poll`] never moves past a due deadline; [`SlotTimer::advance`]
//! consumes it, so a consumer that is not ready yet stays due.

use crate::slot::{current_slot_start_with_period, next_phase_with_period};
use crate::slot_clock::{ClockEvent, SlotReading};
use chrono::{DateTime, Duration, Utc};

/// Why a poll re-anchored the deadline without firing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reanchor {
    /// A resync event on the shared clock.
    ClockStep(ClockEvent),
    /// The deadline was more than one period late with no step (a stalled
    /// consumer, buffer starvation, or a suspend the monotonic clock counted).
    Lagged {
        /// How far `now` was past the deadline.
        behind: Duration,
        /// The slot `now` fell in.
        slot_start: DateTime<Utc>,
    },
}

/// One [`SlotTimer::poll`] result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotPoll {
    /// The per-slot body should run (then call [`SlotTimer::advance`]).
    pub due: bool,
    /// Set when this poll re-anchored the deadline instead of firing.
    pub reanchor: Option<Reanchor>,
}

/// A UTC-anchored slot deadline (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotTimer {
    offset: Duration,
    deadline: DateTime<Utc>,
    deadline_slot_ns: i64,
    poll_slot_ns: i64,
    last_poll_now: DateTime<Utc>,
    last_fired: Option<DateTime<Utc>>,
    resync_epoch: u64,
}

fn phase_after(now: DateTime<Utc>, offset: Duration, slot_ns: i64) -> DateTime<Utc> {
    next_phase_with_period(now, offset, slot_ns)
}

impl SlotTimer {
    /// A timer whose first deadline is the first phase strictly after
    /// `reading.now`. `offset` must lie in `[0, slot_ns)`.
    pub fn new(reading: &SlotReading, offset: Duration, slot_ns: i64) -> Self {
        Self {
            offset,
            deadline: phase_after(reading.now, offset, slot_ns),
            deadline_slot_ns: slot_ns,
            poll_slot_ns: slot_ns,
            last_poll_now: reading.now,
            last_fired: None,
            resync_epoch: reading.resync_epoch,
        }
    }

    /// The tracked deadline.
    pub fn deadline(&self) -> DateTime<Utc> {
        self.deadline
    }

    /// Poll at `reading`, with the active period `slot_ns` read fresh by the
    /// caller.
    pub fn poll(&mut self, reading: &SlotReading, slot_ns: i64) -> SlotPoll {
        let now = reading.now;
        self.poll_slot_ns = slot_ns;

        // (1) A resync on the shared clock: re-anchor from the event instant.
        if reading.resync_epoch != self.resync_epoch {
            self.resync_epoch = reading.resync_epoch;
            if let Some(event) = reading.last_resync {
                self.deadline = phase_after(event.observed_at, self.offset, slot_ns);
                self.deadline_slot_ns = slot_ns;
                self.last_fired = None;
                self.last_poll_now = now;
                return SlotPoll {
                    due: false,
                    reanchor: Some(Reanchor::ClockStep(event)),
                };
            }
        }

        // (2) More than one period late with no step: re-anchor, no fire.
        let behind = now - self.deadline;
        if behind > Duration::nanoseconds(self.deadline_slot_ns) {
            self.deadline = phase_after(now, self.offset, slot_ns);
            self.deadline_slot_ns = slot_ns;
            self.last_poll_now = now;
            return SlotPoll {
                due: false,
                reanchor: Some(Reanchor::Lagged {
                    behind,
                    slot_start: current_slot_start_with_period(now, slot_ns),
                }),
            };
        }

        // (3) PAN-133: one new-mode boundary crossed between polls while the
        // old deadline is still ahead. A backward step cannot satisfy this
        // interval, and several crossed boundaries earn one tick.
        let crossed = (slot_ns != self.deadline_slot_ns)
            .then(|| current_slot_start_with_period(now - self.offset, slot_ns) + self.offset)
            .filter(|b| self.last_poll_now < *b && *b <= now);

        // (4) PAN-72 Fix B: due-ness against the tracked deadline first.
        let due = if now >= self.deadline {
            true
        } else if let Some(boundary) = crossed {
            self.deadline = boundary;
            self.deadline_slot_ns = slot_ns;
            true
        } else {
            if slot_ns != self.deadline_slot_ns {
                self.deadline = phase_after(now, self.offset, slot_ns);
                self.deadline_slot_ns = slot_ns;
            } else if self.deadline - now > Duration::nanoseconds(self.deadline_slot_ns) {
                // The clock moved back with no resync: return to the UTC
                // grid, never re-firing the last boundary.
                let from = self.last_fired.map_or(now, |fired| fired.max(now));
                self.deadline = phase_after(from, self.offset, slot_ns);
                self.deadline_slot_ns = slot_ns;
            }
            false
        };

        // (5)
        self.last_poll_now = now;
        SlotPoll {
            due,
            reanchor: None,
        }
    }

    /// Consume a due deadline: the next deadline is the first phase strictly
    /// after `reading.now` on the period seen by the latest poll.
    pub fn advance(&mut self, reading: &SlotReading) {
        if reading.now >= self.deadline {
            self.last_fired = Some(self.deadline);
        }
        self.deadline = phase_after(reading.now, self.offset, self.poll_slot_ns);
        self.deadline_slot_ns = self.poll_slot_ns;
    }
}

#[cfg(test)]
#[path = "slot_timer/tests/slot_timer_tests.rs"]
mod slot_timer_tests;
