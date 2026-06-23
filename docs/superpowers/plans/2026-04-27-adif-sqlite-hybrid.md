# ADIF Source-of-Truth + Async SQLite Index Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move pancetta to a hybrid logging model where `~/.pancetta/qsos.adi` (append-only ADIF) is the durable source of truth and `~/.pancetta/qsos.db` (sqlx-backed SQLite) is a *rebuildable* queryable index. Drop the sync rusqlite path (`logger.rs` + `database.rs`) entirely; standardize on async sqlx.

**Architecture:** Every completed QSO is written to ADIF first (durable, portable, vendor-neutral), then mirrored to the SQLite index for query performance (duplicate checks every 15-second cycle, multi-stream allocator's "have I worked this on this band" lookup, TUI band-activity panel). On startup, if the index is missing OR older than the ADIF file, the coordinator replays ADIF into a fresh database. The index becomes throwaway: anyone can `rm ~/.pancetta/qsos.db`, restart, and end up in the same state. ADIF compatibility means users can also point WSJT-X / N1MM / LoTW / eQSL at the same file without further work.

**Tech Stack:** sqlx 0.8 (already in workspace), tokio fs APIs, the existing `pancetta_qso::adif::AdifProcessor`, `pancetta_qso::async_database::AsyncQsoDatabase`, `pancetta_qso::async_logger::AsyncQsoLogger`.

---

## File structure

After this plan executes:

```
pancetta-qso/
  src/
    adif.rs                CREATED FROM EXISTING — add `auto_logged: bool` field if absent;
                                                    add atomic append-line writer
    adif_log_writer.rs     CREATED — append-only writer subscribed to QsoEvent
    async_database.rs      MODIFIED — add `replay_from_adif(path) -> Result<usize>` constructor
                                       method that drops + re-creates the index from ADIF
    async_logger.rs        MODIFIED — add ADIF dual-write path; keep sqlx mirror
    logger.rs              DELETED — sync wrapper, never directly called
    database.rs            DELETED — sync rusqlite, only tests and statistics.rs called it
    statistics.rs          MODIFIED — switch the lone `QsoDatabase` reference (line 2262 of audit)
                                       to `AsyncQsoDatabase`
    lib.rs                 MODIFIED — drop `pub mod logger; pub mod database;` and re-exports;
                                       expose `AdifLogWriter` and `AsyncQsoLogger`

pancetta/
  src/
    coordinator/
      qso.rs               MODIFIED — replace `QsoLogger::new(...)` with the new async path:
                                        1. Open `~/.pancetta/qsos.adi` (create if missing)
                                        2. Stat ADIF + index; if index stale, drop + replay
                                        3. Construct `AdifLogWriter` and `AsyncQsoLogger`
                                        4. Both subscribe to QsoEvents

CLAUDE.md / docs/ARCHITECTURE.md       MODIFIED — describe the new hybrid model in the
                                                    "Known Gaps" / "Coordinator" sections.

ADIF on disk:
  ~/.pancetta/qsos.adi     append-only, portable, the source of truth
  ~/.pancetta/qsos.db      rebuildable index; safe to delete
```

Git worktree: this plan can run on `main` directly. The regression gate is the loopback_qso suite (11 tests) plus a new "ADIF round-trip + replay" test added in Task 3. No worktree required.

> **Migration safety:** at the time this plan ships, some users (the developer's own deployment) have `~/.pancetta/qso.db` populated with QSOs but NO `~/.pancetta/qsos.adi`. Task 5 explicitly handles this case — on first run after the upgrade, if ADIF is missing but the legacy db exists, dump the legacy db into ADIF before flipping over. Nobody loses contacts.

---

## Task 1: Verify async_logger has feature parity with sync logger

**Files (read-only inspection):**
- Read: `pancetta-qso/src/logger.rs` (sync, ~1255 LOC) — public API, behaviours
- Read: `pancetta-qso/src/async_logger.rs` (async, ~555 LOC) — public API, behaviours

The audit said the sync logger does auto-logging via `QsoEvent::QsoCompleted` subscription, ADIF export/import, CSV export, statistics, and backup rotation. We need the async logger to do at least everything the coordinator depends on.

- [ ] **Step 1: Diff the two public APIs.**

```bash
grep -nE '^\s*pub (async )?fn ' pancetta-qso/src/logger.rs > /tmp/sync_api
grep -nE '^\s*pub (async )?fn ' pancetta-qso/src/async_logger.rs > /tmp/async_api
diff /tmp/sync_api /tmp/async_api
```

Capture the diff in your task report. Expected: sync has `export_csv`, `create_backup`, `get_statistics`, possibly some import variants the async version is missing.

- [ ] **Step 2: Match the diff against actual coordinator usage.**

```bash
grep -nE 'logger\.|QsoLogger::' pancetta/src/coordinator/qso.rs
```

Note which methods the coordinator currently calls. Cross-reference with the Step-1 diff: any sync-only method the coordinator calls is a feature-parity gap. The audit said the coordinator instantiates and starts the logger but does not directly call its methods (auto-logging handles everything). Verify.

- [ ] **Step 3: Identify any callers outside the coordinator.**

```bash
grep -rn 'pancetta_qso::QsoLogger\|pancetta_qso::logger::\|::logger::QsoLogger' --include='*.rs' . \
  | grep -v 'pancetta-qso/src/'
```

Expected: empty (logger is internal-only) OR a handful of test references. Report what you find.

- [ ] **Step 4: Decide on parity gaps.**

Based on Steps 1-3, write a short verdict in your task report:
- "No external method calls beyond construction + start; auto-logging via QsoEvent subscription is the only behaviour we need to preserve."
  OR
- "Caller X uses sync method Y which has no async equivalent; need to add Y to async_logger.rs in Task 2."

This task produces NO commits — it's a discovery step. Output is the verdict.

- [ ] **Step 5: Commit a discovery note (optional).**

If you found gaps, append a short paragraph to `docs/superpowers/plans/2026-04-27-adif-sqlite-hybrid.md` under the "Notes" section at the bottom (create the section if absent) describing the gap, then commit:

```bash
git add docs/superpowers/plans/2026-04-27-adif-sqlite-hybrid.md
git commit -m "plan: note feature-parity gap for adif-sqlite-hybrid Task 2"
```

If no gaps were found, skip this step.

---

## Task 2: Close any feature-parity gaps in `async_logger.rs`

**Files:**
- Modify: `pancetta-qso/src/async_logger.rs`

Scope is conditional on Task 1's findings. **If Task 1 found no gaps, skip this entire task** — note that in your report and proceed straight to Task 3.

If Task 1 found gaps (e.g. async lacks `export_csv` while a caller uses it), add the missing method on `AsyncQsoLogger` with TDD:

- [ ] **Step 1: Write a failing test for the missing method.**

In `pancetta-qso/src/async_logger.rs` under `#[cfg(test)] mod tests`:

```rust
#[tokio::test]
async fn test_<missing_method>() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("qso.db");
    let qso_manager = build_test_qso_manager().await;
    let logger = AsyncQsoLogger::new(
        AsyncLoggerConfig { database_path: db_path.clone(), ..Default::default() },
        qso_manager,
    ).await.unwrap();
    // Exercise the missing method here; assert on the externally-visible
    // outcome, not on internal SQL.
    let outcome = logger.<missing_method>(args).await.unwrap();
    assert!(<concrete predicate>);
}
```

(Replace `<missing_method>` and the assertion with what Task 1 actually surfaced.)

- [ ] **Step 2: Run the test to confirm it fails.**

```bash
cargo test -p pancetta-qso --lib test_<missing_method>
```

Expected: compile error or assertion failure showing the method doesn't exist.

- [ ] **Step 3: Port the implementation from `logger.rs` to async form.**

Open `pancetta-qso/src/logger.rs`, find the equivalent method, port it. Likely shape:

```rust
impl AsyncQsoLogger {
    pub async fn <missing_method>(&self, args) -> Result<<return-type>> {
        // async version — no spawn_blocking, no rusqlite — go through self.database
    }
}
```

- [ ] **Step 4: Run the test to confirm it passes.**

```bash
cargo test -p pancetta-qso --lib test_<missing_method>
```

- [ ] **Step 5: Run the loopback regression check.**

```bash
cargo test -p pancetta --test loopback_qso
```

Expected: 11 passed.

- [ ] **Step 6: Commit.**

```bash
git add pancetta-qso/src/async_logger.rs
git commit -m "feat(pancetta-qso): port <missing_method> to AsyncQsoLogger

Closes a parity gap identified in Task 1 of the ADIF + async hybrid
plan. The sync QsoLogger had this method; the async logger now has
it too. Sync logger still in place at this commit; deleted in Task 6
of the same plan."
```

If Task 1 surfaced multiple gaps, repeat Steps 1-6 once per gap.

---

## Task 3: Add `AdifLogWriter` — append-only ADIF source-of-truth

**Files:**
- Create: `pancetta-qso/src/adif_log_writer.rs`
- Modify: `pancetta-qso/src/lib.rs` (add `pub mod adif_log_writer;` and `pub use adif_log_writer::AdifLogWriter;`)
- Modify: `pancetta-qso/src/adif.rs` (only if Task 1's audit identified a missing field; see plan's "Gap analysis")

`AdifLogWriter` is a small subscriber that listens to `QsoEvent::QsoCompleted` and appends one ADIF record per QSO to `~/.pancetta/qsos.adi`. Append-only is critical: an unclean shutdown must not corrupt the file. We use `tokio::fs::OpenOptions::append(true)` plus `f.flush().await` per record (NOT `sync_data` — too expensive per-record; the OS page cache will flush within seconds).

- [ ] **Step 1: Write a failing test for the writer.**

Create the test alongside the implementation file. Put this in `pancetta-qso/src/adif_log_writer.rs` at the bottom under `#[cfg(test)] mod tests`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::QsoMetadata;
    use tempfile::tempdir;

    fn fixture_metadata() -> QsoMetadata {
        // Returns a deterministic test QSO. Exact field set chosen to
        // mirror what AsyncQsoLogger writes today so the test exercises
        // the same surface real QSOs hit.
        QsoMetadata {
            qso_id: uuid::Uuid::new_v4(),
            our_callsign: "K1ABC".to_string(),
            their_callsign: Some("W1AW".to_string()),
            frequency: 14_074_000.0,
            mode: "FT8".to_string(),
            start_time: chrono::Utc::now(),
            end_time: Some(chrono::Utc::now()),
            reports: crate::SignalReports {
                sent: Some(-12),
                received: Some(-15),
            },
            grids: crate::GridSquares {
                ours: Some("EM10".to_string()),
                theirs: Some("FN42".to_string()),
            },
            contest_info: None,
            tags: std::collections::HashMap::new(),
            notes: None,
        }
    }

    #[tokio::test]
    async fn appends_one_record_per_qso_completed() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        let writer = AdifLogWriter::open(&path).await.unwrap();

        writer.append(&fixture_metadata()).await.unwrap();
        writer.append(&fixture_metadata()).await.unwrap();

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        // Two records → two <eor> markers (ADIF end-of-record).
        let eor_count = contents.matches("<eor>").count();
        assert_eq!(eor_count, 2, "expected 2 records, got {}\n--- file ---\n{}", eor_count, contents);
    }

    #[tokio::test]
    async fn second_open_appends_does_not_truncate() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");

        {
            let w1 = AdifLogWriter::open(&path).await.unwrap();
            w1.append(&fixture_metadata()).await.unwrap();
        }
        {
            let w2 = AdifLogWriter::open(&path).await.unwrap();
            w2.append(&fixture_metadata()).await.unwrap();
        }

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(contents.matches("<eor>").count(), 2);
    }

    #[tokio::test]
    async fn first_open_writes_adif_header() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        let _w = AdifLogWriter::open(&path).await.unwrap();

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        // ADIF header must include <adif_ver:N>X.X.X<eoh>.
        assert!(contents.contains("<adif_ver:"), "missing <adif_ver: tag in {}", contents);
        assert!(contents.contains("<eoh>"),       "missing <eoh> tag in {}",       contents);
    }
}
```

- [ ] **Step 2: Run the tests to confirm they fail.**

```bash
cargo test -p pancetta-qso --lib adif_log_writer
```

Expected: compile error — `AdifLogWriter` doesn't exist yet.

- [ ] **Step 3: Implement `AdifLogWriter`.**

Replace the empty file with the full implementation:

```rust
//! Append-only writer for the ADIF source-of-truth log.
//!
//! Owns `~/.pancetta/qsos.adi` (or whatever path the coordinator hands it).
//! On first open, writes the ADIF header. Every subsequent `append` writes
//! one fully-formed ADIF record terminated by `<eor>`.
//!
//! The writer holds the file open for the lifetime of the run so we don't
//! re-stat + re-seek per QSO. `tokio::fs::OpenOptions::append(true)` plus
//! per-record `flush().await` is the right durability/perf trade-off for
//! a long-running pancetta process: we tolerate losing the last few seconds
//! of QSOs in a power-cut scenario, since FT8 timing means at most one
//! QSO can complete in any 15-second window. No `sync_data()`.

