//! Redaction of server supplied diagnostics before they reach operator logs.

/// Marker substituted for every known secret value.
const MARKER: &str = "<redacted>";

/// Marker substituted for every cqdx personal access token run (`pat_…`).
const TOKEN_MARKER: &str = "pat_***REDACTED***";

/// Redact known secret values and cqdx personal access tokens, trim edge whitespace from the
/// result, then clip it to 200 characters.
/// Empty values are ignored so ordinary text is preserved.
///
/// Finds every occurrence of every secret and every token run on the original
/// text, overlapping ones included, merges spans that overlap and emits one
/// marker per merged span. A match that starts inside another therefore cannot
/// leave its tail behind, and a secret that overlaps a marker cannot rewrite a
/// marker already emitted for another secret.
pub fn sanitize_error_body(body: &str, secrets: &[&str]) -> String {
    let mut spans = redaction_spans(body, secrets);
    // A secret sorts before a token run that shares its start, so the merged
    // span keeps the secret marker the leftmost-match scan used to emit.
    spans.sort_unstable_by_key(|&(start, _, is_token)| (start, is_token));
    let mut redacted = String::with_capacity(body.len());
    let mut copied = 0;
    let mut spans = spans.into_iter().peekable();
    while let Some((start, mut end, is_token)) = spans.next() {
        while let Some(&(next_start, next_end, _)) = spans.peek() {
            if next_start >= end {
                break;
            }
            end = end.max(next_end);
            spans.next();
        }
        redacted.push_str(&body[copied..start]);
        redacted.push_str(if is_token { TOKEN_MARKER } else { MARKER });
        copied = end;
    }
    redacted.push_str(&body[copied..]);
    // Trim after matching, so a secret's own edge whitespace still matches,
    // and before the clip, so padding does not spend the display budget.
    let redacted = redacted.trim();
    if redacted.chars().count() > 200 {
        let mut clipped: String = redacted.chars().take(200).collect();
        clipped.push('…');
        clipped
    } else {
        redacted.to_string()
    }
}

/// Every byte range of `body` to redact as `(start, end, is_token)`: each
/// occurrence of each non-empty secret, overlapping occurrences included, and
/// each `pat_` run with at least one `[A-Za-z0-9_]` byte after the prefix.
/// Every bound falls on a char boundary.
fn redaction_spans(body: &str, secrets: &[&str]) -> Vec<(usize, usize, bool)> {
    let mut spans = Vec::new();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        let mut from = 0;
        while let Some(offset) = body[from..].find(secret) {
            let start = from + offset;
            spans.push((start, start + secret.len(), false));
            // Step one character, not past the match, so an occurrence that
            // overlaps this one is found too.
            from = start + body[start..].chars().next().map_or(1, char::len_utf8);
        }
    }
    let bytes = body.as_bytes();
    let mut from = 0;
    while let Some(offset) = body[from..].find("pat_") {
        let start = from + offset;
        let mut end = start + 4;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        if end > start + 4 {
            spans.push((start, end, true));
            // `p`, `a`, `t` and `_` are run bytes, so any `pat_` that starts
            // inside this run ends at the same `end` and adds nothing.
            // Resuming at `end` keeps a body of `pat_pat_pat_…` linear.
            from = end;
        } else {
            from = start + 1;
        }
    }
    spans
}

#[cfg(test)]
#[path = "error_redaction/tests/sanitizer_tests.rs"]
mod sanitizer_tests;
