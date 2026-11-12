//! PAN-323: every `| Key | Type | Default | Notes |` table in docs/CONFIG.md
//! is drift-tested against the real loader and `Config::default()`.
//!
//! Three checks per row:
//! - **Key existence**: the documented key is loaded through
//!   `Config::load_from_file_with_warnings`. An "Unknown config key/section"
//!   warning fails with `docs/CONFIG.md:<line>` and the resolved path. Loader
//!   acceptance is the oracle (not serialized defaults) because
//!   `skip_serializing_if = "Option::is_none"` hides real keys such as
//!   `station.operator_name`.
//! - **Defaults**: literal Default cells are compared against
//!   `serde_json::to_value(Config::default())`. `absent` means no default
//!   (an optional key), never `""`; `required` means a record field with no
//!   default whose omission must fail deserialization; an em-dash on a
//!   custom-map row means the map defaults to no entries.
//! - **Notes**: backticked snake_case identifiers and explicit
//!   `[section].key` / `[section] key` references must resolve in their
//!   row's section — the original `(Fox mode uses \`fox_max_streams\`)`
//!   wording is the drift this catches.
//!
//! Tables are detected by their header row, never by fixed line numbers.
//! The upload-destination table (`| Key | Service | Notes |`) and the
//! gateway rule table (`| Key | Default | Rule |`) are excluded by header.

use pancetta_config::Config;
use std::path::{Path, PathBuf};

const EXCEPTIONS: &[(&str, &str)] = &[(
    "respond_only",
    "allowed value of autonomous.unattended_policy, not a config key",
)];

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    /// A plain `section.key` row.
    Plain,
    /// The `"<band>"` row of the `[rig.receive_gain_presets]` table.
    GainPresetMap,
    /// A field of a `[rig.receive_gain_presets."<band>"]` record.
    GainPresetField,
    /// The `custom_bands."<band>"` row of `[rig.frequency.band_plan]`.
    CustomBandsMap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DefaultForm {
    /// The cell holds a TOML literal (string/int/float/bool/array).
    Literal(String),
    /// The key exists but has no default (optional).
    Absent,
    /// A record field with no default; omitting it must fail.
    Required,
    /// An em-dash: the custom map defaults to no entries.
    NoEntries,
}

#[derive(Debug, Clone)]
struct Row {
    line: usize,
    section: String,
    keys: Vec<String>,
    type_cell: String,
    /// Raw (tick-preserving) default cells, one per key.
    defaults: Vec<String>,
    notes: String,
    kind: RowKind,
}

