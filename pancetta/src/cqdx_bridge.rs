//! Coordinator-level wiring for cqdx.io integration.
//!
//! Handles startup (fetch entities + needed), periodic live spot polling,
//! and fire-and-forget spot/QSO reporting.

use crate::coordinator::upload_outbox::clip;
use crate::priority_evaluator::{CachedStationLookup, NeedListAvailability};
use pancetta_cqdx::{
    frequency_to_band, rank_to_rarity, CqdxCache, CqdxClient, CqdxError, NeededEntity, QsoRecord,
    SpotReport,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tokio::time::Duration;
use tracing::{debug, info, warn};

/// Normalize a Maidenhead locator to its 4-character field, uppercased.
///
/// cqdx.io may return needed grids as 4-char (`JD15`) or 6-char (`JD15kl`)
/// locators; the autonomous scorer keys on the 4-char field. Anything shorter
/// than 4 characters yields an empty string (caller filters these out).
fn normalize_grid_field(grid: &str) -> String {
    let trimmed = grid.trim();
    if trimmed.len() < 4 {
        return String::new();
    }
    trimmed[..4].to_uppercase()
}

/// Push a `needed` entity list into the [`CachedStationLookup`], splitting
/// out the ATNO subset. `needed_dxcc` gets every prefix; `needed_atno` gets
/// only those flagged `atno` by cqdx.io. `band` is the per-band scope cqdx
/// answered for (`None` = all bands); the list is marked `Loaded` with it,
/// so an empty answer reads "nothing needed", not "unknown" (PAN-93).
/// Returns `(total, atno)` counts.
fn apply_needed(
    cached_lookup: &CachedStationLookup,
    needed: &[NeededEntity],
    band: Option<&str>,
) -> (usize, usize) {
    let prefixes: std::collections::HashSet<String> =
        needed.iter().map(|n| n.prefix.to_uppercase()).collect();
    let atno: std::collections::HashSet<String> = needed
        .iter()
        .filter(|n| n.atno)
        .map(|n| n.prefix.to_uppercase())
        .collect();
    let (total, atno_count) = (prefixes.len(), atno.len());
    cached_lookup.update_needed_dxcc_for_band(
        prefixes,
        band.map(str::to_owned),
        chrono::Utc::now(),
    );
    cached_lookup.update_needed_atno(atno);
    (total, atno_count)
}

/// PAN-93: how long a needed list that has not loaded waits between retries.
const NEED_DATA_RETRY: Duration = Duration::from_secs(60);

/// PAN-93: false for errors only a config fix and restart can cure (a bad or
/// rejected token, a bad base URL, a broken HTTP stack). Retrying those would
/// only repeat the failure.
fn retry_can_help(e: &CqdxError) -> bool {
    !matches!(
        e,
        CqdxError::Unauthorized
            | CqdxError::NotConfigured
            | CqdxError::InvalidToken(_)
            | CqdxError::InvalidBaseUrl(_)
            | CqdxError::HttpInit(_)
    )
}

/// PAN-93: errors that end a load pass early. A transport failure means the
/// next request would wait out the same timeout, so startup stays bounded by
/// one client timeout; a permanent error fails every endpoint alike. A
/// `Server`/`Json` error on one endpoint does not stop the others.
fn stops_sequence(e: &CqdxError) -> bool {
    matches!(e, CqdxError::Http(_)) || !retry_can_help(e)
}

/// PAN-93: true when a pending load is due: never attempted, or at least
/// [`NEED_DATA_RETRY`] since the last attempt.
fn need_retry_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|t| now.saturating_duration_since(t) >= NEED_DATA_RETRY)
}

/// PAN-93: which startup-dependent cqdx loads have not succeeded yet.
#[derive(Debug, Clone, Copy)]
struct PendingLoads {
    entities: bool,
    dxcc: bool,
    grids: bool,
    /// A permanent error (see [`retry_can_help`]) was seen: stop retrying.
    blocked: bool,
    /// Failed needed-entity attempts before it first loaded, for the
    /// recovery log line.
    dxcc_failures: u32,
    /// When the last load pass started; paces retries.
    last_attempt: Option<Instant>,
}

impl Default for PendingLoads {
    fn default() -> Self {
        Self {
            entities: true,
            dxcc: true,
            grids: true,
            blocked: false,
            dxcc_failures: 0,
            last_attempt: None,
        }
    }
}

impl PendingLoads {
    /// Anything left that a retry could fetch. `fetch_dxcc` is false when the
    /// poller's per-band block owns the needed-entity fetch this tick.
    fn retryable(&self, fetch_dxcc: bool) -> bool {
        !self.blocked && (self.entities || self.grids || (self.dxcc && fetch_dxcc))
    }
}

