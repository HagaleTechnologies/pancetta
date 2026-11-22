//! PAN-359: special-activity references written into logged QSOs.
//!
//! Two sources feed the ADIF `MY_SIG`/`MY_SIG_INFO`/`SIG`/`SIG_INFO`
//! fields, and both are applied in one place, `QsoManager::emit_event`,
//! just before `QsoEvent::QsoCompleted` is broadcast, so every consumer of
//! that event (the `qsos.adi` writer, the SQLite index, uploads, the
//! WSJT-X relay, N1MM) sees the same record:
//!
//! - [`ActivationReference`]: the operator's own programme and reference
//!   from `[station] my_sig` / `my_sig_info`, stamped on every QSO.
//! - [`WorkedReferenceBook`]: the worked station's programme and reference,
//!   filled from cqdx.io's activation feed by the coordinator and looked up
//!   by the partner's callsign at completion.
//!
//! Neither source overwrites a tag already on the QSO's metadata.

use chrono::{DateTime, Utc};
use pancetta_core::callsign::{base_callsign, resolve_hash_render};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// ADIF field for the operator's own special-activity programme.
pub const MY_SIG_TAG: &str = "MY_SIG";
/// ADIF field for the operator's own reference in that programme.
pub const MY_SIG_INFO_TAG: &str = "MY_SIG_INFO";
/// ADIF field for the worked station's special-activity programme.
pub const SIG_TAG: &str = "SIG";
/// ADIF field for the worked station's reference in that programme.
pub const SIG_INFO_TAG: &str = "SIG_INFO";

/// How long a worked-station activation stays usable after the feed last
/// listed it, measured to the QSO's end time (PAN-359 D5).
pub const WORKED_REFERENCE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(30 * 60);

fn max_age() -> chrono::Duration {
    chrono::Duration::from_std(WORKED_REFERENCE_MAX_AGE)
        .unwrap_or_else(|_| chrono::Duration::zero())
}

/// Trim `value`; a blank result means "unset".
fn non_blank(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Insert `key = value` unless the key is already present (in any case:
/// the ADIF writer would otherwise emit both) or `value` is blank. A tag
/// already on the QSO wins (PAN-359 D2).
fn stamp_tag(tags: &mut HashMap<String, String>, key: &str, value: Option<&str>) {
    if tags.keys().any(|k| k.eq_ignore_ascii_case(key)) {
        return;
    }
    if let Some(value) = non_blank(value) {
        tags.insert(key.to_string(), value);
    }
}

/// Is a sighting at `seen_at` within [`WORKED_REFERENCE_MAX_AGE`] of `at`?
/// Bounded in both directions, so a sighting stamped ahead of `at` (the
/// wall clock stepped back after the fetch) cannot stay fresh for longer
/// than the window either.
fn within_window(at: DateTime<Utc>, seen_at: DateTime<Utc>) -> bool {
    at.signed_duration_since(seen_at).abs() <= max_age()
}

/// The operator's own activation: `[station] my_sig` / `my_sig_info`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ActivationReference {
    /// Programme, written as ADIF `MY_SIG` (for example `"POTA"`).
    pub my_sig: Option<String>,
    /// Reference, written as ADIF `MY_SIG_INFO` (for example `"K-1234"`).
    pub my_sig_info: Option<String>,
}

impl ActivationReference {
    /// Build from the configured values: each is trimmed, a blank value
    /// becomes `None`, and case is kept.
    pub fn new(my_sig: Option<&str>, my_sig_info: Option<&str>) -> Self {
        Self {
            my_sig: non_blank(my_sig),
            my_sig_info: non_blank(my_sig_info),
        }
    }

    /// True when neither field would be written.
    pub fn is_empty(&self) -> bool {
        non_blank(self.my_sig.as_deref()).is_none()
            && non_blank(self.my_sig_info.as_deref()).is_none()
    }

    /// Add `MY_SIG` / `MY_SIG_INFO` to `tags`, each on its own, never
    /// replacing a value already there.
    pub fn stamp(&self, tags: &mut HashMap<String, String>) {
        stamp_tag(tags, MY_SIG_TAG, self.my_sig.as_deref());
        stamp_tag(tags, MY_SIG_INFO_TAG, self.my_sig_info.as_deref());
    }
}

/// A worked station's activation, written as ADIF `SIG` / `SIG_INFO`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkedReference {
    /// Programme (`"POTA"` or `"SOTA"`).
    pub sig: String,
    /// Reference in that programme (for example `"DA-0123"`).
    pub sig_info: String,
}

