---
slug: ldpc-iter-sweep-stale
mode: ft8
state: shelved
created: 2026-05-25T21:50:00Z
last_updated: 2026-05-25T21:50:00Z
branch: iter/2026-05-25-batch-11
parent_hypothesis: hb-011
wild_card: false
scorecard: n/a (stale-confirmation)
delta_vs_main: n/a
disposition: SHELVE hb-011 — already covered; LDPC_MAX_ITERATIONS is 100, the 25→50→100 sweep is done and shipped.
---

## Hypothesis

hb-011 (0.46): sweep `ldpc_iterations` 25 → 50 (WSJT-X jt9 uses 50);
expected +0.005..+0.02 synth sensitivity at low SNR. Premise:
`LDPC_MAX_ITERATIONS = 25`.

## Why it's stale

The premise is obsolete on every count:
- `pancetta-ft8/src/decoder.rs:51` now reads `LDPC_MAX_ITERATIONS = 100`,
  not 25.
- **hb-005** (2026-05-22) already swept the OSD-beta × iters grid and
  graduated iters 25 → 50.
- **hb-035** (2026-05-24) extended the sweep to {50, 75, 100}: iters=100
  gains +12 rec at +21 novel — deferred at the time on precision grounds.
- **batch 9 / hb-053** (2026-05-25) then shipped iters=100 once the
  production FP filter (hb-062) could absorb the extra novels.

So the 25 → 50 → 100 progression is fully explored and the production
default is already at the top of that range. There is nothing left for
hb-011 to test.

## Decision

**SHELVE** (already covered). No code, no eval.

## Learnings / follow-ups

- None. Bank hygiene: hb-011 predated the hb-005/hb-035 sweeps that
  superseded it; flagging it keeps the pending list honest. Worth a
  periodic pass to retire other pre-sweep hypotheses whose constants
  have since moved (mr-004 source-drift audit covers the config-field
  side of this).
</content>
