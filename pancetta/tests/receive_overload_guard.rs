//! Offline acceptance tests drive the production policy and CAT transaction.
#![cfg(feature = "pancetta-hamlib")]
use anyhow::{bail, Result};
use async_trait::async_trait;
use pancetta_hamlib::{Mode, PttState, RigControl, RigStatus, Vfo};
use pancetta_lib::coordinator::receive_overload::{
    execute_step, GainSafety, Identity, Measurement, OverloadPolicy, StepOutcome, WindowValidity,
};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use tokio::time::{Duration, Instant};

#[derive(Default)]
struct RecordingRig {
    att: AtomicI32,
    writes: AtomicUsize,
    keyed: AtomicBool,
    disconnected: AtomicBool,
    mismatch: bool,
    fail_set: bool,
    slow_set: bool,
    slow_read: bool,
    set_barrier: Option<std::sync::Arc<tokio::sync::Barrier>>,
}

#[async_trait]
impl RigControl for RecordingRig {
    async fn connect(&self) -> Result<()> {
        Ok(())
    }
    async fn disconnect(&self) -> Result<()> {
        self.disconnected.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn get_attenuator(&self) -> Result<i32> {
        if self.slow_read {
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        Ok(self.att.load(Ordering::SeqCst))
    }
    async fn set_attenuator(&self, db: i32) -> Result<()> {
        if let Some(barrier) = &self.set_barrier {
            barrier.wait().await;
            barrier.wait().await;
        }
        assert!(!self.keyed.load(Ordering::SeqCst), "ATT during hardware TX");
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.fail_set {
            bail!("set failed");
        }
        if self.slow_set {
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        if !self.mismatch {
            self.att.store(db, Ordering::SeqCst);
        }
        Ok(())
    }
    async fn get_ptt(&self, _: Vfo) -> Result<PttState> {
        Ok(if self.keyed.load(Ordering::SeqCst) {
            PttState::On
        } else {
            PttState::Off
        })
    }
    async fn set_ptt(&self, _: Vfo, state: PttState) -> Result<()> {
        self.keyed.store(state != PttState::Off, Ordering::SeqCst);
        Ok(())
    }
    async fn get_status(&self) -> Result<RigStatus> {
        unimplemented!()
    }
    async fn set_frequency(&self, _: Vfo, _: u64) -> Result<()> {
        unimplemented!()
    }
    async fn get_frequency(&self, _: Vfo) -> Result<u64> {
        unimplemented!()
    }
    async fn set_mode(&self, _: Vfo, _: Mode, _: Option<i32>) -> Result<()> {
        unimplemented!()
    }
    async fn get_mode(&self, _: Vfo) -> Result<(Mode, i32)> {
        unimplemented!()
    }
    async fn set_vfo(&self, _: Vfo) -> Result<()> {
        unimplemented!()
    }
    async fn get_vfo(&self) -> Result<Vfo> {
        unimplemented!()
    }
    async fn set_power_level(&self, _: f32) -> Result<()> {
        unimplemented!()
    }
    async fn get_power_level(&self) -> Result<f32> {
        unimplemented!()
    }
    async fn get_s_meter(&self) -> Result<i32> {
        unimplemented!()
    }
    async fn get_swr(&self) -> Result<f32> {
        unimplemented!()
    }
    async fn set_memory_channel(&self, _: Vfo, _: i32) -> Result<()> {
        unimplemented!()
    }
    async fn get_memory_channel(&self, _: Vfo) -> Result<i32> {
        unimplemented!()
    }
    async fn set_scan(&self, _: Vfo, _: bool) -> Result<()> {
        unimplemented!()
    }
    async fn get_info(&self) -> Result<String> {
        unimplemented!()
    }
}

fn measurement(slot: i64, peak_dbfs: f64) -> Measurement {
    Measurement {
        slot,
        identity: Identity {
            dial_hz: 7_074_000,
            connection_generation: 1,
            gain_epoch: 0,
            capture_epoch: 0,
        },
        peak_dbfs: Some(peak_dbfs),
        validity: WindowValidity::Receive,
    }
}

fn hot_policy() -> OverloadPolicy {
    let mut policy = OverloadPolicy::default();
    for slot in 0..3 {
        policy.observe(measurement(slot, -0.5));
    }
    policy
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(1)
}

#[tokio::test(flavor = "current_thread")]
async fn three_hot_40m_windows_apply_one_supported_step_at_admitted_boundary() {
    let mut policy = hot_policy();
    let pending = policy.pending().unwrap();
    assert_eq!(pending.identity.dial_hz, 7_074_000);
    let rig = RecordingRig::default();
    let safety = GainSafety::default();
    safety.confirmed_off(0);
    // Measurement itself has not changed the rig. Boundary owner reserves later.
    assert_eq!(rig.writes.load(Ordering::SeqCst), 0);
    assert!(safety.reserve());
    assert_eq!(
        execute_step(&rig, &safety, &[0, 10, 20], deadline(), || true)
            .await
            .unwrap(),
        StepOutcome::Increased { old: 0, new: 10 }
    );
    policy.applied();
    assert_eq!(rig.writes.load(Ordering::SeqCst), 1);
    assert!(policy.pending().is_none());
    assert!(safety.receiving());
}

#[tokio::test(flavor = "current_thread")]
async fn three_cool_windows_after_a_step_do_not_add_attenuation() {
    let mut policy = hot_policy();
    let rig = RecordingRig::default();
    let safety = GainSafety::default();
    safety.confirmed_off(0);
    assert!(safety.reserve());
    assert_eq!(
        execute_step(&rig, &safety, &[0, 6, 12], deadline(), || true)
            .await
            .unwrap(),
        StepOutcome::Increased { old: 0, new: 6 }
    );
    policy.applied();
    for slot in 3..6 {
        policy.observe(measurement(slot, -10.0));
    }
    assert!(policy.pending().is_none());
    assert_eq!(rig.writes.load(Ordering::SeqCst), 1);
    assert_eq!(rig.att.load(Ordering::SeqCst), 6);
}

#[tokio::test(flavor = "current_thread")]
async fn transmitting_defers_until_confirmed_receive_boundary() {
    let rig = RecordingRig::default();
    let safety = GainSafety::default();
    safety.confirmed_off(0);
    assert!(safety.admit_ptt(1));
    rig.set_ptt(Vfo::Current, PttState::On).await.unwrap();
    safety.ptt_command_consumed();
    assert!(!safety.reserve());
    assert_eq!(
        execute_step(&rig, &safety, &[0, 6], deadline(), || true)
            .await
            .unwrap(),
        StepOutcome::Deferred
    );
    assert_eq!(rig.writes.load(Ordering::SeqCst), 0);
    // An enqueued unkey alone cannot grant a gain reservation.
    assert!(!safety.reserve());
    rig.set_ptt(Vfo::Current, PttState::Off).await.unwrap();
    safety.confirmed_off(2);
    assert!(safety.reserve());
    assert_eq!(
        execute_step(&rig, &safety, &[0, 6], deadline(), || true)
            .await
            .unwrap(),
        StepOutcome::Increased { old: 0, new: 6 }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn expired_and_superseded_transactions_never_write() {
    for expired in [false, true] {
        let rig = RecordingRig::default();
        let safety = GainSafety::default();
        safety.confirmed_off(0);
        assert!(safety.reserve());
        let limit = if expired {
            Instant::now() - Duration::from_millis(1)
        } else {
            deadline()
        };
        assert_eq!(
            execute_step(&rig, &safety, &[0, 6], limit, || expired)
                .await
                .unwrap(),
            StepOutcome::Deferred
        );
        assert_eq!(rig.writes.load(Ordering::SeqCst), 0);
        assert!(safety.receiving());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn failed_set_or_readback_keeps_tx_inhibited_and_disconnects() {
    for mismatch in [false, true] {
        let rig = RecordingRig {
            mismatch,
            fail_set: !mismatch,
            ..Default::default()
        };
        let safety = GainSafety::default();
        safety.confirmed_off(0);
        assert!(safety.reserve());
        assert!(execute_step(&rig, &safety, &[0, 6], deadline(), || true)
            .await
            .is_err());
        assert!(safety.blocked());
        assert!(!safety.admit_ptt(1));
        assert!(rig.disconnected.load(Ordering::SeqCst));
        safety.confirmed_off(2);
        assert!(safety.blocked(), "T0 alone does not confirm gain");
        safety.recover_gain(3);
        assert!(safety.receiving());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn timeout_after_write_never_reports_success_or_releases_inhibition() {
    let rig = RecordingRig {
        slow_set: true,
        ..Default::default()
    };
    let safety = GainSafety::default();
    safety.confirmed_off(0);
    assert!(safety.reserve());
    assert!(execute_step(
        &rig,
        &safety,
        &[0, 6],
        Instant::now() + Duration::from_millis(5),
        || true
    )
    .await
    .is_err());
    assert!(safety.blocked());
    assert!(rig.disconnected.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "current_thread")]
async fn maximum_supported_attenuation_does_not_send_a_setter() {
    let rig = RecordingRig {
        att: AtomicI32::new(20),
        ..Default::default()
    };
    let safety = GainSafety::default();
    safety.confirmed_off(0);
    assert!(safety.reserve());
    assert_eq!(
        execute_step(&rig, &safety, &[0, 10, 20], deadline(), || true)
            .await
            .unwrap(),
        StepOutcome::Maximum { current: 20 }
    );
    assert_eq!(rig.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn forced_att_first_race_blocks_ptt_until_confirmed_readback() {
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let rig = RecordingRig {
        set_barrier: Some(barrier.clone()),
        ..Default::default()
    };
    let safety = GainSafety::default();
    safety.confirmed_off(0);
    assert!(safety.reserve());
    let transaction = execute_step(&rig, &safety, &[0, 6], deadline(), || true);
    let key_attempt = async {
        barrier.wait().await;
        assert!(safety.blocked());
        assert!(!safety.admit_ptt(1));
        assert!(!safety.hardware_ptt_on(1));
        // Emergency unkey stays possible without releasing gain inhibition.
        rig.set_ptt(Vfo::Current, PttState::Off).await.unwrap();
        safety.confirmed_off(2);
        assert!(safety.blocked());
        barrier.wait().await;
    };
    let (outcome, ()) = tokio::join!(transaction, key_attempt);
    assert_eq!(outcome.unwrap(), StepOutcome::Increased { old: 0, new: 6 });
    assert!(safety.receiving());
}

#[tokio::test(flavor = "current_thread")]
async fn forced_ptt_first_race_defers_gain_until_hardware_off() {
    let barrier = tokio::sync::Barrier::new(2);
    let rig = RecordingRig::default();
    let safety = GainSafety::default();
    safety.confirmed_off(0);
    let key = async {
        assert!(safety.admit_ptt(1));
        assert!(safety.hardware_ptt_on(1));
        rig.set_ptt(Vfo::Current, PttState::On).await.unwrap();
        safety.ptt_command_consumed();
        barrier.wait().await;
    };
    let gain = async {
        barrier.wait().await;
        assert!(!safety.reserve());
        assert_eq!(
            execute_step(&rig, &safety, &[0, 6], deadline(), || true)
                .await
                .unwrap(),
            StepOutcome::Deferred
        );
        assert_eq!(rig.writes.load(Ordering::SeqCst), 0);
    };
    tokio::join!(key, gain);
    rig.set_ptt(Vfo::Current, PttState::Off).await.unwrap();
    safety.confirmed_off(2);
    assert!(safety.reserve());
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_keeps_tx_inhibited_until_owner_recovers() {
    let rig = RecordingRig {
        slow_set: true,
        ..Default::default()
    };
    let safety = GainSafety::default();
    safety.confirmed_off(0);
    assert!(safety.reserve());
    let result = tokio::time::timeout(
        Duration::from_millis(5),
        execute_step(&rig, &safety, &[0, 6], deadline(), || true),
    )
    .await;
    assert!(result.is_err());
    assert!(safety.blocked());
    assert!(!safety.admit_ptt(1));
    safety.fail_gain();
    safety.confirmed_off(2);
    assert!(safety.blocked());
    safety.recover_gain(3);
    assert!(safety.receiving());
}

#[tokio::test(flavor = "current_thread")]
async fn superseded_between_read_and_setter_never_writes() {
    let rig = RecordingRig::default();
    let safety = GainSafety::default();
    let authorizations = AtomicUsize::new(0);
    safety.confirmed_off(0);
    let epoch = safety.epoch();
    assert!(safety.reserve());
    assert_eq!(
        execute_step(&rig, &safety, &[0, 6], deadline(), || authorizations
            .fetch_add(1, Ordering::SeqCst)
            == 0)
        .await
        .unwrap(),
        StepOutcome::Deferred
    );
    assert_eq!(rig.writes.load(Ordering::SeqCst), 0);
    assert_eq!(safety.epoch(), epoch);
}

#[tokio::test(flavor = "current_thread")]
async fn timeout_during_initial_read_discards_uncertain_socket_reply() {
    let rig = RecordingRig {
        slow_read: true,
        ..Default::default()
    };
    let safety = GainSafety::default();
    safety.confirmed_off(0);
    assert!(safety.reserve());
    assert!(execute_step(
        &rig,
        &safety,
        &[0, 6],
        Instant::now() + Duration::from_millis(5),
        || true
    )
    .await
    .is_err());
    assert_eq!(rig.writes.load(Ordering::SeqCst), 0);
    assert!(rig.disconnected.load(Ordering::SeqCst));
    assert!(safety.blocked());
}
