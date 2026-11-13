//! PAN-323: the coordinator module table in docs/ARCHITECTURE.md is
//! generated from the directory inventory of `pancetta/src/coordinator`
//! and must stay current.
//!
//! The filesystem supplies the names; the role text is curated in the
//! `roles()` map below (read each module's docs before editing it). A
//! module added without a role, a module deleted while its row remains,
//! or a stale/duplicated row fails the test. Regenerate the marked block
//! with:
//!
//! ```sh
//! PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-config --test architecture_drift coordinator_module_table_is_current
//! ```
//!
//! Normal test runs never write docs. `documented_coordinator_paths_exist`
//! additionally checks every backticked `pancetta/src/coordinator/...`
//! path in the document against the filesystem.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const START_MARKER: &str = "<!-- coordinator-modules:start -->";
const END_MARKER: &str = "<!-- coordinator-modules:end -->";

fn doc_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("docs")
        .join("ARCHITECTURE.md")
}

fn coordinator_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("pancetta")
        .join("src")
        .join("coordinator")
}

/// One concise, accurate role per immediate coordinator module. Keys are
/// names without the directory suffix. Keep in sync with the module docs.
fn roles() -> BTreeMap<&'static str, &'static str> {
    let entries: [(&str, &str); 46] = [
        ("adif_backup", "Changed startup ADIF snapshots and independent retention."),
        (
            "alerts",
            "Decode alert evaluation and bell/ntfy/webhook deliveries.",
        ),
        ("audio", "Audio component startup."),
        (
            "audio_recovery",
            "Pure recovery decisions for the audio thread.",
        ),
        ("autonomous", "Autonomous operator component."),
        (
            "clock_monitor",
            "Live-session clock-skew monitor (PAN-114).",
        ),
        (
            "config_reload",
            "Applying saved config edits: live security fields, restart list.",
        ),
        ("contest", "Durable contest scoring refresh."),
        (
            "desense",
            "Window-based evidence for TX-induced receive desense.",
        ),
        (
            "dsp",
            "Receive audio downmix, subsampling, decode windows and waterfall.",
        ),
        ("dx_cluster", "DX cluster component."),
        (
            "effort",
            "Decode-effort preset to wall-time budget mapping.",
        ),
        ("ft8", "FT8 decoder component startup."),
        (
            "gain_sweep",
            "Receive-gain experiment measurements and inference (PAN-244).",
        ),
        (
            "gain_sweep_runtime",
            "Coordinator runtime driving the receive-gain experiment.",
        ),
        (
            "hamlib",
            "rigctld TCP client: poll loop, watchdog, SWR metering, gain transactions.",
        ),
        (
            "health",
            "Health checks, panic counting, audit-log retention, supervisor restart policy.",
        ),
        (
            "mod",
            "ApplicationCoordinator: component lifecycle and startup sequencing.",
        ),
        (
            "pipeline",
            "Decode-pipeline crossbeam channel wiring and startup.",
        ),
        ("psk_reporter", "PSKReporter component."),
        (
            "qrz_lookup",
            "Interactive, session-cached QRZ station lookups.",
        ),
        (
            "qso",
            "QSO state-machine component and ADIF/SQLite persistence wiring.",
        ),
        (
            "qso_filter",
            "QSO partner band-collapse filtering (hb-229).",
        ),
        (
            "qso_upload_status",
            "Registry of per-QSO upload results for the TUI (PAN-344).",
        ),
        (
            "raw_capture",
            "Timed raw 48 kHz capture, one WAV per FT8 slot (PAN-393).",
        ),
        (
            "readiness",
            "Coordinator side of the TUI readiness strip (PAN-89).",
        ),
        (
            "receive_overload",
            "Receive-window overload policy and bounded ATT transactions.",
        ),
        (
            "remote_gateway",
            "Local read-only WebSocket gateway (Panino).",
        ),
        (
            "replay",
            "Replay mode: listing recorded WAVs and feeding decode windows.",
        ),
        (
            "restart_budget",
            "Reconnect/restart backoff and restart budgets.",
        ),
        (
            "session_state",
            "Session choices (TX-frequency mode, effort preset) restored at startup (PAN-270).",
        ),
        (
            "service_notify",
            "systemd READY/WATCHDOG/STOPPING notifications driven by completed health passes (PAN-335).",
        ),
        (
            "shutdown",
            "Graceful shutdown, task join and direct rigctld PTT-off budgets.",
        ),
        (
            "spot_call",
            "Space on a network spot: QSY to its dial and call (PAN-287).",
        ),
        (
            "station_agent",
            "Station agent component (remote operation, Noise transport).",
        ),
        ("swr_guard", "SWR protection trip logic (PAN-251)."),
        ("tier", "Hardware-tier auto-classification at startup."),
        ("tui_relay", "TUI relay and command forwarding."),
        ("tx", "TX worker scheduling, playback and PTT gates."),
        (
            "tx_halt",
            "Station-wide TX halt shared by emergency stop and SWR trip.",
        ),
        (
            "tx_log",
            "Durable TX log, one JSON line per keyed frame (PAN-122).",
        ),
        (
            "tx_outcome",
            "Single reporting point for TX frame outcomes (PAN-279).",
        ),
        (
            "upload_outbox",
            "Persistent pending-upload worker with per-destination retry lanes (PAN-57).",
        ),
        ("util", "Linear resampling for WAV playback."),
        ("wav_playback", "WAV file playback for offline testing."),
        ("wsjtx_udp", "WSJT-X-compatible UDP interop component."),
    ];
    BTreeMap::from(entries)
}

