//! Persistent operator pins and short-lived (~2-3 minute) per-callsign memory of `PerBandDxccNew`+/`Atno`
//! CQs heard but not pounced on. See
//! `docs/superpowers/specs/2026-07-24-dx-watchlist-design.md`.
//!
//! Never triggers a transmission by itself — it is a one-way read off the
//! CQ-scoring pass in `AutonomousOperator::feed_decoded_messages_at`, purely
//! bookkeeping. A watchlisted callsign only ever gets pounced on the
//! ordinary way: by being freshly, actively re-decoded as a CQ on a later
//! cycle. Pins drive alerts only, never TX decisions.

use crate::priority::PriorityTier;
use chrono::{DateTime, Duration, Utc};
use std::collections::{BTreeSet, HashMap};

/// One remembered near-miss.
#[derive(Debug, Clone, PartialEq)]
pub struct DxWatchlistEntry {
    pub callsign: String,
    pub grid: Option<String>,
    pub tier: PriorityTier,
    pub last_heard: DateTime<Utc>,
}

/// Operator pins plus TTL memory of `PerBandDxccNew`+ near-miss CQs.
#[derive(Debug, Clone)]
pub struct DxWatchlist {
    entries: HashMap<String, DxWatchlistEntry>,
    ttl: Duration,
    pinned: BTreeSet<String>,
}

impl DxWatchlist {
    pub fn new(ttl: Duration) -> Self {
        Self::with_pinned(ttl, std::iter::empty::<&str>())
    }

    /// Seed persistent operator pins; transient entries keep their own TTL.
    pub fn with_pinned<I, S>(ttl: Duration, pinned: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut list = Self {
            entries: HashMap::new(),
            ttl,
            pinned: BTreeSet::new(),
        };
        for call in pinned {
            list.pin(call.as_ref());
        }
        list
    }

    pub fn pin(&mut self, callsign: &str) {
        let key = callsign.trim().to_uppercase();
        if !key.is_empty() {
            self.pinned.insert(key);
        }
    }

    /// Operator removal and completion both remove transient memory too.
    pub fn unpin(&mut self, callsign: &str) {
        self.remove(callsign);
    }

    pub fn pinned_callsigns(&self) -> Vec<String> {
        self.pinned.iter().cloned().collect()
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
        let key = callsign.trim().to_uppercase();
        self.entries.remove(&key);
        self.pinned.remove(&key);
    }

    /// Currently-watchlisted callsigns (uppercased), for TUI/status surfacing.
    pub fn callsigns(&self) -> Vec<String> {
        let mut callsigns: Vec<String> = self
            .entries
            .keys()
            .chain(self.pinned.iter())
            .cloned()
            .collect();
        callsigns.sort();
        callsigns.dedup();
        callsigns
    }

    pub fn len(&self) -> usize {
        self.callsigns().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.pinned.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).unwrap()
    }

    #[test]
    fn pins_survive_ttl_and_seed_normalizes_and_deduplicates() {
        let mut wl = DxWatchlist::with_pinned(Duration::seconds(150), [" vp8pj ", "VP8PJ"]);
        wl.refresh("T88AA", None, PriorityTier::Atno, t(0));
        wl.prune(t(151));
        assert_eq!(wl.callsigns(), vec!["VP8PJ"]);
        assert_eq!(wl.pinned_callsigns(), vec!["VP8PJ"]);
        assert_eq!(wl.len(), 1);
    }

    #[test]
    fn pin_union_is_sorted_and_unpin_removes_auto_entry_too() {
        let mut wl = DxWatchlist::new(Duration::seconds(150));
        wl.refresh("VP8PJ", None, PriorityTier::Atno, t(0));
        wl.pin("vp8pj");
        wl.pin("T88AA");
        assert_eq!(wl.callsigns(), vec!["T88AA", "VP8PJ"]);
        assert_eq!(wl.pinned_callsigns(), vec!["T88AA", "VP8PJ"]);
        wl.unpin("vp8pj");
        wl.unpin("T88AA");
        assert!(wl.is_empty());
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
