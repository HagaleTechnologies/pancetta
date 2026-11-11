//! PAN-347: needed, ATNO, band-slot, mode and confirmed status come from the
//! local logbook (qsos.adi, through the index) while cqdx.io has not answered.
//! Each Gherkin scenario runs from an ADIF file through the index, the
//! lookup, the scorer and the rendered TUI.
use pancetta_lib::priority_evaluator::CachedStationLookup;
use pancetta_qso::async_database::QsoDatabase;
use pancetta_qso::priority::{PriorityScorer, PriorityWeights, WorkedStationLookup};
use pancetta_tui::{
    app::{ActivePanel, App},
    Config, DecodedMessageView,
};
use ratatui::{backend::TestBackend, Terminal};

/// ADIF text for `(call, mode, freq_mhz, extra_fields)`, one minute apart.
fn adif(records: &[(String, &str, f64, &str)]) -> String {
    let mut out = String::from("PAN-347 e2e log\n<ADIF_VER:5>3.1.4 <EOH>\n");
    for (i, (call, mode, freq, extra)) in records.iter().enumerate() {
        let freq = format!("{freq:.6}");
        let time = format!("{:02}{:02}00", (i / 60) % 24, i % 60);
        let date = format!(
            "2025{:02}{:02}",
            1 + (i / 1440) % 12,
            1 + (i / 60 / 24) % 28
        );
        out.push_str(&format!(
            "<CALL:{}>{call} <QSO_DATE:8>{date} <TIME_ON:6>{time} <MODE:{}>{mode} <FREQ:{}>{freq} {extra} <EOR>\n",
            call.len(),
            mode.len(),
            freq.len(),
        ));
    }
    out
}

/// Index `adif` from a file, seed the lookup the way the QSO task does, and
/// take cqdx.io down (configured, unreachable).
async fn offline_lookup(adif: &str) -> (tempfile::TempDir, CachedStationLookup) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("qsos.adi");
    std::fs::write(&path, adif).unwrap();
    let db = QsoDatabase::replay_from_adif(":memory:", &path)
        .await
        .unwrap();
    let lookup = CachedStationLookup::new();
    lookup.set_home_entity(pancetta_core::dxcc::entity_for_callsign("K5ARH"));
    lookup.seed_worked_history(&db).await.unwrap();
    lookup.seed_display_history(&db).await;
    lookup.set_need_lists_loading();
    lookup.mark_need_lists_unavailable("connection refused", true);
    (tmp, lookup)
}

async fn decode(app: &mut App, lookup: &CachedStationLookup, call: &str, mhz: f64, mode: &str) {
    let hz = mhz * 1e6;
    let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup.clone()));
    let explained = scorer.score_tiered_explained(call, None, -10, hz);
    let (needed, atno) = lookup.needed_atno_shown(call);
    app.add_decoded_message(DecodedMessageView {
        timestamp: chrono::Utc::now(),
        frequency: mhz,
        mode: mode.into(),
        snr: -10,
        delta_time: 0.0,
        delta_freq: 1500.0,
        call_sign: Some(call.into()),
        grid_square: None,
        message: format!("CQ {call}"),
        distance: None,
        bearing: None,
        slot_parity: None,
        is_directed_at_us: false,
        worked_before: lookup.is_duplicate(call, hz),
        needed,
        atno,
        band_needed: lookup.is_dxcc_needed_on_band(call, hz),
        priority_score: Some(explained.score.as_display_u32()),
        priority_factors: Some(explained.factors),
        is_own_tx: false,
    })
    .await
    .unwrap();
    app.station_info.operating_frequency = mhz;
    app.refresh_dx_need(Some(lookup));
    app.active_panel = ActivePanel::DxHunter;
    app.clamp_dx_hunter_selection();
}

enum View {
    Hunter,
    Card,
    Overlay,
}

