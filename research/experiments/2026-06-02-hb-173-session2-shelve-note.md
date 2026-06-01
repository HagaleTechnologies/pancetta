---
slug: hb-173-session2-shelve-note
mode: ft8
state: shelved-pending-chronological-eval
created: 2026-06-02T15:30:00Z
branch: iter/2026-06-02-hb-173-session2 (NOT merged to main)
parent_hypothesis: hb-173 (within-QSO context graph, Session 1 PROCEED 2026-06-01)
disposition: SHELVED-PENDING-CHRONOLOGICAL-EVAL — implementation correct but eval harness cannot test cross-slot mechanism
---

## Why this is a journal-only note, not a merged commit

The hb-173 Session 2 implementation IS correct and tested:
- `pancetta-ft8/src/within_qso.rs` — 608 lines, 12 unit tests pass
- `Ft8Decoder::within_qso_pass` wedged between hb-086 V1 (joint_pair_retry) and the SHELVED V3 block
- `Ft8Config::within_qso_enabled` (default false) and `within_qso_threshold_db` (default 6.0)
- 231/231 pancetta-ft8 lib tests pass with the implementation
- Eval CLI flags + builder methods plumbed

But on hard-200 eval (the production graduation bar):
- rec Δ = +0 / +0 / +0 across thresholds {5.0, 6.0, 7.0}
- Bootstrap CI 95% [+0.0, +0.0] on recall and novel
- The cross-slot snapshot is EMPTY on per-WAV decode (eval harness decodes WAVs in isolation)

**Same corpus-mismatch root cause as hb-048 Session 3 (SHELVED 2026-06-02):** mechanisms that depend on cross-slot context cannot be exercised by the existing eval harness which decodes each WAV independently. 80% of hard-200 sessions are single-slot per hb-173 Session 1's diagnostic.

## Why not merge the implementation anyway

Two options were considered:
1. **Merge with conflict resolution** (~12 additive-plumbing conflicts with hb-048 S3 + hb-057 S2 cherry-picks): ship dead-code infrastructure that's default-off and untestable until chronological-replay corpus exists.
2. **Defer the merge** (this note): keep implementation on the iter branch, document the SHELVE here. Re-merge when chronological-replay tier ships.

Chose (2) — Phase A discipline says don't ship code we can't test against composite. The implementation is preserved on `iter/2026-06-02-hb-173-session2` (branch + /tmp worktree) for the future iter that builds the cross-slot eval tier.

## Path forward

When chronological-replay eval tier lands (recommended by hb-173 S1, hb-048 S3, and the audit's "Open Question 3"):
1. Cherry-pick `b958603` + `9f5489f` from `iter/2026-06-02-hb-173-session2` onto main
2. Re-run sweep on chronological-replay tier (NOT hard-200's shuffled WAVs)
3. Apply Phase A label per bootstrap CI outcome

## Composes with

- hb-048 a7 Session 3 (SHELVED-on-within-WAV-path 2026-06-02): same fundamental eval-corpus mismatch.
- hb-057 Session 2 (SHELVED 2026-06-02): different root cause (wrong hook key), but also gated by future infra.
- CrossTimeState plumbing (SHIPPED-INFRA 2026-06-02): provides the substrate for all three.

## Commits preserved on the iter branch (not on main)

- `b958603` — feat(ft8): hb-173 — within-QSO context pair-conditional cross-correlation (config-gated)
- `9f5489f` — research(iter): hb-173 Session 2 — SHELVED-PENDING-CHRONOLOGICAL-EVAL at composite Δ=0 (95% CI [+0.0, +0.0])
