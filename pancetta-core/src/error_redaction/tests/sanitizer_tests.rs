use super::sanitize_error_body;

#[test]
fn redacts_before_unicode_clipping() {
    let text = format!("{} secret 🔑 intact", "é".repeat(185));
    let out = sanitize_error_body(&text, &["secret"]);
    assert!(!out.contains("secret"));
    assert!(out.contains("<redacted>"));
    assert!(out.ends_with('…'));
    assert!(std::str::from_utf8(out.as_bytes()).is_ok());
}

#[test]
fn ignores_empty_secrets() {
    assert_eq!(
        sanitize_error_body("service says pat_ and ok", &[""]),
        "service says pat_ and ok"
    );
}

#[test]
fn secret_overlapping_the_marker_cannot_rewrite_inserted_markers() {
    // "act" is a substring of "<redacted>": replacing secrets one after
    // another against the previous output used to rewrite the marker already
    // inserted for the other secret into a malformed chain.
    let out = sanitize_error_body("mission act", &["mission", "act"]);
    assert_eq!(out, "<redacted> <redacted>");
}

#[test]
fn edge_padding_does_not_spend_the_display_budget() {
    // Whitespace at the edges of a server response used to count against the
    // 200-character clip, so 200 leading spaces left the operator a bare `…`.
    let text = "login rejected: bad API key";
    for pad in [" ".repeat(200), "\r\n\t".repeat(100)] {
        for body in [
            format!("{pad}{text}"),
            format!("{text}{pad}"),
            format!("{pad}{text}{pad}"),
        ] {
            assert_eq!(sanitize_error_body(&body, &[]), text, "{body:?}");
        }
    }
}

#[test]
fn padded_long_body_clips_after_trimming() {
    let body = format!("{}{}", " ".repeat(250), "x".repeat(300));
    let out = sanitize_error_body(&body, &[]);
    assert!(out.starts_with('x'), "{out:?}");
    assert_eq!(out.chars().count(), 201);
    assert!(out.ends_with('…'), "{out:?}");
}

#[test]
fn trailing_padding_alone_adds_no_ellipsis() {
    let body = format!("{}{}", "x".repeat(190), " ".repeat(20));
    assert_eq!(sanitize_error_body(&body, &[]), "x".repeat(190));
}

#[test]
fn edge_whitespace_secret_still_redacts_in_padded_body() {
    // The trim runs on the redacted output, so a secret's own edge whitespace
    // still matches at either edge of a padded body.
    let pad = " ".repeat(200);
    assert_eq!(
        sanitize_error_body(&format!("{pad} pw then"), &[" pw "]),
        "<redacted>then"
    );
    assert_eq!(
        sanitize_error_body(&format!("denied: pw {pad}"), &[" pw "]),
        "denied:<redacted>"
    );
}