impl Row {
    fn path(&self, idx: usize) -> String {
        format!("{}.{}", self.section, self.keys[idx])
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The config section of a `## \`[a.b]\`` heading, if any. Unbracketed
/// subheadings return None so the current section is preserved.
fn section_from_heading(line: &str) -> Option<String> {
    let t = line.trim();
    if !t.starts_with('#') {
        return None;
    }
    let start = t.find('[')?;
    let rest = &t[start + 1..];
    let end = rest.find(']')?;
    let section = &rest[..end];
    if !section.split('.').all(is_ident) {
        return None;
    }
    Some(section.to_string())
}

fn strip_ticks(s: &str) -> &str {
    s.trim().trim_matches('`').trim()
}

fn classify_row(section: &str, key_cell: &str) -> RowKind {
    let key = strip_ticks(key_cell);
    if section == "rig.receive_gain_presets" {
        if key == "\"<band>\"" {
            RowKind::GainPresetMap
        } else {
            RowKind::GainPresetField
        }
    } else if section == "rig.frequency.band_plan" && key.starts_with("custom_bands.") {
        RowKind::CustomBandsMap
    } else {
        RowKind::Plain
    }
}

/// Parse every Key/Type/Default/Notes table. Fenced code is ignored.
/// A malformed row fails rather than being skipped.
fn parse_rows(content: &str) -> Result<Vec<Row>, String> {
    let mut rows: Vec<Row> = Vec::new();
    let mut section = String::new();
    let mut in_fence = false;
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            i += 1;
            continue;
        }
        if in_fence {
            i += 1;
            continue;
        }
        if let Some(s) = section_from_heading(trimmed) {
            section = s;
        }
        if trimmed == "| Key | Type | Default | Notes |" {
            if section.is_empty() {
                return Err(format!("line {}: table before any section heading", i + 1));
            }
            let sep = lines.get(i + 1).map(|l| l.trim()).unwrap_or("");
            if !sep.starts_with("|---") {
                return Err(format!(
                    "line {}: key table header not followed by a separator row",
                    i + 1
                ));
            }
            let mut j = i + 2;
            while j < lines.len() && lines[j].trim().starts_with('|') {
                let raw = lines[j].trim();
                let parts: Vec<&str> = raw.split('|').collect();
                if parts.len() != 6 {
                    return Err(format!(
                        "line {}: malformed table row (expected 4 cells): {raw}",
                        j + 1
                    ));
                }
                let cells: Vec<String> = parts[1..5].iter().map(|c| c.trim().to_string()).collect();
                let key_cell = cells[0].clone();
                let type_cell = cells[1].clone();
                let default_cell = cells[2].clone();
                let notes = cells[3].clone();
                if strip_ticks(&key_cell).is_empty() {
                    return Err(format!("line {}: table row with empty key cell", j + 1));
                }
                let keys: Vec<String> = key_cell
                    .split(" / ")
                    .map(|k| strip_ticks(k).to_string())
                    .collect();
                let defaults: Vec<String> = if keys.len() > 1 {
                    let parts: Vec<&str> = default_cell.split(" / ").collect();
                    if parts.len() != keys.len() {
                        return Err(format!(
                            "line {}: paired keys ({}) but {} default cells",
                            j + 1,
                            keys.len(),
                            parts.len()
                        ));
                    }
                    parts.iter().map(|p| p.trim().to_string()).collect()
                } else {
                    vec![default_cell.trim().to_string()]
                };
                let kind = classify_row(&section, &key_cell);
                rows.push(Row {
                    line: j + 1,
                    section: section.clone(),
                    keys,
                    type_cell,
                    defaults,
                    notes,
                    kind,
                });
                j += 1;
            }
            i = j;
            continue;
        }
        i += 1;
    }
    Ok(rows)
}

fn default_form(cell: &str) -> DefaultForm {
    match cell.trim() {
        "absent" => DefaultForm::Absent,
        "required" => DefaultForm::Required,
        "—" | "-" | "" => DefaultForm::NoEntries,
        _ => DefaultForm::Literal(literal_text(cell)),
    }
}

/// The TOML literal of a default cell: the first backticked span, else the
/// whole (tick-stripped) cell.
fn literal_text(cell: &str) -> String {
    let c = cell.trim();
    if let Some(start) = c.find('`') {
        if let Some(end) = c[start + 1..].find('`') {
            return c[start + 1..start + 1 + end].trim().to_string();
        }
    }
    c.trim_matches('`').to_string()
}

// ---------------------------------------------------------------------------
// Loader probes (key existence oracle)
// ---------------------------------------------------------------------------

/// Load `text` through the real loader from its own temp directory.
fn load_probe(text: &str) -> Result<Vec<String>, String> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("probe.toml");
    std::fs::write(&path, text).unwrap();
    match Config::load_from_file_with_warnings(&path) {
        Ok((_, warnings)) => Ok(warnings),
        Err(e) => Err(e.to_string()),
    }
}

fn unknown_warnings(warnings: &[String]) -> Vec<String> {
    warnings
        .iter()
        .filter(|w| w.contains("Unknown config key") || w.contains("Unknown config section"))
        .cloned()
        .collect()
}

