//! PAN-385: the autonomous band scheduler's data types and decision state.
//!
//! The scheduler moves bands for three reasons, evaluated every slot in this
//! order: (a) UTC entering a configured `[[autonomous.band_plan]]` window whose
//! bands do not include the current band, (b) needed activity — cqdx.io spots
//! show needed entities on another candidate band while no needed caller has
//! been heard on this band for `needed_idle_secs` — and (c) the original
//! low-activity round-robin. Grey-line (local sunrise/sunset at the station's
//! grid) breaks needed-activity ties toward bands below 14 MHz, and on a
//! plan-window entry picks the window's first band below 14 MHz.
//!
//! Every hop carries a [`BandHopReason`] whose [`BandHopReason::why_text`] is
//! the one-line Diagnostics overlay record.

use chrono::{DateTime, Utc};
use pancetta_core::utc_window::UtcWindow;
use pancetta_core::Band;
use serde::{Deserialize, Serialize};

/// A snapshot is ignored once it is older than this (seconds).
pub const OPPORTUNITY_MAX_AGE_SECS: i64 = 600;

/// One `[[autonomous.band_plan]]` window, parsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BandPlanEntry {
    /// UTC time-of-day window (start inclusive, end exclusive, may wrap).
    pub window: UtcWindow,
    /// Bands to operate on during the window, in preference order.
    pub bands: Vec<Band>,
}

/// Needed-entity counts cqdx.io spots show for one band.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BandOpportunity {
    /// The band.
    pub band: Band,
    /// Distinct needed DXCC entities spotted on the band (FT8/FT4, fresh).
    pub needed_entities: u32,
    /// How many of those are all-time new ones.
    pub atno_entities: u32,
}

/// All-band needed-activity snapshot pushed into the operator each slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpportunitySnapshot {
    /// When the underlying spots were fetched.
    pub as_of: DateTime<Utc>,
    /// One entry per candidate band whose needed set has loaded (zero
    /// counts included); a band with no entry has unknown counts.
    pub bands: Vec<BandOpportunity>,
}

impl OpportunitySnapshot {
    /// Counts for `band`, zero when absent.
    pub fn for_band(&self, band: Band) -> BandOpportunity {
        self.bands
            .iter()
            .find(|o| o.band == band)
            .copied()
            .unwrap_or(BandOpportunity {
                band,
                needed_entities: 0,
                atno_entities: 0,
            })
    }

    /// Counts for `band`, `None` when the snapshot has no entry for it (its
    /// needed set has not loaded, or it is not a candidate).
    pub fn get(&self, band: Band) -> Option<BandOpportunity> {
        self.bands.iter().find(|o| o.band == band).copied()
    }

    /// True when the snapshot is no older than [`OPPORTUNITY_MAX_AGE_SECS`].
    pub fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        (now - self.as_of).num_seconds() <= OPPORTUNITY_MAX_AGE_SECS
    }
}

/// Why a band hop happened; rendered as one Diagnostics overlay line.
#[derive(Debug, Clone, PartialEq)]
pub enum BandHopReason {
    /// The original trigger: no decodes for `silent_slots` slots.
    LowActivity {
        /// Band being left (`None` = unknown).
        from: Option<Band>,
        /// Destination band (`None` when the dial is outside the band table).
        to: Option<Band>,
        /// Silent slots counted.
        silent_slots: u32,
    },
    /// UTC entered a plan window whose bands exclude the current band.
    PlanWindow {
        /// Band being left (`None` = unknown).
        from: Option<Band>,
        /// Destination band.
        to: Band,
        /// The window entered.
        window: UtcWindow,
        /// The window's bands, in preference order.
        bands: Vec<Band>,
        /// Grey-line phase label when grey-line decided the pick.
        grey_line: Option<&'static str>,
    },
    /// cqdx spots show more needed entities on another band.
    NeededActivity {
        /// Band being left (`None` = unknown).
        from: Option<Band>,
        /// Destination band.
        to: Band,
        /// Needed entities spotted on `to`.
        to_needed: u32,
        /// ATNOs among them.
        to_atno: u32,
        /// Needed entities spotted on `from`.
        from_needed: u32,
        /// Minutes without a needed caller on `from`.
        idle_mins: u64,
        /// Grey-line phase label when grey-line decided the pick.
        grey_line: Option<&'static str>,
    },
}

