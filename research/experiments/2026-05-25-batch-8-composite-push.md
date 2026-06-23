---
slug: batch-8-composite-push
mode: ft8
state: mixed
created: 2026-05-25T00:00:00Z
last_updated: 2026-05-25T00:00:00Z
branch: iter/2026-05-25-batch-8-composite-push
disposition: |
  Option A "push for composite movement" — 5 iters. Three big pieces
  of work + two hypothesis sweeps. Production filter library + cqdx +
  ADIF integration is DONE (10 unit tests pass) — coordinator hot-path
  wiring deferred to a future iter. mBP offset (hb-067) is a small
  precision finding (-48 novels at zero recall cost). hb-068 confirmed
  hb-044 fails for a different reason than hypothesized.
---

## Iter 1: hb-062 part 1 — pancetta-cqdx CallsignSpotsCache

### Implementation

`pancetta-cqdx/src/cache.rs`: added `CqdxCache::spotted_callsigns() →
HashSet<String>` returning uppercase callsigns from current
spot_groups + rarity_scores. + 1 unit test.

The existing CqdxCache already polls and stores spot data via
`update_spot_groups(...)` from the CqdxBridge — no new polling needed.
The new method just exposes the data to the FP filter.

### Disposition

WIN (infrastructure). 20 tests pass (was 19, +1 new).

---

## Iter 2: hb-062 part 2 — pancetta-qso CallsignContinuityFilter

### Implementation

New file `pancetta-qso/src/callsign_continuity.rs` — production-grade
FP filter:
- `CallsignContinuityFilter` struct with `static_ref: HashSet<String>`
  + `rolling: RwLock<VecDeque<String>>` + `cqdx_spotted: RwLock<HashSet<String>>`
- Sources:
  - `extend_from_iter(calls)` — test/admin
  - `extend_from_adif(path)` — operator log
  - `update_cqdx_spotted(set)` — periodic refresh from cqdx bridge
- Two construction modes:
  - `new(rolling_cap)` — strict from first decode
  - `new_lenient(rolling_cap, cold_start_threshold)` — accept all until
    `reference_size() ≥ cold_start_threshold`
- `accept(message) → bool` with interior mutability (Send + Sync via
  RwLock)
- `parse_adif_calls(text)` helper (extracted from MVP)

### Tests

10 unit tests covering: strict reject/accept, cqdx-source accept,
rolling-window grows via static match, lenient cold-start, lenient
activation, ADIF loading. All pass.

### Disposition

WIN (infrastructure). Production filter library is ready for
coordinator integration. Thread-safe via RwLock.

---

## Iter 3: hb-062 part 3 — integration helper + validation

### Implementation

Added `pancetta-qso::callsign_continuity::build_filter` convenience
constructor that takes (Option<&Path>, HashSet<String>, rolling_cap,
cold_start_threshold) and produces a configured filter. Handles
non-existent ADIF paths gracefully (no error).

Plus 3 more integration tests (combines, lenient mode, missing
ADIF). 10 → 13 callsign_continuity tests pass.

### Coordinator hot-path wiring DEFERRED

The FT8 decoder thread (`pancetta/src/coordinator/ft8.rs`) is on a
performance-critical path that runs every slot. Wiring the filter
there requires:
1. New config field `Ft8FilterConfig` in pancetta-config
2. Filter construction in coordinator startup (combining cqdx bridge
   + ADIF path)
3. Periodic cqdx-spot refresh hook from the cqdx bridge
4. Filter application between merge and broadcast loops at line ~178
5. Test on real WAV recordings end-to-end

That's its own iter (perhaps batch 9 iter 1) to do carefully. The
LIBRARY is ready and tested.

### Disposition

WIN (infrastructure complete). Production deployment of the filter
to the coordinator's decode pipeline is the natural next step but
deferred — needs FT8-thread testing.

---

## Iter 4: hb-067 mBP offset sweep

### Implementation

`pancetta-ft8/src/decoder.rs`:
- `LdpcDecoder` gains `bp_offset_subtract: f32` field +
  `with_bp_offset_subtract(v)` builder
- `Ft8Config::bp_offset_subtract: f32` (default 0.0)
- `DecodeContext.bp_offset_subtract` plumbed through to per-thread
  LDPC decoders
- In `decode_soft`, before invoking OSD: if `bp_offset_subtract > 0`,
  subtract the value from each LLR magnitude (preserving sign)

`pancetta-research` builder `with_bp_offset_subtract(v)` + eval CLI
`--bp-offset-subtract V`.

### Sweep result (curated-hard-200)