/// Manages the cqdx.io integration lifecycle.
pub struct CqdxBridge {
    client: CqdxClient,
    cache: Arc<RwLock<CqdxCache>>,
    cached_lookup: Arc<CachedStationLookup>,
    poll_interval: Duration,
    /// Current band, updated by coordinator when radio frequency changes.
    current_band: Arc<RwLock<Option<String>>>,
    /// The rig's current dial frequency (Hz), shared with the coordinator.
    /// When present, the spot poller derives the operating band from it and
    /// re-fetches per-band needed entities whenever the band changes — so
    /// `needed_dxcc` reflects band-fills for the band actually in use, while
    /// the all-time ATNO set stays global. `None` keeps the all-time
    /// (band-agnostic) needed set fetched at startup.
    operating_frequency_hz: Option<Arc<AtomicU64>>,
    /// PAN-93: loads still owed after a failed or partial start, retried by
    /// the spot poller. Never held across an await.
    pending: std::sync::Mutex<PendingLoads>,
}

impl CqdxBridge {
    /// Create a bridge from config. Returns None if cqdx.io is not configured.
    ///
    /// PAN-93: also records the needed lists' availability. Disabled leaves
    /// them `NotConfigured`; enabled with a missing/empty token or an
    /// unusable client marks them `Unavailable` without retry (only a config
    /// fix and restart help); a built client marks them `Loading`.
    pub fn from_config(
        config: &pancetta_config::network::CqdxConfig,
        cached_lookup: Arc<CachedStationLookup>,
    ) -> Option<Self> {
        if !config.enabled {
            info!("cqdx.io integration disabled");
            return None;
        }
        let token = config.token.as_deref().unwrap_or_default();
        if token.is_empty() {
            warn!("cqdx.io enabled but token is empty, skipping");
            cached_lookup.mark_need_lists_unavailable(
                "cqdx.io is enabled but [network.cqdx] token is empty",
                false,
            );
            return None;
        }
        let client = match CqdxClient::new(config.base_url.clone(), token.to_string()) {
            Ok(c) => c,
            Err(e) => {
                warn!("cqdx.io client creation failed: {}", e);
                cached_lookup.mark_need_lists_unavailable(&clip(&e.to_string()), false);
                return None;
            }
        };
        cached_lookup.set_need_lists_loading();
        Some(Self {
            client,
            cache: Arc::new(RwLock::new(CqdxCache::new())),
            cached_lookup,
            poll_interval: Duration::from_secs(config.poll_interval_secs),
            current_band: Arc::new(RwLock::new(None)),
            operating_frequency_hz: None,
            pending: std::sync::Mutex::new(PendingLoads::default()),
        })
    }

    /// Attach the coordinator's shared dial-frequency atomic so the spot
    /// poller can track the operating band and re-fetch per-band needs.
    pub fn with_operating_frequency(mut self, freq: Arc<AtomicU64>) -> Self {
        self.operating_frequency_hz = Some(freq);
        self
    }

    /// Fetch entities and needed data on startup. Populates cache and
    /// CachedStationLookup. At startup we may not know the operating band
    /// yet, so the needed set is the all-time/ATNO set (band=None); the spot
    /// poller re-fetches per-band fills once the dial frequency is known.
    ///
    /// PAN-93: an `Err` leaves the bridge usable. Whatever did not load is
    /// recorded on the lookup and retried by the spot poller.
    pub async fn startup(&self) -> pancetta_cqdx::Result<()> {
        self.load_pending(None, true).await
    }

