//! Operator-chosen starting step for an FT8 QSO exchange.
//!
//! When an operator picks a station that is **calling them**, the correct
//! reply depends on what that station just sent: a CQ or a bare directed call
//! warrants our grid; a signal report warrants our R-report; an R-report
//! warrants RR73; an RR73/73 warrants a closing 73. Historically every
//! operator-initiated call hard-started at "send our grid", so a caller that
//! sent a report was answered with a grid — out of sequence.
//!
//! [`ResponseStep`] is the small, dependency-free vocabulary the TUI uses to
//! tell the QSO engine which rung of the standard exchange ladder to open at.
//! It lives in `pancetta-core` because both the TUI (which classifies the
//! caller's last message and offers an override) and the QSO engine (which
//! consumes the choice) depend on this crate but not on each other.

use serde::{Deserialize, Serialize};

/// One rung of the standard FT8 QSO exchange ladder, used as the starting
/// point for an operator-initiated reply to a station calling us.
///
/// The mapping from a step to the first transmitted message and the resulting
/// QSO state is owned by the QSO engine (`pancetta-qso`); this enum is just the
/// shared selector. The variant order matches the natural progression of an
/// exchange and is the order the TUI cycles through with Left/Right.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResponseStep {
    /// Send our grid (`THEM US GRID`). The classic opening reply to a CQ or a
    /// bare directed call. This is the historical default behavior.
    Grid,
    /// Send a signal report (`THEM US -NN`). Used when the caller skipped the
    /// grid step and sent — or is expecting — a report.
    Report,
    /// Send an R-report acknowledging theirs (`THEM US R-NN`). Used when the
    /// caller already sent us a signal report.
    ReportAck,
    /// Send the final roger (`THEM US RR73`). Used when the caller sent us an
    /// R-report and only the close remains.
    Rr73,
    /// Send a closing 73 (`THEM US 73`). Used when the caller already closed
    /// (RR73 / RRR / 73) and we just acknowledge to complete and log.
    SeventyThree,
}

impl Default for ResponseStep {
    /// Defaults to [`ResponseStep::Grid`], preserving the historical
    /// "reply with our grid" behavior when no smarter choice is available.
    fn default() -> Self {
        ResponseStep::Grid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_grid() {
        assert_eq!(ResponseStep::default(), ResponseStep::Grid);
    }

    #[test]
    fn round_trips_through_serde() {
        for step in [
            ResponseStep::Grid,
            ResponseStep::Report,
            ResponseStep::ReportAck,
            ResponseStep::Rr73,
            ResponseStep::SeventyThree,
        ] {
            let json = serde_json::to_string(&step).unwrap();
            let back: ResponseStep = serde_json::from_str(&json).unwrap();
            assert_eq!(step, back);
        }
    }
}
