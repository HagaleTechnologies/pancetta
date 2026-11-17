//! Theme snapshot goldens (PAN-387): renders every view, panel and overlay
//! in every theme from one deterministic fixture and compares text plus
//! per-cell style runs with `tests/fixtures/theme_snapshots/<theme>/<surface>.golden`.
//!
//! A change to what any surface looks like in any theme fails this test
//! until the goldens are re-blessed on purpose with
//! `PANCETTA_BLESS_THEME_SNAPSHOTS=1 cargo test -p pancetta-tui --lib theme_snapshots`.
//!
//! Text is masked (digits, slot-bar glyphs and the slot letter) so the
//! clock never changes a golden; styles are never masked. The property
//! tests below check every snapshot cell is explicitly coloured, overlay
//! interiors sit on the modal surface, overlays follow the theme, and
//! glyphs meet 3:1 (Light) and 4.5:1 (High contrast).

use super::draw_frame;
use crate::app::{App, ColorCapability};
use crate::config::Config;
use crate::theme::{self, Theme, ThemeName};
use crate::view::ActiveView;
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::widgets::Block;
use ratatui::Terminal;
use std::path::PathBuf;

const BLESS_VAR: &str = "PANCETTA_BLESS_THEME_SNAPSHOTS";
const BLESS_CMD: &str =
    "PANCETTA_BLESS_THEME_SNAPSHOTS=1 cargo test -p pancetta-tui --lib theme_snapshots";

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/theme_snapshots")
}

fn golden_path(theme: ThemeName, surface: Surface) -> PathBuf {
    golden_dir()
        .join(theme.as_str())
        .join(format!("{}.golden", surface.name()))
}

fn blessing() -> bool {
    std::env::var_os(BLESS_VAR).is_some()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    View,
    Panel,
    Overlay,
}

macro_rules! surfaces {
    ($($name:ident => $kind:ident),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[allow(non_camel_case_types)]
        pub(super) enum Surface {
            $($name),*
        }

        impl Surface {
            const ALL: &'static [Surface] = &[$(Surface::$name),*];

            fn name(self) -> &'static str {
                match self {
                    $(Surface::$name => stringify!($name)),*
                }
            }

            fn kind(self) -> Kind {
                match self {
                    $(Surface::$name => Kind::$kind),*
                }
            }
        }

        mod dark {
            $(
                #[tokio::test]
                async fn $name() {
                    super::check(super::ThemeName::Dark, super::Surface::$name).await;
                }
            )*
        }

        mod light {
            $(
                #[tokio::test]
                async fn $name() {
                    super::check(super::ThemeName::Light, super::Surface::$name).await;
                }
            )*
        }

        mod high_contrast {
            $(
                #[tokio::test]
                async fn $name() {
                    super::check(super::ThemeName::HighContrast, super::Surface::$name).await;
                }
            )*
        }
    };
}

surfaces! {
    view_operate => View,
    view_monitor => View,
    view_hunt => View,
    view_run => View,
    view_monitor_basic => View,
    too_small => View,
    panel_band_activity => Panel,
    panel_qso_status => Panel,
    panel_callers => Panel,
    panel_dx_hunter => Panel,
    panel_tx_placement => Panel,
    panel_tx_placement_zoom => Panel,
    panel_station_card => Panel,
    overlay_help => Overlay,
    overlay_quit_confirm => Overlay,
    overlay_stopped_banner => Overlay,
    overlay_device_modal => Overlay,
    overlay_rig_modal => Overlay,
    overlay_bookmark_name => Overlay,
    overlay_bookmark_list => Overlay,
    overlay_freq_modal => Overlay,
    overlay_watchlist_modal => Overlay,
    overlay_offset_modal => Overlay,
    overlay_out_of_band => Overlay,
    overlay_diagnostics => Overlay,
    overlay_health => Overlay,
    overlay_recent_qsos => Overlay,
    overlay_why => Overlay,
}

// ---------------------------------------------------------------- fixture

fn decode(
    message: &str,
    call: &str,
    snr: i32,
    delta_freq: f32,
    parity: pancetta_core::slot::SlotParity,
    directed: bool,
    own: bool,
) -> crate::app::DecodedMessageView {
    crate::app::DecodedMessageView {
        timestamp: chrono::Utc::now() - chrono::Duration::seconds(10),
        frequency: 14_074_000.0,
        mode: "FT8".to_string(),
        snr,
        delta_time: 0.1,
        delta_freq,
        call_sign: Some(call.to_string()),
        grid_square: None,
        message: message.to_string(),
        distance: None,
        bearing: None,
        slot_parity: Some(parity),
        is_directed_at_us: directed,
        worked_before: false,
        needed: false,
        atno: false,
        band_needed: false,
        priority_score: None,
        priority_factors: None,
        is_own_tx: own,
        external: false,
    }
}