    /// PAN-93: one retry pass over the pending loads (test entry point; the
    /// spot poller calls `load_pending` directly).
    #[cfg(test)]
    pub(crate) async fn retry_pending_loads(
        &self,
        band: Option<&str>,
    ) -> pancetta_cqdx::Result<()> {
        self.load_pending(band, true).await
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, PendingLoads> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// PAN-93: install a needed-entity answer: cache, lookup (with its band
    /// scope), and the recovery log line when earlier attempts failed.
    async fn install_needed(&self, needed: Vec<NeededEntity>, band: Option<&str>) {
        let (total, atno) = apply_needed(&self.cached_lookup, &needed, band);
        self.cache.write().await.load_needed(needed);
        info!(
            "cqdx.io needed ({}): {} DXCC prefix(es), {} flagged ATNO",
            band.unwrap_or("all bands"),
            total,
            atno
        );
        let failures = {
            let mut pending = self.pending();
            pending.dxcc = false;
            std::mem::take(&mut pending.dxcc_failures)
        };
        if failures > 0 {
            info!("cqdx.io needed-entity list loaded after {failures} failed attempt(s)");
        }
    }

    /// PAN-93: record a failed needed-entity fetch. Before the first load the
    /// list becomes `Unavailable`; after it, the last answer is kept.
    fn record_dxcc_failure(&self, e: &CqdxError) {
        let retrying = retry_can_help(e);
        self.cached_lookup
            .mark_dxcc_need_list_failed(&clip(&e.to_string()), retrying);
        let mut pending = self.pending();
        if pending.dxcc {
            pending.dxcc_failures = pending.dxcc_failures.saturating_add(1);
        }
        if !retrying {
            pending.blocked = true;
        }
    }

    /// PAN-93: try each pending load in order (entities, needed entities,
    /// needed grids) and record each outcome on the lookup. A transport or
    /// permanent error stops the pass and marks every list not yet tried as
    /// failed with that error. Returns the first entities or needed-entity
    /// error. `fetch_dxcc` is false when the poller's per-band block already
    /// tried the needed-entity list this tick. Does nothing once a permanent
    /// error has been seen.
    async fn load_pending(
        &self,
        band: Option<&str>,
        fetch_dxcc: bool,
    ) -> pancetta_cqdx::Result<()> {
        let todo = {
            let mut pending = self.pending();
            if pending.blocked {
                return Ok(());
            }
            pending.last_attempt = Some(Instant::now());
            *pending
        };
        let mut first_err: Option<CqdxError> = None;
        // (clipped error text, retrying) once the pass is stopped.
        let mut stopped: Option<(String, bool)> = None;

        if todo.entities {
            match self.client.fetch_entities().await {
                Ok(entities) => {
                    info!("Loaded {} DXCC entities from cqdx.io", entities.len());
                    self.cache.write().await.load_entities(entities);
                    self.pending().entities = false;
                }
                Err(e) => {
                    warn!("cqdx.io entities fetch failed: {e}");
                    if !retry_can_help(&e) {
                        self.pending().blocked = true;
                    }
                    if stops_sequence(&e) {
                        stopped = Some((clip(&e.to_string()), retry_can_help(&e)));
                    }
                    first_err = Some(e);
                }
            }
        }

        if todo.dxcc && fetch_dxcc {
            if let Some((error, retrying)) = &stopped {
                self.cached_lookup
                    .mark_dxcc_need_list_failed(error, *retrying);
            } else {
                match self.client.fetch_needed(band).await {
                    Ok(needed) => self.install_needed(needed, band).await,
                    Err(e) => {
                        warn!("cqdx.io needed-entity fetch failed: {e}");
                        self.record_dxcc_failure(&e);
                        if stops_sequence(&e) {
                            stopped = Some((clip(&e.to_string()), retry_can_help(&e)));
                        }
                        first_err.get_or_insert(e);
                    }
                }
            }
        }

        if todo.grids {
            if let Some((error, retrying)) = &stopped {
                self.cached_lookup
                    .mark_grid_need_list_failed(error, *retrying);
                if !*retrying {
                    self.pending().grids = false;
                }
            } else {
                // The needed-grids endpoint is a roadmap item: a 404 is
                // recorded as "not offered" (status unknown), never as "no
                // grids needed". See docs/cqdx-api-requirements.md.
                match self.client.fetch_needed_grids().await {
                    Ok(Some(grids)) => {
                        let needed_grids: std::collections::HashSet<String> = grids
                            .iter()
                            .map(|g| normalize_grid_field(g))
                            .filter(|g| !g.is_empty())
                            .collect();
                        info!("Loaded {} needed grids from cqdx.io", needed_grids.len());
                        self.cached_lookup.update_needed_grids(needed_grids);
                        self.pending().grids = false;
                    }
                    Ok(None) => {
                        debug!("cqdx.io does not offer needed grids (404)");
                        self.cached_lookup.mark_grid_need_list_not_offered();
                        self.pending().grids = false;
                    }
                    Err(e) => {
                        let retrying = retry_can_help(&e);
                        debug!("needed-grids fetch failed ({e}); grid needs unknown");
                        self.cached_lookup
                            .mark_grid_need_list_failed(&clip(&e.to_string()), retrying);
                        let mut pending = self.pending();
                        if !retrying {
                            pending.grids = false;
                            pending.blocked = true;
                        }
                    }
                }
            }
        }

        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Update the current band filter for the spot poller.
    pub async fn set_current_band(&self, band: Option<String>) {
        *self.current_band.write().await = band;
    }

    /// Spawn a background task that polls live spot groups every N seconds.
    /// Stops polling if no decode activity for 2 hours (watchdog).
    ///
    /// PAN-93: runs after a failed start too. Each tick it retries the loads
    /// still pending (at most every [`NEED_DATA_RETRY`]), and it polls spots
    /// only once the needed-entity list has loaded, so cqdx spots never flow
    /// without the list that scores them.
    pub fn spawn_spot_poller(
        self: &Arc<Self>,
        shutdown: Arc<AtomicBool>,
        last_decode: Arc<std::sync::atomic::AtomicU64>,
        mode: Option<String>,
        tui_tx: Option<crossbeam_channel::Sender<pancetta_tui::tui_runner::TuiMessage>>,
    ) -> tokio::task::JoinHandle<()> {
        let bridge = Arc::clone(self);
        let client = self.client.clone();
        let cache = self.cache.clone();
        let cached_lookup = self.cached_lookup.clone();
        let interval = self.poll_interval;
        let current_band_ref = self.current_band.clone();
        let operating_frequency_hz = self.operating_frequency_hz.clone();
        let watchdog_timeout = std::time::Duration::from_secs(2 * 60 * 60); // 2 hours

        tokio::spawn(async move {
            let mut timer = tokio::time::interval(interval);
            let mut consecutive_failures: u32 = 0;
            let mut polling_paused = false;
            let backoff_interval = Duration::from_secs(5 * 60); // 5 min retry after failures
            let mut last_backoff_attempt = std::time::Instant::now();
            // Last band we re-fetched per-band needs for. `None` until the
            // dial frequency is first observed.
            let mut last_needed_band: Option<String> = None;
            // PAN-85: remember the last US state actually forwarded for each
            // callsign (uppercased key), so a state that is learned *or
            // corrected* later still reaches the TUI. Repeats of an unchanged
            // value are suppressed.
            let mut sent_states: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();

            loop {
                timer.tick().await;

                if shutdown.load(Ordering::Acquire) {
                    break;
                }

                // Per-band needs: when the dial moves to a new band, re-fetch
                // the needed set for that band. cqdx returns the band's fills
                // plus the all-time ATNO set, so this replaces both lookups.
                // Graceful-degrade: on error keep the prior set unchanged
                // (PAN-93: a list that never loaded is marked unavailable).
                // Skipped once a permanent error (bad/rejected token) has
                // left the list unavailable: only a restart helps.
                let dial_band = operating_frequency_hz.as_ref().and_then(|freq| {
                    let dial_hz = freq.load(Ordering::Relaxed);
                    (dial_hz > 0)
                        .then(|| frequency_to_band(dial_hz).map(|b| b.to_uppercase()))
                        .flatten()
                });
                // A rejected token (any endpoint) also stops it after a load,
                // instead of re-sending the rejected fetch every tick.
                let dxcc_blocked = bridge.pending().blocked
                    || matches!(
                        cached_lookup.dxcc_need_list(),
                        NeedListAvailability::Unavailable {
                            retrying: false,
                            ..
                        }
                    );
                if let Some(band) = dial_band.as_ref().filter(|_| !dxcc_blocked) {
                    if last_needed_band.as_deref() != Some(band.as_str()) {
                        match client.fetch_needed(Some(band)).await {
                            Ok(needed) => {
                                bridge.install_needed(needed, Some(band)).await;
                                *current_band_ref.write().await = Some(band.clone());
                                last_needed_band = Some(band.clone());
                            }
                            Err(e) => {
                                debug!(
                                    "per-band needed fetch for {} failed ({e}); keeping prior set",
                                    band
                                );
                                bridge.record_dxcc_failure(&e);
                            }
                        }
                    }
                }

                // PAN-93: retry whatever a failed or partial start left
                // pending. With the dial known, the per-band block above
                // already tried the needed-entity list this tick.
                let fetch_dxcc = dial_band.is_none();
                let due = {
                    let pending = bridge.pending();
                    pending.retryable(fetch_dxcc)
                        && need_retry_due(pending.last_attempt, Instant::now())
                };
                if due {
                    if let Err(e) = bridge.load_pending(dial_band.as_deref(), fetch_dxcc).await {
                        debug!("cqdx.io pending-load retry failed: {e}");
                    }
                }

                // PAN-93: spots wait for the needed-entity list, so network
                // rows are never scored against an unknown list. A permanent
                // error seen on any endpoint (e.g. a rejected PAT) also stops
                // live polling: like `report_spots`/`report_qso`, no further
                // request would succeed until restart.
                if bridge.pending().blocked
                    || !matches!(
                        cached_lookup.dxcc_need_list(),
                        NeedListAvailability::Loaded { .. }
                    )
                {
                    continue;
                }

                // Watchdog: check last decode activity (A9: lock-free atomic
                // epoch-ms; 0 = never decoded yet).
                let last_ms = last_decode.load(std::sync::atomic::Ordering::Relaxed);
                if last_ms != 0 {
                    let elapsed_ms = crate::coordinator::now_epoch_ms().saturating_sub(last_ms);
                    if elapsed_ms > watchdog_timeout.as_millis() as u64 {
                        if !polling_paused {
                            info!("cqdx.io watchdog: no decode activity for 2h, pausing polling");
                            polling_paused = true;
                        }
                        continue;
                    } else if polling_paused {
                        info!("cqdx.io watchdog: decode activity resumed, resuming polling");
                        polling_paused = false;
                        consecutive_failures = 0;
                    }
                }

                // After 3 consecutive failures, retry every 5 minutes
                if consecutive_failures >= 3 {
                    if last_backoff_attempt.elapsed() < backoff_interval {
                        continue;
                    }
                    info!("cqdx.io polling: retrying after backoff");
                    last_backoff_attempt = std::time::Instant::now();
                }

                let current_band = current_band_ref.read().await.clone();
                match client
                    .fetch_live_spots(current_band.as_deref(), mode.as_deref())
                    .await
                {
                    Ok(groups) => {
                        consecutive_failures = 0;
                        debug!("Polled {} spot groups from cqdx.io", groups.len());

                        // Update rarity scores in CachedStationLookup
                        let rarity_map: HashMap<String, f64> = groups
                            .iter()
                            .map(|g| (g.dx_call.to_uppercase(), rank_to_rarity(g.rarity_rank)))
                            .collect();
                        cached_lookup.update_rarity_scores(rarity_map);

                        // Update notable callsigns
                        let notables: std::collections::HashSet<String> = groups
                            .iter()
                            .filter(|g| g.is_notable)
                            .map(|g| g.dx_call.to_uppercase())
                            .collect();
                        cached_lookup.update_notable_callsigns(notables);

                        // Update network SNR data
                        let snr_data: HashMap<String, (u32, i32)> = groups
                            .iter()
                            .filter_map(|g| {
                                g.best_snr
                                    .map(|snr| (g.dx_call.to_uppercase(), (g.reporter_count, snr)))
                            })
                            .collect();
                        cached_lookup.update_network_snr(snr_data);

                        // Update network last-seen timestamps
                        let last_seen_data: HashMap<String, i64> = groups
                            .iter()
                            .map(|g| (g.dx_call.to_uppercase(), g.last_seen))
                            .collect();
                        cached_lookup.update_network_last_seen(last_seen_data);

                        // Push spot groups to TUI
                        if let Some(ref tx) = tui_tx {
                            let spot_infos: Vec<pancetta_tui::tui_runner::CqdxSpotInfo> = groups
                                .iter()
                                .map(|g| {
                                    // Same CachedStationLookup the scorer uses;
                                    // `needed` only when cqdx's loaded list
                                    // names the entity (PAN-93).
                                    let (needed, atno) =
                                        cached_lookup.display_needed_atno(&g.dx_call);
                                    pancetta_tui::tui_runner::CqdxSpotInfo {
                                        dx_call: g.dx_call.clone(),
                                        band: g.band.clone(),
                                        mode: g.mode.clone(),
                                        frequency_hz: g.frequency,
                                        grid: g.dx_grid.clone(),
                                        rarity_tier: g.rarity_tier.clone(),
                                        reporter_count: g.reporter_count,
                                        best_snr: g.best_snr,
                                        confidence: g.confidence,
                                        first_seen: g.first_seen,
                                        last_seen: g.last_seen,
                                        is_notable: g.is_notable,
                                        notable_type: g.notable_type.clone(),
                                        entity_name: g.dx_entity_name.clone(),
                                        needed,
                                        atno,
                                    }
                                })
                                .collect();
                            let _ =
                                tx.send(pancetta_tui::tui_runner::TuiMessage::SpotGroupUpdate {
                                    spots: spot_infos,
                                });

                            // PAN-85: a cqdx network-only spot for a
                            // previously-worked US station also gets its
                            // state, so DX Hunter shows it even without a
                            // local decode.
                            for g in &groups {
                                if let Some(state) = cached_lookup.state_for(&g.dx_call) {
                                    let key = g.dx_call.to_uppercase();
                                    if sent_states.get(&key) != Some(&state) {
                                        sent_states.insert(key, state.clone());
                                        let _ = tx.send(
                                            pancetta_tui::tui_runner::TuiMessage::StationState {
                                                callsign: g.dx_call.clone(),
                                                state,
                                            },
                                        );
                                    }
                                }
                            }
                        }

                        // Update cache
                        let mut c = cache.write().await;
                        c.update_spot_groups(groups);
                    }
                    Err(e) => {
                        if !retry_can_help(&e) {
                            // PAN-93: a permanent error (e.g. a rejected PAT)
                            // fails every later request the same way. Record
                            // it once and stop polling for the session, like
                            // `report_spots`/`report_qso` already do.
                            warn!(
                                "cqdx.io live spot poll failed permanently ({e}); \
                                 stopping until restart"
                            );
                            bridge.pending().blocked = true;
                            continue;
                        }
                        consecutive_failures += 1;
                        warn!(
                            "cqdx.io live spot poll failed ({}/3): {}",
                            consecutive_failures, e
                        );
                        if consecutive_failures >= 3 {
                            warn!("cqdx.io polling stopped after 3 consecutive failures");
                        }
                    }
                }
            }
        })
    }

    /// Report a batch of spots to cqdx.io. Fire-and-forget (spawns a task).
    ///
    /// PAN-93: skipped once cqdx.io has rejected the token (or the config is
    /// unusable), now that the bridge stays registered after a failed start.
    pub fn report_spots(&self, spots: Vec<SpotReport>) {
        if spots.is_empty() || self.pending().blocked {
            return;
        }
        let client = self.client.clone();
        tokio::spawn(async move {
            if let Err(e) = client.report_spots(spots).await {
                debug!("Failed to report spots to cqdx.io: {}", e);
            }
        });
    }

    /// Report a completed QSO to cqdx.io. Fire-and-forget (spawns a task).
    ///
    /// PAN-93: skipped once cqdx.io has rejected the token, like `report_spots`.
    pub fn report_qso(&self, qso: QsoRecord) {
        if self.pending().blocked {
            debug!("cqdx.io token rejected this session; not reporting QSO");
            return;
        }
        let client = self.client.clone();
        tokio::spawn(async move {
            if let Err(e) = client.report_qso(qso).await {
                debug!("Failed to report QSO to cqdx.io: {}", e);
            }
        });
    }

    /// Get current spot frequencies and rarities for frequency nudging.
    pub async fn spot_frequencies(&self) -> Vec<(f64, f64)> {
        let cache = self.cache.read().await;
        cache
            .spot_groups()
            .iter()
            .map(|g| {
                (
                    g.frequency as f64,
                    pancetta_cqdx::rank_to_rarity(g.rarity_rank),
                )
            })
            .collect()
    }

    /// Get a clone of the cache for read access.
    pub fn cache(&self) -> Arc<RwLock<CqdxCache>> {
        self.cache.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pancetta_qso::priority::WorkedStationLookup;

    fn ne(prefix: &str, atno: bool) -> NeededEntity {
        NeededEntity {
            entity_id: 1,
            name: prefix.to_string(),
            prefix: prefix.to_string(),
            atno,
        }
    }

    #[test]
    fn apply_needed_splits_atno_subset() {
        let lookup = CachedStationLookup::new();
        let needed = vec![ne("3Y/B", true), ne("JA", false), ne("VK0", true)];
        let (total, atno) = apply_needed(&lookup, &needed, None);
        assert_eq!(total, 3);
        assert_eq!(atno, 2);
        assert!(matches!(
            lookup.dxcc_need_list(),
            NeedListAvailability::Loaded { band: None, .. }
        ));

        // All three are "needed".
        assert!(lookup.is_needed_dxcc("3Y/B1234"));
        assert!(lookup.is_needed_dxcc("JA1ABC"));
        assert!(lookup.is_needed_dxcc("VK0XYZ"));

        // Only the ATNO-flagged two report is_atno.
        assert!(lookup.is_atno("3Y/B1234"));
        assert!(lookup.is_atno("VK0XYZ"));
        assert!(!lookup.is_atno("JA1ABC")); // band-fill, not ATNO
    }

    #[test]
    fn apply_needed_replaces_prior_set() {
        let lookup = CachedStationLookup::new();
        apply_needed(&lookup, &[ne("JA", true)], None);
        assert!(lookup.is_atno("JA1ABC"));

        // A subsequent (per-band) fetch fully replaces both sets.
        apply_needed(&lookup, &[ne("DL", false)], Some("20M"));
        assert!(matches!(
            lookup.dxcc_need_list(),
            NeedListAvailability::Loaded { band: Some(ref b), .. } if b == "20M"
        ));
        assert!(!lookup.is_needed_dxcc("JA1ABC"));
        assert!(!lookup.is_atno("JA1ABC"));
        assert!(lookup.is_needed_dxcc("DL5XYZ"));
        assert!(!lookup.is_atno("DL5XYZ"));
    }

    use crate::priority_evaluator::NeedStatus;
    use std::collections::HashSet;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TOKEN: &str = "pat_0123456789abcdef";

    fn cfg(base_url: &str, token: Option<&str>) -> pancetta_config::network::CqdxConfig {
        pancetta_config::network::CqdxConfig {
            enabled: true,
            base_url: base_url.to_string(),
            token: token.map(str::to_string),
            ..Default::default()
        }
    }

    fn bridge_for(server_uri: &str) -> (Arc<CqdxBridge>, Arc<CachedStationLookup>) {
        let lookup = Arc::new(CachedStationLookup::new());
        let bridge = CqdxBridge::from_config(&cfg(server_uri, Some(TOKEN)), lookup.clone())
            .expect("enabled + valid token yields a bridge");
        (Arc::new(bridge), lookup)
    }

    async fn mount(server: &MockServer, route: &str, status: u16, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(server)
            .await;
    }

    fn needed_json(prefixes: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "needed": prefixes
                .iter()
                .map(|p| serde_json::json!({"entityId": 1, "name": p, "prefix": p, "atno": false}))
                .collect::<Vec<_>>()
        })
    }

    fn is_unavailable(a: &NeedListAvailability, want_retrying: bool) -> bool {
        matches!(a, NeedListAvailability::Unavailable { retrying, .. } if *retrying == want_retrying)
    }

    #[tokio::test]
    async fn bridge_construction_marks_lists_loading() {
        let (_bridge, lookup) = bridge_for("http://127.0.0.1:9");
        assert_eq!(lookup.dxcc_need_list(), NeedListAvailability::Loading);
        assert_eq!(lookup.grid_need_list(), NeedListAvailability::Loading);
    }

    #[test]
    fn from_config_without_token_marks_lists_unavailable_not_retrying() {
        for token in [None, Some("")] {
            let lookup = Arc::new(CachedStationLookup::new());
            assert!(
                CqdxBridge::from_config(&cfg("https://cqdx.io", token), lookup.clone()).is_none()
            );
            for list in [lookup.dxcc_need_list(), lookup.grid_need_list()] {
                match list {
                    NeedListAvailability::Unavailable {
                        retrying: false,
                        error,
                        ..
                    } => assert!(error.contains("token"), "{error}"),
                    other => panic!("{token:?}: expected Unavailable, got {other:?}"),
                }
            }
        }
        let lookup = Arc::new(CachedStationLookup::new());
        let disabled = pancetta_config::network::CqdxConfig {
            enabled: false,
            ..cfg("https://cqdx.io", Some(TOKEN))
        };
        assert!(CqdxBridge::from_config(&disabled, lookup.clone()).is_none());
        assert_eq!(lookup.dxcc_need_list(), NeedListAvailability::NotConfigured);
        assert_eq!(lookup.grid_need_list(), NeedListAvailability::NotConfigured);
    }

    #[tokio::test]
    async fn startup_success_with_empty_answers_is_loaded_not_needed() {
        let server = MockServer::start().await;
        mount(
            &server,
            "/api/v1/entities",
            200,
            serde_json::json!({"entities": []}),
        )
        .await;
        mount(
            &server,
            "/api/v1/entities/needed",
            200,
            serde_json::json!({"needed": []}),
        )
        .await;
        mount(
            &server,
            "/api/v1/entities/needed-grids",
            200,
            serde_json::json!({"grids": []}),
        )
        .await;
        let (bridge, lookup) = bridge_for(&server.uri());
        lookup.set_excluded_dxcc_prefixes(HashSet::from(["K".to_string()]));
        bridge.startup().await.unwrap();
        assert!(matches!(
            lookup.dxcc_need_list(),
            NeedListAvailability::Loaded { band: None, .. }
        ));
        assert!(matches!(
            lookup.grid_need_list(),
            NeedListAvailability::Loaded { band: None, .. }
        ));
        assert_eq!(lookup.needed_dxcc_status("JA1XYZ"), NeedStatus::NotNeeded);
        assert!(!lookup.is_needed_dxcc("JA1XYZ"));
        assert_eq!(lookup.needed_grid_status("FN42"), NeedStatus::NotNeeded);
    }

    #[tokio::test]
    async fn startup_transport_failure_marks_lists_unavailable_and_retrying() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let (bridge, lookup) = bridge_for(&format!("http://127.0.0.1:{port}"));
        assert!(bridge.startup().await.is_err());
        assert!(
            is_unavailable(&lookup.dxcc_need_list(), true),
            "{:?}",
            lookup.dxcc_need_list()
        );
        assert!(
            is_unavailable(&lookup.grid_need_list(), true),
            "{:?}",
            lookup.grid_need_list()
        );
        assert_eq!(lookup.needed_dxcc_status("JA1XYZ"), NeedStatus::Unknown);
        assert_eq!(lookup.needed_grid_status("FN42"), NeedStatus::Unknown);
    }

