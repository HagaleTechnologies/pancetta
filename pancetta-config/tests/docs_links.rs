//! Every repo-relative link in the curated docs must point at a file or
//! directory that exists (PAN-124). The README's Capability status table
//! cites its evidence this way, so a renamed test or module breaks CI here
//! instead of leaving the table pointing at nothing.
//!
//! Checked: `[text](target)` links outside fenced code blocks and inline code
//! spans in README.md, FEATURES.md, CONTRIBUTING.md, SECURITY.md, AGENTS.md and
//! docs/*.md. Skipped: `http(s):` and `mailto:` URLs and same-page `#anchor`
//! links. A `#anchor` suffix on a file link is dropped; only the file is
//! checked.
use std::path::{Path, PathBuf};

fn curated_docs(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = [
        "README.md",
        "FEATURES.md",
        "CONTRIBUTING.md",
        "SECURITY.md",
        "AGENTS.md",
    ]
    .iter()
    .map(|name| root.join(name))
    .collect();
    let mut docs: Vec<PathBuf> = std::fs::read_dir(root.join("docs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "md"))
        .collect();
    docs.sort();
    files.extend(docs);
    files
}

/// The target of every inline `[text](target)` link outside fenced code and
/// inline code spans, in document order.
fn link_targets(markdown: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut in_fence = false;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        // Drop inline code spans: the odd-numbered pieces between backticks.
        let prose: String = line
            .split('`')
            .enumerate()
            .filter(|(i, _)| i % 2 == 0)
            .map(|(_, piece)| piece)
            .collect::<Vec<_>>()
            .join(" ");
        let mut rest = prose.as_str();
        while let Some(start) = rest.find("](") {
            let after = &rest[start + 2..];
            let end = after
                .find(|c: char| c == ')' || c.is_whitespace())
                .unwrap_or(after.len());
            let target = after[..end].trim_start_matches('<').trim_end_matches('>');
            if !target.is_empty() {
                targets.push(target.to_string());
            }
            rest = &after[end..];
        }
    }
    targets
}

fn is_external(target: &str) -> bool {
    target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
        || target.starts_with('#')
}

#[test]
fn every_relative_doc_link_resolves() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut broken = Vec::new();
    let mut checked = 0;
    for doc in curated_docs(&root) {
        let text = std::fs::read_to_string(&doc).unwrap();
        let dir = doc.parent().unwrap();
        let name = doc.strip_prefix(&root).unwrap().display().to_string();
        for target in link_targets(&text) {
            if is_external(&target) {
                continue;
            }
            let file = target.split('#').next().unwrap_or("");
            checked += 1;
            if !dir.join(file).exists() {
                broken.push(format!("{name}: {target}"));
            }
        }
    }
    assert!(
        checked > 0,
        "no relative links found; is the extractor broken?"
    );
    assert!(
        broken.is_empty(),
        "{} doc link(s) point at a missing file:\n{}",
        broken.len(),
        broken.join("\n")
    );
}

#[test]
fn link_extractor_skips_code() {
    let md = "See [a](a.md) and `[b](b.md)`.\n```\n[c](c.md)\n```\n[d](d.md#x \"t\")\n";
    assert_eq!(link_targets(md), vec!["a.md", "d.md#x"]);
}
