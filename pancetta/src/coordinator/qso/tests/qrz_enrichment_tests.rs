use super::merge_qrz_lookup;
use pancetta_core::time::UtcTimestamp;
use pancetta_dx::QrzLookup;
use pancetta_qso::{GridSquares, QsoMetadata, SignalReports};

/// Build a completed QSO metadata with the given their-grid / notes so the
/// "only fill when missing" merge policy can be exercised in isolation.
fn metadata(their_grid: Option<&str>, notes: Option<&str>) -> QsoMetadata {
    let now = UtcTimestamp::now();
    QsoMetadata {
        qso_id: pancetta_qso::QsoId::new_v4(),
        our_callsign: "K5ARH".to_string(),
        their_callsign: Some("JA1ABC".to_string()),
        frequency: 14_074_000.0,
        completed_rf_frequency_hz: None,
        mode: "FT8".to_string(),
        first_answer_at: None,
        start_time: now,
        end_time: Some(now + jiff::SignedDuration::from_secs(90)),
        reports: SignalReports {
            sent: Some(-8),
            received: Some(-12),
        },
        grids: GridSquares {
            ours: Some("EM10".to_string()),
            theirs: their_grid.map(str::to_string),
        },
        their_state: None,
        contest_info: None,
        tags: std::collections::HashMap::new(),
        adif_field_types: std::collections::HashMap::new(),
        notes: notes.map(str::to_string),
        tx_parity: None,
        initiated_by: Default::default(),
        role: Default::default(),
        call_count: 0,
        first_call_at: None,
        last_call_at: None,
        progressed_this_cycle: false,
        stall_cycles: 0,
        last_known_good_offset_hz: None,
        advance_generation: 0,
        pre_switch_offset: None,
        hound: false,
        partner_freq: None,
        pending_freq_drift: None,
        hound_qsyed: false,
        fox: false,
        remote_origin: false,
        remote_client_key_id: None,
        tx_parity_provisional: false,
    }
}

fn lookup(grid: Option<&str>, name: Option<&str>) -> QrzLookup {
    QrzLookup {
        qth: None,
        call: Some("JA1ABC".to_string()),
        name: name.map(str::to_string),
        grid: grid.map(str::to_string),
        country: Some("Japan".to_string()),
        dxcc: Some("339".to_string()),
        state: None,
    }
}

/// A MISSING grid is filled from a valid QRZ grid.
#[test]
fn fills_missing_grid() {
    let mut md = metadata(None, None);
    let res = merge_qrz_lookup(&mut md, &lookup(Some("PM95"), None));
    assert!(res.grid_filled);
    assert_eq!(md.grids.theirs.as_deref(), Some("PM95"));
}

/// An empty-string grid counts as missing and is filled.
#[test]
fn fills_blank_grid() {
    let mut md = metadata(Some("  "), None);
    let res = merge_qrz_lookup(&mut md, &lookup(Some("PM95"), None));
    assert!(res.grid_filled);
    assert_eq!(md.grids.theirs.as_deref(), Some("PM95"));
}

/// An EXISTING (decoded/cqdx) grid is NEVER overridden by QRZ.
#[test]
fn never_overrides_existing_grid() {
    let mut md = metadata(Some("FN20"), None);
    let res = merge_qrz_lookup(&mut md, &lookup(Some("PM95"), None));
    assert!(!res.grid_filled);
    assert_eq!(md.grids.theirs.as_deref(), Some("FN20"));
}

/// An invalid QRZ grid is rejected (metadata left missing, not poisoned).
#[test]
fn rejects_invalid_grid() {
    let mut md = metadata(None, None);
    let res = merge_qrz_lookup(&mut md, &lookup(Some("not-a-grid!!"), None));
    assert!(!res.grid_filled);
    assert!(md.grids.theirs.is_none());
}

/// A name is appended to notes for logging/display.
#[test]
fn appends_name_to_empty_notes() {
    let mut md = metadata(None, None);
    let res = merge_qrz_lookup(&mut md, &lookup(Some("PM95"), Some("Taro")));
    assert!(res.name_added);
    assert_eq!(md.notes.as_deref(), Some("QRZ: Taro"));
}

