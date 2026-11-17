//! PAN-387 invariant: no literal colours outside the theme module.
//!
//! Every renderer reads semantic tokens from `pancetta-tui/src/theme.rs`;
//! no other file under `pancetta-tui/src` (test modules and comments
//! included) may name a colour variant or make a colour another way.

use std::path::{Path, PathBuf};

const COLOUR_NAMES: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "gray",
    "dark_gray",
    "light_red",
    "light_green",
    "light_yellow",
    "light_blue",
    "light_magenta",
    "light_cyan",
    "white",
];

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn theme_rs() -> PathBuf {
    src_dir().join("theme.rs")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Every `path:line: text` outside `theme.rs` whose line matches `hit`.
fn offending_lines(hit: impl Fn(&str) -> bool) -> Vec<String> {
    let mut files = Vec::new();
    rust_files(&src_dir(), &mut files);
    assert!(files.len() > 10, "scan found too few files: {files:?}");
    let theme = theme_rs();
    let mut found = Vec::new();
    for file in files.iter().filter(|f| **f != theme) {
        let text = std::fs::read_to_string(file).unwrap();
        for (i, line) in text.lines().enumerate() {
            if hit(line) {
                let rel = file.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap();
                found.push(format!("{}:{}: {}", rel.display(), i + 1, line.trim()));
            }
        }
    }
    found
}

fn shortcut_patterns() -> Vec<String> {
    let mut patterns = vec![
        "Color as".to_string(),
        "parse::<Color>".to_string(),
        "parse::<ratatui::style::Color>".to_string(),
        "Stylize".to_string(),
    ];
    for name in COLOUR_NAMES {
        patterns.push(format!(".{name}()"));
        patterns.push(format!(".on_{name}()"));
    }
    patterns
}

#[test]
fn no_colour_literal_outside_theme_rs() {
    let found = offending_lines(|line| line.contains("Color::"));
    assert!(
        found.is_empty(),
        "`Color::` outside pancetta-tui/src/theme.rs (PAN-387): read a token from \
         `app.theme` instead, or add a token to theme.rs.\n{}",
        found.join("\n")
    );
}

#[test]
fn no_colour_shortcut_outside_theme_rs() {
    let patterns = shortcut_patterns();
    assert_eq!(patterns.len(), 4 + 32);
    let found = offending_lines(|line| patterns.iter().any(|p| line.contains(p.as_str())));
    assert!(
        found.is_empty(),
        "colour made without `Color::` outside theme.rs (PAN-387):\n{}",
        found.join("\n")
    );
}

#[test]
fn theme_rs_holds_the_palette() {
    let text = std::fs::read_to_string(theme_rs()).expect("pancetta-tui/src/theme.rs exists");
    assert!(
        text.contains("Color::"),
        "theme.rs names the palette colours"
    );
    for palette in [
        "pub const DARK",
        "pub const LIGHT",
        "pub const HIGH_CONTRAST",
    ] {
        assert!(text.contains(palette), "theme.rs defines {palette}");
    }
}

#[test]
fn the_scan_catches_a_planted_literal() {
    // Guard against a scan that silently matches nothing.
    let planted = "let s = Style::default().fg(Color::Cyan);";
    assert!(planted.contains("Color::"));
    let patterns = shortcut_patterns();
    assert!(patterns.iter().any(|p| "x.on_black()".contains(p.as_str())));
    assert!(patterns.iter().any(|p| "span.red()".contains(p.as_str())));
    assert!(!patterns.iter().any(|p| "theme.error".contains(p.as_str())));
}
