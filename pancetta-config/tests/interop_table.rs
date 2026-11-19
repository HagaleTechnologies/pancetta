//! PAN-382: the logger compatibility table in `docs/INTEROP.md`.
//!
//! The table lists GridTracker 2, JTAlert, Log4OM 2 and WaveLogGate on both
//! outbound channels (WSJT-X UDP and N1MM contactinfo), one row each. A
//! Verified cell is a `YYYY-MM-DD` date (optionally followed by the program
//! version and host), `unverified`, or `unverified (no Windows station)`.
//! Undated rows claim no result (`Not checked` in both result columns), and a
//! dated row must say what was seen. JTAlert and Log4OM 2 are Windows-only, so
//! their undated rows name the missing station.

use std::collections::BTreeSet;
use std::path::Path;

const HEADER: &str =
    "| Program | Platform | Channel | Vendor documentation says | What works | What does not work | Verified |";
const PROGRAMS: [&str; 4] = ["GridTracker 2", "JTAlert", "Log4OM 2", "WaveLogGate"];
const CHANNELS: [&str; 2] = ["WSJT-X UDP", "N1MM contactinfo"];
const WINDOWS_ONLY: [&str; 2] = ["JTAlert", "Log4OM 2"];
const NOT_CHECKED: &str = "Not checked";

struct Row {
    program: String,
    channel: String,
    works: String,
    fails: String,
    verified: String,
}

impl Row {
    fn is_dated(&self) -> bool {
        self.verified.len() >= 10
            && self.verified.is_char_boundary(10)
            && chrono::NaiveDate::parse_from_str(&self.verified[..10], "%Y-%m-%d").is_ok()
    }
}

/// Split a Markdown table row on unescaped `|`, trimming each cell.
fn cells(line: &str) -> Vec<String> {
    let inner = line.trim().trim_start_matches('|').trim_end_matches('|');
    let mut out = Vec::new();
    let mut cell = String::new();
    let mut escaped = false;
    for c in inner.chars() {
        if escaped {
            cell.push(c);
            escaped = false;
        } else if c == '\\' {
            cell.push(c);
            escaped = true;
        } else if c == '|' {
            out.push(cell.trim().to_string());
            cell.clear();
        } else {
            cell.push(c);
        }
    }
    out.push(cell.trim().to_string());
    out
}

fn rows() -> Vec<Row> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/INTEROP.md");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let mut lines = text.lines().skip_while(|l| l.trim() != HEADER);
    assert!(
        lines.next().is_some(),
        "compatibility table header not found in docs/INTEROP.md"
    );
    let separator = lines.next().expect("table separator row");
    assert!(
        separator.trim().starts_with("|---"),
        "expected a separator row, got {separator}"
    );
    lines
        .take_while(|l| l.trim().starts_with('|'))
        .map(|line| {
            let c = cells(line);
            assert_eq!(c.len(), 7, "row does not have 7 cells: {line}");
            Row {
                program: c[0].clone(),
                channel: c[2].clone(),
                works: c[4].clone(),
                fails: c[5].clone(),
                verified: c[6].clone(),
            }
        })
        .collect()
}

#[test]
fn interop_table_lists_every_program_on_both_channels() {
    let rows = rows();
    assert_eq!(rows.len(), 8, "expected 8 rows, found {}", rows.len());
    let found: Vec<(String, String)> = rows
        .iter()
        .map(|r| (r.program.clone(), r.channel.clone()))
        .collect();
    let unique: BTreeSet<(String, String)> = found.iter().cloned().collect();
    assert_eq!(unique.len(), found.len(), "duplicate rows: {found:?}");
    let expected: BTreeSet<(String, String)> = PROGRAMS
        .iter()
        .flat_map(|p| CHANNELS.iter().map(|c| (p.to_string(), c.to_string())))
        .collect();
    assert_eq!(unique, expected);
}

#[test]
fn verified_cells_are_a_date_or_unverified() {
    for row in rows() {
        assert!(
            row.is_dated()
                || row.verified == "unverified"
                || row.verified == "unverified (no Windows station)",
            "{} / {}: Verified cell {:?} is neither a YYYY-MM-DD date nor unverified",
            row.program,
            row.channel,
            row.verified
        );
    }
}

#[test]
fn windows_only_rows_name_the_missing_station() {
    for row in rows().into_iter().filter(|r| !r.is_dated()) {
        let expected = if WINDOWS_ONLY.contains(&row.program.as_str()) {
            "unverified (no Windows station)"
        } else {
            "unverified"
        };
        assert_eq!(
            row.verified, expected,
            "{} / {}: undated Verified cell",
            row.program, row.channel
        );
    }
}

#[test]
fn undated_rows_claim_no_result() {
    for row in rows() {
        if row.is_dated() {
            assert!(
                row.works != NOT_CHECKED && row.fails != NOT_CHECKED,
                "{} / {}: a dated row must say what was seen",
                row.program,
                row.channel
            );
        } else {
            assert_eq!(
                (row.works.as_str(), row.fails.as_str()),
                (NOT_CHECKED, NOT_CHECKED),
                "{} / {}: an undated row claims a result",
                row.program,
                row.channel
            );
        }
    }
}
