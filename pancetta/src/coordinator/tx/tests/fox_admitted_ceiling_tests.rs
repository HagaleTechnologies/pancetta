use super::*;
use std::collections::HashSet;
use std::sync::RwLock;

fn set(ids: &[&str]) -> RwLock<HashSet<String>> {
    RwLock::new(
        ids.iter()
            .map(|s| super::super::active_tx_qso_key(s))
            .collect(),
    )
}

fn poison(lock: &RwLock<HashSet<String>>) {
    std::thread::scope(|scope| {
        let _ = scope
            .spawn(|| {
                let _guard = lock.write().unwrap();
                panic!("poisoning the lock on purpose");
            })
            .join();
    });
    assert!(lock.is_poisoned());
}

#[test]
fn pan76_live_fox_admitted_count_ignores_keys_no_longer_tx_live() {
    let fox = set(&["A", "B", "C", "STALE"]);
    let active = set(&["A", "B", "C", "X"]);
    assert_eq!(live_fox_admitted_count(&fox, &active), 3);
}

#[test]
fn pan76_live_fox_admitted_count_is_zero_when_either_lock_is_poisoned() {
    let fox = set(&["A", "B"]);
    let active = set(&["A", "B"]);
    poison(&fox);
    assert_eq!(live_fox_admitted_count(&fox, &active), 0);

    let fox = set(&["A", "B"]);
    let active = set(&["A", "B"]);
    poison(&active);
    assert_eq!(live_fox_admitted_count(&fox, &active), 0);
}

#[test]
fn pan76_supersede_ceiling_covers_live_fox_admitted_qsos() {
    assert_eq!(supersede_max_concurrent_qsos(1, 3), 4);
    assert_eq!(supersede_max_concurrent_qsos(6, 3), 9);
    assert_eq!(supersede_max_concurrent_qsos(1, 0), 1);
    assert_eq!(supersede_max_concurrent_qsos(u32::MAX, 3), u32::MAX);
}
