//! PAN-77: guard the Serena project configuration that gives catalyst-dev's
//! codebase-analyzer / codebase-locator / codebase-pattern-finder subagents real
//! symbol search in this repo.
//!
//! Serena activation itself cannot be exercised from `cargo test` — it needs the
//! `mcp__serena__*` MCP server, which CI does not run. What CI *can* guard is the
//! shape of the committed config: that it exists, parses, names this repo, still
//! declares Rust, keeps every workspace member's source root visible to the
//! indexer, and ships the `codebase_map` memory that codebase-analyzer reads
//! first. Those are the parts a rename, a move, or a tidy-up would silently
//! break — which would put the repo straight back into the silent grep-fallback
//! state PAN-77 was filed for, with nothing to notice.
//!
//! This file is also load-bearing for CI itself, but only conditionally.
//! `.github/workflows/ci.yml`'s `changes` job filters on `**/*.rs`,
//! `**/Cargo.toml`, `**/Cargo.lock`, and `ci.yml` only — a `.serena/`-only diff
//! matches none of them and runs NO Rust job at all. This test makes a Serena
//! config change visible to the gate only when the same commit *also* touches
//! a `.rs`/`Cargo.toml`/`Cargo.lock` file; a PR that edits `.serena/` alone
//! still runs no Rust job, so this test never executes for it. The weekly
//! schedule lane (`ci.yml`'s `cron` trigger) would eventually catch a
//! `.serena/`-only regression, but only post-merge, not as a merge gate.
//!
//! Parsed with a purpose-built reader rather than a YAML crate on purpose: this
//! workspace has no YAML parser (`Cargo.toml` declares `serde`, `serde_json`,
//! and `toml` — nothing else), and every new dependency here passes through
//! `cargo deny check bans licenses sources advisories`. Pulling a YAML crate
//! into the supply-chain surface for one meta-test is not worth it; the file
//! being read is one we hand-author and whose shape this very test pins, so a
//! small reader for `key: value`, `key:` + `- item`, and `|` block scalars is
//! sufficient.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

fn serena_dir() -> PathBuf {
    repo_root().join(".serena")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

/// Recursively collects every `mod.rs` under `dir`, returned as slash-joined
/// paths relative to `repo_root()`. `crate_root` (lib.rs/main.rs) only probes
/// the crate's top level, so it stays blind to an `ignored_paths` entry like
/// `- "mod.rs"` that would silently de-index every legacy module root nested
/// deeper in the tree; this lets the shadow guard probe those too.
fn find_mod_rs_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(find_mod_rs_files(&path));
        } else if path.file_name().and_then(|f| f.to_str()) == Some("mod.rs") {
            let relative = path
                .strip_prefix(repo_root())
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push(relative);
        }
    }
    out
}

fn unquote(value: &str) -> String {
    value.trim().trim_matches(['\'', '"']).to_string()
}

/// Strips a trailing `#` comment from a line, honoring both `'...'` and
/// `"..."` quoting (YAML and TOML both use one or the other) so a `#` inside
/// either quote style survives. Everything from the first unquoted `#`
/// onward is dropped.
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    for (i, c) in line.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '#' => return &line[..i],
            None => {}
        }
    }
    line
}

#[derive(Default)]
struct FlatYaml {
    scalars: HashMap<String, String>,
    lists: HashMap<String, Vec<String>>,
}

