// PAN-359. The worked-station activation book.
use super::*;
use jiff::SignedDuration;

fn pota(reference: &str) -> WorkedReference {
    WorkedReference {
        sig: "POTA".to_string(),
        sig_info: reference.to_string(),
    }
}

fn entry(call: &str, reference: &str) -> (String, WorkedReference) {
    (call.to_string(), pota(reference))
}

/// PAN-359 AC8: an entry last seen more than 30 minutes before the QSO's
/// end time is not returned; one seen exactly 30 minutes before is.
#[test]
fn stale_entry_is_not_returned() {
    let now = UtcTimestamp::now();
    let book = WorkedReferenceBook::new();
    book.record_feed(
        [entry("DL1ABC", "DA-0123")],
        now - SignedDuration::from_mins(31),
    );
    assert_eq!(book.lookup("DL1ABC", now), None);

    let book = WorkedReferenceBook::new();
    let seen = now - SignedDuration::from_mins(30);
    book.record_feed([entry("DL1ABC", "DA-0123")], seen);
    assert_eq!(book.lookup("DL1ABC", now), Some(pota("DA-0123")));
    assert_eq!(
        book.lookup("DL1ABC", now + SignedDuration::from_secs(1)),
        None
    );

    // Unknown partner.
    assert_eq!(book.lookup("K1XYZ", now), None);
}

/// PAN-359 AC8: an unresolved hash render is never looked up; a resolved
/// one is looked up as the call inside it.
#[test]
fn unresolved_hash_call_is_not_looked_up() {
    let now = UtcTimestamp::now();
    let book = WorkedReferenceBook::new();
    book.record_feed([entry("DL1ABC", "DA-0123")], now);
    assert_eq!(book.lookup("<...>", now), None);
    assert_eq!(book.lookup("<>", now), None);
    assert_eq!(book.lookup("<DL1ABC>", now), Some(pota("DA-0123")));
    assert_eq!(book.lookup(" dl1abc ", now), Some(pota("DA-0123")));
    assert_eq!(book.lookup("", now), None);
}

/// PAN-359 AC9: an exact normalized call wins over a base-call match.
#[test]
fn exact_call_wins_over_base_call() {
    let now = UtcTimestamp::now();
    let book = WorkedReferenceBook::new();
    book.record_feed(
        [entry("DL1ABC", "DA-0123"), entry("DL1ABC/P", "DA-0999")],
        now,
    );
    assert_eq!(book.lookup("DL1ABC", now), Some(pota("DA-0123")));
    assert_eq!(book.lookup("dl1abc/p", now), Some(pota("DA-0999")));
}

/// PAN-359 AC9: with no exact entry, the one fresh entry sharing the base
/// callsign matches, in both directions.
#[test]
fn base_call_matches_when_unique() {
    let now = UtcTimestamp::now();
    let book = WorkedReferenceBook::new();
    book.record_feed([entry("DL1ABC", "DA-0123")], now);
    assert_eq!(book.lookup("DL1ABC/P", now), Some(pota("DA-0123")));

    let book = WorkedReferenceBook::new();
    book.record_feed([entry("DL1ABC/P", "DA-0123")], now);
    assert_eq!(book.lookup("DL1ABC", now), Some(pota("DA-0123")));

    // A stale base-call entry is not used either.
    let book = WorkedReferenceBook::new();
    book.record_feed(
        [entry("DL1ABC/P", "DA-0123")],
        now - SignedDuration::from_mins(45),
    );
    assert_eq!(book.lookup("DL1ABC", now), None);
}

/// PAN-359 AC9: two fresh entries sharing the base with no exact match
/// return nothing.
#[test]
fn ambiguous_base_call_returns_none() {
    let now = UtcTimestamp::now();
    let book = WorkedReferenceBook::new();
    book.record_feed(
        [entry("DL1ABC/P", "DA-0123"), entry("DL1ABC/M", "DA-0456")],
        now,
    );
    assert_eq!(book.lookup("DL1ABC", now), None);
    assert_eq!(book.lookup("DL1ABC/QRP", now), None);
    assert_eq!(book.lookup("DL1ABC/P", now), Some(pota("DA-0123")));
}

