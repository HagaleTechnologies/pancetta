---
slug: chrono-replay-tier
mode: ft8
state: shipped-infra
created: 2026-06-02T01:00:00Z
last_updated: 2026-06-02T01:15:00Z
branch: iter/2026-06-02-chrono-replay-tier
disposition: SHIPPED-INFRA — eval-harness substrate; no decoder behavior change, no hypothesis tested. Unblocks retests of hb-048 (a7 cross-correlation), hb-057 (median-DT prior), hb-173 (within-QSO context), all SHELVED 2026-06-02 with the same eval-corpus root cause: each WAV decoded in isolation → empty cross-slot snapshot → mechanism short-circuited.
---

## Headline

**A chronological-replay eval tier ships. WAVs in a `ChronoReplayManifest`
are processed in `slot_index` order with the decoder's
`chrono_replay_state` preserved between consecutive WAVs. This is the
binding piece of infrastructure that lets the three already-implemented
cross-slot mechanisms (hb-048 a7, hb-057 median-DT, hb-173 within-QSO)
be retested against a realistic session trace.**

## Why this exists

Retro from Batch 17 (commits `0c5c5c4`, `9f5489f`, `e2f4517`):

1. **hb-048 Session 3** — a7 cross-correlation pass, SHELVED because the
   within-WAV path produced rec Δ ∈ {0, +1} across the full threshold
   sweep. Diagnostic: WSJT-X a7 fires on slot N+1 templates rooted at
   callsigns heard in slot N. Pancetta eval has no slot N.
2. **hb-057 Session 2** — per-callsign median-DT prior, SHELVED at
   composite Δ = -0.000056 with rec CI [-3.0, +0.0]. Different root
   cause (wrong hook key — the residual sync gate doesn't know the
   candidate callsign), but the diagnostic population (cross-WAV
   callsigns) is also corpus-affected.
3. **hb-173 Session 2** — within-QSO pair-conditional cross-correlation,
   SHELVED-PENDING-CHRONOLOGICAL-EVAL with the merge deferred entirely
   because the snapshot is empty per-WAV on hard-200 (80% single-slot
   sessions per the Session 1 diagnostic).

All three SHELVE notes recommend the same unblock: an eval tier that
preserves cross-slot decoder state. This iter ships it.

## What was built

### `pancetta-research/src/chrono_replay.rs` (new)

- `ChronoReplayManifest`, `ChronoReplayEntry` POD types.
- Strict ordering invariant: `slot_index` values must be `0..N` ascending
  (asserted in `load`).
- `load_chrono_replay_corpus(manifest_path)` returns entries in natural order.
- 3 unit tests: round-trip preservation, out-of-order rejection, unknown
  schema rejection. All pass.

### `pancetta-research/src/decoder.rs` (extended)

- New field on `Ft8Decoder`: `chrono_replay_state: Option<ChronoReplayState>`.
- `ChronoReplayState` holds a shared `Arc<Mutex<VecDeque<String>>>`
  (callsigns) and a capacity cap (`0` = unbounded).
- Builder `with_chrono_replay(capacity)` returns `(Self, ChronoReplayState)`
  so the caller can inspect the snapshot independently of the boxed
  decoder.
- New `chrono_replay_snapshot()` accessor on the typed handle.
- New trait method `DecoderUnderTest::chrono_replay_snapshot_len() ->
  Option<usize>` for trait-object dispatch (default returns `None`;
  `Ft8Decoder` overrides to return the current deque length when
  stateful mode is on).
- `decode_wav` adds a new branch BEFORE the `rolling_window` branch:
  when `chrono_replay_state` is set, build `ApContext.recent_calls`
  from the persistent deque, run `decode_window_with_ap`, then push
  every from/to callsign from the new decodes into the deque (dedup
  against existing contents).

### `pancetta-research/src/bin/eval.rs` (extended)

- CLI flags: `--chrono-replay-enabled`, `--no-chrono-replay`,
  `--chrono-replay-capacity <N>`, `--chrono-replay-manifest <path>`.
- Tier dispatch: `chrono-replay` resolves to
  `research/corpus/curated/ft8/chrono_replay.manifest.json` by default;
  missing manifest is treated as a SKIP (matches wild-doppler-50 /
  hard-jt9-rich-200 conventions).
- New `run_chrono_replay_tier` function: same scoring shape as
  `run_curated_tier`, plus a snapshot-growth diagnostic log line at
  the end (`STATEFULNESS final snapshot=N callsigns,
  monotonic-growth=true, samples=K`).
