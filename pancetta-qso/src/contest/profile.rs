//! Contest profile data model — see docs/superpowers/specs/2026-08-30-contest-mode-design.md §1.

use serde::{Deserialize, Serialize};

/// A catalog or operator-defined description of one contest's FT8 exchange
/// convention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContestProfile {
    /// Stable identifier, e.g. "us-state-qso-party". Stored verbatim into
    /// `ContestInfo::contest_name` (states.rs) when a QSO engages this
    /// profile, and used to key it back out of the catalog.
    pub id: String,

    /// Human-readable name for operator-facing UI (a later plan).
    pub display_name: String,

    /// CQ modifier text this contest is known to use, e.g. "KSQP" for
    /// `CQ KSQP K5ARH EM10`. Intentionally partial for hand-verified
    /// entries — extend via `contest.custom_profiles` config (a later
    /// plan) as more are confirmed.
    pub cq_tag_patterns: Vec<String>,

    /// Which wire/text shape this contest's exchange uses.
    pub exchange_shape: ExchangeShape,

    /// Whether this profile's exchange shape has been field-confirmed
    /// (live traffic or an official rules/setup document), as opposed to
    /// inferred. Every profile in `catalog::builtin_catalog()` is `true`;
    /// reserved for future not-yet-verified entries (e.g. WW Digi Contest,
    /// deliberately excluded from the catalog until confirmed — see the
    /// design doc §Background).
    pub verified: bool,

    /// Provenance for future maintenance — where the format assumption
    /// came from.
    pub source_notes: String,
}

/// Which FT8 exchange convention a [`ContestProfile`] uses.
///
/// Only `GridWithRAck` is implemented so far (PAN-49's actual bug). The
/// design doc (§1) also names `FieldDayClassSection`, `VhfContestGridReport`,
/// and `RstSerialOrState` for a later plan — deliberately not declared here
/// yet (YAGNI: an unimplemented enum variant is a compile-time placeholder).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExchangeShape {
    /// State QSO parties + ARRL International Digital Contest: plain grid
    /// exchanged both ways, then `"<to> <from> R <grid>"` as the ack —
    /// standing in for a numeric signal report.
    GridWithRAck,
}