fn band_text(band: Option<Band>) -> &'static str {
    band.map(|b| b.name()).unwrap_or("unknown")
}

impl BandHopReason {
    /// Band being left, when known.
    pub fn from_band(&self) -> Option<Band> {
        match self {
            BandHopReason::LowActivity { from, .. }
            | BandHopReason::PlanWindow { from, .. }
            | BandHopReason::NeededActivity { from, .. } => *from,
        }
    }

    /// Destination band, when known.
    pub fn to_band(&self) -> Option<Band> {
        match self {
            BandHopReason::LowActivity { to, .. } => *to,
            BandHopReason::PlanWindow { to, .. } | BandHopReason::NeededActivity { to, .. } => {
                Some(*to)
            }
        }
    }

    /// `"<from> -> <to>"` with `unknown` for an unknown band.
    pub fn route_text(&self) -> String {
        format!(
            "{} -> {}",
            band_text(self.from_band()),
            band_text(self.to_band())
        )
    }

    /// The one-line Diagnostics overlay text for this hop.
    pub fn why_text(&self) -> String {
        let route = self.route_text();
        match self {
            BandHopReason::LowActivity { silent_slots, .. } => {
                format!("Band hop {route}: low activity; no decodes for {silent_slots} slots")
            }
            BandHopReason::PlanWindow {
                window,
                bands,
                grey_line,
                to,
                ..
            } => {
                let names: Vec<&str> = bands.iter().map(|b| b.name()).collect();
                let mut text = format!(
                    "Band hop {route}: UTC plan window {window} ({})",
                    names.join(", ")
                );
                if let Some(phase) = grey_line {
                    text.push_str(&format!("; grey-line {phase} favours {}", to.name()));
                }
                text
            }
            BandHopReason::NeededActivity {
                from,
                to,
                to_needed,
                to_atno,
                from_needed,
                idle_mins,
                grey_line,
            } => {
                let noun = if *to_needed == 1 {
                    "entity"
                } else {
                    "entities"
                };
                let atno = if *to_atno > 0 {
                    format!(" ({to_atno} ATNO)")
                } else {
                    String::new()
                };
                let from_name = band_text(*from);
                let mut text = format!(
                    "Band hop {route}: needed activity; {to_needed} needed {noun} spotted on {}{atno}, \
                     {from_needed} on {from_name}; no needed caller heard on {from_name} for {idle_mins} min",
                    to.name()
                );
                if let Some(phase) = grey_line {
                    text.push_str(&format!("; grey-line {phase} favours {}", to.name()));
                }
                text
            }
        }
    }

    /// `"Band hop <from> -> <to> waiting: <why>"`.
    pub fn waiting_text(&self, why: &str) -> String {
        format!("Band hop {} waiting: {why}", self.route_text())
    }
}

/// True for bands whose upper edge is below 14 MHz (160m, 80m, 60m, 40m,
/// 30m): the bands grey-line favours.
pub fn is_low_band(band: Band) -> bool {
    matches!(
        band,
        Band::Band160m | Band::Band80m | Band::Band60m | Band::Band40m | Band::Band30m
    )
}

/// A hop emitted while the live dial was known, awaiting the dial that shows
/// the coordinator carried it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InFlightHop {
    target: Band,
    needed: bool,
    emitted_at: DateTime<Utc>,
}