fn banner(call: &str, qso_id: &str) -> crate::app::ActiveQsoBanner {
    let at = chrono::Utc::now() - chrono::Duration::seconds(10);
    crate::app::ActiveQsoBanner {
        their_callsign: call.to_string(),
        state: "wait rpt".to_string(),
        started_at: at,
        frequency_hz: 1500.0,
        tx_parity: Some(pancetta_core::slot::SlotParity::Even),
        last_tx_text: None,
        last_tx_at: None,
        last_rx_text: None,
        last_rx_at: None,
        snr_rx: None,
        report_sent: None,
        report_received: None,
        exchange_count: 0,
        qso_id: qso_id.to_string(),
        initiated_by: "Manual".to_string(),
        ladder_labels: vec![],
        ladder_ours: vec![],
        ladder_index: 0,
        now_line: String::new(),
        next_line: String::new(),
        call_count: 0,
        max_calls: 0,
        watchdog_deadline: None,
        dx_last_activity: None,
        hound: false,
        pending_freq_drift_hz: None,
        pending_freq_drift_since: None,
    }
}

/// W1AW, the engaged and focused QSO, with a keep-calling watchdog.
fn engaged_banner() -> crate::app::ActiveQsoBanner {
    let mut b = banner("W1AW", "qso-1");
    b.call_count = 2;
    b.max_calls = 5;
    b.watchdog_deadline = Some(chrono::Utc::now() + chrono::Duration::seconds(120));
    b
}

fn queued_call() -> crate::app::PendingCallBanner {
    crate::app::PendingCallBanner {
        callsign: "ZL2AB".to_string(),
        dx_parity: Some(pancetta_core::slot::SlotParity::Odd),
        waited_secs: 12,
    }
}

fn dx_station(call: &str, priority_score: u32) -> crate::app::DxStation {
    crate::app::DxStation {
        call_sign: call.to_string(),
        grid_square: Some("QH30".to_string()),
        frequency: 14.074,
        band: None,
        mode: "FT8".to_string(),
        last_seen: chrono::Utc::now() - chrono::Duration::seconds(10),
        snr: -10,
        distance: None,
        bearing: None,
        worked_before: false,
        needed: false,
        atno: false,
        band_needed: false,
        priority_score,
        local_rank: None,
        network_rank: None,
        source: crate::app::SpotSource::Local,
        entity_name: None,
        offline_entity: pancetta_core::dxcc::entity_for_callsign(call),
        rarity_tier: None,
        reporter_count: None,
        is_notable: false,
        notable_type: None,
        confidence: None,
        best_snr_network: None,
        last_seen_network: None,
        audio_offset_hz: Some(750),
        slot_parity: Some(pancetta_core::slot::SlotParity::Even),
        spot_rf_hz: None,
        watchlisted: false,
        pinned: false,
    }
}

fn placement() -> crate::app::PlacementView {
    let slice = |offset_hz, score, clear_first, clear_second| crate::app::PlacementSlice {
        offset_hz,
        score,
        clear_first,
        clear_second,
    };
    crate::app::PlacementView {
        slices: vec![
            slice(500.0, 90.0, true, true),
            slice(700.0, 80.0, true, false),
            slice(920.0, 70.0, false, true),
        ],
        openness: (0..96).map(|i| (i % 5) as u8).collect(),
        bin_hz: 25.0,
        range: (200.0, 2600.0),
        received_at: chrono::Utc::now() - chrono::Duration::seconds(10),
    }
}