/// A name is appended to (not clobbering) existing notes.
#[test]
fn appends_name_to_existing_notes() {
    let mut md = metadata(None, Some("contest exchange"));
    let res = merge_qrz_lookup(&mut md, &lookup(Some("PM95"), Some("Taro")));
    assert!(res.name_added);
    assert_eq!(md.notes.as_deref(), Some("contest exchange; QRZ: Taro"));
}

/// A name already present in notes is not appended twice (idempotent).
#[test]
fn does_not_duplicate_name() {
    let mut md = metadata(None, Some("QRZ: Taro"));
    let res = merge_qrz_lookup(&mut md, &lookup(Some("PM95"), Some("Taro")));
    assert!(!res.name_added);
    assert_eq!(md.notes.as_deref(), Some("QRZ: Taro"));
}

/// A lookup with nothing usable is a complete no-op.
#[test]
fn empty_lookup_is_noop() {
    let mut md = metadata(None, None);
    let before = md.clone();
    let res = merge_qrz_lookup(&mut md, &lookup(None, None));
    assert!(!res.grid_filled && !res.name_added);
    assert_eq!(md.grids.theirs, before.grids.theirs);
    assert_eq!(md.notes, before.notes);
}

// --- PAN-85: state fill + widened enrichment gate ---

/// A US-related station whose state is unknown: the metadata helper
/// defaults to a JA call, so the US cases must set it explicitly.
fn us_metadata(their_grid: Option<&str>) -> QsoMetadata {
    let mut m = metadata(their_grid, None);
    m.their_callsign = Some("W5ABC".to_string());
    m
}

#[test]
fn completed_state_history_includes_metadata_and_late_qrz_only_after_completion() {
    let cache = crate::priority_evaluator::CachedStationLookup::new();
    let mut m = us_metadata(Some("EM34"));
    cache.record_state("W5ABC", "AR");
    cache.record_completed_contact(&m);
    assert!(
        cache.is_state_needed("AR"),
        "missing completion state is not invented"
    );
    m.their_state = Some("TX".into());
    cache.record_completed_contact(&m);
    assert!(!cache.is_state_needed("TX"));
    m.their_state = None;
    merge_qrz_lookup(
        &mut m,
        &QrzLookup {
            state: Some("AR".into()),
            ..lookup(None, None)
        },
    );
    cache.record_completed_state(&m);
    assert!(!cache.is_state_needed("AR"));
    assert_eq!(cache.state_for("W5ABC").as_deref(), Some("AR"));
}

#[tokio::test]
async fn live_and_replay_display_history_seeds_agree() {
    use pancetta_qso::async_database::QsoDatabase;
    use pancetta_qso::priority::WorkedStationLookup;
    let dir = tempfile::tempdir().unwrap();
    let adif_path = dir.path().join("history.adi");
    let fixture = "<EOH>\n<CALL:5>W5ABC <QSO_DATE:8>20250102 <TIME_ON:6>120000 <MODE:3>FT8 <FREQ:9>14.074000 <GRIDSQUARE:4>EM34 <STATE:2>CA <EOR>\n<CALL:5>W5ABC <QSO_DATE:8>20250101 <TIME_ON:6>120000 <MODE:3>FT8 <FREQ:9>14.074000 <GRIDSQUARE:4>EM34 <STATE:2>AR <EOR>";
    tokio::fs::write(&adif_path, fixture).await.unwrap();
    let live = QsoDatabase::new_in_memory().await.unwrap();
    let processor = pancetta_qso::AdifProcessor::new();
    live.insert_adif_records(
        &processor,
        &processor.parse_string(fixture).unwrap().records,
    )
    .await
    .unwrap();
    let replay = QsoDatabase::replay_from_adif(":memory:", &adif_path)
        .await
        .unwrap();
    for db in [&live, &replay] {
        let cache = crate::priority_evaluator::CachedStationLookup::new();
        cache.seed_display_history(db).await;
        assert!(!cache.is_state_needed("AR"));
        assert!(!cache.is_state_needed("CA"));
        assert_eq!(cache.state_for("W5ABC").as_deref(), Some("CA"));
        assert!(!cache.is_grid_needed_on_band("em34ab", 14_074_000.0));
        assert!(cache.is_grid_needed_on_band("EM34", 7_074_000.0));
    }
}

