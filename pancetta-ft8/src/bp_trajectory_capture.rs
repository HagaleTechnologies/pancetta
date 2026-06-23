//! BP trajectory capture for the hb-064 research workflow (Session 1).
//!
//! When enabled, the LDPC soft-decode path appends one
//! [`CapturedTrajectory`] sample to a per-thread sink each time belief
//! propagation fails to converge and OSD is invoked. The research harness
//! ([`pancetta-research/examples/hb064_generate_trajectory_dataset.rs`])
//! consumes these samples to build a training set for a layered-BP /
//! pancetta-band-tuned neural OSD.
//!
//! Design notes:
//!
//! * **Thread-local sink.** BP runs single-threaded inside one decoder
//!   call, but Pancetta uses Rayon for inter-candidate parallelism — each
//!   worker thread gets its own sink. Callers must drain every thread
//!   that participated. See [`drain_local`].
//! * **Opt-in.** Capture is OFF by default (the thread-local atomic flag
//!   starts `false`); production decoding pays no overhead beyond a
//!   single relaxed load + branch per OSD-eligible BP failure.
//! * **No allocation on the hot path while disabled.** The recorder only
//!   appends to its `Vec` when the flag is true.
//! * **The captured shape mirrors the neural OSD model contract**
//!   (25 BP iterations × 174 codeword bits). When BP exits before 25
//!   iterations, the remaining trajectory slots hold the final LLRs
//!   (same convention as [`crate::decoder`]'s
//!   `belief_propagation_with_trajectory`).
//!
//! Schema versioning: bump [`CAPTURE_SCHEMA_VERSION`] on any
//! breaking change to the recorded payload.
//!
//! Not part of the production decode surface; never imported by the
//! `pancetta` crate or any release binary.

use std::cell::RefCell;

/// Bumped when the captured-payload format changes in a
/// backward-incompatible way.
pub const CAPTURE_SCHEMA_VERSION: u32 = 1;

/// Per-BP-failure trajectory sample. One record per BP non-convergence
/// that reaches OSD.
#[derive(Debug, Clone)]
pub struct CapturedTrajectory {
    /// Channel LLRs (pre-BP, post-normalization). Length 174.
    pub channel_llrs: [f32; 174],
    /// LLR posterior after each of the 25 BP iterations. Slots
    /// `[max_iters..25]` carry the final LLRs (BP stopped early).
    pub trajectory: [[f32; 174]; 25],
    /// LLR posterior at exit (== `trajectory[max_iters - 1]`).
    pub final_llrs: [f32; 174],
    /// True iff OSD found a CRC-valid codeword (i.e. the BP failure was
    /// recoverable). When false, the truth bits are unknown to the
    /// decoder and `osd_codeword` is `None`.
    pub osd_recovered: bool,
    /// CRC-valid codeword returned by OSD, when `osd_recovered` is
    /// true. Length 174. Used to derive the per-info-bit "was BP's
    /// hard-decision wrong?" labels for training.
    pub osd_codeword: Option<[u8; 174]>,
    /// Number of BP iterations actually run before the loop exited
    /// (early-terminated convergence path is not captured — see
    /// [`record`] for the gate).
    pub bp_iters_run: u16,
}

thread_local! {
    static ENABLED: RefCell<bool> = const { RefCell::new(false) };
    static SINK: RefCell<Vec<CapturedTrajectory>> = const { RefCell::new(Vec::new()) };
}

/// Enable trajectory capture on the current thread. Disabled by
/// default. Safe to call multiple times; subsequent records append to
/// the existing sink without clearing it.
pub fn enable_local() {
    ENABLED.with(|e| *e.borrow_mut() = true);
}

/// Disable trajectory capture on the current thread. Existing records
/// remain in the sink until [`drain_local`] is called.
pub fn disable_local() {
    ENABLED.with(|e| *e.borrow_mut() = false);
}

/// True iff trajectory capture is currently enabled on this thread.
#[inline]
pub fn is_enabled() -> bool {
    ENABLED.with(|e| *e.borrow())
}

/// Drain and return all captured samples for the current thread.
/// Resets the sink to empty.
pub fn drain_local() -> Vec<CapturedTrajectory> {
    SINK.with(|s| std::mem::take(&mut *s.borrow_mut()))
}

/// Append one captured trajectory to the per-thread sink. No-op when
/// capture is disabled. Callers should only invoke this from the BP
/// failure / OSD-fallback path — successful BP convergence carries no
/// trajectory signal and is uninteresting for training.
pub fn record(sample: CapturedTrajectory) {
    if !is_enabled() {
        return;
    }
    SINK.with(|s| s.borrow_mut().push(sample));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_disabled_by_default() {
        // Each test runs on its own thread; the default is `false`.
        assert!(!is_enabled());
        record(zero_sample());
        assert!(drain_local().is_empty());
    }

    #[test]
    fn enable_then_record_then_drain() {
        enable_local();
        assert!(is_enabled());
        record(zero_sample());
        record(zero_sample());
        let drained = drain_local();
        assert_eq!(drained.len(), 2);
        // Drain leaves the sink empty.
        assert!(drain_local().is_empty());
        disable_local();
        assert!(!is_enabled());
    }

    #[test]
    fn disable_blocks_further_records_but_keeps_drained() {
        enable_local();
        record(zero_sample());
        disable_local();
        record(zero_sample()); // should be a no-op
        let drained = drain_local();
        assert_eq!(drained.len(), 1);
    }

    fn zero_sample() -> CapturedTrajectory {
        CapturedTrajectory {
            channel_llrs: [0.0; 174],
            trajectory: [[0.0; 174]; 25],
            final_llrs: [0.0; 174],
            osd_recovered: false,
            osd_codeword: None,
            bp_iters_run: 0,
        }
    }
}