    #[tokio::test]
    async fn unauthorized_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/entities"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let (bridge, lookup) = bridge_for(&server.uri());
        assert!(bridge.startup().await.is_err());
        assert!(is_unavailable(&lookup.dxcc_need_list(), false));
        assert!(is_unavailable(&lookup.grid_need_list(), false));
        bridge.retry_pending_loads(None).await.unwrap();
        bridge.report_spots(vec![SpotReport {
            callsign: "JA1ABC".into(),
            grid: None,
            frequency: 14_074_000,
            mode: "FT8".into(),
            snr: -10,
            timestamp: chrono::Utc::now(),
            reporter: "K5ARH".into(),
            reporter_grid: None,
        }]);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Only the one startup request reached the server.
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn needed_grids_404_is_not_offered() {
        let server = MockServer::start().await;
        mount(
            &server,
            "/api/v1/entities",
            200,
            serde_json::json!({"entities": []}),
        )
        .await;
        mount(
            &server,
            "/api/v1/entities/needed",
            200,
            needed_json(&["JA"]),
        )
        .await;
        mount(
            &server,
            "/api/v1/entities/needed-grids",
            404,
            serde_json::json!({}),
        )
        .await;
        let (bridge, lookup) = bridge_for(&server.uri());
        bridge.startup().await.unwrap();
        assert_eq!(lookup.grid_need_list(), NeedListAvailability::NotOffered);
        assert_eq!(lookup.needed_grid_status("FN42"), NeedStatus::Unknown);
        assert!(matches!(
            lookup.dxcc_need_list(),
            NeedListAvailability::Loaded { .. }
        ));
    }

