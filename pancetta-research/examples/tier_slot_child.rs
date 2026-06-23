//! Smoke-test helper: acquires a slot from a tier-slot pool and sleeps
//! for the requested duration. Used by `tests/tier_slot_cross_process.rs`
//! to exercise cross-process semantics of the file-lock pool.
//!
//! Usage:
//!   tier_slot_child <pool_dir> <pool_size> <hold_ms> <label>
//!
//! Exit code: 0 on successful acquire+release, 1 on usage error,
//! 2 on pool/IO error.

use pancetta_research::TierSlotPool;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: tier_slot_child <pool_dir> <pool_size> <hold_ms> <label>");
        std::process::exit(1);
    }
    let pool_dir = PathBuf::from(&args[1]);
    let pool_size: usize = args[2]
        .parse()
        .expect("pool_size must be a positive integer");
    let hold_ms: u64 = args[3]
        .parse()
        .expect("hold_ms must be a non-negative integer");
    let label = &args[4];

    let pool = match TierSlotPool::new(&pool_dir, pool_size) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("pool init failed: {e}");
            std::process::exit(2);
        }
    };
    let acquire_start = Instant::now();
    let _guard = match pool.acquire(label) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("acquire failed: {e}");
            std::process::exit(2);
        }
    };
    let acquired_at = acquire_start.elapsed();
    // Emit a structured line that the parent can parse to confirm
    // ordering. Stdout (not stderr) so the test harness can pipe it.
    println!(
        "tier_slot_child: label={} acquired_after_ms={} hold_ms={}",
        label,
        acquired_at.as_millis(),
        hold_ms,
    );
    std::thread::sleep(Duration::from_millis(hold_ms));
    // Guard drops here → flock released.
}