/// Reads the flat `key: value` / `key:` + `- item` / `key: |` subset this file uses.
fn parse_flat_yaml(src: &str) -> FlatYaml {
    let mut out = FlatYaml::default();
    let mut current_key: Option<String> = None;
    let mut in_block_scalar = false;

    for raw_line in src.lines() {
        if in_block_scalar {
            // A block scalar's body is indented; the first non-indented,
            // non-empty line ends it.
            if raw_line.trim().is_empty() || raw_line.starts_with(char::is_whitespace) {
                continue;
            }
            in_block_scalar = false;
        }
        let line = strip_comment(raw_line).trim_end();
        if line.is_empty() {
            continue;
        }

        if let Some(item) = line.trim_start().strip_prefix("- ") {
            if let Some(key) = current_key.as_ref() {
                out.lists
                    .entry(key.clone())
                    .or_default()
                    .push(unquote(item));
            }
            continue;
        }

        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.starts_with(char::is_whitespace) || key.is_empty() {
            continue;
        }
        let key = key.to_string();
        let value = value.trim();
        out.lists.entry(key.clone()).or_default();
        if value == "|" || value == ">" {
            in_block_scalar = true;
            out.scalars.insert(key.clone(), String::new());
        } else {
            out.scalars.insert(key.clone(), unquote(value));
        }
        current_key = Some(key);
    }
    out
}