/// One deterministic App per theme, populated with the plan's Phase 1
/// required state: a 20 m dial; decodes 10 s old in both parities (a CQ, one
/// calling us, our own TX, needed, ATNO, worked, band-needed, an engaged
/// station, a plain exchange; SNRs +4/-4/-12); one active QSO with a
/// watchdog deadline, a queued call and ✓/✗ history (the hound and drift
/// QSOs are added for `panel_qso_status`, see `render_with`); DX Hunter
/// stations that are needed, ATNO, notable, worked, engaged, Network and
/// Both; a transmit in progress; readiness Blocker/Warning/Notice and a
/// status line; a 20-row waterfall ramp; and overlay data with every
/// diagnostic level, outcome and upload state. Title-bar chips are set for
/// `view_operate` only (see `render_with`), so the other views keep room for
/// the dial and view chip.
///
/// The QSO-status focus sits on W1AW, which is also engaged, so every other
/// list renders the focused-and-engaged callsign (PAN-387 AC7).
///
/// Not every state reaches a golden: the ✓/✗ history needs a taller
/// single-QSO panel than any surface draws, and the readiness strip folds
/// the Notice into its `+N more` counter at 100 columns.
async fn fixture(name: ThemeName) -> App {
    use crate::app::{ActivePanel, SpotSource};
    use crate::upload_status::{QsoUploadStatus, TargetUploadStatus, UploadState, UploadTarget};
    use pancetta_core::slot::SlotParity::{Even, Odd};
    use pancetta_core::DiagnosticLevel;
    let mut config = Config::default();
    config.ui.theme = name;
    config.station.tx_self_parity = pancetta_config::station::TxSelfParity::Even;
    let mut app = App::new(config, None).await.expect("fixture App::new");
    app.color_capability = ColorCapability::TwoFiftySix;
    app.station_info.operating_frequency = 14.074;
    let own = app.station_info.call_sign.clone();

    // Insertion order is oldest first and Band Activity lists newest first,
    // so the focused, engaged CQ from W1AW is last: it then shows in every
    // view's Band Activity, not only in the taller panel.
    let mut decodes = vec![
        decode("CQ DX JA1ABC PM95", "JA1ABC", 4, 800.0, Odd, false, false),
        decode("CQ VK9XX QH30", "VK9XX", -4, 600.0, Even, false, false),
        decode("CQ 3Y0J JD10", "3Y0J", -12, 950.0, Odd, false, false),
        decode("CQ K1ABC FN42", "K1ABC", 4, 1100.0, Even, false, false),
        decode("CQ ZL2AB RF70", "ZL2AB", -4, 1700.0, Odd, false, false),
        decode(
            "K2XYZ DL1ABC -05",
            "DL1ABC",
            -12,
            1900.0,
            Even,
            false,
            false,
        ),
        decode(
            &format!("{own} G8KHF IO91"),
            "G8KHF",
            -4,
            1234.0,
            Odd,
            true,
            false,
        ),
        decode(
            &format!("{own} W1AW R-08"),
            "W1AW",
            -8,
            2100.0,
            Odd,
            true,
            false,
        ),
        decode(
            &format!("CQ {own} FN42"),
            &own,
            0,
            1500.0,
            Even,
            false,
            true,
        ),
        decode("CQ W1AW FN31", "W1AW", -12, 2100.0, Odd, false, false),
    ];
    decodes[1].needed = true;
    decodes[2].atno = true;
    decodes[2].needed = true;
    decodes[3].worked_before = true;
    decodes[4].band_needed = true;
    // PAN-381: the JA1ABC CQ came from a same-host WSJT-X (`EXT` tag).
    decodes[0].external = true;
    app.decoded_messages.extend(decodes);

    app.device_selection.input_devices = vec![
        ("Fixture input".into(), true),
        ("Second input".into(), false),
    ];
    app.device_selection.output_devices = vec![
        ("Fixture output".into(), true),
        ("Second output".into(), false),
    ];
    for (text, level) in [
        ("Fixture diagnostic", DiagnosticLevel::Info),
        ("Fixture warning", DiagnosticLevel::Warn),
        ("Fixture error", DiagnosticLevel::Error),
    ] {
        app.diagnostic_events
            .push_back(crate::app::DiagnosticEventRecord {
                ts: chrono::Utc::now() - chrono::Duration::seconds(10),
                target: "qso",
                level,
                text: text.into(),
                qso_id: None,
                callsign: None,
            });
    }
    for (callsign, outcome) in [
        ("W1AW", crate::app::QsoOutcome::Completed),
        (
            "G8KHF",
            crate::app::QsoOutcome::Failed(pancetta_qso::QsoFailureReason::Timeout),
        ),
    ] {
        let last_state = match outcome {
            crate::app::QsoOutcome::Completed => "Completed",
            crate::app::QsoOutcome::Failed(_) => "Failed",
        };
        app.recent_qsos.push_back(crate::app::RecentQsoOutcome {
            callsign: callsign.into(),
            qso_uuid: format!("u-{callsign}"),
            outcome,
            last_state: last_state.into(),
            freq_hz: 14_074_000,
            ts: chrono::Utc::now() - chrono::Duration::seconds(10),
            brief_timeline: vec![],
        });
    }
    // Both overlays show a window ending at their cursor: put it on the
    // last entry so every level and outcome is drawn.
    app.diagnostics_scroll = app.diagnostic_events.len() - 1;
    app.recent_qsos_scroll = app.recent_qsos.len() - 1;
    app.qso_uploads = vec![QsoUploadStatus {
        uuid: "u-W1AW".to_string(),
        callsign: "W1AW".to_string(),
        revision: 1,
        targets: vec![
            (
                UploadTarget::Qrz,
                TargetUploadStatus::new(UploadState::Accepted, ""),
            ),
            (
                UploadTarget::ClubLog,
                TargetUploadStatus::new(UploadState::Pending, "queued"),
            ),
            (
                UploadTarget::Eqsl,
                TargetUploadStatus::new(UploadState::Rejected, "refused"),
            ),
            (
                UploadTarget::Cqdx,
                TargetUploadStatus::new(UploadState::Failed, "queue full"),
            ),
            (
                UploadTarget::Lotw,
                TargetUploadStatus::new(UploadState::Unknown, ""),
            ),
        ],
    }];
    for (call_sign, success) in [("K1ABC", true), ("OH2XX", false)] {
        app.qso_history.push_back(crate::app::QsoHistoryItem {
            call_sign: call_sign.to_string(),
            success,
            completed_at: chrono::Utc::now() - chrono::Duration::seconds(10),
        });
    }

    app.apply_active_qsos(vec![engaged_banner()], vec![queued_call()]);

    let mut stations = vec![
        dx_station("W1AW", 900),
        dx_station("VK9XX", 800),
        dx_station("3Y0J", 950),
        dx_station("P5DX", 700),
        dx_station("K1ABC", 100),
        dx_station("ZL2AB", 600),
    ];
    stations[1].needed = true;
    stations[2].atno = true;
    stations[2].needed = true;
    stations[2].source = SpotSource::Network;
    stations[2].snr = 0;
    stations[3].is_notable = true;
    stations[3].notable_type = Some("DXpedition".to_string());
    stations[3].source = SpotSource::Both;
    stations[4].worked_before = true;
    // PAN-381: K1ABC heard only through WSJT-X (`EXT` Src).
    stations[4].source = SpotSource::External;
    stations[5].band_needed = true;
    for s in stations {
        app.dx_stations.insert(s.call_sign.clone(), s);
    }
    app.clamp_dx_hunter_selection();
    app.apply_placement(placement());

    app.tx_now_sending = Some(crate::app::TxQueueItem {
        text: format!("W1AW {own} -12"),
        freq_hz: 1500.0,
        qso_id: Some("qso-1".to_string()),
        deferred: false,
    });
    app.is_transmitting = true;

    app.station_readiness = Some(crate::readiness::StationReadiness {
        ptt_off_unconfirmed: true,
        autonomous_config_disabled: true,
        ..Default::default()
    });
    app.tx_output_default = true;
    app.set_local_status("Fixture status".to_string(), DiagnosticLevel::Info);
    app.pipeline_health = Some(crate::app::PipelineHealth {
        audio_alive: true,
        dsp_windows: 40,
        last_rms: 0.1,
        ft8lib_available: true,
        total_decodes: 10,
        last_decode_elapsed_ms: 900,
        last_decode_budget_exhausted: false,
        tx_attempts: 3,
        tx_defers: 1,
        decode_panic_count: 0,
        wdt_panic_count: 0,
        clock_skew_warning_s: None,
    });
    app.waterfall_data = (0..20).map(|row| vec![row as f32 / 19.0; 64]).collect();

    app.rig_selection.available_ports = vec!["/dev/ttyUSB0".to_string()];
    app.rig_selection.bookmarks = vec![pancetta_config::rig::RigBookmark {
        name: "home".to_string(),
        model: "1042".to_string(),
        port: "/dev/ttyUSB0".to_string(),
        baud_rate: 38400,
        ptt_method: pancetta_config::rig::PttMethod::Cat,
    }];

    // Focus on the engaged W1AW from the QSO-status panel, so the other
    // lists draw it with both the focus and the engaged highlight.
    app.active_panel = ActivePanel::QsoStatus;
    app.qso_cursor = 0;
    app
}