/// PAN-385: the band scheduler's decision state, owned by
/// `BandStrategy`. Holds what the plan, needed-activity and grey-line
/// triggers remember between slots; the trigger order itself lives in
/// `BandStrategy::next_hop`, which also owns the low-activity counters.
#[derive(Debug, Clone, Default)]
pub struct BandScheduler {
    /// Plan window index active at the last evaluation (`None` = never
    /// evaluated, `Some(None)` = no window active).
    last_window: Option<Option<usize>>,
    /// Plan window whose entry still owes a move.
    pending_plan: Option<usize>,
    /// The last window entry was judged before any live dial was seen, so
    /// against the configured fallback band: judge it again against the
    /// first live dial.
    plan_entry_unconfirmed: bool,
    /// Band of the last non-zero live dial observed.
    live_band: Option<Band>,
    /// Destination of the last hop emitted while the live dial was unknown.
    assumed_band: Option<Band>,
    /// When the current band was first observed (or first evaluated).
    arrived_at: Option<DateTime<Utc>>,
    /// Last decoded CQ tiered `PerBandDxccNew` or higher.
    last_needed_heard: Option<DateTime<Utc>>,
    /// Start of the needed-activity dwell (last carried-out needed hop).
    last_needed_hop: Option<DateTime<Utc>>,
    /// A hop awaiting confirmation by the live dial.
    in_flight: Option<InFlightHop>,
    /// Latest cqdx needed-activity snapshot.
    snapshot: Option<OpportunitySnapshot>,
    /// A partner-answered exchange is in progress: no hop is emitted.
    exchange_in_progress: bool,
    /// Station latitude/longitude (grid centre) for grey-line.
    station_location: Option<(f64, f64)>,
    /// What the last `note_hop` replaced, for `undo_hop`.
    before_hop: Option<HopMarks>,
}

/// The state `note_hop` changes, kept so a hop that was not carried out
/// can be undone.
#[derive(Debug, Clone, Copy)]
struct HopMarks {
    assumed_band: Option<Band>,
    arrived_at: Option<DateTime<Utc>>,
    last_needed_hop: Option<DateTime<Utc>>,
    in_flight: Option<InFlightHop>,
}

impl BandScheduler {
    /// A scheduler with no history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the band of a non-zero live dial. A change of band restarts
    /// the idle clock; a dial on an in-flight hop's target confirms it and
    /// starts the needed-activity dwell. An in-flight hop the dial does not
    /// show was not carried out and is dropped, so the triggers re-evaluate.
    pub fn observe_band(&mut self, band: Band, now: DateTime<Utc>) {
        if self.live_band != Some(band) {
            self.live_band = Some(band);
            self.arrived_at = Some(now);
        }
        self.assumed_band = None;
        if let Some(hop) = self.in_flight.take() {
            if hop.target == band && hop.needed {
                self.last_needed_hop = Some(hop.emitted_at);
            }
        }
    }

    /// A decoded CQ tiered `PerBandDxccNew` or higher was heard.
    pub fn note_needed_heard(&mut self, now: DateTime<Utc>) {
        self.last_needed_heard = Some(now);
    }

    /// Replace the needed-activity snapshot.
    pub fn set_snapshot(&mut self, snapshot: Option<OpportunitySnapshot>) {
        self.snapshot = snapshot;
    }

    /// The latest needed-activity snapshot.
    pub fn snapshot(&self) -> Option<&OpportunitySnapshot> {
        self.snapshot.as_ref()
    }

    /// Set the exchange-in-progress gate.
    pub fn set_exchange_in_progress(&mut self, in_progress: bool) {
        self.exchange_in_progress = in_progress;
    }

    /// Whether the exchange-in-progress gate is closed.
    pub fn exchange_in_progress(&self) -> bool {
        self.exchange_in_progress
    }

    /// Set (or clear) the station location used for grey-line.
    pub fn set_station_location(&mut self, location: Option<(f64, f64)>) {
        self.station_location = location;
    }

    /// Band of the live dial, when one has been observed.
    pub fn live_band(&self) -> Option<Band> {
        self.live_band
    }

    /// The band the station is on: the live band, else the last hop's
    /// destination while the dial is unknown, else `fallback`.
    pub fn current_band(&self, fallback: Option<Band>) -> Option<Band> {
        self.live_band.or(self.assumed_band).or(fallback)
    }

