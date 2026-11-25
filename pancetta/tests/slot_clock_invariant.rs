//! PAN-358 invariant: one UTC slot clock.
//!
//! The autonomous slot tick, the DSP decode-window cutter, the QSO watchdog,
//! the queued-call TTL, the clock-skew monitor and the TX worker read time
//! through the coordinator's one `pancetta_core::SlotClock`, the only
//! wall-clock step detector for them. This scan keeps the per-site detectors
//! and slot math those consumers used to carry from coming back, and checks
//! that each consumer stays wired to the coordinator's clock. Comments
//! count: a deleted item named in a comment is stale documentation. Needles
//! are built at runtime so this file never matches itself.
//!
//! The TX worker moved onto the clock in PAN-483: its PAN-114 wait band and
//! PTT-hold bound are gone, and a frame scheduled before a resync is dropped
//! (`docs/DECISIONS/tx-scheduling.md`, "One UTC slot clock"). PAN-484 added
//! its former checks and its wiring to this scan. PAN-489 added the DX-parity
//! gate (`tx/gate.rs`, `tx/bundle.rs`), which ages the DX's sightings against
//! the worker's clock reading and holds a frame across a resync.
//!
//! PAN-477 moved the remaining slot math onto the shared grid: decode parity
//! and the A7 seed age come from the slot the DSP cut the window from
//! (`DecodeWindow::slot`), and the always-answer caller key, the keep-call
//! TX slot (`pancetta_qso::tx_slot`), the replay pre-roll and the TUI QSO
//! timing row follow the active period. Two sites stay FT8-only by recorded
//! decision, with the reason in their code: raw 48 kHz capture
//! (`docs/DECISIONS/config-and-platform.md`, raw capture decisions 3 and 6)
//! and the receive-gain sweep (refused outside FT8). The last test keeps
//! each site's former code from coming back.

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

/// The TX worker's slot-timing files, including the DX-parity gate (PAN-489).
const TX_TIMING_FILES: [&str; 5] = [
    "pancetta/src/coordinator/tx/worker.rs",
    "pancetta/src/coordinator/tx/ptt.rs",
    "pancetta/src/coordinator/tx/supersede.rs",
    "pancetta/src/coordinator/tx/gate.rs",
    "pancetta/src/coordinator/tx/bundle.rs",
];

