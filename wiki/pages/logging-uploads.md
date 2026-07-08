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
  commit: eac7aab
  date: 2026-07-07
links:
  - overview
---
QSO logging is an ADIF-first hybrid: `~/.pancetta/qsos.adi` is the durable,
append-only, vendor-neutral source of truth, and `~/.pancetta/qso.db` is a
disposable SQLite index rebuilt from it. On completion the coordinator
best-effort uploads each QSO to any enabled online logbook (ClubLog, QRZ, cqdx,
LoTW, eQSL), all default OFF. It is shaped this way so the operator's log
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