impl WorkedReference {
    /// Add `SIG` / `SIG_INFO` to `tags`, never replacing a value already
    /// there.
    pub fn stamp(&self, tags: &mut HashMap<String, String>) {
        stamp_tag(tags, SIG_TAG, Some(&self.sig));
        stamp_tag(tags, SIG_INFO_TAG, Some(&self.sig_info));
    }
}

#[derive(Debug, Clone)]
struct BookEntry {
    reference: WorkedReference,
    seen_at: DateTime<Utc>,
}

/// Normalize a callsign for the book: trimmed and upper-case.
fn normalize_call(callsign: &str) -> String {
    callsign.trim().to_ascii_uppercase()
}

/// Shared, cloneable map from a worked station's callsign to its current
/// activation, as last seen on the cqdx.io feed (PAN-359).
///
/// Clones share one map: the coordinator hands one clone to the cqdx bridge
/// (writer) and one to the `QsoManager` (reader). The lock is synchronous so
/// the manager can read it while it holds its own locks; a poisoned lock is
/// recovered, never propagated.
#[derive(Debug, Clone, Default)]
pub struct WorkedReferenceBook {
    inner: Arc<RwLock<HashMap<String, BookEntry>>>,
}

impl WorkedReferenceBook {
    /// Create an empty book.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one feed fetch taken at `seen_at`. Each listed callsign gets
    /// its reference and sighting time replaced; callsigns missing from this
    /// fetch keep their last value until it is more than
    /// [`WORKED_REFERENCE_MAX_AGE`] away from `seen_at` (either direction),
    /// when it is purged.
    pub fn record_feed(
        &self,
        entries: impl IntoIterator<Item = (String, WorkedReference)>,
        seen_at: DateTime<Utc>,
    ) {
        let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
        for (callsign, reference) in entries {
            let key = normalize_call(&callsign);
            if key.is_empty() {
                continue;
            }
            map.insert(key, BookEntry { reference, seen_at });
        }
        map.retain(|_, entry| within_window(seen_at, entry.seen_at));
    }

    /// The activation for `callsign` that the feed listed at most
    /// [`WORKED_REFERENCE_MAX_AGE`] before `at` (or, after a wall-clock step
    /// back, at most that long after it).
    ///
    /// The call is trimmed and upper-cased; a resolved hash render
    /// (`<DL1ABC>`) is looked up as the call inside it and an unresolved
    /// one (`<...>`) is never looked up. An exact match wins; otherwise the
    /// one fresh entry sharing the call's base callsign (`DL1ABC` for
    /// `DL1ABC/P`) matches; two or more such entries match nothing.
    pub fn lookup(&self, callsign: &str, at: DateTime<Utc>) -> Option<WorkedReference> {
        let normalized = normalize_call(callsign);
        let call = resolve_hash_render(&normalized)?;
        if call.is_empty() {
            return None;
        }
        let fresh = |entry: &BookEntry| within_window(at, entry.seen_at);
        let map = self.inner.read().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = map.get(call).filter(|e| fresh(e)) {
            return Some(entry.reference.clone());
        }
        let base = base_callsign(call);
        if base.is_empty() {
            return None;
        }
        let mut matches = map
            .iter()
            .filter(|(key, entry)| fresh(entry) && base_callsign(key) == base);
        let (_, first) = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(first.reference.clone())
    }
}

#[cfg(test)]
#[path = "sig_reference/tests/activation_reference_tests.rs"]
mod activation_reference_tests;

#[cfg(test)]
#[path = "sig_reference/tests/worked_reference_book_tests.rs"]
mod worked_reference_book_tests;