/// Immediate `.rs` files and subdirectories of `dir`; directories carry a
/// trailing `/`. Sorted, no recursion.
fn inventory(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {dir:?}: {e}")) {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        if entry.file_type().unwrap().is_dir() {
            names.push(format!("{name}/"));
        } else if name.ends_with(".rs") {
            names.push(name);
        }
    }
    names.sort();
    names
}

/// The role-map key of an inventory name: `tx.rs` and `tx/` both map to
/// `tx` (one curated role, two distinct table rows).
fn module_key(name: &str) -> &str {
    name.trim_end_matches('/').trim_end_matches(".rs")
}

/// Render `| File | Role |` from an inventory. An entry without a curated
/// role fails even during regeneration.
fn render(inv: &[String], roles: &BTreeMap<&str, &str>) -> String {
    let mut out = String::from("| File | Role |\n|---|---|\n");
    for name in inv {
        let role = roles.get(module_key(name)).unwrap_or_else(|| {
            panic!(
                "no role for coordinator module `{name}` — add one to `roles()` in \
                 pancetta-config/tests/architecture_drift.rs"
            )
        });
        out.push_str(&format!("| `{name}` | {role} |\n"));
    }
    out
}

/// The generated block between the markers, with byte offsets so callers
/// can verify the surrounding text is untouched. Errs when a marker is
/// missing or duplicated.
fn extract_block(text: &str) -> Result<(usize, usize, &str), String> {
    let count = |needle: &str| text.matches(needle).count();
    if count(START_MARKER) != 1 {
        return Err(format!(
            "ARCHITECTURE.md must contain `{START_MARKER}` exactly once (found {})",
            count(START_MARKER)
        ));
    }
    if count(END_MARKER) != 1 {
        return Err(format!(
            "ARCHITECTURE.md must contain `{END_MARKER}` exactly once (found {})",
            count(END_MARKER)
        ));
    }
    let start = text.find(START_MARKER).unwrap() + START_MARKER.len();
    let end = text.find(END_MARKER).unwrap();
    if end < start {
        return Err(format!("`{END_MARKER}` appears before `{START_MARKER}`"));
    }
    Ok((start, end, &text[start..end]))
}

/// Replace the marked block's content, leaving every byte outside it
/// untouched.
fn replace_block(text: &str, new_block: &str) -> Result<String, String> {
    let (start, end, _) = extract_block(text)?;
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(new_block);
    out.push_str(&text[end..]);
    Ok(out)
}

/// Row-level diff between the block in `text` and `expected`.
fn table_diff(block: &str, expected: &str) -> Vec<String> {
    let mut failures = Vec::new();
    let current_rows: Vec<&str> = block.lines().filter(|l| l.starts_with('|')).collect();
    let expected_rows: Vec<&str> = expected.lines().filter(|l| l.starts_with('|')).collect();
    let mut seen = std::collections::BTreeSet::new();
    for row in &current_rows {
        if !seen.insert(row.to_string()) {
            failures.push(format!("duplicate row: {row}"));
        }
    }
    for row in &expected_rows {
        if !current_rows.contains(row) {
            failures.push(format!("missing row: {row}"));
        }
    }
    for row in &current_rows {
        if !expected_rows.contains(row) {
            failures.push(format!("extra or stale row: {row}"));
        }
    }
    failures
}

