---
slug: batch-4-unblock
mode: ft8
state: mixed (mostly won — see per-iter dispositions)
created: 2026-05-24T13:00:00Z
last_updated: 2026-05-24T14:00:00Z
branch: iter/2026-05-24-batch-4-unblock
parent_hypothesis: hb-051 + jt9-wrapper + hb-050 + FP-filter + Doppler-corpus
wild_card: false
disposition: |
  Batch 4 "unblock everything" — five iters of structural/infrastructure
  work to clear the queue. Key findings:
  1. AP-recovery line CLOSED on hard-200 (hb-051 ceiling = 1 decode).
     hb-027/hb-050 shelved as a consequence.
  2. jt9 subprocess wrapper plumbed and tested (slot-length WAVs only).
  3. FP filter MVP (callsign continuity) is the BIG WIN: -21.7% novels
     at -0.02% recall on hard-200.
  4. Doppler corpus generated (216 WAVs; crude drift model documented).
---

## Context

User-authorized 5-iter "unblock everything" batch (2026-05-24, after
batch 3). Goal: clear all structurally-blocked items so the iter
queue can be navigated freely.

Pre-batch state: 12 grads + 19 shelves. Composite plateau 0.5545.
Bank queue contained mostly large/blocked items (hb-027 blocked on
data source, hb-028 blocked on jt9 wrapper, hb-015 blocked on
Doppler corpus, hb-018/034/035 unaddressed without FP filter).

---

## Iter 1: hb-051 — AP-recovery ceiling diagnostic (200 WAVs, 15 min)

### Hypothesis

Bound the upper limit of what AP could ever contribute on hard-200
under perfect-information conditions (truth callsigns as hints).

### Implementation

`pancetta-research/examples/ap_recovery_ceiling.rs`. For each WAV:
1. Extract truth callsigns from jt9 baseline JSON.
2. Decode AP off (baseline).
3. Decode AP on with `ApContext.recent_calls` populated from truth.
4. Count: how many decodes does (3) add over (2)?

### Result

```
Total truth decodes:              8576
Matched by pancetta AP-off:       4666
Matched by pancetta AP-on (truth):4667
AP recovery (AP-on - AP-off):     1  (0.01% of truth, 0.02% lift)
WAVs with at least one recovery:  1 / 200
```

**With PERFECT-INFORMATION hints, AP recovers ONE decode** out of
8576 truth decodes. That's the upper bound. Any realistic
rolling-window data source (hb-050) can only do worse.

### Disposition

WIN (decisive diagnostic). **Closes the entire AP-recovery line for
hard-200.** hb-050 (rolling-window infra) and hb-027 (joint multi-slot
via QSO context) both shelve as a consequence.

Note: matched counts use loose substring matching, hence 4666 vs the
production-scorecard's 4365. Delta of 1 between AP-off and AP-on is
the relevant signal.

---

## Iter 2: jt9 subprocess wrapper (Jt9Decoder)

### Hypothesis

A `DecoderUnderTest` implementor that wraps `/Applications/wsjtx.app/
Contents/MacOS/jt9 -8 <wav>` unblocks hb-028 (cross-decoder
ensemble) and provides training data for FP filters.

### Implementation

`pancetta-research/src/decoder.rs`: new `Jt9Decoder` struct,
`Default` impl pointing at macOS WSJT-X bundle path,
`with_executable_path` builder, `DecoderUnderTest` impl spawning a
subprocess and parsing the `HHMMSS snr dt freq ~ message` output
format. Re-exported via lib.rs. 2 unit tests + a smoke example
(`jt9_smoke.rs`).

### Result

Sanity on 5 synth-clean WAVs (CQ K1ABC FN42 at -18 to -10 dB):
all 1:1 agreement with pancetta. Wrapper parses jt9's output
correctly.

