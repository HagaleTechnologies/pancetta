---
id: local-logbook-needs
title: What will bite you about local-logbook needed and ATNO?
kind: gotcha
status: current
maintainer: agent
sources:
  - pancetta/src/priority_evaluator.rs
  - pancetta/src/priority_evaluator/local_log.rs
  - pancetta-qso/src/async_database.rs
  - pancetta-tui/src/dx_need.rs
  - docs/DECISIONS/priority-scoring.md
verified:
  commit: 2adf2c36
  date: 2026-11-09
links:
  - dxcc-entity-lookup
  - tui
---
While no cqdx.io needed list has loaded, `CachedStationLookup` answers needed
and ATNO from the local logbook (`qsos.adi`, through the index), PAN-347. The
decision record is in `docs/DECISIONS/priority-scoring.md`.

## Two display answers, on purpose

`display_needed_atno` is the cqdx-only answer. It feeds the remote
`DecodedView` and cqdx spot info, whose wire contract says cqdx. The local TUI
relay uses `needed_atno_shown`, which adds the local logbook. Do not swap them:
changing the wire meaning is a dispensa contract change.

## `lookup_need` takes the entity and the mode it is given

`DxNeedLookup::lookup_need` receives the row's entity (PAN-284) and, since
PAN-347, the row's mode. The TUI passes a mode only for rows with a local
decode; cluster and cqdx spot rows carry no reliable mode. A provider must not
resolve the callsign again or invent a mode.

## The availability flag is never reset

`local_log_available()` turns true after the first successful
`seed_worked_history` and stays true. Seeds are additive and live completions
(`record_completed_contact`) keep the sets current. A failed read changes
nothing. Do not reset it in `reset_display_history`.

## Home is an entity, not a prefix

The home entity comes from `[station] callsign`, not `dxcc_entity`. KH6, KL7
and KP4 are separate entities from the US. The old letter-prefix exclusion
set is gone; do not reintroduce prefix matching for home.

## Confirmation-based need (PAN-348)

`[autonomous] needed_means` defaults to `unworked`, preserving cqdx-first
precedence. With `unconfirmed`, a successfully seeded local log takes precedence
for local entity and band need. Global need checks confirmations on all bands;
band need checks the evaluated band, across modes. ATNO always checks worked
history. Home entities and unknown bands retain their existing exclusions.
The cqdx-only remote display helper keeps its existing meaning.

Seeds remain additive and availability never resets after a successful read.
Live completion adds worked history without inventing a confirmation. The LoTW
file command changes only the durable ADIF file under its shared lock and leaves
a rebuild request. Restart rebuilds the index before seeding the cache. A running
station does not pick up imported confirmations. No new remote fields or cqdx
API changes are involved.

## Subdivisions share DXCC identity (PAN-346)

All local entity keys use core's canonical parent name, including worked,
confirmed, band, mode, rarity and home comparisons. A TA1 QSO therefore
prevents a false TA2 ATNO. Unknown entity names stay intact. Loaded cqdx
answers take precedence under the default `unworked` policy. With
`unconfirmed`, a successfully seeded local log takes precedence.
