// PAN-83. Formerly the inline module `pan83_answered_frame_tests`.
//! PAN-83: `answered_frame_offset_hz` — the pure window that decides
//! whether a QSO's newest aired frame is the one a reply answered, using
//! fixed, slot-aligned timestamps (2026-10-24 12:00:00 UTC is a slot
//! boundary on both the FT8 15 s and the FT4 7.5 s grid).
use super::*;
use chrono::TimeZone;

const SLOT_NS: i64 = 15_000_000_000;

fn slot_aligned_reply_slot() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 24, 12, 0, 0).unwrap()
}

fn aired(offset_hz: f64, aired_at: DateTime<Utc>) -> Option<AiredFrame> {
    Some(AiredFrame {
        offset_hz,
        aired_at,
    })
}

fn reply_parity(reply_slot: DateTime<Utc>, slot_ns: i64) -> pancetta_core::slot::SlotParity {
    pancetta_core::slot::SlotParity::of_with_period(reply_slot, slot_ns)
}

#[test]
fn answered_frame_is_the_one_aired_in_the_slot_before_the_reply() {
    let reply_slot = slot_aligned_reply_slot();
    assert_eq!(
        answered_frame_offset_hz(
            aired(1500.0, reply_slot - Duration::seconds(2)),
            reply_slot + Duration::seconds(13),
            Some(reply_parity(reply_slot, SLOT_NS)),
            SLOT_NS
        ),
        Some(1500.0),
        "a frame that finished airing just before the reply's slot began \
         is the frame the reply answered"
    );
}

#[test]
fn answered_frame_survives_the_decode_being_processed_after_the_reply_slot_ends() {
    let reply_slot = slot_aligned_reply_slot();
    assert_eq!(
        answered_frame_offset_hz(
            aired(1500.0, reply_slot - Duration::seconds(2)),
            reply_slot + Duration::seconds(17),
            Some(reply_parity(reply_slot, SLOT_NS)),
            SLOT_NS
        ),
        Some(1500.0),
        "a decode processed just after the reply's slot ended still \
         anchors on that slot, so its answered frame still counts"
    );
}

#[test]
fn a_late_started_frame_that_finishes_inside_the_reply_slot_still_counts() {
    let reply_slot = slot_aligned_reply_slot();
    assert_eq!(
        answered_frame_offset_hz(
            aired(1500.0, reply_slot + Duration::seconds(1)),
            reply_slot + Duration::seconds(16),
            Some(reply_parity(reply_slot, SLOT_NS)),
            SLOT_NS
        ),
        Some(1500.0),
        "`aired_at` is stamped after playback, so a late-started R-1 \
         frame can report just after R began — it is still the answered \
         frame"
    );
}

#[test]
fn a_frame_from_an_earlier_own_slot_does_not_count() {
    let reply_slot = slot_aligned_reply_slot();
    assert_eq!(
        answered_frame_offset_hz(
            aired(1500.0, reply_slot - Duration::seconds(17)),
            reply_slot + Duration::seconds(13),
            Some(reply_parity(reply_slot, SLOT_NS)),
            SLOT_NS
        ),
        None,
        "a frame from two of our own slots before the reply is too old to \
         be the answered one"
    );
}

#[test]
fn our_next_frame_after_the_reply_does_not_count() {
    let reply_slot = slot_aligned_reply_slot();
    assert_eq!(
        answered_frame_offset_hz(
            aired(1500.0, reply_slot + Duration::seconds(28)),
            reply_slot + Duration::seconds(29),
            Some(reply_parity(reply_slot, SLOT_NS)),
            SLOT_NS
        ),
        None,
        "a frame that aired in the slot AFTER the reply answered nothing \
         the DX could have decoded in time"
    );
}

#[test]
fn no_slot_parity_means_no_answer() {
    let reply_slot = slot_aligned_reply_slot();
    assert_eq!(
        answered_frame_offset_hz(
            aired(1500.0, reply_slot - Duration::seconds(2)),
            reply_slot + Duration::seconds(13),
            None,
            SLOT_NS
        ),
        None,
        "without the decode's slot parity there is no reply slot R to \
         anchor the window on — fall back to provenance"
    );
}

#[test]
fn no_aired_record_means_no_answer() {
    let reply_slot = slot_aligned_reply_slot();
    assert_eq!(
        answered_frame_offset_hz(
            None,
            reply_slot + Duration::seconds(13),
            Some(reply_parity(reply_slot, SLOT_NS)),
            SLOT_NS
        ),
        None,
        "no recorded aired frame pins the answer — fall back to provenance"
    );
}

#[test]
fn the_window_scales_with_the_active_slot_length() {
    // FT4: 7.5 s slots. 12:00:00 is also on the 7.5 s grid.
    const FT4_SLOT_NS: i64 = 7_500_000_000;
    let reply_slot = slot_aligned_reply_slot();
    let parity = reply_parity(reply_slot, FT4_SLOT_NS);
    assert_eq!(
        answered_frame_offset_hz(
            aired(1500.0, reply_slot - Duration::seconds(1)),
            reply_slot + Duration::seconds(6),
            Some(parity),
            FT4_SLOT_NS
        ),
        Some(1500.0),
        "one FT4 slot before the reply's slot — inside the scaled window"
    );
    assert_eq!(
        answered_frame_offset_hz(
            aired(1500.0, reply_slot - Duration::seconds(8)),
            reply_slot + Duration::seconds(6),
            Some(parity),
            FT4_SLOT_NS
        ),
        None,
        "more than one FT4 slot before the reply — outside the scaled window"
    );
}