/// PAN-359 AC9: a later feed updates an entry's reference and sighting
/// time; a callsign missing from it keeps its last value until it ages
/// out, and is purged once it does.
#[test]
fn later_feed_updates_and_absent_calls_are_retained() {
    let t0 = UtcTimestamp::now() - SignedDuration::from_mins(40);
    let book = WorkedReferenceBook::new();
    book.record_feed([entry("DL1ABC", "DA-0123"), entry("K1XYZ", "K-0001")], t0);

    let t1 = t0 + SignedDuration::from_mins(20);
    book.record_feed([entry("DL1ABC", "DA-0456")], t1);
    assert_eq!(book.lookup("DL1ABC", t1), Some(pota("DA-0456")));
    // Absent from the later feed, still within 30 minutes of its sighting.
    assert_eq!(book.lookup("K1XYZ", t1), Some(pota("K-0001")));

    // The DL1ABC sighting moved to t1, so it is fresh 25 minutes after t1
    // although t0 is 45 minutes back; K1XYZ (last seen t0) has aged out.
    let t2 = t1 + SignedDuration::from_mins(25);
    assert_eq!(book.lookup("DL1ABC", t2), Some(pota("DA-0456")));
    assert_eq!(book.lookup("K1XYZ", t2), None);

    // A feed 31 minutes after K1XYZ's sighting purges it: even a lookup
    // at its original sighting time finds nothing.
    book.record_feed(
        [entry("DL1ABC", "DA-0456")],
        t0 + SignedDuration::from_mins(31),
    );
    assert_eq!(book.lookup("K1XYZ", t0), None);
    assert_eq!(book.lookup("DL1ABC", t2), Some(pota("DA-0456")));
}

/// Clones share one map: the bridge writes, the manager reads.
#[test]
fn clones_share_one_book() {
    let now = UtcTimestamp::now();
    let writer = WorkedReferenceBook::new();
    let reader = writer.clone();
    writer.record_feed([entry("DL1ABC", "DA-0123")], now);
    assert_eq!(reader.lookup("DL1ABC", now), Some(pota("DA-0123")));
}

/// Review fix: a sighting stamped ahead of the lookup time (the wall clock
/// stepped back after the fetch) is bounded by the same window, and such
/// an entry is purged by a later feed.
#[test]
fn future_sighting_is_bounded_by_the_window() {
    let now = UtcTimestamp::now();
    let book = WorkedReferenceBook::new();
    book.record_feed(
        [entry("DL1ABC", "DA-0123")],
        now + SignedDuration::from_mins(1),
    );
    assert_eq!(book.lookup("DL1ABC", now), Some(pota("DA-0123")));
    assert_eq!(
        book.lookup("DL1ABC", now - SignedDuration::from_mins(31)),
        None
    );

    book.record_feed(
        [entry("K1XYZ", "K-0001")],
        now - SignedDuration::from_mins(40),
    );
    assert_eq!(book.lookup("DL1ABC", now), None, "purged by the later feed");
    assert_eq!(
        book.lookup("K1XYZ", now - SignedDuration::from_mins(40)),
        Some(pota("K-0001"))
    );
}

/// PAN-359 AC9: the base-call fallback needs one side to be the bare base
/// call. Two different decorated forms of one call (a club call signed
/// `/4` and `/7` by different operators) never match each other.
#[test]
fn different_decorated_forms_never_match() {
    let now = UtcTimestamp::now();
    let book = WorkedReferenceBook::new();
    book.record_feed([entry("W1AW/4", "US-1234")], now);
    assert_eq!(book.lookup("W1AW/7", now), None);
    assert_eq!(book.lookup("EA8/W1AW", now), None);
    assert_eq!(book.lookup("W1AW", now), Some(pota("US-1234")));
    assert_eq!(book.lookup("W1AW/4", now), Some(pota("US-1234")));

    // A decorated partner call still matches the bare entry.
    let book = WorkedReferenceBook::new();
    book.record_feed([entry("W1AW", "US-1234")], now);
    assert_eq!(book.lookup("W1AW/7", now), Some(pota("US-1234")));
}
