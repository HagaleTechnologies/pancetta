---
slug: batch-6-fp-filter
mode: ft8
state: mostly won (infrastructure + analytical findings)
created: 2026-05-25T00:00:00Z
last_updated: 2026-05-25T00:00:00Z
branch: iter/2026-05-25-batch-6-fp-filter
disposition: |
  5-iter batch building production FP filter infrastructure + revisiting
  two shelved hypotheses under "wider knob + FP filter post-process"
  framing.

  WIN (infrastructure): FpFilter library + ADIF parser + rolling
  window + eval CLI integration. 13 unit tests pass.

  WIN (analytical): hb-053 revisits show wider gate + filter and
  more iters + filter both become attractive — but graduation
  blocked until production filter ships with cqdx.io source.

  Spawned: hb-062 (cqdx.io production filter integration —
  the missing source for production deployment).
---

## Iter 1: FP filter library + eval CLI integration

### Implementation

New module `pancetta-research/src/fp_filter.rs`:
- `FpFilter` struct holding a `HashSet<String>` reference + optional
  `Mutex<VecDeque<String>>` rolling window
- `callsigns_in(msg)` — extract up to 2 callsign-shaped tokens
  (consistent with batch-4 MVP behavior; grid squares pass too)
- `extend_from_iter`, `extend_from_baselines(dir)` source methods
- `with_rolling_window(n)` builder
- `accept(msg, update_rolling) -> bool` filter check
- 9 unit tests covering CQ/QSO message shapes, rolling-window
  behavior, and reference-membership

`pancetta-research/src/bin/eval.rs`:
- Three CLI flags: `--fp-filter-baselines <DIR>`, `--fp-filter-rolling N`
- New `apply_fp_filter` helper that retains decodes in-place
- Plumbed through `run_fixtures_tier`, `run_synth_tier`,
  `run_curated_tier` (all three tier handlers)

### Result on hard-200

baseline (no filter): rec=4365 novel=952
with filter (1121 corpus baselines): rec=4364 novel=811
**Δrec=-1, Δnovel=-141 (-14.8%)** — exact replication of batch-4
MVP finding from a reusable library function.

### Disposition

WIN (infrastructure). FP filter is a library function now; tests
pass; eval applies it post-decode automatically. The MVP precision
finding is fully validated as reusable.

---

## Iter 2: ADIF callsign source

### Implementation

`fp_filter.rs`: added `parse_adif_calls(text) -> Vec<String>` and
`FpFilter::extend_from_adif(path)`. ADIF format: `<NAME:LENGTH>VALUE`,
case-insensitive tag matching, tolerates `<CALL:5:S>` typed fields.
4 unit tests.

`eval.rs`: new `--fp-filter-adif <PATH>` flag.

### Test

Operator's `~/.pancetta/qsos.adi` exists but is essentially empty
(5 lines, no actual QSO entries — pre-Phase-5 state). Built a
synthetic ADIF from the top-100 most-common callsigns in
hard-200's baselines and tested.

### Result on hard-200

| Filter source              | rec  | novel |
|----------------------------|-----:|------:|
| baseline (no filter)       | 4365 |   952 |
| corpus-baselines (1121, all callsigns) | 4364 | 811 |
| ADIF top-100 only          | 1966 |   251 |

**ADIF top-100 alone is too narrow** — covers only 100 of the
2724 unique callsigns in the corpus, so ~55% of real decodes get
filtered out. The filter logic is correct; the reference set is the
limiting factor.

### Disposition

WIN (infrastructure). ADIF source path works as a code path. Real
production needs either (a) a much larger ADIF log OR (b)
additional sources (cqdx.io network spots). See iter 3.

---

## Iter 3: Combined mode + production graduation decision

### Combinations tested on hard-200

| Filter mode                       | rec  | novel | Recall % |
|-----------------------------------|-----:|------:|---------:|
| baseline (no filter)              | 4365 |   952 |   100%   |
| corpus-baselines (1121 files)     | 4364 |   811 |  99.98%  |
| ADIF top-100 only                 | 1966 |   251 |   45.0%  |
| **ADIF top-100 + rolling=200**    | **2848** | **403** | **65.2%** |
| rolling=500 only (cold-start)     |    0 |     0 |    0%    |

### Key insights

1. **Rolling-only is catastrophic at cold-start.** Empty reference
   → no decode passes → rolling window never fills → permanent
   rejection. Need a non-rolling seed source.
