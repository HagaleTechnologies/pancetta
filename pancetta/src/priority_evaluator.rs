//! Coordinator-level priority evaluator wiring.
//!
//! Bridges `pancetta_qso::PriorityScorer` with the QSO database for
//! duplicate checking and DXCC need lookups.

use chrono::{DateTime, Utc};
use pancetta_qso::priority::{NeedSource, WorkedStationLookup};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

mod local_log;
use local_log::LocalLog;

/// PAN-58: resolve a possibly hash-rendered callsign (`"<N7RLK>"`, FT8's
/// i3=4 nonstandard-callsign hash-render — see
/// `pancetta_core::callsign::resolve_hash_render`) to the plain, uppercase
/// identity it represents, before any DXCC/needed/atno/duplicate/notable
/// lookup below. Without this, a leading `'<'` defeats every prefix/exact
/// match those lookups do, so a hash-rendered decode of an already-worked
/// or home-country station silently fails every check and reads as
/// "unknown, never excluded, never worked" — ranking it artificially high.
/// Returns `None` for the unresolved hash-miss placeholder `"<...>"`, which
/// carries no station identity at all and must never be treated as
/// needed/worked/notable.
fn resolved_identity(callsign: &str) -> Option<String> {
    let upper = callsign.trim().to_uppercase();
    pancetta_core::callsign::resolve_hash_render(&upper).map(|s| s.to_string())
}

/// PAN-93: one needed verdict. `Unknown` when its cqdx list has not loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeedStatus {
    Needed,
    NotNeeded,
    Unknown,
}

/// PAN-93: whether a cqdx.io needed list has answered this session, kept
/// apart from the list so "nothing is needed" and "no answer" differ.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NeedListAvailability {
    /// `[network.cqdx]` is off. The default for a fresh lookup.
    #[default]
    NotConfigured,
    /// The client is built; the first fetch has not finished.
    Loading,
    /// cqdx.io does not offer this list yet (needed-grids answers 404).
    NotOffered,
    /// cqdx.io answered. `band` is the per-band scope, `None` for all bands.
    Loaded {
        fetched_at: DateTime<Utc>,
        band: Option<String>,
    },
    /// No answer this session. `retrying` is false when only a config fix
    /// and restart can help.
    Unavailable {
        since: DateTime<Utc>,
        error: String,
        retrying: bool,
    },
}

/// PAN-93: a cqdx.io needed list and its availability under one lock, so a
/// reader never sees a new list with an old availability.
#[derive(Debug, Default)]
struct NeedList {
    entries: HashSet<String>,
    availability: NeedListAvailability,
}

impl NeedList {
    fn is_loaded(&self) -> bool {
        matches!(self.availability, NeedListAvailability::Loaded { .. })
    }

    /// No-op once loaded: a failed refresh keeps the last answer (D7).
    /// Keeps `since` from an earlier failure so the outage start survives.
    fn mark_failed(&mut self, error: &str, retrying: bool) {
        let since = match &self.availability {
            NeedListAvailability::Loaded { .. } => return,
            NeedListAvailability::Unavailable { since, .. } => *since,
            _ => Utc::now(),
        };
        self.availability = NeedListAvailability::Unavailable {
            since,
            error: error.to_string(),
            retrying,
        };
    }
}

/// Cached station lookup that holds a snapshot of worked stations.
///
/// Refreshed periodically by the coordinator. The `PriorityScorer` calls
/// this synchronously via the `WorkedStationLookup` trait.
#[derive(Debug, Clone)]
pub struct CachedStationLookup {
    /// Callsigns worked per band.  Key = uppercase band name (e.g. "20M"),
    /// value = latest contact start date per resolved uppercase callsign.
    worked_on_band: Arc<RwLock<HashMap<String, HashMap<String, DateTime<Utc>>>>>,
    /// DXCC entities worked per band, derived locally from `worked_on_band`
    /// via the offline prefix→entity resolver (`pancetta_core::dxcc`) — no
    /// cqdx dependency. Key = uppercase band name, value = set of resolved
    /// entity names (e.g. "Japan"). A callsign whose prefix doesn't resolve
    /// contributes nothing (safe: never claims an unresolvable entity is
    /// "needed"). Powers `is_dxcc_needed_on_band` (2026-07-18, DX Hunter
    /// per-band-needed gap — see docs/DECISIONS/2026-07-development-phases-and-gaps.md).
    worked_dxcc_on_band: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    /// Grid squares worked per band, local history only (mirrors
    /// `worked_dxcc_on_band` one tier down — #164 tier 4). Key = uppercase
    /// band name, value = set of 4-char uppercase Maidenhead fields.
    worked_grids_on_band: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    /// Callsigns where a recent QSO attempt failed.
    recent_failures: Arc<RwLock<HashSet<String>>>,
    /// DXCC entity prefixes cqdx.io lists as still needed, with whether
    /// cqdx has answered at all (PAN-93). An empty `Loaded` list means
    /// "nothing needed"; anything but `Loaded` means "unknown".
    needed_dxcc: Arc<RwLock<NeedList>>,
    /// DXCC entity prefixes that are ATNO (all-time new ones — never worked
    /// on any band). A strict subset of `needed_dxcc`'s entries; written by
    /// the same cqdx answer from its `atno` flag. Empty/inert until a cqdx
    /// list loads, so `!` is never shown for an unknown status.
    needed_atno: Arc<RwLock<HashSet<String>>>,
    /// 4-char grid squares cqdx.io lists as still needed for award
    /// tracking, with the list's availability (PAN-93).
    needed_grids: Arc<RwLock<NeedList>>,
    /// Rarity scores from cqdx.io, keyed by uppercase callsign.
    rarity_scores: Arc<RwLock<HashMap<String, f64>>>,
    /// Rarity scores from cqdx.io, re-indexed by resolved DXCC entity name
    /// (derived from `rarity_scores` — see `update_rarity_scores`). BUG
    /// #163: cqdx's live-spots feed only reports rarity for whatever exact
    /// callsigns it happened to see recently, so most locally-decoded calls
    /// never get an exact match and silently fall through to `rarity()`'s
    /// neutral 0.5 default — collapsing the DX Hunter's priority score into
    /// two flat buckets regardless of true rarity. This entity-keyed cache
    /// lets a callsign that was never itself spotted still inherit real
    /// rarity data reported for ANY other callsign from the same entity.
    rarity_by_entity: Arc<RwLock<HashMap<String, f64>>>,
    /// Notable callsigns from cqdx.io spot groups.
    notable_callsigns: Arc<RwLock<HashSet<String>>>,
    /// Network SNR data: callsign -> (reporter_count, best_snr).
    network_snr: Arc<RwLock<HashMap<String, (u32, i32)>>>,
    /// Network last-seen timestamps: callsign -> unix timestamp.
    network_last_seen: Arc<RwLock<HashMap<String, i64>>>,
    /// PAN-347: per-entity worked, confirmed and per-mode sets from the
    /// local logbook, with whether its first full read has succeeded. While
    /// no cqdx list has loaded, it answers needed and ATNO.
    local_log: Arc<RwLock<LocalLog>>,
    /// PAN-347: the operator's own DXCC entity, from `[station] callsign`.
    /// Never needed, ATNO, new on band or new in mode. Replaces the
    /// letter-prefix exclusions that treated KH6/KL7/KP4 as home.
    home_entity: Arc<RwLock<Option<&'static str>>>,
    /// Contacted stations' US state/territory, keyed by UPPERCASE callsign.
    /// Seeded at startup from the QSO log's ADIF `STATE` and updated on QSO
    /// completion / QRZ enrichment. Display-only (PAN-85) — deliberately NOT
    /// a `WorkedStationLookup` method, because state has no bearing on
    /// priority scoring. Values are pre-validated to a canonical 2-letter
    /// code; an unvalidatable value is dropped, never stored.
    station_states: Arc<RwLock<HashMap<String, String>>>,
    /// Local contacts across all bands and modes; independent of state knowledge.
    worked_states: Arc<RwLock<HashSet<String>>>,
    grid_history_available: Arc<RwLock<bool>>,
    state_history_available: Arc<RwLock<bool>>,
}

impl Default for CachedStationLookup {
    fn default() -> Self {
        Self::new()
    }
}

impl CachedStationLookup {
    pub fn new() -> Self {
        Self {
            worked_on_band: Arc::new(RwLock::new(HashMap::new())),
            worked_dxcc_on_band: Arc::new(RwLock::new(HashMap::new())),
            worked_grids_on_band: Arc::new(RwLock::new(HashMap::new())),
            recent_failures: Arc::new(RwLock::new(HashSet::new())),
            needed_dxcc: Arc::new(RwLock::new(NeedList::default())),
            needed_atno: Arc::new(RwLock::new(HashSet::new())),
            needed_grids: Arc::new(RwLock::new(NeedList::default())),
            rarity_scores: Arc::new(RwLock::new(HashMap::new())),
            rarity_by_entity: Arc::new(RwLock::new(HashMap::new())),
            notable_callsigns: Arc::new(RwLock::new(HashSet::new())),
            network_snr: Arc::new(RwLock::new(HashMap::new())),
            network_last_seen: Arc::new(RwLock::new(HashMap::new())),
            local_log: Arc::new(RwLock::new(LocalLog::default())),
            home_entity: Arc::new(RwLock::new(None)),
            station_states: Arc::new(RwLock::new(HashMap::new())),
            worked_states: Arc::new(RwLock::new(HashSet::new())),
            grid_history_available: Arc::new(RwLock::new(false)),
            state_history_available: Arc::new(RwLock::new(false)),
        }
    }

    /// PAN-347: the operator's own DXCC entity, never needed, ATNO, new on
    /// band or new in mode.
    pub fn set_home_entity(&self, entity: Option<&'static str>) {
        *self.home_entity.write() = entity;
    }

    /// PAN-347: true once the local logbook's first full read succeeded
    /// this session. Never reset: seeds are additive and live completions
    /// keep it current.
    pub fn local_log_available(&self) -> bool {
        self.local_log.read().available
    }

    fn is_home_entity(&self, entity: Option<&str>) -> bool {
        matches!((entity, *self.home_entity.read()), (Some(e), Some(h)) if e == h)
    }

    /// Seed `worked_on_band` for `band` from a list of callsigns loaded out-of-band
    /// (e.g. from the QSO database at startup).  Both the band key and callsigns
    /// are uppercased for consistent comparison.
    pub fn seed_worked_from_list(&self, band: &str, callsigns: Vec<String>) {
        self.seed_dated_worked(
            callsigns
                .into_iter()
                .map(|c| (band.to_string(), c, Utc::now()))
                .collect(),
        );
    }

    /// Merge dated history for all bands. Dates never expire the worked-before display.
    pub fn seed_dated_worked(&self, rows: Vec<(String, String, DateTime<Utc>)>) {
        for (band, call, date) in rows {
            let _ = self.record_worked_at(&call, &band, date);
        }
    }

    /// A failed read leaves the previous successfully seeded cache intact.
    pub async fn seed_worked_history(
        &self,
        db: &pancetta_qso::async_database::QsoDatabase,
    ) -> Result<(), pancetta_qso::async_database::AsyncDatabaseError> {
        let rows = db.worked_history().await?;
        for row in rows {
            self.record_logged_contact(
                &row.callsign,
                &row.band,
                &row.mode,
                row.confirmed,
                row.start_time,
            );
        }
        // PAN-347: publish only after a full successful read.
        self.local_log.write().available = true;
        Ok(())
    }

    /// Seed `worked_dxcc_on_band` from every (band, callsign) pair ever
    /// worked, loaded out-of-band (the QSO database at startup — see
    /// `QsoDatabase::get_worked_bands_and_callsigns`, which pulls ALL bands
    /// in one query since, unlike `seed_worked_from_list`, the DX Hunter
    /// needs this for bands other than wherever the rig happens to be
    /// tuned at startup). Each callsign is resolved to a DXCC entity name
    /// via the offline prefix resolver; unresolvable callsigns are skipped.
    pub fn seed_worked_dxcc_from_list(&self, pairs: Vec<(String, String)>) {
        let mut map = self.worked_dxcc_on_band.write();
        let mut resolved = 0usize;
        let mut entities = Vec::new();
        let mut skipped = 0usize;
        for (band, callsign) in pairs {
            match pancetta_core::dxcc::entity_for_callsign(&callsign) {
                Some(entity) => {
                    map.entry(band.trim().to_uppercase())
                        .or_default()
                        .insert(entity.to_string());
                    entities.push(entity);
                    resolved += 1;
                }
                None => skipped += 1,
            }
        }
        // PAN-347: ATNO and the band maps never disagree.
        let band_count = map.len();
        drop(map);
        {
            let mut log = self.local_log.write();
            for entity in entities {
                log.mark_worked(entity);
            }
        }
        tracing::info!(
            "CachedStationLookup: seeded per-band worked-DXCC from {} resolved QSO(s) \
             across {} band(s) ({} callsign(s) had no resolvable entity)",
            resolved,
            band_count,
            skipped
        );
    }

