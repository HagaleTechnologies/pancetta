# 2026-06-02 — Eval concurrency guard (`--max-concurrent-tiers`)

**Status:** SHIPPED-INFRA. Local-only harness change, no decoder behaviour
delta, no composite delta. Default behaviour unchanged (opt-in flag).

**Branch:** `iter/2026-06-02-batch-19-concurrency`

## Motivation

Batch 18 (2026-06-02 closeout) recorded a recurring CPU-starvation
pattern: 4+ parallel agents each running their own `eval` invocation
against the heavy real-WAV tiers (curated-hard-200, curated-hard-1000,
chrono-replay) drove load average to **96–135 on a 10-core mac**.
Multiple tier runs timed out or returned non-deterministic composites,
forcing eval re-runs and wasting hours of wall time.

The decoder itself is internally parallelised (rayon across WAVs / per-WAV
sync candidates). Layering N eval processes on top compounds CPU
contention catastrophically. We need a host-level guard that an agent
can opt into without changing decoder behaviour.

## Design

**Cross-process file-lock semaphore.** Each eval invocation, if
launched with `--max-concurrent-tiers N`, acquires one of N slots from
a shared pool directory (default `/tmp/pancetta-eval-tier-slots/`)
before running each heavy tier, and releases on tier completion.
Slots are per-tier — a single `eval --tier hard-200,chrono-replay`
acquires twice, sequentially.

**Mechanism: raw `libc::flock(LOCK_EX|LOCK_NB)`.** Per-slot lockfiles
`slot-0`, `slot-1`, ... live under the pool directory. To acquire, an
eval tries each slot non-blocking; the first one that succeeds becomes
the held slot. If all are held, it logs `WAITING for tier slot (N/N held)`
and polls at 100ms granularity. Drop closes the FD; the kernel releases
the open-file-description lock automatically, so a crashed eval (panic,
SIGKILL, OOM) does NOT leak a slot — the next acquire reclaims it
without any cleanup script.

**Heavy vs light classification** (`tier_slots::is_heavy_tier`):
- **Heavy (gated):** `curated-hard-200`, `curated-hard-1000`,
  `hard-jt9-rich-200`, `chrono-replay`, `wild-50`, `wild-100`,
  `wild-doppler-50`. These run real-WAV corpora through the full
  multi-pass FT8 pipeline (rayon-parallel inside).
- **Light (ungated):** `fixtures`, `synth-clean`, `synth-doppler`,
  `synth-pair-200`. Fast regression / synthetic tiers; no CPU
  contention concern.
- Unknown tier names default to **non-heavy** — explicit allowlist
  semantics, so adding a future tier without updating the classifier
  silently degrades to "no gate" rather than "accidentally serialised".

**Why not `fs2` / `file-lock` / `fd-lock`?** None of those crates are
in the workspace. `libc` is already transitively present (pulled by
cpal, hound, etc.) and the call site is ~3 lines of `unsafe`. Adding
a single-purpose dep for that wasn't worth the supply-chain surface.

**Why not pure thread-level semaphore (e.g. `Arc<Semaphore>`)?**
The pathology is cross-process: independent `cargo run --bin eval`
invocations launched by separate research agents. Thread-level
primitives don't reach across PID boundaries; `flock(2)` on a file
in `/tmp` does.

## Implementation surface

- `pancetta-research/src/tier_slots.rs` — new module. `TierSlotPool`,
  `SlotGuard` (RAII), `is_heavy_tier(&str) -> bool`,
  `DEFAULT_POOL_DIR = "/tmp/pancetta-eval-tier-slots"`. 6 unit tests
  covering: zero-size rejection, single-slot acquire+release,
  two-slot serving two concurrent, blocking acquire unblocks on
  release (cross-thread proxy for cross-process), heavy-tier
  classification, and FD-drop releases the lock.
- `pancetta-research/src/lib.rs` — re-exports.
- `pancetta-research/Cargo.toml` — `libc = "0.2"` direct dep.
- `pancetta-research/src/bin/eval.rs` — adds `--max-concurrent-tiers N`
  and `--max-concurrent-tiers-pool-dir PATH` flags. Builds the pool
  once if N is set; per-tier, acquires a slot iff the tier is heavy.
  RAII releases at end of each loop iteration.
- `pancetta-research/examples/tier_slot_child.rs` — tiny helper that
  acquires a slot, prints `acquired_after_ms=...` on stdout, holds
  for a configurable duration, then exits. Used only by the
  cross-process smoke test.
