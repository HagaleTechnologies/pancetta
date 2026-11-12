use std::sync::atomic::{AtomicU32, Ordering};

fn next(state: u32) -> u32 {
    state.wrapping_mul(1103515245).wrapping_add(12345)
}

fn random_from(seed: &AtomicU32) -> f32 {
    let previous = seed
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |state| {
            Some(next(state))
        })
        .expect("the LCG always supplies a next state");
    ((next(previous) / 65536) % 1000) as f32 / 1000.0
}

pub fn random<T: RandomValue>() -> T {
    T::random()
}

pub trait RandomValue {
    fn random() -> Self;
}

impl RandomValue for f32 {
    fn random() -> Self {
        static SEED: AtomicU32 = AtomicU32::new(12345);
        random_from(&SEED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_serial_lcg() {
        for start in [12345, u32::MAX, 0] {
            let seed = AtomicU32::new(start);
            let mut reference = start;
            for _ in 0..1000 {
                reference = reference.wrapping_mul(1103515245).wrapping_add(12345);
                assert_eq!(
                    random_from(&seed),
                    ((reference / 65536) % 1000) as f32 / 1000.0
                );
                assert_eq!(seed.load(Ordering::Relaxed), reference);
            }
        }
        assert!((0.0..1.0).contains(&random::<f32>()));
    }

    #[test]
    fn concurrent_updates_are_not_lost() {
        let seed = AtomicU32::new(12345);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let seed = &seed;
                scope.spawn(move || {
                    for _ in 0..1000 {
                        random_from(seed);
                    }
                });
            }
        });
        let expected = (0..8000).fold(12345u32, |state, _| {
            state.wrapping_mul(1103515245).wrapping_add(12345)
        });
        assert_eq!(seed.load(Ordering::Relaxed), expected);
    }
}