fn open_overlay(app: &mut App, surface: Surface) {
    match surface {
        Surface::overlay_help => app.help_visible = true,
        Surface::overlay_quit_confirm => app.quit_confirm_visible = true,
        Surface::overlay_stopped_banner => app.stopped_by_operator = true,
        Surface::overlay_device_modal => app.device_selection.visible = true,
        Surface::overlay_rig_modal => {
            app.rig_selection.snapshot_committed();
            app.rig_selection.visible = true;
        }
        Surface::overlay_bookmark_name => {
            app.rig_selection.visible = true;
            app.rig_selection.naming_bookmark = true;
            app.rig_selection.bookmark_name_input = "home".to_string();
        }
        Surface::overlay_bookmark_list => {
            app.rig_selection.visible = true;
            app.rig_selection.bookmark_overlay_visible = true;
        }
        Surface::overlay_freq_modal => {
            app.freq_modal.visible = true;
            app.freq_modal.rx_buffer = "14.074".to_string();
        }
        Surface::overlay_watchlist_modal => {
            app.watchlist_modal.visible = true;
            app.watchlist_modal.buffer = "K1ABC".to_string();
        }
        Surface::overlay_offset_modal => {
            app.offset_modal.visible = true;
            app.offset_modal.buffer = "1500".to_string();
        }
        Surface::overlay_out_of_band => {
            app.out_of_band_ack_visible = true;
            app.out_of_band_rf_hz = 14_400_000;
        }
        Surface::overlay_diagnostics => app.show_diagnostics = true,
        Surface::overlay_health => app.show_health = true,
        Surface::overlay_recent_qsos => app.show_recent_qsos = true,
        Surface::overlay_why => app.show_why = true,
        _ => unreachable!("{surface:?} is not an overlay"),
    }
}