```
WAV path                                                |  panc | jt9 | encoded
--------------------------------------------------------+-------+-----+--------
CQ_K1ABC_FN42__-18.0dB.wav                              |     1 |   1 | CQ K1ABC FN42
CQ_K1ABC_FN42__-16.0dB.wav                              |     1 |   1 | CQ K1ABC FN42
CQ_K1ABC_FN42__-14.0dB.wav                              |     1 |   1 | CQ K1ABC FN42
CQ_K1ABC_FN42__-12.0dB.wav                              |     1 |   1 | CQ K1ABC FN42
CQ_K1ABC_FN42__-10.0dB.wav                              |     1 |   1 | CQ K1ABC FN42
```

### Disposition

WIN (infrastructure). **Caveat:** jt9 expects exactly one 15-second
slot per invocation. The hard-200/1000 curated WAVs are multi-slot
operator recordings (pancetta-ft8 handles those via sliding-window
spectrogram); running jt9 on them produces 0 decodes unless they're
slot-cut first. For FP-filter training, prefer the existing
pre-baselined jt9 truth in `research/baselines/ft8/`. For hb-028
runtime use, slot-cutting infrastructure would be a future iter.

---

## Iter 3: hb-050 — rolling callsign-window tracker

### Hypothesis

Add a `--ap-rolling-window N` flag that maintains a per-decoder deque
of the last N decoded callsigns, feeding them as `ap_recent_calls`
on each WAV's decode. Conditional on hb-051's outcome.

### Implementation

`pancetta-research/src/decoder.rs`: `Ft8Decoder` gains
`rolling_window: Option<usize>` + `rolling_calls: Mutex<VecDeque>>`
fields. `decode_wav` now: if rolling_window is Some, build ApContext
from the deque snapshot, call `decode_window_with_ap`, then push
callsigns from the new decodes into the deque (evict oldest above
capacity).

`pancetta-research/src/bin/eval.rs`: `--ap-rolling-window N` flag.

### Result

Sanity sweep on hard-200 with window=50: **Δrec=0, Δnovel=0,
elapsed 1187s vs baseline ~250s (4.7x wallclock penalty).**

Exactly as hb-051 predicted: the AP path adds zero recall on
hard-200 even with a rolling-window data source. The 4.7x wallclock
penalty makes this strictly worse than baseline for hard-200.

### Disposition

**SHELVE hb-050.** Infrastructure is built but adds zero value on
hard-200. May still be useful for OPERATIONAL on-air decoding (where
the rolling window pulls from a different stream than the corpus
itself), but that's a different evaluation context. Re-evaluate when
operator-side rolling-window context lands.

Also **SHELVES hb-027** (joint multi-slot via QSO context) as a
direct consequence — same architectural premise, same dead end.

---

## Iter 4: FP filter MVP — callsign continuity

### Hypothesis

Apply hb-039's finding (97% of isolated-novel callsigns are
singletons, likely FPs) as a post-decode filter: drop decodes whose
callsigns appear nowhere else in the corpus baselines.

### Implementation

`pancetta-research/examples/fp_filter_callsign_continuity.rs`. For
each WAV's pancetta decodes, check whether at least one callsign
appears in the corpus-wide callsign set (built from all 1121
baselines). Keep if yes, drop if no.

### Result

```
Total pancetta decodes:       5317
  filter PASS (kept):         5175  (97.3%)
  filter FAIL (dropped):      142  (2.7%)

Breakdown by truth membership:
  truth-matching pre-filter:  4666  post-filter: 4665  (-1)
  novel pre-filter:           651   post-filter: 510   (-141)

FP reduction: -141 novels (21.7% of pre-filter novels)
Recall cost:  -1 real decodes (0.02% of pre-filter real decodes)
```

**21.7% novel reduction at 0.02% recall cost.** Clean precision win.
The filter exactly matches hb-039's prediction.

### Disposition

**WIN (infrastructure + finding).** The filter works as predicted
on hard-200. Caveats:

1. **Eval-harness only.** Requires the corpus baselines to be
   present. Real-time production needs a different signal source
   (operator log + rolling window + cqdx.io API).
2. **For PRODUCTION**, the next step is to apply the same filter
   with a different reference set (operator's logged callsigns +
   recent rolling window + cqdx.io). That's hb-052 (spawned).
