//! PAN-441 invariant: every serialized instant goes through
//! `pancetta_core::time::UtcTimestamp`, which writes chrono's exact text.
//!
//! A raw jiff `Timestamp` serializes `.5Z` where the stored data says
//! `.500Z`, so no workspace source outside the core time module (and the
//! build script's identity stamp, which cannot depend on pancetta-core) may
//! name it.

use std::path::{Path, PathBuf};

/// Files and directories (relative to the workspace root) allowed to name
/// jiff's `Timestamp` type.
const ALLOWED: [&str; 3] = [
    "pancetta-core/src/time.rs",
    "pancetta-core/src/time/tests/",
    "pancetta/src/build_identity.rs",
];

/// jiff paths that write jiff's own text form instead of chrono's, built so
/// this file does not name them: the `Timestamp` type, the zoned type, and
/// jiff's serde helper modules.
const RAW_PATHS: [&str; 3] = [
    concat!("jiff", "::", "Timestamp"),
    concat!("jiff", "::", "Zoned"),
    concat!("jiff", "::", "fmt", "::", "serde"),
];
const RAW_PATH: &str = RAW_PATHS[0];

const SCANNED_DIRS: [&str; 4] = ["src", "tests", "examples", "benches"];

/// Workspace-root directories outside any member that crates compile in
/// through `#[path]` or `include!`.
const SHARED_DIRS: [&str; 1] = ["test-support"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("pancetta sits inside the workspace")
        .to_path_buf()
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

/// Every workspace member: a top-level directory holding a `Cargo.toml`.
fn member_dirs(root: &Path) -> Vec<PathBuf> {
    let mut members: Vec<PathBuf> = std::fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir() && p.join("Cargo.toml").is_file())
        .filter(|p| p.file_name().is_some_and(|n| n != "target"))
        .collect();
    members.sort();
    members
}

/// Every Rust source the workspace compiles: each member's `build.rs` and
/// `src`/`tests`/`examples`/`benches` trees, plus the shared directories.
fn workspace_sources(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for member in member_dirs(root) {
        let build_rs = member.join("build.rs");
        if build_rs.is_file() {
            files.push(build_rs);
        }
        for dir in SCANNED_DIRS {
            let dir = member.join(dir);
            if dir.is_dir() {
                rust_files(&dir, &mut files);
            }
        }
    }
    for dir in SHARED_DIRS {
        let dir = root.join(dir);
        if dir.is_dir() {
            rust_files(&dir, &mut files);
        }
    }
    files
}

/// The `use` or `extern crate` item a line starts, with any visibility
/// prefix removed, when that item is rooted at the jiff crate.
fn jiff_item_start(line: &str) -> Option<&str> {
    let mut code = line.trim_start();
    if let Some(rest) = code.strip_prefix("pub") {
        code = rest.trim_start();
        if code.starts_with('(') {
            code = code[code.find(')')? + 1..].trim_start();
        }
    }
    let rest = code
        .strip_prefix("use ")
        .or_else(|| code.strip_prefix("extern crate "))?
        .trim_start();
    let rest = rest.strip_prefix("::").unwrap_or(rest);
    let after = rest.strip_prefix("jiff")?;
    after
        .chars()
        .next()
        .filter(|c| !(c.is_alphanumeric() || *c == '_'))
        .map(|_| rest)
}