/// Does `section.key` exist? A key is accepted when some candidate value
/// loads with no unknown-key/section warning.
fn key_exists(section: &str, key: &str, candidates: &[&str]) -> Result<(), String> {
    assert!(
        !candidates.is_empty(),
        "no probe candidates for {section}.{key}"
    );
    let mut last_parse_error = String::new();
    for candidate in candidates {
        let text = format!("[{section}]\n{key} = {candidate}\n");
        match load_probe(&text) {
            Ok(warnings) => {
                let bad = unknown_warnings(&warnings);
                if bad.is_empty() {
                    return Ok(());
                }
                // The warning is value-independent: no point trying others.
                return Err(bad.join("; "));
            }
            Err(e) => last_parse_error = e,
        }
    }
    Err(format!(
        "every probe value was rejected by the schema (last error: {last_parse_error})"
    ))
}

/// The complete-record TOML body for the receive-gain adapter.
fn gain_preset_record(omit: Option<&str>) -> String {
    let mut body = String::from("[rig.receive_gain_presets.\"20m\"]\n");
    for field in ["preamp_db", "attenuator_db"] {
        if Some(field) != omit {
            body.push_str(&format!("{field} = 0\n"));
        }
    }
    body
}

fn custom_band_record() -> &'static str {
    "[rig.frequency.band_plan.custom_bands.\"40m\"]\n\
     name = \"40m\"\n\
     ranges = [{ start = 7_000_000, end = 7_200_000, modes = [] }]\n\
     default_mode = \"FT8\"\n\
     band_type = \"HF\"\n"
}

/// Probe values for a documented type cell. The documented literal comes
/// first when there is one.
fn probe_candidates(row: &Row, idx: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let DefaultForm::Literal(l) = default_form(&row.defaults[idx]) {
        out.push(l);
    }
    let t = row.type_cell.to_lowercase();
    if t.starts_with("bool") {
        out.push("true".into());
    } else if t.starts_with("int") {
        out.push("1".into());
        out.push("0".into());
    } else if t.starts_with("float") {
        out.push("1.0".into());
    } else if t.starts_with("string") {
        out.push("\"pancetta_probe\"".into());
        out.push("\"\"".into());
    } else if t.starts_with("enum") {
        // The documented literal above is the only accepted probe.
    } else if t.starts_with("list") || t.contains("array") {
        out.push("[]".into());
    } else if t.starts_with("table") {
        // Structural adapters handle map rows; a plain `table` row would be
        // new and must be wired in explicitly, not silently skipped.
        out.clear();
    }
    out
}

fn check_existence(row: &Row) -> Result<(), String> {
    match row.kind {
        RowKind::Plain => {
            for idx in 0..row.keys.len() {
                let candidates = probe_candidates(row, idx);
                if candidates.is_empty() {
                    return Err(format!(
                        "unsupported type cell `{}` for `{}` (add an adapter)",
                        row.type_cell,
                        row.path(idx)
                    ));
                }
                let refs: Vec<&str> = candidates.iter().map(|s| s.as_str()).collect();
                key_exists(&row.section, &row.keys[idx], &refs)
                    .map_err(|e| format!("`{}`: {e}", row.path(idx)))?;
            }
            Ok(())
        }
        RowKind::GainPresetField => {
            // The field probed inside a complete record. A field already in
            // the record is fine; anything else must warn.
            let record = gain_preset_record(None);
            let text = if record.contains(&format!("{} =", row.keys[0])) {
                record
            } else {
                format!("{record}{} = 1\n", row.keys[0])
            };
            let warnings = load_probe(&text).map_err(|e| format!("record failed to load: {e}"))?;
            let bad = unknown_warnings(&warnings);
            if bad.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "`{}.\"<band>\".{}` inside a complete record: {}",
                    row.section,
                    row.keys[0],
                    bad.join("; ")
                ))
            }
        }
        RowKind::GainPresetMap => {
            let warnings = load_probe(&gain_preset_record(None))
                .map_err(|e| format!("band-entry record failed to load: {e}"))?;
            let bad = unknown_warnings(&warnings);
            if bad.is_empty() {
                Ok(())
            } else {
                Err(bad.join("; ").to_string())
            }
        }
        RowKind::CustomBandsMap => {
            let warnings = load_probe(custom_band_record())
                .map_err(|e| format!("custom band record failed to load: {e}"))?;
            let bad = unknown_warnings(&warnings);
            if bad.is_empty() {
                Ok(())
            } else {
                Err(bad.join("; ").to_string())
            }
        }
    }
}

