//! Coordinator-level wiring for cqdx.io integration.
//!
//! Handles startup (fetch entities + needed), periodic live spot polling,
//! and fire-and-forget spot/QSO reporting.

use crate::priority_evaluator::CachedStationLookup;
use pancetta_cqdx::{CqdxClient, CqdxCache, SpotReport, QsoRecord, rank_to_rarity};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::sync::RwLock;
use tokio::time::Duration;
use tracing::{debug, error, info, warn};

/// Manages the cqdx.io integration lifecycle.
pub struct CqdxBridge {
    client: CqdxClient,
    cache: Arc<RwLock<CqdxCache>>,
    cached_lookup: Arc<CachedStationLookup>,
    poll_interval: Duration,
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
        let client = CqdxClient::new(config.base_url.clone(), token.clone());
        Some(Self {
            client,
            cache: Arc::new(RwLock::new(CqdxCache::new())),
            cached_lookup,
            poll_interval: Duration::from_secs(config.poll_interval_secs),
        })
    }

    /// Fetch entities and needed data on startup. Populates cache and CachedStationLookup.
    pub async fn startup(&self) -> pancetta_cqdx::Result<()> {
        // Fetch entities
        let entities = self.client.fetch_entities().await?;
        info!("Loaded {} DXCC entities from cqdx.io", entities.len());

        // Fetch needed
        let needed = self.client.fetch_needed().await?;
        info!("Loaded {} needed entities from cqdx.io", needed.len());

        // Populate cache
        let mut cache = self.cache.write().await;
        cache.load_entities(entities);
        cache.load_needed(needed.clone());

        // Update CachedStationLookup needed_dxcc with prefix strings
        let needed_prefixes: std::collections::HashSet<String> = needed
            .iter()
            .map(|n| n.prefix.to_uppercase())
            .collect();
        self.cached_lookup.update_needed_dxcc(needed_prefixes);

        Ok(())
    }

    /// Spawn a background task that polls live spot groups every N seconds.
    /// Stops polling if no decode activity for 2 hours (watchdog).
    pub fn spawn_spot_poller(
        &self,
        shutdown: Arc<AtomicBool>,
        last_decode: Arc<RwLock<Option<Instant>>>,
        band: Option<String>,
        mode: Option<String>,
    ) -> tokio::task::JoinHandle<()> {
        let client = self.client.clone();
        let cache = self.cache.clone();
        let cached_lookup = self.cached_lookup.clone();
        let interval = self.poll_interval;
        let watchdog_timeout = std::time::Duration::from_secs(2 * 60 * 60); // 2 hours

        tokio::spawn(async move {
            let mut timer = tokio::time::interval(interval);
            let mut consecutive_failures: u32 = 0;
            let mut polling_paused = false;
            let backoff_interval = Duration::from_secs(5 * 60); // 5 min retry after failures
            let mut last_backoff_attempt = std::time::Instant::now();

            loop {
                timer.tick().await;

                if shutdown.load(Ordering::Acquire) {
                    break;
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

                match client.fetch_live_spots(
                    band.as_deref(),
                    mode.as_deref(),
                ).await {
                    Ok(groups) => {
                        consecutive_failures = 0;
                        debug!("Polled {} spot groups from cqdx.io", groups.len());

                        // Update rarity scores in CachedStationLookup
                        let rarity_map: HashMap<String, f64> = groups
                            .iter()
                            .map(|g| (g.dx_call.to_uppercase(), rank_to_rarity(g.rarity_rank)))
                            .collect();
                        cached_lookup.update_rarity_scores(rarity_map);

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

    /// Get a clone of the cache for read access.
    pub fn cache(&self) -> Arc<RwLock<CqdxCache>> {
        self.cache.clone()
    }
}
