//! Every config snippet an operator can copy from README.md or docs/*.md must
//! load on its own through the real loader with zero warnings (PAN-90).
//! Validation is NOT run: several snippets carry deliberate placeholders.
//!
//! Two kinds of snippet are checked:
//! - fenced ```toml blocks (skip one only by putting
//!   `<!-- docs-snippets: skip -->` on the line before the fence, and only
//!   when the block is not pancetta config);
//! - inline code spans of the form `[section] key = value` or
//!   `[section].key = value` whose first path segment is a real top-level
//!   `Config` section (so Cargo.toml snippets are not mistaken for config).
use pancetta_config::Config;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const SKIP_MARKER: &str = "<!-- docs-snippets: skip -->";

struct Snippet {
    origin: String,
    text: String,
}

fn doc_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = vec![root.join("README.md")];
    let mut docs: Vec<PathBuf> = std::fs::read_dir(root.join("docs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "md"))
        .collect();
    docs.sort();
    files.extend(docs);
    files
}

fn display_name(path: &Path) -> String {
    let s = path.to_string_lossy();
    match s.find("/../") {
        Some(i) => s[i + 4..].to_string(),
        None => s.into_owned(),
    }
}

fn known_sections() -> BTreeSet<String> {
    serde_json::to_value(Config::default())
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Match `[a.b] key = value` / `[a.b].key = value`; returns (section, key, value).
fn parse_inline_assignment(span: &str) -> Option<(String, String, String)> {
    let rest = span.strip_prefix('[')?;
    let close = rest.find(']')?;
    let section = &rest[..close];
    if !section.split('.').all(is_ident) {
        return None;
    }
    let rest = rest[close + 1..].strip_prefix('.').unwrap_or(&rest[close + 1..]);
    let rest = rest.trim_start();
    let eq = rest.find('=')?;
    let key = rest[..eq].trim_end();
    if !is_ident(key) {
        return None;
    }
    let value = rest[eq + 1..].trim();
    if value.is_empty() {
        return None;
    }
    Some((section.to_string(), key.to_string(), value.to_string()))
}

fn extract(path: &Path, known: &BTreeSet<String>) -> (Vec<Snippet>, Vec<Snippet>) {
    let name = display_name(path);
    let content = std::fs::read_to_string(path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    let mut fenced = Vec::new();
    let mut inline = Vec::new();
    let mut i = 0;
    let mut prev_non_empty = "";
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.starts_with("```") {
            let is_toml = trimmed.starts_with("```toml");
            let skip = prev_non_empty == SKIP_MARKER;
            let start = i;
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim().starts_with("```") {
                body.push(lines[i]);
                i += 1;
            }
            if is_toml && !skip {
                fenced.push(Snippet {
                    origin: format!("{name}:{}", start + 1),
                    text: body.join("\n") + "\n",
                });
            }
            prev_non_empty = "```";
            i += 1;
            continue;
        }
        // Inline code spans: the odd segments between single backticks.
        for (n, span) in lines[i].split('`').enumerate() {
            if n % 2 == 0 {
                continue;
            }
            if let Some((section, key, value)) = parse_inline_assignment(span.trim()) {
                let top = section.split('.').next().unwrap();
                if known.contains(top) {
                    inline.push(Snippet {
                        origin: format!("{name}:{} `{}`", i + 1, span.trim()),
                        text: format!("[{section}]\n{key} = {value}\n"),
                    });
                }
            }
        }
        if !trimmed.is_empty() {
            prev_non_empty = trimmed;
        }
        i += 1;
    }
    (fenced, inline)
}

fn load_failure(snippet: &Snippet, dir: &Path) -> Option<String> {
    let path = dir.join("snippet.toml");
    std::fs::write(&path, &snippet.text).unwrap();
    match Config::load_from_file_with_warnings(&path) {
        Ok((_, warnings)) if warnings.is_empty() => None,
        Ok((_, warnings)) => Some(format!("{}: warnings {warnings:?}", snippet.origin)),
        Err(e) => Some(format!("{}: {e}", snippet.origin)),
    }
}

#[test]
fn every_documented_config_snippet_loads_cleanly() {
    let known = known_sections();
    let mut fenced = Vec::new();
    let mut inline = Vec::new();
    for file in doc_files() {
        let (f, i) = extract(&file, &known);
        fenced.extend(f);
        inline.extend(i);
    }

    // Non-vacuity: an extractor regression must not pass trivially.
    assert!(
        fenced.len() >= 18,
        "found only {} fenced toml blocks",
        fenced.len()
    );
    assert!(
        fenced.iter().any(|s| s.origin.starts_with("docs/GUIDE.md")
            && s.text.contains("destination = \"127.0.0.1:2237\"")),
        "GUIDE.md same-host UDP recipe not found"
    );
    assert!(
        fenced.iter().any(|s| s.origin.starts_with("docs/GUIDE.md")
            && s.text.contains("multicast_interface = \"192.168.1.50\"")),
        "GUIDE.md cross-machine UDP recipe not found"
    );
    assert!(
        inline.iter().any(|s| s.origin.starts_with("README.md")
            && s.text == "[autonomous]\nenabled = true\n"),
        "README.md `[autonomous] enabled = true` not found"
    );

    // Each snippet loads in its own directory; a cached parse from an
    // earlier snippet must not mask a later one.
    let mut failures = Vec::new();
    for snippet in fenced.iter().chain(&inline) {
        let dir = tempfile::tempdir().unwrap();
        if let Some(f) = load_failure(snippet, dir.path()) {
            failures.push(f);
        }
    }
    assert!(
        failures.is_empty(),
        "{} documented config snippet(s) do not load cleanly:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn inline_assignment_matcher() {
    assert_eq!(
        parse_inline_assignment("[autonomous] enabled = true"),
        Some(("autonomous".into(), "enabled".into(), "true".into()))
    );
    assert_eq!(
        parse_inline_assignment("[network.cqdx].enabled = false"),
        Some(("network.cqdx".into(), "enabled".into(), "false".into()))
    );
    assert_eq!(parse_inline_assignment("[autonomous]"), None);
    assert_eq!(parse_inline_assignment("[Foo] x = 1"), None);
    assert_eq!(parse_inline_assignment("enabled = true"), None);
}
