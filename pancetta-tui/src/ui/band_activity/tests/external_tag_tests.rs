// PAN-381. Band Activity leads a WSJT-X-relayed decode's Msg cell with
// `EXT `; pancetta's own decodes render exactly as before.
use super::*;
use crate::app::{ActivePanel, App, DecodedMessageView};
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;

fn view(message: &str, call: &str) -> DecodedMessageView {
    DecodedMessageView {
        timestamp: chrono::Utc::now(),
        frequency: 14.074,
        mode: "FT8".to_string(),
        snr: -5,
        delta_time: 0.2,
        delta_freq: 1302.0,
        call_sign: Some(call.to_string()),
        grid_square: None,
        message: message.to_string(),
        distance: None,
        bearing: None,
        slot_parity: None,
        is_directed_at_us: false,
        worked_before: false,
        needed: false,
        atno: false,
        band_needed: false,
        priority_score: None,
        priority_factors: None,
        is_own_tx: false,
        external: false,
    }
}

/// Render one decode into an 80-column Band Activity panel, not focused so
/// no selection highlight lands on the row.
async fn render_one(msg: DecodedMessageView) -> (App, Buffer) {
    let mut app = App::new(crate::config::Config::default(), None)
        .await
        .unwrap();
    app.active_panel = ActivePanel::Callers;
    app.decoded_messages.push_back(msg);
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 6)).unwrap();
    term.draw(|f| render_band_activity(f, f.area(), &app).unwrap())
        .unwrap();
    let buf = term.backend().buffer().clone();
    (app, buf)
}

fn row(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

/// The buffer cell where `token` starts on the first decode row.
fn cell_at<'a>(buf: &'a Buffer, token: &str) -> &'a ratatui::buffer::Cell {
    let text = row(buf, 2);
    let byte = text
        .find(token)
        .unwrap_or_else(|| panic!("missing {token:?}: {text}"));
    &buf[(text[..byte].chars().count() as u16, 2)]
}

#[tokio::test]
async fn external_row_leads_msg_with_ext() {
    let mut msg = view("CQ DX JA1ABC PM95", "JA1ABC");
    msg.external = true;
    let (app, buf) = render_one(msg).await;

    let text = row(&buf, 2);
    assert!(text.contains("EXT CQ DX JA1ABC PM95"), "row: {text}");
    // `EXT` takes the `external` token's foreground ...
    let ext = cell_at(&buf, "EXT ");
    assert_eq!(ext.fg, app.theme.external.fg.unwrap());
    // ... and the message keeps its normal (CQ) style.
    let cq = cell_at(&buf, "CQ DX");
    assert_eq!(cq.fg, app.theme.cq.fg.unwrap());
    assert!(cq.modifier.contains(Modifier::BOLD));
}

#[tokio::test]
async fn directed_external_row_uses_directed_style() {
    let mut msg = view("K5ARH W1ABC FN31", "W1ABC");
    msg.external = true;
    msg.is_directed_at_us = true;
    let (app, buf) = render_one(msg).await;

    let text = row(&buf, 2);
    assert!(text.contains("EXT K5ARH W1ABC FN31"), "row: {text}");
    let ext = cell_at(&buf, "EXT ");
    let directed = app.theme.directed.add_modifier(Modifier::BOLD);
    assert_eq!(ext.fg, directed.fg.unwrap());
    assert_ne!(ext.fg, app.theme.external.fg.unwrap());
    assert!(ext.modifier.contains(Modifier::BOLD));
    // The message itself is in the same directed style.
    let body = cell_at(&buf, "K5ARH W1ABC");
    assert_eq!(body.fg, ext.fg);
    assert_eq!(body.bg, ext.bg);
}

#[tokio::test]
async fn native_row_has_no_ext() {
    let (app, buf) = render_one(view("CQ DX JA1ABC PM95", "JA1ABC")).await;
    let text = row(&buf, 2);
    assert!(!text.contains("EXT"), "row: {text}");
    assert!(text.contains("CQ DX JA1ABC PM95"), "row: {text}");
    // The Msg cell starts with the message, in the CQ style, with no
    // `external` colour anywhere on the row.
    let cq = cell_at(&buf, "CQ DX");
    assert_eq!(cq.fg, app.theme.cq.fg.unwrap());
    assert!((0..buf.area.width).all(|x| buf[(x, 2)].fg != app.theme.external.fg.unwrap()));

    // An external flag on our own TX row never adds the tag.
    let mut own = view("K1ABC K5ARH -10", "K1ABC");
    own.is_own_tx = true;
    own.external = true;
    let (_, buf) = render_one(own).await;
    assert!(!row(&buf, 2).contains("EXT"), "row: {}", row(&buf, 2));
}
