//! Contest scoring rules and the pure scoring reducer (PAN-50).
//!
//! Scope: the two concrete contest editions named by the built-in
//! `us-state-qso-party` family profile — Kansas (KSQP) FT4/FT8 2026 and
//! South Carolina (SCQP) fixed digital 2026. Rule data is pinned to the
//! verified edition and carries provenance; later sponsor rule changes need
//! a new rule id, never a silent edit (see the sponsor-rule notes on each
//! [`ContestRules`] entry).
//!
//! This module is deliberately separate from exchange matching
//! (`matcher.rs`/`profile.rs`): a `ContestProfile` describes the *exchange*
//! (verified means exchange-verified), while a [`ContestRules`] entry
//! describes *scoring* (eligibility, duplicate keys, multipliers, points).
//! One rule path serves both the score reduction and contest-dupe queries so
//! log-time checking cannot drift from the displayed score.
//!
//! All functions here are pure over typed ledger facts; the coordinator owns
//! context selection and fact collection (never the current dial — identity
//! uses the logged band, as in PAN-260).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Rule id for the 2026 Kansas QSO Party FT4/FT8 category.
pub const KSQP_FT_2026: &str = "ksqp-ft-2026";
/// Rule id for the 2026 South Carolina QSO Party fixed digital category.
pub const SCQP_DIGITAL_FIXED_2026: &str = "scqp-digital-fixed-2026";

/// US state plus DC codes (region/multiplier validation: 50 states + DC).
pub const US_STATE_AND_DC_CODES: &[&str] = &[
    "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "DC", "FL", "GA", "HI", "ID", "IL", "IN", "IA",
    "KS", "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ", "NM",
    "NY", "NC", "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA",
    "WV", "WI", "WY",
];

/// Canadian province/territory codes (the VE multiplier set).
pub const CANADIAN_PROVINCE_CODES: &[&str] = &[
    "AB", "BC", "MB", "NB", "NL", "NS", "NT", "NU", "ON", "PE", "QC", "SK", "YT",
];

/// The `DX` operator-region sentinel.
pub const REGION_DX: &str = "DX";

/// Every region an `operator_region` (or partner region) may carry.
pub fn is_valid_region(region: &str) -> bool {
    let normalized = region.trim().to_ascii_uppercase();
    normalized == REGION_DX
        || US_STATE_AND_DC_CODES.contains(&normalized.as_str())
        || CANADIAN_PROVINCE_CODES.contains(&normalized.as_str())
}

/// One sponsor-designated bonus station (SCQP): its contacts add `weight`
/// points once per band/digital group, on top of the points × multipliers
/// formula.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContestBonusStation {
    /// Uppercase callsign, compared after normalization.
    pub callsign: &'static str,
    /// Published bonus weight in points.
    pub weight: u64,
}

/// Which concrete scoring formula applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RulesKind {
    /// Kansas QSO Party FT4/FT8: count eligible unique contacts, no
    /// multiplier factor.
    KansasFt,
    /// South Carolina QSO Party fixed digital: points × multipliers +
    /// per-band digital station bonuses.
    ScqpDigitalFixed,
}

/// A pinned scoring-rule edition with provenance.
#[derive(Debug, Clone, Copy)]
pub struct ContestRules {
    /// Stable rule id (`ksqp-ft-2026`, `scqp-digital-fixed-2026`).
    pub id: &'static str,
    /// Short display name for the score strip ("KSQP 2026").
    pub display_name: &'static str,
    /// The built-in family exchange profile these rules attach to.
    pub profile_id: &'static str,
    /// Which formula applies.
    pub kind: RulesKind,
    /// Bands that may earn credit (normalized uppercase, as
    /// `utils::frequency_to_band` produces).
    pub allowed_bands: &'static [&'static str],
    /// Modes that may earn credit (normalized uppercase).
    pub allowed_modes: &'static [&'static str],
    /// Sponsor bonus stations (SCQP); empty where the formula has none.
    pub bonus_stations: &'static [ContestBonusStation],
    /// Where the rule summary came from.
    pub source_notes: &'static str,
}

const KSQP_FT_2026_RULES: ContestRules = ContestRules {
    id: KSQP_FT_2026,
    display_name: "KSQP 2026",
    profile_id: "us-state-qso-party",
    kind: RulesKind::KansasFt,
    // Item 16 + general band/period rules: 160/80/40/20/15/10 m, separate
    // operating intervals.
    allowed_bands: &["160M", "80M", "40M", "20M", "15M", "10M"],
    allowed_modes: &["FT4", "FT8"],
    bonus_stations: &[],
    source_notes: "2026 Kansas QSO Party rules, FT4/8 item 16 and general \
         band/period rules: https://www.ksqsoparty.org/rules/KSQPRules2026.pdf",
};