#[test]
fn coordinator_module_table_is_current() {
    let roles = roles();
    let dir = coordinator_dir();
    let inv = inventory(&dir);

    // The role map and the filesystem must agree exactly (`tx.rs` and `tx/`
    // share the `tx` role key).
    let fs_keys: std::collections::BTreeSet<String> =
        inv.iter().map(|n| module_key(n).to_string()).collect();
    let role_keys: std::collections::BTreeSet<String> =
        roles.keys().map(|k| k.to_string()).collect();
    assert_eq!(
        fs_keys, role_keys,
        "coordinator directory and role map disagree"
    );
    assert!(
        roles.values().all(|r| !r.is_empty()),
        "every coordinator module needs a nonempty role"
    );
    assert!(
        inv.iter().any(|n| n == "tx.rs") && inv.iter().any(|n| n == "tx/"),
        "tx.rs and tx/ must stay distinct entries"
    );

    let expected = render(&inv, &roles);
    let path = doc_path();
    let text = std::fs::read_to_string(&path).unwrap();
    let (_, _, block) = extract_block(&text)
        .unwrap_or_else(|e| panic!("docs/ARCHITECTURE.md coordinator table block: {e}"));

    if std::env::var("PANCETTA_REGEN_DOCS").is_ok() {
        // The block starts with the newline that ends the marker line, so
        // the regenerated table stays on its own line.
        let updated = replace_block(&text, &format!("\n{expected}")).unwrap();
        std::fs::write(&path, updated).expect("regenerate ARCHITECTURE.md");
        return;
    }

    let failures = table_diff(block, &expected);
    assert!(
        failures.is_empty(),
        "docs/ARCHITECTURE.md coordinator module table is stale ({}):\n{}\n\
         Regenerate with:\n  PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-config \
         --test architecture_drift coordinator_module_table_is_current",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn module_inventory_changes_fail() {
    let roles = roles();
    let base = vec![
        "mod.rs".to_string(),
        "pipeline.rs".to_string(),
        "tx.rs".to_string(),
        "tx/".to_string(),
    ];
    let expected = render(&base, &roles);

    // A module present on disk but missing from the table.
    let stale = "| File | Role |\n|---|---|\n| `mod.rs` | Coordinator. |\n| `pipeline.rs` | Channels. |\n| `tx.rs` | TX worker. |\n";
    let failures = table_diff(stale, &expected);
    assert!(
        failures
            .iter()
            .any(|f| f.contains("missing row") && f.contains("`tx/`")),
        "{failures:?}"
    );

    // A row for a module that no longer exists.
    let extra = format!("{stale}| `tx/` | TX helpers. |\n| `gone.rs` | Ghost. |\n");
    let failures = table_diff(&extra, &expected);
    assert!(
        failures
            .iter()
            .any(|f| f.contains("extra or stale row") && f.contains("gone.rs")),
        "{failures:?}"
    );

    // A duplicated row.
    let dup = format!("{stale}| `tx/` | TX helpers. |\n| `tx/` | TX helpers. |\n");
    let failures = table_diff(&dup, &expected);
    assert!(
        failures.iter().any(|f| f.contains("duplicate row")),
        "{failures:?}"
    );
}

#[test]
fn regeneration_preserves_surrounding_text() {
    let text = "Intro prose.\n\n<!-- coordinator-modules:start -->\nold block\n<!-- coordinator-modules:end -->\n\nOutro prose.\n";
    // The block is everything between the markers (here "\nold block\n");
    // replacing it must not touch a byte outside.
    let replaced = replace_block(text, "\nnew block\n").unwrap();
    assert_eq!(
        replaced,
        "Intro prose.\n\n<!-- coordinator-modules:start -->\nnew block\n<!-- coordinator-modules:end -->\n\nOutro prose.\n"
    );
    assert!(replace_block(text, "\nx\n")
        .unwrap()
        .starts_with("Intro prose.\n\n"));
    assert!(replace_block(text, "\nx\n")
        .unwrap()
        .ends_with("Outro prose.\n"));

    // Missing and duplicated markers are refused.
    assert!(extract_block("no markers here").is_err());
    let dup_start = format!("{START_MARKER}{START_MARKER}{END_MARKER}");
    assert!(extract_block(&dup_start).is_err());
    let dup_end = format!("{START_MARKER}{END_MARKER}{END_MARKER}");
    assert!(extract_block(&dup_end).is_err());
    let reversed = format!("{END_MARKER}{START_MARKER}");
    assert!(extract_block(&reversed).is_err());
}

#[test]
fn documented_coordinator_paths_exist() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let text = std::fs::read_to_string(doc_path()).unwrap();
    let mut checked = 0;
    for (n, span) in text.split('`').enumerate() {
        if n % 2 == 0 || !span.starts_with("pancetta/src/coordinator/") {
            continue;
        }
        // Skip brace-expanded or wildcard paths; only literal paths.
        if !span
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '/' | '.' | '-'))
        {
            continue;
        }
        let rel = span.trim_end_matches('/');
        let path = root.join(rel);
        assert!(
            path.exists(),
            "docs/ARCHITECTURE.md names `{span}` but {rel} does not exist"
        );
        checked += 1;
    }
    assert!(
        checked >= 2,
        "expected to check several coordinator paths, checked {checked}"
    );
}
