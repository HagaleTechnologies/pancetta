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
