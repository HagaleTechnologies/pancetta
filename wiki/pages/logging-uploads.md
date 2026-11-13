---
id: logging-uploads
title: Why is QSO logging ADIF-first with opt-in per-QSO uploads?
kind: decision-digest
status: current
maintainer: agent
sources:
  - docs/DECISIONS/logging-uploads.md
  - pancetta/src/coordinator/qso.rs
verified:
  commit: bfac504
  date: 2026-10-27
links:
  - overview
---
QSO logging is an ADIF-first hybrid: `~/.pancetta/qsos.adi` is the durable,
append-only, vendor-neutral source of truth, and `~/.pancetta/qso.db` is a
disposable SQLite index rebuilt from it. On completion the coordinator
uploads each QSO to any enabled online logbook (ClubLog, QRZ, cqdx, LoTW,
eQSL), all default OFF. ClubLog/QRZ/eQSL/cqdx go through a durable
pending-upload queue (`~/.pancetta/upload_outbox.json`, PAN-57) with retries,
a slow lane and readiness-strip warnings; LoTW keeps a single tqsl attempt. It is shaped this way so the operator's log
survives any store corruption and so uploads can never block or fail the QSO
pipeline. Full digest: `docs/DECISIONS/logging-uploads.md`; cqdx contract:
`docs/cqdx-api-requirements.md`.

## Digest

Every upload renders the *same* ADIF record as the source-of-truth writer
(`AdifProcessor::qso_to_adif`), so local and remote logs are identical; a
duplicate is advisory and non-fatal everywhere. Gotcha worth knowing: enabling
an upload target requires credentials or validation rejects it, and credentials
are never logged. Endpoint shapes, status-code mappings, and the cqdx idempotency
key are normative in the digest and the dispensa `cqdx-api.v1` contract.

Gotcha (PAN-232): the upload subscriber does not stop on the shutdown flag;
it stops when `stop_upload_outbox` asks it to (after the QSO components are
joined), because the Qso task can still complete a QSO after the flag flips.
Shutdown cancels a QRZ XML lookup in flight rather than waiting for it. The
worker closes its queue once every `UploadOutboxHandle` is dropped, so a new
long-lived holder of a handle must also watch `stop_requested()`, or every
shutdown waits out the 2 s backstop.

Gotcha (PAN-274): the ADIF subscriber does not stop on the shutdown flag
either. Shutdown calls `AdifSubscribers::stop` after the QSO components are
joined; the subscriber writes what is left in its channel and shutdown waits
up to 5 s for it. A new task that writes `qsos.adi` on `QsoCompleted` must
register with `AdifSubscribers` the same way, or runtime teardown cancels it
during shutdown.

Gotcha (PAN-281): `qsos.adi` appends and `qso.db` commits are fsynced.
`AdifLogWriter` runs `sync_all` after every header and record write (about
1 ms per QSO), and `qso.db` gets WAL and `synchronous = FULL` as connect
options, because `synchronous` is per connection and a `PRAGMA` through the
pool reaches only one of its five connections.

Gotcha (PAN-122): the local TX log `~/.pancetta/tx-log.jsonl` is written by
the TX worker directly through `TxFrameLog` (`coordinator/tx_log.rs`), never
via the message bus, and only once a frame's audio is `Enqueued`. The worker
waits at most 250 ms for the fsync, so a stalled disk never holds PTT, and
`--replay` writes nothing. Don't route it through `TxFrameLogged`: that event
is best-effort and is dropped headless. A test that bounds the time until
`TxFrameLogged` also times that fsync, so time it on a worker harness built
with `TxFrameLog::disabled()` (PAN-248).

## Contest history recovery

PAN-50 keeps instance context and a stable contact UUID in four ADIF app
fields, so deleting the SQLite index does not erase contest membership.
Standard CONTEST_ID remains absent for family profiles. The score reader
requires the index and ADIF to agree, so a completion event alone cannot
publish a ready score. Missing location evidence stays pending and unknown
context versions remain lossless but unavailable. Rule ids pin sponsor
editions; see docs/superpowers/specs/2026-11-05-contest-scoring.md.

Replay differs (PAN-268): `--replay` scoring reads only the in-memory seed
prepared once at startup from the canonical ADIF (an empty seed when ADIF is
absent). The contest worker never receives or reopens the real
`~/.pancetta/qso.db` during replay, and simulated completions are never
persisted, so they never change the replayed score. A failed seed preparation
reports the score unavailable instead of falling back to disk.

## Per-QSO upload state gotcha (PAN-344)

The outbox deletes an accepted entry and publishes only aggregate health,
so neither a missing queue row nor a health count proves a contact was
accepted. Correlate by QSO UUID, never callsign, and treat an explicit
service response (duplicate included) as the only acceptance; absence is
unknown, an empty captured target set is disabled, and config changes after
completion never rewrite a contact's captured denominator.


## Startup ADIF snapshots (PAN-304)

After index reconciliation and before opening the ADIF writer or starting its
subscriber, live startup snapshots changed logbook bytes. Replay and refused
legacy migration skip this step. The shared logbook lock covers comparison,
publication and pruning; a one-second timeout skips the snapshot with a warning.
The default keeps seven snapshots beside the index in `backups/`.
Only complete generated ADIF names count, separately from SQLite and import
backups. Retention never rotates or replaces the source `qsos.adi`.
A temporary owner-only file is synced before no-clobber hard-link publication;
a failed write or publication never prunes old snapshots.