// ---------------------------------------------------------------- render

/// A rendered region: origin in the full buffer plus its cells, row-major.
#[derive(Clone)]
struct Snapshot {
    origin: (u16, u16),
    width: u16,
    height: u16,
    cells: Vec<Cell>,
}

impl Snapshot {
    fn from_buffer(buf: &Buffer, area: Rect) -> Self {
        let mut cells = Vec::with_capacity(area.area() as usize);
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                cells.push(buf[(x, y)].clone());
            }
        }
        Snapshot {
            origin: (area.x, area.y),
            width: area.width,
            height: area.height,
            cells,
        }
    }

    fn cell(&self, col: u16, row: u16) -> &Cell {
        &self.cells[(row as usize) * (self.width as usize) + col as usize]
    }

    fn cell_mut(&mut self, col: u16, row: u16) -> &mut Cell {
        let w = self.width as usize;
        &mut self.cells[(row as usize) * w + col as usize]
    }
}

fn draw_full(app: &mut App, w: u16, h: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
    terminal.draw(|f| draw_frame(f, app)).expect("draw");
    terminal.backend().buffer().clone()
}

fn draw_panel(app: &App, surface: Surface) -> Buffer {
    let (w, h) = if surface == Surface::panel_station_card {
        (60, 6)
    } else {
        (80, 16)
    };
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
    terminal
        .draw(|f| {
            let area = f.area();
            f.render_widget(Block::default().style(app.theme.page), area);
            let r = match surface {
                Surface::panel_band_activity => {
                    crate::ui::band_activity::render_band_activity(f, area, app)
                }
                Surface::panel_qso_status => crate::ui::qso_status::render_qso_status(f, area, app),
                Surface::panel_callers => crate::ui::callers::render_callers(f, area, app),
                Surface::panel_dx_hunter => crate::ui::dx_hunter::render_dx_hunter(f, area, app),
                Surface::panel_tx_placement => {
                    crate::ui::tx_placement::render_tx_placement(f, area, app)
                }
                Surface::panel_tx_placement_zoom => {
                    crate::ui::tx_placement::render_placement_zoom(f, area, app)
                }
                Surface::panel_station_card => {
                    crate::ui::station_card::render_station_card(f, area, app)
                }
                _ => unreachable!("{surface:?} is not a panel"),
            };
            r.expect("panel render");
        })
        .expect("draw");
    terminal.backend().buffer().clone()
}

fn mask_symbol(sym: &str) -> String {
    sym.chars()
        .map(|c| match c {
            '0'..='9' => '#',
            '▮' | '▯' => '▪',
            other => other,
        })
        .collect()
}

/// Masked-symbol plus style identity, used to diff overlay frames so the
/// ticking clock never widens an overlay's bounding box.
fn cell_key(c: &Cell) -> (String, Color, Color, Modifier) {
    (mask_symbol(c.symbol()), c.fg, c.bg, c.modifier)
}