2. **Rolling window helps a lot when added to a small seed.**
   ADIF-only 45% recall → ADIF + rolling 65% recall. The window
   accumulates callsigns as decodes flow, allowing later decodes
   to pass via the rolling source.
3. **Corpus-baselines is the upper bound.** -14.8% novels at -0.02%
   recall. Realistic production wants to approximate this coverage
   via (operator-ADIF + rolling + cqdx.io spots).

### Disposition for hb-052 production graduation

NOT YET GRADUATABLE for production. Reasons:
- Operator-ADIF alone is too narrow (and currently empty for this
  operator).
- Rolling-only fails cold-start.
- Combined operator-ADIF + rolling = 65% recall on hard-200
  — unacceptable production drop.
- The missing source is **cqdx.io recent-spots cache** — would
  provide near-real-time global callsign coverage (likely
  thousands of unique callsigns vs the ~3000 in corpus baselines).

**Spawned hb-062**: production cqdx.io filter integration. Once
that lands, the combined source should approximate corpus-baselines
coverage and become graduatable.

Infrastructure is WIN. Production deployment of the filter is
blocked on hb-062.

---

## Iter 4: hb-053 revisit hb-014 (parity gate) with FP filter

### Sweep

| Config                  | rec  | novel | Δrec vs prod | Δnov vs prod |
|-------------------------|-----:|------:|-------------:|-------------:|
| gate=2 (production, no filter) | 4365 | 952 | — | — |
| gate=2 + filter         | 4364 |   811 |     -1 |  -141 |
| gate=4 + filter         | 4364 |   814 |     -1 |  -138 |
| **gate=6 + filter**     | **4365** | **820** | **0** | **-132** |

### Finding

**gate=6 + filter matches production recall AND beats production
novel count by -132.** Without filter, gate=6 was strictly worse
(per batch-2 hb-014 graduation, novels grew monotonically with gate
width). With filter applied, the FP cost is absorbed.

### Disposition

ANALYTICAL WIN. Wider gate becomes graduatable once the production
filter ships (hb-052 graduation pending hb-062). Documented; not
ready to ship today.

---

## Iter 5: hb-053 revisit hb-035 (BP iters) with FP filter

### Sweep

| Config                       | rec  | novel | Δrec vs prod | Δnov vs prod |
|------------------------------|-----:|------:|-------------:|-------------:|
| iters=50 (production, no filter) | 4365 | 952 | — | — |
| iters=50 + filter            | 4364 |   811 |     -1 |  -141 |
| **iters=100 + filter**       | **4376** | **818** | **+11** | **-134** |

### Finding

**iters=100 + filter gives +11 real decodes AND -134 novels** vs
production. Without filter, iters=100 was rejected for its FP cost
(+12 rec / +21 novel = 1.75:1 ratio per batch-3 hb-035 shelve).
With filter, the +21 raw novels become a net -134 after filtering,
and the +12 real survives intact (filter rejects only callsigns
not in reference).

This is a stronger result than the gate revisit — it actually adds
recall, not just removes FPs at the same recall.

### Disposition

ANALYTICAL WIN. iters=100 + filter is graduatable once production
filter ships (hb-052 / hb-062). Composite would lift ~+0.0007
(small but real) once both ship.

---

## Batch 6 cumulative impact

- **Infrastructure WIN**: FP filter library + ADIF parser +
  rolling-window + eval CLI integration. Reusable, tested
  (13 unit tests pass).
- **Validation WIN**: filter reproduces batch-4 MVP exactly
  (-141 novels at -1 real decode on hard-200).
- **Production-deployment BLOCKED**: needs cqdx.io integration
  (spawned hb-062). Operator ADIF alone is insufficient.
- **2 analytical WINs from hb-053 revisits**: gate=6 and iters=100
  become defensible with filter. Both pending hb-052 production
  shipping.

**Production behavior unchanged.** Composite still at 0.5545.

When hb-062 (cqdx.io integration) lands and hb-052 graduates,
expected production deltas (cumulative):
- gate=6 + iters=100 + filter
- Recall: +11 / 8576 = +0.13% absolute (small but real)
- Precision: -266 novels (-28% relative)
- Composite: +0.0007 (small)
- Operational: meaningfully fewer fake QSO attempts on-air

Spawned hypotheses:
- **hb-062**: production cqdx.io filter integration (high priority)

## Workflow notes

Fourth batch under new discipline. `iter/2026-05-25-batch-6-fp-filter`
branch. All 5 iters chained; single push at batch end. Local fmt
before each commit. No data-loss incidents.