/// `required` cells: omitting the field from its minimal complete record
/// must fail deserialization.
fn check_required(row: &Row) -> Result<(), String> {
    match row.kind {
        RowKind::GainPresetField => {
            let text = gain_preset_record(Some(&row.keys[0]));
            match load_probe(&text) {
                Err(_) => Ok(()),
                Ok(warnings) => Err(format!(
                    "`{}` documented `required` but a record without it loaded (warnings: {warnings:?})",
                    row.path(0)
                )),
            }
        }
        _ => Err(format!(
            "`{}`: `required` is only meaningful for record fields (found in kind {:?})",
            row.path(0),
            row.kind
        )),
    }
}

// ---------------------------------------------------------------------------
// Defaults comparison (Config::default() oracle)
// ---------------------------------------------------------------------------

fn json_at<'a>(root: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = root;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

fn parse_toml_literal(text: &str) -> Result<toml::Value, String> {
    if text == "[]" {
        return Ok(toml::Value::Array(Vec::new()));
    }
    text.parse::<toml::Value>()
        .map_err(|e| format!("cannot parse documented default `{text}` as a TOML literal: {e}"))
}

fn values_match(doc: &toml::Value, actual: &serde_json::Value) -> bool {
    match (doc, actual) {
        (toml::Value::String(a), serde_json::Value::String(b)) => a == b,
        (toml::Value::Integer(a), v) => v.as_i64() == Some(*a),
        (toml::Value::Float(a), v) => v.as_f64().is_some_and(|b| (a - b).abs() < 1e-9),
        (toml::Value::Boolean(a), serde_json::Value::Bool(b)) => a == b,
        (toml::Value::Array(a), serde_json::Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| values_match(x, y))
        }
        _ => false,
    }
}

fn describe(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unserializable>".into())
}

/// "No default entries": the map is absent or empty in `Config::default()`.
fn map_has_no_default_entries(root: &serde_json::Value, path: &str) -> Result<(), String> {
    match json_at(root, path) {
        None | Some(serde_json::Value::Null) => Ok(()),
        Some(serde_json::Value::Object(map)) if map.is_empty() => Ok(()),
        Some(v) => Err(format!(
            "documented no default entries but `Config::default()` has `{}`",
            describe(v)
        )),
    }
}

fn check_default(
    path: &str,
    form: &DefaultForm,
    defaults_root: &serde_json::Value,
) -> Result<(), String> {
    match form {
        DefaultForm::Absent => match json_at(defaults_root, path) {
            None | Some(serde_json::Value::Null) => Ok(()),
            Some(v) => Err(format!(
                "documented `absent` but `Config::default()` has `{}`",
                describe(v)
            )),
        },
        DefaultForm::NoEntries => {
            Err("`—` (no default entries) is only valid on a custom-map row".to_string())
        }
        DefaultForm::Required => {
            // Handled by check_required; nothing to compare.
            Ok(())
        }
        DefaultForm::Literal(text) => {
            let doc = parse_toml_literal(text)?;
            match json_at(defaults_root, path) {
                None | Some(serde_json::Value::Null) => Err(format!(
                    "documented `{text}` but the key has no default in `Config::default()`"
                )),
                Some(actual) if values_match(&doc, actual) => Ok(()),
                Some(actual) => Err(format!(
                    "documented `{text}`, actual `{}`",
                    describe(actual)
                )),
            }
        }
    }
}