- Auto-enable: when `chrono-replay` is in `--tier`, the decoder is
  constructed with `with_chrono_replay(0)` so the operator doesn't
  need to remember a redundant flag.

### `pancetta-research/src/bin/curate_chrono_replay.rs` (new)

- Scans `--source-dir` for `<session-prefix>*.wav` files.
- Lex sort = time sort (because filenames are `ft8_YYYYMMDD_HHMMSS.wav`).
- Takes a contiguous block `[skip..skip+slots)`.
- SHA-256 each WAV, parse timestamp from filename, emit manifest with
  span info.
- Does NOT generate jt9 baselines — the standard `baseline` binary
  (extended below) handles that, and the baselines cache by SHA so
  any overlap with hard-200 / wild-100 WAVs is free truth.

### `pancetta-research/src/bin/baseline.rs` (extended)

- Adds `chrono-replay` tier dispatch (loads via the new corpus loader).
- One-line change to the tier-match, one-line update to the
  bail-out error message.

### Manifest: `research/corpus/curated/ft8/chrono_replay.manifest.json`

Seeded from the operator's 2026-05-30 K5ARH session:
- Session prefix: `ft8_20260530_`
- Skip: 2 (the first two recordings at 152356 / 152358 are out-of-cadence
  warm-up; from 152413 onward the natural 15s slot cadence holds)
- Slots: 300
- Span: 4485 s ≈ 74.75 min ≈ 300 × 15 s (confirms slot-aligned)
- First WAV: 2026-05-30T15:24:13Z (slot_index 0)
- Last WAV:  2026-05-30T16:38:58Z (slot_index 299)

## Stateful semantics

A `chrono-replay` eval invocation:

1. Constructs ONE `Ft8Decoder` with `with_chrono_replay(0)` (unbounded
   deque).
2. Iterates `entries` in `slot_index` order.
3. For each WAV: builds `ApContext.recent_calls` from the current
   snapshot, runs `decode_window_with_ap`, pushes new callsigns
   (`from_callsign` ∪ `to_callsign`) into the deque.
4. The next WAV sees the accumulated snapshot. Statefulness is the
   binding contract — by design, the harness DOES NOT reset state
   between WAVs on this tier (and CONTINUES to reset on all other
   tiers).

The trait-level `chrono_replay_snapshot_len()` accessor lets the tier
log per-slot snapshot length and emit a single `STATEFULNESS …`
summary line at the end. A stateless tier returns `None` from this
hook; a stateful tier returns `Some(N)` and N grows monotonically.

## Smoke test

### Stand-alone trait verification (ran 2026-06-02 during iter)

A 3-WAV stand-alone driver verified that `chrono_replay_snapshot_len()`
grows monotonically across consecutive `decode_wav` calls against
real WAVs (`pancetta-ft8/tests/fixtures/wav/wsjt/{210703_133430,
181201_180245, 170709_135615}.wav`):

```
snapshot at start: Some(0)
slot 0: 13 decodes, snapshot len = 25
slot 1: 13 decodes, snapshot len = 47
slot 2: 1 decodes, snapshot len = 49
```

`(0 → 25 → 47 → 49)` proves the persistent deque accumulates across
consecutive WAVs (not reset between calls) and that
`chrono_replay_snapshot_len()` faithfully reports the deque length.
This is the binding statefulness assertion.

### Full eval-tier dispatch (300-slot K5ARH manifest)

The eval-tier driver is structurally identical to the curated tiers
plus the new `STATEFULNESS ...` summary line. End-to-end command:

```
cargo run --release -p pancetta-research --bin curate_chrono_replay -- \
  --source-dir ~/.pancetta/recordings \
  --output research/corpus/curated/ft8/chrono_replay.manifest.json \
  --session-prefix ft8_20260530_ --slots 300 --skip 2

cargo run --release -p pancetta-research --bin baseline -- \
  --tier chrono-replay --mode ft8

cargo run --release -p pancetta-research --bin eval -- \
  --tier chrono-replay --mode ft8 \
  --output research/scorecards/chrono-replay/smoke.json \
  --fp-filter-baselines research/baselines/ft8
```

The eval was launched during this iter; by commit time 42/300
baselines were cached (the rest populate via standard
`baseline --tier chrono-replay --mode ft8`; baselines are
SHA-keyed so re-runs are no-ops). The full eval-tier scorecard
will land in a follow-up commit once baseline coverage is complete.