async fn render(theme: ThemeName, surface: Surface) -> Snapshot {
    let mut app = fixture(theme).await;
    render_with(&mut app, surface)
}

fn render_with(app: &mut App, surface: Surface) -> Snapshot {
    match surface.kind() {
        Kind::View => {
            let (w, h) = match surface {
                Surface::view_operate => {
                    app.active_view = ActiveView::Operate;
                    app.tx_policy = pancetta_core::TxPolicy::Full;
                    // Title-bar chips: here only, because together they
                    // push the dial and view chip out of a 100-column bar.
                    app.fox_mode = true;
                    app.autonomous_init_paused = true;
                    app.split_tx_hz = 14_076_000;
                    app.tx_offset_hold_hz = Some(1500);
                    (100, 30)
                }
                Surface::view_monitor => {
                    app.active_view = ActiveView::Monitor;
                    app.config.replay = true;
                    (100, 30)
                }
                Surface::view_hunt => {
                    app.active_view = ActiveView::Hunt;
                    app.tx_policy = pancetta_core::TxPolicy::RespondOnly;
                    (100, 30)
                }
                Surface::view_run => {
                    app.active_view = ActiveView::Run;
                    app.tx_policy = pancetta_core::TxPolicy::Disabled;
                    (100, 30)
                }
                Surface::view_monitor_basic => {
                    app.active_view = ActiveView::Monitor;
                    app.color_capability = ColorCapability::Basic;
                    (100, 30)
                }
                Surface::too_small => (60, 16),
                _ => unreachable!(),
            };
            let buf = draw_full(app, w, h);
            Snapshot::from_buffer(&buf, buf.area)
        }
        Kind::Panel => {
            // The panel under test is the active one, so its focus border
            // and selection highlight show.
            use crate::app::ActivePanel;
            app.active_panel = match surface {
                Surface::panel_band_activity => ActivePanel::BandActivity,
                Surface::panel_qso_status => ActivePanel::QsoStatus,
                Surface::panel_callers => ActivePanel::Callers,
                Surface::panel_dx_hunter => ActivePanel::DxHunter,
                Surface::panel_tx_placement | Surface::panel_tx_placement_zoom => {
                    ActivePanel::TxPlacement
                }
                _ => app.active_panel,
            };
            if surface == Surface::panel_qso_status {
                // Three active QSOs select the multi-QSO list, with the hound
                // and drift badges; the single-QSO detail is in the views.
                let mut hound = banner("DL1HND", "qso-2");
                hound.hound = true;
                let mut drift = banner("F5DRF", "qso-3");
                drift.pending_freq_drift_hz = Some(25.0);
                drift.pending_freq_drift_since =
                    Some(chrono::Utc::now() - chrono::Duration::seconds(10));
                app.apply_active_qsos(vec![engaged_banner(), hound, drift], vec![queued_call()]);
            }
            let buf = draw_panel(app, surface);
            Snapshot::from_buffer(&buf, buf.area)
        }
        Kind::Overlay => {
            app.active_view = ActiveView::Operate;
            if matches!(
                surface,
                Surface::overlay_bookmark_name | Surface::overlay_bookmark_list
            ) {
                // Bookmark dialogs sit on the rig modal: keep only their own box.
                app.rig_selection.visible = true;
            }
            let base = draw_full(app, 100, 30);
            open_overlay(app, surface);
            let over = draw_full(app, 100, 30);
            let (mut x0, mut y0, mut x1, mut y1) = (u16::MAX, u16::MAX, 0u16, 0u16);
            for y in 0..30 {
                for x in 0..100 {
                    if cell_key(&base[(x, y)]) != cell_key(&over[(x, y)]) {
                        x0 = x0.min(x);
                        y0 = y0.min(y);
                        x1 = x1.max(x);
                        y1 = y1.max(y);
                    }
                }
            }
            assert!(
                x0 != u16::MAX,
                "{} did not change the frame",
                surface.name()
            );
            Snapshot::from_buffer(&over, Rect::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1))
        }
    }
}

// ---------------------------------------------------------------- golden text

fn mods(m: Modifier) -> String {
    let names: Vec<&str> = m.iter_names().map(|(n, _)| n).collect();
    if names.is_empty() {
        "-".to_string()
    } else {
        names.join("|")
    }
}

