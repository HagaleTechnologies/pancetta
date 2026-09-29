use pancetta_tui::{
    app::{App, QsoStatus},
    config::Config,
    view::ActiveView,
};
use ratatui::{backend::TestBackend, Terminal};
use std::path::PathBuf;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let out = PathBuf::from(std::env::args().nth(1).expect("capture directory"));
    std::fs::create_dir_all(&out)?;
    let mut cfg = Config::default();
    cfg.station.call_sign = "N0CALL".into();
    cfg.station.grid_square = "AA00aa".into();
    let mut app = App::new(cfg, None).await?;
    app.decode_effort = "AUTO".into();
    app.tx_policy = pancetta_core::TxPolicy::Disabled;
    app.status_message = "REVIEW FIXTURE: no audio, no rig, no transmission".into();
    for (w, h) in [(80, 24), (100, 30), (132, 40)] {
        for view in [
            ActiveView::Operate,
            ActiveView::Hunt,
            ActiveView::Run,
            ActiveView::Monitor,
        ] {
            app.active_view = view;
            let mut term = Terminal::new(TestBackend::new(w, h))?;
            term.draw(|frame| {
                pancetta_tui::ui::draw(frame, &app).unwrap();
            })?;
            let b = term.backend().buffer();
            let mut txt = String::new();
            for y in 0..h {
                for x in 0..w {
                    txt.push_str(b[(x, y)].symbol());
                }
                txt.push('\n');
            }
            let name = format!("{view:?}-{w}x{h}.txt");
            println!(
                "{name}: UTC visible in header={}",
                txt.lines().next().unwrap().contains("UTC")
            );
            std::fs::write(out.join(name), txt)?;
        }
    }
    let now = chrono::Utc::now();
    app.qso_statuses = vec![QsoStatus {
        active: true,
        call_sign: Some("W1AW".into()),
        frequency: Some(1500.0),
        mode: Some("FT8".into()),
        state: Some("WaitingReport".into()),
        snr_tx: Some(-10),
        snr_rx: Some(-12),
        started_at: Some(now),
        last_tx: Some(now),
        last_rx: Some(now),
        last_tx_text: Some("W1AW N0CALL AA00".into()),
        last_rx_text: Some("CQ W1AW FN31".into()),
        report_sent: Some(-12),
        report_received: None,
        exchange_count: 1,
        qso_id: Some("fixture-qso".into()),
        initiated_by: Some("Manual".into()),
        ladder_labels: vec!["Grid".into(), "Report".into(), "RR73".into()],
        ladder_ours: vec![true, false, true],
        ladder_index: 1,
        now_line: "Waiting for W1AW report".into(),
        next_line: "Send R-12 after their report".into(),
        call_count: 1,
        max_calls: 5,
        watchdog_deadline: None,
        dx_last_activity: Some("CQ".into()),
        hound: false,
    }];
    app.active_view = ActiveView::Operate;
    for (w, h) in [(80, 24), (132, 40)] {
        let mut term = Terminal::new(TestBackend::new(w, h))?;
        term.draw(|f| {
            pancetta_tui::ui::draw(f, &app).unwrap();
        })?;
        let b = term.backend().buffer();
        let mut txt = String::new();
        for y in 0..h {
            for x in 0..w {
                txt.push_str(b[(x, y)].symbol());
            }
            txt.push('\n');
        }
        std::fs::write(out.join(format!("ActiveQso-{w}x{h}.txt")), txt)?;
    }
    for (name, input) in [
        ("minimal station", "[station]\ncallsign = 'N0CALL'\ngrid_square = 'AA00aa'\n"),
        ("documented autonomous toggle", "[autonomous]\nenabled = true\n"),
        ("minimal UDP toggle", "[network.wsjtx_udp]\nenabled = true\n"),
        ("GUIDE same-host UDP recipe", "[network.wsjtx_udp]\nenabled = true\ndestination = '127.0.0.1:2237'\n"),
        ("GUIDE cross-machine UDP recipe", "[network.wsjtx_udp]\nenabled = true\ndestination = '224.0.0.73:2237'\nmulticast_interface = '192.168.1.50'\nmulticast_ttl = 3\n"),
    ] {
        match toml::from_str::<pancetta_config::Config>(input) {
            Ok(_)=>println!("CONFIG {name}: parsed"),
            Err(e)=>println!("CONFIG {name}: ERROR {e}"),
        }
    }
    let full = toml::to_string_pretty(&pancetta_config::Config::default())?;
    println!("DEFAULT CONFIG: {} lines", full.lines().count());
    println!(
        "HELP: {} keybinding entries",
        pancetta_tui::keymap::KEYBINDINGS.len()
    );
    Ok(())
}