const SCQP_DIGITAL_FIXED_2026_RULES: ContestRules = ContestRules {
    id: SCQP_DIGITAL_FIXED_2026,
    display_name: "SCQP 2026",
    profile_id: "us-state-qso-party",
    kind: RulesKind::ScqpDigitalFixed,
    // Sections 9.1–9.5 (revision 2.2.26) cover 160 m through 2 m; the FT
    // instructions group FT4/FT8 with the other digital modes for
    // duplicates.
    allowed_bands: &["160M", "80M", "40M", "20M", "15M", "10M", "6M", "2M"],
    allowed_modes: &["FT4", "FT8", "RTTY", "PSK31", "MFSK"],
    // Sponsor revision 2.2.26, section 9.3. Once per band/digital group.
    bonus_stations: &[
        ContestBonusStation {
            callsign: "W4CAE",
            weight: 350,
        },
        ContestBonusStation {
            callsign: "WW4SF",
            weight: 250,
        },
        ContestBonusStation {
            callsign: "K4YTZ",
            weight: 250,
        },
    ],
    source_notes: "SC QSO Party rules revision 2.2.26 sections 9.1–9.5 \
         (https://scqso.com/wp-content/uploads/2026/02/SCQPRULES2024_0126.pdf) \
         and FT8/FT4 instructions (https://scqso.com/ft8-4-instructions/)",
};

/// Every scoring-rule edition pancetta knows. Unknown ids are rejected, not
/// approximated.
pub fn known_rules() -> &'static [ContestRules] {
    &[KSQP_FT_2026_RULES, SCQP_DIGITAL_FIXED_2026_RULES]
}

/// Resolve a rule id to its pinned edition.
pub fn resolve_rules(rules_id: &str) -> Result<&'static ContestRules, ContestRuleError> {
    let trimmed = rules_id.trim();
    known_rules()
        .iter()
        .find(|rules| rules.id == trimmed)
        .ok_or(ContestRuleError::UnknownRulesId)
}

/// Errors from resolving contest configuration against the rule catalog.
/// The Display strings are the exact operator-facing validation messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContestRuleError {
    /// `contest: unknown rules_id`
    UnknownRulesId,
    /// `contest: operator_region is not valid for the selected rules`
    InvalidOperatorRegion,
    /// `contest: periods must be ordered, non-overlapping UTC ranges with
    /// start before end`
    InvalidPeriods,
}

impl fmt::Display for ContestRuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContestRuleError::UnknownRulesId => {
                write!(f, "contest: unknown rules_id")
            }
            ContestRuleError::InvalidOperatorRegion => {
                write!(
                    f,
                    "contest: operator_region is not valid for the selected rules"
                )
            }
            ContestRuleError::InvalidPeriods => {
                write!(
                    f,
                    "contest: periods must be ordered, non-overlapping UTC ranges with start \
                     before end"
                )
            }
        }
    }
}

impl std::error::Error for ContestRuleError {}

/// One UTC operating interval, half-open `[start, end)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContestPeriod {
    /// Interval start (UTC, inclusive).
    pub start: DateTime<Utc>,
    /// Interval end (UTC, exclusive).
    pub end: DateTime<Utc>,
}

/// A resolved, immutable contest instance: one entrant in one contest
/// edition, scored under one pinned rule set. Built once from validated
/// `[contest]` configuration at coordinator startup; the rules, entrant, and
/// periods of an instance never change mid-flight.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContestScoringContext {
    /// The entrant/edition identity. Contacts, dupes and multipliers never
    /// mix across instance ids.
    pub instance_id: String,
    /// The family exchange profile id (`us-state-qso-party`).
    pub profile_id: String,
    /// The pinned scoring-rule id.
    pub rules_id: String,
    /// The entrant's state, DC, Canadian province/territory, or `DX`.
    /// Never inferred from a callsign.
    pub operator_region: String,
    /// Ordered, non-overlapping UTC operating intervals.
    pub periods: Vec<ContestPeriod>,
}

impl ContestScoringContext {
    /// Validate and build a context. Rule-dependent validation (unknown
    /// rules id, region validity) happens here so the coordinator can refuse
    /// to run the station with the exact operator-facing message.
    pub fn new(
        instance_id: &str,
        profile_id: &str,
        rules_id: &str,
        operator_region: &str,
        periods: Vec<ContestPeriod>,
    ) -> Result<Self, ContestRuleError> {
        resolve_rules(rules_id)?;
        let region = operator_region.trim().to_ascii_uppercase();
        if !is_valid_region(&region) {
            return Err(ContestRuleError::InvalidOperatorRegion);
        }
        for window in &periods {
            if window.start >= window.end {
                return Err(ContestRuleError::InvalidPeriods);
            }
        }
        for pair in periods.windows(2) {
            if pair[0].end > pair[1].start {
                return Err(ContestRuleError::InvalidPeriods);
            }
        }
        Ok(Self {
            instance_id: instance_id.trim().to_string(),
            profile_id: profile_id.trim().to_string(),
            rules_id: rules_id.trim().to_string(),
            operator_region: region,
            periods,
        })
    }