fn golden_text(theme: ThemeName, surface: Surface, snap: &Snapshot) -> String {
    let mut out = String::new();
    out.push_str("# Generated by pancetta-tui theme_snapshots (PAN-387); do not edit by hand.\n");
    out.push_str(&format!("# Re-bless an intended change: {BLESS_CMD}\n"));
    out.push_str(&format!(
        "# surface={} theme={} origin={},{} size={}x{}\n",
        surface.name(),
        theme.as_str(),
        snap.origin.0,
        snap.origin.1,
        snap.width,
        snap.height
    ));
    for row in 0..snap.height {
        let text: String = (0..snap.width)
            .map(|col| mask_symbol(snap.cell(col, row).symbol()))
            .collect();
        let text = text.replace("[E ▪", "[P ▪").replace("[O ▪", "[P ▪");
        out.push_str(&format!("r{row:02} |{text}|\n"));

        let mut runs: Vec<String> = Vec::new();
        let mut start = 0u16;
        while start < snap.width {
            let first = snap.cell(start, row);
            let style = (first.fg, first.bg, first.modifier);
            let mut end = start + 1;
            while end < snap.width {
                let c = snap.cell(end, row);
                if (c.fg, c.bg, c.modifier) != style {
                    break;
                }
                end += 1;
            }
            runs.push(format!(
                "{}+{} {:?}/{:?}/{}",
                start,
                end - start,
                style.0,
                style.1,
                mods(style.2)
            ));
            start = end;
        }
        out.push_str(&format!("r{row:02} s {}\n", runs.join(" ")));
    }
    out
}

async fn check(theme: ThemeName, surface: Surface) {
    let snap = render(theme, surface).await;
    let actual = golden_text(theme, surface, &snap);
    let path = golden_path(theme, surface);
    if blessing() {
        std::fs::create_dir_all(path.parent().unwrap()).expect("create golden dir");
        std::fs::write(&path, &actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden {} ({e}); bless it with `{BLESS_CMD}`",
            path.display()
        )
    });
    if expected != actual {
        let (i, (want, got)) = expected
            .lines()
            .chain(std::iter::repeat(""))
            .zip(actual.lines().chain(std::iter::repeat("")))
            .enumerate()
            .find(|(_, (a, b))| a != b)
            .expect("texts differ");
        panic!(
            "{} {} differs from {} at line {}:\n  golden: {want}\n  actual: {got}\n\
             If the change is intended, re-bless with `{BLESS_CMD}`",
            theme.as_str(),
            surface.name(),
            path.display(),
            i + 1
        );
    }
}

// ---------------------------------------------------------------- properties

async fn all_snapshots() -> Vec<(ThemeName, Surface, Snapshot)> {
    let mut out = Vec::new();
    for &theme in ThemeName::ALL.iter() {
        for &surface in Surface::ALL {
            out.push((theme, surface, render(theme, surface).await));
        }
    }
    out
}

fn is_glyph(sym: &str) -> bool {
    sym.chars()
        .any(|c| !c.is_whitespace() && !('\u{2580}'..='\u{259F}').contains(&c))
}

