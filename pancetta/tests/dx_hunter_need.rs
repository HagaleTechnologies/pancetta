//! PAN-86: real cache -> prepared App status -> Ratatui, with real ADIF persistence.
use pancetta_lib::priority_evaluator::CachedStationLookup;
use pancetta_qso::{adif_log_writer::AdifLogWriter, async_database::QsoDatabase, AdifProcessor};
use pancetta_tui::{app::App, tui_runner::CqdxSpotInfo, Config, DecodedMessageView};
use ratatui::{backend::TestBackend, buffer::Buffer, style::Modifier, Terminal};

fn contact(state: &str, date: &str) -> pancetta_qso::QsoMetadata {
    let processor = AdifProcessor::new();
    let input = format!("<EOH>\n<CALL:5>W5ABC <STATION_CALLSIGN:5>K1OWN <QSO_DATE:8>{date} <TIME_ON:6>120000 <MODE:3>FT8 <FREQ:9>14.074000 <BAND:3>20m <GRIDSQUARE:6>EM34ab <STATE:2>{state} <EOR>");
    let file = processor.parse_string(&input).unwrap();
    processor.adif_to_qso(&processor.record_to_qso(&file.records[0]).unwrap())
}

async fn add_rows(app: &mut App) {
    app.add_decoded_message(DecodedMessageView {
        timestamp: chrono::Utc::now(),
        frequency: app.station_info.operating_frequency,
        mode: "FT8".into(),
        snr: -10,
        delta_time: 0.0,
        delta_freq: 1500.0,
        call_sign: Some("W5ABC".into()),
        grid_square: Some("em34ab".into()),
        message: "CQ W5ABC EM34".into(),
        distance: None,
        bearing: None,
        slot_parity: None,
        is_directed_at_us: false,
        worked_before: false,
        needed: false,
        atno: false,
        band_needed: false,
        priority_score: None,
        is_own_tx: false,
    })
    .await
    .unwrap();
    app.merge_spot_groups(&[CqdxSpotInfo {
        dx_call: "W1XYZ".into(),
        band: "40m".into(),
        mode: "FT8".into(),
        frequency_hz: 7_074_000,
        grid: Some("EM34cd".into()),
        rarity_tier: "common".into(),
        reporter_count: 3,
        best_snr: Some(-10),
        confidence: 0.9,
        first_seen: chrono::Utc::now().timestamp(),
        last_seen: chrono::Utc::now().timestamp(),
        is_notable: false,
        notable_type: None,
        entity_name: "United States".into(),
        needed: false,
        atno: false,
    }]);
    for call in ["W5ABC", "W1XYZ"] {
        app.set_station_state(call, "AR");
    }
}

fn render(app: &mut App, cache: &CachedStationLookup) -> Buffer {
    app.refresh_dx_need(Some(cache));
    let mut terminal = Terminal::new(TestBackend::new(100, 8)).unwrap();
    terminal
        .draw(|f| pancetta_tui::ui::dx_hunter::render_dx_hunter(f, f.area(), app).unwrap())
        .unwrap();
    terminal.backend().buffer().clone()
}