    /// Grey-line phase label at the station now, when enabled and known.
    pub fn grey_line(&self, enabled: bool, now: DateTime<Utc>) -> Option<&'static str> {
        if !enabled {
            return None;
        }
        let (lat, lon) = self.station_location?;
        pancetta_core::solar::grey_line_phase(
            lat,
            lon,
            now,
            pancetta_core::solar::GREY_LINE_WINDOW_SECS,
        )
        .map(|phase| phase.label())
    }

    /// Start the idle clock at the first evaluation when no dial was seen.
    pub fn ensure_arrival(&mut self, now: DateTime<Utc>) {
        if self.arrived_at.is_none() {
            self.arrived_at = Some(now);
        }
    }

    /// Drop an in-flight hop no dial observation confirmed or refuted.
    pub fn expire_in_flight(&mut self) {
        self.in_flight = None;
    }

    /// Track plan-window entry (edge-triggered, including the first
    /// evaluation) and return the active window index and the window that
    /// still owes a move. An entry judged before any live dial was seen is
    /// judged again once, against the first live dial, so a configured
    /// fallback band that happens to be in the window cannot consume the
    /// entry for a rig that is really on another band.
    pub fn update_plan(
        &mut self,
        plan: &[BandPlanEntry],
        now: DateTime<Utc>,
        current: Option<Band>,
    ) -> (Option<usize>, Option<usize>) {
        let time = now.time();
        let active = plan.iter().position(|e| e.window.contains(time));
        let in_window = |i: usize| current.is_some_and(|b| plan[i].bands.contains(&b));
        if self.last_window != Some(active) {
            self.last_window = Some(active);
            self.plan_entry_unconfirmed = self.live_band.is_none();
            self.pending_plan = match active {
                Some(i) if !in_window(i) => Some(i),
                _ => None,
            };
        } else if self.plan_entry_unconfirmed && self.live_band.is_some() {
            self.plan_entry_unconfirmed = false;
            if let Some(i) = active.filter(|&i| !in_window(i)) {
                self.pending_plan = Some(i);
            }
        }
        if let Some(i) = self.pending_plan {
            if active != Some(i) || in_window(i) {
                self.pending_plan = None;
            }
        }
        (active, self.pending_plan)
    }

    /// Seconds since the later of arrival on this band and the last needed
    /// caller.
    pub fn idle_secs(&self, now: DateTime<Utc>) -> i64 {
        let start = match (self.arrived_at, self.last_needed_heard) {
            (Some(a), Some(h)) => a.max(h),
            (Some(a), None) => a,
            (None, Some(h)) => h,
            (None, None) => now,
        };
        (now - start).num_seconds()
    }

    /// Whether one hop interval has passed since the last needed hop.
    pub fn needed_dwell_passed(&self, now: DateTime<Utc>, hop_interval: chrono::Duration) -> bool {
        self.last_needed_hop.is_none_or(|t| now - t >= hop_interval)
    }

    /// Record an emitted hop. With a known live dial the hop waits for the
    /// dial to confirm it; with an unknown dial it is assumed carried out.
    pub fn note_hop(&mut self, to: Option<Band>, needed: bool, now: DateTime<Utc>) {
        self.before_hop = Some(HopMarks {
            assumed_band: self.assumed_band,
            arrived_at: self.arrived_at,
            last_needed_hop: self.last_needed_hop,
            in_flight: self.in_flight,
        });
        if self.live_band.is_some() {
            self.in_flight = to.map(|target| InFlightHop {
                target,
                needed,
                emitted_at: now,
            });
        } else {
            self.assumed_band = to;
            self.arrived_at = Some(now);
            if needed {
                self.last_needed_hop = Some(now);
            }
        }
    }

    /// The hop last noted was not carried out: put back what `note_hop`
    /// changed, so an unknown dial is not assumed to be on its target and a
    /// needed hop does not start the dwell. Does nothing twice.
    pub fn undo_hop(&mut self) {
        if let Some(marks) = self.before_hop.take() {
            self.assumed_band = marks.assumed_band;
            self.arrived_at = marks.arrived_at;
            self.last_needed_hop = marks.last_needed_hop;
            self.in_flight = marks.in_flight;
        }
    }
}

#[cfg(test)]
#[path = "band_scheduler/tests/why_text_tests.rs"]
mod why_text_tests;
