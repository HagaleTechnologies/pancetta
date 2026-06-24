//! Coordinator-level wiring for cqdx.io integration.
//!
//! Handles startup (fetch entities + needed), periodic live spot polling,
//! and fire-and-forget spot/QSO reporting.

use crate::priority_evaluator::CachedStationLookup;
use pancetta_cqdx::{
    frequency_to_band, rank_to_rarity, CqdxCache, CqdxClient, NeededEntity, QsoRecord, SpotReport,
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
/// only those flagged `atno` by cqdx.io. Returns `(total, atno)` counts.
fn apply_needed(cached_lookup: &CachedStationLookup, needed: &[NeededEntity]) -> (usize, usize) {
    let prefixes: std::collections::HashSet<String> =
        needed.iter().map(|n| n.prefix.to_uppercase()).collect();
    let atno: std::collections::HashSet<String> = needed
        .iter()
        .filter(|n| n.atno)
        .map(|n| n.prefix.to_uppercase())
        .collect();
    let (total, atno_count) = (prefixes.len(), atno.len());
    cached_lookup.update_needed_dxcc(prefixes);
    cached_lookup.update_needed_atno(atno);
    (total, atno_count)
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
}

impl CqdxBridge {
    /// Create a bridge from config. Returns None if cqdx.io is not configured.
    pub fn from_config(
        config: &pancetta_config::network::CqdxConfig,
        cached_lookup: Arc<CachedStationLookup>,
    ) -> Option<Self> {
        if !config.enabled {
            info!("cqdx.io integration disabled");
            return None;
        }
        let token = config.token.as_ref()?;
        if token.is_empty() {
            warn!("cqdx.io enabled but token is empty, skipping");
            return None;
        }
        let client = match CqdxClient::new(config.base_url.clone(), token.clone()) {
            Ok(c) => c,
            Err(e) => {
                warn!("cqdx.io client creation failed: {}", e);
                return None;
            }
        };
        Some(Self {
            client,
            cache: Arc::new(RwLock::new(CqdxCache::new())),
            cached_lookup,
            poll_interval: Duration::from_secs(config.poll_interval_secs),
            current_band: Arc::new(RwLock::new(None)),
            operating_frequency_hz: None,
        })
    }

    /// Attach the coordinator's shared dial-frequency atomic so the spot
    /// poller can track the operating band and re-fetch per-band needs.
    pub fn with_operating_frequency(mut self, freq: Arc<AtomicU64>) -> Self {
        self.operating_frequency_hz = Some(freq);
        self
    }

    /// Fetch entities and needed data on startup. Populates cache and CachedStationLookup.
    pub async fn startup(&self) -> pancetta_cqdx::Result<()> {
        // Fetch entities
        let entities = self.client.fetch_entities().await?;
        info!("Loaded {} DXCC entities from cqdx.io", entities.len());

        // Fetch needed. At startup we may not know the operating band yet,
        // so fetch the all-time/ATNO set (band=None). The spot poller
        // re-fetches per-band fills once the dial frequency is known.
        let needed = self.client.fetch_needed(None).await?;
        info!("Loaded {} needed entities from cqdx.io", needed.len());

        // Populate cache
        let mut cache = self.cache.write().await;
        cache.load_entities(entities);
        cache.load_needed(needed.clone());

        // Update CachedStationLookup needed_dxcc + ATNO subset.
        let (total, atno) = apply_needed(&self.cached_lookup, &needed);
        info!(
            "cqdx.io needed: {} DXCC prefix(es), {} flagged ATNO",
            total, atno
        );

        // Populate needed_grids from the (roadmap) grid-needed endpoint.
        // Graceful-degrade: if the cqdx.io server hasn't shipped
        // /api/v1/entities/needed-grids yet, fetch_needed_grids returns an
        // empty Vec on 404, and any other error is swallowed here — the set is
        // left empty so is_needed_grid returns false ("unknown" = "no bonus"
        // rather than inflating all priority scores). Once the endpoint is
        // live, the needed_grid priority weight starts contributing with no
        // further code change. See: docs/cqdx-api-requirements.md
        match self.client.fetch_needed_grids().await {
            Ok(grids) => {
                let needed_grids: std::collections::HashSet<String> = grids
                    .iter()
                    .map(|g| normalize_grid_field(g))
                    .filter(|g| !g.is_empty())
                    .collect();
                info!("Loaded {} needed grids from cqdx.io", needed_grids.len());
                self.cached_lookup.update_needed_grids(needed_grids);
            }
            Err(e) => {
                debug!("needed-grids fetch failed ({e}); leaving grid set empty (inert)");
            }
        }

        Ok(())
    }

    /// Update the current band filter for the spot poller.
    pub async fn set_current_band(&self, band: Option<String>) {
        *self.current_band.write().await = band;
    }

    /// Spawn a background task that polls live spot groups every N seconds.
    /// Stops polling if no decode activity for 2 hours (watchdog).
    pub fn spawn_spot_poller(
        &self,
        shutdown: Arc<AtomicBool>,
        last_decode: Arc<RwLock<Option<Instant>>>,
        mode: Option<String>,
        tui_tx: Option<crossbeam_channel::Sender<pancetta_tui::tui_runner::TuiMessage>>,
    ) -> tokio::task::JoinHandle<()> {
        let client = self.client.clone();
        let cache = self.cache.clone();
        let cached_lookup = self.cached_lookup.clone();
        let interval = self.poll_interval;
        let current_band_ref = self.current_band.clone();
        let operating_frequency_hz = self.operating_frequency_hz.clone();
        let needed_lookup = self.cached_lookup.clone();
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

            loop {
                timer.tick().await;

                if shutdown.load(Ordering::Acquire) {
                    break;
                }

                // Per-band needs: when the dial moves to a new band, re-fetch
                // the needed set for that band. cqdx returns the band's fills
                // plus the all-time ATNO set, so this replaces both lookups.
                // Graceful-degrade: on error keep the prior set unchanged.
                if let Some(ref freq) = operating_frequency_hz {
                    let dial_hz = freq.load(Ordering::Relaxed);
                    if dial_hz > 0 {
                        if let Some(band) = frequency_to_band(dial_hz).map(|b| b.to_uppercase()) {
                            if last_needed_band.as_deref() != Some(band.as_str()) {
                                match client.fetch_needed(Some(&band)).await {
                                    Ok(needed) => {
                                        let (total, atno) = apply_needed(&needed_lookup, &needed);
                                        info!(
                                            "cqdx.io per-band needs for {}: {} prefix(es), {} ATNO",
                                            band, total, atno
                                        );
                                        *current_band_ref.write().await = Some(band.clone());
                                        last_needed_band = Some(band);
                                    }
                                    Err(e) => {
                                        debug!(
                                            "per-band needed fetch for {} failed ({e}); keeping prior set",
                                            band
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                // Watchdog: check last decode activity
                let last = last_decode.read().await;
                if let Some(ts) = *last {
                    if ts.elapsed() > watchdog_timeout {
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
                drop(last);

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
                                .map(|g| pancetta_tui::tui_runner::CqdxSpotInfo {
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
                                })
                                .collect();
                            let _ =
                                tx.send(pancetta_tui::tui_runner::TuiMessage::SpotGroupUpdate {
                                    spots: spot_infos,
                                });
                        }

                        // Update cache
                        let mut c = cache.write().await;
                        c.update_spot_groups(groups);
                    }
                    Err(e) => {
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
    pub fn report_spots(&self, spots: Vec<SpotReport>) {
        if spots.is_empty() {
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
    pub fn report_qso(&self, qso: QsoRecord) {
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
        let (total, atno) = apply_needed(&lookup, &needed);
        assert_eq!(total, 3);
        assert_eq!(atno, 2);

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
        apply_needed(&lookup, &[ne("JA", true)]);
        assert!(lookup.is_atno("JA1ABC"));

        // A subsequent (per-band) fetch fully replaces both sets.
        apply_needed(&lookup, &[ne("DL", false)]);
        assert!(!lookup.is_needed_dxcc("JA1ABC"));
        assert!(!lookup.is_atno("JA1ABC"));
        assert!(lookup.is_needed_dxcc("DL5XYZ"));
        assert!(!lookup.is_atno("DL5XYZ"));
    }
}
