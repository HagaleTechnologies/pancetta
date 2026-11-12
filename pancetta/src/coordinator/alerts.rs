//! Decode alerts run inside the supervised Autonomous task, including manual mode.
//! HTTP deliveries never hold up decode admission or TX decisions.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pancetta_config::alerts::{AlertsConfig, DxTier};
use pancetta_qso::{PriorityScorer, PriorityTier};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::priority_evaluator::{CachedStationLookup, NeedListAvailability, NeedStatus};

pub(super) fn priority_tier(tier: DxTier) -> PriorityTier {
    match tier {
        DxTier::Atno => PriorityTier::Atno,
        DxTier::PerBandDxccNew => PriorityTier::PerBandDxccNew,
        DxTier::SpecialStation => PriorityTier::SpecialStation,
        DxTier::PerBandGridNew => PriorityTier::PerBandGridNew,
        DxTier::Standard => PriorityTier::Standard,
        DxTier::Suspect => PriorityTier::Suspect,
    }
}

fn tier_name(tier: PriorityTier) -> &'static str {
    match tier {
        PriorityTier::Atno => "atno",
        PriorityTier::PerBandDxccNew => "per_band_dxcc_new",
        PriorityTier::SpecialStation => "special_station",
        PriorityTier::PerBandGridNew => "per_band_grid_new",
        PriorityTier::Standard => "standard",
        PriorityTier::Suspect => "suspect",
    }
}

#[derive(Default)]
struct DeliveryState {
    failures: u8,
    paused_until: Option<Instant>,
}
impl DeliveryState {
    fn available(&self, now: Instant) -> bool {
        self.paused_until.is_none_or(|until| now >= until)
    }
    fn finish(&mut self, success: bool, now: Instant) {
        if success {
            self.failures = 0;
            self.paused_until = None;
        } else {
            self.failures += 1;
            if self.failures >= 3 {
                self.failures = 0;
                self.paused_until = Some(now + Duration::from_secs(300));
                warn!(target: "pancetta::alerts", "alert target paused for five minutes after three failures");
            }
        }
    }
}