    /// Seed `worked_grids_on_band` from (band, grid) pairs loaded at startup
    /// (`QsoDatabase::get_worked_bands_and_grids`). Grids shorter than 4
    /// chars are skipped (not a valid Maidenhead field).
    pub fn seed_worked_grids_from_list(&self, pairs: Vec<(String, String)>) {
        let mut map = self.worked_grids_on_band.write();
        let mut inserted = 0usize;
        for (band, grid) in pairs {
            let trimmed = grid.trim();
            if trimmed.len() < 4 {
                continue;
            }
            let field = trimmed[..4].to_uppercase();
            map.entry(band.trim().to_uppercase())
                .or_default()
                .insert(field);
            inserted += 1;
        }
        tracing::info!(
            "CachedStationLookup: seeded {} worked grid(s) across {} band(s)",
            inserted,
            map.len()
        );
    }

    /// Record a station's US state/territory, validating first. An
    /// unvalidatable value (wrong length, not a real subdivision, free text)
    /// is silently dropped rather than stored — PAN-85 forbids ever showing
    /// a placeholder or a guessed state.
    ///
    /// The map is keyed by `resolved_identity`, exactly like every sibling
    /// accessor on this struct: a hash-rendered decode (`"<W5ABC>"`) must
    /// store under the same key its plain form does, and the unresolved
    /// placeholder `"<...>"` — which identifies no station at all — must
    /// never be stored.
    pub fn record_state(&self, callsign: &str, state: &str) {
        let Some(key) = resolved_identity(callsign) else {
            return;
        };
        if let Some(code) = pancetta_core::dxcc::normalize_us_state(state) {
            self.station_states.write().insert(key, code.to_string());
        }
    }

    /// Seed from `(callsign, state)` pairs loaded at startup, oldest first —
    /// later rows overwrite earlier ones so the most recent log entry wins.
    pub fn seed_states_from_list(&self, pairs: Vec<(String, String)>) {
        let mut latest = HashMap::new();
        for (call, state) in pairs {
            self.record_worked_state(&call, &state);
            if let (Some(key), Some(code)) = (
                resolved_identity(&call),
                pancetta_core::dxcc::normalize_us_state(&state),
            ) {
                latest.insert(key, code.to_string());
            }
        }
        // History is chronological, but must not overwrite a live observation
        // received while the startup query was in flight.
        let mut states = self.station_states.write();
        for (call, state) in latest {
            states.entry(call).or_insert(state);
        }
    }

    /// Record a completed contact's validated US subdivision, not an observation.
    pub fn record_worked_state(&self, callsign: &str, state: &str) {
        if !pancetta_core::dxcc::entity_for_callsign(callsign)
            .is_some_and(pancetta_core::dxcc::is_us_related_entity)
        {
            return;
        }
        if let Some(code) = pancetta_core::dxcc::normalize_us_state(state) {
            self.worked_states.write().insert(code.to_string());
        }
    }

    /// Local all-time worked status. Display callers must also check availability.
    pub fn is_state_needed(&self, state: &str) -> bool {
        pancetta_core::dxcc::normalize_us_state(state)
            .is_some_and(|code| !self.worked_states.read().contains(code))
    }

    /// Suppress display claims before opening history, including supervisor restarts.
    pub fn reset_display_history(&self) {
        *self.grid_history_available.write() = false;
        *self.state_history_available.write() = false;
    }

    /// Shared by live and replay startup. Seed additively, then publish availability.
    pub async fn seed_display_history(&self, db: &pancetta_qso::async_database::QsoDatabase) {
        self.reset_display_history();
        match db.try_get_worked_bands_and_grids().await {
            Ok(pairs) => {
                self.seed_worked_grids_from_list(pairs);
                *self.grid_history_available.write() = true;
            }
            Err(error) => tracing::warn!(%error, "DX Hunter grid history unavailable"),
        }
        match db.try_get_worked_callsigns_and_states().await {
            Ok(pairs) => {
                self.seed_states_from_list(pairs);
                *self.state_history_available.write() = true;
            }
            Err(error) => tracing::warn!(%error, "DX Hunter state history unavailable"),
        }
    }

    /// The completion event's local worked-history update, also used by headless tests.
    pub fn record_completed_contact(&self, metadata: &pancetta_qso::QsoMetadata) {
        let Some(call) = metadata.their_callsign.as_deref() else {
            return;
        };
        let band = pancetta_qso::utils::frequency_to_band(metadata.frequency);
        // PAN-347: a live contact is worked, in its mode, and not yet confirmed.
        self.record_logged_contact(call, &band, &metadata.mode, false, metadata.start_time);
        if let Some(grid) = metadata.grids.theirs.as_deref() {
            self.record_worked_grid(grid, &band);
        }
        self.record_completed_state(metadata);
    }

    /// Also called after QRZ enriches an already-completed contact (session only).
    pub fn record_completed_state(&self, metadata: &pancetta_qso::QsoMetadata) {
        if let (Some(call), Some(state)) = (
            metadata.their_callsign.as_deref(),
            metadata.their_state.as_deref(),
        ) {
            self.record_state(call, state);
            self.record_worked_state(call, state);
        }
    }

    /// The station's known US state, if any. `None` when unknown — the
    /// caller then renders the entity alone (PAN-85). Resolves hash-rendered
    /// callsigns first, so `"<W5ABC>"` finds what `"W5ABC"` stored.
    pub fn state_for(&self, callsign: &str) -> Option<String> {
        let key = resolved_identity(callsign)?;
        self.station_states.read().get(&key).cloned()
    }

    pub fn update_recent_failures(&self, callsigns: HashSet<String>) {
        *self.recent_failures.write() = callsigns;
    }

    /// Install an all-bands cqdx needed-entity answer fetched now.
    pub fn update_needed_dxcc(&self, patterns: HashSet<String>) {
        self.update_needed_dxcc_for_band(patterns, None, Utc::now());
    }

    /// PAN-93: install a cqdx needed-entity answer and mark the list
    /// `Loaded` under one lock. `band` is the per-band scope cqdx answered
    /// for, `None` for all bands. An empty `patterns` means "nothing needed".
    pub fn update_needed_dxcc_for_band(
        &self,
        patterns: HashSet<String>,
        band: Option<String>,
        fetched_at: DateTime<Utc>,
    ) {
        let mut list = self.needed_dxcc.write();
        list.entries = patterns;
        list.availability = NeedListAvailability::Loaded { fetched_at, band };
    }

    /// Install the set of ATNO ("all-time new one") DXCC prefixes. Should
    /// be a subset of the `needed_dxcc` set and written by the same cqdx
    /// answer. Uppercase enforced. Replaces the prior set on each call.
    pub fn update_needed_atno(&self, prefixes: HashSet<String>) {
        let upper: HashSet<String> = prefixes.into_iter().map(|p| p.to_uppercase()).collect();
        *self.needed_atno.write() = upper;
    }

    /// Install a cqdx needed-grid answer (4-char uppercase squares) and
    /// mark the list `Loaded`. An empty set means "no grids needed".
    pub fn update_needed_grids(&self, grids: HashSet<String>) {
        let mut list = self.needed_grids.write();
        list.entries = grids;
        list.availability = NeedListAvailability::Loaded {
            fetched_at: Utc::now(),
            band: None,
        };
    }

    /// PAN-93: the cqdx client is built and the first fetch is under way.
    /// Moves each list to `Loading` only from `NotConfigured`.
    pub fn set_need_lists_loading(&self) {
        for list in [&self.needed_dxcc, &self.needed_grids] {
            let mut list = list.write();
            if list.availability == NeedListAvailability::NotConfigured {
                list.availability = NeedListAvailability::Loading;
            }
        }
    }

    /// PAN-93: the needed-entity list could not be fetched. A no-op once a
    /// list has loaded (the last answer is kept; its age shows staleness).
    pub fn mark_dxcc_need_list_failed(&self, error: &str, retrying: bool) {
        self.needed_dxcc.write().mark_failed(error, retrying);
    }

    /// PAN-93: the needed-grid list could not be fetched. A no-op once loaded.
    pub fn mark_grid_need_list_failed(&self, error: &str, retrying: bool) {
        self.needed_grids.write().mark_failed(error, retrying);
    }

    /// PAN-93: apply one failure to both needed lists.
    pub fn mark_need_lists_unavailable(&self, error: &str, retrying: bool) {
        self.mark_dxcc_need_list_failed(error, retrying);
        self.mark_grid_need_list_failed(error, retrying);
    }

    /// PAN-93: cqdx.io does not offer the needed-grid list (404). A no-op
    /// once a grid list has loaded.
    pub fn mark_grid_need_list_not_offered(&self) {
        let mut list = self.needed_grids.write();
        if !list.is_loaded() {
            list.availability = NeedListAvailability::NotOffered;
        }
    }

    /// PAN-93: the needed-entity list's availability, scope and age.
    pub fn dxcc_need_list(&self) -> NeedListAvailability {
        self.needed_dxcc.read().availability.clone()
    }

    /// PAN-93: the needed-grid list's availability.
    pub fn grid_need_list(&self) -> NeedListAvailability {
        self.needed_grids.read().availability.clone()
    }

    /// PAN-93: cqdx.io's verdict for this station's entity. `Unknown` for
    /// the unresolved hash placeholder or while no list has loaded, with or
    /// without exclusions; a loaded empty list is `NotNeeded` for everyone.
    pub fn needed_dxcc_status(&self, callsign: &str) -> NeedStatus {
        let Some(upper) = resolved_identity(callsign) else {
            return NeedStatus::Unknown;
        };
        let list = self.needed_dxcc.read();
        if !list.is_loaded() {
            return NeedStatus::Unknown;
        }
        if list
            .entries
            .iter()
            .any(|prefix| upper.starts_with(prefix.as_str()))
        {
            NeedStatus::Needed
        } else {
            NeedStatus::NotNeeded
        }
    }

    /// PAN-93: cqdx.io's verdict for a grid's 4-char square. `Unknown` while
    /// no grid list has loaded (including a 404 "not offered") or when the
    /// grid is too short to name a square.
    pub fn needed_grid_status(&self, grid: &str) -> NeedStatus {
        let list = self.needed_grids.read();
        if !list.is_loaded() {
            return NeedStatus::Unknown;
        }
        // The DX's decoded grid may be 4 or 6 chars; the cqdx-populated set
        // is stored as 4-char uppercase fields (see CqdxBridge normalization).
        let Some(field) = grid.trim().get(..4) else {
            return NeedStatus::Unknown;
        };
        if list.entries.contains(&field.to_uppercase()) {
            NeedStatus::Needed
        } else {
            NeedStatus::NotNeeded
        }
    }

    /// PAN-93: `(needed, atno)` for display and wire surfaces (`+`/`!`, the
    /// station card, `CqdxSpotInfo`, the remote `DecodedView`). Never true
    /// while the list is unknown: the scorer's home/logged-prefix guess in
    /// `is_needed_dxcc` is not shown as a cqdx claim.
    ///
    /// PAN-347: this is the cqdx-only answer, kept for the remote
    /// `DecodedView` and cqdx spot info (the wire contract says cqdx). The
    /// local TUI uses `needed_atno_shown`, which adds the local logbook.
    pub fn display_needed_atno(&self, callsign: &str) -> (bool, bool) {
        let needed = self.needed_dxcc_status(callsign) == NeedStatus::Needed;
        (needed, needed && self.cqdx_is_atno(callsign))
    }

    /// PAN-347: needed-entity verdict and who answered. cqdx once its list
    /// loaded; else the local logbook once seeded, for a call that resolves
    /// to an entity; else `(Unknown, Guess)`.
    pub fn dxcc_need_verdict(&self, callsign: &str) -> (NeedStatus, NeedSource) {
        let Some(upper) = resolved_identity(callsign) else {
            return (NeedStatus::Unknown, NeedSource::Guess);
        };
        let entity = pancetta_core::dxcc::entity_for_callsign(&upper);
        self.verdict_for(&upper, entity)
    }

    /// PAN-347: `(needed, atno)` for the local TUI: cqdx's answer, else the
    /// local logbook's, never the scorer's guess.
    pub fn needed_atno_shown(&self, callsign: &str) -> (bool, bool) {
        match self.dxcc_need_verdict(callsign) {
            (NeedStatus::Needed, NeedSource::Cqdx) => (true, self.cqdx_is_atno(callsign)),
            (NeedStatus::Needed, NeedSource::LocalLogbook) => (true, true),
            _ => (false, false),
        }
    }

    /// `upper` is already resolved; `entity` is its offline entity, passed in
    /// so a per-frame caller resolves nothing (PAN-284).
    fn verdict_for(&self, upper: &str, entity: Option<&'static str>) -> (NeedStatus, NeedSource) {
        let cqdx = self.needed_dxcc_status(upper);
        if cqdx != NeedStatus::Unknown {
            return (cqdx, NeedSource::Cqdx);
        }
        let Some(entity) = entity else {
            return (NeedStatus::Unknown, NeedSource::Guess);
        };
        let worked = {
            let log = self.local_log.read();
            if !log.available {
                return (NeedStatus::Unknown, NeedSource::Guess);
            }
            log.is_worked(entity)
        };
        if worked || self.is_home_entity(Some(entity)) {
            (NeedStatus::NotNeeded, NeedSource::LocalLogbook)
        } else {
            (NeedStatus::Needed, NeedSource::LocalLogbook)
        }
    }

