//! PAN-2 verification tests — the render-level assertions the implementation
//! plan (`thoughts/shared/plans/2026-07-31-pan-2.md`, Phases 2-4) specified
//! but the implement phase did not land.
//!
//! Written by phase-verify, which is READ-ONLY over application code: these
//! tests only observe the shipped behavior. Where a test documents a defect
//! rather than a guarantee, it says so explicitly and asserts the CURRENT
//! behavior so the finding is reproducible without a red suite.

use pancetta_tui::app::{
    ActivePanel, ActiveQsoBanner, App, PlacementSlice, PlacementView, TxQueueItem,
};
use pancetta_tui::config::Config;
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

fn banner_at(call: &str, hz: f64, qso_id: &str) -> ActiveQsoBanner {
    ActiveQsoBanner {
        their_callsign: call.into(),
        state: "wait rpt".into(),
        started_at: chrono::Utc::now(),
        frequency_hz: hz,
        tx_parity: None,
        last_tx_text: None,
        last_tx_at: None,
        last_rx_text: None,
        last_rx_at: None,
        snr_rx: None,
        report_sent: None,
        report_received: None,
        exchange_count: 0,
        qso_id: qso_id.into(),
        initiated_by: "Manual".into(),
        ladder_labels: Vec::new(),
        ladder_ours: Vec::new(),
        ladder_index: 0,
        now_line: "waiting".into(),
        next_line: "their signal report".into(),
        call_count: 0,
        max_calls: 0,
        watchdog_deadline: None,
        dx_last_activity: None,
        hound: false,
    }
}

fn placement_view(offsets: &[(f64, f64)]) -> PlacementView {
    PlacementView {
        slices: offsets
            .iter()
            .map(|&(offset_hz, score)| PlacementSlice {
                offset_hz,
                score,
                clear_first: true,
                clear_second: true,
            })
            .collect(),
        // 96 bins × 25 Hz spanning 200-2600 Hz — the allocator's shape.
        openness: vec![3u8; 96],
        bin_hz: 25.0,
        range: (200.0, 2600.0),
        received_at: chrono::Utc::now(),
    }
}

async fn new_app() -> App {
    App::new(Config::default(), None).await.unwrap()
}

fn render(app: &App, w: u16, h: u16) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| pancetta_tui::ui::draw(f, app).unwrap())
        .unwrap();
    term.backend().buffer().clone()
}

fn rows(buf: &Buffer) -> Vec<String> {
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect()
        })
        .collect()
}

fn row_containing(buf: &Buffer, needle: &str) -> Option<String> {
    rows(buf).into_iter().find(|r| r.contains(needle))
}

fn contains(buf: &Buffer, needle: &str) -> bool {
    rows(buf).iter().any(|r| r.contains(needle))
}

// ---------------------------------------------------------------- TX strip

/// Plan Phase 2: with NO active QSOs the strip is genuinely idle and keeps
/// its pre-existing placeholder. Guards the `live.is_empty()` branch, which
/// no shipped test exercises.
#[tokio::test]
async fn tx_strip_keeps_idle_placeholder_with_no_active_qsos() {
    let app = new_app().await;
    let buf = render(&app, 120, 40);
    assert!(
        contains(&buf, "NOW: (idle)"),
        "{:?}",
        row_containing(&buf, "QUEUED")
    );
    assert!(!contains(&buf, "NEXT TX"));
}

/// Plan Phase 2 / ticket Scenarios 1-2: every active QSO's assigned offset is
/// named while idle, ascending by frequency.
#[tokio::test]
async fn tx_strip_shows_every_assigned_offset_while_idle_in_ascending_order() {
    let mut app = new_app().await;
    app.apply_active_qsos(
        vec![
            banner_at("JA1ABC", 1720.0, "qso-a"),
            banner_at("K1ABC", 1480.0, "qso-b"),
        ],
        Vec::new(),
    );
    assert!(app.tx_now_sending.is_none(), "precondition: idle");

    let buf = render(&app, 140, 40);
    let row = row_containing(&buf, "NEXT TX").expect("NEXT TX strip row");
    assert!(row.contains("K1ABC @1480Hz"), "got {row}");
    assert!(row.contains("JA1ABC @1720Hz"), "got {row}");
    assert!(
        row.find("K1ABC").unwrap() < row.find("JA1ABC").unwrap(),
        "ascending offset order: {row}"
    );
    assert!(!row.contains("NOW: (idle)"), "got {row}");
}