fn contrast_failures(theme: ThemeName, min: f64, snaps: &[(ThemeName, Surface, Snapshot)]) {
    let mut failures = Vec::new();
    for (t, surface, snap) in snaps.iter().filter(|(t, _, _)| *t == theme) {
        for row in 0..snap.height {
            for col in 0..snap.width {
                let c = snap.cell(col, row);
                if !is_glyph(c.symbol()) {
                    continue;
                }
                let ratio = theme::contrast_ratio(c.fg, c.bg);
                if ratio.is_none_or(|r| r < min) {
                    failures.push(format!(
                        "{} {} ({},{}) {:?} fg={:?} bg={:?} ratio={:?}",
                        t.as_str(),
                        surface.name(),
                        snap.origin.0 + col,
                        snap.origin.1 + row,
                        c.symbol(),
                        c.fg,
                        c.bg,
                        ratio.map(|r| (r * 100.0).round() / 100.0)
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} glyph cells below {min}:1 contrast:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn snapshot_set_is_complete() {
    if blessing() {
        return;
    }
    let mut expected: Vec<PathBuf> = Vec::new();
    for &theme in ThemeName::ALL.iter() {
        for &surface in Surface::ALL {
            expected.push(golden_path(theme, surface));
        }
    }
    let missing: Vec<String> = expected
        .iter()
        .filter(|p| !p.is_file())
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        missing.is_empty(),
        "missing goldens (bless with `{BLESS_CMD}`):\n{}",
        missing.join("\n")
    );
    let mut stray = Vec::new();
    for entry in std::fs::read_dir(golden_dir()).expect("golden dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            for f in std::fs::read_dir(&path).expect("theme dir") {
                let f = f.expect("dir entry").path();
                if !expected.contains(&f) {
                    stray.push(f.display().to_string());
                }
            }
        } else {
            stray.push(path.display().to_string());
        }
    }
    assert!(
        stray.is_empty(),
        "stray golden files:\n{}",
        stray.join("\n")
    );
}

#[tokio::test]
async fn snapshots_are_deterministic() {
    for &surface in Surface::ALL {
        let a = golden_text(
            ThemeName::Dark,
            surface,
            &render(ThemeName::Dark, surface).await,
        );
        let b = golden_text(
            ThemeName::Dark,
            surface,
            &render(ThemeName::Dark, surface).await,
        );
        assert_eq!(a, b, "{} is not deterministic", surface.name());
    }
}

#[tokio::test]
async fn snapshot_comparison_catches_a_colour_only_change() {
    let surface = Surface::panel_band_activity;
    let mut snap = render(ThemeName::Dark, surface).await;
    let before = golden_text(ThemeName::Dark, surface, &snap);
    let cell = snap.cell_mut(2, 1);
    // Two theme colours (no literals outside theme.rs): swap to whichever
    // the cell does not already have.
    let (a, b) = (
        theme::fg_color(Theme::DARK.error),
        theme::fg_color(Theme::DARK.success),
    );
    assert_ne!(a, b);
    let fg = if cell.fg == a { b } else { a };
    cell.set_fg(fg);
    let after = golden_text(ThemeName::Dark, surface, &snap);
    assert_ne!(before, after, "a colour-only change must alter the golden");
    let text_lines = |s: &str| -> Vec<String> {
        s.lines()
            .filter(|l| l.contains(" |"))
            .map(str::to_string)
            .collect()
    };
    assert_eq!(text_lines(&before), text_lines(&after));
}

#[tokio::test]
async fn every_cell_is_explicitly_coloured() {
    let mut failures = Vec::new();
    for (theme, surface, snap) in all_snapshots().await {
        for row in 0..snap.height {
            for col in 0..snap.width {
                let c = snap.cell(col, row);
                if !(theme::is_explicit(c.fg) && theme::is_explicit(c.bg)) {
                    failures.push(format!(
                        "{} {} ({},{}) {:?} fg={:?} bg={:?}",
                        surface.name(),
                        theme.as_str(),
                        snap.origin.0 + col,
                        snap.origin.1 + row,
                        c.symbol(),
                        c.fg,
                        c.bg
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} cells are not explicitly coloured:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[tokio::test]
async fn overlay_interiors_use_the_modal_surface() {
    let mut failures = Vec::new();
    for &theme in ThemeName::ALL.iter() {
        let t = Theme::for_name(theme);
        for &surface in Surface::ALL.iter().filter(|s| s.kind() == Kind::Overlay) {
            let snap = render(theme, surface).await;
            let c = snap.cell(1, 1);
            // The operator-stop banner is an alarm band, not a dialog: its
            // surface is the `alarm` token by design.
            let modal_bg = if surface == Surface::overlay_stopped_banner {
                t.alarm.bg
            } else {
                t.modal.bg
            };
            if Some(c.bg) != modal_bg {
                failures.push(format!(
                    "{} {} inner cell ({},{}) bg={:?}, modal bg={:?}",
                    surface.name(),
                    theme.as_str(),
                    snap.origin.0 + 1,
                    snap.origin.1 + 1,
                    c.bg,
                    modal_bg
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn overlays_follow_the_theme() {
    let styles = |snap: &Snapshot| -> Vec<(Color, Color, Modifier)> {
        snap.cells
            .iter()
            .map(|c| (c.fg, c.bg, c.modifier))
            .collect()
    };
    let mut failures = Vec::new();
    for &surface in Surface::ALL.iter().filter(|s| s.kind() == Kind::Overlay) {
        let dark = styles(&render(ThemeName::Dark, surface).await);
        for theme in [ThemeName::Light, ThemeName::HighContrast] {
            if styles(&render(theme, surface).await) == dark {
                failures.push(format!(
                    "{} {} matches Dark",
                    surface.name(),
                    theme.as_str()
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn light_glyphs_meet_3_to_1() {
    contrast_failures(ThemeName::Light, 3.0, &all_snapshots().await);
}

#[tokio::test]
async fn high_contrast_glyphs_meet_4_5_to_1() {
    contrast_failures(ThemeName::HighContrast, 4.5, &all_snapshots().await);
}