    /// The pinned rules this instance scores under. Contexts are validated
    /// at construction, so this only fails for a hand-built context.
    pub fn rules(&self) -> Result<&'static ContestRules, ContestRuleError> {
        resolve_rules(&self.rules_id)
    }

    /// Whether `t` falls inside any operating interval (half-open, matched on
    /// contact start time only).
    pub fn in_period(&self, t: DateTime<Utc>) -> bool {
        self.periods.iter().any(|w| t >= w.start && t < w.end)
    }

    /// The instant the rate window ends at: `now` during the contest, the
    /// final interval end after it (final rate freezes at contest end).
    pub fn rate_time(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        match self.periods.last() {
            Some(last) if now > last.end => last.end,
            _ => now,
        }
    }
}

/// Scoring/evidence stamp carried on a QSO's `ContestInfo` (PAN-50): which
/// contest instance the contact belongs to. Evidence itself stays in raw
/// logged fields; nothing here is a cached score.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContestScoringStamp {
    /// The contest instance this contact was made under.
    pub instance_id: String,
    /// The pinned rule id the contact was made under.
    pub rules_id: String,
}

/// One completed ledger row, reduced to the typed facts scoring needs.
/// Identity never includes the audio offset or today's dial: the band is the
/// logged historical band.
#[derive(Debug, Clone, PartialEq)]
pub struct ContestContactFact {
    /// Stable contact id (survives replay and rebuilds); delivery
    /// duplication of the same id counts once.
    pub contact_id: String,
    /// Contest instance this row belongs to; empty for a noncontest record.
    pub instance_id: String,
    /// Partner callsign (case and surrounding whitespace normalize; portable
    /// suffixes are preserved).
    pub callsign: String,
    /// Normalized logged band (uppercase, e.g. `20M`); `None` when the
    /// logged RF frequency cannot produce a band.
    pub band: Option<String>,
    /// Logged mode string, normalized uppercase on classification.
    pub mode: String,
    /// Actual QSO start time (UTC) — the only time scope for eligibility,
    /// dupes, multipliers, and rate. Never database insertion time.
    pub start: DateTime<Utc>,
    /// Whether the attempt failed (failed attempts never earn credit and
    /// never become worked entries, as in PAN-260).
    pub failed: bool,
    /// Partner's explicit state/province/`DX` evidence, if logged.
    pub their_region: Option<String>,
    /// Partner's explicit county evidence, if logged.
    pub their_county: Option<String>,
    /// Our explicit county evidence at contact time, if logged (entrant
    /// county changes).
    pub our_county: Option<String>,
}

/// Why a contact earned nothing and never will under this instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusionReason {
    /// Record belongs to another contest instance or no contest.
    WrongInstance,
    /// Failed (or in-progress) attempt.
    Failed,
    /// Band known and not one the rules permit.
    DisallowedBand,
    /// Mode not one the rules permit.
    UnsupportedMode,
    /// Contact start falls outside every operating interval (including the
    /// future).
    OutOfPeriod,
    /// Partner location known and ineligible under these rules.
    IneligiblePartnerLocation,
}

/// The reducer's per-contact verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContactClassification {
    /// Eligible, nonduplicate, with sufficient rule evidence. `points` is
    /// the rule-defined QSO points; `multiplier_keys` are the (possibly
    /// several) multiplier keys this contact contributes.
    Scored {
        points: u64,
        multiplier_keys: Vec<MultiplierKey>,
    },
    /// Eligible contact with an earlier equal rule-defined contact key.
    Dupe,
    /// Completed contact whose eligibility, duplicate key, or multiplier
    /// cannot be resolved from the logged evidence.
    Pending,
    /// Known-ineligible under these rules.
    Excluded(ExclusionReason),
}

/// A rule-defined multiplier key, already scoped (per band where the rules
/// say so). Opaque string; the cardinality of the scored set is the mult
/// count.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MultiplierKey(pub String);

/// One contact's verdict paired with its stable id, for log-time display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedContact {
    /// The fact's stable contact id.
    pub contact_id: String,
    /// The verdict.
    pub classification: ContactClassification,
}

