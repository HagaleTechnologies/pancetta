use super::*;

fn slot_instant(milliseconds: i64) -> UtcTimestamp {
    static SLOT: std::sync::OnceLock<UtcTimestamp> = std::sync::OnceLock::new();
    *SLOT.get_or_init(|| {
        UtcTimestamp::from_second((UtcTimestamp::now().as_second() / 15) * 15).unwrap()
    }) + SignedDuration::from_millis(milliseconds)
}

fn buffer_text(buffer: &Buffer) -> String {
    (0..buffer.area.height)
        .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_string()))
        .collect()
}

#[tokio::test]
async fn overlay_goldens_match_across_ft8_clock_transitions() {
    // Independent control draws show that these instants do change the title
    // and the timing row when a panel is tall enough to show it.
    let mut app = fixture(ThemeName::Light, slot_instant(12_999)).await;
    let titles: Vec<String> = [14_999, 15_001]
        .map(|ms| {
            crate::render_time::with_utc(slot_instant(ms), || {
                buffer_text(&draw_full(&mut app, 100, 30))
            })
        })
        .into_iter()
        .collect();
    assert!(titles[0].contains("[E ") || titles[0].contains("[O "));
    assert!(titles[1].contains("[E ") || titles[1].contains("[O "));
    assert_ne!(
        titles[0].contains("[E "),
        titles[1].contains("[E "),
        "the title must cross slot parity"
    );
    let timing: Vec<String> = [12_999, 13_001]
        .map(|ms| {
            crate::render_time::with_utc(slot_instant(ms), || {
                let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
                terminal
                    .draw(|f| {
                        crate::ui::qso_status::render_qso_status(f, f.area(), &app).unwrap();
                    })
                    .unwrap();
                buffer_text(terminal.backend().buffer())
            })
        })
        .into_iter()
        .collect();
    assert!(timing[0].contains("RX Period"));
    assert!(timing[1].contains("TX Window"));

    for (before, after) in [(12_999, 13_001), (14_999, 15_001)] {
        for &theme in ThemeName::ALL.iter() {
            for &surface in Surface::ALL.iter().filter(|s| s.kind() == Kind::Overlay) {
                let mut offered = [slot_instant(before), slot_instant(after)].into_iter();
                let snap = render_with_clock(theme, surface, || offered.next().unwrap()).await;
                assert_eq!(
                    offered.next(),
                    Some(slot_instant(after)),
                    "{} sampled the clock twice",
                    surface.name()
                );
                let actual = golden_text(theme, surface, &snap);
                let expected = std::fs::read_to_string(golden_path(theme, surface)).unwrap();
                assert_eq!(
                    actual,
                    expected,
                    "{} {} at {before} ms",
                    theme.as_str(),
                    surface.name()
                );
            }
        }
    }
}

#[tokio::test]
async fn overlay_golden_detects_a_border_style_change() {
    let theme = ThemeName::Light;
    let surface = Surface::overlay_watchlist_modal;
    let at = slot_instant(5_000);
    let original = render_with_clock(theme, surface, || at).await;
    let expected = std::fs::read_to_string(golden_path(theme, surface)).unwrap();
    assert_eq!(golden_text(theme, surface, &original), expected);

    let mut app = fixture(theme, at).await;
    app.theme.modal_border = app.theme.modal_border.add_modifier(Modifier::UNDERLINED);
    let changed = crate::render_time::with_utc(at, || render_with(&mut app, surface));
    let actual = golden_text(theme, surface, &changed);
    assert_eq!(
        (original.origin, original.width, original.height),
        (changed.origin, changed.width, changed.height)
    );
    let text_rows = |text: &str| {
        text.lines()
            .filter(|line| line.contains(" |"))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(text_rows(&expected), text_rows(&actual));
    assert_ne!(
        expected, actual,
        "the golden comparison must reject a border style change"
    );
}
