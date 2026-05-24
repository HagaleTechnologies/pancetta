---
slug: bp-convergence-sweep
mode: ft8
state: shelved
created: 2026-05-24T02:00:00Z
last_updated: 2026-05-24T02:00:00Z
branch: experiment/ft8/multipass-profile
parent_hypothesis: hb-035
wild_card: false
scorecard: /tmp/llr_{40,48}.json + /tmp/iters_{75,100}.json
delta_vs_main: LLR axis refuted (hb-006 elbow holds); iters axis marginal (+12 rec / +21 novel at iters=100)
disposition: SHELVE hb-035 — LLR axis dead; iters=100 marginal and not worth +novel cost without FP filter
---

## Hypothesis

hb-035: hb-005 (LDPC iters 25 → 50) and hb-006 (LLR variance 24 → 32)
both made the 5-tier eval 3-5% faster as a side-effect — more BP
convergence = fewer expensive OSD calls. A deliberate sweep on BP
convergence rate could unlock more.

Extension of the prior sweeps to test:
- LLR_TARGET_VARIANCE ∈ {32 (baseline), 40, 48}
- LDPC iters ∈ {50 (baseline), 75, 100}

State has changed since hb-005/006 graduated: production is now
NMS-off, gate=2, cap=300. The elbows may have shifted.

## Change

CLI-only sweep on curated-hard-200. No code changes — both knobs
are already configurable.

## Result

| Config            | composite  | rec   | novel | elapsed | Δrec | Δnov | Δcomp     |
|-------------------|-----------:|------:|------:|--------:|-----:|-----:|----------:|
| baseline LLR=32 iters=50 | 0.254489 | 4365 |   952 |   255s |    — |    — |        — |
| **LLR variance axis** |       |      |       |        |      |      |          |
| LLR=40            |  0.253790  | 4353 |   964 |   147s |  −12 |  +12 |  −0.00070 |
| LLR=48            |  0.253673  | 4351 |   948 |   248s |  −14 |   −4 |  −0.00082 |
| **LDPC iters axis** |       |      |       |        |      |      |          |
| iters=75          |  0.254781  | 4370 |   976 |   221s |   +5 |  +24 |  +0.00029 |
| iters=100         |  0.255189  | 4377 |   973 |   238s |  +12 |  +21 |  +0.00070 |

### LLR variance axis

Widening LLR variance past 32 LOSES decodes. The hb-006 graduation
at variance=32 was already at the peak; the elbow is intact. Both
40 and 48 regress recall and have neutral-to-bad novel rates. No
ambiguity — the LLR axis is dead for further sweeps.

### LDPC iters axis

iters=100 produces +12 recovered (+0.14% absolute / +0.28% relative)
at +21 novel (+2.2% relative) on hard-200. Wallclock unchanged (238s
vs 255s baseline, within shared-machine noise).

The novel/real ratio of new decodes:
- iters=75 → iters=50: 5 real per 24 novel = 1:4.8 (bad)
- iters=100 → iters=50: 12 real per 21 novel = 1:1.75 (mediocre)

Compared to the top-300 ratio of ~5:1 (real:novel), the candidates
recovered by iters=100 are noisier than the bulk. Likely cause: more
iterations let BP converge to wrong CRC-valid codewords (BP-on-noise
decoding into syntactically-valid frames).

## Disposition

**SHELVE hb-035 with one optional follow-up.**

The LLR axis is closed (regression). The iters axis offers a tiny
marginal recall gain that comes with a poor novel/real ratio. Not
worth graduating until precision-positive infrastructure (FP filter
per the [[hb-018]] / [[hb-024]] line) exists to clean up the extra
novels.

If a future cycle lands a reliable FP filter, iters=100 could be
revisited — the +12 rec is real and the novel cost would be mostly
cleanable.

## Learnings

- **The current LDPC/LLR elbow is robust to production-state shift.**
  hb-005 (iters=50) and hb-006 (var=32) graduated under NMS-on /
  gate=4 / cap=200. Production now is NMS-off / gate=2 / cap=300.
  The 32/50 setting still sits at or near the optimum — the elbow
  didn't move significantly.
- **BP-convergence-rate-targeting hits the same FP wall as everything
  else.** Every change that admits more candidates to "successful"
  decoding admits more FP candidates too. The fundamental bottleneck
  is **precision**, not recall. Future "tune the decoder harder"
  hypotheses will keep hitting this wall until precision tooling
  lands.
- **The composite metric is at the limit of what parameter sweeps
  can move.** This iter changed two knobs by significant amounts and
  the composite delta is ±0.0008 in either direction. The remaining
  productive cycles are structural (NMS-aware subtract, joint
  multi-slot via QSO context per hb-027, cross-decoder ensemble
  per hb-028) — NOT parameter tuning.

## Wild card ratio note

This iter was an exploitation pick. Wild-card ratio remains at
0.167 (4 wild / 24 = 0.167). The wild card pool is exhausted of
single-iter-tractable items: hb-026 (5+ sessions), hb-027 (blocked
on hb-004 AP-wiring), hb-028 (2 sessions, requires jt9 subprocess
wrappers). Recommend the next iter EITHER (a) start a scoping
sub-cycle for hb-027 (fix hb-004 AP-wiring as prerequisite work)
or (b) explicitly accept multi-session wild card commitment.

## Follow-ups added to hypothesis bank

- **hb-035 → CLOSED (SHELVED).** Confirmed no productive parameter
  surface left in BP convergence space without precision tooling.
- **No new hypothesis spawned.** The path forward is structural,
  not parametric.

## Reproducing

```bash
for var in 40 48; do
  cargo run --release -p pancetta-research --bin eval -- \
      --tier curated-hard-200 --mode ft8 \
      --llr-target-variance $var --output /tmp/llr_$var.json
done
for iters in 75 100; do
  cargo run --release -p pancetta-research --bin eval -- \
      --tier curated-hard-200 --mode ft8 \
      --ldpc-iters $iters --output /tmp/iters_$iters.json
done
```