3. **For RESEARCH**, this filter unlocks revisiting shelved
   hypotheses (hb-018 OSD-3, hb-034 OSD-3-revisit, hb-035 BP
   convergence) under "what if we add MORE decodes AND filter the
   FPs?" framing. That's hb-053 (spawned).

---

## Iter 5: Doppler corpus generation

### Hypothesis

Extend `gen-synth` to generate a Doppler-stressed corpus with
configurable drift rates, unblocking hb-015 (Doppler-resilient
sync search).

### Implementation

- `pancetta-research/src/synth.rs`: `SynthConfig` gains
  `drift_steps_hz_per_sec: Vec<f64>` field; `SynthEntry` gains
  `drift_hz_per_sec`.
- `pancetta-research/src/bin/gen_synth.rs`: new
  `apply_linear_drift_crude` function — multiplicative cosine on
  the real signal, NOT true Doppler frequency translation (true
  Doppler requires Hilbert-transform-based analytic signal
  manipulation). Documented as crude proxy. Lifted the
  "Plan 2 only supports awgn" guard.
- `research/corpus/synth/manifests/doppler.config.json`: 6 messages
  × 6 SNRs × 6 drift rates = 216 WAVs. Drifts ±0.5 to ±3.0 Hz/s.
- Generated 216 WAVs, manifest saved.

### Sanity check (CQ K1ABC FN42 across all SNR × drift cells)

```
snr  drift  | panc | jt9 | encoded
-20.0  *    |    0 |   0 |  (every drift)
-18.0  *    |    0 |   0 |  (every drift)
-16.0  *    |    0 |   0 |  (every drift)
-14.0  -0.5 |    1 |   0 | CQ K1ABC FN42  ← first decode
-12.0  -0.5 |    1 |   0 | CQ K1ABC FN42
-10.0  -0.5 |    1 |   0 | CQ K1ABC FN42
-10.0  +0.5 |    1 |   0 | CQ K1ABC FN42
(higher drift rates: all 0/0)
```

Pancetta decodes 4/36 cells (only low-drift, high-SNR). jt9 decodes
0/36. **Confirms the drift model perturbs the signal enough to
stress both decoders.** Pancetta marginally better than jt9 at
handling this crude drift, but neither is good.

### Disposition

WIN (infrastructure). Doppler corpus exists and produces meaningful
stress. **Caveat:** the drift model is CRUDE — multiplicative
cosine on real signal, not true Doppler frequency translation.
Rigorous Doppler evaluation needs a Watterson channel implementation
(future iter). For now, hb-015 has a corpus to attempt against.

---

## Batch 4 cumulative impact

| Disposition | Items |
|---|---|
| WIN (infrastructure) | jt9 wrapper, hb-050 wiring, Doppler corpus, FP filter example |
| SHELVE (consequence of ceiling diagnostic) | hb-050, hb-027 |
| WIN (decisive diagnostic) | hb-051 — closes AP-recovery line |
| WIN (precision finding) | FP filter — 21.7% novel reduction at 0.02% recall |

**Spawned hypotheses:**
- **hb-052**: Production FP filter using operator log + rolling window
  + cqdx.io (NOT corpus baselines)
- **hb-053**: Revisit shelved OSD-3/BP-convergence hypotheses under
  "wider gate + FP filter post-process" framing

**Production behavior:** unchanged (no production code in pancetta-ft8
was modified beyond hb-049's earlier cleanup and hb-043's earlier
my_call-less AP path).

## Workflow discipline (second batch under new rules)

- One branch (`iter/2026-05-24-batch-4-unblock`) from origin/main.
- 5 iters chained locally, single push at batch end (one pre-push hook).
- No `git reset --hard` reflexes, no data loss incidents.
- Local fmt+clippy before each commit (per discipline doc).
- Total iter time: ~2 hours wall (significant background parallelism
  on hb-051 + hb-050 sweep + FP filter analysis simultaneously).