    /// cqdx's ATNO prefix set; empty (false) until a cqdx list loads.
    fn cqdx_is_atno(&self, callsign: &str) -> bool {
        let atno = self.needed_atno.read();
        if atno.is_empty() {
            return false;
        }
        // PAN-58: resolve a hash-render before prefix-matching — see
        // `resolved_identity`'s doc comment.
        let Some(upper) = resolved_identity(callsign) else {
            return false;
        };
        atno.iter().any(|prefix| upper.starts_with(prefix.as_str()))
    }

    pub fn update_rarity_scores(&self, scores: HashMap<String, f64>) {
        // Re-index by resolved DXCC entity (offline prefix resolver, same one
        // `worked_dxcc_on_band` uses) so `rarity()` can fall back to another
        // callsign's rarity from the same entity — see the field doc on
        // `rarity_by_entity` for why this matters.
        let mut by_entity: HashMap<String, f64> = HashMap::new();
        for (callsign, rarity) in &scores {
            if let Some(entity) = pancetta_core::dxcc::entity_for_callsign(callsign) {
                by_entity.insert(entity.to_string(), *rarity);
            }
        }
        *self.rarity_by_entity.write() = by_entity;
        *self.rarity_scores.write() = scores;
    }

    pub fn update_notable_callsigns(&self, callsigns: HashSet<String>) {
        *self.notable_callsigns.write() = callsigns;
    }

    pub fn update_network_snr(&self, data: HashMap<String, (u32, i32)>) {
        *self.network_snr.write() = data;
    }

    pub fn update_network_last_seen(&self, data: HashMap<String, i64>) {
        *self.network_last_seen.write() = data;
    }

    pub fn rarity(&self, callsign: &str) -> f64 {
        // PAN-58 round 1 (Codex P2): resolve before the exact-key lookup so
        // a resolved hash-render scores identically to its plain form.
        let Some(upper) = resolved_identity(callsign) else {
            return 0.5;
        };
        if let Some(r) = self.rarity_scores.read().get(&upper).copied() {
            return r;
        }
        // Entity-keyed fallback (BUG #163): this exact callsign was never
        // itself seen in a cqdx live-spot poll, but another callsign from the
        // same DXCC entity may have been — reuse that rarity rather than the
        // flat neutral default.
        if let Some(entity) = pancetta_core::dxcc::entity_for_callsign(&upper) {
            if let Some(r) = self.rarity_by_entity.read().get(entity).copied() {
                return r;
            }
        }
        0.5
    }

    pub fn record_failure(&self, callsign: &str) {
        // PAN-58 round 1: canonicalize on write so a later resolved-form
        // lookup (`is_recent_failure`) actually finds it — see
        // `resolved_identity`'s doc comment. Never record the unresolved
        // hash-miss placeholder; it isn't a real station identity.
        let Some(upper) = resolved_identity(callsign) else {
            return;
        };
        self.recent_failures.write().insert(upper);
    }

    pub fn record_worked(&self, callsign: &str, band: &str) {
        let _ = self.record_worked_at(callsign, band, Utc::now());
    }

    /// PAN-347: one logged contact: the band maps plus the local log's
    /// worked, confirmed and per-mode sets.
    fn record_logged_contact(
        &self,
        callsign: &str,
        band: &str,
        mode: &str,
        confirmed: bool,
        date: DateTime<Utc>,
    ) {
        if let Some(entity) = self.record_worked_at(callsign, band, date) {
            self.local_log.write().record(entity, mode, confirmed);
        }
    }

    /// Returns the resolved entity, if any (PAN-347), so callers resolve once.
    fn record_worked_at(
        &self,
        callsign: &str,
        band: &str,
        date: DateTime<Utc>,
    ) -> Option<&'static str> {
        // PAN-58 round 1 (Codex P1): canonicalize on write, matching
        // `is_duplicate`'s resolved-form read — otherwise a QSO completed
        // with a hash-rendered identity is stored under a key later lookups
        // (already resolved to plain form) never match, silently losing the
        // duplicate penalty. Never record the unresolved hash-miss
        // placeholder; it isn't a real station identity.
        let upper = resolved_identity(callsign)?;
        self.worked_on_band
            .write()
            .entry(band.trim().to_uppercase())
            .or_default()
            .entry(upper.clone())
            .and_modify(|old| *old = (*old).max(date))
            .or_insert(date);
        // Keep worked_dxcc_on_band live-updated as QSOs complete during the
        // session, not just at startup seeding — same resolver, same
        // skip-if-unresolvable behavior as seed_worked_dxcc_from_list.
        let entity = pancetta_core::dxcc::entity_for_callsign(&upper)?;
        self.worked_dxcc_on_band
            .write()
            .entry(band.trim().to_uppercase())
            .or_default()
            .insert(entity.to_string());
        // PAN-347: ATNO and the band maps never disagree.
        self.local_log.write().mark_worked(entity);
        Some(entity)
    }

    /// Record a worked grid square for #164 tier 4, called alongside
    /// `record_worked` on live QSO completion when the DX's grid is known.
    pub fn record_worked_grid(&self, grid: &str, band: &str) {
        let trimmed = grid.trim();
        if trimmed.len() < 4 {
            return;
        }
        let field = trimmed[..4].to_uppercase();
        self.worked_grids_on_band
            .write()
            .entry(band.trim().to_uppercase())
            .or_default()
            .insert(field);
    }
}

impl pancetta_tui::dx_need::DxNeedLookup for CachedStationLookup {
    fn lookup_need(
        &self,
        callsign: &str,
        entity: Option<&'static str>,
        grid: Option<&str>,
        known_state: Option<&str>,
        current_frequency_hz: f64,
        mode: Option<&str>,
    ) -> pancetta_tui::dx_need::DxNeedStatus {
        use pancetta_core::dxcc;
        use pancetta_tui::dx_need::{canonical_grid_square, DxNeedStatus};
        let Some(call) = resolved_identity(callsign) else {
            return DxNeedStatus::default();
        };
        let grid_available = *self.grid_history_available.read();
        let state_available = *self.state_history_available.read();
        let known_band = current_frequency_hz.is_finite()
            && current_frequency_hz > 0.0
            && !pancetta_qso::utils::frequency_to_band(current_frequency_hz).ends_with("MHZ");
        let grid_needed = grid
            .filter(|_| grid_available && known_band)
            .and_then(canonical_grid_square)
            .map(|square| self.is_grid_needed_on_band(&square, current_frequency_hz));
        let state = self.state_for(&call).or_else(|| {
            known_state
                .and_then(dxcc::normalize_us_state)
                .map(str::to_string)
        });
        // PAN-284: the row resolved its entity once, at creation; never
        // resolve `callsign` again here (this runs per row per frame).
        let eligible = entity.is_some_and(dxcc::is_us_related_entity);
        let state_needed = state
            .as_deref()
            .filter(|_| state_available && eligible)
            .map(|code| self.is_state_needed(code));
        // PAN-347: cqdx, else the local logbook, from the row's entity.
        let (status, source) = self.verdict_for(&call, entity);
        let dxcc_needed = match status {
            NeedStatus::Needed => Some(true),
            NeedStatus::NotNeeded => Some(false),
            NeedStatus::Unknown => None,
        };
        let dxcc_source = dxcc_needed.map(|_| source);
        let dxcc_list = match self.dxcc_need_list() {
            NeedListAvailability::Loaded { fetched_at, band }
                if dxcc_source == Some(NeedSource::Cqdx) =>
            {
                Some(pancetta_tui::dx_need::NeedListSource { band, fetched_at })
            }
            _ => None,
        };
        let home = self.is_home_entity(entity);
        let (entity_standing, new_mode) = {
            let log = self.local_log.read();
            match entity.filter(|_| log.available) {
                Some(e) => (
                    Some(log.standing(e)),
                    mode.filter(|_| !home).and_then(|m| log.new_in_mode(e, m)),
                ),
                None => (None, None),
            }
        };
        DxNeedStatus {
            grid_needed,
            state,
            state_needed,
            dxcc_needed,
            dxcc_list,
            dxcc_source,
            entity_standing,
            new_mode,
        }
    }
}

impl WorkedStationLookup for CachedStationLookup {
    fn is_duplicate(&self, callsign: &str, freq_hz: f64) -> bool {
        // PAN-58: an already-worked station heard again via a resolved
        // hash-render must still count as a duplicate — see
        // `resolved_identity`'s doc comment.
        let Some(upper) = resolved_identity(callsign) else {
            return false;
        };
        let band = pancetta_qso::utils::frequency_to_band(freq_hz).to_uppercase();
        let worked = self.worked_on_band.read();
        worked
            .get(&band)
            .is_some_and(|set| set.contains_key(&upper))
    }

    fn is_recent_failure(&self, callsign: &str) -> bool {
        // PAN-58 round 1 (Codex P2): resolve before the exact-key lookup —
        // see `resolved_identity`'s doc comment.
        let Some(upper) = resolved_identity(callsign) else {
            return false;
        };
        self.recent_failures.read().contains(&upper)
    }

    fn is_needed_dxcc(&self, callsign: &str) -> bool {
        // PAN-16 / PAN-58: FT8's `"<...>"` placeholder for an unresolved
        // i3=4 nonstandard-callsign hash (message.rs's `parse_nonstd_call`,
        // used when the local hash table has no entry for the 12-bit hash)
        // is not a callsign at all — it can never be scored as "needed".
        // Without this guard it fell through every branch below into
        // "needed" (the historical-default branch treats anything
        // non-excluded as needed; the exclusion-set branch treats anything
        // that doesn't match a *real* prefix as automatically outside the
        // excluded set too) and picked up `needed_dxcc`'s weight — the
        // scorer's single largest term — ranking an uncallable placeholder
        // above real, workable stations. `resolved_identity` rejects this
        // exact literal, not "any callsign whose prefix doesn't resolve in
        // the bundled offline DXCC table" (unlike `is_dxcc_needed_on_band`'s
        // broader `entity_for_callsign` guard) — cqdx's `needed_dxcc` set
        // can name a prefix that's newer than, or otherwise absent from,
        // the bundled static BigCTY table, and a real, valid,
        // cqdx-confirmed-needed callsign with that prefix must still win
        // via the branches below, not be silently zeroed here just because
        // the local table has a gap.
        //
        // PAN-58: a *resolved* hash form `<CALLSIGN>` (a real, identifiable
        // callsign the local hash table did map) is also a real callsign
        // and must keep resolving/scoring normally against the branches
        // below — `resolved_identity` strips its brackets first so the
        // leading `'<'` doesn't defeat every prefix match that follows.
        let Some(upper) = resolved_identity(callsign) else {
            return false;
        };
        match self.dxcc_need_verdict(&upper).0 {
            // A loaded cqdx list (empty means "nothing needed"), else the
            // local logbook (PAN-347): never logged means needed.
            NeedStatus::Needed => true,
            NeedStatus::NotNeeded => false,
            // PAN-347 guess, scoring only: neither a cqdx list nor the local
            // logbook has answered. Every entity except the operator's home
            // entity scores as needed; displays never show this guess.
            NeedStatus::Unknown => {
                !self.is_home_entity(pancetta_core::dxcc::entity_for_callsign(&upper))
            }
        }
    }

    fn is_atno(&self, callsign: &str) -> bool {
        // PAN-347: the local logbook's ATNO is its needed verdict (never
        // logged on any band or mode); otherwise cqdx's ATNO list.
        match self.dxcc_need_verdict(callsign) {
            (status, NeedSource::LocalLogbook) => status == NeedStatus::Needed,
            _ => self.cqdx_is_atno(callsign),
        }
    }

    fn need_source(&self, callsign: &str) -> NeedSource {
        self.dxcc_need_verdict(callsign).1
    }

    fn is_dxcc_needed_on_band(&self, callsign: &str, freq_hz: f64) -> bool {
        // BUG #163 follow-up: this signal is purely "never worked this
        // entity on this band per local history" — with no concept of home
        // exclusion, a fresh session (nothing worked on ANY band yet) would
        // trivially claim the operator's OWN callsign is "needed" too.
        // Mirror is_needed_dxcc's home-exclusion semantics so a home-country
        // station is never "needed," regardless of local worked history.

        // AP-decoding Task 5 review fix: `freq_hz <= 0.0` means "no real RF
        // dial frequency available" (callers like the recent_calls/Ap2
        // ranking pool in `coordinator/ft8.rs` pass 0.0 because
        // `RecentCallAp` carries no frequency data at all). Naively
        // classifying that into a band string bins it into a synthetic
        // "0MHZ" bucket (`freq_hz_to_band`'s fallthrough) that no real QSO
        // is EVER logged against — so `!worked.get(&band).is_some_and(..)`
        // would return `true` (needed) for essentially every callsign,
        // always, corrupting `needed_dxcc` (the largest weight in
        // `score_cq_detailed`) into a constant `true`. Return `false`
        // instead: with no real per-band evidence, this signal contributes
        // nothing, and `needed_dxcc` correctly falls back to the reliable
        // global `is_needed_dxcc` check alone.
        if freq_hz <= 0.0 {
            return false;
        }

        // PAN-58 round 1 (Codex P1): this exclusion check used the raw
        // callsign, so a resolved hash-render ("<N7RLK>") never matched the
        // excluded "N" prefix and fell through to `entity_for_callsign`
        // below (which DOES resolve) — still returning `needed=true` for an
        // already-excluded home station via the ORed `is_needed_dxcc` path.
        // Use the same resolved identity for both checks.
        let Some(upper) = resolved_identity(callsign) else {
            return false;
        };
        // Unresolvable callsign (no matching prefix in the offline table):
        // never claim "needed" for an entity we can't even identify.
        let Some(entity) = pancetta_core::dxcc::entity_for_callsign(&upper) else {
            return false;
        };
        // PAN-347: the home entity, not letter prefixes. A DL and a DA call
        // are one entity; KH6 is not the US.
        if self.is_home_entity(Some(entity)) {
            return false;
        }
        let band = pancetta_qso::utils::frequency_to_band(freq_hz).to_uppercase();
        let worked = self.worked_dxcc_on_band.read();
        !worked.get(&band).is_some_and(|set| set.contains(entity))
    }