- `pancetta-research/tests/tier_slot_cross_process.rs` — integration
  test that spawns two `tier_slot_child` processes against a 1-slot
  pool and asserts the second's reported wait time is ≥ 500ms when
  the first holds for 1500ms. Synchronisation: the parent reads
  child A's stdout line-by-line until A reports its acquisition,
  THEN spawns child B. This avoids cargo-startup variance racing
  the test.

## Operator UX

```
# Old (unchanged): no guard. 4 parallel agents each launch their own eval.
cargo run --release -p pancetta-research --bin eval -- \
  --tier curated-hard-200,chrono-replay --mode ft8 --output ...

# New: opt-in. Each agent passes the same flag; only N=2 heavy tiers
# run concurrently across all eval invocations on this host.
cargo run --release -p pancetta-research --bin eval -- \
  --tier curated-hard-200,chrono-replay --mode ft8 --output ... \
  --max-concurrent-tiers 2
```

Stderr emits four lines per heavy tier:
- `tier-slot: pool active (size=2, dir=/tmp/pancetta-eval-tier-slots); heavy tiers will acquire one slot before running` (once at startup if `--max-concurrent-tiers` set)
- `tier-slot: WAITING for tier slot (2/2 held) tier=curated-hard-200 ...` (only when all slots held)
- `tier-slot: ACQUIRED slot N (curated-hard-200) after waiting X.Xs` (or `in <dir>` for the immediate-acquire path)
- `tier-slot: RELEASED slot N (curated-hard-200) in <dir>`

Operator can `ls /tmp/pancetta-eval-tier-slots/` to see slot files; `fuser`
or `lsof` will show which PID holds which slot. There's nothing to clean
up between batches — lockfiles are cheap to leave on disk.

**Logging note:** I used `eprintln!` rather than `info!` because the
eval binary doesn't initialise a `log` / `tracing` subscriber and the
existing per-tier observability in `eval.rs` is all `eprintln!`. This
keeps the slot messages visible by default without a flag dance.

## Smoke-test result

```
running 1 test
test two_processes_serialize_on_single_slot_pool ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.95s
```

Two child processes, 1-slot pool. Child A spawned, parent waited for A
to report acquisition (synchronous handshake via stdout), then spawned
child B. Child B's `pool.acquire()` blocked until A's 1500ms hold
elapsed; B's measured `acquired_after_ms` was well above the 500ms
threshold the test asserts. Stderr from B included both `WAITING` and
`ACQUIRED` markers, confirming the operator-visible signalling path.

Unit-test suite:
```
test result: ok. 57 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
Includes 6 new `tier_slots::tests::*` cases.

Clippy: no new warnings introduced (pre-existing pancetta-ft8 and
pancetta-research warnings unchanged; 114 vs 115 baseline on main,
i.e. one fewer because cargo fmt normalised an unrelated line).

## What this does NOT do

- **No throttling within a single eval invocation.** If you launch one
  eval with `--max-concurrent-tiers 1`, tiers within it already run
  sequentially (the dispatch loop is sequential), so the guard is
  effectively a no-op. The mechanism only matters across invocations.
- **No CPU pinning, no rayon thread-pool capping.** A single eval's
  rayon pool is still sized to `available_parallelism`. If a single
  eval saturates the box, this guard won't help. The hypothesis from
  Batch 18 is that no single eval saturates — it's the **layering** of
  N evals on top of one another that kills throughput. (The 4-evals-on-10-cores
  observation matches that: each rayon pool nominally caps at 10
  workers, but with 4 evals running you have ~40 nominal workers
  contending for 10 cores.)
- **No automatic detection of CPU load.** The flag is a hard cap on
  concurrent heavy tier runs, not an adaptive throttle. If the
  operator wants finer control they can wrap eval in a queueing
  script; this guard is the minimum-viable knob.

## Follow-ups

- Once we have wall-time data from a real multi-agent batch with
  `--max-concurrent-tiers 1` (or 2), revisit whether to make it
  default-on in `scripts/research-env.sh` or in subagent prompts.
- Consider extending the guard to other heavy binaries (`baseline`,
  `curate`) if those start to feature in parallel dispatch.
- A `tier-slot list` subcommand showing held slots + waiters could be
  nice ergonomics, but defer until an operator actually wants it.

## Files touched

- `pancetta-research/src/tier_slots.rs` (new)
- `pancetta-research/src/lib.rs` (re-export)
- `pancetta-research/Cargo.toml` (`libc` dep)
- `pancetta-research/src/bin/eval.rs` (flags + dispatch wrap)
- `pancetta-research/examples/tier_slot_child.rs` (new — test helper)
- `pancetta-research/tests/tier_slot_cross_process.rs` (new — smoke)
- `research/experiments/2026-06-02-eval-concurrency-guard.md` (this file)
