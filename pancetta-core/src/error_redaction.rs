//! Redaction of server supplied diagnostics before they reach operator logs.

/// Marker substituted for every known secret value.
const MARKER: &str = "<redacted>";

/// Redact known secret values and cqdx personal access tokens, then clip to 200 characters.
/// Empty values are ignored so ordinary text is preserved.
///
/// Scans the original text in one pass so a secret that overlaps the marker
/// cannot rewrite a marker already emitted for another secret.
pub fn sanitize_error_body(body: &str, secrets: &[&str]) -> String {
    let mut ordered: Vec<&str> = secrets.iter().copied().filter(|s| !s.is_empty()).collect();
    ordered.sort_unstable_by_key(|s| std::cmp::Reverse(s.len()));
    let mut redacted = String::with_capacity(body.len());
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let mut matched = false;
        for secret in &ordered {
            if body[i..].starts_with(secret) {
                redacted.push_str(MARKER);
                i += secret.len();
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }
        if bytes[i..].starts_with(b"pat_") {
            let mut j = i + 4;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j > i + 4 {
                redacted.push_str("pat_***REDACTED***");
                i = j;
                continue;
            }
        }
        let ch = body[i..].chars().next().expect("valid UTF-8");
        redacted.push(ch);
        i += ch.len_utf8();
    }
    if redacted.chars().count() > 200 {
        let mut clipped: String = redacted.chars().take(200).collect();
        clipped.push('…');
        clipped
    } else {
        redacted
    }
}

#[cfg(test)]
#[path = "error_redaction/tests/sanitizer_tests.rs"]
mod sanitizer_tests;
