//! Short-lived (~2-3 minute) per-callsign memory of `PerBandDxccNew`+/`Atno`
//! CQs heard but not pounced on. See
//! `docs/superpowers/specs/2026-07-24-dx-watchlist-design.md`.
//!
//! Never triggers a transmission by itself — it is a one-way read off the
//! CQ-scoring pass in `AutonomousOperator::feed_decoded_messages_at`, purely
//! bookkeeping. A watchlisted callsign only ever gets pounced on the
//! ordinary way: by being freshly, actively re-decoded as a CQ on a later
//! cycle.

use crate::priority::PriorityTier;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;

/// One remembered near-miss.
#[derive(Debug, Clone, PartialEq)]
pub struct DxWatchlistEntry {
    pub callsign: String,
    pub grid: Option<String>,
    pub tier: PriorityTier,
    pub last_heard: DateTime<Utc>,
}

/// Short-lived memory of `PerBandDxccNew`+ CQs heard but not pounced on.
#[derive(Debug, Clone)]
pub struct DxWatchlist {
    entries: HashMap<String, DxWatchlistEntry>,
    ttl: Duration,
}

impl DxWatchlist {
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            ttl,
        }
    }

    /// Insert or refresh an entry (uppercased callsign key). Overwrites any
    /// existing entry for the same callsign, including its tier — a station
    /// re-heard at a different tier reflects the current, more accurate
    /// classification.
    pub fn refresh(
        &mut self,
        callsign: &str,
        grid: Option<&str>,
        tier: PriorityTier,
        now: DateTime<Utc>,
    ) {
        let key = callsign.to_uppercase();
        self.entries.insert(
            key.clone(),
            DxWatchlistEntry {
                callsign: key,
                grid: grid.map(|g| g.to_uppercase()),
                tier,
                last_heard: now,
            },
        );
    }

    /// Drop entries not refreshed within the TTL.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        let ttl = self.ttl;
        self.entries
            .retain(|_, e| now.signed_duration_since(e.last_heard) < ttl);
    }

    /// Remove a specific callsign immediately (e.g. once worked).
    pub fn remove(&mut self, callsign: &str) {
        self.entries.remove(&callsign.to_uppercase());
    }

    /// Currently-watchlisted callsigns (uppercased), for TUI/status surfacing.
    pub fn callsigns(&self) -> Vec<String> {
        let mut callsigns: Vec<String> = self.entries.keys().cloned().collect();
        callsigns.sort();
        callsigns
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).unwrap()
    }

    #[test]
    fn refresh_inserts_new_entry() {
        let mut wl = DxWatchlist::new(Duration::seconds(150));
        wl.refresh("JA1ABC", Some("PM95"), PriorityTier::PerBandDxccNew, t(0));
        assert_eq!(wl.len(), 1);
        assert_eq!(wl.callsigns(), vec!["JA1ABC".to_string()]);
    }

    #[test]
    fn refresh_is_case_insensitive_and_uppercases_grid() {
        let mut wl = DxWatchlist::new(Duration::seconds(150));
        wl.refresh("ja1abc", Some("pm95"), PriorityTier::Atno, t(0));
        assert_eq!(wl.callsigns(), vec!["JA1ABC".to_string()]);
    }

    #[test]
    fn refresh_twice_updates_last_heard_without_duplicating() {
        let mut wl = DxWatchlist::new(Duration::seconds(150));
        wl.refresh("JA1ABC", None, PriorityTier::PerBandDxccNew, t(0));
        wl.refresh("JA1ABC", None, PriorityTier::Atno, t(10));
        assert_eq!(wl.len(), 1, "same callsign must not duplicate");
    }

    #[test]
    fn prune_removes_expired_entries() {
        let mut wl = DxWatchlist::new(Duration::seconds(150));
        wl.refresh("JA1ABC", None, PriorityTier::PerBandDxccNew, t(0));
        wl.prune(t(151));
        assert!(wl.is_empty(), "entry older than TTL must be pruned");
    }

    #[test]
    fn prune_keeps_entries_within_ttl() {
        let mut wl = DxWatchlist::new(Duration::seconds(150));
        wl.refresh("JA1ABC", None, PriorityTier::PerBandDxccNew, t(0));
        wl.prune(t(149));
        assert_eq!(wl.len(), 1, "entry within TTL must survive prune");
    }

    #[test]
    fn remove_deletes_a_specific_entry() {
        let mut wl = DxWatchlist::new(Duration::seconds(150));
        wl.refresh("JA1ABC", None, PriorityTier::PerBandDxccNew, t(0));
        wl.refresh("W1XYZ", None, PriorityTier::Atno, t(0));
        wl.remove("ja1abc");
        assert_eq!(wl.callsigns(), vec!["W1XYZ".to_string()]);
    }

    #[test]
    fn prune_removes_entry_at_exact_ttl_boundary() {
        let mut wl = DxWatchlist::new(Duration::seconds(150));
        wl.refresh("JA1ABC", None, PriorityTier::PerBandDxccNew, t(0));
        wl.prune(t(150));
        assert!(
            wl.is_empty(),
            "entry must be pruned at exactly TTL boundary (now - last_heard == ttl)"
        );
    }
}
