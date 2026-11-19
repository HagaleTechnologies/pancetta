use super::*;
use crate::message_bus::{MessageBus, RigControlMessage};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

async fn guard(bus: &MessageBus, ptt_active: &Arc<AtomicBool>) -> PttGuard {
    PttGuard::new(
        bus.clone(),
        ptt_active.clone(),
        &Arc::new(tokio::sync::Mutex::new(())),
        &Arc::new(AtomicU64::new(0)),
        &Arc::new(AtomicU64::new(0)),
        &PendingPttOff::default(),
    )
    .await
}
#[tokio::test(flavor = "current_thread")]
async fn gain_recovery_before_key_keeps_relocation_interlock_active() {
    let bus = MessageBus::new(16).unwrap();
    let (_s, rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    bus.receive_gain.confirmed_off(100);
    assert!(bus.receive_gain.reserve());
    let ptt_active = Arc::new(AtomicBool::new(false));
    let mut g = guard(&bus, &ptt_active).await;

    // ATT finishes while the worker yields to publish TX status.
    tokio::task::yield_now().await;
    bus.receive_gain.finish_gain(101);
    assert_eq!(
        send_ptt_command(&bus, ComponentId::Ft8Transmitter, true, || true),
        SendIfOutcome::Enqueued
    );
    assert!(rx.try_iter().any(|m| matches!(
        m.message_type,
        MessageType::RigControl(RigControlMessage::SetPtt { state: true })
    )));
    assert!(
        ptt_active.load(Ordering::Acquire),
        "keyed TX must block relocation even if gain was blocked at guard construction"
    );
    assert_eq!(g.release("gain recovery test").await, PttRelease::Delivered);
    assert!(!ptt_active.load(Ordering::Acquire));
}

async fn backlog(bus: &MessageBus) -> bool {
    bus.send_message_checked(ComponentMessage::new(
        ComponentId::Hamlib,
        ComponentId::Hamlib,
        MessageType::AudioData(Vec::new()),
        Instant::now(),
    ))
    .await
    .unwrap()
}
fn ptt_offs(rx: &crossbeam_channel::Receiver<ComponentMessage>) -> usize {
    rx.try_iter()
        .filter(|m| {
            matches!(
                m.message_type,
                MessageType::RigControl(RigControlMessage::SetPtt { state: false })
            )
        })
        .count()
}
async fn let_drop_task_run() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn timestamps_cover_release_drop_and_second_key_cycle() {
    let bus = MessageBus::new(16).unwrap();
    let (_s, _rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    let active = Arc::new(AtomicBool::new(false));
    let on = Arc::new(AtomicU64::new(0));
    let off = Arc::new(AtomicU64::new(0));
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    let pending = PendingPttOff::default();
    let mut g = PttGuard::new(bus.clone(), active.clone(), &gate, &on, &off, &pending).await;
    assert!(on.load(Ordering::Acquire) > 0);
    assert_eq!(off.load(Ordering::Acquire), 0);
    assert_eq!(g.release("timestamp test").await, PttRelease::Delivered);
    assert!(off.load(Ordering::Acquire) >= on.load(Ordering::Acquire));
    drop(g);
    let first_on = on.load(Ordering::Acquire);
    tokio::time::sleep(Duration::from_millis(2)).await;
    let g = PttGuard::new(bus, active.clone(), &gate, &on, &off, &pending).await;
    assert!(on.load(Ordering::Acquire) > first_on);
    assert!(off.load(Ordering::Acquire) < on.load(Ordering::Acquire));
    drop(g);
    let_drop_task_run().await;
    assert!(!active.load(Ordering::Acquire));
    assert!(off.load(Ordering::Acquire) >= on.load(Ordering::Acquire));
}

#[tokio::test(flavor = "current_thread")]
async fn release_on_a_healthy_channel_unkeys_once_and_stands_the_guard_down() {
    let bus = MessageBus::new(16).unwrap();
    let (_s, rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    let ptt_active = Arc::new(AtomicBool::new(false));
    let mut g = guard(&bus, &ptt_active).await;
    assert_eq!(g.release("test").await, PttRelease::Delivered);
    assert!(!ptt_active.load(Ordering::Acquire));
    drop(g);
    let_drop_task_run().await;
    assert_eq!(
        ptt_offs(&rx),
        1,
        "no duplicate unkey from a stood-down guard"
    );
}

/// THE regression (reproduced this session against the old sequence:
/// 0 PTT-offs ever reached Hamlib).
#[tokio::test(flavor = "current_thread")]
async fn release_into_a_full_hamlib_channel_keeps_the_fallback_armed() {
    let bus = MessageBus::new(1).unwrap();
    let (_s, rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    assert!(backlog(&bus).await);
    let ptt_active = Arc::new(AtomicBool::new(false));
    let mut g = guard(&bus, &ptt_active).await;
    assert_eq!(g.release("test").await, PttRelease::Undelivered);
    assert!(
        ptt_active.load(Ordering::Acquire),
        "unkey not delivered: still keyed"
    );
    let _ = rx.try_iter().count(); // Hamlib catches up
    drop(g);
    let_drop_task_run().await;
    assert_eq!(
        ptt_offs(&rx),
        1,
        "the armed guard's Drop delivers the unkey"
    );
    assert!(!ptt_active.load(Ordering::Acquire));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_retries_until_hamlib_frees_room() {
    let bus = MessageBus::new(1).unwrap();
    let (_s, rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    assert!(backlog(&bus).await);
    let rx2 = rx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let _ = rx2.try_recv();
    });
    let ptt_active = Arc::new(AtomicBool::new(false));
    let mut g = guard(&bus, &ptt_active).await;
    assert_eq!(g.release("test").await, PttRelease::Delivered);
    assert_eq!(ptt_offs(&rx), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn armed_guard_drop_retries_until_hamlib_frees_room() {
    let bus = MessageBus::new(1).unwrap();
    let (_s, rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    assert!(backlog(&bus).await);
    let ptt_active = Arc::new(AtomicBool::new(false));
    drop(guard(&bus, &ptt_active).await); // armed
    tokio::time::sleep(Duration::from_millis(10)).await;
    let _ = rx.try_recv(); // room frees inside the Drop's retry window
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(ptt_offs(&rx), 1, "Drop retries, not one best-effort shot");
}

/// PAN-273 (Codex P2, PR #587): an undelivered release leaves the
/// guard's `Drop` fallback PTT-off outstanding. The next key-down (a
/// supersede re-key, or the next frame) must not overtake it: the next
/// `PttGuard::new` waits for it, so Hamlib sees that PTT-off before the
/// new PTT-on. Before, the new guard came straight back and the stale
/// PTT-off landed after the re-key's PTT-on, cutting the new frame off.
#[tokio::test(flavor = "current_thread")]
async fn next_key_down_waits_for_an_outstanding_drop_fallback_unkey() {
    let bus = MessageBus::new(2).unwrap();
    let (_s, rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    assert!(backlog(&bus).await);
    assert!(backlog(&bus).await);
    let active = Arc::new(AtomicBool::new(false));
    let on = Arc::new(AtomicU64::new(0));
    let off = Arc::new(AtomicU64::new(0));
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    let pending = PendingPttOff::default();
    let mut a = PttGuard::new(bus.clone(), active.clone(), &gate, &on, &off, &pending).await;
    assert_eq!(a.release("Supersede").await, PttRelease::Undelivered);
    drop(a); // armed: its fallback PTT-off is now outstanding
    let _ = rx.try_iter().count(); // Hamlib catches up
    let b = PttGuard::new(bus.clone(), active.clone(), &gate, &on, &off, &pending).await;
    assert_eq!(
        ptt_offs(&rx),
        1,
        "the earlier guard's fallback PTT-off is on Hamlib's channel before the new key-down"
    );
    drop(b);
    let_drop_task_run().await;
}

/// PAN-273 self-review: a key-down's wait for the fallback can be
/// cancelled (worker abort or supervisor restart). The unfinished
/// fallback must stay recorded, so the restarted worker's first
/// key-down still waits for it.
#[tokio::test(flavor = "current_thread")]
async fn a_cancelled_settle_keeps_the_fallback_for_the_next_key_down() {
    let pending = PendingPttOff::default();
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    pending.push(tokio::task::spawn(async move {
        let _ = held.await;
    }));
    assert!(
        tokio::time::timeout(Duration::from_millis(10), pending.settle())
            .await
            .is_err(),
        "the fallback is still running"
    );
    assert!(
        pending.0.lock().unwrap().is_some(),
        "the cancelled wait put the unfinished fallback back"
    );
    let _ = release.send(());
    pending.settle().await;
    assert!(pending.0.lock().unwrap().is_none());
}

/// PAN-273: with no Hamlib channel (`--replay`) nothing was keyed, so `release` stands the guard
/// down without a send. Before, it stalled ~78 ms, logged an error and
/// left the guard armed with 4 drops.
#[tokio::test(flavor = "current_thread")]
async fn release_with_no_hamlib_channel_stands_down_without_sending() {
    let bus = MessageBus::new(16).unwrap();
    let ptt_active = Arc::new(AtomicBool::new(false));
    let off = Arc::new(AtomicU64::new(0));
    let mut g = PttGuard::new(
        bus.clone(),
        ptt_active.clone(),
        &Arc::new(tokio::sync::Mutex::new(())),
        &Arc::new(AtomicU64::new(0)),
        &off,
        &PendingPttOff::default(),
    )
    .await;
    assert!(ptt_active.load(Ordering::Acquire));
    assert_eq!(g.release("test").await, PttRelease::NoRig);
    assert!(!g.armed, "nothing to unkey: the fallback stands down");
    assert!(!ptt_active.load(Ordering::Acquire));
    assert!(off.load(Ordering::Acquire) > 0, "PTT-off time is stamped");
    assert_eq!(
        bus.get_statistics().dropped_messages,
        0,
        "no PTT-off was attempted"
    );
    drop(g);
    let_drop_task_run().await;
    assert_eq!(bus.get_statistics().dropped_messages, 0, "no Drop retry");
}
