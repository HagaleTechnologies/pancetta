# pancetta-dx Cleanup + cqdx Hybrid Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Resolve the `pancetta-dx` ↔ `pancetta-cqdx` overlap by deleting the modules in `pancetta-dx` that are genuinely dead, moving the workspace's two true utility modules (`gridsquare`, eventually `geography` if we end up wanting it) into `pancetta-core`, and pruning the per-integration credential config types in `pancetta-config` whose clients never got wired up. Establishes the foundation for follow-on per-integration specs (LoTW upload, QRZ paid lookup, eQSL upload, Clublog upload) that will keep their credentials local to the pancetta process — cqdx.io stays a non-credentialed read-only pull cache.

**Architecture:** Hybrid model. `pancetta-cqdx` (the cqdx.io HTTP client + cache) handles non-credentialed *public* lookups: DXCC entity prefix matching, callsign rarity, live spots, needed-DXCC sets, and any future propagation models. `pancetta-dx` keeps only the modules that are either credentialed (`lotw.rs`, kept as scaffolding) or non-cqdx network integrations (`cluster.rs` for traditional DX cluster telnet, `pskreporter.rs` for spot upload). After this refactor, `pancetta-dx` will be ~3.5k LOC instead of ~9k LOC. No behavioural changes — every module deleted has zero external callers per the audit. Every change is verified by the loopback_qso suite and the workspace test run.

**Tech Stack:** Rust 2021 workspace, cargo, the existing `pancetta-cqdx` HTTP/cache layer, `pancetta-core` shared-types crate.

---

## File structure

After this plan executes:

```
pancetta-core/
  src/
    gridsquare.rs           CREATED — moved from pancetta-dx
    lib.rs                  MODIFIED — re-export Maidenhead helpers

pancetta-dx/
  src/
    cluster.rs              KEPT — DX cluster telnet client, called from coordinator/dx_cluster.rs
    dxcc.rs                 DELETED — replaced by pancetta-cqdx CqdxCache::resolve_entity / rarity
    geography.rs            DELETED — never called externally; geographiclib-rs covers callers directly
    gridsquare.rs           DELETED — moved to pancetta-core
    lotw.rs                 KEPT — scaffolding for credentialed LoTW upload (follow-on spec)
    priorities.rs           DELETED — never called; priority scoring lives in pancetta-qso/priority.rs
    propagation_enhanced.rs DELETED — never integrated; propagation will live server-side in cqdx.io
    pskreporter.rs          KEPT — called from coordinator/psk_reporter.rs
    statistics.rs           DELETED — instantiated but never accessed; cqdx-side stats now
    tracker.rs              DELETED — designed but never integrated; QSO logging lives in pancetta-qso
    lib.rs                  MODIFIED — drop dead pub mod / pub use entries

pancetta-config/
  src/
    network.rs              MODIFIED — delete QrzConfig, LotwConfig, EqslConfig, ClublogConfig
                                       (zero callers in workspace per audit)

pancetta/
  src/
    coordinator/
      tui_relay.rs          MODIFIED — switch `pancetta_dx::gridsquare::*` → `pancetta_core::gridsquare::*`
```

Git worktree: this plan can run in `main` directly because every change is mechanical and the loopback_qso suite is the regression gate. No worktree required.

---

## Task 1: Migrate `gridsquare` from `pancetta-dx` to `pancetta-core`

**Files:**
- Create: `pancetta-core/src/gridsquare.rs`
- Modify: `pancetta-core/src/lib.rs`
- Modify: `pancetta/src/coordinator/tui_relay.rs`
- Delete (in Task 4): `pancetta-dx/src/gridsquare.rs`

`gridsquare.rs` is pure Maidenhead↔coordinate math with no `pancetta-dx` internal dependencies. Two callers exist (`coordinator/tui_relay.rs:77,104`) and both will switch import path in this task. We move first, switch caller imports, verify, then delete the original file in Task 4 once nothing references it.

- [ ] **Step 1: Add `gridsquare` module to `pancetta-core`.**

```bash
cp pancetta-dx/src/gridsquare.rs pancetta-core/src/gridsquare.rs
```