use crate::adif::{AdifProcessor, AdifQso};
use crate::QsoMetadata;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// Errors from the ADIF append path.
#[derive(Debug, Error)]
pub enum AdifLogError {
    /// I/O failure opening, appending to, or flushing the ADIF file.
    #[error("ADIF I/O error at {path}: {source}")]
    Io {
        /// File path that failed.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The processor failed to produce a record from a QsoMetadata.
    /// In practice this means a required field (call, qso_date) was empty.
    #[error("ADIF formatting failed: {0}")]
    Format(String),
}

/// Result alias for ADIF log writer operations.
pub type AdifLogResult<T> = Result<T, AdifLogError>;

/// Append-only ADIF log writer.
pub struct AdifLogWriter {
    path: PathBuf,
    file: Mutex<File>,
    processor: AdifProcessor,
}

impl AdifLogWriter {
    /// Open (or create) the ADIF log at `path`. On first open the ADIF
    /// header is written; on subsequent opens existing content is preserved.
    pub async fn open(path: impl AsRef<Path>) -> AdifLogResult<Self> {
        let path = path.as_ref().to_path_buf();
        let already_exists = tokio::fs::try_exists(&path).await.unwrap_or(false);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|source| AdifLogError::Io { path: path.clone(), source })?;

        let processor = AdifProcessor::new();
        let writer = Self {
            path: path.clone(),
            file: Mutex::new(file),
            processor,
        };