fn render(app: &mut App, view: View, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| match view {
            View::Hunter => {
                pancetta_tui::ui::dx_hunter::render_dx_hunter(f, f.area(), app).unwrap()
            }
            View::Card => {
                pancetta_tui::ui::station_card::render_station_card(f, f.area(), app).unwrap()
            }
            View::Overlay => pancetta_tui::ui::why::render_why_overlay(f, f.area(), app),
        })
        .unwrap();
    let buf = terminal.backend().buffer();
    (0..height)
        .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn row_of<'a>(screen: &'a str, call: &str) -> &'a str {
    screen
        .lines()
        .find(|l| l.contains(call))
        .unwrap_or_else(|| panic!("no row for {call}:\n{screen}"))
}

/// 400 QSOs across 20 entities, all on 20 m FT8.
fn four_hundred_qsos() -> String {
    let prefixes = [
        "JA1", "DL1", "G4", "F5", "I1", "EA1", "VE3", "VK2", "PY2", "LU1", "ZS1", "UA3", "SM5",
        "OH2", "OZ1", "LA1", "ON4", "PA3", "HB9", "SP5",
    ];
    let records: Vec<(String, &str, f64, &str)> = prefixes
        .iter()
        .flat_map(|p| {
            (0..20u8).map(move |j| (format!("{p}A{}", (b'A' + j) as char), "FT8", 14.074, ""))
        })
        .collect();
    assert_eq!(records.len(), 400);
    adif(&records)
}

#[tokio::test]
async fn needed_dxcc_works_offline() {
    let (_tmp, lookup) = offline_lookup(&four_hundred_qsos()).await;
    let mut app = App::new(Config::default(), None).await.unwrap();
    decode(&mut app, &lookup, "ZL1ABC", 14.074, "FT8").await;

    let hunter = render(&mut app, View::Hunter, 220, 12);
    let row = row_of(&hunter, "ZL1ABC");
    assert!(row.contains("!ZL1ABC"), "{row}");
    assert!(row.contains("all-time new entity (local logbook)"), "{row}");

    let card = render(&mut app, View::Card, 130, 6);
    assert!(card.contains("ATNO\u{2605}"), "{card}");

    let overlay = render(&mut app, View::Overlay, 120, 40);
    assert!(
        overlay.lines().any(|l| l.contains("all-time new entity")
            && l.contains("yes")
            && l.contains("local logbook")),
        "{overlay}"
    );
    for screen in [&hunter, &card, &overlay] {
        assert!(!screen.contains("CQDX NEEDS UNKNOWN"), "{screen}");
    }
}

#[tokio::test]
async fn band_slot_and_mode_are_tracked() {
    let (_tmp, lookup) = offline_lookup(&adif(&[("DL1ABC".into(), "FT8", 14.074, "")])).await;

    // 40 m FT4: new band-slot and new mode.
    let mut app = App::new(Config::default(), None).await.unwrap();
    decode(&mut app, &lookup, "DL1XYZ", 7.0475, "FT4").await;
    let hunter = render(&mut app, View::Hunter, 220, 12);
    let row = row_of(&hunter, "DL1XYZ");
    assert!(row.contains("\u{25b2}DL1XYZ"), "{row}");
    assert!(row.contains("new entity on 40M; new mode FT4"), "{row}");

    // 20 m FT4: band worked, mode not.
    let mut app = App::new(Config::default(), None).await.unwrap();
    decode(&mut app, &lookup, "DL1XYZ", 14.080, "FT4").await;
    let hunter = render(&mut app, View::Hunter, 220, 12);
    let row = row_of(&hunter, "DL1XYZ");
    assert!(!row.contains("\u{25b2}DL1XYZ"), "{row}");
    assert!(row.contains("new mode FT4"), "{row}");

    // 40 m FT8: band new, mode worked.
    let mut app = App::new(Config::default(), None).await.unwrap();
    decode(&mut app, &lookup, "DL1XYZ", 7.074, "FT8").await;
    let hunter = render(&mut app, View::Hunter, 220, 12);
    let row = row_of(&hunter, "DL1XYZ");
    assert!(row.contains("\u{25b2}DL1XYZ"), "{row}");
    assert!(!row.contains("new mode"), "{row}");
}