    #[tokio::test]
    async fn server_error_on_needed_does_not_block_grids() {
        let server = MockServer::start().await;
        mount(
            &server,
            "/api/v1/entities",
            200,
            serde_json::json!({"entities": []}),
        )
        .await;
        mount(
            &server,
            "/api/v1/entities/needed",
            503,
            serde_json::json!({}),
        )
        .await;
        mount(
            &server,
            "/api/v1/entities/needed-grids",
            200,
            serde_json::json!({"grids": ["FN42"]}),
        )
        .await;
        let (bridge, lookup) = bridge_for(&server.uri());
        assert!(bridge.startup().await.is_err());
        assert!(is_unavailable(&lookup.dxcc_need_list(), true));
        assert!(matches!(
            lookup.grid_need_list(),
            NeedListAvailability::Loaded { .. }
        ));
        assert_eq!(lookup.needed_grid_status("FN42"), NeedStatus::Needed);
    }

    #[tokio::test]
    async fn retry_recovers_after_outage() {
        let server = MockServer::start().await;
        mount(
            &server,
            "/api/v1/entities",
            200,
            serde_json::json!({"entities": []}),
        )
        .await;
        mount(
            &server,
            "/api/v1/entities/needed-grids",
            200,
            serde_json::json!({"grids": []}),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/entities/needed"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mount(
            &server,
            "/api/v1/entities/needed",
            200,
            needed_json(&["JA"]),
        )
        .await;
        let (bridge, lookup) = bridge_for(&server.uri());
        assert!(bridge.startup().await.is_err());
        assert_eq!(lookup.needed_dxcc_status("JA1ABC"), NeedStatus::Unknown);
        bridge.retry_pending_loads(None).await.unwrap();
        assert!(matches!(
            lookup.dxcc_need_list(),
            NeedListAvailability::Loaded { .. }
        ));
        assert_eq!(lookup.needed_dxcc_status("JA1ABC"), NeedStatus::Needed);
        assert_eq!(lookup.needed_dxcc_status("DL1ABC"), NeedStatus::NotNeeded);
    }

