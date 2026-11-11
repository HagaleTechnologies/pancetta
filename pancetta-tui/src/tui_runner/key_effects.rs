//! Key-effect golden (PAN-327): presses every key, in five modifier sets,
//! in a fixed list of scenarios against `TuiRunner::handle_key_event`, and
//! compares what each press did with `tests/fixtures/key_effects.golden`.
//!
//! The golden was blessed from the pre-PAN-327 handler (commit 7053b62), so
//! it proves the table-driven dispatch behaves like the old `match key.code`.
//! It then guards every later key change: a change to what a key does fails
//! this test until the golden is re-blessed on purpose with
//! `PANCETTA_BLESS_KEY_EFFECTS=1 cargo test -p pancetta-tui --lib key_effects_match_golden`.
//!
//! This file uses only APIs that existed before PAN-327 (`new_for_test`,
//! the private `handle_key_event`, `App` fields, `TuiCommand`), so it can be
//! dropped onto the base commit to re-check the golden there.

use super::{TuiCommand, TuiMessage, TuiRunner};
use crate::app::{ActivePanel, App};
use crate::config::Config;
use crossbeam_channel::unbounded;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::RwLock;

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/key_effects.golden"
);
const BLESS_VAR: &str = "PANCETTA_BLESS_KEY_EFFECTS";

pub(super) const MOD_SETS: [(&str, KeyModifiers); 5] = [
    ("none", KeyModifiers::NONE),
    ("shift", KeyModifiers::SHIFT),
    ("ctrl", KeyModifiers::CONTROL),
    ("alt", KeyModifiers::ALT),
    (
        "ctrl+shift",
        KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
    ),
];

pub(super) struct Scenario {
    pub(super) name: &'static str,
    rich: bool,
    setup: fn(&mut App),
}