fn read(rel: &str) -> String {
    let path = workspace_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Builds `a` + `b` at runtime so the literal never appears in this file.
fn needle(a: &str, b: &str) -> String {
    format!("{a}{b}")
}

/// Direct `Timestamp::now`/`SystemTime::now` calls in `text`, the contents of
/// the TX timing file `rel`, as `file:line` reports, and the number of exempt
/// `.admit_ptt(` stamps (a `tx/ptt.rs` line only).
fn wall_reads(rel: &str, text: &str) -> (Vec<String>, usize) {
    let calls = [needle("Timestamp", "::now"), needle("SystemTime", "::now")];
    let mut reads = Vec::new();
    let mut admission_stamps = 0;
    for (i, line) in text.lines().enumerate() {
        let Some(wall) = calls.iter().find(|w| line.contains(w.as_str())) else {
            continue;
        };
        if rel.ends_with("/ptt.rs") && line.contains(".admit_ptt(") {
            admission_stamps += 1;
        } else {
            reads.push(format!(
                "{rel}:{}: read slot timing through the shared SlotClock, not {wall}",
                i + 1
            ));
        }
    }
    (reads, admission_stamps)
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
        // The DSP cutter's own deadline-band re-anchor.
        needle("reanchor_decode", "_window"),
        // The shared deadline-band classifier the per-site checks used.
        needle("classify_deadline", "_jump"),
        // The TX worker's PAN-114 PTT-hold bound and its slack.
        needle("ptt_hold_exceeds", "_bound"),
        needle("PTT_HOLD_SLACK", "_MS"),
        // The TX worker's PAN-114 late-forward wait rule.
        needle("waited_from_before", "_target"),
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
    // FT8-only slot math in the DSP cutter's file. The other PAN-477 sites
    // are checked by `pan477_sites_read_the_shared_slot_grid`.
    let dsp_rel = "pancetta/src/coordinator/dsp.rs";
    let slot_math = needle("div_euclid(", "15)");
    for (i, line) in read(dsp_rel).lines().enumerate() {
        if line.contains(slot_math.as_str()) {
            found.push(format!("{dsp_rel}:{}: {}", i + 1, line.trim()));
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

    let dsp = read("pancetta/src/coordinator/dsp.rs");
    assert!(
        dsp.contains(&needle("let slot_clock = self.slot", "_clock();")),
        "the DSP cutter must read the coordinator's SlotClock"
    );
    assert!(
        dsp.contains(&needle("decode_window_clock", "_step(&mut window_timer")),
        "the DSP cutter must poll its SlotTimer through decode_window_clock_step"
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

    let tx_worker = read("pancetta/src/coordinator/tx/worker.rs");
    assert!(
        tx_worker.contains(&needle("self.slot", "_clock()")),
        "the TX worker must capture the coordinator's slot clock (PAN-483)"
    );
    for rel in TX_TIMING_FILES {
        let text = read(rel);
        for banned in [needle("Utc", "Clock"), needle("utc", "_clock")] {
            assert!(
                !text.contains(banned.as_str()),
                "{rel} must read slot timing through the shared SlotClock, not {banned}"
            );
        }
    }
    // A direct `Timestamp::now`/`SystemTime::now` call skips the clock's step
    // detection. This covers only these five files and only direct calls,
    // not helpers such as `now_epoch_ms`. The one exempt line is the
    // receive-overload PTT admission stamp in `tx/ptt.rs` (`.admit_ptt(`),
    // which is not slot timing and stays on the wall clock; it must stay
    // exactly one line, so a second or a stale exemption fails.
    let mut admission_stamps = 0;
    for rel in TX_TIMING_FILES {
        let (reads, stamps) = wall_reads(rel, &read(rel));
        assert!(reads.is_empty(), "{}", reads.join("\n"));
        admission_stamps += stamps;
    }
    assert_eq!(
        admission_stamps, 1,
        "tx/ptt.rs must hold exactly one wall-clock `.admit_ptt(` stamp; \
         update this exemption if the admission stamp moved or a second appeared"
    );

    let coordinator = read("pancetta/src/coordinator/mod.rs");
    assert!(
        coordinator.contains(&needle("spawn_slot_clock", "_driver(")),
        "the coordinator must start the slot-clock driver"
    );
}

/// PAN-489: the wall-read ban above covers the DX-parity gate. Each file is
/// in `TX_TIMING_FILES` and clean today, and `wall_reads`, the scan
/// `every_consumer_is_wired_to_the_coordinator_clock` runs over every
/// `TX_TIMING_FILES` entry, reports a direct `SystemTime::now()` or
/// `UtcTimestamp::now()` call appended to it at that call's line.
#[test]
fn wall_read_ban_covers_the_dx_parity_gate() {
    for rel in [
        "pancetta/src/coordinator/tx/gate.rs",
        "pancetta/src/coordinator/tx/bundle.rs",
    ] {
        assert!(
            TX_TIMING_FILES.contains(&rel),
            "{rel} must be in TX_TIMING_FILES"
        );
        let mut text = read(rel);
        assert_eq!(wall_reads(rel, &text), (Vec::new(), 0), "{rel} is clean");
        if !text.ends_with('\n') {
            text.push('\n');
        }
        let probe_line = text.lines().count() + 1;
        for (call, wall) in [
            (
                needle("std::time::SystemTime", "::now()"),
                needle("SystemTime", "::now"),
            ),
            (
                needle("UtcTimestamp", "::now()"),
                needle("Timestamp", "::now"),
            ),
        ] {
            let probed = format!("{text}let _probe = {call};\n");
            assert_eq!(
                wall_reads(rel, &probed),
                (
                    vec![format!(
                        "{rel}:{probe_line}: read slot timing through the shared \
                         SlotClock, not {wall}"
                    )],
                    0
                ),
                "a {call} appended to {rel} must be reported"
            );
        }
    }
}

/// Lines of `rel` containing `banned`, as `file:line: text`.
fn lines_containing(rel: &str, banned: &str, found: &mut Vec<String>) {
    for (i, line) in read(rel).lines().enumerate() {
        if line.contains(banned) {
            found.push(format!("{rel}:{}: {}", i + 1, line.trim()));
        }
    }
}

#[test]
fn pan477_sites_read_the_shared_slot_grid() {
    let root = workspace_root();
    let mut found = Vec::new();

    // Deleted helpers, mirrors and FT8-only constants, anywhere scanned.
    let banned_everywhere = [
        // The decoder's receipt-time parity helper.
        needle("slot_parity_for", "_receipt"),
        // The FT8-only autonomous parity helpers.
        needle("from_unix", "_secs"),
        needle("SlotParity::", "current()"),
        needle("should_transmit_this", "_slot"),
        // The keep-call mirrors of the TX worker's acceptance rule.
        needle("keep_call_acceptance", "_cap_ms"),
        needle("TX_WORKER", "_COALESCE_"),
        // The A7 seed cache's FT8 slot length.
        needle("SLOT_DURATION", "_SECS"),
    ];
    for file in scanned_files() {
        let text = std::fs::read_to_string(&file).unwrap();
        for (i, line) in text.lines().enumerate() {
            for b in &banned_everywhere {
                if line.contains(b.as_str()) {
                    let rel = file.strip_prefix(&root).unwrap();
                    found.push(format!("{}:{}: {}", rel.display(), i + 1, line.trim()));
                }
            }
        }
    }

    // Per-file former code.
    let per_file: [(&str, Vec<String>); 7] = [
        (
            "pancetta/src/coordinator/ft8.rs",
            vec![
                needle("window_received_utc - ", "decode_phase"),
                needle("active_decode", "_phase_ns"),
                needle("SystemTime", "::now()"),
            ],
        ),
        (
            "pancetta/src/coordinator/raw_capture.rs",
            vec![needle("FT8_SLOT", "_NS")],
        ),
        (
            "pancetta/src/coordinator/dsp.rs",
            vec![needle("end: UtcTimestamp", "::now()")],
        ),
        (
            "pancetta-qso/src/cross_sequence.rs",
            vec![needle("System", "Time")],
        ),
        (
            "pancetta/src/coordinator/qso.rs",
            vec![
                needle("unix_secs_of_decode ", "/ 15"),
                needle("floor(unix_seconds ", "/ 15)"),
            ],
        ),
        (
            "pancetta/src/coordinator/replay.rs",
            vec![
                needle("replay_preroll_wait(UtcTimestamp", "::now())"),
                needle("slot::next_slot", "_start("),
            ],
        ),
        (
            "pancetta-tui/src/ui/qso_status.rs",
            vec![needle("% ", "15"), needle("/ ", "15.0")],
        ),
    ];
    for (rel, needles) in &per_file {
        for n in needles {
            lines_containing(rel, n, &mut found);
        }
    }
    for rel in [
        "pancetta/src/coordinator/gain_sweep.rs",
        "pancetta/src/coordinator/gain_sweep_runtime.rs",
    ] {
        lines_containing(rel, &needle("div_euclid(", "15"), &mut found);
    }
    lines_containing(
        "pancetta/src/coordinator/gain_sweep_runtime.rs",
        &needle("Timestamp", "::now()"),
        &mut found,
    );

    assert!(
        found.is_empty(),
        "PAN-477 slot math is back; read the slot the DSP cut, the shared \
         SlotClock or the active period instead:\n{}",
        found.join("\n")
    );

    let dsp = read("pancetta/src/coordinator/dsp.rs");
    assert!(
        dsp.contains(&needle("end: slot_clock", ".now()")),
        "raw capture's batch wall time must come from the shared SlotClock"
    );
    assert!(
        dsp.contains(&needle("decode_window", "_slot(")),
        "the DSP must stamp each decode window with decode_window_slot"
    );
}
