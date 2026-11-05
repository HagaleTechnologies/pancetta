//! Durable contest refresh, independent of UI presence and TX timing.
use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};
use pancetta_qso::{
    async_database::QsoDatabase,
    contest::{ledger::*, scoring::*},
    QsoManager,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub(super) struct Worker(tokio::task::JoinHandle<()>);
impl Drop for Worker {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Where the contest worker may read history from. The live variant carries
/// the real station index and ADIF paths. The replay variant carries the
/// in-memory seed replay startup already prepared (or the failure that
/// stopped its preparation) and no station database path at all, so replay
/// can neither open nor write the operator's real index (PAN-268).
pub(super) enum HistorySource {
    Live {
        db_path: PathBuf,
        adif_path: PathBuf,
    },
    Replay {
        seed: Result<Arc<QsoDatabase>, String>,
    },
}

/// Select the worker's history source exactly as production startup does:
/// replay scores the prepared seed, cloning the `Arc` on success and keeping
/// the initialization failure on error, while live keeps its disk paths.
pub(super) fn select_history_source(
    replay: bool,
    prepared: &Result<Arc<QsoDatabase>, pancetta_qso::async_database::AsyncDatabaseError>,
    db_path: PathBuf,
    adif_path: PathBuf,
) -> HistorySource {
    if replay {
        HistorySource::Replay {
            seed: prepared
                .as_ref()
                .map(Arc::clone)
                .map_err(|error| error.to_string()),
        }
    } else {
        HistorySource::Live { db_path, adif_path }
    }
}

/// Live-only refresh bookkeeping: completed-contact persistence
/// expectations plus the disk-read cache. Replay keeps none of it — its seed
/// never changes and its simulated completions are never persisted.
struct LiveRefresh {
    observed: std::collections::HashSet<String>,
    pending: std::collections::BTreeSet<String>,
    cached: Option<Vec<ContestContactFact>>,
    signature: Option<(u64, std::time::SystemTime)>,
    last_read: Instant,
}

impl LiveRefresh {
    fn new() -> Self {
        Self {
            observed: std::collections::HashSet::new(),
            pending: std::collections::BTreeSet::new(),
            cached: None,
            signature: None,
            last_read: Instant::now() - Duration::from_secs(30),
        }
    }

    async fn refresh(
        &mut self,
        manager: &QsoManager,
        db_path: &std::path::Path,
        adif_path: &std::path::Path,
        context: &ContestScoringContext,
        operator: &str,
    ) -> Result<Vec<ContestContactFact>, String> {
        let sig = tokio::fs::metadata(adif_path)
            .await
            .ok()
            .and_then(|m| m.modified().ok().map(|t| (m.len(), t)));
        self.pending.extend(
            manager
                .completed_contest_contact_ids()
                .await
                .into_iter()
                .filter(|id| !self.observed.contains(id)),
        );
        let expected: Vec<_> = self.pending.iter().cloned().collect();
        let missing = self.cached.as_ref().is_none_or(|facts| {
            expected
                .iter()
                .any(|id| !facts.iter().any(|f| &f.contact_id == id))
        });
        if self.cached.is_none()
            || missing
            || sig != self.signature
            || self.last_read.elapsed() >= Duration::from_secs(30)
        {
            self.signature = sig;
            self.last_read = Instant::now();
            match read_history(db_path, adif_path, context, operator, &expected).await {
                Ok(facts) => self.cached = Some(facts),
                Err(error) => {
                    self.cached = None;
                    return Err(error);
                }
            }
        }
        if let Some(facts) = &self.cached {
            self.observed
                .extend(facts.iter().map(|f| f.contact_id.clone()));
            self.pending.retain(|id| !self.observed.contains(id));
        }
        self.cached
            .clone()
            .ok_or_else(|| "index unavailable".into())
    }
}

pub(super) fn start(
    manager: QsoManager,
    bus: MessageBus,
    source: HistorySource,
    shutdown: Arc<AtomicBool>,
) -> Worker {
    Worker(tokio::spawn(async move {
        let Some(context) = manager.config().contest_scoring.clone() else {
            return;
        };
        let operator = manager.config().our_callsign.clone();
        let name = context
            .rules()
            .map(|r| r.display_name)
            .unwrap_or("Contest")
            .to_string();
        // Shared sequence survives component restarts and wall-clock corrections.
        static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut live = LiveRefresh::new();
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        let mut first = true;
        let mut warned = None::<String>;
        loop {
            tick.tick().await;
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            let now = chrono::Utc::now();
            let state = if first {
                first = false;
                ContestScoreState::Loading
            } else {
                // PAN-268: replay scores only the prepared in-memory seed.
                // It never polls ADIF metadata, opens the station index or
                // waits for simulated completions to reach the log writers
                // replay disabled.
                let history = match &source {
                    HistorySource::Live { db_path, adif_path } => {
                        live.refresh(&manager, db_path, adif_path, &context, &operator)
                            .await
                    }
                    HistorySource::Replay { seed } => match seed {
                        Ok(db) => db
                            .contest_ledger(&context, &operator)
                            .await
                            .map_err(|e| e.to_string()),
                        Err(message) => Err(message.clone()),
                    },
                };
                match &history {
                    Ok(_) => warned = None,
                    Err(error) => {
                        if warned.as_ref() != Some(error) {
                            tracing::warn!(%error, "Contest score unavailable");
                        }
                        warned = Some(error.clone());
                    }
                }
                match history {
                    Ok(facts) => match reduce_contest_score(&context, &facts, now) {
                        Ok(snapshot) => ContestScoreState::Ready(snapshot),
                        Err(_) => {
                            ContestScoreState::Unavailable("contest rules are unsupported".into())
                        }
                    },
                    Err(error) => ContestScoreState::Unavailable(
                        if error.contains("unsupported") || error.contains("unknown rules_id") {
                            "contest rules are unsupported".into()
                        } else {
                            "contest history could not be read".into()
                        },
                    ),
                }
            };
            let update = ContestScoreUpdate {
                generation: GENERATION.fetch_add(1, Ordering::Relaxed),
                instance_id: context.instance_id.clone(),
                rules_id: context.rules_id.clone(),
                display_name: name.clone(),
                outside_period: !context.in_period(now),
                state,
            };
            let _ = bus
                .send_message(ComponentMessage::new(
                    ComponentId::Qso,
                    ComponentId::Tui,
                    MessageType::ContestScore(update),
                    Instant::now(),
                ))
                .await;
        }
    }))
}

async fn read_history(
    db_path: &std::path::Path,
    adif_path: &std::path::Path,
    context: &ContestScoringContext,
    operator: &str,
    expected: &[String],
) -> Result<Vec<ContestContactFact>, String> {
    // Opening an existing prepared index never manufactures an empty history.
    if !tokio::fs::try_exists(db_path)
        .await
        .map_err(|e| e.to_string())?
    {
        return Err("index unavailable".into());
    }
    let db = pancetta_qso::async_database::QsoDatabase::open(db_path)
        .await
        .map_err(|e| e.to_string())?;
    let indexed = db
        .contest_ledger(context, operator)
        .await
        .map_err(|e| e.to_string())?;
    let raw = tokio::fs::read_to_string(adif_path)
        .await
        .map_err(|e| e.to_string())?;
    let ending = raw.trim_end().to_ascii_uppercase();
    if !ending.is_empty() && !ending.ends_with("<EOR>") && !ending.ends_with("<EOH>") {
        return Err("incomplete ADIF write".into());
    }
    let context = context.clone();
    let operator = operator.to_string();
    let durable =
        tokio::task::spawn_blocking(move || -> Result<Vec<ContestContactFact>, String> {
            let p = pancetta_qso::adif::AdifProcessor::new();
            let file = p.parse_string(&raw).map_err(|e| e.to_string())?;
            let mut facts = Vec::new();
            for record in file.records {
                let adif = p.record_to_qso(&record).map_err(|e| e.to_string())?;
                if adif.additional_fields.get(INSTANCE) != Some(&context.instance_id) {
                    continue;
                }
                let metadata = p.adif_to_qso(&adif);
                let row = fact(&metadata, &context, &operator)?;
                if context.in_period(row.start) {
                    facts.push(row);
                }
            }
            Ok(facts)
        })
        .await
        .map_err(|e| e.to_string())??;
    // Neither a completion event nor one successful writer proves durability.
    // A pending/failed write hides numbers until both ledgers agree.
    for id in expected {
        if !indexed.iter().any(|f| &f.contact_id == id)
            || !durable.iter().any(|f| &f.contact_id == id)
        {
            return Err("contest write pending".into());
        }
    }
    if indexed.len() != durable.len() || indexed.iter().any(|f| !durable.contains(f)) {
        return Err("contest index differs from durable log".into());
    }
    Ok(durable)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD_START: &str = "2026-08-29T00:00:00Z";
    const PERIOD_END: &str = "2026-12-31T00:00:00Z";
    const SENTINEL_INDEX: &[u8] = b"not a sqlite station index";

    fn context() -> ContestScoringContext {
        ContestScoringContext::new(
            "test",
            "us-state-qso-party",
            KSQP_FT_2026,
            "TX",
            vec![ContestPeriod {
                start: PERIOD_START.parse().unwrap(),
                end: PERIOD_END.parse().unwrap(),
            }],
        )
        .unwrap()
    }

    fn contest_manager(context: &ContestScoringContext) -> QsoManager {
        let mut manager = pancetta_qso::QsoManager::new(pancetta_qso::QsoManagerConfig {
            our_callsign: "K5ARH".into(),
            contest_scoring: Some(context.clone()),
            ..Default::default()
        });
        manager.require_persistent_history();
        manager.set_dial_frequency_source(Arc::new(std::sync::atomic::AtomicU64::new(14_074_000)));
        manager
    }

    /// One logged contest contact matching `context`, exactly like the
    /// durable-history fixtures above.
    fn logged_adif() -> String {
        let logged = serde_json::json!({
            "version": 1, "profile_id": "us-state-qso-party",
            "operator_callsign": "K5ARH", "operator_region": "TX",
            "periods": [{"start": PERIOD_START, "end": PERIOD_END}],
            "evidence": {"their_region": null, "their_county": null, "our_county": null, "source": "import"}
        })
        .to_string();
        format!(
            "<CALL:4>K0KS<QSO_DATE:8>20260829<TIME_ON:6>143000<FREQ:6>14.074<MODE:3>FT8<STATION_CALLSIGN:5>K5ARH<STATE:2>KS<CNTY:5>KS,SB<APP_PANCETTA_CONTEST_INSTANCE:4>test<APP_PANCETTA_CONTEST_RULES:12>ksqp-ft-2026<APP_PANCETTA_CONTEST_CONTACT_ID:36>9b16f2be-7d63-4a89-b789-f863df27bccf<APP_PANCETTA_CONTEST_CONTEXT:{}>{logged}<EOR>",
            logged.len()
        )
    }

    /// Next ContestScore update on the Tui channel, bounded. The worker
    /// runs on the multi-thread test runtime; the blocking crossbeam
    /// receive parks on the blocking pool so it never starves the worker.
    async fn next_score(rx: &crossbeam_channel::Receiver<ComponentMessage>) -> ContestScoreUpdate {
        loop {
            let rx = rx.clone();
            let message =
                tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(30)))
                    .await
                    .expect("receiver task alive")
                    .expect("contest score update within 30s");
            if let MessageType::ContestScore(update) = message.message_type {
                return update;
            }
        }
    }

    struct Harness {
        manager: QsoManager,
        rx: crossbeam_channel::Receiver<ComponentMessage>,
        shutdown: Arc<AtomicBool>,
        _worker: Worker,
    }

    async fn spawn_worker(
        prepared: &Result<Arc<QsoDatabase>, pancetta_qso::async_database::AsyncDatabaseError>,
        context: &ContestScoringContext,
        db_path: std::path::PathBuf,
        adif_path: std::path::PathBuf,
    ) -> Harness {
        let mut manager = contest_manager(context);
        // Same wiring as production replay startup: QSO admission shares the
        // prepared seed (initialize_qso_history attaches it).
        if let Ok(db) = &prepared {
            manager.attach_database(db.clone());
        }
        let bus = MessageBus::new(64).unwrap();
        let (_tx, rx) = bus.create_channel(ComponentId::Tui).await.unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = start(
            manager.clone(),
            bus,
            select_history_source(true, prepared, db_path, adif_path),
            shutdown.clone(),
        );
        Harness {
            manager,
            rx,
            shutdown,
            _worker: worker,
        }
    }

    fn dir_entries(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scores_the_prepared_seed_without_creating_a_station_index() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("qso.db");
        let adif_path = dir.path().join("qsos.adi");
        std::fs::write(&adif_path, logged_adif()).unwrap();
        let prepared = super::super::qso::replay_history_db(&adif_path).await;
        assert!(prepared.is_ok());
        let harness = spawn_worker(&prepared, &context(), db_path.clone(), adif_path.clone()).await;
        assert_eq!(
            next_score(&harness.rx).await.state,
            ContestScoreState::Loading
        );
        let update = next_score(&harness.rx).await;
        let ContestScoreState::Ready(snapshot) = update.state else {
            panic!("seeded replay history must score, got {:?}", update.state)
        };
        assert_eq!(snapshot.scored, 1);
        assert_eq!(snapshot.score, 1);
        harness.shutdown.store(true, Ordering::Relaxed);
        drop(harness);
        assert!(
            !db_path.exists(),
            "replay must never create the real station index"
        );
        assert_eq!(dir_entries(dir.path()), vec!["qsos.adi".to_string()]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn never_opens_a_sentinel_station_index() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("qso.db");
        let adif_path = dir.path().join("qsos.adi");
        let adif = logged_adif();
        std::fs::write(&adif_path, &adif).unwrap();
        // A sentinel detects an accidental open even when tests run as
        // root and permission bits are useless: a write-mode SQLite open
        // of these bytes fails (score stops being Ready) or rewrites them.
        std::fs::write(&db_path, SENTINEL_INDEX).unwrap();
        let prepared = super::super::qso::replay_history_db(&adif_path).await;
        assert!(prepared.is_ok());
        let harness = spawn_worker(&prepared, &context(), db_path.clone(), adif_path.clone()).await;
        assert_eq!(
            next_score(&harness.rx).await.state,
            ContestScoreState::Loading
        );
        let update = next_score(&harness.rx).await;
        assert!(
            matches!(&update.state, ContestScoreState::Ready(s) if s.scored == 1),
            "sentinel station index must never be consulted: {:?}",
            update.state
        );
        harness.shutdown.store(true, Ordering::Relaxed);
        drop(harness);
        assert_eq!(std::fs::read(&db_path).unwrap(), SENTINEL_INDEX);
        assert_eq!(std::fs::read(&adif_path).unwrap(), adif.as_bytes());
        assert_eq!(
            dir_entries(dir.path()),
            vec!["qso.db".to_string(), "qsos.adi".to_string()]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scores_an_empty_seed_when_adif_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("qso.db");
        let adif_path = dir.path().join("qsos.adi");
        // No ADIF: replay preparation yields a valid empty in-memory seed.
        let prepared = super::super::qso::replay_history_db(&adif_path).await;
        assert!(prepared.is_ok());
        let harness = spawn_worker(&prepared, &context(), db_path.clone(), adif_path.clone()).await;
        assert_eq!(
            next_score(&harness.rx).await.state,
            ContestScoreState::Loading
        );
        let update = next_score(&harness.rx).await;
        let ContestScoreState::Ready(snapshot) = update.state else {
            panic!("absent ADIF is a valid empty seed, got {:?}", update.state)
        };
        assert_eq!(snapshot.scored, 0);
        assert_eq!(snapshot.score, 0);
        harness.shutdown.store(true, Ordering::Relaxed);
        drop(harness);
        assert!(!db_path.exists());
        assert!(dir_entries(dir.path()).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn simulated_completions_never_change_the_seeded_score() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("qso.db");
        let adif_path = dir.path().join("qsos.adi");
        std::fs::write(&adif_path, logged_adif()).unwrap();
        let prepared = super::super::qso::replay_history_db(&adif_path).await;
        assert!(prepared.is_ok());
        let harness = spawn_worker(&prepared, &context(), db_path.clone(), adif_path.clone()).await;
        assert_eq!(
            next_score(&harness.rx).await.state,
            ContestScoreState::Loading
        );
        let ready = next_score(&harness.rx).await;
        let ContestScoreState::Ready(seeded) = ready.state else {
            panic!("seeded replay history must score, got {ready:?}")
        };
        assert_eq!(seeded.scored, 1);
        // Complete a simulated contest QSO through the manager. Replay's
        // log writers are suppressed, so the score must stay exactly the
        // seeded one — never a write-pending wait for the new contact.
        harness
            .manager
            .respond_to_caller(
                "K0KS".into(),
                14_074_000.0,
                None,
                pancetta_core::ResponseStep::SeventyThree,
                None,
                None,
                None,
                false,
                None,
            )
            .await
            .unwrap();
        for _ in 0..2 {
            let after = next_score(&harness.rx).await;
            assert!(after.generation > ready.generation);
            match after.state {
                ContestScoreState::Ready(snapshot) => assert_eq!(
                    snapshot, seeded,
                    "simulated completions add no durable replay score"
                ),
                other => panic!("replay score must stay Ready, got {other:?}"),
            }
        }
        harness.shutdown.store(true, Ordering::Relaxed);
        drop(harness);
        assert!(!db_path.exists());
        assert_eq!(dir_entries(dir.path()), vec!["qsos.adi".to_string()]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_seed_is_unavailable_without_a_disk_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("qso.db");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"regular file").unwrap();
        // ADIF existence probe under a regular file fails, so replay
        // preparation itself fails.
        let adif_path = dir.path().join("blocker").join("qsos.adi");
        let prepared = super::super::qso::replay_history_db(&adif_path).await;
        assert!(prepared.is_err());
        std::fs::write(&db_path, SENTINEL_INDEX).unwrap();
        let harness = spawn_worker(&prepared, &context(), db_path.clone(), adif_path.clone()).await;
        assert_eq!(
            next_score(&harness.rx).await.state,
            ContestScoreState::Loading
        );
        let update = next_score(&harness.rx).await;
        assert_eq!(
            update.state,
            ContestScoreState::Unavailable("contest history could not be read".into())
        );
        harness.shutdown.store(true, Ordering::Relaxed);
        drop(harness);
        assert_eq!(std::fs::read(&db_path).unwrap(), SENTINEL_INDEX);
        assert_eq!(
            dir_entries(dir.path()),
            vec!["blocker".to_string(), "qso.db".to_string()]
        );
    }

    #[tokio::test]
    async fn freq_without_band_history_still_scores_after_rebuild() {
        use pancetta_qso::async_database::QsoDatabase;
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("qso.db");
        let adif_path = dir.path().join("qsos.adi");
        let context = ContestScoringContext::new(
            "test",
            "us-state-qso-party",
            KSQP_FT_2026,
            "TX",
            vec![ContestPeriod {
                start: "2026-08-29T14:00:00Z".parse().unwrap(),
                end: "2026-08-30T02:00:00Z".parse().unwrap(),
            }],
        )
        .unwrap();
        // ADIF permits FREQ with no BAND. The index rebuild derives the band
        // from the frequency (qso_to_adif); the durable reader must apply the
        // same normalization or the cross-check rejects the history forever.
        let logged = serde_json::json!({
            "version": 1, "profile_id": "us-state-qso-party",
            "operator_callsign": "K5ARH", "operator_region": "TX",
            "periods": [{"start": "2026-08-29T14:00:00Z", "end": "2026-08-30T02:00:00Z"}],
            "evidence": {"their_region": null, "their_county": null, "our_county": null, "source": "import"}
        })
        .to_string();
        let raw = format!(
            "<CALL:4>K0KS<QSO_DATE:8>20260829<TIME_ON:6>143000<FREQ:6>14.074<MODE:3>FT8<STATION_CALLSIGN:5>K5ARH<STATE:2>KS<CNTY:5>KS,SB<APP_PANCETTA_CONTEST_INSTANCE:4>test<APP_PANCETTA_CONTEST_RULES:12>ksqp-ft-2026<APP_PANCETTA_CONTEST_CONTACT_ID:36>9b16f2be-7d63-4a89-b789-f863df27bccf<APP_PANCETTA_CONTEST_CONTEXT:{}>{logged}<EOR>",
            logged.len()
        );
        tokio::fs::write(&adif_path, &raw).await.unwrap();
        QsoDatabase::replay_from_adif(&db_path, &adif_path)
            .await
            .unwrap();
        let facts = read_history(&db_path, &adif_path, &context, "K5ARH", &[])
            .await
            .unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].band.as_deref(), Some("20M"));
    }

    #[tokio::test]
    async fn persisted_history_requires_both_writers_and_recovers_after_rebuild_and_undo() {
        use pancetta_qso::{adif::AdifProcessor, async_database::QsoDatabase};
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("qso.db");
        let adif_path = dir.path().join("qsos.adi");
        let context = ContestScoringContext::new(
            "test",
            "us-state-qso-party",
            KSQP_FT_2026,
            "TX",
            vec![ContestPeriod {
                start: "2026-08-29T14:00:00Z".parse().unwrap(),
                end: "2026-08-30T02:00:00Z".parse().unwrap(),
            }],
        )
        .unwrap();
        let p = AdifProcessor::new();
        let file = p.parse_string("<CALL:4>K0KS<QSO_DATE:8>20260829<TIME_ON:6>143000<FREQ:6>14.074<MODE:3>FT8<STATION_CALLSIGN:5>K5ARH<STATE:2>KS<CNTY:5>KS,SB<EOR>").unwrap();
        let mut metadata = p.adif_to_qso(&p.record_to_qso(&file.records[0]).unwrap());
        attach(&mut metadata, &context);
        let id = metadata.tags[CONTACT_ID].clone();
        let text = p
            .generate_record(&p.qso_to_adif(&metadata, metadata.contest_info.as_ref()))
            .unwrap();
        tokio::fs::write(&adif_path, &text).await.unwrap();
        assert!(
            read_history(&db_path, &adif_path, &context, "K5ARH", &[id.clone()])
                .await
                .is_err()
        );
        let db = QsoDatabase::replay_from_adif(&db_path, &adif_path)
            .await
            .unwrap();
        drop(db);
        let first = read_history(&db_path, &adif_path, &context, "K5ARH", &[id])
            .await
            .unwrap();
        assert_eq!(
            reduce_contest_score(&context, &first, context.periods[0].end)
                .unwrap()
                .score,
            1
        );
        // A failed/partial ADIF append hides all old metrics.
        tokio::fs::write(&adif_path, format!("{text}<CALL:"))
            .await
            .unwrap();
        assert!(read_history(&db_path, &adif_path, &context, "K5ARH", &[])
            .await
            .is_err());
        tokio::fs::write(&adif_path, &text).await.unwrap();
        assert_eq!(
            first,
            read_history(&db_path, &adif_path, &context, "K5ARH", &[])
                .await
                .unwrap()
        );
        // Undo removes the row from both authoritative evidence and index.
        tokio::fs::write(&adif_path, "").await.unwrap();
        let db = QsoDatabase::replay_from_adif(&db_path, &adif_path)
            .await
            .unwrap();
        drop(db);
        assert!(read_history(&db_path, &adif_path, &context, "K5ARH", &[])
            .await
            .unwrap()
            .is_empty());
    }
}