pub(super) const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "empty/band",
        rich: false,
        setup: |a| a.active_panel = ActivePanel::BandActivity,
    },
    Scenario {
        name: "empty/qso",
        rich: false,
        setup: |a| a.active_panel = ActivePanel::QsoStatus,
    },
    Scenario {
        name: "empty/callers",
        rich: false,
        setup: |a| a.active_panel = ActivePanel::Callers,
    },
    Scenario {
        name: "empty/dx",
        rich: false,
        setup: |a| a.active_panel = ActivePanel::DxHunter,
    },
    Scenario {
        name: "empty/placement",
        rich: false,
        setup: |a| a.active_panel = ActivePanel::TxPlacement,
    },
    Scenario {
        name: "rich/band",
        rich: true,
        setup: |a| a.active_panel = ActivePanel::BandActivity,
    },
    Scenario {
        name: "rich/qso",
        rich: true,
        setup: |a| a.active_panel = ActivePanel::QsoStatus,
    },
    Scenario {
        name: "rich/callers",
        rich: true,
        setup: |a| a.active_panel = ActivePanel::Callers,
    },
    Scenario {
        name: "rich/dx",
        rich: true,
        setup: |a| a.active_panel = ActivePanel::DxHunter,
    },
    Scenario {
        name: "rich/placement",
        rich: true,
        setup: |a| a.active_panel = ActivePanel::TxPlacement,
    },
    Scenario {
        name: "rich/band/zoomed",
        rich: true,
        setup: |a| a.zoomed = true,
    },
    Scenario {
        name: "rich/band/stopped",
        rich: true,
        setup: |a| a.stopped_by_operator = true,
    },
    Scenario {
        name: "rich/band/zoomed+stopped",
        rich: true,
        setup: |a| {
            a.zoomed = true;
            a.stopped_by_operator = true;
        },
    },
    Scenario {
        name: "rich/band/tx-disabled",
        rich: true,
        setup: |a| a.tx_policy = pancetta_core::TxPolicy::Disabled,
    },
    Scenario {
        name: "rich/out-of-band-ack",
        rich: true,
        setup: |a| a.out_of_band_ack_visible = true,
    },
    Scenario {
        name: "rich/quit-confirm",
        rich: true,
        setup: |a| a.quit_confirm_visible = true,
    },
    Scenario {
        name: "rich/device-picker",
        rich: true,
        setup: |a| a.device_selection.visible = true,
    },
    Scenario {
        name: "rich/rig-form",
        rich: true,
        setup: |a| {
            a.rig_selection.snapshot_committed();
            a.rig_selection.visible = true;
        },
    },
    Scenario {
        name: "rich/rig-form/model",
        rich: true,
        setup: |a| {
            a.rig_selection.snapshot_committed();
            a.rig_selection.visible = true;
            a.rig_selection.active_field = crate::app::RigField::Model;
        },
    },
    Scenario {
        name: "rich/rig-bookmark-name",
        rich: true,
        setup: |a| {
            a.rig_selection.visible = true;
            a.rig_selection.naming_bookmark = true;
            a.rig_selection.bookmark_name_input = "home".to_string();
        },
    },
    Scenario {
        name: "empty/rig-bookmark-name",
        rich: false,
        setup: |a| {
            a.rig_selection.visible = true;
            a.rig_selection.naming_bookmark = true;
        },
    },
    Scenario {
        name: "rich/rig-bookmark-list",
        rich: true,
        setup: |a| {
            a.rig_selection.visible = true;
            a.rig_selection.bookmark_overlay_visible = true;
        },
    },
    Scenario {
        name: "empty/rig-bookmark-list",
        rich: false,
        setup: |a| {
            a.rig_selection.visible = true;
            a.rig_selection.bookmark_overlay_visible = true;
        },
    },
    Scenario {
        name: "rich/watchlist",
        rich: true,
        setup: |a| {
            a.watchlist_modal.visible = true;
            a.watchlist_modal.buffer = "K1ABC".to_string();
        },
    },
    Scenario {
        name: "empty/watchlist",
        rich: false,
        setup: |a| a.watchlist_modal.visible = true,
    },
    Scenario {
        name: "rich/freq-modal",
        rich: true,
        setup: |a| {
            a.freq_modal.visible = true;
            a.freq_modal.rx_buffer = "14.074".to_string();
        },
    },
    Scenario {
        name: "empty/freq-modal",
        rich: false,
        setup: |a| a.freq_modal.visible = true,
    },
    Scenario {
        name: "rich/offset-modal",
        rich: true,
        setup: |a| {
            a.offset_modal.visible = true;
            a.offset_modal.buffer = "1500".to_string();
        },
    },
    Scenario {
        name: "empty/offset-modal",
        rich: false,
        setup: |a| a.offset_modal.visible = true,
    },
    Scenario {
        name: "empty/compose",
        rich: false,
        setup: |a| a.enter_compose_mode(),
    },
    Scenario {
        name: "rich/diagnostics",
        rich: true,
        setup: |a| a.show_diagnostics = true,
    },
    Scenario {
        name: "rich/health",
        rich: true,
        setup: |a| a.show_health = true,
    },
    Scenario {
        name: "rich/recent-qsos",
        rich: true,
        setup: |a| a.show_recent_qsos = true,
    },
    Scenario {
        name: "rich/why",
        rich: true,
        setup: |a| a.show_why = true,
    },
    Scenario {
        name: "rich/help",
        rich: true,
        setup: |a| a.help_visible = true,
    },
    Scenario {
        name: "rich/compose",
        rich: true,
        setup: |a| {
            a.enter_compose_mode();
            for c in "CQ TEST".chars() {
                a.input_char(c);
            }
        },
    },
];

pub(super) fn key_universe() -> Vec<KeyCode> {
    let mut codes: Vec<KeyCode> = (' '..='~').map(KeyCode::Char).collect();
    codes.extend([
        KeyCode::Enter,
        KeyCode::Esc,
        KeyCode::Tab,
        KeyCode::BackTab,
        KeyCode::Backspace,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Delete,
        KeyCode::Insert,
        KeyCode::F(1),
        KeyCode::F(2),
        KeyCode::F(3),
        KeyCode::F(5),
    ]);
    codes
}

