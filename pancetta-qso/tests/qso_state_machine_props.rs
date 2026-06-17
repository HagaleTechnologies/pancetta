//! Property-based hardening of the QSO state machine.
//!
//! The example-based suites (`qso_scenarios`, `autonomous_scenarios`,
//! `adversarial_*`) pin specific exchanges. These properties instead feed
//! **randomized** frame sequences into a real [`QsoManager`] QSO and assert the
//! invariants that must hold for *every* sequence — the kind of guarantee that
//! catches the exchange nobody thought to write a scenario for.
//!
//! Invariants under test (for an autonomous `CallInitiation::Auto` QSO, the
//! Phase-5 path that now auto-sequences):
//!
//! 1. **No panic, ever.** `process_message` tolerates any frame in any order.
//! 2. **Sender verification is absolute.** A frame from a station that is NOT
//!    the latched partner never changes the QSO's state variant — impostors and
//!    third parties cannot drive or corrupt our QSO.
//! 3. **Completion implies a partner close.** The QSO never reaches `Completed`
//!    unless the verified partner actually sent a close token (RR73 / RRR / 73)
//!    at some point — we never log a contact the DX didn't close.
//!
//! Run: `cargo test -p pancetta-qso --test qso_state_machine_props`.

use pancetta_qso::states::QsoState;
use pancetta_qso::{QsoManager, QsoManagerConfig};
use proptest::prelude::*;

const US: &str = "K5ARH";
const PARTNER: &str = "VB7F";
const IMPOSTOR: &str = "W9XYZ";
const FREQ: f64 = 1500.0;

/// The shapes a station can send us, mid-QSO.
#[derive(Debug, Clone, Copy)]
enum FrameKind {
    Grid,
    Report,
    RReport,
    Rr73,
    Rrr,
    Seventy,
}

impl FrameKind {
    /// Is this a close token (a roger/sign-off the partner sends to end)?
    fn is_close(self) -> bool {
        matches!(self, FrameKind::Rr73 | FrameKind::Rrr | FrameKind::Seventy)
    }

    /// Render "<to> <from> <field>" for the given sender.
    fn render(self, from: &str) -> String {
        let field = match self {
            FrameKind::Grid => "EM20",
            FrameKind::Report => "-12",
            FrameKind::RReport => "R-12",
            FrameKind::Rr73 => "RR73",
            FrameKind::Rrr => "RRR",
            FrameKind::Seventy => "73",
        };
        format!("{US} {from} {field}")
    }
}

/// One frame in the random sequence: from the real partner, or an impostor.
#[derive(Debug, Clone, Copy)]
struct Frame {
    from_partner: bool,
    kind: FrameKind,
}

fn frame_kind_strategy() -> impl Strategy<Value = FrameKind> {
    prop_oneof![
        Just(FrameKind::Grid),
        Just(FrameKind::Report),
        Just(FrameKind::RReport),
        Just(FrameKind::Rr73),
        Just(FrameKind::Rrr),
        Just(FrameKind::Seventy),
    ]
}

fn frame_strategy() -> impl Strategy<Value = Frame> {
    // Bias toward partner frames (so QSOs actually progress) but keep a healthy
    // fraction of impostors to exercise sender verification.
    (proptest::bool::weighted(0.7), frame_kind_strategy())
        .prop_map(|(from_partner, kind)| Frame { from_partner, kind })
}

/// Discriminant-only view of the state, so "unchanged variant" comparisons
/// ignore per-transition timestamps (`started_at = Utc::now()`).
fn state_tag(s: &QsoState) -> std::mem::Discriminant<QsoState> {
    std::mem::discriminant(s)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 200, ..ProptestConfig::default() })]

    #[test]
    fn auto_qso_invariants_hold_for_any_frame_sequence(
        frames in proptest::collection::vec(frame_strategy(), 0..14)
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async move {
            let manager = QsoManager::new(QsoManagerConfig {
                our_callsign: US.to_string(),
                our_grid: Some("EM10".to_string()),
                ..Default::default()
            });

            // Open an autonomous pounce on PARTNER (the Phase-5 Auto path).
            let qso_id = manager
                .respond_to_cq(PARTNER.to_string(), FREQ, None)
                .await
                .expect("respond_to_cq (Auto)");

            let mut partner_sent_close = false;

            for f in &frames {
                let from = if f.from_partner { PARTNER } else { IMPOSTOR };
                let text = f.kind.render(from);
                let msg = pancetta_qso::utils::parse_ft8_message(&text, US)
                    .expect("frame must parse");

                // State before this frame.
                let before = manager
                    .get_qso(qso_id.clone())
                    .await
                    .ok()
                    .map(|p| state_tag(&p.state));

                // INVARIANT 1: never panics, never hard-errors on a valid frame.
                manager
                    .process_message(msg, text.clone(), FREQ, Some(-12.0))
                    .await
                    .expect("process_message must not error on a valid frame");

                let after = manager
                    .get_qso(qso_id.clone())
                    .await
                    .ok()
                    .map(|p| state_tag(&p.state));

                if f.from_partner && f.kind.is_close() {
                    partner_sent_close = true;
                }

                // INVARIANT 2: an impostor frame never changes the state variant.
                if !f.from_partner {
                    prop_assert_eq!(
                        before, after,
                        "impostor frame {:?} from {} changed the QSO state",
                        f.kind, IMPOSTOR
                    );
                }
            }

            // INVARIANT 3: completion implies the partner actually closed.
            if let Ok(progress) = manager.get_qso(qso_id.clone()).await {
                if matches!(progress.state, QsoState::Completed { .. }) {
                    prop_assert!(
                        partner_sent_close,
                        "QSO reached Completed but the partner never sent a close token; \
                         sequence = {:?}",
                        frames
                    );
                    // A completed QSO must be logged against the real partner,
                    // never the impostor.
                    if let QsoState::Completed { their_callsign, .. } = &progress.state {
                        prop_assert_eq!(
                            their_callsign.as_str(),
                            PARTNER,
                            "completed QSO logged against the wrong callsign"
                        );
                    }
                }
            }

            Ok(())
        })?;
    }
}