    fn is_grid_needed_on_band(&self, grid: &str, freq_hz: f64) -> bool {
        let trimmed = grid.trim();
        if trimmed.len() < 4 {
            return false;
        }
        let field = trimmed[..4].to_uppercase();
        let band = pancetta_qso::utils::frequency_to_band(freq_hz).to_uppercase();
        !self
            .worked_grids_on_band
            .read()
            .get(&band)
            .is_some_and(|s| s.contains(&field))
    }

    fn is_needed_grid(&self, grid: &str) -> bool {
        // Scoring policy over the PAN-93 verdict: only a loaded cqdx list
        // naming the square adds the needed_grid weight. `Unknown` (no list,
        // 404 "not offered", short grid) means "no bonus" rather than
        // inflating every score; display surfaces read `needed_grid_status`.
        self.needed_grid_status(grid) == NeedStatus::Needed
    }

    fn rarity(&self, callsign: &str) -> f64 {
        // Delegates to the inherent method above (single source of truth —
        // avoids the two copies silently diverging).
        CachedStationLookup::rarity(self, callsign)
    }

    fn is_notable(&self, callsign: &str) -> bool {
        // PAN-58: resolve a hash-render before the exact-match lookup —
        // see `resolved_identity`'s doc comment.
        let Some(upper) = resolved_identity(callsign) else {
            return false;
        };
        self.notable_callsigns.read().contains(&upper)
    }

    fn network_snr(&self, callsign: &str) -> Option<(u32, i32)> {
        // PAN-58 round 1 (Codex P2): resolve before the exact-key lookup —
        // see `resolved_identity`'s doc comment. cqdx's network data is
        // always keyed by a plain callsign; only the query side (a local
        // decode) can be hash-rendered.
        let upper = resolved_identity(callsign)?;
        self.network_snr.read().get(&upper).copied()
    }