pub(super) struct AlertDispatcher {
    config: AlertsConfig,
    client: reqwest::Client,
    scorer: PriorityScorer,
    lookup: CachedStationLookup,
    tui: bool,
    /// (cqdx list loaded, local logbook available) at the last log line.
    available: Option<(bool, bool)>,
    dedupe: HashMap<(usize, String, String), Instant>,
    delivery: [Arc<Mutex<DeliveryState>>; 2],
}
impl AlertDispatcher {
    pub(super) fn new(
        config: AlertsConfig,
        lookup: CachedStationLookup,
        weights: pancetta_qso::priority::PriorityWeights,
        tui: bool,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            config,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            scorer: PriorityScorer::new(weights, Box::new(lookup.clone())),
            lookup,
            tui,
            available: None,
            dedupe: HashMap::new(),
            delivery: std::array::from_fn(|_| Arc::new(Mutex::new(DeliveryState::default()))),
        })
    }

    /// Returns bell admission and detached single-attempt HTTP tasks.
    pub(super) fn on_decode(
        &mut self,
        decoded: &pancetta_ft8::DecodedMessage,
        live_dial: u64,
        pins: &[String],
    ) -> (bool, Vec<tokio::task::JoinHandle<()>>) {
        if !(self.config.bell.enabled || self.config.ntfy.enabled || self.config.webhook.enabled)
            || super::is_stale_band_decode(decoded.captured_dial_hz, live_dial)
        {
            return (false, vec![]);
        }
        let Some(call) = decoded.message.from_callsign.as_deref() else {
            return (false, vec![]);
        };
        let call = call.trim_matches(['<', '>']).to_uppercase();
        if call.is_empty() || call == "..." {
            return (false, vec![]);
        }
        let pinned = pins.iter().any(|p| p.eq_ignore_ascii_case(&call));
        let cq = pancetta_qso::autonomous::is_cq_message(&decoded.text);
        let dial = decoded.captured_dial_hz.unwrap_or(live_dial) as f64;
        let band = pancetta_qso::utils::frequency_to_band(dial).to_lowercase();
        let scored = self.scorer.score_tiered_explained(
            &call,
            decoded.message.grid_square.as_deref(),
            decoded.snr_db as i8,
            dial,
        );
        let loaded = matches!(
            self.lookup.dxcc_need_list(),
            NeedListAvailability::Loaded { .. }
        );
        let local = self.lookup.local_log_available();
        if self.available != Some((loaded, local)) {
            self.available = Some((loaded, local));
            if !loaded && local {
                info!(target: "pancetta::alerts", "cqdx need list unavailable; tier alerts use the local logbook");
            } else if !loaded {
                info!(target: "pancetta::alerts", "cqdx need list unavailable; tier alerts limited to local per-band evidence");
            } else {
                info!(target: "pancetta::alerts", "cqdx need list available for tier alerts");
            }
        }
        // Unknown needed-list fallback is a scoring heuristic, never evidence for an alert.
        // PAN-347: a local logbook verdict is evidence, like a loaded cqdx list.
        let mut tier = scored.score.tier;
        if tier == PriorityTier::PerBandDxccNew
            && self.lookup.dxcc_need_verdict(&call).0 == NeedStatus::Unknown
            && !scored.factors.dxcc_new_on_band
        {
            tier = if scored.factors.notable || scored.factors.special_event_callsign {
                PriorityTier::SpecialStation
            } else if scored.factors.grid_new_on_band {
                PriorityTier::PerBandGridNew
            } else {
                PriorityTier::Standard
            };
        }
        let payload = json!({
            "type": "decode_alert", "source": "pancetta", "version": 1,
            "utc_time": chrono::DateTime::<chrono::Utc>::from(decoded.timestamp).to_rfc3339(),
            "callsign": call,
            "entity": pancetta_core::dxcc::entity_for_callsign(&call).unwrap_or("unknown entity"),
            "band": band, "audio_offset_hz": decoded.frequency_offset,
            "snr_db": decoded.snr_db, "tier": tier_name(tier),
            "tier_rank": 6 - tier as u8,
            "trigger": if pinned { "watchlist" } else { "tier" }, "watchlisted": pinned,
        });
        let now = Instant::now();
        let window = Duration::from_secs(self.config.dedupe_secs);
        self.dedupe
            .retain(|_, seen| now.duration_since(*seen) < window);
        let channels = [
            (
                self.config.bell.enabled && self.tui,
                self.config.bell.min_tier,
            ),
            (self.config.ntfy.enabled, self.config.ntfy.min_tier),
            (self.config.webhook.enabled, self.config.webhook.min_tier),
        ];
        let mut bell = false;
        let mut tasks = vec![];
        let mut raised = vec![];
        for (channel, (enabled, min_tier)) in channels.into_iter().enumerate() {
            if !enabled || !(pinned || cq && tier >= priority_tier(min_tier)) {
                continue;
            }
            let key = (channel, call.clone(), band.clone());
            if self.dedupe.contains_key(&key) {
                continue;
            }
            if channel > 0 {
                let state = &self.delivery[channel - 1];
                let guard = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !guard.available(now) {
                    continue;
                }
            }
            // Hard cap also bounds memory under maliciously varied decodes.
            if self.dedupe.len() >= 4096 {
                if let Some(oldest) = self
                    .dedupe
                    .iter()
                    .min_by_key(|(_, seen)| **seen)
                    .map(|(key, _)| key.clone())
                {
                    self.dedupe.remove(&oldest);
                }
            }
            self.dedupe.insert(key, now);
            raised.push(["bell", "ntfy", "webhook"][channel]);
            if channel == 0 {
                bell = true;
                continue;
            }
            let (url, body) = if channel == 1 {
                (
                    self.config.ntfy.server.clone(),
                    json!({
                        "topic": self.config.ntfy.topic,
                        "title": format!("Decode alert: {} - {}", call, if tier == PriorityTier::Atno { "all-time new entity" } else { tier_name(tier) }),
                        "message": format!("{} ({}) {} on {} at {} Hz, SNR {}", call, payload["entity"].as_str().unwrap_or("unknown entity"), if cq { "CQ" } else { "heard" }, band, decoded.frequency_offset, decoded.snr_db),
                    }),
                )
            } else {
                (self.config.webhook.url.clone(), payload.clone())
            };
            tasks.push(deliver(
                self.client.clone(),
                url,
                body,
                self.delivery[channel - 1].clone(),
                if channel == 1 { "ntfy" } else { "webhook" },
            ));
        }
        if !raised.is_empty() {
            info!(target: "pancetta::alerts", callsign = %call, tier = tier_name(tier), trigger = %payload["trigger"], band = %band, offset_hz = decoded.frequency_offset, entity = %payload["entity"], channels = %raised.join(","), "decode alert");
        }
        (bell, tasks)
    }
}

