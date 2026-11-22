//! PAN-358 invariant: one UTC slot clock.
//!
//! The autonomous slot tick, the QSO watchdog, the queued-call TTL and the
//! clock-skew monitor read time through the coordinator's one
//! `pancetta_core::SlotClock`, the only wall-clock step detector for them.
//! This scan keeps the per-site detectors and slot math those consumers used
//! to carry from coming back, and checks that each consumer stays wired to
//! the coordinator's clock. Comments count: a deleted item named in a
//! comment is stale documentation. Needles are built at runtime so this file
//! never matches itself.
//!
//! The DSP decode-window cutter and the TX worker still carry their PAN-114
//! checks (`docs/DECISIONS/tx-scheduling.md`, "One UTC slot clock"); their
//! needles join this scan when they move onto the clock.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("pancetta crate sits in the workspace root")
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

/// Every `.rs` file under the scanned crates' `src` directories.
fn scanned_files() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut files = Vec::new();
    for krate in ["pancetta", "pancetta-qso", "pancetta-core", "pancetta-tui"] {
        rust_files(&root.join(krate).join("src"), &mut files);
    }
    assert!(
        files.len() > 50,
        "scan found too few files: {}",
        files.len()
    );
    files
}

fn read(rel: &str) -> String {
    let path = workspace_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Builds `a` + `b` at runtime so the literal never appears in this file.
fn needle(a: &str, b: &str) -> String {
    format!("{a}{b}")
}

#[test]
fn no_per_site_slot_clock_code_remains() {
    let banned = [
        // The QSO watchdog's and clock-skew monitor's own step detector.
        needle("ClockStep", "Detector"),
        // The autonomous tick's deadline helpers and ticker.
        needle("Slot", "Ticker"),
        needle("poll_slot_", "deadline"),
        // The queued-call TTL's private wall+monotonic stamp.
        needle("Queue", "Stamp"),
        // The watchdog pass that took its own detector.
        needle("watchdog_pass", "_at"),
    ];
    let root = workspace_root();
    let mut found = Vec::new();
    for file in scanned_files() {
        let text = std::fs::read_to_string(&file).unwrap();
        for (i, line) in text.lines().enumerate() {
            for b in &banned {
                if line.contains(b.as_str()) {
                    let rel = file.strip_prefix(&root).unwrap();
                    found.push(format!("{}:{}: {}", rel.display(), i + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "per-site slot-clock code is back; read time through the shared \
         SlotClock instead (PAN-358):\n{}",
        found.join("\n")
    );
}

#[test]
fn every_consumer_is_wired_to_the_coordinator_clock() {
    let autonomous = read("pancetta/src/coordinator/autonomous.rs");
    assert!(
        autonomous.contains(&needle("self.slot", "_clock()")),
        "the autonomous task must capture the coordinator's slot clock"
    );
    assert!(
        autonomous.contains(&needle("SlotTimer", "::new")),
        "the autonomous slot tick must run on a core SlotTimer"
    );

    let qso = read("pancetta/src/coordinator/qso.rs");
    assert!(
        qso.contains(&needle("set_slot_clock(self", ".slot_clock())")),
        "the QSO watchdog must read the coordinator's slot clock"
    );
    assert!(
        qso.contains(&needle("spawn_queued_call", "_ttl(")),
        "the queued-call TTL task must run through spawn_queued_call_ttl"
    );

    let manager = read("pancetta-qso/src/qso_manager.rs");
    assert!(
        manager.contains(&needle("self.watchdog", "_pass().await")),
        "the QSO watchdog loop must take steps from the slot clock"
    );

    let monitor = read("pancetta/src/coordinator/clock_monitor.rs");
    assert!(
        !monitor.contains(&needle("Wall", "Clock")),
        "the clock-skew monitor must not take a wall clock of its own"
    );
    assert!(
        monitor.contains(&needle("clock: Slot", "Clock")),
        "the clock-skew monitor must read the shared SlotClock"
    );

    let coordinator = read("pancetta/src/coordinator/mod.rs");
    assert!(
        coordinator.contains(&needle("spawn_slot_clock", "_driver(")),
        "the coordinator must start the slot-clock driver"
    );
}
