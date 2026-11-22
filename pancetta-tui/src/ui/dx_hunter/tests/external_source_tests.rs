// PAN-381. DX Hunter's Src column labels stations heard through a same-host
// WSJT-X's UDP decodes `EXT` (and `EX+N` once a network spot merges), in
// the `external` token's foreground.
use super::*;
use crate::app::DecodedMessageView;
use pancetta_core::time::UtcTimestamp;
use ratatui::buffer::Buffer;

fn external_view(call: &str, snr: i32) -> DecodedMessageView {
    DecodedMessageView {
        timestamp: UtcTimestamp::now(),
        frequency: 14.074,
        mode: "FT8".to_string(),
        snr,
        delta_time: 0.1,
        delta_freq: 1302.0,
        call_sign: Some(call.to_string()),
        grid_square: None,
        message: format!("CQ {call}"),
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
        external: true,
    }
}

fn row_with(buf: &Buffer, call: &str) -> (u16, String) {
    (0..buf.area.height)
        .map(|y| {
            let text: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            (y, text)
        })
        .find(|(_, text)| text.contains(call))
        .unwrap_or_else(|| panic!("no row for {call}"))
}

#[tokio::test]
async fn external_rows_show_ext_labels_in_external_style() {
    let mut app = App::new(crate::config::Config::default(), None)
        .await
        .unwrap();
    // Not focused, so no selection highlight sits on a row.
    app.active_panel = ActivePanel::Callers;
    app.add_decoded_message(external_view("N4BP", -5))
        .await
        .unwrap();
    app.add_decoded_message(external_view("P5DX", -12))
        .await
        .unwrap();
    assert_eq!(app.dx_stations["N4BP"].source, SpotSource::External);
    app.dx_stations.get_mut("P5DX").unwrap().source = SpotSource::ExternalNetwork;

    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(106, 8)).unwrap();
    terminal
        .draw(|f| render_dx_hunter(f, f.area(), &app).unwrap())
        .unwrap();
    let buf = terminal.backend().buffer().clone();

    for (call, label) in [("N4BP", "EXT "), ("P5DX", "EX+N")] {
        let (y, text) = row_with(&buf, call);
        let byte = text
            .find(label)
            .unwrap_or_else(|| panic!("{call} row lacks {label:?}: {text}"));
        // Src is the row's first column, ahead of the callsign.
        assert!(byte < text.find(call).unwrap(), "{text}");
        let x = text[..byte].chars().count() as u16;
        assert_eq!(
            buf[(x, y)].fg,
            app.theme.external.fg.unwrap(),
            "{call}: {text}"
        );
        assert_ne!(buf[(x, y)].fg, app.theme.success.fg.unwrap());
        assert_ne!(buf[(x, y)].fg, app.theme.info.fg.unwrap());
    }
}