fn deliver(
    client: reqwest::Client,
    url: String,
    body: Value,
    state: Arc<Mutex<DeliveryState>>,
    channel: &'static str,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let success = client
            .post(url)
            .json(&body)
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
        if !success {
            // Do not print URL-bearing reqwest errors: webhook URLs can contain secrets.
            warn!(target: "pancetta::alerts", channel, "alert delivery failed");
        }
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .finish(success, Instant::now());
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        matchers::{body_partial_json, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    fn decode(call: &str, cq: bool) -> pancetta_ft8::DecodedMessage {
        let mut msg = pancetta_ft8::DecodedMessage::new(
            pancetta_ft8::Ft8Message {
                from_callsign: Some(call.into()),
                grid_square: Some("FN42".into()),
                ..Default::default()
            },
            -8.0,
            1.0,
            1500.0,
            0.0,
        );
        msg.text = if cq {
            format!("CQ {call} FN42")
        } else {
            format!("K1ABC {call} -08")
        };
        msg.captured_dial_hz = Some(14_074_000);
        msg
    }
    fn dispatcher(config: AlertsConfig, tui: bool) -> AlertDispatcher {
        AlertDispatcher::new(config, CachedStationLookup::new(), Default::default(), tui).unwrap()
    }
    async fn complete(tasks: Vec<tokio::task::JoinHandle<()>>) {
        for task in tasks {
            task.await.unwrap();
        }
    }
    #[test]
    fn alternate_prefix_alert_uses_entity_needed_verdict() {
        let lookup = CachedStationLookup::new();
        lookup.update_needed_dxcc(["JA".into(), "K".into()].into());
        lookup.update_needed_atno(["JA".into(), "K".into()].into());
        let mut config = AlertsConfig::default();
        config.bell.enabled = true;
        config.bell.min_tier = DxTier::Atno;
        let mut dispatcher =
            AlertDispatcher::new(config.clone(), lookup.clone(), Default::default(), true).unwrap();
        for call in ["7K1ABC", "JR1ABC", "JH1ABC", "KG4OJT"] {
            assert_eq!(lookup.needed_atno_shown(call), (true, true), "{call}");
            assert!(
                dispatcher.on_decode(&decode(call, true), 14_074_000, &[]).0,
                "{call}"
            );
        }
        assert!(
            !dispatcher
                .on_decode(&decode("KH6ABC", true), 14_074_000, &[])
                .0
        );

        lookup.update_needed_atno(Default::default());
        let mut needed_only =
            AlertDispatcher::new(config.clone(), lookup.clone(), Default::default(), true).unwrap();
        assert_eq!(lookup.needed_atno_shown("7K1ABC"), (true, false));
        assert!(
            !needed_only
                .on_decode(&decode("7K1ABC", true), 14_074_000, &[])
                .0
        );
        config.bell.min_tier = DxTier::PerBandDxccNew;
        let mut needed_threshold =
            AlertDispatcher::new(config, lookup, Default::default(), true).unwrap();
        assert!(
            needed_threshold
                .on_decode(&decode("7K1ABC", true), 14_074_000, &[])
                .0
        );
    }

    #[tokio::test]
    async fn alerts_watchlist_webhook_json_and_dedupe_and_stale_band() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .and(body_partial_json(json!({
                "type":"decode_alert", "source":"pancetta", "version":1,
                "callsign":"VP8ABC", "band":"20m", "audio_offset_hz":1500.0,
                "snr_db":-8.0, "trigger":"watchlist", "watchlisted":true,
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let mut config = AlertsConfig::default();
        config.webhook.enabled = true;
        config.webhook.url = format!("{}/hook", server.uri());
        let mut d = dispatcher(config, false);
        let msg = decode("VP8ABC", false);
        let pins = vec!["VP8ABC".into()];
        assert!(d.on_decode(&msg, 7_074_000, &pins).1.is_empty());
        complete(d.on_decode(&msg, 14_074_000, &pins).1).await;
        assert!(d.on_decode(&msg, 14_074_000, &pins).1.is_empty());
        let requests = server.received_requests().await.unwrap();
        let payload: Value = requests[0].body_json().unwrap();
        assert!(payload["entity"].as_str().unwrap().contains("Falkland"));
        assert_eq!(payload["tier_rank"], 2);
    }
    #[tokio::test]
    async fn alerts_ntfy_json_root_and_per_channel_tiers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(json!({"topic":"ham"})))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let mut config = AlertsConfig::default();
        config.ntfy.enabled = true;
        config.ntfy.server = server.uri();
        config.ntfy.topic = "ham".into();
        config.ntfy.min_tier = DxTier::SpecialStation;
        config.webhook.enabled = true;
        config.webhook.url = format!("{}/hook", server.uri());
        config.webhook.min_tier = DxTier::PerBandDxccNew;
        let mut d = dispatcher(config, false);
        // Local per-band evidence remains valid without a loaded cqdx list.
        complete(d.on_decode(&decode("VP8PJ", true), 14_074_000, &[]).1).await;
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let ntfy: Value = requests
            .iter()
            .find(|r| r.url.path() == "/")
            .unwrap()
            .body_json()
            .unwrap();
        assert!(ntfy["message"].as_str().unwrap().contains("1500"));
        assert!(ntfy["message"].as_str().unwrap().contains("20m"));
    }
    #[tokio::test]
    async fn alerts_non_cq_requires_pin_bell_is_tui_only_and_unknown_entity() {
        let mut config = AlertsConfig::default();
        config.bell.enabled = true;
        let mut d = dispatcher(config.clone(), true);
        assert!(!d.on_decode(&decode("VP8PJ", false), 14_074_000, &[]).0);
        assert!(!d.on_decode(&decode("VP8PJ", true), 14_074_000, &[]).0); // below ATNO
        assert!(
            d.on_decode(&decode("QZ9ZZ", false), 14_074_000, &["QZ9ZZ".into()])
                .0
        );
        assert!(
            !dispatcher(config, false)
                .on_decode(&decode("QZ9ZZ", false), 14_074_000, &["QZ9ZZ".into()])
                .0
        );
        assert_eq!(pancetta_core::dxcc::entity_for_callsign("QZ9ZZ"), None);
    }
    #[tokio::test]
    async fn alerts_delivery_three_failures_pause_and_resume_without_retry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(4)
            .mount(&server)
            .await;
        let mut config = AlertsConfig::default();
        config.webhook.enabled = true;
        config.webhook.url = server.uri();
        let mut d = dispatcher(config, false);
        for call in ["VP8PJ", "T88AA", "JA1ABC"] {
            complete(
                d.on_decode(&decode(call, false), 14_074_000, &[call.into()])
                    .1,
            )
            .await;
        }
        assert!(d
            .on_decode(&decode("K1ABC", false), 14_074_000, &["K1ABC".into()])
            .1
            .is_empty());
        let mut state = d.delivery[1].lock().unwrap();
        assert!(!state.available(Instant::now()));
        assert!(state.available(Instant::now() + Duration::from_secs(301)));
        state.paused_until = Some(Instant::now() - Duration::from_secs(1));
        drop(state);
        complete(
            d.on_decode(&decode("K1ABC", false), 14_074_000, &["K1ABC".into()])
                .1,
        )
        .await;
    }
    #[tokio::test]
    async fn alerts_redirect_is_failure_and_unknown_entity_payload_is_explicit() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({
                "entity": "unknown entity", "callsign": "QZ9ZZ", "trigger": "watchlist",
                "tier": "per_band_grid_new", "tier_rank": 4,
            })))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/redirected"))
            .expect(1)
            .mount(&server)
            .await;
        let mut config = AlertsConfig::default();
        config.webhook.enabled = true;
        config.webhook.url = server.uri();
        let mut d = dispatcher(config, false);
        complete(
            d.on_decode(&decode("QZ9ZZ", false), 14_074_000, &["QZ9ZZ".into()])
                .1,
        )
        .await;
        assert_eq!(d.delivery[1].lock().unwrap().failures, 1);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[test]
    fn alerts_all_six_config_tiers_map_without_renumbering() {
        for (config, runtime) in [
            (DxTier::Atno, PriorityTier::Atno),
            (DxTier::PerBandDxccNew, PriorityTier::PerBandDxccNew),
            (DxTier::SpecialStation, PriorityTier::SpecialStation),
            (DxTier::PerBandGridNew, PriorityTier::PerBandGridNew),
            (DxTier::Standard, PriorityTier::Standard),
            (DxTier::Suspect, PriorityTier::Suspect),
        ] {
            assert_eq!(priority_tier(config), runtime);
        }
    }
}