    fn network_last_seen(&self, callsign: &str) -> Option<i64> {
        let upper = resolved_identity(callsign)?;
        self.network_last_seen.read().get(&upper).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn display_adapter_gates_availability_and_reuses_grid_predicate() {
        use pancetta_tui::dx_need::{DxNeedLookup, NeedListSource};
        let l = CachedStationLookup::new();
        // PAN-93: dxcc_needed is None until a list loads, then Some(true/false)
        // with its source.
        assert_eq!(
            l.lookup_need(
                "JA1ABC",
                pancetta_core::dxcc::entity_for_callsign("JA1ABC"),
                None,
                None,
                14_074_000.0,
                None
            )
            .dxcc_needed,
            None
        );
        assert_eq!(
            l.lookup_need(
                "JA1ABC",
                pancetta_core::dxcc::entity_for_callsign("JA1ABC"),
                None,
                None,
                14_074_000.0,
                None
            )
            .dxcc_list,
            None
        );
        let at = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        l.update_needed_dxcc_for_band(HashSet::from(["JA".into()]), Some("20M".into()), at);
        let s = l.lookup_need(
            "JA1ABC",
            pancetta_core::dxcc::entity_for_callsign("JA1ABC"),
            None,
            None,
            14_074_000.0,
            None,
        );
        assert_eq!(s.dxcc_needed, Some(true));
        assert_eq!(
            s.dxcc_list,
            Some(NeedListSource {
                band: Some("20M".into()),
                fetched_at: at
            })
        );
        assert_eq!(
            l.lookup_need(
                "DL1ABC",
                pancetta_core::dxcc::entity_for_callsign("DL1ABC"),
                None,
                None,
                14_074_000.0,
                None
            )
            .dxcc_needed,
            Some(false)
        );
        let l = CachedStationLookup::new();
        assert!(l.is_grid_needed_on_band("EM34", 14_074_000.0));
        assert_eq!(
            l.lookup_need(
                "W5ABC",
                pancetta_core::dxcc::entity_for_callsign("W5ABC"),
                Some("EM34"),
                Some("AR"),
                14_074_000.0,
                None
            )
            .grid_needed,
            None
        );
        let db = pancetta_qso::async_database::QsoDatabase::new_in_memory()
            .await
            .unwrap();
        l.seed_display_history(&db).await;
        l.record_worked_grid("em34cd", "20m");
        for grid in ["EM34", "em34ab", "FN42", "PM95"] {
            for hz in [14_074_000.0, 7_074_000.0] {
                assert_eq!(
                    l.lookup_need(
                        "W5ABC",
                        pancetta_core::dxcc::entity_for_callsign("W5ABC"),
                        Some(grid),
                        None,
                        hz,
                        None
                    )
                    .grid_needed,
                    Some(l.is_grid_needed_on_band(grid, hz))
                );
            }
        }
        for grid in [
            "",
            "EM",
            "EM3",
            "ZZ99",
            "ééé",
            "EM3é",
            "EM34☃",
            "EM34garbage",
        ] {
            assert_eq!(
                l.lookup_need(
                    "W5ABC",
                    pancetta_core::dxcc::entity_for_callsign("W5ABC"),
                    Some(grid),
                    None,
                    14_074_000.0,
                    None
                )
                .grid_needed,
                None,
                "{grid}"
            );
        }
        for hz in [0.0, -1.0, f64::NAN, f64::INFINITY, 123_000_000.0] {
            assert_eq!(
                l.lookup_need(
                    "W5ABC",
                    pancetta_core::dxcc::entity_for_callsign("W5ABC"),
                    Some("EM34"),
                    None,
                    hz,
                    None
                )
                .grid_needed,
                None
            );
        }
        assert_eq!(
            l.lookup_need(
                "<...>",
                pancetta_core::dxcc::entity_for_callsign("<...>"),
                Some("EM34"),
                Some("AR"),
                14_074_000.0,
                None
            ),
            Default::default()
        );
        assert_eq!(
            l.lookup_need(
                "JA1ABC",
                pancetta_core::dxcc::entity_for_callsign("JA1ABC"),
                None,
                Some("CA"),
                14_074_000.0,
                None
            )
            .state_needed,
            None
        );
        l.record_state("W5ABC", "AR");
        let need = l.lookup_need(
            "<W5ABC>",
            pancetta_core::dxcc::entity_for_callsign("<W5ABC>"),
            None,
            Some("CA"),
            14_074_000.0,
            None,
        );
        assert_eq!(need.state.as_deref(), Some("AR"));
        assert_eq!(need.state_needed, Some(true));
        l.record_worked_state("W1AW", "AR");
        assert_eq!(
            l.lookup_need(
                "W5ABC",
                pancetta_core::dxcc::entity_for_callsign("W5ABC"),
                None,
                None,
                7_074_000.0,
                None
            )
            .state_needed,
            Some(false)
        );
    }

    /// PAN-284: the display adapter runs per row per frame, so it takes state
    /// eligibility from the entity the row resolved at creation and never
    /// resolves the callsign itself.
    #[tokio::test]
    async fn display_adapter_takes_state_eligibility_from_the_carried_entity() {
        use pancetta_tui::dx_need::DxNeedLookup;
        let l = CachedStationLookup::new();
        let db = pancetta_qso::async_database::QsoDatabase::new_in_memory()
            .await
            .unwrap();
        l.seed_display_history(&db).await;
        // JA1ABC resolves to Japan, but the row says United States: eligible.
        assert_eq!(
            l.lookup_need(
                "JA1ABC",
                Some("United States"),
                None,
                Some("AR"),
                14_074_000.0,
                None
            )
            .state_needed,
            Some(true)
        );
        // W5ABC resolves to United States, but the row says Japan: not eligible.
        assert_eq!(
            l.lookup_need("W5ABC", Some("Japan"), None, Some("AR"), 14_074_000.0, None)
                .state_needed,
            None
        );
    }

    #[tokio::test]
    async fn display_history_availability_requires_successful_seeds() {
        let l = CachedStationLookup::new();
        assert!(!*l.grid_history_available.read());
        assert!(!*l.state_history_available.read());
        let db = pancetta_qso::async_database::QsoDatabase::new_in_memory()
            .await
            .unwrap();
        l.seed_display_history(&db).await;
        assert!(*l.grid_history_available.read());
        assert!(*l.state_history_available.read());
        l.record_worked_state("W1AW", "AR");
        l.record_worked_grid("EM34", "20m");
        db.close().await;
        l.seed_display_history(&db).await;
        assert!(!*l.grid_history_available.read());
        assert!(!*l.state_history_available.read());
        assert!(!l.is_state_needed("AR"));
        assert!(!l.is_grid_needed_on_band("EM34", 14_074_000.0));
        let reopened = pancetta_qso::async_database::QsoDatabase::new_in_memory()
            .await
            .unwrap();
        l.seed_display_history(&reopened).await;
        assert!(*l.grid_history_available.read());
        assert!(*l.state_history_available.read());
        assert!(
            !l.is_state_needed("AR"),
            "empty seed preserves live additions"
        );
    }

    #[test]
    fn worked_states_are_contacts_not_observations() {
        let l = CachedStationLookup::new();
        l.record_state("W5ABC", " ar ");
        assert!(l.is_state_needed("AR"));
        l.record_worked_state("<W5ABC>", " ar ");
        l.record_worked_state("W5ABC", "AR");
        assert!(!l.is_state_needed("ar"));
        assert_eq!(l.worked_states.read().len(), 1);
        for (call, state) in [
            ("JA1ABC", "CA"),
            ("<...>", "CA"),
            ("???", "CA"),
            ("W1AW", "XX"),
        ] {
            l.record_worked_state(call, state);
        }
        assert!(l.is_state_needed("CA"));
        for code in ["AK", "HI", "AS", "GU", "MP", "PR", "VI", "DC"] {
            l.record_worked_state("W1AW", code);
            assert!(!l.is_state_needed(code));
        }
        assert!(l.is_state_needed("MD"), "DC remains distinct from MD");
        assert!(!l.is_state_needed("invalid"));
    }

    #[test]
    fn historical_states_accumulate_and_seeding_preserves_live_contacts() {
        let l = CachedStationLookup::new();
        l.record_worked_state("W1AW", "TX");
        l.record_worked_grid("EM34", "20m");
        l.seed_states_from_list(vec![
            ("W5ABC".into(), "AR".into()),
            ("W5ABC".into(), "CA".into()),
        ]);
        l.seed_worked_grids_from_list(vec![("40m".into(), "CM87".into())]);
        for code in ["AR", "CA", "TX"] {
            assert!(!l.is_state_needed(code));
        }
        assert_eq!(l.state_for("W5ABC").as_deref(), Some("CA"));
        assert!(!l.is_grid_needed_on_band("em34ab", 14_074_000.0));
        assert!(l.is_grid_needed_on_band("EM34", 7_074_000.0));
    }

    #[test]
    fn test_band_aware_duplicate() {
        let lookup = CachedStationLookup::new();

        // Work K9ZZ on 20m
        lookup.record_worked("K9ZZ", "20m");

        // Should be duplicate on 20m
        assert!(lookup.is_duplicate("K9ZZ", 14_074_000.0));

        // Should NOT be duplicate on 40m
        assert!(!lookup.is_duplicate("K9ZZ", 7_074_000.0));

        // Should NOT be duplicate on 15m
        assert!(!lookup.is_duplicate("K9ZZ", 21_074_000.0));

        // Work K9ZZ on 40m too
        lookup.record_worked("K9ZZ", "40m");

        // Now duplicate on both bands
        assert!(lookup.is_duplicate("K9ZZ", 14_074_000.0));
        assert!(lookup.is_duplicate("K9ZZ", 7_074_000.0));

        // Still not on 15m
        assert!(!lookup.is_duplicate("K9ZZ", 21_074_000.0));
    }

    #[test]
    fn test_unknown_frequency_not_duplicate() {
        let lookup = CachedStationLookup::new();
        lookup.record_worked("K9ZZ", "20M");
        // freq_hz=0.0 (uninitialized) should not match any band
        assert!(!lookup.is_duplicate("K9ZZ", 0.0));
    }

    #[test]
    fn test_seed_worked_from_list() {
        let lookup = CachedStationLookup::new();
        lookup.seed_worked_from_list("20m", vec!["W1ABC".into(), "K2DEF".into()]);

        assert!(lookup.is_duplicate("W1ABC", 14_074_000.0));
        assert!(lookup.is_duplicate("K2DEF", 14_074_000.0));
        assert!(!lookup.is_duplicate("W1ABC", 7_074_000.0)); // not on 40m
    }

    // --- DX Hunter per-band-needed (2026-07-18) ---

    #[test]
    fn dxcc_needed_on_band_true_before_ever_worked() {
        let lookup = CachedStationLookup::new();
        // JA1ABC resolves to a real DXCC entity (Japan) via the offline
        // prefix table; nothing has been worked yet, so it's needed.
        assert!(lookup.is_dxcc_needed_on_band("JA1ABC", 14_074_000.0));
    }

    #[test]
    fn dxcc_needed_on_band_false_after_working_that_band() {
        let lookup = CachedStationLookup::new();
        lookup.record_worked("JA1ABC", "20m");
        assert!(!lookup.is_dxcc_needed_on_band("JA1ABC", 14_074_000.0));
    }

    #[test]
    fn dxcc_needed_on_band_true_on_a_different_band() {
        // The whole point of this feature: worked on 20m must NOT satisfy
        // "needed" on 40m — this is per-band, not all-time.
        let lookup = CachedStationLookup::new();
        lookup.record_worked("JA1ABC", "20m");
        assert!(lookup.is_dxcc_needed_on_band("JA1ABC", 7_074_000.0));
    }

    #[test]
    fn dxcc_needed_on_band_is_entity_scoped_not_callsign_scoped() {
        // Working a DIFFERENT station from the SAME DXCC entity on a band
        // must also satisfy "needed" for that band — this tracks entities,
        // not individual callsigns (unlike is_duplicate).
        let lookup = CachedStationLookup::new();
        lookup.record_worked("JA2XYZ", "20m");
        assert!(!lookup.is_dxcc_needed_on_band("JA1ABC", 14_074_000.0));
    }

    #[test]
    fn dxcc_needed_on_band_false_for_unresolvable_callsign() {
        // A callsign with no matching entry in the offline prefix table
        // must never be reported as "needed" for an entity we can't even
        // identify (safe default, matches the fn's own doc contract).
        let lookup = CachedStationLookup::new();
        assert!(!lookup.is_dxcc_needed_on_band("1", 14_074_000.0));
    }

    #[test]
    fn dxcc_needed_on_band_false_for_zero_freq_even_when_never_worked() {
        // AP-decoding Task 5 review fix: `freq_hz = 0.0` means "no real RF
        // dial frequency available" (the recent_calls/Ap2 ranking pool in
        // `coordinator/ft8.rs` has none). Before the fix, `freq_hz = 0.0`
        // classified into the synthetic "0MHZ" band, which is NEVER
        // populated by real QSOs — so `!worked.get(&band).is_some_and(..)`
        // returned `true` (needed) unconditionally, for every callsign,
        // even ones that are NOT genuinely needed. Assert the opposite of
        // `dxcc_needed_on_band_true_before_ever_worked` (which uses a real
        // freq_hz) to prove the always-true corruption is gone: with no
        // real frequency, this band-scoped signal must be inert (false),
        // never a universal "needed" hit.
        let lookup = CachedStationLookup::new();
        assert!(!lookup.is_dxcc_needed_on_band("JA1ABC", 0.0));

        // Also true even for an entity that WOULD read as needed on a real
        // band (nothing worked anywhere yet) — freq_hz=0.0 must short-
        // circuit before any worked-on-band lookup happens at all.
        assert!(lookup.is_dxcc_needed_on_band("JA1ABC", 14_074_000.0));
        assert!(!lookup.is_dxcc_needed_on_band("JA1ABC", 0.0));
    }

    #[test]
    fn score_cq_detailed_not_needed_dxcc_with_zero_freq_when_not_globally_needed() {
        // End-to-end regression for the AP-decoding Task 5 review finding:
        // `recent_calls_scorer.evaluate_cq(callsign, None, snr, 0.0)`
        // (`coordinator/ft8.rs`) must NOT inflate `needed_dxcc` to `true`
        // for a callsign that is neither globally needed (cqdx set) nor
        // an ATNO — even though `is_dxcc_needed_on_band` used to always
        // report "needed" for freq_hz=0.0 regardless of truth, corrupting
        // the OR in `score_cq_detailed`'s `needed_dxcc` term (weight 0.35,
        // the largest single weight in the formula).
        use pancetta_qso::priority::{PriorityScorer, PriorityWeights};

        let lookup = CachedStationLookup::new();
        // Populate cqdx's needed-DXCC set with a prefix that does NOT match
        // JA1ABC ("DL" only) — this puts `is_needed_dxcc` on the
        // cqdx-populated branch (prefix-match, no exclusion fallback
        // involved), so `is_needed_dxcc("JA1ABC")` is false purely because
        // JA isn't in cqdx's needed set, genuinely not needed. Critically,
        // no home entity is set here, so
        // `is_dxcc_needed_on_band`'s own home short-circuit can't mask
        // the freq_hz=0.0 corruption under test — before the fix, it would
        // still (wrongly) report "needed" for the synthetic "0MHZ" band.
        let mut needed = HashSet::new();
        needed.insert("DL".to_string());
        lookup.update_needed_dxcc(needed);
        assert!(!lookup.is_needed_dxcc("JA1ABC"));

        let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));
        let breakdown = scorer.score_cq_detailed("JA1ABC", None, -10, 0.0);
        assert_eq!(
            breakdown.needed_dxcc, 0.0,
            "needed_dxcc must reflect the genuinely-not-needed global \
             signal, not be forced to 1.0 by the freq_hz=0.0 band-scoped \
             OR term"
        );
    }

    #[test]
    fn score_cq_detailed_still_needed_dxcc_via_global_signal_with_zero_freq() {
        // Companion test: a genuinely globally-needed callsign must still
        // score as needed through `is_needed_dxcc` even with freq_hz=0.0 —
        // the fix must not throw out the reliable global signal along with
        // the corrupted band-scoped one.
        use pancetta_qso::priority::{PriorityScorer, PriorityWeights};

        let lookup = CachedStationLookup::new();
        let mut needed = HashSet::new();
        needed.insert("JA".to_string());
        lookup.update_needed_dxcc(needed);

        let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));
        let breakdown = scorer.score_cq_detailed("JA1ABC", None, -10, 0.0);
        assert_eq!(
            breakdown.needed_dxcc, 1.0,
            "genuinely needed (via cqdx global set) must still score as \
             needed even when freq_hz=0.0"
        );
    }

    #[test]
    fn dxcc_needed_on_band_excludes_home_entity_even_when_nothing_worked_yet() {
        // BUG #163 follow-up: a fresh session (no QSOs on ANY band yet) must
        // NOT report the operator's own excluded (home) callsign as "needed
        // on this band" just because nothing has been worked there — that
        // would incorrectly boost the home station's own score once this
        // signal feeds into PriorityScorer.
        let lookup = CachedStationLookup::new();
        lookup.set_home_entity(Some("United States"));

        assert!(!lookup.is_dxcc_needed_on_band("K5ARH", 14_074_000.0));
        // Non-excluded (foreign) entity is unaffected.
        assert!(lookup.is_dxcc_needed_on_band("JA1ABC", 14_074_000.0));
    }

    #[test]
    fn seed_worked_dxcc_from_list_matches_record_worked() {
        let seeded = CachedStationLookup::new();
        seeded.seed_worked_dxcc_from_list(vec![("20m".to_string(), "JA1ABC".to_string())]);

        let recorded = CachedStationLookup::new();
        recorded.record_worked("JA1ABC", "20m");

        for lookup in [&seeded, &recorded] {
            assert!(!lookup.is_dxcc_needed_on_band("JA1ABC", 14_074_000.0));
            assert!(lookup.is_dxcc_needed_on_band("JA1ABC", 7_074_000.0));
        }
    }

    #[test]
    fn seed_worked_dxcc_from_list_skips_unresolvable_callsigns_without_panicking() {
        let lookup = CachedStationLookup::new();
        lookup.seed_worked_dxcc_from_list(vec![
            ("20m".to_string(), "1".to_string()),
            ("20m".to_string(), "JA1ABC".to_string()),
        ]);
        assert!(!lookup.is_dxcc_needed_on_band("JA1ABC", 14_074_000.0));
    }

    // --- DX Hunter per-band grid tracking (#164 tier 4) ---

    #[test]
    fn grid_needed_on_band_true_before_ever_worked() {
        let lookup = CachedStationLookup::new();
        assert!(lookup.is_grid_needed_on_band("PM95", 14_074_000.0));
    }

    #[test]
    fn grid_needed_on_band_false_after_working_that_band() {
        let lookup = CachedStationLookup::new();
        lookup.record_worked_grid("PM95", "20m");
        assert!(!lookup.is_grid_needed_on_band("PM95", 14_074_000.0));
    }

    #[test]
    fn grid_needed_on_band_true_on_a_different_band() {
        let lookup = CachedStationLookup::new();
        lookup.record_worked_grid("PM95", "20m");
        assert!(lookup.is_grid_needed_on_band("PM95", 7_074_000.0));
    }

    #[test]
    fn grid_needed_on_band_case_insensitive_and_4_char_field() {
        let lookup = CachedStationLookup::new();
        lookup.record_worked_grid("pm95xx", "20m"); // 6-char, lowercase
        assert!(!lookup.is_grid_needed_on_band("PM95", 14_074_000.0));
    }

    #[test]
    fn grid_needed_on_band_false_for_too_short_grid() {
        let lookup = CachedStationLookup::new();
        assert!(!lookup.is_grid_needed_on_band("PM", 14_074_000.0));
    }

    #[test]
    fn seed_worked_grids_from_list_matches_record_worked_grid() {
        let seeded = CachedStationLookup::new();
        seeded.seed_worked_grids_from_list(vec![("20m".to_string(), "PM95".to_string())]);

        let recorded = CachedStationLookup::new();
        recorded.record_worked_grid("PM95", "20m");

        for lookup in [&seeded, &recorded] {
            assert!(!lookup.is_grid_needed_on_band("PM95", 14_074_000.0));
            assert!(lookup.is_grid_needed_on_band("PM95", 7_074_000.0));
        }
    }

    // --- BUG #163: rarity entity-keyed fallback ---

    #[test]
    fn rarity_exact_callsign_match_wins_over_entity_fallback() {
        let lookup = CachedStationLookup::new();
        let mut scores = HashMap::new();
        scores.insert("JA1ABC".to_string(), 0.9);
        lookup.update_rarity_scores(scores);
        assert!((lookup.rarity("JA1ABC") - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn rarity_falls_back_to_entity_when_exact_callsign_never_spotted() {
        // JA1ABC and JA2XYZ resolve to the same DXCC entity (Japan), per
        // `dxcc_needed_on_band_is_entity_scoped_not_callsign_scoped` above.
        // cqdx's live-spots feed only ever reported JA1ABC's rarity; JA2XYZ
        // was never itself spotted but should still inherit real data
        // instead of the flat 0.5 default.
        let lookup = CachedStationLookup::new();
        let mut scores = HashMap::new();
        scores.insert("JA1ABC".to_string(), 0.9);
        lookup.update_rarity_scores(scores);

        assert!(
            (lookup.rarity("JA2XYZ") - 0.9).abs() < f64::EPSILON,
            "expected JA2XYZ to inherit JA1ABC's entity rarity, got {}",
            lookup.rarity("JA2XYZ")
        );
    }

    #[test]
    fn rarity_defaults_to_neutral_when_entity_never_reported_either() {
        let lookup = CachedStationLookup::new();
        let mut scores = HashMap::new();
        scores.insert("JA1ABC".to_string(), 0.9);
        lookup.update_rarity_scores(scores);

        // W1ABC is a different (US) entity, never reported at all.
        assert!((lookup.rarity("W1ABC") - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn rarity_via_trait_object_matches_inherent_method() {
        // Guards against the inherent method and the WorkedStationLookup
        // trait impl silently diverging (the trait impl delegates to the
        // inherent one) — exercise through the trait object, as
        // PriorityScorer actually does.
        let lookup = CachedStationLookup::new();
        let mut scores = HashMap::new();
        scores.insert("JA1ABC".to_string(), 0.9);
        lookup.update_rarity_scores(scores);

        let boxed: Box<dyn WorkedStationLookup> = Box::new(lookup.clone());
        assert!((boxed.rarity("JA2XYZ") - lookup.rarity("JA2XYZ")).abs() < f64::EPSILON);
    }

    #[test]
    fn unloaded_dxcc_list_reports_unknown_not_needed() {
        // PAN-93 Gherkin scenario 1: no cqdx answer → "unknown", with or
        // without exclusions.
        let lookup = CachedStationLookup::new();
        assert_eq!(lookup.dxcc_need_list(), NeedListAvailability::NotConfigured);
        for call in ["K1ABC", "JA1XYZ", "3Y/B1234"] {
            assert_eq!(lookup.needed_dxcc_status(call), NeedStatus::Unknown);
        }
        lookup.set_home_entity(Some("United States"));
        assert_eq!(lookup.needed_dxcc_status("JA1XYZ"), NeedStatus::Unknown);
        assert_eq!(lookup.display_needed_atno("JA1XYZ"), (false, false));
    }

    #[test]
    fn unloaded_grid_list_reports_unknown_not_not_needed() {
        // PAN-93 Gherkin scenario 2.
        let lookup = CachedStationLookup::new();
        assert_eq!(lookup.needed_grid_status("FN42"), NeedStatus::Unknown);
        lookup.mark_grid_need_list_not_offered();
        assert_eq!(lookup.grid_need_list(), NeedListAvailability::NotOffered);
        assert_eq!(lookup.needed_grid_status("FN42"), NeedStatus::Unknown);
    }

    #[test]
    fn empty_loaded_dxcc_list_means_nothing_needed() {
        // PAN-93 / P04: a successful empty answer is not "everything needed".
        let lookup = CachedStationLookup::new();
        lookup.set_home_entity(Some("United States"));
        lookup.update_needed_dxcc(HashSet::new());
        assert_eq!(lookup.needed_dxcc_status("JA1XYZ"), NeedStatus::NotNeeded);
        assert!(!lookup.is_needed_dxcc("JA1XYZ"));
        assert_eq!(lookup.display_needed_atno("JA1XYZ"), (false, false));
    }

    #[test]
    fn empty_loaded_grid_list_is_confirmed_not_needed() {
        let lookup = CachedStationLookup::new();
        lookup.update_needed_grids(HashSet::new());
        assert_eq!(lookup.needed_grid_status("FN42"), NeedStatus::NotNeeded);
        assert!(!lookup.is_needed_grid("FN42"));
    }

    #[test]
    fn loaded_lists_report_needed_and_not_needed_with_scope() {
        let lookup = CachedStationLookup::new();
        let at = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        lookup.update_needed_dxcc_for_band(HashSet::from(["JA".into()]), Some("20M".into()), at);
        assert_eq!(lookup.needed_dxcc_status("JA1ABC"), NeedStatus::Needed);
        assert_eq!(lookup.needed_dxcc_status("<JA1ABC>"), NeedStatus::Needed);
        assert_eq!(lookup.needed_dxcc_status("DL1ABC"), NeedStatus::NotNeeded);
        assert_eq!(
            lookup.dxcc_need_list(),
            NeedListAvailability::Loaded {
                fetched_at: at,
                band: Some("20M".into())
            }
        );
        lookup.update_needed_atno(HashSet::from(["JA".into()]));
        assert_eq!(lookup.display_needed_atno("JA1ABC"), (true, true));
        assert_eq!(lookup.display_needed_atno("DL1ABC"), (false, false));
        lookup.update_needed_grids(HashSet::from(["FN42".into()]));
        assert_eq!(lookup.needed_grid_status("fn42ab"), NeedStatus::Needed);
        assert_eq!(lookup.needed_grid_status("JO62"), NeedStatus::NotNeeded);
        assert_eq!(lookup.needed_grid_status("FN"), NeedStatus::Unknown);
        assert!(lookup.is_needed_grid("FN42"));
        assert!(!lookup.is_needed_grid("JO62"));
    }

    #[test]
    fn failure_after_a_load_keeps_the_last_answer() {
        let lookup = CachedStationLookup::new();
        lookup.update_needed_dxcc(HashSet::from(["JA".into()]));
        lookup.mark_dxcc_need_list_failed("HTTP request failed", true);
        assert!(matches!(
            lookup.dxcc_need_list(),
            NeedListAvailability::Loaded { .. }
        ));
        assert_eq!(lookup.needed_dxcc_status("JA1ABC"), NeedStatus::Needed);
        lookup.update_needed_grids(HashSet::new());
        lookup.mark_grid_need_list_failed("Server error: 503", true);
        lookup.mark_grid_need_list_not_offered();
        assert!(matches!(
            lookup.grid_need_list(),
            NeedListAvailability::Loaded { .. }
        ));
    }

    #[test]
    fn failure_before_any_load_is_unavailable_and_keeps_its_first_time() {
        let lookup = CachedStationLookup::new();
        lookup.set_need_lists_loading();
        lookup.mark_need_lists_unavailable("HTTP request failed", true);
        let NeedListAvailability::Unavailable { since: first, .. } = lookup.dxcc_need_list() else {
            panic!("expected Unavailable, got {:?}", lookup.dxcc_need_list());
        };
        std::thread::sleep(std::time::Duration::from_millis(5));
        lookup.mark_dxcc_need_list_failed("Authentication failed", false);
        assert_eq!(
            lookup.dxcc_need_list(),
            NeedListAvailability::Unavailable {
                since: first,
                error: "Authentication failed".into(),
                retrying: false,
            }
        );
        assert!(matches!(
            lookup.grid_need_list(),
            NeedListAvailability::Unavailable { retrying: true, .. }
        ));
        assert_eq!(lookup.needed_dxcc_status("JA1ABC"), NeedStatus::Unknown);
        assert_eq!(lookup.needed_grid_status("FN42"), NeedStatus::Unknown);
    }

    #[test]
    fn loading_does_not_downgrade_a_loaded_list() {
        let lookup = CachedStationLookup::new();
        lookup.set_need_lists_loading();
        assert_eq!(lookup.dxcc_need_list(), NeedListAvailability::Loading);
        assert_eq!(lookup.grid_need_list(), NeedListAvailability::Loading);
        lookup.update_needed_dxcc(HashSet::from(["JA".into()]));
        lookup.mark_grid_need_list_failed("Server error: 503", true);
        lookup.set_need_lists_loading();
        assert!(matches!(
            lookup.dxcc_need_list(),
            NeedListAvailability::Loaded { .. }
        ));
        assert!(matches!(
            lookup.grid_need_list(),
            NeedListAvailability::Unavailable { .. }
        ));
    }

    #[test]
    fn hash_placeholder_is_unknown_and_never_scored_needed() {
        let lookup = CachedStationLookup::new();
        assert_eq!(lookup.needed_dxcc_status("<...>"), NeedStatus::Unknown);
        assert!(!lookup.is_needed_dxcc("<...>"));
        lookup.update_needed_dxcc(HashSet::from(["<".into()]));
        assert_eq!(lookup.needed_dxcc_status("<...>"), NeedStatus::Unknown);
        assert!(!lookup.is_needed_dxcc("<...>"));
    }

    #[test]
    fn test_empty_needed_no_exclusions_keeps_all_needed_default() {
        // Phase-5 hardening #2: scoring guess while the cqdx list is
        // unknown. With neither cqdx data nor exclusions, the scorer keeps
        // the historical "everything is needed" guess, but the verdict
        // reported to display surfaces is "unknown" (PAN-93).
        let lookup = CachedStationLookup::new();
        for call in ["K1ABC", "JA1XYZ", "3Y/B1234"] {
            assert!(lookup.is_needed_dxcc(call));
            assert_eq!(lookup.needed_dxcc_status(call), NeedStatus::Unknown);
        }
    }

    #[test]
    fn test_unknown_needed_guess_is_all_except_home_entity() {
        // PAN-347: scoring guess while neither the cqdx list nor the local
        // logbook has answered: every entity except the home entity is
        // needed; the reported verdict stays "unknown" (PAN-93).
        let lookup = CachedStationLookup::new();
        lookup.set_home_entity(Some("United States"));

        // US calls — NOT needed
        assert!(!lookup.is_needed_dxcc("K5ARH"));
        assert!(!lookup.is_needed_dxcc("W1ABC"));
        assert!(!lookup.is_needed_dxcc("N9ZZ"));
        assert!(!lookup.is_needed_dxcc("AA1XX"));

        // Non-US calls — needed
        assert!(lookup.is_needed_dxcc("JA1ABC"));
        assert!(lookup.is_needed_dxcc("DL5XYZ"));
        assert!(lookup.is_needed_dxcc("VK2DEF"));
        assert!(lookup.is_needed_dxcc("3Y/B1234"));
        for call in ["K5ARH", "JA1ABC", "DL5XYZ"] {
            assert_eq!(lookup.needed_dxcc_status(call), NeedStatus::Unknown);
        }
    }

    #[test]
    fn test_cqdx_needed_set_wins_over_exclusions() {
        // When cqdx populates needed_dxcc, the exclusion fallback is
        // bypassed entirely — needed-set semantics rule.
        let lookup = CachedStationLookup::new();
        // A home entity (would normally apply to the guess)
        lookup.set_home_entity(Some("United States"));
        // But cqdx says only Bouvet is needed
        let mut needed = HashSet::new();
        needed.insert("3Y/B".to_string());
        lookup.update_needed_dxcc(needed);

        // K5ARH should NOT be needed (not in cqdx-needed set)
        assert!(!lookup.is_needed_dxcc("K5ARH"));
        // JA1ABC — also not in cqdx-needed
        assert!(!lookup.is_needed_dxcc("JA1ABC"));
        // 3Y/B1234 — is in cqdx-needed
        assert!(lookup.is_needed_dxcc("3Y/B1234"));
    }

    /// PAN-16: the literal unresolved-hash placeholder `"<...>"` must never
    /// be scored as "needed", in any of `is_needed_dxcc`'s three branches
    /// (default "everything needed", exclusion fallback, cqdx-populated
    /// needed set).
    #[test]
    fn test_unresolved_placeholder_never_needed() {
        // Branch 1: no needed set, no exclusions — historical default
        // would otherwise treat this as "everything needed".
        let lookup = CachedStationLookup::new();
        assert!(!lookup.is_needed_dxcc("<...>"));

        // Branch 2: no needed set, home entity set.
        lookup.set_home_entity(Some("United States"));
        assert!(!lookup.is_needed_dxcc("<...>"));

        // Branch 3: cqdx-populated needed set (prefix-match branch).
        let mut needed = HashSet::new();
        needed.insert("3Y/B".to_string());
        lookup.update_needed_dxcc(needed);
        assert!(!lookup.is_needed_dxcc("<...>"));

        // A resolved hash form (real callsign in brackets) is out of scope
        // for this ticket and must still resolve/score normally — it never
        // equals the "<...>" literal.
        let lookup2 = CachedStationLookup::new();
        assert!(lookup2.is_needed_dxcc("<K1ABC>"));
    }

    /// PAN-16 round-2 (Codex P2): the guard must reject only the literal
    /// `"<...>"` placeholder, not "any callsign whose prefix isn't in the
    /// bundled offline BigCTY table." cqdx's `needed_dxcc` set can name a
    /// prefix that's newer than, or otherwise missing from, that static
    /// table — a real, valid, cqdx-confirmed-needed callsign with such a
    /// prefix must still score as needed via the branches below, not be
    /// silently zeroed just because the local table has a gap. "QQ" is used
    /// here as a stand-in for a locally-unresolvable-but-valid-shaped
    /// prefix (confirmed absent from the offline `pancetta_core::dxcc` table
    /// and not covered by its K/W/N/AA-AL US safety net).
    #[test]
    fn test_locally_unresolvable_prefix_not_wrongly_zeroed() {
        assert_eq!(pancetta_core::dxcc::entity_for_callsign("QQ9XYZ"), None);

        // Historical default (no needed set, no exclusions): still needed.
        let lookup = CachedStationLookup::new();
        assert!(lookup.is_needed_dxcc("QQ9XYZ"));

        // cqdx explicitly names this prefix as needed — must not be
        // overridden by a blanket local-table-resolvability guard.
        let mut needed = HashSet::new();
        needed.insert("QQ".to_string());
        lookup.update_needed_dxcc(needed);
        assert!(lookup.is_needed_dxcc("QQ9XYZ"));
    }

    #[test]
    fn test_atno_empty_set_is_inert() {
        // No ATNO data loaded: is_atno is false for everything.
        let lookup = CachedStationLookup::new();
        assert!(!lookup.is_atno("3Y/B1234"));
        assert!(!lookup.is_atno("K5ARH"));
    }

    #[test]
    fn test_atno_prefix_match() {
        let lookup = CachedStationLookup::new();
        let mut atno = HashSet::new();
        atno.insert("3Y/B".to_string());
        atno.insert("ja".to_string()); // lower-case is normalized on update
        lookup.update_needed_atno(atno);

        assert!(lookup.is_atno("3Y/B1234"));
        assert!(lookup.is_atno("JA1ABC")); // case-insensitive prefix
        assert!(!lookup.is_atno("DL5XYZ")); // not in ATNO set
    }

    #[test]
    fn test_atno_bonus_lifts_score_over_plain_needed() {
        // An ATNO entity should score strictly higher than the same entity
        // when only band-needed (not ATNO), via the atno_bonus weight.
        // PAN-54 round 5 (Codex #3910841457): the fixture used to be
        // "3Y/B1234" — a digit-terminal home call with no suffix letter —
        // which `is_plausible_callsign` (added by this PR) now correctly
        // rejects as implausible, zeroing both totals and breaking this
        // assertion. "3Y/B1ABC" still matches the "3Y/B" needed-prefix
        // lookup below while passing the new shape gate.
        use pancetta_qso::priority::{PriorityScorer, PriorityWeights};
        use pancetta_qso::DxEvaluator;

        let mut needed = HashSet::new();
        needed.insert("3Y/B".to_string());

        // Lookup A: needed but NOT atno.
        let plain = CachedStationLookup::new();
        plain.update_needed_dxcc(needed.clone());
        let plain_scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(plain));
        let plain_score = plain_scorer.evaluate_cq("3Y/B1ABC", None, -10, 14_074_000.0);

        // Lookup B: needed AND atno.
        let atno_lookup = CachedStationLookup::new();
        atno_lookup.update_needed_dxcc(needed.clone());
        atno_lookup.update_needed_atno(needed);
        let atno_scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(atno_lookup));
        let atno_score = atno_scorer.evaluate_cq("3Y/B1ABC", None, -10, 14_074_000.0);

        assert!(
            atno_score > plain_score,
            "ATNO ({atno_score}) should outscore plain-needed ({plain_score})"
        );
    }

    #[test]
    fn test_needed_grids_empty_set_is_inert() {
        // Scoring guess while the cqdx grid list is unknown: nothing is
        // "needed", so the needed_grid weight doesn't inflate every score.
        // The reported verdict is "unknown", not "not needed" (PAN-93).
        let lookup = CachedStationLookup::new();
        assert!(!lookup.is_needed_grid("FN42"));
        assert!(!lookup.is_needed_grid("JD15"));
        assert_eq!(lookup.needed_grid_status("FN42"), NeedStatus::Unknown);
    }

    #[test]
    fn test_update_needed_grids_marks_needed() {
        let lookup = CachedStationLookup::new();
        let mut needed = HashSet::new();
        needed.insert("JD15".to_string());
        needed.insert("FN42".to_string());
        lookup.update_needed_grids(needed);

        // In the set — needed.
        assert!(lookup.is_needed_grid("JD15"));
        assert!(lookup.is_needed_grid("FN42"));
        // Case-insensitive match.
        assert!(lookup.is_needed_grid("jd15"));
        // 6-char locator normalizes to its 4-char field before comparison.
        assert!(lookup.is_needed_grid("JD15kl"));
        // Not in the set — not needed.
        assert!(!lookup.is_needed_grid("PM95"));
        // Too short to be a valid field — not needed.
        assert!(!lookup.is_needed_grid("JD"));
    }

    #[test]
    fn test_priority_score_non_us_high_with_default_exclusions() {
        // Phase-5 hardening #2 success criteria: a Japanese CQ
        // (DXCC 339) must score ≥ 0.35 when default exclusions are
        // US-only, and a US CQ must score < 0.30 (won't respond).
        use pancetta_qso::priority::{PriorityScorer, PriorityWeights};
        use pancetta_qso::DxEvaluator;
        let lookup = CachedStationLookup::new();
        lookup.set_home_entity(Some("United States"));

        let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));
        let ja_score = scorer.evaluate_cq("JA1ABC", Some("PM95"), -10, 14_074_000.0);
        let us_score = scorer.evaluate_cq("K5ARH", Some("EM12"), -10, 14_074_000.0);

        assert!(
            ja_score >= 0.35,
            "non-home (JA) call should score >= 0.35; got {}",
            ja_score
        );
        assert!(
            us_score < 0.30,
            "home (US) call should score < 0.30; got {}",
            us_score
        );
    }

    // --- PAN-58: a resolved i3=4 hash-render ("<N7RLK>") must score
    // identically to the plain callsign it represents, everywhere a
    // decode's identity drives DXCC/needed/atno/worked/notable lookups.
    // Without this, the leading '<' defeats every prefix/exact match below
    // and each of these signals silently fails toward "unknown station,
    // never excluded, never worked, never a duplicate" — which happens to
    // rank an already-worked home-country station artificially HIGH
    // (needed_dxcc fails open) while also blanking its Entity column
    // (entity_for_callsign can't parse a DXCC prefix starting with '<').

    #[test]
    fn is_needed_dxcc_resolves_hash_render_before_prefix_match() {
        let lookup = CachedStationLookup::new();
        lookup.set_home_entity(Some("United States"));

        // N7RLK is a home (excluded) US call — must read as "not needed"
        // whether decoded plain or via a resolved hash-render.
        assert!(!lookup.is_needed_dxcc("N7RLK"));
        assert!(
            !lookup.is_needed_dxcc("<N7RLK>"),
            "resolved hash-render of an excluded prefix must not read as needed"
        );
        // A genuinely-needed DX call must still be needed in both forms.
        assert!(lookup.is_needed_dxcc("JA1ABC"));
        assert!(lookup.is_needed_dxcc("<JA1ABC>"));
    }

    #[test]
    fn is_needed_dxcc_unresolved_hash_placeholder_is_never_needed() {
        let lookup = CachedStationLookup::new();
        // Even with an empty exclusion/needed set (the "everything is
        // needed" fallback), the unresolved hash-miss placeholder carries
        // no identity and must never be treated as a callable, needed
        // station.
        assert!(!lookup.is_needed_dxcc("<...>"));
    }

    #[test]
    fn is_atno_resolves_hash_render_before_prefix_match() {
        let lookup = CachedStationLookup::new();
        let mut atno = HashSet::new();
        atno.insert("JA".to_string());
        lookup.update_needed_atno(atno);

        assert!(lookup.is_atno("JA1ABC"));
        assert!(
            lookup.is_atno("<JA1ABC>"),
            "resolved hash-render of an ATNO prefix must still read as ATNO"
        );
    }

    #[test]
    fn is_duplicate_resolves_hash_render_before_worked_lookup() {
        let lookup = CachedStationLookup::new();
        lookup.record_worked("N7RLK", "20m");

        assert!(lookup.is_duplicate("N7RLK", 14_074_000.0));
        assert!(
            lookup.is_duplicate("<N7RLK>", 14_074_000.0),
            "an already-worked station heard again via a resolved hash-render \
             must still count as a duplicate (worked-before)"
        );
    }

    /// PAN-58 round 1 (Codex P1): the write side must canonicalize too — a
    /// QSO completed while the partner's identity was still a resolved
    /// hash-render must be findable by a later PLAIN-form lookup (and vice
    /// versa), not just the reverse direction the first test above covers.
    #[test]
    fn record_worked_canonicalizes_hash_render_key_for_later_plain_lookup() {
        let lookup = CachedStationLookup::new();
        lookup.record_worked("<N7RLK>", "20m");

        assert!(
            lookup.is_duplicate("N7RLK", 14_074_000.0),
            "a QSO logged under a resolved hash-render identity must still \
             be found by a later plain-callsign duplicate check"
        );
        assert!(lookup.is_duplicate("<N7RLK>", 14_074_000.0));
    }

    #[test]
    fn record_worked_never_stores_unresolved_hash_placeholder() {
        let lookup = CachedStationLookup::new();
        lookup.record_worked("<...>", "20m");
        assert!(!lookup.is_duplicate("<...>", 14_074_000.0));
    }

    #[test]
    fn record_failure_and_rarity_resolve_hash_render_symmetrically() {
        let lookup = CachedStationLookup::new();
        lookup.record_failure("<N7RLK>");
        assert!(
            lookup.is_recent_failure("N7RLK"),
            "a recorded failure under a resolved hash-render identity must \
             still be found by a later plain-callsign lookup"
        );

        let mut scores = HashMap::new();
        scores.insert("N7RLK".to_string(), 0.9);
        lookup.update_rarity_scores(scores);
        assert_eq!(CachedStationLookup::rarity(&lookup, "<N7RLK>"), 0.9);
    }

    #[test]
    fn is_notable_resolves_hash_render_before_exact_match() {
        let lookup = CachedStationLookup::new();
        let mut notable = HashSet::new();
        notable.insert("K1SE".to_string());
        lookup.update_notable_callsigns(notable);

        assert!(lookup.is_notable("K1SE"));
        assert!(lookup.is_notable("<K1SE>"));
    }

    // --- PAN-85: per-callsign US state store ---

    #[test]
    fn state_store_normalizes_and_is_case_insensitive_on_callsign() {
        let l = CachedStationLookup::new();
        l.record_state("w5abc", "ar");
        assert_eq!(l.state_for("W5ABC").as_deref(), Some("AR"));
        assert_eq!(l.state_for("w5abc").as_deref(), Some("AR"));
        assert_eq!(l.state_for("W1XYZ"), None);
    }

    #[test]
    fn state_store_rejects_unvalidatable_values() {
        let l = CachedStationLookup::new();
        l.record_state("W5ABC", "Arkansas");
        l.record_state("W1XYZ", "");
        assert_eq!(l.state_for("W5ABC"), None);
        assert_eq!(l.state_for("W1XYZ"), None);
    }

    #[test]
    fn seed_states_from_list_matches_record_state_and_last_wins() {
        let seeded = CachedStationLookup::new();
        seeded.seed_states_from_list(vec![
            ("W5ABC".into(), "TX".into()),
            ("W5ABC".into(), "AR".into()), // later log row wins
            ("BAD".into(), "zz".into()),   // dropped by validation
        ]);
        assert_eq!(seeded.state_for("W5ABC").as_deref(), Some("AR"));
        assert_eq!(seeded.state_for("BAD"), None);
    }

    #[test]
    fn state_store_resolves_hash_render_before_exact_match() {
        // A station worked as "W5ABC" and later decoded hash-rendered as
        // "<W5ABC>" must still report its state — the same contract
        // `is_notable_resolves_hash_render_before_exact_match` pins for the
        // sibling accessors.
        let l = CachedStationLookup::new();
        l.record_state("W5ABC", "AR");
        assert_eq!(l.state_for("<W5ABC>").as_deref(), Some("AR"));

        // ...and the write side normalizes identically, so a state first
        // learned from a hash-rendered decode is found under the plain call.
        let l2 = CachedStationLookup::new();
        l2.record_state("<W5ABC>", "AR");
        assert_eq!(l2.state_for("W5ABC").as_deref(), Some("AR"));
        assert_eq!(l2.state_for("<W5ABC>").as_deref(), Some("AR"));
    }

    #[test]
    fn state_store_ignores_the_unresolved_hash_placeholder() {
        let l = CachedStationLookup::new();
        l.record_state("<...>", "AR");
        assert_eq!(l.state_for("<...>"), None);
        assert!(l.station_states.read().is_empty());
    }
}

#[cfg(test)]
mod pan260 {
    use super::*;
    #[tokio::test]
    async fn dated_all_band_history_preserves_latest_and_survives_failed_refresh() {
        let l = CachedStationLookup::new();
        let now = Utc::now();
        let old = now - chrono::Duration::days(90);
        l.seed_dated_worked(vec![
            ("40m".into(), "ja1xyz".into(), old),
            ("20m".into(), "k1abc".into(), now),
        ]);
        assert!(l.is_duplicate("JA1XYZ", 7_074_000.0));
        assert!(!l.is_duplicate("JA1XYZ", 14_074_000.0));
        assert!(l.is_duplicate("K1ABC", 14_074_000.0));
        l.seed_dated_worked(vec![("20M".into(), "K1ABC".into(), old)]);
        assert_eq!(l.worked_on_band.read()["20M"]["K1ABC"], now);
        l.record_worked_at("<K1ABC>", "20m", old);
        assert_eq!(l.worked_on_band.read()["20M"]["K1ABC"], now);
        let manager = pancetta_qso::QsoManager::new(pancetta_qso::QsoManagerConfig {
            our_callsign: "W1ABC".into(),
            ..Default::default()
        });
        let id = manager
            .respond_to_cq_manual("K1ABC".into(), 14_074_000.0, None)
            .await
            .unwrap();
        let mut metadata = manager.get_qso(id).await.unwrap().metadata;
        metadata.start_time = old;
        l.record_completed_contact(&metadata);
        assert_eq!(l.worked_on_band.read()["20M"]["K1ABC"], now);
        metadata.start_time = now + chrono::Duration::seconds(1);
        l.record_completed_contact(&metadata);
        assert_eq!(l.worked_on_band.read()["20M"]["K1ABC"], metadata.start_time);
        let db = pancetta_qso::async_database::QsoDatabase::new_in_memory()
            .await
            .unwrap();
        db.close().await;
        assert!(l.seed_worked_history(&db).await.is_err());
        assert!(l.is_duplicate("JA1XYZ", 7_074_000.0));
    }

    // ----- PAN-347: needed, ATNO, band-slot, mode and standing from the
    // local logbook. ---------------------------------------------------------

    use super::test_support::{adif, seeded_lookup};
    use pancetta_qso::priority::{PriorityScorer, PriorityTier, PriorityWeights};
    use pancetta_tui::dx_need::{DxNeedLookup, EntityStanding};

    const DE: &str = "Fed. Rep. of Germany";

    /// The probe's 400-record log: 20 entities x 20 QSOs on 20 m FT8, one
    /// JA record LoTW-confirmed.
    fn four_hundred_qso_log() -> String {
        let prefixes = [
            "JA1", "DL1", "G4", "F5", "I1", "EA1", "VE3", "VK2", "PY2", "LU1", "ZS1", "UA3", "SM5",
            "OH2", "OZ1", "LA1", "ON4", "PA3", "HB9", "SP5",
        ];
        let mut recs = Vec::new();
        for p in prefixes {
            for j in 0..20u8 {
                let call = format!("{p}A{}", (b'A' + j) as char);
                let extra = if p == "JA1" && j == 0 {
                    "<LOTW_QSL_RCVD:1>Y"
                } else {
                    ""
                };
                recs.push((call, "FT8".to_string(), 14.074, extra.to_string()));
            }
        }
        let refs: Vec<(&str, &str, f64, &str)> = recs
            .iter()
            .map(|(c, m, f, e)| (c.as_str(), m.as_str(), *f, e.as_str()))
            .collect();
        adif(&refs)
    }

    fn scorer(l: &CachedStationLookup) -> PriorityScorer {
        PriorityScorer::new(PriorityWeights::default(), Box::new(l.clone()))
    }

    #[tokio::test]
    async fn local_log_answers_needed_and_atno_when_cqdx_has_not() {
        let (_tmp, l) = seeded_lookup(&four_hundred_qso_log(), "K5ARH").await;
        l.set_need_lists_loading();
        l.mark_need_lists_unavailable("connection refused", true);
        assert!(l.local_log_available());
        assert_eq!(
            l.dxcc_need_verdict("ZL1ABC"),
            (NeedStatus::Needed, NeedSource::LocalLogbook)
        );
        assert!(l.is_atno("ZL1ABC"));
        assert_eq!(l.needed_atno_shown("ZL1ABC"), (true, true));
        // The wire stays cqdx-only.
        assert_eq!(l.display_needed_atno("ZL1ABC"), (false, false));
        let zl = scorer(&l).score_tiered_explained("ZL1ABC", None, -10, 14_074_000.0);
        assert_eq!(zl.score.tier, PriorityTier::Atno);
        assert_eq!(zl.factors.need_source, NeedSource::LocalLogbook);
        assert_eq!(
            l.dxcc_need_verdict("JA1XYZ"),
            (NeedStatus::NotNeeded, NeedSource::LocalLogbook)
        );
        let ja = scorer(&l).score_tiered_explained("JA1XYZ", None, -10, 14_074_000.0);
        assert_eq!(ja.score.tier, PriorityTier::Standard);
        assert!(!ja.factors.needed_dxcc && !ja.factors.atno);
    }

    #[tokio::test]
    async fn local_log_answers_while_cqdx_is_not_configured_and_while_loading() {
        let (_tmp, l) = seeded_lookup(&four_hundred_qso_log(), "K5ARH").await;
        assert_eq!(l.dxcc_need_list(), NeedListAvailability::NotConfigured);
        assert_eq!(
            l.dxcc_need_verdict("ZL1ABC"),
            (NeedStatus::Needed, NeedSource::LocalLogbook)
        );
        l.set_need_lists_loading();
        assert_eq!(
            l.dxcc_need_verdict("ZL1ABC"),
            (NeedStatus::Needed, NeedSource::LocalLogbook)
        );
        assert_eq!(l.needed_atno_shown("ZL1ABC"), (true, true));
    }

    #[tokio::test]
    async fn loaded_cqdx_list_wins_over_the_local_log() {
        let (_tmp, l) = seeded_lookup(&four_hundred_qso_log(), "K5ARH").await;
        l.update_needed_dxcc(HashSet::new());
        assert_eq!(
            l.dxcc_need_verdict("ZL1ABC"),
            (NeedStatus::NotNeeded, NeedSource::Cqdx)
        );
        assert!(!l.is_atno("ZL1ABC"));
        assert_eq!(l.needed_atno_shown("ZL1ABC"), (false, false));
    }

    #[tokio::test]
    async fn band_slot_is_entity_scoped_and_ignores_letter_prefixes() {
        let (_tmp, l) = seeded_lookup(&adif(&[("DL1ABC", "FT8", 14.074, "")]), "K5ARH").await;
        for call in ["DL1XYZ", "DA1XYZ"] {
            assert!(l.is_dxcc_needed_on_band(call, 7_047_500.0), "{call}");
            assert!(!l.is_dxcc_needed_on_band(call, 14_080_000.0), "{call}");
        }
        let at40 = scorer(&l).score_tiered_explained("DL1XYZ", None, -12, 7_047_500.0);
        assert_eq!(at40.score.tier, PriorityTier::PerBandDxccNew);
        assert!(at40.factors.dxcc_new_on_band && !at40.factors.atno);
    }

    #[tokio::test]
    async fn new_mode_is_per_entity_across_bands() {
        let (_tmp, l) = seeded_lookup(&adif(&[("DL1ABC", "FT8", 14.074, "")]), "K5ARH").await;
        let need = |mode| l.lookup_need("DL1XYZ", Some(DE), None, None, 7_047_500.0, mode);
        assert_eq!(need(Some("FT4")).new_mode, Some(true));
        assert_eq!(need(Some("FT8")).new_mode, Some(false));
        assert_eq!(need(None).new_mode, None);
    }

    #[tokio::test]
    async fn standing_distinguishes_worked_from_confirmed() {
        let (_tmp, l) = seeded_lookup(
            &adif(&[
                ("DL1ABC", "FT8", 14.074, ""),
                ("JA1ABC", "FT8", 14.074, "<LOTW_QSL_RCVD:1>Y"),
            ]),
            "K5ARH",
        )
        .await;
        let standing = |call, entity| {
            l.lookup_need(call, Some(entity), None, None, 14_074_000.0, None)
                .entity_standing
        };
        assert_eq!(
            standing("DL1XYZ", DE),
            Some(EntityStanding::WorkedUnconfirmed)
        );
        assert_eq!(standing("JA1XYZ", "Japan"), Some(EntityStanding::Confirmed));
        assert_eq!(
            standing("ZL1ABC", "New Zealand"),
            Some(EntityStanding::NotWorked)
        );
    }

    #[tokio::test]
    async fn home_entity_is_never_needed_and_dependencies_are_not_home() {
        let (_tmp, l) = seeded_lookup(&adif(&[]), "W5AU").await;
        assert_eq!(
            l.dxcc_need_verdict("K1ABC"),
            (NeedStatus::NotNeeded, NeedSource::LocalLogbook)
        );
        assert!(!l.is_dxcc_needed_on_band("K1ABC", 14_074_000.0));
        let home = pancetta_core::dxcc::entity_for_callsign("K1ABC");
        assert_eq!(
            l.lookup_need("K1ABC", home, None, None, 14_074_000.0, Some("FT8"))
                .new_mode,
            None
        );
        for call in ["KH6ABC", "KL7ABC", "KP4ABC", "AL7ABC"] {
            assert_eq!(
                l.dxcc_need_verdict(call),
                (NeedStatus::Needed, NeedSource::LocalLogbook),
                "{call}"
            );
            assert!(l.is_dxcc_needed_on_band(call, 14_074_000.0), "{call}");
        }
    }

    #[tokio::test]
    async fn logged_prefixes_no_longer_hide_other_entities() {
        let (_tmp, l) = seeded_lookup(
            &adif(&[("EA1ABC", "FT8", 14.074, ""), ("VK2ABC", "FT8", 14.074, "")]),
            "K5ARH",
        )
        .await;
        for call in ["EA8ABC", "VK9ABC"] {
            assert!(l.is_needed_dxcc(call), "{call}");
        }
        for call in ["EA4XYZ", "VK4XYZ"] {
            assert!(!l.is_needed_dxcc(call), "{call}");
        }
    }

    #[test]
    fn unseeded_lookup_keeps_the_guess_and_shows_nothing() {
        let l = CachedStationLookup::new();
        l.set_home_entity(Some("United States"));
        assert_eq!(
            l.dxcc_need_verdict("ZL1ABC"),
            (NeedStatus::Unknown, NeedSource::Guess)
        );
        assert!(l.is_needed_dxcc("ZL1ABC"));
        assert!(!l.is_needed_dxcc("K1ABC"));
        assert_eq!(l.needed_atno_shown("ZL1ABC"), (false, false));
        let need = l.lookup_need(
            "ZL1ABC",
            Some("New Zealand"),
            None,
            None,
            14_074_000.0,
            Some("FT8"),
        );
        assert_eq!(need.entity_standing, None);
        assert_eq!(need.dxcc_needed, None);
        assert_eq!(need.dxcc_source, None);
        assert_eq!(need.new_mode, None);
    }

    #[tokio::test]
    async fn failed_seed_leaves_the_local_log_unavailable() {
        let l = CachedStationLookup::new();
        let db = pancetta_qso::async_database::QsoDatabase::new_in_memory()
            .await
            .unwrap();
        db.close().await;
        assert!(l.seed_worked_history(&db).await.is_err());
        assert!(!l.local_log_available());
    }

    #[tokio::test]
    async fn live_completion_updates_mode_and_standing() {
        let (_tmp, l) = seeded_lookup(&four_hundred_qso_log(), "K5ARH").await;
        let manager = pancetta_qso::QsoManager::new(pancetta_qso::QsoManagerConfig {
            our_callsign: "K5ARH".into(),
            ..Default::default()
        });
        let id = manager
            .respond_to_cq_manual("ZL1ABC".into(), 14_074_000.0, None)
            .await
            .unwrap();
        let mut metadata = manager.get_qso(id).await.unwrap().metadata;
        metadata.mode = "FT4".into();
        l.record_completed_contact(&metadata);
        assert_eq!(
            l.dxcc_need_verdict("ZL1ABC"),
            (NeedStatus::NotNeeded, NeedSource::LocalLogbook)
        );
        let need = l.lookup_need(
            "ZL2XYZ",
            Some("New Zealand"),
            None,
            None,
            14_074_000.0,
            Some("FT4"),
        );
        assert_eq!(
            need.entity_standing,
            Some(EntityStanding::WorkedUnconfirmed)
        );
        assert_eq!(need.new_mode, Some(false));
    }

    #[tokio::test]
    async fn reseeding_after_restart_keeps_availability_and_is_additive() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        std::fs::write(&path, adif(&[("DL1ABC", "FT8", 14.074, "")])).unwrap();
        let db = pancetta_qso::async_database::QsoDatabase::replay_from_adif(":memory:", &path)
            .await
            .unwrap();
        let l = CachedStationLookup::new();
        l.set_home_entity(Some("United States"));
        l.seed_worked_history(&db).await.unwrap();
        l.seed_worked_history(&db).await.unwrap();
        assert!(l.local_log_available());
        assert_eq!(
            l.dxcc_need_verdict("DL1XYZ"),
            (NeedStatus::NotNeeded, NeedSource::LocalLogbook)
        );
        assert_eq!(l.worked_on_band.read()["20M"].len(), 1);
        assert_eq!(
            l.dxcc_need_verdict("ZL1ABC"),
            (NeedStatus::Needed, NeedSource::LocalLogbook)
        );
    }

    #[tokio::test]
    async fn seeded_band_pairs_also_mark_the_local_log() {
        let (_tmp, l) = seeded_lookup(&adif(&[]), "K5ARH").await;
        l.seed_worked_dxcc_from_list(vec![("20m".into(), "JA1ABC".into())]);
        assert_eq!(
            l.dxcc_need_verdict("JA1XYZ"),
            (NeedStatus::NotNeeded, NeedSource::LocalLogbook)
        );
    }

    #[tokio::test]
    async fn hash_placeholder_is_unknown_and_never_needed() {
        let (_tmp, l) = seeded_lookup(&adif(&[]), "K5ARH").await;
        assert_eq!(
            l.dxcc_need_verdict("<...>"),
            (NeedStatus::Unknown, NeedSource::Guess)
        );
        assert!(!l.is_needed_dxcc("<...>"));
        assert_eq!(
            l.dxcc_need_verdict("<ZL1ABC>"),
            l.dxcc_need_verdict("ZL1ABC")
        );
        assert_eq!(l.needed_atno_shown("<ZL1ABC>"), (true, true));
    }
}

