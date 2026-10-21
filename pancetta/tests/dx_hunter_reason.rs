//! PAN-94: the shared lookup's decision inputs reach the operator's screen.
use pancetta_lib::priority_evaluator::CachedStationLookup;
use pancetta_qso::priority::{PriorityScorer, PriorityWeights, WorkedStationLookup};
use pancetta_tui::{
    app::{ActivePanel, App},
    Config, DecodedMessageView,
};
use ratatui::{backend::TestBackend, Terminal};

async fn decode(app: &mut App, lookup: &CachedStationLookup, call: &str) {
    let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup.clone()));
    let explained = scorer.score_tiered_explained(call, None, -3, 14_074_000.0);
    app.add_decoded_message(DecodedMessageView {
        timestamp: chrono::Utc::now(),
        frequency: 14.074,
        mode: "FT8".into(),
        snr: -3,
        delta_time: 0.0,
        delta_freq: 1500.0,
        call_sign: Some(call.into()),
        grid_square: None,
        message: format!("CQ {call}"),
        distance: None,
        bearing: None,
        slot_parity: None,
        is_directed_at_us: false,
        worked_before: lookup.is_duplicate(call, 14_074_000.0),
        needed: lookup.is_needed_dxcc(call),
        atno: lookup.is_atno(call),
        band_needed: lookup.is_dxcc_needed_on_band(call, 14_074_000.0),
        priority_score: Some(explained.score.as_display_u32()),
        priority_factors: Some(explained.factors),
        is_own_tx: false,
    })
    .await
    .unwrap();
    app.active_panel = ActivePanel::DxHunter;
    app.clamp_dx_hunter_selection();
}

fn render(app: &mut App, overlay: bool, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| {
            if overlay {
                pancetta_tui::ui::why::render_why_overlay(f, f.area(), app);
            } else {
                pancetta_tui::ui::station_card::render_station_card(f, f.area(), app).unwrap();
            }
        })
        .unwrap();
    let buf = terminal.backend().buffer();
    (0..height)
        .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn recorded_failure_reaches_the_card_and_overlay() {
    let lookup = CachedStationLookup::new();
    lookup.record_failure("K1ABC/P");
    let mut app = App::new(Config::default(), None).await.unwrap();
    decode(&mut app, &lookup, "K1ABC/P").await;
    let card = render(&mut app, false, 130, 6);
    assert!(card.contains("QSO failed this session"), "{card}");
    assert!(card.contains("maybe POTA/SOTA (/P suffix)"), "{card}");
    let overlay = render(&mut app, true, 100, 30);
    assert!(
        overlay
            .lines()
            .any(|l| l.contains("QSO failed this session") && l.contains("yes")),
        "{overlay}"
    );
    assert!(
        overlay.contains("not a confirmed park or summit activation"),
        "{overlay}"
    );
}

#[tokio::test]
async fn working_a_station_lowers_its_score_and_says_so() {
    let lookup = CachedStationLookup::new();
    // A populated cqdx list distinguishes a band-new entity from the
    // existing empty-list fallback that considers every entity needed.
    lookup.update_needed_dxcc(std::collections::HashSet::from(["DL".to_owned()]));
    let mut app = App::new(Config::default(), None).await.unwrap();
    decode(&mut app, &lookup, "JA1ABC").await;
    let before = app.dx_stations["JA1ABC"].priority_score;
    assert!((4000..5000).contains(&before));
    lookup.record_worked("JA1ABC", "20M");
    decode(&mut app, &lookup, "JA1ABC").await;
    assert!(app.dx_stations["JA1ABC"].priority_score < 4000);
    let card = render(&mut app, false, 130, 6);
    assert!(card.contains("already worked on 20M"), "{card}");
}
