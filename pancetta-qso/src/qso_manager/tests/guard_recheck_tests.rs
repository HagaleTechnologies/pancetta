// PAN-138. Formerly the inline module `pan138_recheck_tests`.
//! PAN-138: re-keying an EXISTING QSO for a (possibly remote) caller
//! re-checks TX permission under the same `qsos` write guard that rebinds
//! the QSO and charges the resend (or advances it). The permission source
//! doubles as a probe: when primed, its next evaluation tries to take the
//! `qsos` write lock itself, standing in for concurrent QSO processing
//! that grabs the lock right after the check. If it gets the lock, the
//! check ran outside the critical section and a disarm could land between
//! the check and the charge.
use super::*;
use pancetta_core::slot::SlotParity;
use pancetta_core::ResponseStep;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

const DX: &str = "K9ZZ";
const FREQ: f64 = 1500.0;

type ParkedGuard = tokio::sync::OwnedRwLockWriteGuard<HashMap<QsoId, QsoProgress>>;

#[derive(Default)]
struct Probe {
    permitted: AtomicBool,
    primed: AtomicBool,
    interposed: AtomicBool,
    checked_under_lock: AtomicBool,
    parked: std::sync::Mutex<Option<ParkedGuard>>,
}

fn manager_with_probe() -> (Arc<QsoManager>, Arc<Probe>) {
    let mut manager = QsoManager::new(QsoManagerConfig {
        our_callsign: "K5ARH".into(),
        our_grid: Some("EM12".into()),
        ..QsoManagerConfig::default()
    });
    let probe = Arc::new(Probe::default());
    probe.permitted.store(true, SeqCst);
    let qsos = Arc::clone(&manager.qsos);
    let p = Arc::clone(&probe);
    manager.set_remote_tx_permitted_source(Arc::new(move |_| {
        let permitted = p.permitted.load(SeqCst);
        if p.primed.swap(false, SeqCst) {
            match Arc::clone(&qsos).try_write_owned() {
                Ok(guard) => {
                    *p.parked.lock().unwrap() = Some(guard);
                    p.interposed.store(true, SeqCst);
                }
                Err(_) => p.checked_under_lock.store(true, SeqCst),
            }
        }
        permitted
    }));
    (Arc::new(manager), probe)
}

/// Run `action` with the probe primed. The client disarms as soon as the
/// probe has interposed (or once `action` is done), then the parked guard
/// is released so `action` can finish.
async fn race<T: Send + 'static>(
    probe: &Probe,
    action: impl Future<Output = T> + Send + 'static,
) -> T {
    probe.primed.store(true, SeqCst);
    let task = tokio::spawn(action);
    while !probe.interposed.load(SeqCst) && !task.is_finished() {
        tokio::task::yield_now().await;
    }
    probe.permitted.store(false, SeqCst);
    drop(probe.parked.lock().unwrap().take());
    task.await.unwrap()
}

fn assert_check_shares_the_guard(probe: &Probe) {
    assert!(
        !probe.interposed.load(SeqCst),
        "another task took the qsos write lock between the permission \
         check and the charge, so a disarm there would still be charged"
    );
    assert!(
        probe.checked_under_lock.load(SeqCst),
        "the permission check must run under the qsos write guard"
    );
}

async fn open(manager: &QsoManager, step: ResponseStep, client: &str) -> QsoId {
    manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            Some(SlotParity::Even),
            step,
            None,
            None,
            None,
            true,
            Some(client.into()),
        )
        .await
        .unwrap()
}

fn reply(
    manager: &Arc<QsoManager>,
    step: ResponseStep,
    client: &str,
) -> impl Future<Output = Result<QsoId, QsoManagerError>> + Send + 'static {
    let manager = Arc::clone(manager);
    let client = client.to_string();
    async move {
        manager
            .respond_to_caller(
                DX.into(),
                FREQ,
                Some(SlotParity::Even),
                step,
                None,
                None,
                None,
                true,
                Some(client),
            )
            .await
    }
}

#[tokio::test]
async fn keep_call_recheck_shares_the_charge_guard() {
    let (manager, probe) = manager_with_probe();
    let id = manager
        .respond_to_cq_with(
            DX.into(),
            FREQ,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            true,
            Some("client-a".into()),
        )
        .await
        .unwrap();
    let m = Arc::clone(&manager);
    let again = race(&probe, async move {
        m.respond_to_cq_with(
            DX.into(),
            FREQ,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            true,
            Some("client-b".into()),
        )
        .await
    })
    .await
    .unwrap();
    assert_eq!(again, id);
    assert_check_shares_the_guard(&probe);
    let after = manager.get_qso(id).await.unwrap();
    assert_eq!(after.metadata.call_count, 2);
    assert_eq!(
        after.metadata.remote_client_key_id.as_deref(),
        Some("client-b")
    );
}

#[tokio::test]
async fn idempotent_reply_recheck_shares_the_charge_guard() {
    let (manager, probe) = manager_with_probe();
    let id = open(&manager, ResponseStep::Report, "client-a").await;
    let again = race(&probe, reply(&manager, ResponseStep::Report, "client-b"))
        .await
        .unwrap();
    assert_eq!(again, id);
    assert_check_shares_the_guard(&probe);
    assert_eq!(manager.get_qso(id).await.unwrap().metadata.call_count, 2);
}

#[tokio::test]
async fn completed_qso_rekey_recheck_shares_the_charge_guard() {
    let (manager, probe) = manager_with_probe();
    let id = open(&manager, ResponseStep::SeventyThree, "client-a").await;
    let again = race(
        &probe,
        reply(&manager, ResponseStep::SeventyThree, "client-a"),
    )
    .await
    .unwrap();
    assert_eq!(again, id);
    assert_check_shares_the_guard(&probe);
    assert_eq!(manager.get_qso(id).await.unwrap().metadata.call_count, 2);
}

#[tokio::test]
async fn advance_recheck_shares_the_mutation_guard() {
    let (manager, probe) = manager_with_probe();
    let id = open(&manager, ResponseStep::Report, "client-a").await;
    let again = race(
        &probe,
        reply(&manager, ResponseStep::SeventyThree, "client-b"),
    )
    .await
    .unwrap();
    assert_eq!(again, id);
    assert_check_shares_the_guard(&probe);
    assert!(matches!(
        manager.get_qso(id).await.unwrap().state,
        QsoState::Completed { .. }
    ));
}