#[tokio::test]
async fn confirmed_status_is_distinct_from_worked() {
    let (_tmp, lookup) = offline_lookup(&adif(&[
        ("DL1ABC".into(), "FT8", 14.074, ""),
        ("JA1ABC".into(), "FT8", 14.074, "<LOTW_QSL_RCVD:1>Y"),
    ]))
    .await;

    let mut app = App::new(Config::default(), None).await.unwrap();
    decode(&mut app, &lookup, "DL1XYZ", 14.074, "FT8").await;
    let card = render(&mut app, View::Card, 130, 6);
    assert!(card.contains("worked-unconfirmed"), "{card}");
    let overlay = render(&mut app, View::Overlay, 120, 40);
    assert!(
        overlay.contains("Local logbook (all bands, not scored)"),
        "{overlay}"
    );
    assert!(
        overlay
            .lines()
            .any(|l| l.contains("entity") && l.contains("worked-unconfirmed")),
        "{overlay}"
    );

    let mut app = App::new(Config::default(), None).await.unwrap();
    decode(&mut app, &lookup, "JA1XYZ", 14.074, "FT8").await;
    let card = render(&mut app, View::Card, 130, 6);
    assert!(card.contains(" confirmed"), "{card}");
    assert!(!card.contains("worked-unconfirmed"), "{card}");
}

