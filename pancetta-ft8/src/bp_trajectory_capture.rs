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
///
/// * **v1** — Sessions 1-3. Channel LLRs, 25-iteration trajectory, final
///   LLRs, `osd_recovered`, `osd_codeword`, `bp_iters_run`.
/// * **v2** — PAN-9. Adds [`CapturedTrajectory::mrb_perm`] (OSD's
///   most-reliable-basis permutation) and
///   [`CapturedTrajectory::syndrome_counts`] (per-bit unsatisfied-check
///   counts). Both are required by the soft-rank training objective, and a
///   v1 record silently read as v2 would train on labels in the wrong basis
///   with a missing input channel.
///
/// Use [`validate_schema_version`] to check a version read off disk. A
/// version constant nobody checks is not a guard.
pub const CAPTURE_SCHEMA_VERSION: u32 = 2;

/// Why a captured-corpus file was rejected by [`validate_schema_version`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaVersionError {
    /// A pre-PAN-9 file. Named separately from `Unsupported` because these
    /// genuinely exist on the operator's machine (Sessions 1-3) and the
    /// remedy is specific: regenerate, do not migrate.
    LegacyV1,
    /// Any other version this build does not understand.
    Unsupported(u32),
}

impl std::fmt::Display for SchemaVersionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LegacyV1 => write!(
                f,
                "capture schema v1 is not readable by this build (expected v{CAPTURE_SCHEMA_VERSION}). \
                 v1 records carry no MRB permutation and no syndrome counts, so soft-rank labels \
                 cannot be computed from them. Regenerate the corpus rather than migrating it."
            ),
            Self::Unsupported(v) => write!(
                f,
                "unsupported capture schema v{v} (this build reads v{CAPTURE_SCHEMA_VERSION})"
            ),
        }
    }
}

impl std::error::Error for SchemaVersionError {}

/// Accept only the schema version this build can actually parse.
///
/// Callers reading a captured corpus off disk MUST run this before parsing
/// records. Rejecting v1 explicitly — rather than mis-parsing it into a v2
/// struct with defaulted fields — is the whole point: a defaulted
/// `mrb_perm` would put every training label in the identity basis and look
/// exactly like "the new objective just doesn't help."
pub fn validate_schema_version(version: u32) -> Result<(), SchemaVersionError> {
    match version {
        CAPTURE_SCHEMA_VERSION => Ok(()),
        1 => Err(SchemaVersionError::LegacyV1),
        other => Err(SchemaVersionError::Unsupported(other)),
    }
}

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
    /// Schema v2. OSD's most-reliable-basis permutation for this sample:
    /// `mrb_perm[i]` is the original codeword position of MRB column `i`,
    /// obtained from [`crate::osd::OsdDecoder::mrb_permutation`] — the same
    /// code the decode path itself runs, never a re-derivation.
    ///
    /// `None` when no full-rank basis existed (Gaussian elimination failed),
    /// which is also exactly when OSD produced nothing to label.
    ///
    /// The soft-rank objective is defined over reprocessing *order*, and
    /// reprocessing happens in this basis — a label computed in the natural
    /// bit order is measuring a different quantity than the one production
    /// pays for.
    pub mrb_perm: Option<[u16; 174]>,
    /// Schema v2. Per-bit unsatisfied-check counts at BP exit,
    /// `s = H · hard(final_llrs)`, from
    /// [`crate::syndrome::unsatisfied_check_counts_from_llrs`]. Each entry is
    /// in `0..=3`. This is the model's 26th input row.
    pub syndrome_counts: [u8; 174],
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
            mrb_perm: None,
            syndrome_counts: [0; 174],
        }
    }

    #[test]
    fn current_schema_version_is_v2() {
        assert_eq!(CAPTURE_SCHEMA_VERSION, 2);
        assert_eq!(validate_schema_version(CAPTURE_SCHEMA_VERSION), Ok(()));
    }

    #[test]
    fn v1_is_rejected_by_name_not_mis_parsed() {
        let err = validate_schema_version(1).expect_err("v1 must be rejected");
        assert_eq!(err, SchemaVersionError::LegacyV1);
        // The message has to tell the operator what to DO — these files
        // exist on their machine from Sessions 1-3.
        let msg = err.to_string();
        assert!(msg.contains("v1"), "message must name v1: {msg}");
        assert!(
            msg.contains("Regenerate"),
            "message must state the remedy: {msg}"
        );
    }

    #[test]
    fn unknown_versions_are_rejected() {
        for v in [0u32, 3, 99] {
            assert_eq!(
                validate_schema_version(v),
                Err(SchemaVersionError::Unsupported(v)),
                "version {v} must not be accepted"
            );
        }
    }

    #[test]
    fn v2_fields_round_trip_through_the_sink() {
        let mut sample = zero_sample();
        let mut perm = [0u16; 174];
        for (i, slot) in perm.iter_mut().enumerate() {
            *slot = (173 - i) as u16; // a reversal — clearly not the identity
        }
        sample.mrb_perm = Some(perm);
        sample.syndrome_counts[7] = 3;
        sample.syndrome_counts[173] = 1;

        enable_local();
        record(sample);
        let drained = drain_local();
        disable_local();

        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].mrb_perm, Some(perm));
        assert_eq!(drained[0].syndrome_counts[7], 3);
        assert_eq!(drained[0].syndrome_counts[173], 1);
    }
}
