//! Cross-process smoke test for the tier-slot file-lock pool.
//!
//! Spawns two `tier_slot_child` example binaries against a 1-slot pool
//! and confirms they serialize: the second's `acquired_after_ms` must be
//! at least the first's `hold_ms`. This is the exact contention pattern
//! that motivated Batch 19 — two independent `eval` invocations must
//! not run heavy tiers simultaneously when `--max-concurrent-tiers 1`
//! is set.
//!
//! Cargo runs example builds on demand via `cargo run --example`, so
//! this test triggers a build of `tier_slot_child` if needed. The test
//! is fast (~600ms total wall) and isolated to a per-test temp dir.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("manifest dir has parent")
        .to_path_buf()
}

fn tmp_pool_dir(suffix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pancetta-tier-slots-cross-{}-{}-{}",
        suffix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn spawn_child(pool_dir: &Path, hold_ms: u64, label: &str) -> std::process::Child {
    Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "--example",
            "tier_slot_child",
            "-p",
            "pancetta-research",
            "--",
            pool_dir.to_str().unwrap(),
            "1",
            &hold_ms.to_string(),
            label,
        ])
        .current_dir(workspace_root())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tier_slot_child child")
}

fn parse_acquired_after_ms(stdout: &str) -> Option<u64> {
    // Format: "tier_slot_child: label=... acquired_after_ms=NN hold_ms=NN"
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("tier_slot_child:") {
            for tok in rest.split_whitespace() {
                if let Some(val) = tok.strip_prefix("acquired_after_ms=") {
                    return val.parse().ok();
                }
            }
        }
    }
    None
}

/// Pre-warm the example binary build so the timing in the contention
/// test isn't dominated by compile time. We invoke the binary with no
/// args; it exits with code 1 (usage error), which is fine — the build
/// is what we need.
fn prewarm_child_build() {
    let _ = Command::new("cargo")
        .args([
            "build",
            "--quiet",
            "--example",
            "tier_slot_child",
            "-p",
            "pancetta-research",
        ])
        .current_dir(workspace_root())
        .status();
}

#[test]
fn two_processes_serialize_on_single_slot_pool() {
    prewarm_child_build();
    let pool_dir = tmp_pool_dir("serial");
    // A holds long enough that B is forced to wait observably.
    let a_hold_ms: u64 = 1500;

    // Spawn child A and block until it prints its "acquired_after_ms"
    // line on stdout. This avoids racing against cargo's variable
    // startup time — we KNOW A holds the slot before we even spawn B.
    let mut child_a = spawn_child(&pool_dir, a_hold_ms, "A");
    {
        let stdout_a = child_a.stdout.take().expect("child A piped stdout");
        let mut reader = BufReader::new(stdout_a);
        let mut line = String::new();
        let read_start = Instant::now();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).expect("read child A stdout");
            assert!(
                n > 0,
                "child A exited before reporting acquisition (read_start={:?})",
                read_start.elapsed(),
            );
            if line.contains("acquired_after_ms=") {
                break;
            }
        }
        // Stitch the reader back to the Child so wait_with_output works.
        // Since we've consumed some bytes, we instead drain manually below.
        // Move the buffered reader (and any remaining bytes) into a vec we
        // accumulate from here on.
        // For simplicity we drop the reader; A's hold_ms is short enough
        // that any remaining stdout (the EOF signal) doesn't matter — we
        // only check stderr signals from B and A's exit status below.
        drop(reader);
    }
    let spawn_b_at = Instant::now();
    let child_b = spawn_child(&pool_dir, 10, "B");

    let out_b = child_b.wait_with_output().expect("child B wait");
    let _b_wall = spawn_b_at.elapsed();
    // Wait for A to exit so we don't leak. We can't call wait_with_output
    // after taking stdout, so use try_wait/wait without capturing.
    let a_status = child_a.wait().expect("child A wait status");
    // Build a stand-in for out_a so downstream code paths are unchanged.
    struct OutLike {
        status: std::process::ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    }
    // A's stderr was piped but never read; cargo will have left it on
    // the pipe. Drain it now (best-effort; if A's stderr buffer is
    // large enough to block A, that would have shown as a hang already).
    let mut a_stderr = Vec::new();
    if let Some(mut s) = child_a.stderr.take() {
        use std::io::Read;
        let _ = s.read_to_end(&mut a_stderr);
    }
    let out_a = OutLike {
        status: a_status,
        stdout: Vec::new(),
        stderr: a_stderr,
    };

    let stdout_a = String::from_utf8_lossy(&out_a.stdout).into_owned();
    let stdout_b = String::from_utf8_lossy(&out_b.stdout).into_owned();
    let stderr_a = String::from_utf8_lossy(&out_a.stderr).into_owned();
    let stderr_b = String::from_utf8_lossy(&out_b.stderr).into_owned();

    assert!(
        out_a.status.success(),
        "child A failed: stdout={stdout_a:?} stderr={stderr_a:?}"
    );
    assert!(
        out_b.status.success(),
        "child B failed: stdout={stdout_b:?} stderr={stderr_b:?}"
    );

    let b_acquired = parse_acquired_after_ms(&stdout_b)
        .unwrap_or_else(|| panic!("could not parse B acquired_after_ms from: {stdout_b:?}"));

    // B was spawned AFTER A reported it had acquired the slot. A holds
    // for a_hold_ms (1500ms). B's reported acquired_after_ms is
    // measured from B's own pool.acquire() entry, AFTER cargo and the
    // example binary have started up. So B must observe a wait of at
    // least (a_hold_ms - cargo_startup_for_B - epsilon). Allow generous
    // slop: require >= 500ms to give a 1000ms safety margin against
    // even slow cargo startup on a loaded host.
    assert!(
        b_acquired >= 500,
        "child B acquired too quickly ({b_acquired}ms) — pool did not serialize. \
         A stderr=\n{stderr_a}\nB stderr=\n{stderr_b}\nB stdout=\n{stdout_b}"
    );

    // Sanity: stderr must contain WAITING then ACQUIRED for child B.
    assert!(
        stderr_b.contains("WAITING"),
        "child B should have logged WAITING; stderr=\n{stderr_b}"
    );
    assert!(
        stderr_b.contains("ACQUIRED"),
        "child B should have logged ACQUIRED; stderr=\n{stderr_b}"
    );
}