#[tokio::test]
async fn unconfirmed_policy_renders_needed_then_confirmed() {
    let text = adif(&[("DL1ABC".into(), "FT8", 14.074, "")]);
    let (tmp, lookup) = offline_lookup(&text).await;
    lookup.set_needed_means(pancetta_config::NeededMeans::Unconfirmed);
    let mut app = App::new(Config::default(), None).await.unwrap();
    decode(&mut app, &lookup, "DL1XYZ", 14.074, "FT8").await;
    let hunter = render(&mut app, View::Hunter, 220, 12);
    let row = row_of(&hunter, "DL1XYZ");
    assert!(
        row.contains("+▲DL1XYZ") && !row.contains("!DL1XYZ"),
        "{row}"
    );
    assert!(row.contains("unconfirmed entity on 20M"), "{row}");
    assert!(!row.contains("new entity"), "{row}");
    let card = render(&mut app, View::Card, 130, 6);
    assert!(card.contains(" needed"), "{card}");
    let overlay = render(&mut app, View::Overlay, 140, 45);
    assert!(
        overlay.contains("Needed policy: unconfirmed (local logbook)"),
        "{overlay}"
    );
    assert!(overlay.contains("Local logbook (all bands)"), "{overlay}");
    assert!(
        overlay.lines().any(|l| l.contains("entity needed on band")
            && l.contains("yes")
            && l.contains("local logbook")),
        "{overlay}"
    );
    assert!(
        overlay
            .lines()
            .any(|l| l.contains("all-time new entity") && l.contains("no")),
        "{overlay}"
    );

    let home = tmp.path().join("home");
    let logdir = home.join(".pancetta");
    std::fs::create_dir_all(&logdir).unwrap();
    let local_adif = logdir.join("qsos.adi");
    std::fs::write(&local_adif, &text).unwrap();
    let dbpath = logdir.join("qso.db");
    let stale = QsoDatabase::replay_from_adif(&dbpath, &local_adif)
        .await
        .unwrap();
    stale.close().await;
    let report = tmp.path().join("lotw.adi");
    std::fs::write(
        &report,
        adif(&[(
            "DL1ABC".into(),
            "FT8",
            14.074,
            "<LOTW_QSL_RCVD:1>Y <LOTW_QSLRDATE:8>20261111",
        )]),
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pancetta"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .arg("import-confirmations")
        .arg(&report)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rebuilt =
        match pancetta_lib::coordinator::prepare_local_qso_index(&local_adif, &dbpath).await {
            pancetta_lib::coordinator::LocalIndex::Ready(db) => db,
            _ => panic!("confirmation import must rebuild a ready index"),
        };
    let lookup = CachedStationLookup::new();
    lookup.set_needed_means(pancetta_config::NeededMeans::Unconfirmed);
    lookup.seed_worked_history(&rebuilt).await.unwrap();
    lookup.seed_display_history(&rebuilt).await;
    let mut app = App::new(Config::default(), None).await.unwrap();
    decode(&mut app, &lookup, "DL1XYZ", 14.074, "FT8").await;
    let hunter = render(&mut app, View::Hunter, 220, 12);
    let row = row_of(&hunter, "DL1XYZ");
    assert!(
        !row.contains("+▲DL1XYZ") && !row.contains("+DL1XYZ"),
        "{row}"
    );
    assert!(row.contains("no award need"), "{row}");
    let card = render(&mut app, View::Card, 130, 6);
    assert!(card.contains(" confirmed"), "{card}");
    let overlay = render(&mut app, View::Overlay, 140, 45);
    assert!(
        overlay
            .lines()
            .any(|l| l.contains("entity needed on band") && l.contains("no")),
        "{overlay}"
    );
}

#[tokio::test]
async fn confirmation_on_other_band_keeps_band_need() {
    let (_tmp, lookup) = offline_lookup(&adif(&[(
        "DL1ABC".into(),
        "FT4",
        7.0475,
        "<LOTW_QSL_RCVD:1>V",
    )]))
    .await;
    lookup.set_needed_means(pancetta_config::NeededMeans::Unconfirmed);
    let mut app = App::new(Config::default(), None).await.unwrap();
    decode(&mut app, &lookup, "DL1XYZ", 14.074, "FT8").await;
    assert_eq!(lookup.needed_atno_shown("DL1XYZ"), (false, false));
    let hunter = render(&mut app, View::Hunter, 220, 12);
    let row = row_of(&hunter, "DL1XYZ");
    assert!(row.contains("▲DL1XYZ"), "{row}");
    assert!(row.contains("unconfirmed entity on 20M"), "{row}");
    assert!(
        !row.contains("all-time new") && !row.contains("new entity"),
        "{row}"
    );
    let overlay = render(&mut app, View::Overlay, 140, 45);
    assert!(
        overlay
            .lines()
            .any(|l| l.contains("entity needed on band") && l.contains("yes")),
        "{overlay}"
    );
}

#[tokio::test]
async fn unconfirmed_policy_preserves_unknown_and_empty_states() {
    use pancetta_tui::readiness::{readiness_items, NeedListHealth, StationReadiness};
    let lookup = CachedStationLookup::new();
    lookup.set_needed_means(pancetta_config::NeededMeans::Unconfirmed);
    let mut app = App::new(Config::default(), None).await.unwrap();
    decode(&mut app, &lookup, "DL1XYZ", 0.0, "FT8").await;
    let hunter = render(&mut app, View::Hunter, 220, 12);
    assert!(
        hunter.contains("maybe needed entity (cqdx list not loaded)"),
        "{hunter}"
    );
    app.station_readiness = Some(StationReadiness {
        cqdx_dxcc_needs: NeedListHealth::Unavailable {
            error: "offline".into(),
            retrying: true,
        },
        ..Default::default()
    });
    assert!(readiness_items(&app)
        .iter()
        .any(|i| i.label.contains("CQDX NEEDS UNKNOWN")));
    let (_tmp, lookup) = offline_lookup(&adif(&[])).await;
    lookup.set_needed_means(pancetta_config::NeededMeans::Unconfirmed);
    let mut app = App::new(Config::default(), None).await.unwrap();
    decode(&mut app, &lookup, "DL1XYZ", 14.074, "FT8").await;
    let hunter = render(&mut app, View::Hunter, 220, 12);
    assert!(hunter.contains("!DL1XYZ"), "{hunter}");
    assert!(
        hunter.contains("all-time new entity (local logbook)"),
        "{hunter}"
    );
    app.station_readiness = Some(StationReadiness {
        cqdx_dxcc_needs: NeedListHealth::LocalLogbook {
            error: "offline".into(),
            retrying: true,
        },
        ..Default::default()
    });
    assert!(readiness_items(&app)
        .iter()
        .any(|i| i.label.contains("NEEDS FROM LOCAL LOGBOOK")));
}