| Config | rec | novel | Δrec | Δnov |
|---|---:|---:|---:|---:|
| baseline (no offset) | 4365 | 952 | — | — |
| bp_offset=0.5 | 4365 | 949 |   0 |   −3 |
| bp_offset=1.0 | 4365 | 932 |   0 |  −20 |
| bp_offset=2.0 | 4365 | 920 |   0 |  −32 |
| **bp_offset=4.0** | **4365** | **904** | **0** | **−48** |

### Analysis

**Monotonic novel reduction at zero recall cost.** Small but real
precision win. bp_offset=4.0 gives -48 novels (-5.0%).

But the **mechanism is not what the paper described.** arXiv:2306.00443
claims the offset reduces BP confidence so OSD considers MORE flip
patterns (potentially +novels too). Instead pancetta sees -novels —
the offset is interacting with the parity-gate check (which uses the
offset-adjusted LLRs); larger offset → more "errors" detected →
parity gate kicks in MORE often → fewer OSD calls → fewer FPs.

So this is effectively a dynamic parity-gate tightening, not the
OSD-pattern-exploration mechanism the paper described.

### Disposition

**SOFT WIN (precision).** Graduate `bp_offset_subtract: 2.0` as
default? -32 novels at zero recall is real but small. Composite
unchanged (composite doesn't weight novels).

Decision: **don't graduate yet.** The mechanism mismatch suggests
we're getting a coincidental benefit, not the paper's intended
lever. May interact poorly with hb-014 parity-gate work or future
gate retuning. Worth documenting and keeping the flag available.

Spawn hb-067-followup: investigate the mechanism more carefully and
sweep against the parity_gate axis to confirm independence.

---

## Iter 5: hb-068 — hb-044 variant (no sort-score inflation)

### Hypothesis

hb-044's hard-200 regression (-116 recovered) was caused by refined
scores being higher than integer-bin scores, displacing better
candidates in the top-300 cap.

### Implementation

Modified `costas_sync_search` to compute the parabolic refinement
but ONLY use it as `time_refinement` for symbol extraction. Sort
score stays as the unrefined integer-bin `score`.

### Result

| Config | rec | novel |
|---|---:|---:|
| baseline | 4365 | 952 |
| hb-044 (sort by refined score) | 4249 | 925 (Δ−116) |
| **hb-068 (sort by integer-bin score)** | **4248** | **914 (Δ−117)** |

synth-clean SNR@90% gain preserved by both: −18 → −20 dB.

### Analysis

**The displacement hypothesis was wrong.** hb-068 produces essentially
the same hard-200 regression as hb-044 original (−117 vs −116).

The actual cause must be the **spectrogram interpolation itself**.
Linear interpolation in dB-space across the time axis perturbs
already-correctly-aligned candidates, even when the refinement
itself is small. Most hard-200 candidates have correct integer-bin
alignment; the fractional shift pushes the lookup slightly off the
peak and misses bits.

### Disposition

**SHELVE hb-068** alongside hb-044. Reverted the implementation to
the original (refined score for sort) since neither variant
graduates.

**Spawn hb-069**: try interpolation in LINEAR POWER space rather
than dB space. The spectrogram stores dB values; converting to
linear (10^(dB/10)) before interpolation, then back to dB, may
preserve symbol energies more accurately at fractional positions.
Higher CPU but might rescue the technique.

---

## Batch 8 cumulative impact

- **3 infrastructure WINs** (hb-062 parts 1+2+3): production FP filter
  library, ADIF parser, cqdx integration, build_filter helper.
  13 unit tests pass.
- **1 soft precision WIN** (hb-067 mBP offset): -48 novels at zero
  recall cost; not graduated due to mechanism mismatch.
- **1 negative finding** (hb-068): hb-044's regression is from
  spectrogram interpolation itself, not score-inflation. Spawn
  hb-069 for linear-space interpolation try.
- **Production behavior unchanged** — composite still 0.5545. The
  hb-062 library is ready but coordinator wiring is deferred to
  batch 9.
- 1 spawned hypothesis (hb-069).

**What's ready to ship after coordinator wiring (deferred to batch 9):**
- Production FP filter using operator-ADIF + rolling-window + cqdx
- Once shipped → hb-053 graduations (gate=6 + iters=100) apply
- Net expected impact: ~+0.0007 composite + ~-266 novels operationally
- Plus the mBP offset (-48 novels) if combined

**The Option A path forward:** batch 9 = coordinator-wire hb-062 +
ship hb-053 graduations + decide on bp_offset_subtract default.

Counters: exploitation_run 43 → 48, current_ratio 0.077.

## Workflow

Sixth batch under new discipline. Branch
`iter/2026-05-25-batch-8-composite-push`. Single push at batch end.
Local fmt before commits. No data-loss incidents.
