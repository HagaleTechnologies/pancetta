# Pancetta Project Review — 2026-03-31

Comprehensive audit covering architecture, security, code quality, and next steps.

---

## Executive Summary

**pancetta-ft8 is production-grade.** 200 tests, bit-exact with WSJT-X, cross-validated
with ft8_lib FFI, property-tested. The FT8 engine is the crown jewel.

**The application shell is ~60-70% complete.** Good architecture (10-crate workspace,
message bus, coordinator pattern), but 6 coordinator component integrations are stubs.
The TUI renders panels but isn't wired to the live pipeline. QSO management has an
impressive schema but incomplete async paths.

**Overall: a verified FT8 engine inside a well-architected but half-integrated app shell.**

---

## Project Statistics

| Metric | Value |
|--------|-------|
| Workspace Members | 10 crates |
| Total Source Lines | ~76,672 LOC |
| Largest File | coordinator.rs (2,749 lines) |
| Files > 500 Lines | 53 |
| Test Functions | ~563 |
| FT8 Tests (passing) | 200 |
| Unsafe Blocks | 48 (all in FFI boundaries) |
| Unsafe Trait Impls | 36 |
| Production unwrap() | ~200 |
| Workspace Dependencies | 44 |
| Feature Flags | 14 across workspace |
| CI Jobs | 4 (tests, check, clippy, fmt) |

---

## Security Review

### HIGH Priority

1. **Plaintext credentials** — `pancetta-dx/src/lotw.rs` stores LoTW username/password
   as bare `String` fields. No encryption, no zeroization.
   - **Fix**: Use `zeroize` crate, consider system keyring (`keyring` crate),
     document file permission requirements (chmod 600).

2. **`overflow-checks = false` in release profile** (`Cargo.toml:119`) — Disables
   integer overflow detection in release builds. Silent wraparound in all arithmetic.
   - **Fix**: Document this decision. Consider per-crate profile overrides so
     only DSP hot paths skip checks.

3. **~200 `unwrap()` calls in production code** — Concentrated in TUI, QSO, DX modules.
   Each is a potential crash path.
   - **Fix**: Systematic audit. Prioritize TUI (user-facing) and QSO (data loss risk).

### MEDIUM Priority

4. **No file size limits on input** — `fs::read_to_string()` on config files, ADIF
   imports, WAV files without size caps. A 10GB ADIF file would exhaust memory.
   - **Fix**: Add size checks before reading external files.

5. **Lock poisoning** — Several `.unwrap()` on `Mutex::lock()` / `RwLock::read()`.
   If a thread panics while holding a lock, subsequent attempts panic too.
   - **Fix**: Switch fully to `parking_lot` (already a dependency) which doesn't poison.

6. **CI supply chain** — Actions pinned to major versions (`@v6`, `@v2`) not SHA hashes.
   - **Fix**: Pin to SHA. Add `cargo-audit` as a CI job.

### LOW Priority

7. **FFI safety gaps** — `ft8_lib_ffi.rs` assumes null-terminated buffers without
   defensive checks. Hamlib `unsafe impl Send/Sync` relies on Mutex discipline
   not enforced by types.

8. **`shellexpand::full()` on config content** — Evaluates environment variables in
   config values. Probably intentional but undocumented.

---

## Code Quality Assessment

### Strengths

- **Architecture**: Clean crate boundaries, message bus, coordinator, hot-reload config
- **FT8 DSP**: Exemplary testing, cross-validation, honest documentation (ANALYSIS.md)
- **Rust idioms**: Proper error types, workspace dependency sharing, feature gating
- **CI**: 4 jobs covering tests, clippy, formatting
- **Unsafe discipline**: All unsafe code compartmentalized to FFI boundaries
- **Concurrency**: Lock-free channels (crossbeam), proper atomics, no deadlock patterns

### Concerns

1. **53 files exceed 500 lines** — coordinator.rs (2,749), statistics.rs (2,743),
   decoder.rs (2,041). These need decomposition.

2. **Dual database abstraction** — QSO uses both `rusqlite` (sync) and `sqlx` (async).
   Should consolidate to sqlx since the app is async-first.