/// Every `(line number, text)` in `text` that names a raw jiff instant: one
/// of [`RAW_PATHS`] on the line, or a jiff `use` item (however rustfmt wraps
/// it) that brings in `Timestamp` or `Zoned`, reaches `fmt::serde`, or
/// renames jiff or one of its modules with `as`.
fn raw_timestamp_hits(text: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut hits = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if RAW_PATHS.iter().any(|path| line.contains(path)) {
            hits.push((i + 1, line.trim().to_string()));
            i += 1;
            continue;
        }
        let Some(start) = jiff_item_start(line) else {
            i += 1;
            continue;
        };
        let mut item = start.to_string();
        let first = i;
        while !item.contains(';') && i + 1 < lines.len() {
            i += 1;
            item.push(' ');
            item.push_str(lines[i].trim());
        }
        let item = item.split(';').next().unwrap_or_default();
        let words: Vec<&str> = item
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|w| !w.is_empty())
            .collect();
        let serde_helpers = words.windows(2).any(|w| w == ["fmt", "serde"]);
        // `as` after a module (`jiff`, `self`, `fmt`) hides later paths from
        // the line-level match; `as` after a type (`Date as CivilDate`) does not.
        let module_alias = words
            .windows(2)
            .any(|w| w[1] == "as" && w[0].chars().next().is_some_and(|c| c.is_lowercase()));
        if words.iter().any(|w| matches!(*w, "Timestamp" | "Zoned"))
            || serde_helpers
            || module_alias
        {
            hits.push((first + 1, item.trim().to_string()));
        }
        i += 1;
    }
    hits
}

fn is_allowed(rel: &str) -> bool {
    ALLOWED.iter().any(|allowed| {
        if allowed.ends_with('/') {
            rel.starts_with(allowed)
        } else {
            rel == *allowed
        }
    })
}

#[test]
fn no_raw_jiff_timestamp_outside_core_time() {
    let root = workspace_root();
    let files = workspace_sources(&root);
    assert!(
        files.len() > 100,
        "scan found too few files ({}), so it is not looking at the workspace",
        files.len()
    );

    let mut found = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if is_allowed(&rel) {
            continue;
        }
        let text = std::fs::read_to_string(file).unwrap();
        for (line, hit) in raw_timestamp_hits(&text) {
            found.push(format!("{rel}:{line}: {hit}"));
        }
    }
    assert!(
        found.is_empty(),
        "raw jiff instant outside pancetta-core's time module; use \
         pancetta_core::time::UtcTimestamp so stored text keeps chrono's form:\n{}",
        found.join("\n")
    );
}

#[test]
fn raw_timestamp_matcher_catches_paths_and_imports() {
    let hit = |text: &str| !raw_timestamp_hits(text).is_empty();
    assert!(hit(&format!("let t: {RAW_PATH} = now;")));
    assert!(hit(&format!("use {RAW_PATH};")));
    assert!(hit(&format!("let z: {} = now;", RAW_PATHS[1])));
    assert!(hit(&format!(
        "#[serde(with = \"{}::timestamp\")]",
        RAW_PATHS[2]
    )));
    assert!(hit("use jiff::{civil, Timestamp};"));
    assert!(hit("pub use jiff::{civil, Timestamp};"));
    assert!(hit(&format!("pub(crate) use ::{RAW_PATH} as Instant;")));
    // rustfmt's wrapped import: `Timestamp` sits on its own line.
    assert!(hit("use jiff::{\n    civil::Date,\n    Timestamp,\n};"));
    assert_eq!(
        raw_timestamp_hits("fn f() {}\nuse jiff::{\n    civil,\n    Timestamp,\n};\n")[0].0,
        2
    );
    // Aliases hide the path from a line-level match.
    assert!(hit("use jiff as j;"));
    assert!(hit("use jiff::{self as tz};"));
    assert!(hit("extern crate jiff as j;"));
    assert!(hit(&format!("use {}::timestamp;", RAW_PATHS[2])));

    assert!(!hit("use jiff::civil::Date;"));
    assert!(!hit("use jiff::{civil, SignedDuration};"));
    assert!(!hit("use jiff::civil::Date as CivilDate;"));
    assert!(!hit("use jiffy::Timestamp;"));
    assert!(!hit("let t = UtcTimestamp::now();"));
    assert!(!hit("use pancetta_core::time::UtcTimestamp;"));

    assert!(is_allowed("pancetta-core/src/time.rs"));
    assert!(is_allowed(
        "pancetta-core/src/time/tests/chrono_text_compat_tests.rs"
    ));
    assert!(!is_allowed("pancetta-qso/src/adif.rs"));
}