/// The aggregate snapshot. Every metric carries the same rule version and
/// the same ledger generation; totals are recomputed from raw evidence,
/// never trusted from imported points. These are local claimed scores before
/// sponsor adjudication, not promised final results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContestScoreSnapshot {
    /// Eligible nonduplicate contacts with sufficient rule evidence.
    pub scored: u64,
    /// Eligible contacts with an earlier equal rule-defined contact key.
    pub dupes: u64,
    /// Completed contacts whose eligibility, duplicate key, or multiplier
    /// cannot be resolved.
    pub pending: u64,
    /// Records with known-ineligible band, mode, instance, period, or
    /// partner location.
    pub excluded: u64,
    /// Cardinality of the rule-defined multiplier key set from scored
    /// contacts.
    pub multiplier_count: u64,
    /// Whether these rules use multipliers at all (Kansas displays n/a).
    pub multipliers_available: bool,
    /// Kansas: scored contact count. SC: QSO points × multiplier count +
    /// earned station bonuses (checked arithmetic; overflow makes the score
    /// unavailable rather than wrong).
    pub score: u64,
    /// At least one pending contact (displayed as "partial").
    pub partial: bool,
    /// Scored contacts whose start falls in the trailing 60 wall-clock
    /// minutes at snapshot time, per hour, with no startup extrapolation.
    /// Freezes at the final interval end after the contest.
    pub rate_per_hour: u64,
    /// Per-contact verdicts in reduction order, for log-time dupe display.
    pub per_contact: Vec<ClassifiedContact>,
}

/// Why no score could be produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoringError {
    /// The context names a rule id this build does not know (e.g. an
    /// imported future version). Scores stay unavailable, never guessed.
    UnsupportedRules,
    /// An aggregate would overflow u64.
    ScoreUnavailable,
}

impl fmt::Display for ScoringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScoringError::UnsupportedRules => write!(f, "contest: contest rules are unsupported"),
            ScoringError::ScoreUnavailable => write!(f, "contest: score unavailable"),
        }
    }
}

impl std::error::Error for ScoringError {}

fn normalize_callsign(call: &str) -> String {
    call.trim().to_ascii_uppercase()
}

fn normalize_region(region: &Option<String>) -> Option<String> {
    region
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_uppercase())
}

/// County evidence normalizes (trim + uppercase); structurally invalid
/// values (not 2–4 letters/digits) count as missing evidence, since sponsor
/// county-abbreviation lists vary by year and mode and a closed list would
/// reject valid evidence. Evidence must be explicit logged data either way —
/// never a grid-to-county guess.
fn normalize_county(county: &Option<String>) -> Option<String> {
    let normalized = county
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_uppercase());
    match normalized {
        Some(c) if (2..=4).contains(&c.len()) && c.chars().all(|ch| ch.is_ascii_alphanumeric()) => {
            Some(c)
        }
        _ => None,
    }
}

/// Partner region classification used by both rule families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartnerRegion {
    /// The contest's host state.
    Host,
    /// A valid non-host state/DC/province code.
    Domestic,
    /// The literal `DX`.
    Dx,
    /// Present but not a valid region code.
    Invalid,
    /// Absent.
    Unknown,
}

fn classify_partner_region(their_region: &Option<String>, host: &str) -> PartnerRegion {
    match normalize_region(their_region) {
        None => PartnerRegion::Unknown,
        Some(region) if region == host => PartnerRegion::Host,
        Some(region) if region == REGION_DX => PartnerRegion::Dx,
        Some(region) if is_valid_region(&region) => PartnerRegion::Domestic,
        Some(_) => PartnerRegion::Invalid,
    }
}

/// Kansas county evidence for the dupe key: the partner's Kansas county when
/// the partner is in Kansas, otherwise the entrant's own county when the
/// entrant is in Kansas (a documented entrant county change lets the same
/// partner be worked again). `Some(None)` = the county applies but is
/// missing; `None` = no county component at all.
fn kansas_county_evidence(
    their_region: &Option<String>,
    their_county: &Option<String>,
    our_county: &Option<String>,
    operator_region: &str,
) -> Option<Option<String>> {
    match normalize_region(their_region) {
        Some(region) if region == "KS" => Some(normalize_county(their_county)),
        Some(_) if operator_region == "KS" => Some(normalize_county(our_county)),
        Some(_) => None,
        None => {
            if operator_region == "KS" {
                Some(normalize_county(our_county))
            } else {
                // Partner location itself is unresolved for a non-Kansas
                // entrant; conservatively treat the county as applying but
                // missing so the group collapses to one key.
                Some(None)
            }
        }
    }
}

/// The rule-defined dupe-key county component. Empty string = no county
/// component; the surrounding key parts (call/band/mode) make it
/// unambiguous.
struct DupeCounty {
    /// Whether this rule family uses a county component here.
    applies: bool,
    value: Option<String>,
}

