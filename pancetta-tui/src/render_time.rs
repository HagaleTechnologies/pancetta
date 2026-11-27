//! UTC used by TUI rendering. Snapshot tests can hold one instant for a draw pair.
use pancetta_core::time::UtcTimestamp;

#[cfg(test)]
thread_local! {
    static OVERRIDE: std::cell::Cell<Option<UtcTimestamp>> = const { std::cell::Cell::new(None) };
}

pub(crate) fn now() -> UtcTimestamp {
    #[cfg(test)]
    if let Some(at) = OVERRIDE.with(|cell| cell.get()) {
        return at;
    }
    UtcTimestamp::now()
}

#[cfg(test)]
pub(crate) fn with_utc<R>(at: UtcTimestamp, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<UtcTimestamp>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OVERRIDE.with(|cell| cell.set(self.0));
        }
    }
    let previous = OVERRIDE.with(|cell| cell.replace(Some(at)));
    let _restore = Restore(previous);
    f()
}
