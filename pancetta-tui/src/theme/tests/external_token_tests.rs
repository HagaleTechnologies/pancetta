// PAN-381: the `external` token that tags decodes a same-host WSJT-X sent
// over UDP. Only theme.rs names a colour (PAN-387), so the values are
// checked against other tokens of the same palette; Dark's colour is
// pinned by `dark_tokens_match_legacy_dark_colours`.
use super::*;

#[test]
fn external_token_is_distinct_from_src_styles() {
    // Dark: a colour only, like every Dark token.
    assert!(Theme::DARK.external.fg.is_some());
    assert!(Theme::DARK.external.bg.is_none());
    assert!(Theme::DARK.external.add_modifier.is_empty());
    // Light: `info`'s blue, in italic.
    assert_eq!(
        Theme::LIGHT.external,
        Theme::LIGHT.info.add_modifier(Modifier::ITALIC)
    );
    // HighContrast: `title`'s light cyan, in italic.
    assert_eq!(
        Theme::HIGH_CONTRAST.external,
        Theme::HIGH_CONTRAST.title.add_modifier(Modifier::ITALIC)
    );

    for name in ThemeName::ALL {
        let t = Theme::for_name(name);
        // DX Hunter's Src column already paints `RX` with `success` and
        // `NET`/`RX+N` with `info`; `EXT`/`EX+N` must read differently.
        assert_ne!(t.external, t.success, "{name:?}: external == success");
        assert_ne!(t.external, t.info, "{name:?}: external == info");
        // Listed once in `tokens()`.
        assert_eq!(
            t.tokens().iter().filter(|(n, _)| *n == "external").count(),
            1,
            "{name:?}"
        );
    }
    assert_eq!(Theme::DARK.tokens().len(), 54);

    // Contrast on the page, xterm default palette (the plan's measured
    // 9.40:1 and 16.75:1).
    let on_page = |t: &Theme| {
        contrast_ratio(t.external.fg.unwrap(), bg_color(t.page)).expect("explicit colours")
    };
    let light = on_page(&Theme::LIGHT);
    assert!(light >= 9.39, "Light external is {light:.2}:1");
    let hc = on_page(&Theme::HIGH_CONTRAST);
    assert!(hc >= 16.7, "HighContrast external is {hc:.2}:1");
}