- [ ] **Step 2: Verify the copy has no `pancetta-dx`-internal dependencies.**

```bash
grep -E '^use crate::' pancetta-core/src/gridsquare.rs
```

Expected output: empty, OR only `use crate::error::*;` style imports that resolve through `pancetta-core` too. If you see anything else, STOP — the audit was wrong and this module isn't actually self-contained; document the surprise and discuss with the human reviewer before proceeding.

- [ ] **Step 3: Re-export from `pancetta-core/src/lib.rs`.**

Find the existing `pub mod` declarations near the top of `pancetta-core/src/lib.rs`. Add `pub mod gridsquare;` alphabetically.

```rust
// pancetta-core/src/lib.rs (around line 25, sorted)
pub mod error;
pub mod gridsquare;     // NEW
pub mod slot;
pub mod types;
```

- [ ] **Step 4: Verify `pancetta-core` compiles in isolation.**

```bash
cargo check -p pancetta-core
```

Expected: `Finished dev [unoptimized] target(s)` with no warnings about `pancetta_core::gridsquare`.

- [ ] **Step 5: Update the two callers in `coordinator/tui_relay.rs`.**

Find both occurrences and change the import path:

```rust
// Before:
pancetta_dx::gridsquare::grid_to_coordinates(&config.station.grid_square).ok()
// After:
pancetta_core::gridsquare::grid_to_coordinates(&config.station.grid_square).ok()

// Before:
match pancetta_dx::gridsquare::grid_to_coordinates(remote_grid) {
// After:
match pancetta_core::gridsquare::grid_to_coordinates(remote_grid) {
```

- [ ] **Step 6: Verify the workspace still compiles.**

```bash
cargo check --workspace
```

Expected: clean. (At this stage `pancetta-dx::gridsquare` still exists too — that's intentional; it's removed in Task 4.)

- [ ] **Step 7: Run the gridsquare test from its new home.**

```bash
cargo test -p pancetta-core gridsquare
```

Expected: same number of `test result: ok.` reports as ran before in `pancetta-dx`. (`gridsquare.rs` historically had ~7 unit tests at the bottom of the file under `#[cfg(test)] mod tests`.)

- [ ] **Step 8: Run the loopback_qso regression check.**

```bash
cargo test -p pancetta --test loopback_qso
```

Expected: `test result: ok. 11 passed`.

- [ ] **Step 9: Commit.**

```bash
git add pancetta-core/src/gridsquare.rs pancetta-core/src/lib.rs pancetta/src/coordinator/tui_relay.rs
git commit -m "refactor: move gridsquare from pancetta-dx to pancetta-core

Maidenhead grid <-> lat/lon math is pure utility code with no DX-specific
domain logic. Move it to pancetta-core where every other crate can reach
it without taking on the rest of pancetta-dx as a dependency. The two
existing callers (coordinator/tui_relay.rs) update their import path;
behaviour is identical.

The duplicate copy in pancetta-dx/src/gridsquare.rs is left in place by
this commit and removed in the upcoming pancetta-dx prune."
```

---

## Task 2: Delete dead modules from `pancetta-dx`

**Files:**
- Delete: `pancetta-dx/src/dxcc.rs`
- Delete: `pancetta-dx/src/geography.rs`
- Delete: `pancetta-dx/src/priorities.rs`
- Delete: `pancetta-dx/src/propagation_enhanced.rs`
- Delete: `pancetta-dx/src/statistics.rs`
- Delete: `pancetta-dx/src/tracker.rs`
- Modify: `pancetta-dx/src/lib.rs`
- Modify: `pancetta-dx/Cargo.toml` (remove deps that only those dead modules used)

The audit confirmed zero external callers for each of these six modules. Each was instantiated only inside `pancetta-dx` itself (e.g. `DxHunter::new` constructs a `StatisticsEngine`) but the constructed instances were never *queried*. We're deleting the construction sites in `lib.rs` along with the modules.

- [ ] **Step 1: Delete the six dead module files.**

```bash
git rm pancetta-dx/src/dxcc.rs \
       pancetta-dx/src/geography.rs \
       pancetta-dx/src/priorities.rs \
       pancetta-dx/src/propagation_enhanced.rs \
       pancetta-dx/src/statistics.rs \
       pancetta-dx/src/tracker.rs
```