/// PAN-347: shared test helpers that build a seeded lookup from ADIF text.
#[cfg(test)]
pub(crate) mod test_support {
    use super::CachedStationLookup;

    /// ADIF text for `(call, mode, freq_mhz, extra_fields)` records, one
    /// minute apart so no two records collide.
    pub(crate) fn adif(records: &[(&str, &str, f64, &str)]) -> String {
        let mut out = String::from("PAN-347 test log\n<ADIF_VER:5>3.1.4 <EOH>\n");
        for (i, (call, mode, freq, extra)) in records.iter().enumerate() {
            let freq = format!("{freq:.6}");
            let time = format!("{:02}{:02}00", (i / 60) % 24, i % 60);
            let date = format!(
                "2025{:02}{:02}",
                1 + (i / 1440) % 12,
                1 + (i / 60 / 24) % 28
            );
            out.push_str(&format!(
                "<CALL:{}>{} <QSO_DATE:8>{} <TIME_ON:6>{} <MODE:{}>{} <FREQ:{}>{} {} <EOR>\n",
                call.len(),
                call,
                date,
                time,
                mode.len(),
                mode,
                freq.len(),
                freq,
                extra
            ));
        }
        out
    }

    /// Replay `adif` into `:memory:`, seed the lookup from it, and set the
    /// operator's home entity. The temp dir must outlive the test.
    pub(crate) async fn seeded_lookup(
        adif: &str,
        operator: &str,
    ) -> (tempfile::TempDir, CachedStationLookup) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        std::fs::write(&path, adif).unwrap();
        let db = pancetta_qso::async_database::QsoDatabase::replay_from_adif(":memory:", &path)
            .await
            .unwrap();
        let l = CachedStationLookup::new();
        l.set_home_entity(pancetta_core::dxcc::entity_for_callsign(operator));
        l.seed_worked_history(&db).await.unwrap();
        (tmp, l)
    }
}
