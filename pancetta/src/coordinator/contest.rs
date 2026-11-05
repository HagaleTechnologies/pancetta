//! Durable contest refresh, independent of UI presence and TX timing.
use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};
use pancetta_qso::{
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

pub(super) fn start(
    manager: QsoManager,
    bus: MessageBus,
    db_path: PathBuf,
    adif_path: PathBuf,
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
        let mut observed = std::collections::HashSet::new();
        let mut pending = std::collections::BTreeSet::new();
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        let mut cached = None::<Vec<ContestContactFact>>;
        let mut signature = None;
        let mut last_read = Instant::now() - Duration::from_secs(30);
        let mut first = true;
        let mut history_error = None::<String>;
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
                let sig = tokio::fs::metadata(&adif_path)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok().map(|t| (m.len(), t)));
                pending.extend(
                    manager
                        .completed_contest_contact_ids()
                        .await
                        .into_iter()
                        .filter(|id| !observed.contains(id)),
                );
                let expected: Vec<_> = pending.iter().cloned().collect();
                let missing = cached.as_ref().is_none_or(|facts| {
                    expected
                        .iter()
                        .any(|id| !facts.iter().any(|f| &f.contact_id == id))
                });
                if cached.is_none()
                    || missing
                    || sig != signature
                    || last_read.elapsed() >= Duration::from_secs(30)
                {
                    signature = sig;
                    last_read = Instant::now();
                    match read_history(&db_path, &adif_path, &context, &operator, &expected).await {
                        Ok(facts) => {
                            cached = Some(facts);
                            history_error = None;
                        }
                        Err(error) => {
                            if history_error.as_ref() != Some(&error) {
                                tracing::warn!(%error, "Contest score unavailable");
                            }
                            cached = None;
                            history_error = Some(error);
                        }
                    }
                }
                if let Some(facts) = &cached {
                    observed.extend(facts.iter().map(|f| f.contact_id.clone()));
                    pending.retain(|id| !observed.contains(id));
                }
                match &cached {
                    Some(facts) => match reduce_contest_score(&context, facts, now) {
                        Ok(snapshot) => ContestScoreState::Ready(snapshot),
                        Err(_) => {
                            ContestScoreState::Unavailable("contest rules are unsupported".into())
                        }
                    },
                    None => ContestScoreState::Unavailable(
                        if history_error.as_ref().is_some_and(|e| {
                            e.contains("unsupported") || e.contains("unknown rules_id")
                        }) {
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
