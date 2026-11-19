//! Redaction of server supplied diagnostics before they reach operator logs.

/// Redact known secret values and cqdx personal access tokens, then clip to 200 characters.
/// Empty values are ignored so ordinary text is preserved.
pub fn sanitize_error_body(body: &str, secrets: &[&str]) -> String {
    let mut redacted = body.to_owned();
    let mut ordered: Vec<&str> = secrets.iter().copied().filter(|s| !s.is_empty()).collect();
    ordered.sort_unstable_by_key(|s| std::cmp::Reverse(s.len()));
    for secret in ordered {
        redacted = redacted.replace(secret, "<redacted>");
    }
    let mut token_safe = String::with_capacity(redacted.len());
    let bytes = redacted.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if redacted[i..].starts_with("pat_") {
            let mut j = i + 4;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j > i + 4 {
                token_safe.push_str("pat_***REDACTED***");
                i = j;
                continue;
            }
        }
        let ch = redacted[i..].chars().next().expect("valid UTF-8");
        token_safe.push(ch);
        i += ch.len_utf8();
    }
    if token_safe.chars().count() > 200 {
        let mut clipped: String = token_safe.chars().take(200).collect();
        clipped.push('…');
        clipped
    } else {
        token_safe
    }
}

#[cfg(test)]
#[path = "error_redaction/tests/sanitizer_tests.rs"]
mod sanitizer_tests;