#[test]
fn merge_qrz_fills_missing_state() {
    let mut m = us_metadata(Some("EM34"));
    let l = QrzLookup {
        state: Some("AR".into()),
        ..lookup(None, None)
    };
    let r = merge_qrz_lookup(&mut m, &l);
    assert_eq!(m.their_state.as_deref(), Some("AR"));
    assert!(r.state_filled);
}

/// QRZ's casing/padding is normalized to the canonical ADIF code before
/// it is stored, so the uploaded `STATE` field is always enumerated.
#[test]
fn merge_qrz_normalizes_state_case_and_padding() {
    let mut m = us_metadata(Some("EM34"));
    let l = QrzLookup {
        state: Some(" ar ".into()),
        ..lookup(None, None)
    };
    let r = merge_qrz_lookup(&mut m, &l);
    assert_eq!(m.their_state.as_deref(), Some("AR"));
    assert!(r.state_filled);
}

#[test]
fn merge_qrz_never_overrides_existing_state() {
    let mut m = us_metadata(Some("EM34"));
    m.their_state = Some("TX".into());
    let l = QrzLookup {
        state: Some("AR".into()),
        ..lookup(None, None)
    };
    let r = merge_qrz_lookup(&mut m, &l);
    assert_eq!(m.their_state.as_deref(), Some("TX"));
    assert!(!r.state_filled);
}

#[test]
fn merge_qrz_ignores_blank_state() {
    let mut m = us_metadata(Some("EM34"));
    let l = QrzLookup {
        state: Some("   ".into()),
        ..lookup(None, None)
    };
    merge_qrz_lookup(&mut m, &l);
    assert!(m.their_state.is_none());
}

/// A non-US station's free-text QRZ `<state>` (a Japanese prefecture, a
/// Canadian province) is never stored -- `their_state` rides into the
/// uploaded ADIF `STATE`, an enumerated field.
#[test]
fn merge_qrz_rejects_state_for_non_us_entity() {
    let mut m = metadata(Some("PM95"), None); // JA1ABC
    let l = QrzLookup {
        state: Some("Tokyo".into()),
        ..lookup(None, None)
    };
    let r = merge_qrz_lookup(&mut m, &l);
    assert!(m.their_state.is_none());
    assert!(!r.state_filled);
}

/// Even for a US station, a value that is not a known US subdivision code
/// is dropped rather than stored verbatim.
#[test]
fn merge_qrz_rejects_unrecognized_state_value() {
    let mut m = us_metadata(Some("EM34"));
    let l = QrzLookup {
        state: Some("Arkansas".into()),
        ..lookup(None, None)
    };
    let r = merge_qrz_lookup(&mut m, &l);
    assert!(m.their_state.is_none());
    assert!(!r.state_filled);
}

#[test]
fn qrz_enrichment_gate_covers_us_state_gap() {
    use super::needs_qrz_enrichment;

    // grid known + US-related + state unknown  -> now fires (PAN-85)
    let mut m = metadata(Some("EM34"), None);
    m.their_callsign = Some("W5ABC".to_string());
    assert!(needs_qrz_enrichment(&m));

    // grid known + US-related + state known    -> does not fire
    m.their_state = Some("AR".to_string());
    assert!(!needs_qrz_enrichment(&m));

    // grid known + NOT US-related + no state   -> does not fire (D7)
    let j = metadata(Some("PM95"), None);
    assert!(!needs_qrz_enrichment(&j));

    // grid missing -> fires regardless of entity (pre-existing behaviour)
    let j2 = metadata(None, None);
    assert!(needs_qrz_enrichment(&j2));
}

