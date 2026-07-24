# DX Watchlist Design

**Status:** Draft, pending review
**Date:** 2026-07-24
**Related:** GitHub issue [#197](https://github.com/HagaleTechnologies/pancetta/issues/197),
`docs/superpowers/specs/2026-07-22-mid-tx-abort-restart-design.md` (predecessor — split this
feature off explicitly as out of scope), `pancetta-qso/src/priority.rs`, `pancetta-qso/src/autonomous.rs`

## Problem

`AutonomousOperator::feed_decoded_messages_at` rebuilds `pending_cqs` from scratch every ~15s
decode cycle, from that cycle's decoded messages only. A genuinely valuable CQ (new-per-band DXCC
or better) heard in one cycle but not acted on — because we were at TX capacity, mid-transmission
on another QSO, or it simply lost that cycle's single-pounce-slot competition to an even better
candidate — is gone the instant the next cycle clears and rebuilds the list. There is no memory
across cycles. If that same station doesn't happen to CQ again in the exact cycle we next have
capacity, it's silently missed with no record it was ever heard.

The mid-TX abort/restart design (2026-07-22) explicitly carved this out as a separate, narrower
problem: autonomous abort-and-rekey is reserved for `PriorityTier::Atno`/`PerBandDxccNew` candidates
that justify truncating a live transmission. Everything in that same tier range that shows up while
we're merely *busy* (not truncation-worthy, just not free) deserves a lighter-weight response:
remember it, and give it a fair shot the next time it's heard actively CQing and we have room.

## Scope

**In scope:**
- A short-lived (~2-3 minute) per-callsign memory (`DxWatchlist`) of `PerBandDxccNew`+ CQs heard but
  not pounced on.
- Extending `DxEvaluator` (`pancetta-qso/src/autonomous.rs`) with a tier-returning method, since the
  autonomous decision loop today only ever sees a continuous `dx_score: f64` — `PriorityTier` exists
  only on the `pancetta-dx` side (DX Hunter display). Wiring the real `PriorityScorer::score_tiered`
  through `pancetta/src/priority_evaluator.rs` so the autonomous loop can classify tier at all.
- A small marker on existing DX Hunter TUI rows showing watchlist membership.
- Removing an entry early once that callsign is actually worked.

**Explicitly out of scope:**
- Any change to what triggers a transmission. The watchlist never manufactures a pounce target —
  it only ever reacts to a callsign that is freshly, actively CQing in the current decode cycle.
  It does not call anyone who has gone quiet, however recently.
- A dedicated watchlist panel. DX Hunter already shows every heard station persistently (24h
  retention); this reuses that surface.
- Mid-TX abort integration — this queue exists precisely *because* these candidates are NOT
  worth aborting a live TX for (that's the Atno/PerBandDxccNew abort path's job, already scoped
  separately).

## Design

### Data model

New module `pancetta-qso/src/watchlist.rs` — kept out of `autonomous.rs` (already ~3275 lines)
as its own single-purpose, independently testable unit, mirroring how `BandStrategy` /
`FrequencyAllocator` / `CollisionDetector` are already separate structs in this crate.

```rust
pub struct DxWatchlistEntry {
    pub callsign: String,
    pub grid: Option<String>,
    pub tier: PriorityTier,       // PerBandDxccNew or Atno only
    pub last_heard: DateTime<Utc>,
}

pub struct DxWatchlist {
    entries: HashMap<String, DxWatchlistEntry>,
    ttl: Duration,                // default ~2.5 min
}

impl DxWatchlist {
    pub fn new(ttl: Duration) -> Self;
    pub fn refresh(&mut self, callsign: &str, grid: Option<&str>, tier: PriorityTier, now: DateTime<Utc>);
    pub fn prune(&mut self, now: DateTime<Utc>);
    pub fn remove(&mut self, callsign: &str);   // called once a QSO with this callsign completes
    pub fn entries(&self) -> impl Iterator<Item = &DxWatchlistEntry>;  // read-only, for TUI/status
}
```

`AutonomousOperator` owns one `DxWatchlist` alongside its existing `frequency_allocator` /
`band_strategy` fields.

### `DxEvaluator` tier extension

`pancetta-qso/src/autonomous.rs`'s `DxEvaluator` trait currently exposes only:

```rust
pub trait DxEvaluator: Send + Sync {
    fn evaluate_cq(&self, callsign: &str, grid: Option<&str>, snr: i8, freq_hz: f64) -> f64;
}
```

Add a second method with a default, so existing implementors (`NullDxEvaluator`, test scaffolding)
are unaffected:

```rust
pub trait DxEvaluator: Send + Sync {
    fn evaluate_cq(&self, callsign: &str, grid: Option<&str>, snr: i8, freq_hz: f64) -> f64;

    /// Tiered classification, when available. `None` for evaluators that don't
    /// implement tiered scoring (e.g. `NullDxEvaluator`) — callers that need a
    /// tier (the watchlist) simply skip entries with `None`.
    fn evaluate_cq_tiered(
        &self,
        _callsign: &str,
        _grid: Option<&str>,
        _snr: i8,
        _freq_hz: f64,
    ) -> Option<crate::priority::TieredScore> {
        None
    }
}
```

`pancetta/src/priority_evaluator.rs`'s real adapter (wrapping `pancetta_qso::priority::PriorityScorer`)
implements `evaluate_cq_tiered` by delegating to the existing `PriorityScorer::score_tiered` — the
exact same call DX Hunter's display path already uses, preserving the single-scorer invariant (no
second, divergent tier computation).

### Population — one-way read, not an injection path

The watchlist never creates a `Transmit` action by itself and never feeds synthetic entries back
into `pending_cqs`. It only observes:

1. In `feed_decoded_messages_at`, after `pending_cqs` is built and sorted (unchanged from today),
   iterate it once more: for any candidate where `evaluator.evaluate_cq_tiered(...)` returns
   `PerBandDxccNew` or `Atno`, call `watchlist.refresh(callsign, grid, tier, now)`. This runs
   regardless of what `decide_at` goes on to do that cycle — it captures both gap cases from the
   Problem section: heard-while-at-capacity, and heard-but-lost-that-cycle's-single-pounce-slot.
2. `watchlist.prune(now)` runs once per cycle, dropping anything older than the TTL.
3. When a QSO completes (existing completion signal already reaches `AutonomousOperator` via
   `set_active_qso_count`/auto-sequencer bookkeeping — exact hook point to be confirmed during
   planning), call `watchlist.remove(callsign)` so a just-worked station doesn't linger.

The very next cycle that callsign is freshly, actively heard CQing again, it flows through the
*existing* `pending_cqs` → tier-sort → `best_cq` selection in `decide_at` completely unchanged —
tier already dominates `TieredScore`'s ordering, so a watchlisted station doesn't need or get a
score boost to win against routine competition; it simply isn't skipped anymore for "we didn't
have a record it mattered." No second decision path to keep in sync with the real scorer.

### TUI surfacing

`pancetta-tui`'s `DxStation` (in `app.rs`) gains a field, e.g. `watchlisted: bool`, populated the
same way other DX Hunter fields already flow from coordinator status. `dx_hunter.rs`'s
`create_dx_row`/`need_marker` cluster (`!` ATNO, `+` needed, `★` notable, `▲` band-needed) gains one
more glyph, e.g. `◇`, when `station.watchlisted` is true. No new panel; DX Hunter already displays
every heard station.

### Testing

- `watchlist.rs` unit tests: refresh/prune/TTL-boundary behavior, `remove` on worked-callsign,
  multiple refreshes to the same callsign extend `last_heard` rather than duplicating.
- `autonomous.rs` tests: a `PerBandDxccNew`+ CQ heard while at TX capacity populates the watchlist;
  an entry is pruned after TTL with no re-hear and does not resurrect on its own; a fresh re-CQ of a
  watchlisted callsign flows through the ordinary pounce path unchanged (no behavior change to
  `decide_at`'s existing selection logic).
- `priority_evaluator.rs`: the real adapter's `evaluate_cq_tiered` matches
  `PriorityScorer::score_tiered` exactly (same single-scorer test pattern used elsewhere in this
  codebase).

## Open questions for planning

- **Resolved during planning:** no explicit "QSO completed → `watchlist.remove(callsign)`" hook was
  added. `AutonomousOperator` (which owns the watchlist) isn't reachable from the QSO-completion
  call site (`pancetta/src/coordinator/qso.rs`'s `record_worked` call) without new cross-module
  plumbing, and a stale entry has no side effect — it never triggers a transmission, it's inert
  bookkeeping that self-clears via TTL (~2.5 min) either way. If on-air experience shows the DX
  Hunter marker lingering on a just-worked station is confusing, add the hook then.
- Default TTL: ~2.5 minutes is a reasonable starting point (roughly the midpoint of the "~2-3
  minutes" the operator specified); expose as a config field under `AutonomousConfig` rather than
  hardcoding, in case on-air experience says otherwise.