fn banner(call: &str, qso_id: &str) -> crate::app::ActiveQsoBanner {
    crate::app::ActiveQsoBanner {
        their_callsign: call.to_string(),
        state: "wait rpt".to_string(),
        started_at: chrono::Utc::now(),
        frequency_hz: 1500.0,
        tx_parity: None,
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

fn cq_dx_station() -> crate::app::DxStation {
    crate::app::DxStation {
        call_sign: "VK9XX".to_string(),
        grid_square: Some("QH30".to_string()),
        frequency: 14.074,
        mode: "FT8".to_string(),
        last_seen: chrono::Utc::now(),
        snr: -10,
        distance: None,
        bearing: None,
        worked_before: false,
        needed: true,
        atno: false,
        band_needed: false,
        priority_score: 800,
        local_rank: None,
        network_rank: None,
        source: crate::app::SpotSource::Local,
        entity_name: None,
        offline_entity: pancetta_core::dxcc::entity_for_callsign("VK9XX"),
        rarity_tier: None,
        reporter_count: None,
        is_notable: false,
        notable_type: None,
        confidence: None,
        best_snr_network: None,
        last_seen_network: None,
        audio_offset_hz: Some(750),
        slot_parity: Some(pancetta_core::slot::SlotParity::Even),
        watchlisted: false,
        pinned: false,
    }
}

fn placement_fixture() -> crate::app::PlacementView {
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
        openness: vec![3; 96],
        bin_hz: 25.0,
        range: (200.0, 2600.0),
        received_at: chrono::Utc::now(),
    }
}

/// One active QSO (W1AW), one directed-at-us caller (G8KHF), one DX Hunter
/// station (VK9XX), a placement snapshot, autonomous enabled, one serial
/// port, one rig bookmark and a 20 m dial.
fn rich_fixture(app: &mut App) {
    app.device_selection.input_devices = vec![
        ("Fixture input".into(), true),
        ("Second input".into(), false),
    ];
    app.device_selection.output_devices = vec![
        ("Fixture output".into(), true),
        ("Second output".into(), false),
    ];
    app.diagnostic_events
        .push_back(crate::app::DiagnosticEventRecord {
            ts: chrono::Utc::now(),
            target: "qso",
            level: pancetta_core::DiagnosticLevel::Info,
            text: "Fixture diagnostic".into(),
            qso_id: None,
            callsign: None,
        });
    app.diagnostic_events
        .push_back(crate::app::DiagnosticEventRecord {
            ts: chrono::Utc::now(),
            target: "qso",
            level: pancetta_core::DiagnosticLevel::Info,
            text: "Second diagnostic".into(),
            qso_id: None,
            callsign: None,
        });
    for callsign in ["W1AW", "G8KHF"] {
        app.recent_qsos.push_back(crate::app::RecentQsoOutcome {
            callsign: callsign.into(),
            qso_uuid: format!("u-{callsign}"),
            outcome: crate::app::QsoOutcome::Completed,
            last_state: "Completed".into(),
            freq_hz: 14_074_000,
            ts: chrono::Utc::now(),
            brief_timeline: vec![],
        });
    }
    app.station_info.operating_frequency = 14.074;
    app.apply_active_qsos(vec![banner("W1AW", "qso-1")], vec![]);
    app.decoded_messages
        .push_back(crate::app::DecodedMessageView {
            timestamp: chrono::Utc::now(),
            frequency: 14_074_000.0,
            mode: "FT8".to_string(),
            snr: -8,
            delta_time: 0.0,
            delta_freq: 1234.0,
            call_sign: Some("G8KHF".to_string()),
            grid_square: Some("IO91".to_string()),
            message: "K5ARH G8KHF IO91".to_string(),
            distance: None,
            bearing: None,
            slot_parity: Some(pancetta_core::slot::SlotParity::Even),
            is_directed_at_us: true,
            worked_before: false,
            needed: false,
            atno: false,
            band_needed: false,
            priority_score: None,
            priority_factors: None,
            is_own_tx: false,
        });
    app.dx_stations.insert("VK9XX".to_string(), cq_dx_station());
    app.clamp_dx_hunter_selection();
    app.apply_placement(placement_fixture());
    app.rig_selection.available_ports = vec!["/dev/ttyUSB0".to_string()];
    app.rig_selection.bookmarks = vec![pancetta_config::rig::RigBookmark {
        name: "home".to_string(),
        model: "1042".to_string(),
        port: "/dev/ttyUSB0".to_string(),
        baud_rate: 38400,
        ptt_method: pancetta_config::rig::PttMethod::Cat,
    }];
    app.autonomous_status = Some(crate::app::AutonomousStatus {
        enabled: true,
        state: "Hunting".to_string(),
        slot_parity: None,
        listen_counter: String::new(),
        active_qsos: 1,
        max_qsos: 2,
        idle_cycles: 0,
        band_name: "20m".to_string(),
        tx_offset_hz: 1500.0,
    });
}

/// The App fields a key handler may write. Timestamps are reduced to
/// `is_some()`, and sets are sorted, so nothing depends on time or hashing.
fn snapshot(app: &App) -> Vec<(&'static str, String)> {
    let mut ack: Vec<String> = app
        .acknowledged_readiness
        .iter()
        .map(|k| format!("{k:?}"))
        .collect();
    ack.sort();
    vec![
        ("status_message", format!("{:?}", app.status_message)),
        (
            "stopped_by_operator",
            format!("{:?}", app.stopped_by_operator),
        ),
        ("tx_policy", format!("{:?}", app.tx_policy)),
        ("tx_freq_mode", format!("{:?}", app.tx_freq_mode)),
        (
            "tx_frequency_offset",
            format!("{:?}", app.tx_frequency_offset),
        ),
        ("tx_offset_hold_hz", format!("{:?}", app.tx_offset_hold_hz)),
        ("split_tx_hz", format!("{:?}", app.split_tx_hz)),
        ("parked_since", format!("{:?}", app.parked_since.is_some())),
        (
            "out_of_band_ack_visible",
            format!("{:?}", app.out_of_band_ack_visible),
        ),
        (
            "out_of_band_warned",
            format!("{:?}", app.out_of_band_warned),
        ),
        ("out_of_band_rf_hz", format!("{:?}", app.out_of_band_rf_hz)),
        (
            "quit_confirm_visible",
            format!("{:?}", app.quit_confirm_visible),
        ),
        ("help_visible", format!("{:?}", app.help_visible)),
        ("help_scroll", format!("{:?}", app.help_scroll)),
        ("device_selection", format!("{:?}", app.device_selection)),
        ("rig_selection", format!("{:?}", app.rig_selection)),
        ("watchlist_modal", format!("{:?}", app.watchlist_modal)),
        ("freq_modal", format!("{:?}", app.freq_modal)),
        ("offset_modal", format!("{:?}", app.offset_modal)),
        ("compose_mode", format!("{:?}", app.compose_mode)),
        ("tx_input_buffer", format!("{:?}", app.tx_input_buffer)),
        ("tx_input_cursor", format!("{:?}", app.tx_input_cursor)),
        ("show_diagnostics", format!("{:?}", app.show_diagnostics)),
        (
            "diagnostics_scroll",
            format!("{:?}", app.diagnostics_scroll),
        ),
        ("show_health", format!("{:?}", app.show_health)),
        ("health_scroll", format!("{:?}", app.health_scroll)),
        ("show_recent_qsos", format!("{:?}", app.show_recent_qsos)),
        (
            "recent_qsos_scroll",
            format!("{:?}", app.recent_qsos_scroll),
        ),
        ("show_why", format!("{:?}", app.show_why)),
        ("why_scroll", format!("{:?}", app.why_scroll)),
        ("zoomed", format!("{:?}", app.zoomed)),
        ("active_panel", format!("{:?}", app.active_panel)),
        ("active_view", format!("{:?}", app.active_view)),
        ("qso_cursor", format!("{:?}", app.qso_cursor)),
        (
            "band_activity_scroll",
            format!("{:?}", app.band_activity_scroll),
        ),
        ("callers_scroll", format!("{:?}", app.callers_scroll)),
        ("dx_hunter_scroll", format!("{:?}", app.dx_hunter_scroll)),
        (
            "caller_reply_override",
            format!("{:?}", app.caller_reply_override),
        ),
        ("placement_cursor", format!("{:?}", app.placement_cursor)),
        (
            "current_band_index",
            format!("{:?}", app.current_band_index),
        ),
        (
            "operating_frequency",
            format!("{:?}", app.station_info.operating_frequency),
        ),
        ("autonomous_status", format!("{:?}", app.autonomous_status)),
        (
            "autonomous_init_paused",
            format!("{:?}", app.autonomous_init_paused),
        ),
        ("fox_mode", format!("{:?}", app.fox_mode)),
        ("is_monitoring", format!("{:?}", app.is_monitoring)),
        (
            "clear_armed_at",
            format!("{:?}", app.clear_armed_at.is_some()),
        ),
        ("acknowledged_readiness", format!("{ack:?}")),
    ]
}

/// A runner whose `App` starts from a state file that does not exist yet,
/// inside `dir`, so a file left by another test can never change it.
async fn golden_runner(
    dir: &std::path::Path,
    n: usize,
) -> (
    TuiRunner,
    crossbeam_channel::Receiver<TuiCommand>,
    Arc<RwLock<App>>,
) {
    App::set_test_tui_state_path(dir.join(format!("state_{n}.json")));
    let app = Arc::new(RwLock::new(
        App::new(Config::default(), None).await.unwrap(),
    ));
    let (_tui_msg_tx, tui_msg_rx) = unbounded::<TuiMessage>();
    let (cmd_tx, cmd_rx) = unbounded::<TuiCommand>();
    let runner = TuiRunner::new_for_test(
        Arc::clone(&app),
        Config::default(),
        tui_msg_rx,
        cmd_tx,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    (runner, cmd_rx, app)
}

impl Scenario {
    /// True for the main-view scenarios (no dialog or overlay open).
    pub(super) fn is_main_view(&self) -> bool {
        matches!(
            self.name.split('/').nth(1),
            Some("band" | "qso" | "callers" | "dx" | "placement")
        )
    }
}

/// A scratch directory for per-press state files, removed first if stale.
pub(super) fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pancetta_key_effects_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Press `code`+`mods` once on a fresh runner in `scenario`. Returns the
/// runner and the press's effect (empty when the key did nothing).
pub(super) async fn press(
    dir: &std::path::Path,
    n: usize,
    scenario: &Scenario,
    code: KeyCode,
    mods: KeyModifiers,
) -> (TuiRunner, String) {
    let (mut runner, cmd_rx, app) = golden_runner(dir, n).await;
    {
        let mut a = app.write().await;
        if scenario.rich {
            rich_fixture(&mut a);
        }
        (scenario.setup)(&mut a);
    }
    let before = snapshot(&*app.read().await);
    let result = runner.handle_key_event(KeyEvent::new(code, mods)).await;
    let after = snapshot(&*app.read().await);
    let mut parts = Vec::new();
    match result {
        Ok(true) => {}
        Ok(false) => parts.push("ret=false".to_string()),
        Err(e) => parts.push(format!("err={e:?}")),
    }
    let cmds: Vec<String> = cmd_rx.try_iter().map(|c| format!("{c:?}")).collect();
    if !cmds.is_empty() {
        parts.push(format!("cmds={cmds:?}"));
    }
    for ((field, b), (_, a)) in before.iter().zip(after.iter()) {
        if b != a {
            parts.push(format!("{field}: {b} -> {a}"));
        }
    }
    (runner, parts.join("; "))
}

/// Every effectful press, one line per (scenario, key, distinct effect),
/// listing the modifier sets that produced it.
async fn collect_effects() -> Vec<String> {
    let dir = scratch_dir("golden");
    let mut n = 0usize;
    let mut lines = Vec::new();
    for scenario in SCENARIOS {
        for code in key_universe() {
            // effect -> modifier-set names, in MOD_SETS order.
            let mut effects: BTreeMap<String, Vec<&str>> = BTreeMap::new();
            for (mod_name, mods) in MOD_SETS {
                n += 1;
                let (_runner, effect) = press(&dir, n, scenario, code, mods).await;
                if !effect.is_empty() {
                    effects.entry(effect).or_default().push(mod_name);
                }
            }
            for (effect, mods) in effects {
                lines.push(format!(
                    "{} | {:?} | {} | {}",
                    scenario.name,
                    code,
                    mods.join(","),
                    effect
                ));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    lines
}

fn render(lines: &[String]) -> String {
    let mut out = String::from(
        "# Key-effect golden (PAN-327). Generated by tui_runner::key_effects; do not edit by hand.\n\
         # One line per (scenario | key | modifier sets | effect); keys with no effect have no line.\n\
         # Re-bless only for an intended key-behaviour change:\n\
         #   PANCETTA_BLESS_KEY_EFFECTS=1 cargo test -p pancetta-tui --lib key_effects_match_golden\n",
    );
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[tokio::test]
async fn key_effects_match_golden() {
    let rendered = render(&collect_effects().await);
    if std::env::var(BLESS_VAR).is_ok() {
        std::fs::write(GOLDEN, &rendered).expect("write key_effects.golden");
        return;
    }
    let expected = std::fs::read_to_string(GOLDEN).unwrap_or_default();
    if expected == rendered {
        return;
    }
    let want: Vec<&str> = expected.lines().collect();
    let got: Vec<&str> = rendered.lines().collect();
    let first = want
        .iter()
        .zip(got.iter())
        .position(|(w, g)| w != g)
        .unwrap_or(want.len().min(got.len()));
    let window = |v: &[&str]| {
        let lo = first.saturating_sub(3);
        let hi = (first + 4).min(v.len());
        v[lo..hi]
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{:>6}: {l}", lo + i + 1))
            .collect::<Vec<_>>()
            .join("\n")
    };
    panic!(
        "key behaviour differs from tests/fixtures/key_effects.golden at line {}.\n\
         golden:\n{}\n\
         now:\n{}\n\
         If the change is intended, re-bless with:\n  \
         {BLESS_VAR}=1 cargo test -p pancetta-tui --lib key_effects_match_golden",
        first + 1,
        window(&want),
        window(&got)
    );
}