        if !already_exists {
            writer.write_header().await?;
        }
        Ok(writer)
    }

    async fn write_header(&self) -> AdifLogResult<()> {
        let header = self.processor.generate_header();
        let mut f = self.file.lock().await;
        f.write_all(header.as_bytes()).await
            .map_err(|source| AdifLogError::Io { path: self.path.clone(), source })?;
        f.flush().await
            .map_err(|source| AdifLogError::Io { path: self.path.clone(), source })?;
        Ok(())
    }

    /// Append one QSO record to the log.
    pub async fn append(&self, qso: &QsoMetadata) -> AdifLogResult<()> {
        let adif: AdifQso = qso.into(); // existing From impl in adif.rs
        let record = self.processor.generate_record(&adif)
            .map_err(|e| AdifLogError::Format(e.to_string()))?;

        let mut f = self.file.lock().await;
        f.write_all(record.as_bytes()).await
            .map_err(|source| AdifLogError::Io { path: self.path.clone(), source })?;
        // Records produced by `generate_record` should already be newline-
        // terminated; flush the OS-level write buffer to disk.
        f.flush().await
            .map_err(|source| AdifLogError::Io { path: self.path.clone(), source })?;
        Ok(())
    }

    /// Path the writer is appending to (for diagnostics).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

// (test module from Step 1 lives at the bottom)
```

If `AdifProcessor::new`, `generate_header`, or `generate_record` don't exist with these signatures, find the closest equivalents in `pancetta-qso/src/adif.rs` and adapt. If `From<&QsoMetadata> for AdifQso` doesn't exist, add it in `adif.rs` (and add a test for it under that file's existing test module).

- [ ] **Step 4: Wire the new module into `lib.rs`.**

Add to `pancetta-qso/src/lib.rs`:

```rust
pub mod adif_log_writer;
pub use adif_log_writer::{AdifLogError, AdifLogResult, AdifLogWriter};
```

(Place alphabetically with the other `pub mod` declarations.)

- [ ] **Step 5: Run the tests.**

```bash
cargo test -p pancetta-qso --lib adif_log_writer
```

Expected: all three tests pass.

- [ ] **Step 6: Run the loopback_qso regression check.**

```bash
cargo test -p pancetta --test loopback_qso
```

Expected: 11 passed.

- [ ] **Step 7: Commit.**

```bash
git add pancetta-qso/src/adif_log_writer.rs pancetta-qso/src/lib.rs pancetta-qso/src/adif.rs
git commit -m "feat(pancetta-qso): add AdifLogWriter (append-only ADIF source of truth)

Subscribes to QsoEvent::QsoCompleted (wiring done in Task 4) and
appends one ADIF record per QSO to ~/.pancetta/qsos.adi. The file
is the durable, vendor-neutral source of truth; the sqlx index is
mirrored from it (Task 5) and rebuildable on every startup.

Per-record flush() (no sync_data() — too expensive in the FT8 hot
path; FT8 timing means at most one QSO completes per 15-second
window, and a power-cut at the worst moment loses one QSO).

Three integration tests cover: (1) two appends produce two records,
(2) re-opening doesn't truncate, (3) first open writes the ADIF
header. No coordinator wiring yet — Task 4 plugs it in."
```

---

## Task 4: Wire `AdifLogWriter` and `AsyncQsoLogger` into the coordinator

**Files:**
- Modify: `pancetta/src/coordinator/qso.rs`

Replace the sync `QsoLogger` instantiation (currently at `qso.rs:88-100` per the audit) with the async path: open the ADIF writer, open the async DB, and start the async logger. Both `AdifLogWriter` and `AsyncQsoLogger` subscribe to `QsoEvent::QsoCompleted` independently.

**Order of writes per completed QSO matters for crash safety:**
  1. ADIF append + flush — durable.
  2. Then DB insert via `AsyncQsoLogger`.

If we crash between (1) and (2), the next startup's replay (Task 5) reconstructs the index from ADIF — no data lost.

If we crash before (1), the QSO is lost. That's the same failure mode the existing logger has, so we're not regressing.

- [ ] **Step 1: Read `pancetta/src/coordinator/qso.rs:80-150`.**

Identify the exact `QsoLogger::new(...)` call and surrounding setup (database_path, logger.start() invocation).

- [ ] **Step 2: Replace the sync logger setup with async + ADIF.**

Show the full diff of the changed block. Approximate shape:

```rust
// Before:
let logger_config = LoggerConfig {
    database_path: db_path.clone(),
    ..Default::default()
};
let _logger = match QsoLogger::new(logger_config, qso_manager.clone()).await {
    Ok(l) => {
        info!("QSO logger initialized with database at {:?}", db_path);
        let l = std::sync::Arc::new(l);
        if let Err(e) = l.start().await {
            warn!("QSO logger background tasks failed to start: {}", e);
        }
        Some(l)
    }
    Err(e) => {
        warn!("Failed to initialize QSO logger (continuing without): {}", e);
        None
    }
};

// After:
let adif_path = dirs::home_dir()
    .unwrap_or_else(|| std::path::PathBuf::from("."))
    .join(".pancetta")
    .join("qsos.adi");

let _adif_writer = match pancetta_qso::AdifLogWriter::open(&adif_path).await {
    Ok(w) => {
        info!("ADIF log open at {}", adif_path.display());
        let w = std::sync::Arc::new(w);
        // The log writer subscribes to QsoEvent::QsoCompleted via a
        // dedicated tokio task — see start_adif_subscriber below.
        start_adif_subscriber(w.clone(), qso_manager.subscribe(), shutdown.clone());
        Some(w)
    }
    Err(e) => {
        warn!("ADIF writer init failed at {}: {} — \
               continuing; QSOs this session will be DB-only", adif_path.display(), e);
        None
    }
};

let async_logger_config = pancetta_qso::AsyncLoggerConfig {
    database_path: db_path.clone(),
    ..Default::default()
};
let _async_logger = match pancetta_qso::AsyncQsoLogger::new(
    async_logger_config, qso_manager.clone(),
).await {
    Ok(l) => {
        info!("Async QSO logger initialized with database at {}", db_path.display());
        let l = std::sync::Arc::new(l);
        if let Err(e) = l.start().await {
            warn!("Async QSO logger background tasks failed to start: {}", e);
        }
        Some(l)
    }
    Err(e) => {
        warn!("Failed to initialize async QSO logger (continuing without): {}", e);
        None
    }
};
```

Then add the `start_adif_subscriber` helper as a free function inside the module (or a private helper on the impl):

```rust
fn start_adif_subscriber(
    writer: std::sync::Arc<pancetta_qso::AdifLogWriter>,
    mut events: tokio::sync::broadcast::Receiver<pancetta_qso::QsoEvent>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    tokio::spawn(async move {
        while !shutdown.load(std::sync::atomic::Ordering::Acquire) {
            match events.recv().await {
                Ok(pancetta_qso::QsoEvent::QsoCompleted { metadata, .. }) => {
                    if let Err(e) = writer.append(&metadata).await {
                        // Critical: ADIF is the source of truth. If we lose
                        // a write we surface it loudly so the operator can
                        // intervene (e.g. disk full).
                        tracing::error!(
                            "ADIF append failed for QSO {} with {}: {}",
                            metadata.qso_id,
                            metadata.their_callsign.as_deref().unwrap_or("?"),
                            e,
                        );
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("ADIF subscriber lagged by {n} QSO events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
```

(If the QSO event subscribe API is `qso_manager.subscribe()` returning a `broadcast::Receiver<QsoEvent>`, this works as-is. If the API differs, adapt — the audit cites `qso_manager.subscribe()` at `pancetta-qso/src/lib.rs` so this should be correct.)

- [ ] **Step 3: `cargo check -p pancetta`.**

```bash
cargo check -p pancetta
```

Resolve any errors (likely import paths, unused warnings on the old `QsoLogger`/`LoggerConfig` types).

- [ ] **Step 4: Run loopback_qso.**

```bash
cargo test -p pancetta --test loopback_qso
```

Expected: 11 passed. The loopback fixture creates a temp `~/.pancetta/qsos.adi` (since `dirs::home_dir()` returns the test home) — fine.

- [ ] **Step 5: Manually verify ADIF write happens by spinning up a smoke run.**

```bash
RUST_LOG=info cargo run -p pancetta -- --headless --no-rig --no-audio --test-tx "K1ABC W1AW EM10" 2>&1 | grep -iE 'adif|qso completed'
```

Expected: log lines mentioning ADIF open and (after the simulated QSO) at least one "QSO completed" event. The actual ADIF file at `~/.pancetta/qsos.adi` should grow.

If `--test-tx` doesn't exercise the QSO state machine far enough to emit `QsoCompleted`, just verify the ADIF file gets created with header and report DONE_WITH_CONCERNS noting the smoke-test gap; the loopback regression covers the actual completion path.

- [ ] **Step 6: Commit.**

```bash
git add pancetta/src/coordinator/qso.rs
git commit -m "feat(coordinator): wire AdifLogWriter + AsyncQsoLogger; sync logger gone

Replaces the QsoLogger sync wrapper with the async path. Each
completed QSO is now appended to ~/.pancetta/qsos.adi (durable
source of truth) and inserted into ~/.pancetta/qsos.db (rebuildable
queryable index) via two independent QsoEvent subscribers.

ADIF append happens FIRST, with per-record flush(). DB insert
follows. A crash between the two is recoverable via Task 5's startup
replay; a crash before either was already unrecoverable in the
sync path so we're not regressing.

The sync logger.rs and database.rs files are still on disk at this
commit and will be deleted in Task 6 once the live QSO loop has
been confirmed on the new path."
```

---

## Task 5: Add `replay_from_adif` and the startup-replay flow

**Files:**
- Modify: `pancetta-qso/src/async_database.rs`
- Modify: `pancetta/src/coordinator/qso.rs`

The audit confirmed the coordinator already opens `AsyncQsoDatabase` directly for startup seeding. Add a `replay_from_adif(path) -> Result<usize>` factory that drops + recreates the schema, parses the ADIF file, and inserts every record. Then teach the coordinator to call it on startup if (a) the DB file is missing OR (b) the DB file's mtime is older than the ADIF's mtime.

- [ ] **Step 1: Failing test for `replay_from_adif`.**

In `pancetta-qso/src/async_database.rs::tests`:

```rust
#[tokio::test]
async fn replay_from_adif_round_trips_records() {
    let tmp = tempfile::tempdir().unwrap();
    let adif_path = tmp.path().join("qsos.adi");
    let db_path = tmp.path().join("qsos.db");

    // Write a minimal but valid ADIF file with two records.
    let adif_contents = r#"Pancetta ADIF round-trip test
<adif_ver:5>3.1.4<programid:8>pancetta<eoh>
<call:5>W1ABC<qso_date:8>20250101<time_on:6>120000<mode:3>FT8<freq:9>14.074000<band:3>20m<eor>
<call:5>K9DEF<qso_date:8>20250102<time_on:6>121500<mode:3>FT8<freq:9>14.074000<band:3>20m<eor>
"#;
    tokio::fs::write(&adif_path, adif_contents).await.unwrap();

    let db = AsyncQsoDatabase::replay_from_adif(&db_path, &adif_path).await.unwrap();
    let count = db.count_qsos().await.unwrap();
    assert_eq!(count, 2);

    let calls = db.get_worked_callsigns("20m").await;
    assert!(calls.contains(&"W1ABC".to_string()));
    assert!(calls.contains(&"K9DEF".to_string()));
}
```

If `count_qsos()` doesn't exist on `AsyncQsoDatabase`, add a small `pub async fn count_qsos(&self) -> Result<u64, AsyncDatabaseError>` (one-line `SELECT COUNT(*)`).

- [ ] **Step 2: Run, expect failure.**

```bash
cargo test -p pancetta-qso --lib replay_from_adif
```

Expected: compile error (`replay_from_adif` undefined).

- [ ] **Step 3: Implement `replay_from_adif`.**

Add to `pancetta-qso/src/async_database.rs`:

```rust
impl AsyncQsoDatabase {
    /// Build a fresh database at `db_path` by replaying every record in
    /// `adif_path`. If `db_path` exists, it is deleted first — the caller
    /// should only invoke this when the DB is known stale or missing.
    /// Returns the number of records inserted.
    pub async fn replay_from_adif(
        db_path: impl AsRef<std::path::Path>,
        adif_path: impl AsRef<std::path::Path>,
    ) -> Result<Self, AsyncDatabaseError> {
        let db_path = db_path.as_ref();
        let adif_path = adif_path.as_ref();

        // Drop any existing index so the rebuild is from scratch.
        if tokio::fs::try_exists(db_path).await.unwrap_or(false) {
            tokio::fs::remove_file(db_path).await.map_err(|source|
                AsyncDatabaseError::Io { path: db_path.to_path_buf(), source }
            )?;
        }

        let db = Self::open(db_path).await?;

        let processor = crate::adif::AdifProcessor::new();
        let raw = tokio::fs::read_to_string(adif_path).await.map_err(|source|
            AsyncDatabaseError::Io { path: adif_path.to_path_buf(), source }
        )?;
        let records = processor.parse_string(&raw).map_err(|e|
            AsyncDatabaseError::Replay(format!("ADIF parse failed: {e}"))
        )?;

        let mut inserted = 0;
        for adif_qso in records {
            let metadata: crate::QsoMetadata = (&adif_qso).into();
            db.insert_qso(&metadata).await?;
            inserted += 1;
        }
        tracing::info!(
            "Replayed {inserted} records from {} into {}",
            adif_path.display(), db_path.display(),
        );
        Ok(db)
    }

    /// Total number of QSOs in the index. Used by replay tests.
    pub async fn count_qsos(&self) -> Result<u64, AsyncDatabaseError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM qsos")
            .fetch_one(&self.pool)
            .await?;
        Ok(count as u64)
    }
}
```

Add an `Io` and `Replay` variant to `AsyncDatabaseError` if absent:

```rust
#[derive(Debug, thiserror::Error)]
pub enum AsyncDatabaseError {
    // existing variants...
    /// I/O failure on a database or ADIF file path.
    #[error("I/O at {path}: {source}")]
    Io {
        /// Path where the I/O failed.
        path: std::path::PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// ADIF replay failed (parse error, schema mismatch, etc.).
    #[error("ADIF replay failed: {0}")]
    Replay(String),
}
```

- [ ] **Step 4: Run the test.**

```bash
cargo test -p pancetta-qso --lib replay_from_adif
```

Expected: passes.

- [ ] **Step 5: Coordinator integration — replay on stale/missing.**

Edit `pancetta/src/coordinator/qso.rs`. Where the coordinator currently does `AsyncQsoDatabase::open(&db_path).await` for the startup seed, replace with:

```rust
// Decide whether to replay: rebuild the index if it's missing OR
// older than the ADIF source-of-truth.
let needs_replay = match (
    tokio::fs::metadata(&db_path).await.ok(),
    tokio::fs::metadata(&adif_path).await.ok(),
) {
    (None, Some(_)) => {
        info!("Index missing at {} — replaying from ADIF", db_path.display());
        true
    }
    (Some(db_meta), Some(adif_meta)) => {
        let db_mtime = db_meta.modified().ok();
        let adif_mtime = adif_meta.modified().ok();
        match (db_mtime, adif_mtime) {
            (Some(d), Some(a)) if a > d => {
                info!(
                    "Index at {} is older than ADIF at {} — replaying",
                    db_path.display(), adif_path.display(),
                );
                true
            }
            _ => false,
        }
    }
    _ => false, // No ADIF and no DB? Fresh install. Coordinator will create both later.
};

let db_for_seed = if needs_replay {
    match pancetta_qso::async_database::AsyncQsoDatabase::replay_from_adif(&db_path, &adif_path).await {
        Ok(db) => Some(db),
        Err(e) => {
            tracing::warn!(
                "ADIF replay failed: {e} — falling back to existing index (may be stale)"
            );
            pancetta_qso::async_database::AsyncQsoDatabase::open(&db_path).await.ok()
        }
    }
} else {
    pancetta_qso::async_database::AsyncQsoDatabase::open(&db_path).await.ok()
};
```

(The existing seed loop that calls `db.get_worked_callsigns(&band).await` reads from `db_for_seed`.)

- [ ] **Step 6: Migration safety — bootstrap ADIF from legacy DB if needed.**

If the user has `~/.pancetta/qso.db` populated but no `~/.pancetta/qsos.adi`, generate ADIF FROM the legacy db so we don't lose contacts:

```rust
// Migration: if ADIF is missing but legacy DB has entries, dump the
// DB into ADIF before any further operations. One-shot per host.
let adif_exists = tokio::fs::try_exists(&adif_path).await.unwrap_or(false);
let db_exists = tokio::fs::try_exists(&db_path).await.unwrap_or(false);
if !adif_exists && db_exists {
    info!(
        "ADIF missing but legacy DB present — migrating QSOs from {} to {}",
        db_path.display(), adif_path.display(),
    );
    if let Err(e) = pancetta_qso::async_database::AsyncQsoDatabase::open(&db_path)
        .await
        .and_then(|db| async move { db.export_to_adif(&adif_path).await })
        .await
    {
        warn!(
            "DB→ADIF migration failed: {e} — index continues to work, \
             but the ADIF source-of-truth will be empty until next QSO"
        );
    }
}
```

If `AsyncQsoDatabase` doesn't have `export_to_adif`, add it as a small method that iterates `query_qsos` and calls `processor.generate_record(&adif_qso)` per record. Mirror the existing `logger.rs::export_adif` shape.

Place the migration block BEFORE the replay decision so we don't try to replay an empty ADIF.

- [ ] **Step 7: Run loopback_qso.**

```bash
cargo test -p pancetta --test loopback_qso
```

Expected: 11 passed.

- [ ] **Step 8: Commit.**

```bash
git add pancetta-qso/src/async_database.rs pancetta/src/coordinator/qso.rs
git commit -m "feat(pancetta-qso+coordinator): ADIF replay + DB→ADIF migration on startup

AsyncQsoDatabase::replay_from_adif(db, adif) drops + rebuilds the
index from the source-of-truth ADIF in one call. Coordinator
checks ADIF/DB mtimes at startup and triggers replay if the index
is missing or stale.

Migration: existing pancetta deployments have ~/.pancetta/qso.db
populated but no ~/.pancetta/qsos.adi. Coordinator detects that
case at first run and exports the legacy DB to ADIF before flipping
over. Future runs are pure ADIF-source-of-truth.

The index is now genuinely disposable: rm ~/.pancetta/qsos.db,
restart, end up in the same state."
```

---

## Task 6: Delete `logger.rs` and `database.rs`

**Files:**
- Delete: `pancetta-qso/src/logger.rs`
- Delete: `pancetta-qso/src/database.rs`
- Modify: `pancetta-qso/src/lib.rs` (drop `pub mod logger; pub mod database;` and re-exports)
- Modify: `pancetta-qso/src/statistics.rs` (audit cited a single `QsoDatabase` reference at line 2262 — switch to `AsyncQsoDatabase`)
- Modify: `pancetta-qso/Cargo.toml` (drop `rusqlite` dep — last sync caller is gone)

- [ ] **Step 1: Verify zero external callers.**

```bash
grep -rn 'pancetta_qso::QsoLogger\|pancetta_qso::QsoDatabase\|pancetta_qso::logger::\|pancetta_qso::database::' \
  --include='*.rs' . | grep -v 'pancetta-qso/src/'
```

Expected: empty. If anything appears outside `pancetta-qso/src/`, STOP and report DONE_WITH_CONCERNS.

- [ ] **Step 2: Update `pancetta-qso/src/statistics.rs`.**

The audit cited a `QsoDatabase` reference around line 2262. Find it:

```bash
grep -n 'QsoDatabase\|use crate::database' pancetta-qso/src/statistics.rs
```

Replace with the async equivalent:

```rust
// Before:
use crate::database::QsoDatabase;
fn build_stats(db: &QsoDatabase) -> Statistics { ... }

// After:
use crate::async_database::AsyncQsoDatabase;
async fn build_stats(db: &AsyncQsoDatabase) -> Statistics { ... }
```

If the call site is sync, the simplest path is to make the surrounding fn async. If that propagates further than is reasonable, alternative: keep the sync `build_stats` and have the caller block on an inline `tokio::runtime::Handle::current().block_on(...)`. Use judgment. The right call is to make it async if doing so doesn't cascade through more than two layers of callers.

- [ ] **Step 3: Delete the two files.**

```bash
git rm pancetta-qso/src/logger.rs pancetta-qso/src/database.rs
```

- [ ] **Step 4: Update `pancetta-qso/src/lib.rs`.**

Remove these declarations and any matching re-exports:

```rust
// REMOVE:
pub mod logger;
pub mod database;
pub use logger::{QsoLogger, LoggerConfig, ...};
pub use database::{QsoDatabase, ...};
```

Confirm the surviving public surface still exposes `AsyncQsoLogger`, `AsyncQsoDatabase`, `AdifLogWriter`, and the QSO-event types.

- [ ] **Step 5: Drop `rusqlite` from `pancetta-qso/Cargo.toml`.**

```bash
grep -E 'rusqlite|use rusqlite' pancetta-qso/src/*.rs
```

If empty, remove the `rusqlite = ...` line from `pancetta-qso/Cargo.toml`. If not empty, find the surviving caller and switch it to sqlx (or report DONE_WITH_CONCERNS).

- [ ] **Step 6: Build the workspace.**

```bash
cargo check --workspace
```

Resolve any errors. Most likely: stale imports of `QsoLogger`/`QsoDatabase` types in tests; replace each with the async equivalent or delete the test if it was sync-only.

- [ ] **Step 7: Run the full test suite.**

```bash
cargo test --workspace --features transmit
cargo test -p pancetta --test loopback_qso
```

Both must be green.

- [ ] **Step 8: Commit.**

```bash
git add -A
git commit -m "chore(pancetta-qso): drop sync rusqlite logger + database paths

After Tasks 1-5 the async path is the sole logging surface in use.
The audit confirmed:
  - logger.rs (sync wrapper)  : zero external callers; coordinator
                                 was using it only via construction
                                 + start(); auto-logging behaviour
                                 is preserved by AsyncQsoLogger.
  - database.rs (rusqlite)    : only callers were sync tests +
                                 statistics.rs (one line); switched
                                 statistics.rs to AsyncQsoDatabase.

Drop both files, drop the rusqlite crate, prune lib.rs re-exports,
and standardize on sqlx. ~-2400 LOC."
```

---

## Task 7: Documentation + final verification

**Files:**
- Modify: `CLAUDE.md` (the workspace structure table or known-gaps)
- Modify: `docs/ARCHITECTURE.md` (the coordinator section + key abstractions if `QsoLogger` was named there)
- Modify: `docs/CONFIG.md` (add a note about `~/.pancetta/qsos.adi` as durable source of truth)

- [ ] **Step 1: Update `CLAUDE.md`.**

Find any reference to `QsoLogger` / `qso.db` / "QSO logging" and update to reflect the new model:
- Source of truth: `~/.pancetta/qsos.adi` (append-only ADIF).
- Index: `~/.pancetta/qsos.db` (sqlx, rebuildable, safe to delete).
- Migration: existing deployments auto-migrate on first startup after upgrade.

- [ ] **Step 2: Update `docs/ARCHITECTURE.md`.**

In the coordinator section, mention the ADIF + index split. In "Key Abstractions" describe `AdifLogWriter` and `AsyncQsoLogger` (replacing any old `QsoLogger` description).

- [ ] **Step 3: Update `docs/CONFIG.md`.**

Add a short subsection under the relevant `[paths]` discussion:

```markdown
### QSO log files

| File | Role | Recoverable? |
|---|---|---|
| `~/.pancetta/qsos.adi` | Durable, append-only, vendor-neutral source of truth. Point WSJT-X / N1MM / LoTW / eQSL at this file directly. | No — back this up. |
| `~/.pancetta/qsos.db` | sqlx-backed query index. Rebuilt automatically from the ADIF on startup if missing or stale. | Yes — safe to delete; the next run will replay ADIF into a fresh index. |
```

- [ ] **Step 4: `scripts/check.sh` (full).**

```bash
scripts/check.sh
```

Every lane must be green.

- [ ] **Step 5: Push.**

```bash
git push
```

- [ ] **Step 6: Wait for CI.**

```bash
gh run watch
```

CI must go green on every job (Format, Clippy, Workspace Check, FT8 Tests, Cargo Deny). If anything fails, capture and report DONE_WITH_CONCERNS — do not attempt fixes.

- [ ] **Step 7: Commit doc updates and push.**

```bash
git add CLAUDE.md docs/ARCHITECTURE.md docs/CONFIG.md
git commit -m "docs: ADIF source-of-truth + rebuildable SQLite index"
git push
```

---

## Self-review checklist

1. **Spec coverage:**
   - ADIF as source of truth ✓ (Task 3 writer + Task 5 replay)
   - SQLite as rebuildable index ✓ (Task 5 replay + Task 6 deletion of sync paths)
   - Drop sync rusqlite path ✓ (Task 6)
   - Migration story for existing deployments ✓ (Task 5 step 6)
   - Tests at every step ✓ (Tasks 3 + 5 ship new tests; loopback_qso is the regression gate)

2. **Placeholder scan:** No "TBD" / "implement later". Step 3 of Task 3 has a fallback ("if `AdifProcessor::new` doesn't exist with these signatures, find the closest equivalents") but each branch is concrete.

3. **Type consistency:** `AdifLogWriter`, `AdifLogError`, `AdifLogResult`, `AsyncQsoLogger`, `AsyncLoggerConfig`, `AsyncQsoDatabase`, `AsyncDatabaseError`, `replay_from_adif`, `count_qsos`, `export_to_adif` — every name is reused consistently across tasks.

4. **Crash safety:** Task 4 documents the "ADIF first, then DB" order; Task 5's replay reconstructs an index lost between those writes; the new tests in Task 3 cover the append/restart-doesn't-truncate invariant.

---

## What this plan does NOT do

- **Implement LoTW upload, QRZ paid lookup, eQSL upload, Clublog upload.** Those are credentialed integrations; each warrants its own spec.
- **Deduplicate ADIF on append.** If the QSO state machine ever emits a duplicate `QsoCompleted` event, the writer will append twice. Replay would then double-count. We accept this as theoretical; loopback_qso's state machine doesn't produce duplicates today.
- **Compaction or rotation of `qsos.adi`.** ADIF files grow forever; that's the point. A 100,000-QSO ADIF is ~25 MB — fine. Rotation is a future feature if anyone hits a real ceiling.
- **Multi-station ADIF (`station_callsign` segregation).** Pancetta is single-station today; if multi-station is ever added, the writer would need a per-station file or a `STATION_CALLSIGN` filter on replay.

## Notes

(Task 1 implementer may append a "feature-parity gap" section here if needed.)