3. **Dead code in decoder** — `is_synchronized()` always returns `true`.
   `_num_candidates` / `_best_score` are debug leftovers.

4. **Waterfall `log10(0)` bug** — decoder.rs ~line 1132: `10.0 * power.log10()` produces
   `-inf` when power is zero. Should be `10.0 * (power + 1e-12).log10()`.

5. **`bits_to_u16` boundary issue** — decoder.rs ~line 1197: `bits.len() - 1 - i` can
   produce wrong results if `bits.len() > 16`.

6. **No TUI tests** — Terminal UI has zero test coverage.

7. **Stubbed components in coordinator** — 6 components are stubs:
   - `start_hamlib_component()`
   - `start_qso_component()`
   - `start_transmitter_component()`
   - `start_autonomous_component()`
   - `start_dx_cluster_component()`
   - `start_pskreporter_component()`

8. **Stubbed CLI commands** — `test-audio` and `benchmark` subcommands exit with
   "not yet implemented".

---

## What to Add, Modify, or Remove

### Add

- `cargo-audit` in CI for automated vulnerability scanning
- Fuzz testing (`cargo-fuzz`) for the decoder
- Integration tests for the coordinator (currently disabled)
- `zeroize` for credential handling
- File size limits before `read_to_string` of external files

### Modify

- Release profile: document `overflow-checks = false`; consider per-crate overrides
- Replace `unwrap()` in production paths (prioritize TUI, QSO)
- Decompose large files: coordinator.rs, statistics.rs, autonomous.rs
- Consolidate database layer to sqlx
- Pin CI actions to SHA hashes

### Remove

- `is_synchronized()` dead code in decoder
- Debug underscore-prefixed variables in decoder
- Redundant `DecodedMessageView` in TUI (duplicates pancetta-ft8 type)

---

## Recommended Next Steps

### Immediate

1. Commit pending CI path-filter changes (good optimization, sitting uncommitted)

### Short Term (next sprint)

2. Harden error handling — systematic `unwrap()` audit, starting with TUI
3. Wire TUI to live pipeline (audio -> DSP -> FT8 -> display)
4. Implement `test-audio` subcommand (needed for hardware setup)
5. Add `cargo-audit` to CI

### Medium Term (next month)

6. Implement hamlib + transmit coordinator components (gateway to radio operation)
7. Decompose coordinator.rs into per-component modules
8. Credential management (zeroize + keyring for LoTW)
9. Off-air decode rate improvement (only 3/9 WAV files decode)

### Longer Term

10. Performance benchmarking against real-time (can decoder keep up with 15s windows?)
11. i3=4 nonstandard callsign support (needed for real-world operation)
12. GFSK modulation (better spectral efficiency for transmit)

---

## Component Readiness Matrix

| Component | Architecture | Implementation | Tests | Production-Ready |
|-----------|-------------|---------------|-------|-----------------|
| pancetta-ft8 (encoder) | Excellent | Complete | 200 pass | Yes |
| pancetta-ft8 (decoder) | Excellent | Complete | 200 pass | Yes (speed TBD) |
| pancetta-ft8 (modulator) | Good | Complete | Included above | Yes |
| pancetta-ft8 (OSD) | Excellent | Complete | Included above | Yes |
| pancetta-config | Good | Complete | 49 pass | Yes |
| pancetta-core | Good | Complete | N/A | Yes |
| pancetta-audio | Good | Functional | Minimal | Needs testing |
| pancetta-dsp | Good | Functional | Minimal | Needs testing |
| pancetta-tui | Good | Scaffold | None | No |
| pancetta-qso | Good | Partial | Some | No |
| pancetta-dx | Good | Partial | Some | No |
| pancetta-hamlib | Good | FFI done | Some | Needs integration |
| pancetta (coordinator) | Good | Partial (6 stubs) | Disabled | No |
| Message bus | Good | Complete | 7 pass | Yes |
| Runtime | Good | Complete | 6 pass | Yes |

---

_Review conducted 2026-03-31. Next review recommended after completing short-term items._