/// Extracts a `key = [ "a", "b", ... ]` TOML array's entries. Used for
/// `Cargo.toml`'s `members = [...]` block, which is TOML, not the YAML subset
/// `parse_flat_yaml` understands.
fn parse_toml_string_array(src: &str, key: &str) -> Vec<String> {
    let marker = format!("{key} = [");
    // Match only at the start of a line (ignoring leading whitespace): a
    // plain substring search for "members = [" also matches inside
    // "default-members = [", so an unqualified `.find` can silently return
    // the wrong array depending on which key appears first in the file.
    let mut search_from = 0;
    let after = loop {
        let rel = src[search_from..]
            .find(&marker)
            .unwrap_or_else(|| panic!("{key} array not found in Cargo.toml"));
        let abs = search_from + rel;
        let line_start = src[..abs].rfind('\n').map_or(0, |i| i + 1);
        if src[line_start..abs].trim().is_empty() {
            break &src[abs + marker.len()..];
        }
        search_from = abs + marker.len();
    };
    // Strip trailing `# comment`s line-by-line *before* looking for the
    // closing `]`, so a `]` that appears inside a comment (e.g.
    // `"pancetta-audio", # excluded from default-members [1]`) is not
    // mistaken for the array terminator and does not truncate the list.
    let cleaned: String = after
        .lines()
        .map(strip_comment)
        .collect::<Vec<_>>()
        .join(",");
    let end = cleaned.find(']').expect("unterminated members array");
    cleaned[..end]
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Minimal gitignore-style glob match, sufficient for `ignored_paths`: `**`
/// matches any number of path segments (including none), `*` matches any run
/// of non-`/` characters within one segment, and a pattern with no `/` at all
/// — or with only a *trailing* `/` marking "must be a directory" — matches at
/// any depth (gitignore semantics). A `/` anywhere else (leading or internal)
/// anchors the pattern to the repo root instead.
fn glob_shadows(pattern: &str, path: &str) -> bool {
    // `?` and `[...]` are not implemented below; failing loudly on them beats
    // silently answering "no shadow" for a pattern this matcher cannot
    // actually evaluate — a shadow guard that means "I don't know" must never
    // report that as "safe".
    assert!(
        !pattern.contains(['?', '[']),
        "glob_shadows does not support `?`/`[...]` wildcard syntax (pattern: {pattern:?}); \
         rewrite the ignored_paths entry to use only literal segments and `*`/`**`, or extend \
         segment_matches to support it"
    );
    // Handles any number of `*`s within a segment by peeling off the literal
    // prefix before the first `*`, then trying every position at which that
    // `*` could stop consuming characters and recursing on the remainder —
    // so e.g. "*research*" matches "pancetta-research" via the `*` at index 0
    // consuming "pancetta-" and the trailing `*` consuming nothing.
    fn segment_matches(pat: &str, seg: &str) -> bool {
        match pat.split_once('*') {
            None => pat == seg,
            Some((pre, rest)) => match seg.strip_prefix(pre) {
                None => false,
                Some(after_pre) => (0..=after_pre.len())
                    .filter(|&i| after_pre.is_char_boundary(i))
                    .any(|i| segment_matches(rest, &after_pre[i..])),
            },
        }
    }
    fn matches(pat: &[&str], path: &[&str]) -> bool {
        match pat.split_first() {
            None => true, // pattern fully consumed: it shadows this path (or a prefix of it)
            Some((&"**", rest)) => (0..=path.len()).any(|i| matches(rest, &path[i..])),
            Some((seg, rest)) => {
                path.first().is_some_and(|p| segment_matches(seg, p)) && matches(rest, &path[1..])
            }
        }
    }
    // A trailing-only slash (e.g. "src/") does not anchor under gitignore
    // semantics, so it must not be treated the same as a leading or internal
    // one (e.g. ".serena/cache", "/assets") when deciding anchoring.
    let core = pattern.trim_matches('/');
    let anchored = pattern.starts_with('/') || core.contains('/');
    let normalized = if anchored {
        core.to_string()
    } else {
        format!("**/{core}")
    };
    let pat_segs: Vec<&str> = normalized.split('/').collect();
    let path_segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    matches(&pat_segs, &path_segs)
}

#[test]
fn project_yml_is_committed_at_the_repo_root() {
    assert!(
        serena_dir().join("project.yml").exists(),
        ".serena/project.yml must exist at the repo root — it is the whole \
         activation contract for mcp__serena__activate_project (PAN-77)"
    );
}

#[test]
fn project_yml_identifies_this_repo_and_stays_navigation_only() {
    let cfg = parse_flat_yaml(&read(&serena_dir().join("project.yml")));
    assert_eq!(
        cfg.scalars.get("project_name").map(String::as_str),
        Some("pancetta")
    );
    assert_eq!(
        cfg.scalars.get("read_only").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        cfg.scalars
            .get("ignore_all_files_in_gitignore")
            .map(String::as_str),
        Some("true")
    );
}

#[test]
fn project_yml_declares_rust() {
    let cfg = parse_flat_yaml(&read(&serena_dir().join("project.yml")));
    // The legacy `languages:` key must not be present: reverting to it keeps
    // this file loading, but retriggers Serena's in-place migration rewrite
    // on next activation, which strips every hand-written rationale comment
    // in the file (wiki/pages/serena-project-config.md trap 4). Prefer
    // `language_servers:` and fail loudly if the old key creeps back in,
    // rather than silently accepting either.
    assert!(
        !cfg.lists.contains_key("languages"),
        "legacy `languages:` key is present in .serena/project.yml — this \
         retriggers Serena's migration rewrite (which strips the file's \
         rationale comments) on next activation; use `language_servers:` instead"
    );
    let languages = cfg
        .lists
        .get("language_servers")
        .expect("language_servers key missing from .serena/project.yml");
    assert!(
        languages.iter().any(|l| l == "rust"),
        "dropping `rust` from language_servers silently disables symbol search \
         across all 14 crates; got {languages:?}"
    );
}

#[test]
fn project_yml_ignores_the_noise_that_would_dominate_an_index_pass() {
    let cfg = parse_flat_yaml(&read(&serena_dir().join("project.yml")));
    let ignored = cfg
        .lists
        .get("ignored_paths")
        .expect("ignored_paths key missing");
    for required in [".serena/cache", "target", "thoughts"] {
        assert!(
            ignored.iter().any(|p| p == required),
            "ignored_paths must contain {required:?}; got {ignored:?}"
        );
    }
}

#[test]
fn no_ignored_path_shadows_a_workspace_member_source_root() {
    let cfg = parse_flat_yaml(&read(&serena_dir().join("project.yml")));
    let ignored = cfg
        .lists
        .get("ignored_paths")
        .expect("ignored_paths key missing");
    // Derived from Cargo.toml's `members` list rather than hardcoded, so a new
    // crate is covered automatically.
    let manifest = read(&repo_root().join("Cargo.toml"));
    let members = parse_toml_string_array(&manifest, "members");
    assert_eq!(
        members.len(),
        14,
        "workspace member count changed; got {members:?}"
    );

    for member in &members {
        let src = format!("{member}/src");
        assert!(
            repo_root().join(&src).is_dir(),
            "{src} should exist in the checkout"
        );
        // Every crate has exactly one of these as its crate root file. Probing
        // a real file path (not just the directory) catches a file-glob entry
        // like `- "*.rs"` that would de-index every Rust file in the workspace
        // while still returning false for glob_shadows(pattern, ".../src").
        let crate_root = ["lib.rs", "main.rs"]
            .into_iter()
            .map(|f| format!("{src}/{f}"))
            .find(|f| repo_root().join(f).is_file())
            .unwrap_or_else(|| panic!("{src} has neither lib.rs nor main.rs"));
        for pattern in ignored {
            assert!(
                !glob_shadows(pattern, &src),
                "ignored_paths entry {pattern:?} shadows workspace source root {src}"
            );
            assert!(
                !glob_shadows(pattern, &crate_root),
                "ignored_paths entry {pattern:?} shadows workspace source file {crate_root}"
            );
        }
        for mod_rs in find_mod_rs_files(&repo_root().join(&src)) {
            for pattern in ignored {
                assert!(
                    !glob_shadows(pattern, &mod_rs),
                    "ignored_paths entry {pattern:?} shadows workspace source file {mod_rs}"
                );
            }
        }
    }
}

#[test]
fn glob_shadows_treats_trailing_slash_as_any_depth_directory_match() {
    // "src/" has only a trailing slash, so gitignore semantics still match it
    // at any depth — the same as bare "src" would. A version of this matcher
    // that treated any '/' as anchoring would let `src/` in `ignored_paths`
    // silently miss every workspace crate's `<crate>/src`, defeating
    // `no_ignored_path_shadows_a_workspace_member_source_root` above.
    assert!(glob_shadows("src/", "pancetta-core/src"));
    assert!(glob_shadows("src", "pancetta-core/src"));
    // A leading or internal slash still anchors to the repo root.
    assert!(!glob_shadows("/assets", "pancetta-ft8/assets"));
    assert!(glob_shadows("/assets", "assets"));
}

#[test]
fn serena_ignore_wiring_keeps_cache_and_local_override_out_of_git() {
    let nested = read(&serena_dir().join(".gitignore"));
    // Line-wise, not a raw substring search: `nested.contains("/cache")` would
    // also pass for a commented-out `# /cache` or a negated `!/cache`, either
    // of which would let the generated cache directory back into git while
    // this guard stayed green.
    let active_lines: Vec<&str> = nested
        .lines()
        .map(strip_comment)
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('!'))
        .collect();
    for required in ["/cache", "/project.local.yml"] {
        assert!(
            active_lines.contains(&required),
            "{required} must be an active (non-commented, non-negated) entry in \
             .serena/.gitignore; got lines {active_lines:?}"
        );
    }
}

#[test]
fn codebase_map_memory_exists_under_the_name_the_analyzer_reads() {
    // codebase-analyzer opens with read_memory("codebase_map") — the filename is
    // the contract, not a convention.
    assert!(
        serena_dir()
            .join("memories")
            .join("codebase_map.md")
            .exists(),
        ".serena/memories/codebase_map.md must exist (PAN-77)"
    );
}

#[test]
fn codebase_map_names_every_workspace_crate() {
    let map = read(&serena_dir().join("memories").join("codebase_map.md"));
    let manifest = read(&repo_root().join("Cargo.toml"));
    let members = parse_toml_string_array(&manifest, "members");
    assert_eq!(
        members.len(),
        14,
        "workspace member count changed; got {members:?}"
    );
    for crate_name in &members {
        // Match the map's own bullet shape rather than a bare word: `pancetta`
        // is itself a substring of the title line ("# pancetta — codebase
        // map") and of other crates' prose, so a whole-word search alone
        // would report `pancetta` as "named" even with its own bullet deleted.
        let bullet = format!("- **{crate_name}** —");
        assert!(
            map.contains(&bullet),
            "codebase_map.md has no {bullet:?} bullet — a new crate was \
             added without updating the orientation memory"
        );
    }
}