#[cfg(test)]
mod evidence_tests {
    use super::*;
    use std::io::Write;

    #[derive(Clone)]
    struct Log(Arc<Mutex<Vec<u8>>>);
    impl Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn cq(call: &str, dial: u64) -> pancetta_ft8::DecodedMessage {
        let mut msg = pancetta_ft8::DecodedMessage::new(
            pancetta_ft8::Ft8Message {
                from_callsign: Some(call.into()),
                ..Default::default()
            },
            -8.0,
            1.0,
            1500.0,
            0.0,
        );
        msg.text = format!("CQ {call} FN42");
        msg.captured_dial_hz = Some(dial);
        msg
    }
    #[test]
    fn alerts_atno_bell_info_availability_and_window_expiry() {
        // Exact log counts need process isolation: parallel scoped subscribers
        // share tracing's callsite interest cache and can miss an event.
        if std::env::var_os("PAN261_ALERT_LOG_CHILD").is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "coordinator::alerts::evidence_tests::alerts_atno_bell_info_availability_and_window_expiry", "--nocapture", "--test-threads=1"])
                .env("PAN261_ALERT_LOG_CHILD", "1")
                .output().unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let log = Log(Arc::default());
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let lookup = CachedStationLookup::new();
        lookup.update_needed_atno(["VP8".into()].into());
        let mut config = AlertsConfig::default();
        config.bell.enabled = true;
        let mut d = AlertDispatcher::new(config, lookup.clone(), Default::default(), true).unwrap();
        let msg = cq("VP8ABC", 14_074_000);
        assert!(d.on_decode(&msg, 14_074_000, &[]).0);
        assert!(!d.on_decode(&msg, 14_074_000, &[]).0);
        for seen in d.dedupe.values_mut() {
            *seen -= Duration::from_secs(301);
        }
        assert!(d.on_decode(&msg, 14_074_000, &[]).0);
        let other_band = cq("VP8ABC", 7_074_000);
        assert!(d.on_decode(&other_band, 7_074_000, &[]).0);
        lookup.update_needed_dxcc(["VP8".into()].into());
        d.on_decode(&msg, 14_074_000, &[]);
        let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert_eq!(text.matches("cqdx need list unavailable;").count(), 1);
        assert_eq!(text.matches("cqdx need list available for").count(), 1);
        assert_eq!(text.matches("decode alert").count(), 3);
        assert!(text.contains("INFO"));
        assert!(text.contains("VP8ABC"));
        assert!(text.contains("atno"));
    }
    #[test]
    fn alerts_unknown_fallback_suppressed_but_local_band_evidence_allowed() {
        let mut config = AlertsConfig::default();
        config.bell.enabled = true;
        config.bell.min_tier = DxTier::PerBandDxccNew;
        let mut d =
            AlertDispatcher::new(config, CachedStationLookup::new(), Default::default(), true)
                .unwrap();
        // Without a known dial neither local-band nor remote needed-list evidence exists.
        assert!(!d.on_decode(&cq("VP8PJ", 0), 0, &[]).0);
        assert!(d.on_decode(&cq("VP8PJ", 14_074_000), 14_074_000, &[]).0);
    }
    #[tokio::test]
    async fn local_logbook_atno_alerts_without_cqdx() {
        // PAN-347: an entity absent from qsos.adi is ATNO evidence offline.
        use crate::priority_evaluator::test_support::{adif, seeded_lookup};
        let (_tmp, lookup) = seeded_lookup(&adif(&[("JA1ABC", "FT8", 14.074, "")]), "K5ARH").await;
        lookup.set_need_lists_loading();
        lookup.mark_need_lists_unavailable("connection refused", true);
        let mut config = AlertsConfig::default();
        config.bell.enabled = true;
        config.bell.min_tier = DxTier::Atno;
        let mut d =
            AlertDispatcher::new(config.clone(), lookup.clone(), Default::default(), true).unwrap();
        let tier = d
            .scorer
            .score_tiered_explained("ZL1ABC", None, -8, 14_074_000.0)
            .score
            .tier;
        assert_eq!(tier, PriorityTier::Atno);
        assert!(d.on_decode(&cq("ZL1ABC", 14_074_000), 14_074_000, &[]).0);
        // Without the local log, the same CQ never reaches the ATNO threshold.
        let mut unseeded =
            AlertDispatcher::new(config, CachedStationLookup::new(), Default::default(), true)
                .unwrap();
        assert!(
            !unseeded
                .on_decode(&cq("ZL1ABC", 14_074_000), 14_074_000, &[])
                .0
        );
    }
    #[test]
    fn alerts_special_tier_only_reaches_special_threshold() {
        let lookup = CachedStationLookup::new();
        lookup.update_needed_dxcc(Default::default());
        lookup.seed_worked_dxcc_from_list(vec![("20m".into(), "K1ABC".into())]);
        let mut config = AlertsConfig::default();
        config.bell.enabled = true;
        config.bell.min_tier = DxTier::SpecialStation;
        let mut d =
            AlertDispatcher::new(config.clone(), lookup.clone(), Default::default(), true).unwrap();
        // W1A is a recognized special station; its US entity was already worked on this band.
        let msg = cq("W1A", 14_074_000);
        let tier = d
            .scorer
            .score_tiered_explained("W1A", None, -8, 14_074_000.0)
            .score
            .tier;
        assert_eq!(tier, PriorityTier::SpecialStation);
        assert!(d.on_decode(&msg, 14_074_000, &[]).0);
        config.bell.min_tier = DxTier::PerBandDxccNew;
        assert!(
            !AlertDispatcher::new(config, lookup, Default::default(), true)
                .unwrap()
                .on_decode(&msg, 14_074_000, &[])
                .0
        );
    }
}
