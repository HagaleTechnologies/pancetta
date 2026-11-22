//! PAN-76: `maybe_answer_caller` records the QSOs it opens under Fox
//! mode's raised cap in `fox_admitted_qsos`, and nothing else.
use super::maybe_answer_caller;
use crate::message_bus::MessageBus;
use pancetta_core::slot::SlotParity;
use pancetta_core::TxPolicy;
use pancetta_qso::states::MessageType as Mt;
use pancetta_qso::{QsoManager, QsoManagerConfig};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize};
use std::sync::RwLock;

const OUR: &str = "K5ARH";

async fn manager() -> QsoManager {
    let m = QsoManager::new(QsoManagerConfig {
        our_callsign: OUR.to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    });
    m.start().await.expect("manager start");
    m
}

fn caller(call: &str) -> Mt {
    Mt::CqResponse {
        calling_station: OUR.to_string(),
        responding_station: call.to_string(),
        grid: Some("FN31".to_string()),
    }
}

struct Station {
    manager: QsoManager,
    bus: MessageBus,
    tx_policy: AtomicU8,
    fox_mode: AtomicBool,
    fox_max_streams: AtomicUsize,
    fox_admitted_qsos: RwLock<HashSet<String>>,
}

impl Station {
    async fn new(fox_mode: bool, fox_max_streams: usize) -> Self {
        Self {
            manager: manager().await,
            bus: MessageBus::new(1000).expect("bus"),
            tx_policy: AtomicU8::new(TxPolicy::Full.as_u8()),
            fox_mode: AtomicBool::new(fox_mode),
            fox_max_streams: AtomicUsize::new(fox_max_streams),
            fox_admitted_qsos: RwLock::new(HashSet::new()),
        }
    }

    async fn answer(&self, call: &str, freq: f64, max_concurrent: usize) {
        self.answer_msg(&caller(call), freq, max_concurrent).await;
    }

    async fn answer_msg(&self, msg: &Mt, freq: f64, max_concurrent: usize) {
        maybe_answer_caller(
            msg,
            OUR,
            freq,
            Some(SlotParity::Even),
            -10.0,
            &self.manager,
            &self.tx_policy,
            pancetta_config::UnattendedPolicy::Off,
            true,
            max_concurrent,
            &self.bus,
            &self.fox_mode,
            &self.fox_max_streams,
            &self.fox_admitted_qsos,
        )
        .await;
    }

    async fn key_of(&self, call: &str) -> String {
        let (id, _) = self
            .manager
            .get_active_qsos()
            .await
            .into_iter()
            .find(|(_, p)| p.metadata.their_callsign.as_deref() == Some(call))
            .unwrap_or_else(|| panic!("{call} must have an active QSO"));
        crate::coordinator::active_tx_qso_key(&id.to_string())
    }

    fn fox_set(&self) -> HashSet<String> {
        self.fox_admitted_qsos.read().unwrap().clone()
    }
}

#[tokio::test]
async fn pan76_auto_answer_in_fox_mode_records_the_hound() {
    let st = Station::new(true, 5).await;
    st.answer("W1AAA", 1200.0, 1).await;
    let key = st.key_of("W1AAA").await;
    assert_eq!(st.fox_set(), HashSet::from([key]));
}

#[tokio::test]
async fn pan76_auto_answer_outside_fox_mode_records_nothing() {
    let st = Station::new(false, 5).await;
    st.answer("W1AAA", 1200.0, 1).await;
    assert_eq!(st.manager.active_qso_count().await, 1);
    assert!(st.fox_set().is_empty());
}

#[tokio::test]
async fn pan76_refused_auto_answer_records_nothing() {
    let st = Station::new(true, 1).await;
    st.answer("W1AAA", 1200.0, 1).await;
    st.answer("W2BBB", 1600.0, 1).await;
    assert_eq!(
        st.manager.active_caller_qso_count().await,
        1,
        "W2BBB must be refused at the Fox capacity of 1"
    );
    let key = st.key_of("W1AAA").await;
    assert_eq!(st.fox_set(), HashSet::from([key]));
}

/// PR #421 review (Codex P2, rounds 2 and 4): Hounds that open already
/// complete (their RR73 straight to us) hold a Fox-admitted TX stream for
/// their closing 73 through the 45 s grace, so they fill Fox capacity, and
/// they count the moment they open. This station has no event forwarder
/// at all, which is the same decode batch draining before it runs.
#[tokio::test]
async fn pan76_hounds_in_their_completed_grace_fill_the_fox_cap() {
    let st = Station::new(true, 2).await;
    for (call, freq) in [("W1AAA", 1200.0), ("W2BBB", 1600.0)] {
        st.answer_msg(
            &Mt::FinalConfirmation {
                to_station: OUR.to_string(),
                from_station: call.to_string(),
            },
            freq,
            1,
        )
        .await;
    }
    assert_eq!(
        st.fox_set().len(),
        2,
        "both completed Hounds are Fox-admitted"
    );
    assert_eq!(st.manager.active_caller_qso_count().await, 0);

    st.answer("W3CCC", 2000.0, 1).await;
    assert_eq!(
        st.manager.active_caller_qso_count().await,
        0,
        "W3CCC must be refused: two grace-period Hounds fill the Fox cap of 2"
    );
    assert_eq!(st.fox_set().len(), 2);
}

/// Fox-set keys are never pruned, so a stale one (a QSO that failed or
/// was cancelled, gone from the manager, or not a QSO id at all) must
/// not hold Fox capacity.
#[tokio::test]
async fn pan76_stale_fox_keys_do_not_fill_the_fox_cap() {
    let st = Station::new(true, 1).await;
    {
        let mut set = st.fox_admitted_qsos.write().unwrap();
        set.insert(crate::coordinator::active_tx_qso_key(
            &uuid::Uuid::new_v4().to_string(),
        ));
        set.insert("NOT-A-QSO-ID".to_string());
    }
    st.answer("W1AAA", 1200.0, 1).await;
    assert_eq!(
        st.manager.active_caller_qso_count().await,
        1,
        "W1AAA must be admitted: no stale key counts against the cap of 1"
    );
}