- [ ] **Step 2: Open `pancetta-dx/src/lib.rs` and find the module declarations.**

```bash
grep -n '^pub mod\|^mod\|^pub use' pancetta-dx/src/lib.rs
```

Note the line numbers of `dxcc`, `geography`, `priorities`, `propagation_enhanced`, `statistics`, `tracker` declarations and any matching `pub use` re-exports.

- [ ] **Step 3: Remove the dead `pub mod` declarations from `lib.rs`.**

For each of the six modules, remove the `pub mod NAME;` line. A typical state after this step (illustrative — actual line order may differ):

```rust
// pancetta-dx/src/lib.rs (after edit, modules section)
pub mod cluster;
pub mod gridsquare;       // (still present in this task; deleted in Task 4)
pub mod lotw;
pub mod pskreporter;
```

- [ ] **Step 4: Remove dead `pub use` re-exports from `lib.rs`.**

Anything that re-exported a type from one of the deleted modules (e.g. `pub use dxcc::{DxccDatabase, DxccEntity};`, `pub use statistics::StatisticsEngine;`) gets deleted too. Use the grep output from Step 2 as the authoritative list.

- [ ] **Step 5: Remove the `DxHunter` struct's dead-module construction sites (if any).**

```bash
grep -n 'StatisticsEngine\|DxccDatabase\|DxTracker\|PropagationEnhanced\|PriorityManager' pancetta-dx/src/lib.rs
```

For each match: delete the field declaration in the struct and the corresponding `field: …::new()` line in `DxHunter::new`. If `DxHunter` itself ends up empty (no remaining fields), delete the struct entirely; nothing external constructs it (audit confirmed).

- [ ] **Step 6: Run `cargo check -p pancetta-dx` and react to errors.**

```bash
cargo check -p pancetta-dx
```

Expected error categories:
- "unresolved import `crate::dxcc`" → there are still references inside other `pancetta-dx` modules. Check `cluster.rs`, `lotw.rs`, `pskreporter.rs` for those imports and remove them. (These modules are alive but may have referenced dead utility types.)
- "use of undefined type `StatisticsEngine`" etc. → same fix.
- Unused-import warnings on `tracing::*` etc. → fine, suppress in next step.

- [ ] **Step 7: Trim now-unused dependencies in `pancetta-dx/Cargo.toml`.**

After deleting the modules, run:

```bash
cargo machete -p pancetta-dx 2>/dev/null || cargo --list | grep machete
```

If `cargo-machete` isn't installed, use `cargo build -p pancetta-dx 2>&1 | grep 'unused dependency'` as a less-thorough fallback. Remove any dependency that's no longer pulled in by `cluster.rs`, `lotw.rs`, `pskreporter.rs`, or `gridsquare.rs`.

Common candidates that *may* drop out: `geographiclib-rs` (geography.rs), `nalgebra` (statistics.rs), `rusqlite` (tracker.rs). Verify each before removing.

- [ ] **Step 8: Verify the workspace compiles.**

```bash
cargo check --workspace
```

Expected: clean.

- [ ] **Step 9: Run the loopback_qso regression check.**

```bash
cargo test -p pancetta --test loopback_qso
```

Expected: `test result: ok. 11 passed`.

- [ ] **Step 10: Run the full workspace tests.**

```bash
cargo test --workspace --features transmit
```

Expected: every prior test still passes (the deleted modules had their own internal tests, but those went with the modules).

- [ ] **Step 11: Commit.**