## What this enables (intent)

Three SHELVED hypotheses can now be retested:

1. **hb-048 a7** (currently SHELVED-on-within-WAV) — the cross-slot path
   is what WSJT-X's a7 actually does. With `chrono-replay` providing
   slot N+1 audio + slot N callsigns in the snapshot, the eval can
   exercise the same mechanism as the WSJT-X mainline a7. Retest
   plan: cherry-pick the a7 production wiring + sweep against
   `chrono-replay` instead of `curated-hard-200`.
2. **hb-057 median-DT** — Session 1's diagnostic measured cross-WAV
   callsigns; Session 2's implementation hooked the wrong gate. With
   `chrono-replay`, the deque feeds `ApContext.recent_calls`, which
   is what AP sees — so a Session 3 attempt that hooks AP-time
   (where the candidate callsign IS known) can be retested against
   a corpus where the prior actually has population.
3. **hb-173 within-QSO** — implementation is preserved on
   `iter/2026-06-02-hb-173-session2`. Cherry-pick `b958603` + the
   snapshot-bridging code, then run the sweep on `chrono-replay`.
   The empty-snapshot gate that nullified Session 2 will no longer
   nullify.

NONE of those retests are part of THIS iter. This is infrastructure
only — no decoder behavior change.

## Files changed

- `pancetta-research/src/chrono_replay.rs` — new module (199 lines + tests).
- `pancetta-research/src/lib.rs` — module export.
- `pancetta-research/src/decoder.rs` — `ChronoReplayState`, builder,
  trait method, decode_wav branch.
- `pancetta-research/src/bin/eval.rs` — Args fields, parsing, tier
  dispatch, `run_chrono_replay_tier`, decoder-construction auto-enable.
- `pancetta-research/src/bin/curate_chrono_replay.rs` — new binary
  (~260 lines).
- `pancetta-research/src/bin/baseline.rs` — chrono-replay tier
  dispatch (small additive change).
- `research/corpus/curated/ft8/chrono_replay.manifest.json` — seeded
  manifest (300 slots, K5ARH 2026-05-30).
- `research/scorecards/chrono-replay/smoke.json` — smoke-test
  scorecard.

## Branch + commits

Branch: `iter/2026-06-02-chrono-replay-tier`

1. `feat(research): chronological-replay eval tier (stateful CrossTimeState across consecutive WAVs)`
2. `feat(research): curate_chrono_replay binary + 300-slot K5ARH 20260530 manifest`
3. `research(meta): chrono-replay tier shipped — unblocks hb-048/057/173 retests`

## Composes with

- hb-048 Session 3 (SHELVED 2026-06-02-on-within-WAV): the cross-slot
  retest can now be staged.
- hb-057 Session 2 (SHELVED 2026-06-02 at composite Δ=-0.000056):
  Session 3 reattempt hookable into `ApContext.recent_calls`.
- hb-173 Session 2 (SHELVED-PENDING-CHRONOLOGICAL-EVAL): the merge
  precondition has shipped.
- CrossTimeState (`pancetta-qso/src/cross_time_state.rs`, commits
  `20fdb0c`, `e86b264`) — the production cross-slot container.
  `ChronoReplayState` mirrors a SUBSET of it (the
  `a7_recent_calls` table) for the offline harness; a future iter can
  swap `ChronoReplayState` for an `Arc<CrossTimeState>` once
  pancetta-ft8 grows the dep (or once a thin POD wrapper crosses the
  crate boundary, per the within_qso pattern from hb-173 Session 2).

## Hard constraints

- SHIPPED-INFRA, NOT GRADUATED. No decoder behavior change at default
  config. The `chrono-replay` tier is opt-in (must be in `--tier`).
- All other tiers (hard-200, hard-1000, wild-N, synth-*) retain their
  existing stateless semantics — the chrono-replay state is ONLY
  enabled when the chrono-replay tier is requested.

## References

- hb-048 Session 3 SHELVE: `research/experiments/2026-06-02-hb-048-session3.md`
- hb-057 Session 2 SHELVE: `research/experiments/2026-06-02-hb-057-session2.md`
- hb-173 Session 2 SHELVE-PENDING: `research/experiments/2026-06-02-hb-173-session2-shelve-note.md`
- CrossTimeState: `pancetta-qso/src/cross_time_state.rs`
- hb-050 rolling-window AP (orthogonal sibling): `pancetta-research/src/decoder.rs::with_rolling_window`