/// Build the dupe key from normalized parts. Kansas keeps FT4 and FT8
/// distinct; SC's digital grouping drops the mode entirely.
fn dupe_key(family: RulesKind, call: &str, band: &str, mode: &str, county: &DupeCounty) -> String {
    let county_part = if county.applies {
        format!("|{}", county.value.as_deref().unwrap_or("?"))
    } else {
        String::new()
    };
    match family {
        RulesKind::KansasFt => format!("{call}|{band}|{mode}{county_part}"),
        RulesKind::ScqpDigitalFixed => format!("{call}|{band}{county_part}"),
    }
}

/// Reduce typed ledger facts to the deterministic contest snapshot.
///
/// Determinism: facts are ordered by `(start, contact_id)` and deduplicated
/// by `contact_id`, so replay, out-of-order input, and cold reconstruction
/// of the same ledger produce byte-identical results. Deleting a contact or
/// correcting evidence simply changes the facts; nothing incremental is
/// persisted as authority.
pub fn reduce_contest_score(
    context: &ContestScoringContext,
    facts: &[ContestContactFact],
    now: DateTime<Utc>,
) -> Result<ContestScoreSnapshot, ScoringError> {
    let rules = context
        .rules()
        .map_err(|_| ScoringError::UnsupportedRules)?;
    reduce_with_rules(rules, context, facts, now)
}

/// Classify one not-yet-logged candidate against the existing ledger facts
/// under the same key builder the score uses (the contest-dupe lookup for
/// coordinator consumers). This marks; it never refuses a manual contact,
/// replaces PAN-260's autonomous admission gate, or introduces autonomous
/// contest operation.
pub fn contest_dupe_status(
    context: &ContestScoringContext,
    facts: &[ContestContactFact],
    candidate: &ContestContactFact,
) -> ContactClassification {
    let rules = match context.rules() {
        Ok(rules) => rules,
        Err(_) => return ContactClassification::Pending,
    };
    let mut all: Vec<ContestContactFact> = facts.to_vec();
    all.push(candidate.clone());
    let snapshot = match reduce_with_rules(rules, context, &all, Utc::now()) {
        Ok(snapshot) => snapshot,
        Err(_) => return ContactClassification::Pending,
    };
    snapshot
        .per_contact
        .into_iter()
        .find(|entry| entry.contact_id == candidate.contact_id)
        .map(|entry| entry.classification)
        .unwrap_or(ContactClassification::Pending)
}