/// Plan Phase 2: keying must still win — the red TX-NOW banner, never the
/// idle NEXT-TX list.
#[tokio::test]
async fn tx_strip_keyed_branch_is_unchanged() {
    let mut app = new_app().await;
    app.apply_active_qsos(vec![banner_at("JA1ABC", 1480.0, "qso-a")], Vec::new());
    app.tx_now_sending = Some(TxQueueItem {
        text: "JA1ABC K5ARH -12".into(),
        freq_hz: 1480.0,
        qso_id: Some("qso-a".into()),
        deferred: false,
    });
    let buf = render(&app, 120, 40);
    assert!(contains(&buf, "TX NOW"));
    assert!(!contains(&buf, "NEXT TX"));
}

// ------------------------------------------------------------ QSO banner

/// Plan Phase 2 / Decision 5: the banner announces dropped entries with
/// `+N more` and never half-prints a callsign or an Hz value.
#[tokio::test]
async fn active_qsos_banner_reports_overflow_instead_of_clipping_silently() {
    let mut app = new_app().await;
    app.apply_active_qsos(
        (0..8)
            .map(|i| {
                banner_at(
                    &format!("K{i}ABC"),
                    400.0 + 100.0 * i as f64,
                    &format!("q{i}"),
                )
            })
            .collect(),
        Vec::new(),
    );
    let buf = render(&app, 80, 40);
    let row = row_containing(&buf, "QSO: ").expect("banner row");
    assert!(row.contains("more"), "overflow must be announced: {row}");
    assert!(!row.trim_end().ends_with('H'), "clipped mid-token: {row}");
}

/// DEFECT PROBE (phase-verify finding): `render_active_qsos` accumulates its
/// width budget with `str::len()` (BYTES) while seeding it from
/// `Span::width()` (DISPLAY CELLS). Each entry's detail carries two `·`
/// (U+00B7 — 2 bytes, 1 cell) and each separator a `│` (U+2502 — 3 bytes,
/// 1 cell), so the accumulator over-charges 2 cells per entry plus 2 per
/// separator and drops entries that would have fit.
///
/// Constructed so the discrepancy alone decides the outcome. Three entries
/// measure `5 ("QSO: ") + 32 + 5 (sep) + 32 + 5 + 32 = 111` display cells and
/// fit in a 115-column banner. The renderer must budget display cells rather
/// than UTF-8 bytes so multibyte separators and arrows do not hide an entry.
#[tokio::test]
async fn active_qsos_banner_budget_uses_display_width_for_multibyte_glyphs() {
    const W: u16 = 115;
    let mut app = new_app().await;
    app.apply_active_qsos(
        vec![
            banner_at("K1ABC", 1480.0, "qso-a"),
            banner_at("K2ABC", 1720.0, "qso-b"),
            banner_at("K3ABC", 1960.0, "qso-c"),
        ],
        Vec::new(),
    );
    let buf = render(&app, W, 40);
    let row = row_containing(&buf, "QSO: ").expect("banner row");
    // All three entries measure 111 display cells together — inside 115.
    let entry = "K1ABC (wait rpt · 0:00 · 1480Hz)".chars().count();
    let sep = "  │  ".chars().count();
    let all_three = "QSO: ".chars().count() + 3 * entry + 2 * sep;
    assert_eq!(all_three, 111, "fixture drift: recompute the widths");
    assert!(all_three <= W as usize, "fixture must fit the terminal");

    assert!(row.contains("K1ABC"), "first QSO missing: {row}");
    assert!(row.contains("K2ABC"), "second QSO missing: {row}");
    assert!(row.contains("K3ABC"), "third QSO missing: {row}");
    assert!(!row.contains("more"), "a fitting QSO was hidden: {row}");
}

/// Plan Phase 2 / Decision 6: zoom is exactly the state that shows ONLY
/// candidates, so the live half (the banner) must survive it.
#[tokio::test]
async fn active_qsos_banner_stays_visible_while_zoomed() {
    let mut app = new_app().await;
    app.apply_active_qsos(vec![banner_at("JA1ABC", 1480.0, "qso-a")], Vec::new());
    app.apply_placement(placement_view(&[(1480.0, 62.0)]));
    app.active_panel = ActivePanel::TxPlacement;
    app.zoomed = true;

    let buf = render(&app, 120, 40);
    assert!(contains(&buf, "TX Placement"), "zoomed panel still renders");
    let row = row_containing(&buf, "QSO: ").expect("banner must survive zoom");
    assert!(row.contains("JA1ABC"), "got {row}");
    assert!(
        row.contains("1480Hz"),
        "the live offset must survive zoom: {row}"
    );
}

