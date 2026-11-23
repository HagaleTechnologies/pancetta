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
fn later_secret_overlapping_an_earlier_match_redacts_fully() {
    // A secret that starts inside another secret's match and ends after it
    // used to be skipped, leaving its tail after the marker.
    assert_eq!(
        sanitize_error_body("abcdef", &["abc", "cdef"]),
        "<redacted>"
    );
    assert_eq!(
        sanitize_error_body("abcdef", &["cdef", "abc"]),
        "<redacted>"
    );
    assert_eq!(
        sanitize_error_body("user=w1aw73", &["w1aw", "aw73"]),
        "user=<redacted>"
    );
    assert_eq!(
        sanitize_error_body("abcdefgh", &["abc", "cde", "efgh"]),
        "<redacted>"
    );
}

#[test]
fn overlapping_occurrences_of_one_secret_redact_fully() {
    assert_eq!(sanitize_error_body("ababab", &["abab"]), "<redacted>");
}

#[test]
fn token_run_covering_a_secret_start_redacts_the_secret_tail() {
    assert_eq!(
        sanitize_error_body("echo pat_abcDE-fgh! end", &["cDE-fgh!"]),
        "echo pat_***REDACTED*** end"
    );
}

#[test]
fn touching_matches_and_marker_kind_are_unchanged() {
    assert_eq!(
        sanitize_error_body("abcdef", &["abc", "def"]),
        "<redacted><redacted>"
    );
    assert_eq!(
        sanitize_error_body("x pat_abc-x y", &["pat_abc-x"]),
        "x <redacted> y"
    );
    assert_eq!(
        sanitize_error_body("k=pat_ab1 ok", &["zz"]),
        "k=pat_***REDACTED*** ok"
    );
}

/// The characters of `body` that no secret occurrence (overlapping ones
/// included) and no `pat_` token run covers, found by trying every rule at
/// every char boundary.
fn uncovered(body: &str, secrets: &[&str]) -> String {
    let bytes = body.as_bytes();
    let mut covered = vec![false; bytes.len()];
    for (start, _) in body.char_indices() {
        for secret in secrets.iter().filter(|s| !s.is_empty()) {
            if body[start..].starts_with(secret) {
                covered[start..start + secret.len()].fill(true);
            }
        }
        if bytes[start..].starts_with(b"pat_") {
            let mut end = start + 4;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            if end > start + 4 {
                covered[start..end].fill(true);
            }
        }
    }
    body.char_indices()
        .filter(|&(i, _)| !covered[i])
        .map(|(_, ch)| ch)
        .collect()
}

#[test]
fn no_covered_byte_survives_redaction() {
    const PIECES: [&str; 7] = ["a", "b", "é", "_", "-", "1", "pat_"];
    let mut state: u64 = 0x5eed_1234_abcd_0001;
    let mut next = move |bound: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % bound
    };
    for case in 0..20_000 {
        let body: String = (0..next(10))
            .map(|_| PIECES[next(PIECES.len() as u64) as usize])
            .collect();
        let secrets: Vec<String> = (0..1 + next(3))
            .map(|_| {
                (0..1 + next(4))
                    .map(|_| PIECES[next(PIECES.len() as u64 - 1) as usize])
                    .collect()
            })
            .collect();
        let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
        let out = sanitize_error_body(&body, &refs);
        let stripped = out
            .replace("pat_***REDACTED***", "")
            .replace(super::MARKER, "");
        assert_eq!(
            stripped,
            uncovered(&body, &refs),
            "case {case}: body {body:?} secrets {refs:?} gave {out:?}"
        );
    }
}