```bash
git add -A
git commit -m "refactor(pancetta-dx): delete dead modules superseded by cqdx + qso

Six modules in pancetta-dx had zero external callers per the workspace
audit and were duplicating capability that lives elsewhere:

- dxcc.rs              → pancetta-cqdx CqdxCache::resolve_entity covers
                         DXCC prefix matching against the live cqdx.io
                         DXCC database.
- geography.rs         → never called outside pancetta-dx itself; the
                         only consumer (coordinator/tui_relay.rs) goes
                         direct to geographiclib-rs.
- priorities.rs        → priority scoring lives in pancetta-qso/priority.rs
                         and was always the canonical implementation.
- propagation_enhanced → never integrated into the spot pipeline; future
                         propagation modelling will be a server-side
                         cqdx.io feature.
- statistics.rs        → instantiated by DxHunter but never queried;
                         per-band worked counts now flow from CqdxCache.
- tracker.rs           → SQLite QSO store designed but never wired;
                         actual QSO logging lives in pancetta-qso/logger.

DxHunter is gone too — the only external reference was the implicit
construction inside pancetta-dx::lib.

Behaviour unchanged. Loopback QSO regression suite (11 tests) green.

Net: -~5800 LOC."
```

---

## Task 3: Trim dead credential config types from `pancetta-config`

**Files:**
- Modify: `pancetta-config/src/network.rs`
- Modify: `pancetta-config/defaults.toml` (drop the `[network.qrz]` etc. blocks)
- Modify: `pancetta-config/src/lib.rs` (drop `pub use` if any)

The audit confirmed that `config.network.qrz`, `config.network.lotw`, `config.network.eqsl`, and `config.network.clublog` are never accessed anywhere in the workspace. The structs exist purely as schema noise. The clients in `pancetta-dx/src/lotw.rs` (kept as scaffolding) define their own `LotwConfig` struct internally and don't read `pancetta_config::network::lotw`. Future credentialed-integration specs will reintroduce the right config under each integration's own roof.

`PskReporterConfig`, `DxClusterConfig`, and `CqdxConfig` are LIVE — they stay.

- [ ] **Step 1: Identify the dead config types.**

```bash
grep -n 'pub struct \(Qrz\|Lotw\|Eqsl\|Clublog\).*Config' pancetta-config/src/network.rs
```

Expected: matches for the four dead config families plus their nested helper types (e.g. `LotwCertificateConfig`, `QrzSessionConfig`, `LotwUploadConfig`, etc.).

- [ ] **Step 2: Confirm zero external callers.**

```bash
grep -rn 'config\.network\.\(qrz\|lotw\|eqsl\|clublog\)\|::network::\(Qrz\|Lotw\|Eqsl\|Clublog\)' \
  --include='*.rs' .
```

Expected output: matches only inside `pancetta-config/` itself. If anything outside that crate appears, STOP — the audit missed a caller; document and discuss before deleting.

- [ ] **Step 3: Delete the four config families and their nested helpers from `network.rs`.**

This is a wholesale removal. Each `pub struct QrzFooConfig`, `pub struct LotwBarConfig`, etc., goes — along with their `Default` impls, the field that holds them on the parent `NetworkConfig`, and any associated comments.

Approach: open `pancetta-config/src/network.rs` and search for the headline structs. Track outward to:
1. The `pub qrz: QrzConfig,` field on `NetworkConfig` — delete it.
2. The default value in `impl Default for NetworkConfig` — delete the `qrz: QrzConfig::default(),` line.
3. The struct definitions — delete `QrzConfig`, `QrzSessionConfig`, `QrzLookupConfig`, `QrzLogbookConfig`, etc.

Repeat for `lotw`, `eqsl`, `clublog`. Be systematic — skipping a child struct will produce "no-such-type" errors at the next compile.

- [ ] **Step 4: Update `pancetta-config/defaults.toml`.**

Find and delete the corresponding TOML sections:

```toml
# DELETE these blocks entirely:
[network.qrz]
...
[network.lotw]
...
[network.eqsl]
...
[network.clublog]
...
```

- [ ] **Step 5: Compile and react.**

```bash
cargo check -p pancetta-config
```

Expected error categories (resolve each before moving on):
- Missing field `qrz` / `lotw` / etc. on `NetworkConfig` initializer → delete those lines too.
- Tests in `network.rs::tests` that reference deleted types → delete those test fns; they were testing dead code.
- `serde` deserialize errors only at runtime; build-time check should pass.

- [ ] **Step 6: Run the config tests.**

```bash
cargo test -p pancetta-config
```

Expected: clean. Some tests will have been deleted in Step 5; the surviving ones should still pass.