/// A completed QSO logged under a resolved FT8 hash render ("<W5ABC>")
/// is enriched from the plain call's QRZ record: the gate already treats
/// it as United States, so the cache key and the QRZ request must use
/// the resolved call too. The cache is seeded with a hit under "W5ABC",
/// so the fixed path never touches the network. The unresolved "<...>"
/// placeholder names no station: it is neither looked up nor cached.
#[tokio::test]
async fn enrichment_resolves_hash_rendered_call_before_cache_and_lookup() {
    use super::maybe_enrich_from_qrz;
    use std::collections::HashMap;
    use tokio::sync::Mutex;

    let mut m = us_metadata(Some("EM34"));
    m.their_callsign = Some("<W5ABC>".to_string());
    let client = pancetta_dx::QrzXmlClient::new("user", "pass", "pancetta-test");
    let mut seeded = HashMap::new();
    seeded.insert(
        "W5ABC".to_string(),
        Some(QrzLookup {
            call: Some("W5ABC".to_string()),
            state: Some("AR".into()),
            ..lookup(None, None)
        }),
    );
    let cache = Mutex::new(seeded);

    maybe_enrich_from_qrz(&mut m, &client, &cache).await;

    assert_eq!(m.their_state.as_deref(), Some("AR"));

    // Grid missing, so the gate fires; the placeholder still returns
    // before the cache or the client is touched.
    let mut p = metadata(None, None);
    p.their_callsign = Some("<...>".to_string());
    let empty = Mutex::new(HashMap::new());
    maybe_enrich_from_qrz(&mut p, &client, &empty).await;
    assert!(empty.lock().await.is_empty());
    assert!(p.grids.theirs.is_none());
}

/// Collects formatted log output so a test can assert on it.
#[derive(Clone)]
struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Serve each canned raw HTTP response to one connection, in order, and
/// return the mock's QRZ base URL.
fn serve(responses: Vec<String>) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for r in responses {
            let Ok((mut s, _)) = listener.accept() else {
                return;
            };
            let mut b = [0u8; 4096];
            let _ = s.read(&mut b);
            let _ = s.write_all(r.as_bytes());
        }
    });
    format!("http://{addr}/xml/current/")
}

/// Run `maybe_enrich_from_qrz` for a grid-less QSO with JA1ABC while every
/// target is captured at TRACE, and return what was logged.
async fn enrich_capturing_logs(client: &pancetta_dx::QrzXmlClient) -> String {
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = Capture(buf.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let mut m = metadata(None, None);
    let cache = tokio::sync::Mutex::new(std::collections::HashMap::new());
    super::maybe_enrich_from_qrz(&mut m, client, &cache).await;
    // Copy out in its own statement so the lock guard drops before `buf`.
    let bytes = buf.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}

const QRZ_PASSWORD: &str = "pw-7Hq2-secret";
const QRZ_SESSION_KEY: &str = "sk-4c1e9b7f-secret";

/// Authentication fails at the transport level: the logged failure must
/// not contain the password.
#[tokio::test]
async fn qrz_auth_failure_log_omits_password() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let client = pancetta_dx::QrzXmlClient::new("user", QRZ_PASSWORD, "pancetta-test")
        .with_mock_base_url(format!("http://{closed}/xml/current/"))
        .unwrap();
    let logs = enrich_capturing_logs(&client).await;
    assert!(logs.contains("QRZ lookup failed for JA1ABC"), "{logs}");
    assert!(!logs.contains(QRZ_PASSWORD), "{logs}");
}

/// Authentication succeeds, then the session-keyed lookup gets a 503: the
/// logged failure must contain neither the session key nor the password.
#[tokio::test]
async fn qrz_lookup_failure_log_omits_session_key() {
    let auth =
        format!("<QRZDatabase><Session><Key>{QRZ_SESSION_KEY}</Key></Session></QRZDatabase>");
    let base = serve(vec![
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{auth}",
            auth.len()
        ),
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string(),
    ]);
    let client = pancetta_dx::QrzXmlClient::new("user", QRZ_PASSWORD, "pancetta-test")
        .with_mock_base_url(base)
        .unwrap();
    let logs = enrich_capturing_logs(&client).await;
    assert!(logs.contains("QRZ lookup failed for JA1ABC"), "{logs}");
    assert!(!logs.contains(QRZ_SESSION_KEY), "{logs}");
    assert!(!logs.contains(QRZ_PASSWORD), "{logs}");
}