// ------------------------------------------------- TX-placement instrument

/// Plan Phase 3: the candidate row names itself and the park line legends the
/// live-vs-candidate vocabulary.
#[tokio::test]
async fn tx_placement_labels_the_candidate_row_and_legends_the_distinction() {
    let mut app = new_app().await;
    app.apply_placement(placement_view(&[(1480.0, 62.0), (1720.0, 55.0)]));
    let buf = render(&app, 120, 40);
    assert!(contains(&buf, "BEST"), "candidate row must name itself");
    assert!(contains(&buf, "=live"), "legend must define the live glyph");
    assert!(
        contains(&buf, "=cand"),
        "legend must define the candidate glyph"
    );
}

/// Plan Phase 3: a candidate sharing an openness bin with a live assignment
/// is tagged with the callsign that owns it; a free one stays untagged.
#[tokio::test]
async fn tx_placement_best_row_tags_candidates_already_held_by_a_live_stream() {
    let mut app = new_app().await;
    app.apply_placement(placement_view(&[(1480.0, 62.0), (1720.0, 55.0)]));
    app.apply_active_qsos(vec![banner_at("JA1ABC", 1480.0, "qso-a")], Vec::new());

    let buf = render(&app, 140, 40);
    let row = row_containing(&buf, "BEST").expect("BEST row");
    assert!(
        row.contains("=JA1ABC"),
        "occupied candidate must name its owner: {row}"
    );
    let after = &row[row.find("1720").expect("second candidate")..];
    assert!(
        !after.contains('='),
        "free candidate must stay untagged: {after}"
    );
}

/// Plan Phase 3: no live QSOs ⇒ no owner tags at all.
#[tokio::test]
async fn tx_placement_best_row_is_untagged_without_live_assignments() {
    let mut app = new_app().await;
    app.apply_placement(placement_view(&[(1480.0, 62.0)]));
    let buf = render(&app, 120, 40);
    let row = row_containing(&buf, "BEST").expect("BEST row");
    assert!(!row.contains('='), "got {row}");
}

/// Plan Phase 4: the top-10 zoom table's `Live` column names the owner of an
/// occupied candidate and renders the placeholder for a free one.
#[tokio::test]
async fn placement_zoom_table_has_a_live_column_naming_the_owner() {
    let mut app = new_app().await;
    app.apply_placement(placement_view(&[(1480.0, 62.0), (1720.0, 55.0)]));
    app.apply_active_qsos(vec![banner_at("JA1ABC", 1480.0, "qso-a")], Vec::new());
    app.active_panel = ActivePanel::TxPlacement;
    app.zoomed = true;

    let buf = render(&app, 120, 40);
    assert!(contains(&buf, "Live"), "Live header column");
    let occupied = row_containing(&buf, "1480").expect("occupied candidate row");
    assert!(
        occupied.contains("JA1ABC"),
        "owner named on its row: {occupied}"
    );
    let free = row_containing(&buf, "1720").expect("free candidate row");
    assert!(
        free.contains('-'),
        "free rows render the placeholder: {free}"
    );
}

/// Plan Phase 4 (ticket's optional stretch): the stream-marker row carries the
/// numeric offset after the callsign, so the callsign appears on the banner,
/// the marker row, AND the idle NEXT-TX strip.
#[tokio::test]
async fn stream_marker_row_labels_each_stream_with_its_offset() {
    let mut app = new_app().await;
    app.apply_placement(placement_view(&[(1480.0, 62.0)]));
    app.apply_active_qsos(vec![banner_at("JA1ABC", 1480.0, "qso-a")], Vec::new());

    let buf = render(&app, 140, 40);
    let marker = rows(&buf)
        .into_iter()
        .find(|r| r.contains("JA1ABC") && r.contains('│') && !r.contains("QSO: "))
        .expect("marker row");
    assert!(
        marker.contains("1480"),
        "marker row must carry the number: {marker}"
    );
}

/// Plan Phase 4 edge case: a stream near the right edge of the strip must
/// clip its label, never panic or wrap.
#[tokio::test]
async fn stream_marker_label_is_clipped_not_wrapped_at_the_right_edge() {
    let mut app = new_app().await;
    app.apply_placement(placement_view(&[(1480.0, 62.0)]));
    app.apply_active_qsos(vec![banner_at("VERYLONGCALL", 2590.0, "qso-a")], Vec::new());
    let buf = render(&app, 120, 40);
    assert!(contains(&buf, "TX Placement"));
}