- [ ] **Step 7: Run the loopback_qso regression check.**

```bash
cargo test -p pancetta --test loopback_qso
```

Expected: `test result: ok. 11 passed`.

- [ ] **Step 8: Commit.**

```bash
git add -A
git commit -m "refactor(pancetta-config): drop unused QRZ/LoTW/eQSL/Clublog config types

Each of these four config families had zero callers in the workspace
per the audit — the structs existed only as serde schema noise that
made the config TOML look like more of pancetta was wired up than
actually was. Drop them. PskReporterConfig, DxClusterConfig, and
CqdxConfig stay (live).

When a follow-on spec adds real LoTW upload, QRZ paid lookup, eQSL
upload, or Clublog upload, the integration will define its own config
struct under its own crate's roof — pancetta-config's job is the
shared backbone, not a museum of every API somebody mentioned.

The defaults.toml blocks for [network.qrz], [network.lotw],
[network.eqsl], and [network.clublog] go too.

Net: ~-1400 LOC of dead schema."
```

---

## Task 4: Remove the duplicate `gridsquare.rs` from `pancetta-dx`

**Files:**
- Delete: `pancetta-dx/src/gridsquare.rs`
- Modify: `pancetta-dx/src/lib.rs`
- Modify: `pancetta-dx/Cargo.toml` (if `pancetta-core` wasn't a dep yet)

Task 1 added the canonical copy to `pancetta-core` and switched the only external caller. The copy in `pancetta-dx/src/gridsquare.rs` is now dead weight. We left it in place during Task 1 specifically so that the migration commit stays a clean two-line behaviour change rather than a delete + add + caller switch + re-export rebuild all at once. Now we finish the move.

If `cluster.rs`, `lotw.rs`, or `pskreporter.rs` use `crate::gridsquare::*` internally, they switch to `pancetta_core::gridsquare::*` here too.

- [ ] **Step 1: Check if any module inside `pancetta-dx` still uses the local `gridsquare`.**

```bash
grep -n 'crate::gridsquare\|use crate::gridsquare' pancetta-dx/src/*.rs
```

Expected: probably empty (the audit said external callers were the only consumers), but verify.

- [ ] **Step 2: If Step 1 found internal callers, switch them to `pancetta_core::gridsquare`.**

Example transformation:

```rust
// Before:
use crate::gridsquare::grid_to_coordinates;
// After:
use pancetta_core::gridsquare::grid_to_coordinates;
```

If `pancetta-dx/Cargo.toml` doesn't already depend on `pancetta-core`, add:

```toml
[dependencies]
pancetta-core = { path = "../pancetta-core" }
```

(It almost certainly already depends on `pancetta-core` — `pancetta_dx::DxError` reuses `pancetta_core` types — but verify.)

- [ ] **Step 3: Delete the duplicate file and its module declaration.**

```bash
git rm pancetta-dx/src/gridsquare.rs
```

Open `pancetta-dx/src/lib.rs` and remove `pub mod gridsquare;` (and any `pub use gridsquare::…;` re-exports).

- [ ] **Step 4: Verify the workspace compiles.**

```bash
cargo check --workspace
```

Expected: clean.

- [ ] **Step 5: Run the gridsquare tests in their new home.**

```bash
cargo test -p pancetta-core gridsquare
```

Expected: same `test result: ok.` count that Task 1 saw.

- [ ] **Step 6: Run the loopback_qso regression check.**

```bash
cargo test -p pancetta --test loopback_qso
```

Expected: `test result: ok. 11 passed`.

- [ ] **Step 7: Commit.**

```bash
git add -A
git commit -m "refactor(pancetta-dx): remove duplicate gridsquare.rs (now in pancetta-core)

Task 1 of this refactor added gridsquare to pancetta-core and switched
the external caller (coordinator/tui_relay.rs) to the new path. The
copy left in pancetta-dx was always dead-on-arrival from the moment
that earlier commit landed; this commit removes it.

Maidenhead helpers are now reachable as pancetta_core::gridsquare from
every workspace crate without taking on pancetta-dx as a dependency."
```

---

## Task 5: Document the new `pancetta-dx` shape

**Files:**
- Modify: `pancetta-dx/src/lib.rs` (rewrite the crate-level `//!` doc)
- Modify: `docs/ARCHITECTURE.md` (the crate-list table)
- Modify: `README.md` (the workspace-layout table)

The crate's old name and old doc no longer match what's in it. After Tasks 1-4 it's a small, focused collection of network-integration modules — DX cluster telnet, PSKReporter spot upload, and a LoTW client kept warm for a follow-on spec. We don't rename the crate (cosmetic churn), but we do honestly document what it now contains.

- [ ] **Step 1: Rewrite `pancetta-dx/src/lib.rs` crate doc.**

Replace the existing `//!` block at the top of `lib.rs` with:

```rust
//! # pancetta-dx
//!
//! Network integrations for amateur radio data sources that don't fit
//! the cqdx.io HTTP client (`pancetta-cqdx`) — typically because they
//! speak a non-cqdx protocol (DX cluster telnet) or because the call
//! requires per-operator credentials we keep local on the pancetta
//! host (LoTW upload, future eQSL/Clublog/QRZ).
//!
//! ## Live integrations
//!
//! - [`cluster`] — traditional DX cluster telnet client, used by the
//!   `dx_cluster` coordinator component to receive spots from human
//!   operators worldwide.
//! - [`pskreporter`] — uploads locally-decoded FT8 messages to the
//!   global PSKReporter database for reciprocal spot visibility.
//!
//! ## Scaffolding
//!
//! - [`lotw`] — ARRL LoTW client with login + ADIF upload + QSL download
//!   wired but no caller yet. The credentialed-integration build-out is
//!   tracked under `docs/superpowers/specs/`. Until then the module is
//!   covered by the HTTPS scheme guard tests but isn't run from the
//!   coordinator.
//!
//! ## What used to live here
//!
//! Several modules were deleted in 2026-04 because cqdx.io now serves
//! the same data through `pancetta-cqdx`:
//!
//! | Removed module           | Replacement                                      |
//! |--------------------------|--------------------------------------------------|
//! | `dxcc.rs`                | `pancetta_cqdx::CqdxCache::resolve_entity`       |
//! | `priorities.rs`          | `pancetta_qso::priority::PriorityScorer`         |
//! | `propagation_enhanced`   | (deferred — future cqdx.io feature)              |
//! | `statistics.rs`          | `pancetta_cqdx::CqdxCache` + per-band rolling    |
//! | `tracker.rs`             | `pancetta_qso::logger::QsoLogger`                |
//! | `gridsquare.rs`          | `pancetta_core::gridsquare`                      |
//! | `geography.rs`           | `geographiclib_rs::Geodesic` directly            |
```

- [ ] **Step 2: Update `docs/ARCHITECTURE.md` crate-list table.**

Find the table that lists the eleven workspace crates (it's near the top of `docs/ARCHITECTURE.md` under "Crate Dependency Graph"). Update the `pancetta-dx` row description to match the new shape, e.g.:

```
| `pancetta-dx`    | DX cluster + PSKReporter + (scaffolded) LoTW    | Live + scaffolded |
```

If the table sets a status column, update it from "Partial implementation" to something like "Live (cluster + pskreporter); LoTW scaffolded".

- [ ] **Step 3: Update `README.md` workspace-layout table.**

Same update as Step 2 but on the table in `README.md` under "Workspace layout".

- [ ] **Step 4: Verify markdown still renders (no broken refs).**

```bash
grep -n 'pancetta-dx\|pancetta_dx' README.md docs/ARCHITECTURE.md
```

Expected: every match should resolve to text that's still accurate after the refactor (no leftover claims of "DX hunting" features that the deleted modules used to provide).

- [ ] **Step 5: Build doc to surface any broken doctest links.**

```bash
cargo doc -p pancetta-dx --no-deps 2>&1 | grep -E 'warning|error'
```

Expected: empty. Broken intra-doc links should produce "unresolved link" warnings.

- [ ] **Step 6: Commit.**

```bash
git add -A
git commit -m "docs: rewrite pancetta-dx crate doc + workspace tables for new scope

After the cleanup commits, pancetta-dx is no longer a kitchen-sink
'DX hunting' grab-bag — it's a short list of network integrations
that don't fit cqdx.io: DX cluster telnet, PSKReporter upload, and
LoTW scaffolding for a follow-on credentialed-integration spec.

Update the crate-level //! doc, ARCHITECTURE.md crate table, and
README.md workspace layout table to honestly describe what's there.
The list of removed modules + their replacements is documented inline
so future readers don't have to chase commit history."
```

---

## Task 6: Final verification + push

- [ ] **Step 1: Run the full pre-flight check.**

```bash
scripts/check.sh
```

Expected: every lane passes. If anything fails, fix it before moving on; this is the integration gate that proves the refactor is clean across the whole workspace.

- [ ] **Step 2: Push the branch.**

```bash
git push
```

- [ ] **Step 3: Wait for CI and confirm green.**

```bash
gh run watch
```

Expected: every job (`Format`, `Clippy`, `Workspace Check`, `FT8 Tests`, `Cargo Deny`, `Cross-Platform Check`) completes with `conclusion: success`.

- [ ] **Step 4: Update CLAUDE.md known-gaps section.**

Open `CLAUDE.md`, find the `## Known Gaps and TODOs` section, and remove or rewrite any entry that mentioned the deleted modules. Specifically the `pancetta-dx` "Partial implementation" callout — the crate is no longer partial, it's deliberately scoped.

```bash
grep -n 'pancetta-dx\|partial implementation' CLAUDE.md
```

Update lines that no longer reflect reality.

- [ ] **Step 5: Commit the CLAUDE.md update.**

```bash
git add CLAUDE.md
git commit -m "docs: CLAUDE.md known-gaps reflects post-prune pancetta-dx shape"
git push
```

---

## Self-review checklist (run before handing the plan to an executor)

1. **Spec coverage:** Did we account for every module the audit flagged?
   - `cluster.rs` ✓ KEEP (mentioned in Task 5 doc)
   - `dxcc.rs` ✓ DELETE (Task 2)
   - `geography.rs` ✓ DELETE (Task 2)
   - `gridsquare.rs` ✓ MOVE (Task 1) + DELETE original (Task 4)
   - `lotw.rs` ✓ KEEP scaffolding (mentioned in Task 5 doc)
   - `priorities.rs` ✓ DELETE (Task 2)
   - `propagation_enhanced.rs` ✓ DELETE (Task 2)
   - `pskreporter.rs` ✓ KEEP (mentioned in Task 5 doc)
   - `statistics.rs` ✓ DELETE (Task 2)
   - `tracker.rs` ✓ DELETE (Task 2)
   - `lib.rs` ✓ MODIFY (Tasks 2, 4, 5)
   - Config dead types ✓ DELETE (Task 3)

2. **Placeholder scan:** No "TBD" / "implement later" / "similar to Task N" / "add appropriate error handling" — verified.

3. **Type consistency:** `gridsquare::grid_to_coordinates` is the same identifier in Tasks 1, 4. `LotwConfig` referenced as the dead pancetta-config struct (Task 3) and the live pancetta-dx struct are explicitly distinguished in the Task 3 prelude.

4. **Verification gates:** Every task ends with `cargo test -p pancetta --test loopback_qso` (the canonical regression gate) plus a workspace check before commit. Task 6 runs `scripts/check.sh` as the integration gate.

---

## What this plan deliberately does NOT do

- **Rename `pancetta-dx` → `pancetta-services` or similar.** Cosmetic churn, no behavioural value, breaks every git blame for the touched files. Future spec can do it after the credentialed integrations actually land.
- **Implement LoTW upload, QRZ paid lookup, eQSL, or Clublog.** Each is a credentialed integration with its own threat model, secret-management story, ADIF generation logic, and round-trip QSL handling. They warrant their own specs once the foundation in this plan is in place.
- **Touch `pancetta-cqdx`.** The cqdx.io client is already the right shape; this plan only deletes things that *cqdx already covers* but doesn't change cqdx itself.
- **Touch the QSO logger consolidation (issue #6).** ADIF + async SQLite hybrid is the next plan after this one.