    #[test]
    fn need_retry_due_after_sixty_seconds() {
        let t = Instant::now();
        assert!(need_retry_due(None, t));
        assert!(!need_retry_due(Some(t), t + Duration::from_secs(59)));
        assert!(need_retry_due(Some(t), t + Duration::from_secs(60)));
    }

    #[tokio::test]
    async fn poller_does_not_poll_spots_until_needed_list_loads() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/entities/needed"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1..)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/spots"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"groups": []})),
            )
            .expect(0)
            .mount(&server)
            .await;
        let lookup = Arc::new(CachedStationLookup::new());
        let config = pancetta_config::network::CqdxConfig {
            poll_interval_secs: 10,
            ..cfg(&server.uri(), Some(TOKEN))
        };
        let bridge = Arc::new(CqdxBridge::from_config(&config, lookup.clone()).unwrap());
        let handle = bridge.spawn_spot_poller(
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(0)),
            None,
            None,
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        handle.abort();
        assert!(is_unavailable(&lookup.dxcc_need_list(), true));
        server.verify().await;
    }

    #[tokio::test]
    async fn live_polling_stops_after_a_permanent_error_on_a_later_endpoint() {
        // Review finding (PAN-93 remediation): entities and the needed
        // list load, then needed-grids answers 401 — a permanent error,
        // so `pending.blocked` is set while the DXCC list stays Loaded.
        // The spot gate must also refuse on `blocked`, or the poller
        // keeps sending the rejected PAT to /api/v1/spots every tick.
        let server = MockServer::start().await;
        mount(
            &server,
            "/api/v1/entities",
            200,
            serde_json::json!({"entities": []}),
        )
        .await;
        mount(
            &server,
            "/api/v1/entities/needed",
            200,
            needed_json(&["JA"]),
        )
        .await;
        mount(
            &server,
            "/api/v1/entities/needed-grids",
            401,
            serde_json::json!({}),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/spots"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"groups": []})),
            )
            .expect(0)
            .mount(&server)
            .await;
        let lookup = Arc::new(CachedStationLookup::new());
        let config = pancetta_config::network::CqdxConfig {
            poll_interval_secs: 10,
            ..cfg(&server.uri(), Some(TOKEN))
        };
        let bridge = Arc::new(CqdxBridge::from_config(&config, lookup.clone()).unwrap());
        bridge.startup().await.unwrap();
        assert!(matches!(
            lookup.dxcc_need_list(),
            NeedListAvailability::Loaded { .. }
        ));
        assert!(bridge.pending().blocked, "grids 401 must block the session");
        let handle = bridge.spawn_spot_poller(
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(0)),
            None,
            None,
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        handle.abort();
        server.verify().await;
    }

    #[tokio::test]
    async fn permanent_live_spot_error_blocks_further_polling() {
        // Review finding (PAN-93 remediation): a permanent error from the
        // live-spots request itself (401) must classify as blocked instead
        // of retrying on backoff until restart.
        let server = MockServer::start().await;
        mount(
            &server,
            "/api/v1/entities",
            200,
            serde_json::json!({"entities": []}),
        )
        .await;
        mount(
            &server,
            "/api/v1/entities/needed",
            200,
            needed_json(&["JA"]),
        )
        .await;
        mount(
            &server,
            "/api/v1/entities/needed-grids",
            200,
            serde_json::json!({"grids": []}),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/spots"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let lookup = Arc::new(CachedStationLookup::new());
        let config = pancetta_config::network::CqdxConfig {
            poll_interval_secs: 1,
            ..cfg(&server.uri(), Some(TOKEN))
        };
        let bridge = Arc::new(CqdxBridge::from_config(&config, lookup.clone()).unwrap());
        bridge.startup().await.unwrap();
        assert!(!bridge.pending().blocked);
        let handle = bridge.spawn_spot_poller(
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(0)),
            None,
            None,
        );
        tokio::time::sleep(std::time::Duration::from_millis(2300)).await;
        handle.abort();
        assert!(
            bridge.pending().blocked,
            "401 on spots must block the session"
        );
        server.verify().await;
    }
}