fn text(buf: &Buffer) -> String {
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_token_style(buf: &Buffer, token: &str, fg: ratatui::style::Color, bold: bool) {
    let mut count = 0;
    for y in 0..buf.area.height {
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        if let Some(byte) = row.find(token) {
            let x = row[..byte].chars().count() as u16;
            assert_eq!(buf[(x, y)].fg, fg, "{row}");
            assert_eq!(buf[(x, y)].modifier.contains(Modifier::BOLD), bold, "{row}");
            count += 1;
        }
    }
    assert_eq!(count, 2, "expected both existing rows: {}", text(buf));
}

#[tokio::test]
async fn completed_and_logged_contact_clears_existing_rows_across_bands_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let adif = dir.path().join("history.adi");
    let writer = AdifLogWriter::open(&adif).await.unwrap();
    let empty = QsoDatabase::replay_from_adif(":memory:", &adif)
        .await
        .unwrap();
    let cache = CachedStationLookup::new();
    cache.seed_display_history(&empty).await;
    cache.record_state("W5ABC", "AR"); // knowledge alone is not a contact
    let mut app = App::new(Config::default(), None).await.unwrap();
    app.station_info.operating_frequency = 14.074;
    add_rows(&mut app).await;
    let before = render(&mut app, &cache);
    assert_eq!(text(&before).matches("United States - +AR").count(), 2);
    assert_token_style(&before, "+EM34", app.theme.error_color(), true);
    assert_token_style(&before, "+AR", app.theme.error_color(), true);
    println!("Before completion:\n{}", text(&before));

    let metadata = contact("AR", "20250101");
    cache.record_completed_contact(&metadata); // exact helper used by QsoCompleted
    writer.append(&metadata).await.unwrap();
    let index = QsoDatabase::replay_from_adif(dir.path().join("qso.db"), &adif)
        .await
        .unwrap();
    assert_eq!(index.count_qsos().await.unwrap(), 1);
    let after = render(&mut app, &cache); // no new spot, decode, or row
    assert_eq!(app.dx_stations.len(), 2);
    assert!(!text(&after).contains("+EM34"));
    assert!(!text(&after).contains("+AR"));
    assert_token_style(&after, "EM34", app.theme.foreground_color(), false);
    assert_token_style(&after, "AR", app.theme.accent_color(), false);
    println!("After completion, without another spot:\n{}", text(&after));

    app.band_down(); // 30m
    app.band_down(); // 40m
    assert_eq!(app.station_info.operating_frequency, 7.074);
    add_rows(&mut app).await;
    let forty = render(&mut app, &cache);
    assert_eq!(text(&forty).matches("+EM34").count(), 2);
    assert!(!text(&forty).contains("+AR"));
    app.band_up();
    app.band_up(); // restore original 20m snapshot
    let restored = render(&mut app, &cache);
    assert!(!text(&restored).contains("+EM34"));
    assert!(!text(&restored).contains("+AR"));
    assert_eq!(app.dx_stations.len(), 2);

    let fresh = CachedStationLookup::new();
    fresh.seed_display_history(&index).await;
    let restarted_render = render(&mut app, &fresh);
    assert_eq!(
        text(&restarted_render)
            .matches("United States - AR")
            .count(),
        2
    );
    assert!(!text(&restarted_render).contains("+EM34"));
    index.close().await;

    // A station moving does not erase the older worked state on restart.
    let moved = contact("CA", "20250102");
    cache.record_completed_contact(&moved);
    writer.append(&moved).await.unwrap();
    let session = render(&mut app, &cache);
    assert!(text(&session).contains("United States - CA"));
    let rebuilt = QsoDatabase::replay_from_adif(":memory:", &adif)
        .await
        .unwrap();
    let restarted = CachedStationLookup::new();
    restarted.seed_display_history(&rebuilt).await;
    assert!(!restarted.is_state_needed("AR"));
    assert!(!restarted.is_state_needed("CA"));
    assert_eq!(restarted.state_for("W5ABC").as_deref(), Some("CA"));
    let restarted_render = render(&mut app, &restarted);
    assert!(text(&restarted_render).contains("United States - CA"));
    assert!(text(&restarted_render).contains("United States - AR"));
    assert!(!text(&restarted_render).contains("+EM34"));
    assert!(!text(&restarted_render).contains("- +"));
}

#[tokio::test]
async fn unavailable_history_preserves_known_values_but_empty_history_enables_need() {
    let cache = CachedStationLookup::new();
    let mut app = App::new(Config::default(), None).await.unwrap();
    app.station_info.operating_frequency = 14.074;
    add_rows(&mut app).await;
    let loading = render(&mut app, &cache);
    assert_eq!(text(&loading).matches("United States - AR").count(), 2);
    assert!(!text(&loading).contains("+EM34"));
    let db = QsoDatabase::new_in_memory().await.unwrap();
    cache.seed_display_history(&db).await;
    let available = render(&mut app, &cache);
    assert_eq!(text(&available).matches("+EM34").count(), 2);
    assert_eq!(text(&available).matches("- +AR").count(), 2);
    db.close().await;
    cache.seed_display_history(&db).await;
    let failed = render(&mut app, &cache);
    assert_eq!(text(&failed).matches("United States - AR").count(), 2);
    assert!(!text(&failed).contains("+EM34"));
    assert_token_style(&failed, "EM34", app.theme.foreground_color(), false);
}