fn check_row_defaults(row: &Row, defaults_root: &serde_json::Value) -> Result<(), String> {
    match row.kind {
        RowKind::GainPresetMap => {
            map_has_no_default_entries(defaults_root, "rig.receive_gain_presets")
        }
        RowKind::CustomBandsMap => {
            map_has_no_default_entries(defaults_root, "rig.frequency.band_plan.custom_bands")
        }
        _ => {
            for idx in 0..row.keys.len() {
                let form = default_form(&row.defaults[idx]);
                check_default(&row.path(idx), &form, defaults_root)?;
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Notes references
// ---------------------------------------------------------------------------

/// Explicit `[section].key` / `[section] key` reference, or a section-only
/// `[section]` mention.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NoteRef {
    Key { section: String, key: String },
    SectionOnly(String),
}

fn parse_explicit_ref(span: &str) -> Option<NoteRef> {
    let rest = span.strip_prefix('[')?;
    let end = rest.find(']')?;
    let section = &rest[..end];
    if !section.split('.').all(is_ident) {
        return None;
    }
    let after = &rest[end + 1..];
    if let Some(key) = after.strip_prefix('.') {
        let key = key.trim();
        if is_ident(key) {
            return Some(NoteRef::Key {
                section: section.to_string(),
                key: key.to_string(),
            });
        }
        return Some(NoteRef::SectionOnly(section.to_string()));
    }
    let word = after.trim_start();
    let key = word.split_whitespace().next().unwrap_or("");
    if is_ident(key) {
        return Some(NoteRef::Key {
            section: section.to_string(),
            key: key.to_string(),
        });
    }
    Some(NoteRef::SectionOnly(section.to_string()))
}

const NOTE_PROBE_CANDIDATES: &[&str] = &["1", "1.0", "true", "\"pancetta_probe\""];

fn check_note_ref(note_ref: &NoteRef, defaults_root: &serde_json::Value) -> Result<(), String> {
    match note_ref {
        NoteRef::Key { section, key } => key_exists(section, key, NOTE_PROBE_CANDIDATES)
            .map_err(|e| format!("reference `[{section}].{key}` does not resolve: {e}")),
        NoteRef::SectionOnly(section) => match json_at(defaults_root, section) {
            Some(_) => Ok(()),
            None => Err(format!("referenced section `[{section}]` does not exist")),
        },
    }
}

/// Check one row's Notes cell: every backticked snake_case identifier and
/// explicit `[section].key` reference must resolve in context.
fn check_notes(
    row: &Row,
    defaults_root: &serde_json::Value,
    exceptions_used: &mut Vec<&'static str>,
) -> Result<(), String> {
    for (n, span) in row.notes.split('`').enumerate() {
        if n % 2 == 0 {
            continue;
        }
        if span.contains("::") {
            continue; // Rust-qualified path, not a config key
        }
        if let Some(note_ref) = parse_explicit_ref(span) {
            check_note_ref(&note_ref, defaults_root)?;
            continue;
        }
        if !(is_ident(span) && span.contains('_')) {
            continue;
        }
        if let Some((name, _reason)) = EXCEPTIONS.iter().copied().find(|&(name, _)| name == span) {
            exceptions_used.push(name);
            continue;
        }
        let result = match row.kind {
            RowKind::Plain => key_exists(&row.section, span, NOTE_PROBE_CANDIDATES),
            RowKind::GainPresetField | RowKind::GainPresetMap => {
                let record = gain_preset_record(None);
                if record.contains(&format!("{span} =")) {
                    Ok(())
                } else {
                    let text = format!("{record}{span} = 1\n");
                    let warnings =
                        load_probe(&text).map_err(|e| format!("record failed to load: {e}"))?;
                    let bad = unknown_warnings(&warnings);
                    if bad.is_empty() {
                        Ok(())
                    } else {
                        Err(bad.join("; ").to_string())
                    }
                }
            }
            RowKind::CustomBandsMap => key_exists(
                &format!("{}.custom_bands", row.section),
                span,
                NOTE_PROBE_CANDIDATES,
            ),
        };
        result.map_err(|e| {
            format!(
                "notes name `{span}` in section `[{}]` is not a config key: {e}",
                row.section
            )
        })?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Whole-document check
// ---------------------------------------------------------------------------

fn doc_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("docs")
        .join("CONFIG.md")
}

fn doc_label(line: usize) -> String {
    format!("docs/CONFIG.md:{line}")
}

/// The full drift check over a document. Returns one error per failure,
/// each prefixed with its location.
fn check_document(content: &str) -> Vec<String> {
    let rows = match parse_rows(content) {
        Ok(rows) => rows,
        Err(e) => return vec![e],
    };
    let defaults_root = serde_json::to_value(Config::default()).unwrap();
    let mut failures = Vec::new();
    let mut exceptions_used: Vec<&'static str> = Vec::new();
    for row in &rows {
        if let Err(e) = check_existence(row) {
            failures.push(format!("{}: {e}", doc_label(row.line)));
        }
        if row
            .defaults
            .iter()
            .any(|d| default_form(d) == DefaultForm::Required)
        {
            if let Err(e) = check_required(row) {
                failures.push(format!("{}: {e}", doc_label(row.line)));
            }
        }
        if let Err(e) = check_row_defaults(row, &defaults_root) {
            failures.push(format!("{}: default mismatch: {e}", doc_label(row.line)));
        }
        if let Err(e) = check_notes(row, &defaults_root, &mut exceptions_used) {
            failures.push(format!("{}: {e}", doc_label(row.line)));
        }
    }
    failures
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn documented_config_tables_match_runtime() {
    let content = std::fs::read_to_string(doc_path()).unwrap();
    let rows = parse_rows(&content).expect("CONFIG.md tables parse");

    // Non-vacuity: an extractor regression must not pass trivially.
    assert!(
        rows.len() >= 57,
        "found only {} key/type/default rows (expected at least 57)",
        rows.len()
    );
    let table_headers = content
        .lines()
        .filter(|l| l.trim() == "| Key | Type | Default | Notes |")
        .count();
    assert!(
        table_headers >= 11,
        "found only {} key tables (expected at least 11)",
        table_headers
    );
    let has = |section: &str, key: &str| {
        rows.iter()
            .any(|r| r.section == section && r.keys.iter().any(|k| k == key))
    };
    assert!(
        has("station", "operator_name"),
        "station.operator_name row missing"
    );
    assert!(
        has("rig", "interface.port"),
        "rig.interface.port row missing"
    );
    assert!(
        has("autonomous", "max_concurrent_qsos"),
        "autonomous.max_concurrent_qsos row missing"
    );
    assert!(
        rows.iter()
            .any(|r| r.section == "hound" && r.keys.len() == 2),
        "hound paired rows missing"
    );

    // Optional and map keys are genuinely covered, not skipped.
    assert!(
        rows.iter().any(|r| r.kind == RowKind::GainPresetMap),
        "receive_gain_presets map row missing"
    );
    assert!(
        rows.iter().any(|r| r.kind == RowKind::CustomBandsMap),
        "custom_bands map row missing"
    );
    assert!(
        rows.iter().any(|r| r.section == "rig"
            && r.keys.contains(&"data_mode".to_string())
            && default_form(&r.defaults[0]) == DefaultForm::Absent),
        "rig.data_mode absent row missing"
    );

    let failures = check_document(&content);
    assert!(
        failures.is_empty(),
        "{} documented table drift(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn notes_exceptions_are_justified_and_used() {
    let content = std::fs::read_to_string(doc_path()).unwrap();
    let rows = parse_rows(&content).unwrap();
    for (name, reason) in EXCEPTIONS {
        assert!(!reason.is_empty(), "exception `{name}` needs a reason");
        let used = rows.iter().any(|r| {
            r.notes
                .split('`')
                .enumerate()
                .any(|(n, s)| n % 2 == 1 && s == *name)
        });
        assert!(
            used,
            "notes exception `{name}` is unused — remove it ({reason})"
        );
    }
}

#[test]
fn unknown_key_reports_location() {
    let doc = "## `[fox]`\n\n| Key | Type | Default | Notes |\n|---|---|---|---|\n| `max_stream` | integer | absent | Wrong key. |\n";
    let failures = check_document(doc);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].contains("docs/CONFIG.md:5"),
        "failure must name the line: {}",
        failures[0]
    );
    assert!(
        failures[0].contains("fox.max_stream"),
        "failure must name the resolved path: {}",
        failures[0]
    );
}

#[test]
fn optional_and_map_keys_are_accepted() {
    // An Option key absent from serialized defaults is still a real key.
    key_exists("station", "operator_name", &["\"pancetta_probe\""])
        .expect("station.operator_name must be accepted by the loader");
    key_exists("rig", "data_mode", &["\"PKTUSB\""]).expect("rig.data_mode must be accepted");
    let warnings = load_probe(&gain_preset_record(None)).unwrap();
    assert!(unknown_warnings(&warnings).is_empty());
    let warnings = load_probe(custom_band_record()).unwrap();
    assert!(unknown_warnings(&warnings).is_empty());
}

#[test]
fn wrong_default_reports_both_values() {
    let defaults_root = serde_json::to_value(Config::default()).unwrap();
    // The real documented value matches...
    check_default(
        "station.callsign",
        &DefaultForm::Literal("\"N0CALL\"".into()),
        &defaults_root,
    )
    .expect("N0CALL is the real default");
    // ...and a wrong one reports documented and actual values.
    let err = check_default(
        "station.callsign",
        &DefaultForm::Literal("\"WRONG\"".into()),
        &defaults_root,
    )
    .unwrap_err();
    assert!(
        err.contains("WRONG") && err.contains("N0CALL"),
        "failure must name documented and actual values: {err}"
    );
    let err = check_default(
        "rig.interface.port",
        &DefaultForm::Literal("\"\"".into()),
        &defaults_root,
    )
    .unwrap_err();
    assert!(
        err.contains("/dev/ttyUSB0"),
        "the original documented-empty port must be caught: {err}"
    );
    // `absent` versus a real default is caught too.
    let err =
        check_default("rig.interface.port", &DefaultForm::Absent, &defaults_root).unwrap_err();
    assert!(err.contains("/dev/ttyUSB0"), "{err}");
}

#[test]
fn default_cell_forms_are_checked() {
    assert_eq!(default_form("absent"), DefaultForm::Absent);
    assert_eq!(default_form("required"), DefaultForm::Required);
    assert_eq!(default_form("—"), DefaultForm::NoEntries);
    assert_eq!(
        default_form("`\"N0CALL\"`"),
        DefaultForm::Literal("\"N0CALL\"".into())
    );
    assert_eq!(default_form("`100`"), DefaultForm::Literal("100".into()));
    // Prose after the literal is ignored.
    assert_eq!(
        default_form("`[]` (empty)"),
        DefaultForm::Literal("[]".into())
    );
    assert!(parse_toml_literal("[]").is_ok());
    assert!(parse_toml_literal("\"WSJT-X - pancetta\"").is_ok());
    assert!(parse_toml_literal("3.0").is_ok());
}

#[test]
fn table_note_key_references_match_runtime() {
    let defaults_root = serde_json::to_value(Config::default()).unwrap();
    let mut used = Vec::new();

    // A valid optional key named in notes passes.
    let row = Row {
        line: 1,
        section: "station".into(),
        keys: vec!["callsign".into()],
        type_cell: "string".into(),
        defaults: vec!["`\"N0CALL\"`".into()],
        notes: "See `operator_name` too.".into(),
        kind: RowKind::Plain,
    };
    assert!(check_notes(&row, &defaults_root, &mut used).is_ok());

    // An identifier from another section does not resolve here.
    let row = Row {
        notes: "Uses `max_streams`.".into(),
        ..row
    };
    assert!(check_notes(&row, &defaults_root, &mut used).is_err());

    // Explicit references resolve in their own section.
    let row = Row {
        notes: "Clamps `[fox].max_streams` and `[audio] sample_rate = 48000`.".into(),
        ..row
    };
    assert!(check_notes(&row, &defaults_root, &mut used).is_ok());

    // Rust-qualified paths are not config keys.
    let row = Row {
        notes: "See `pancetta_config::Config`.".into(),
        ..row
    };
    assert!(check_notes(&row, &defaults_root, &mut used).is_ok());
}

#[test]
fn original_fox_note_is_rejected() {
    let defaults_root = serde_json::to_value(Config::default()).unwrap();
    let row = Row {
        line: 513,
        section: "autonomous".into(),
        keys: vec!["max_concurrent_qsos".into()],
        type_cell: "integer".into(),
        defaults: vec!["`1`".into()],
        notes: "Also caps manual opens (Fox mode uses `fox_max_streams`).".into(),
        kind: RowKind::Plain,
    };
    let mut used = Vec::new();
    let err = check_notes(&row, &defaults_root, &mut used).unwrap_err();
    assert!(
        err.contains("fox_max_streams"),
        "the original wording must be rejected: {err}"
    );

    // The corrected explicit reference passes.
    let row = Row {
        notes: "Also caps manual opens (Fox mode uses `[fox].max_streams`).".into(),
        ..row
    };
    assert!(check_notes(&row, &defaults_root, &mut used).is_ok());
}

#[test]
fn paired_keys_placeholders_and_required_forms_parse() {
    let doc = "## `[hound]`\n\n| Key | Type | Default | Notes |\n|---|---|---|---|\n| `call_min_hz` / `call_max_hz` | float | `1000.0` / `2700.0` | Range. |\n\n## `[rig.receive_gain_presets]`\n\n| Key | Type | Default | Notes |\n|---|---|---|---|\n| `\"<band>\"` | table | absent | Map. |\n| `preamp_db` | integer | required | Field. |\n";
    let rows = parse_rows(doc).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].keys, vec!["call_min_hz", "call_max_hz"]);
    assert_eq!(
        rows[0].defaults,
        vec!["`1000.0`".to_string(), "`2700.0`".to_string()]
    );
    assert_eq!(rows[1].kind, RowKind::GainPresetMap);
    assert_eq!(rows[2].kind, RowKind::GainPresetField);
    assert_eq!(default_form(&rows[2].defaults[0]), DefaultForm::Required);

    // A nested typo inside a record table is caught.
    let defaults_root = serde_json::to_value(Config::default()).unwrap();
    let mut used = Vec::new();
    let err = check_notes(
        &Row {
            notes: "Uses `preamp_dbb`.".into(),
            ..rows[2].clone()
        },
        &defaults_root,
        &mut used,
    )
    .unwrap_err();
    assert!(err.contains("preamp_dbb"), "{err}");
}

#[test]
fn fences_whitespace_and_malformed_rows() {
    // A table inside a fenced block is ignored...
    let doc = "## `[fox]`\n\n```toml\n| Key | Type | Default | Notes |\n|---|---|---|---|\n| `fenced` | integer | `1` | Ignored. |\n```\n\n| Key | Type | Default | Notes |\n|---|---|---|---|\n| ` max_streams ` | integer | ` 3 ` | Real. |\n";
    let rows = parse_rows(doc).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].keys, vec!["max_streams".to_string()]);
    assert_eq!(rows[0].defaults, vec!["` 3 `".to_string()]);
    assert_eq!(
        default_form(&rows[0].defaults[0]),
        DefaultForm::Literal("3".into())
    );

    // ...and a malformed row fails instead of being skipped.
    let doc = "## `[fox]`\n\n| Key | Type | Default | Notes |\n|---|---|---|---|\n| `max_streams` | integer | `5` |\n";
    assert!(parse_rows(doc).is_err());

    // Unbracketed subheadings preserve the current section.
    let doc = "## `[rig]`\n\n### Optional extras\n\n| Key | Type | Default | Notes |\n|---|---|---|---|\n| `model` | string | `\"Generic\"` | Model. |\n";
    let rows = parse_rows(doc).unwrap();
    assert_eq!(rows[0].section, "rig");
}