fn reduce_with_rules(
    rules: &ContestRules,
    context: &ContestScoringContext,
    facts: &[ContestContactFact],
    now: DateTime<Utc>,
) -> Result<ContestScoreSnapshot, ScoringError> {
    // Deterministic order, then delivery dedup by stable contact id.
    let mut ordered: Vec<&ContestContactFact> = facts.iter().collect();
    ordered.sort_by(|a, b| (&a.start, &a.contact_id).cmp(&(&b.start, &b.contact_id)));
    ordered.dedup_by(|a, b| a.contact_id == b.contact_id);

    let operator_in_host = match rules.kind {
        RulesKind::KansasFt => context.operator_region == "KS",
        RulesKind::ScqpDigitalFixed => context.operator_region == "SC",
    };
    let host = match rules.kind {
        RulesKind::KansasFt => "KS",
        RulesKind::ScqpDigitalFixed => "SC",
    };

    // Stage 1: mechanical eligibility (instance, failure, mode, band,
    // period). Only a band the rules provably disallow excludes; a missing
    // band is unresolvable eligibility evidence, i.e. pending.
    enum Stage1<'a> {
        Excluded(&'a ContestContactFact, ExclusionReason),
        PendingBand(&'a ContestContactFact),
        Eligible(&'a ContestContactFact),
    }
    let staged: Vec<Stage1> = ordered
        .into_iter()
        .map(|fact| {
            if fact.failed {
                return Stage1::Excluded(fact, ExclusionReason::Failed);
            }
            if fact.instance_id != context.instance_id {
                return Stage1::Excluded(fact, ExclusionReason::WrongInstance);
            }
            // Exclude future evidence before band-pending and county collapse.
            if fact.start > now || !context.in_period(fact.start) {
                return Stage1::Excluded(fact, ExclusionReason::OutOfPeriod);
            }
            let mode = fact.mode.trim().to_ascii_uppercase();
            if !rules.allowed_modes.contains(&mode.as_str()) {
                return Stage1::Excluded(fact, ExclusionReason::UnsupportedMode);
            }
            let band = match fact
                .band
                .as_deref()
                .map(str::trim)
                .filter(|b| !b.is_empty())
            {
                None => return Stage1::PendingBand(fact),
                Some(band) => band.to_ascii_uppercase(),
            };
            if !rules.allowed_bands.contains(&band.as_str()) {
                return Stage1::Excluded(fact, ExclusionReason::DisallowedBand);
            }
            Stage1::Eligible(fact)
        })
        .collect();

    // Kansas conservative county collapse: within each (callsign, band,
    // mode) group, if any contact that carries a county component lacks the
    // county evidence, the whole group collapses to one county-less key so
    // an unknown plus a known key can never earn two contacts.
    let mut collapsed_groups: BTreeSet<(String, String, String)> = BTreeSet::new();
    if rules.kind == RulesKind::KansasFt {
        let mut group_needs: BTreeMap<(String, String, String), bool> = BTreeMap::new();
        for entry in &staged {
            let fact = match entry {
                Stage1::Eligible(fact) | Stage1::PendingBand(fact) => *fact,
                Stage1::Excluded(..) => continue,
            };
            let mode = fact.mode.trim().to_ascii_uppercase();
            let band = fact
                .band
                .as_deref()
                .map(|b| b.trim().to_ascii_uppercase())
                .unwrap_or_default();
            let group = (normalize_callsign(&fact.callsign), band, mode);
            let needs = group_needs.entry(group).or_insert(false);
            if kansas_county_evidence(
                &fact.their_region,
                &fact.their_county,
                &fact.our_county,
                &context.operator_region,
            ) == Some(None)
            {
                *needs = true;
            }
        }
        collapsed_groups = group_needs
            .into_iter()
            .filter(|(_, needs)| *needs)
            .map(|(group, _)| group)
            .collect();
    }

    // Stage 2: partner-location eligibility + evidence, then dupe-key
    // claiming in reduction order.
    let mut claimed_keys: BTreeSet<String> = BTreeSet::new();
    let mut per_contact: Vec<ClassifiedContact> = Vec::new();
    let mut multiplier_keys: BTreeSet<MultiplierKey> = BTreeSet::new();
    let mut bonus_credit: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut scored: u64 = 0;
    let mut dupes: u64 = 0;
    let mut pending: u64 = 0;
    let mut excluded: u64 = 0;
    let mut points_total: u64 = 0;
    let mut rate_window_count: u64 = 0;
    let rate_time = context.rate_time(now);
    let rate_start = rate_time - chrono::Duration::minutes(60);

    for entry in staged {
        let fact = match entry {
            Stage1::Eligible(fact) => fact,
            Stage1::PendingBand(fact) => {
                pending += 1;
                per_contact.push(ClassifiedContact {
                    contact_id: fact.contact_id.clone(),
                    classification: ContactClassification::Pending,
                });
                continue;
            }
            Stage1::Excluded(fact, reason) => {
                excluded += 1;
                per_contact.push(ClassifiedContact {
                    contact_id: fact.contact_id.clone(),
                    classification: ContactClassification::Excluded(reason),
                });
                continue;
            }
        };
        let call = normalize_callsign(&fact.callsign);
        let mode = fact.mode.trim().to_ascii_uppercase();
        let band = fact
            .band
            .as_deref()
            .map(|b| b.trim().to_ascii_uppercase())
            .unwrap_or_default();

        // Partner-location eligibility. Unresolved stays pending — never
        // guessed as DX or host.
        let partner = classify_partner_region(&fact.their_region, host);
        let eligible = match (rules.kind, operator_in_host, partner) {
            // A Kansas entrant works everyone; partner location is not
            // eligibility evidence there (it only feeds the dupe key).
            (RulesKind::KansasFt, true, _) => true,
            // Outside entrants score host-state partners only.
            (_, false, PartnerRegion::Host) => true,
            // Unresolved or invalid partner location: pending — never
            // guessed as DX or host. (For a Kansas entrant this was already
            // handled above; this arm keeps SC entrants pending.)
            (_, _, PartnerRegion::Unknown) | (_, _, PartnerRegion::Invalid) => {
                pending += 1;
                per_contact.push(ClassifiedContact {
                    contact_id: fact.contact_id.clone(),
                    classification: ContactClassification::Pending,
                });
                continue;
            }
            // An SC entrant works everyone with a resolved location.
            (RulesKind::ScqpDigitalFixed, true, _) => true,
            // Known-ineligible partner location (non-host US/VE, DX) under
            // rules that only score host partners.
            (_, false, PartnerRegion::Domestic) | (_, false, PartnerRegion::Dx) => false,
        };
        if !eligible {
            excluded += 1;
            per_contact.push(ClassifiedContact {
                contact_id: fact.contact_id.clone(),
                classification: ContactClassification::Excluded(
                    ExclusionReason::IneligiblePartnerLocation,
                ),
            });
            continue;
        }

        // County component of the dupe key.
        let county: DupeCounty = match rules.kind {
            RulesKind::KansasFt => {
                let group = (call.clone(), band.clone(), mode.clone());
                if collapsed_groups.contains(&group) {
                    DupeCounty {
                        applies: true,
                        value: None,
                    }
                } else {
                    match kansas_county_evidence(
                        &fact.their_region,
                        &fact.their_county,
                        &fact.our_county,
                        &context.operator_region,
                    ) {
                        Some(value) => DupeCounty {
                            applies: true,
                            value,
                        },
                        None => DupeCounty {
                            applies: false,
                            value: None,
                        },
                    }
                }
            }
            RulesKind::ScqpDigitalFixed => {
                if partner == PartnerRegion::Host {
                    match normalize_county(&fact.their_county) {
                        Some(county) => DupeCounty {
                            applies: true,
                            value: Some(county),
                        },
                        // A host-state partner without explicit county
                        // evidence is pending: the multiplier cannot be
                        // resolved and a grid never manufactures one.
                        None => {
                            pending += 1;
                            per_contact.push(ClassifiedContact {
                                contact_id: fact.contact_id.clone(),
                                classification: ContactClassification::Pending,
                            });
                            continue;
                        }
                    }
                } else {
                    DupeCounty {
                        applies: false,
                        value: None,
                    }
                }
            }
        };

        let key = dupe_key(rules.kind, &call, &band, &mode, &county);
        if !claimed_keys.insert(key) {
            dupes += 1;
            per_contact.push(ClassifiedContact {
                contact_id: fact.contact_id.clone(),
                classification: ContactClassification::Dupe,
            });
            continue;
        }

        // Scored: points and multiplier keys.
        let (points, mults) = match rules.kind {
            RulesKind::KansasFt => (1, Vec::new()),
            RulesKind::ScqpDigitalFixed => {
                let region = normalize_region(&fact.their_region).unwrap_or_default();
                if region == "SC" {
                    let county = county.value.clone().unwrap_or_default();
                    let mut keys = vec![MultiplierKey(format!("{band}:county:{county}"))];
                    if operator_in_host {
                        keys.push(MultiplierKey(format!("{band}:state:SC")));
                    }
                    (2, keys)
                } else if region == REGION_DX {
                    // DX earns points but no multiplier.
                    (4, Vec::new())
                } else if operator_in_host {
                    // SC entrant: counties, states including DC, and
                    // Canadian provinces/territories all multiply, per band.
                    (4, vec![MultiplierKey(format!("{band}:state:{region}"))])
                } else {
                    (2, Vec::new())
                }
            }
        };
        points_total = points_total
            .checked_add(points)
            .ok_or(ScoringError::ScoreUnavailable)?;
        for mult in &mults {
            multiplier_keys.insert(mult.clone());
        }
        if let Some(bonus) = rules.bonus_stations.iter().find(|b| b.callsign == call) {
            bonus_credit
                .entry((call.clone(), band.clone()))
                .or_insert(bonus.weight);
        }
        if fact.start > rate_start && fact.start <= rate_time {
            rate_window_count += 1;
        }
        scored += 1;
        per_contact.push(ClassifiedContact {
            contact_id: fact.contact_id.clone(),
            classification: ContactClassification::Scored {
                points,
                multiplier_keys: mults,
            },
        });
    }

    let bonus_total: u64 = bonus_credit.values().sum();
    let multiplier_count = if rules.multipliers_enabled() {
        multiplier_keys.len() as u64
    } else {
        0
    };
    let score = match rules.kind {
        RulesKind::KansasFt => scored,
        RulesKind::ScqpDigitalFixed => points_total
            .checked_mul(multiplier_count)
            .and_then(|value| value.checked_add(bonus_total))
            .ok_or(ScoringError::ScoreUnavailable)?,
    };

    Ok(ContestScoreSnapshot {
        scored,
        dupes,
        pending,
        excluded,
        multiplier_count,
        multipliers_available: rules.multipliers_enabled(),
        score,
        partial: pending > 0,
        rate_per_hour: rate_window_count,
        per_contact,
    })
}

impl ContestRules {
    /// Kansas's no-multiplier formula is an explicit separate rule: an empty
    /// multiplier set displayed as n/a, never as one.
    fn multipliers_enabled(&self) -> bool {
        self.kind == RulesKind::ScqpDigitalFixed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_rules_ids_resolve_and_unknown_is_rejected() {
        assert_eq!(
            resolve_rules(KSQP_FT_2026).unwrap().display_name,
            "KSQP 2026"
        );
        assert_eq!(
            resolve_rules(SCQP_DIGITAL_FIXED_2026).unwrap().display_name,
            "SCQP 2026"
        );
        assert_eq!(
            resolve_rules("naqp-2027").unwrap_err(),
            ContestRuleError::UnknownRulesId
        );
        assert_eq!(
            ContestRuleError::UnknownRulesId.to_string(),
            "contest: unknown rules_id"
        );
    }

    #[test]
    fn operator_region_validation_covers_dc_canada_and_dx() {
        assert!(is_valid_region("DC"));
        for code in CANADIAN_PROVINCE_CODES {
            assert!(is_valid_region(code), "{code} must be valid");
        }
        assert!(is_valid_region("dx"));
        assert!(!is_valid_region("ZZ"));
        assert!(!is_valid_region(""));
        assert_eq!(
            ContestRuleError::InvalidOperatorRegion.to_string(),
            "contest: operator_region is not valid for the selected rules"
        );
    }

    #[test]
    fn context_rejects_bad_periods_and_region() {
        let start = DateTime::parse_from_rfc3339("2026-08-29T14:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let end = DateTime::parse_from_rfc3339("2026-08-30T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(ContestScoringContext::new(
            "KSQP-2026-K5ARH",
            "us-state-qso-party",
            KSQP_FT_2026,
            "TX",
            vec![ContestPeriod { start, end }]
        )
        .is_ok());
        assert_eq!(
            ContestScoringContext::new(
                "KSQP-2026-K5ARH",
                "us-state-qso-party",
                KSQP_FT_2026,
                "ZZ",
                vec![ContestPeriod { start, end }]
            )
            .unwrap_err(),
            ContestRuleError::InvalidOperatorRegion
        );
        assert_eq!(
            ContestScoringContext::new(
                "KSQP-2026-K5ARH",
                "us-state-qso-party",
                KSQP_FT_2026,
                "TX",
                vec![ContestPeriod {
                    start: end,
                    end: start
                }]
            )
            .unwrap_err(),
            ContestRuleError::InvalidPeriods
        );
    }

    /// The bonus engine: credited once per band/digital group even after a
    /// county change, with synthetic weights independent of the catalog.
    /// Production sponsor weights are covered by integration fixtures.
    #[test]
    fn bonus_stations_credit_once_per_band_with_all_three_weights() {
        let synthetic = ContestRules {
            id: SCQP_DIGITAL_FIXED_2026,
            display_name: "SCQP 2026",
            profile_id: "us-state-qso-party",
            kind: RulesKind::ScqpDigitalFixed,
            allowed_bands: SCQP_DIGITAL_FIXED_2026_RULES.allowed_bands,
            allowed_modes: SCQP_DIGITAL_FIXED_2026_RULES.allowed_modes,
            bonus_stations: &[
                ContestBonusStation {
                    callsign: "N0BNS",
                    weight: 25,
                },
                ContestBonusStation {
                    callsign: "W1BNS",
                    weight: 50,
                },
                ContestBonusStation {
                    callsign: "K2BNS",
                    weight: 100,
                },
            ],
            source_notes: "test-only weights",
        };
        let start = DateTime::parse_from_rfc3339("2026-03-01T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let end = DateTime::parse_from_rfc3339("2026-03-01T21:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let context = ContestScoringContext {
            instance_id: "SCQP-2026-K5ARH".to_string(),
            profile_id: "us-state-qso-party".to_string(),
            rules_id: synthetic.id.to_string(),
            operator_region: "SC".to_string(),
            periods: vec![ContestPeriod { start, end }],
        };
        let t = |mins: i64, id: &str, call: &str, band: &str, county: &str| ContestContactFact {
            contact_id: id.to_string(),
            instance_id: "SCQP-2026-K5ARH".to_string(),
            callsign: call.to_string(),
            band: Some(band.to_string()),
            mode: "FT8".to_string(),
            start: start + chrono::Duration::minutes(mins),
            failed: false,
            their_region: Some("SC".to_string()),
            their_county: Some(county.to_string()),
            our_county: None,
        };
        // N0BNS twice on 20M in different counties (a documented county
        // change makes them distinct contacts), yet the bonus still credits
        // once for that band; the same station on 40M credits again.
        let facts = vec![
            t(5, "a", "N0BNS", "20M", "GV"),
            t(7, "b", "N0BNS", "20M", "SP"),
            t(9, "c", "N0BNS", "40M", "GV"),
            t(11, "d", "W1BNS", "20M", "RI"),
            t(13, "e", "K2BNS", "80M", "CH"),
        ];
        let snapshot = reduce_with_rules(&synthetic, &context, &facts, end).unwrap();
        // points: five SC partners × 2 = 10; multipliers: 20M:county:GV,
        // 20M:county:SP, 40M:county:GV, 20M:county:RI, 80M:county:CH,
        // plus the SC state key on each of the three bands = 8;
        // bonuses 25 (20M once) + 25 (40M) + 50 + 100 = 200.
        assert_eq!(snapshot.scored, 5);
        assert_eq!(snapshot.multiplier_count, 8);
        assert_eq!(snapshot.score, 10 * 8 + 25 + 25 + 50 + 100);
    }
}
