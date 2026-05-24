---
slug: batch-5-plumbing
mode: ft8
state: mixed (mostly won — infrastructure + bank refill)
created: 2026-05-25T00:00:00Z
last_updated: 2026-05-25T00:00:00Z
branch: iter/2026-05-25-batch-5-plumbing
parent_hypotheses: mr-002, jt9-slot-cut, doppler-tier, mr-004, hb-013
disposition: |
  5-iter plumbing batch. Cleans the pipeline for downstream work.
  - jt9 slot-cutting helper: works on slot-aligned WAVs; alignment
    problem for unaligned multi-slot WAVs spawned as hb-059.
  - Doppler tier wired into eval composite (5% weight slot now scoreable).
  - mr-004 source-drift audit: 2 new dead-flag findings (hb-060, hb-061).
  - hb-013 MIN_FREQ_BIN verify: confirmed at 0, shelved.
  - mr-002 JTDX audit: rich harvest of 5 candidate hypotheses
    (hb-054..hb-058), one of which is the headline JTDX sensitivity
    advantage (cross-cycle coherent averaging).
---

## Iter 1: mr-002 JTDX audit (background Explore agent)

Spawned at batch start; ran ~5 minutes. Agent applied mr-007
architecture-fit audit at harvest time.

### Headline finding from agent

**JTDX public source is frozen at 2.2.159 (April 2022).** v2.2.160 was
a beta-only release. The dev team halted public publishing. Recent
SourceForge uploads ("ALLCALL7_*") are callsign data files, not code.
So JTDX's `lib/ft8b.f90` (Feb 2022) and `lib/ft8_decode.f90` (Feb 2022)
are the canonical decoders — what's there is what we have. Rich harvest,
no future-version risk. Recommendation: pivot to mr-003 (academic LDPC
literature) next external-source run.

### Harvested hypotheses (5 candidates)

Renumbered from agent's hb-053..hb-057 (collision with existing hb-053
spawned in batch 4) to **hb-054..hb-058**:

- **hb-054**: Costas 2-of-3 sync rescore (sync8 segment fallback) —
  **clean attach** (~30 LOC). Aggregation rule change only:
  `max(syncf, syncs)` accepting the better of "all 3 Costas blocks"
  vs "trailing 2 blocks" — recovers signals with a corrupted leading
  Costas block.
- **hb-055**: Adaptive OSD depth based on signal context (ndeep 3→4→5
  near QSO/MyCall) — **clean attach**. Spend OSD effort where prior
  evidence says a real signal lives. CLI flag for gating logic.
- **hb-056**: Cross-cycle coherent symbol averaging (csold buffer) —
  **needs plumbing first**. The headline JTDX sensitivity advantage on
  repeating CQs. JTDX maintains `complex csold(0:7,79)` across cycle
  boundaries, coherently averages amplitudes when a CQ repeats at
  the same freq+DT. Pancetta has no cross-slot symbol cache; building
  it requires ~200-400 LOC + new buffer module + coordinator
  integration. Plan-sized item.
- **hb-057**: Median-filter DT averaging for sync/AP — **needs minor
  plumbing**. JTDX tracks median DT per QSO; pancetta would need
  per-callsign DT history first. Low value until pancetta's multi-pass
  comes back online.
- **hb-058**: `/R` and ARRL Field-Day false-decode filters —
  **clean attach**. Pure post-LDPC-CRC sanity rules; complements
  the FP filter MVP (hb-052 in bank). Wild-50 is the target corpus.

### Disposition

WIN (bank refill). 3 clean-attach candidates + 1 plan-sized item +
1 deferred-plumbing item. The architecture-fit audit at harvest time
(per mr-007) is doing exactly its job — caught hb-056 as needing
plumbing BEFORE it ate an iter slot.

---

## Iter 2: jt9 slot-cutting helper

### Implementation

`pancetta-research/src/decoder.rs`: `Jt9Decoder` gains `slot_cut: bool`
field + `with_slot_cut(on: bool)` builder. When enabled, `decode_wav`:
1. Loads the input WAV (12kHz mono).
2. Splits samples into 15-second chunks.
3. For each chunk, writes to a tempfile (zero-padded to exactly 15s)
   and runs jt9 on it.
4. Aggregates decodes with `dt_s` adjusted by slot offset.

`tempfile` promoted from `[dev-dependencies]` to `[dependencies]` (used
by the helper at runtime, not just in tests).

Refactored `decode_one_file` as a private helper that does the actual
subprocess invocation + output parsing; both the non-cut and cut paths
call it.

### Result

```
--- Hard-200 (multi-slot operator recordings; unaligned) ---
WAV                          | panc | jt9 raw | jt9 slot-cut
0: raw_decimated_12khz.wav  |   73 |       0 |            0
1: raw_subsampled_12khz.wav |   73 |       0 |            0
2: python_fir_decimated.wav |   72 |       0 |            0

--- Synth-clean (one slot per WAV; aligned) ---
WAV                          | panc | jt9 raw | jt9 slot-cut
   CQ_K1ABC_FN42__-14.0dB.wav   |    1 |       1 |            1
   CQ_K1ABC_FN42__-12.0dB.wav   |    1 |       1 |            1
   CQ_K1ABC_FN42__-10.0dB.wav   |    1 |       1 |            1
```

### Disposition

WIN (infrastructure) on slot-aligned WAVs. **Limitation:** hard-200's
operator recordings are NOT slot-aligned (they start at arbitrary times
within the FT8 schedule). My crude chunking at 0/15/30/45/60/75s offsets
gives chunks misaligned with actual slot boundaries — jt9 finds nothing.

**Spawned hb-059** to address this with slot-alignment detection
(sweep starting offsets 0-14s OR detect from spectral energy
patterns).

### Bug-fix during dev

First implementation used `chunk.len() < 12000 * 13` as the minimum-
chunk-size guard. Synth WAVs are exactly 12.64s (151680 samples) which
is < 156000, so they were skipped → 0 decodes. Lowered threshold to
12s (144000 samples). Fix verified.

---

## Iter 3: Doppler tier wired into eval composite

### Implementation

`pancetta-research/src/bin/eval.rs`: added `"synth-doppler"` arm in the
tier-match dispatch. Looks for the manifest at
`research/corpus/synth/manifests/doppler.manifest.json` (created in
batch 4), runs the standard `run_synth_tier` handler (same as
synth-clean since the manifest schema is identical).

Help-text updated to include `synth-doppler`.

### What was already in place

The composite scoring side (`metrics.rs::compute_composite`) was
already wired to read `synth-doppler` tier results — has been since
batch 1 of the harness build. Only the eval-CLI tier dispatch was
missing. `default_weights()` already had `snr_50pct_synth_doppler:
0.05`.

### Smoke result

```
$ cargo run --release -p pancetta-research --bin eval -- \
      --tier synth-doppler --mode ft8 --output /tmp/doppler.json

wrote scorecard: /tmp/doppler.json (composite 0.0000, 1 tier(s), 10.3s)

synth-doppler SNR@50%: None (decoder never reaches 50% recovery
                              under crude drift model)
by SNR:
  -20.0 dB: 0/36 decoded
  -18.0 dB: 0/36
  -16.0 dB: 0/36
  -14.0 dB: 1/36 (low drift only)
  ... (single-digit decodes at high SNR + low drift)
```

This is the honest signal hb-015 needs: current production decoder
fails on Doppler-drift signals (which is the entire point of hb-015 —
to test a Doppler-resilient sync search against this corpus).

### Disposition

WIN (infrastructure). Doppler tier is now scoreable. Future hb-015
work will use this as the metric.

---

## Iter 4: mr-004 source-drift audit

### Method

Per-field count of usage outside the struct-decl + Default + tests, for
each `Ft8Config` pub field.

### Findings

Two dead pub config fields, both mirrors of the
aggressive_decoding/min_snr_db pattern (hb-020/032 + hb-045/049):

- **`Ft8Config::enable_multithreading`** (decoder.rs:105). Set to true
  in Default (line 207). Set to false in `test_decoder_configuration_variants`
  (integration_tests.rs:205) and `single_thread` benchmark
  (decoder_benchmark.rs:135). Asserted in `test_ft8_config_default`
  (decoder.rs:3783). **Never read in the decode pipeline.** The
  parallel decode in `par_try_ap_decode` uses rayon unconditionally.
- **`Ft8Config::frequency_range`** (decoder.rs:114). Defaulted to
  200.0 (line 210). Set to 300.0 in `examples/enhanced_spectral_analysis.rs:26`.
  **Never read in the decode pipeline.** Actual frequency-range
  bounds (`MIN_FREQ_BIN..max_freq_bin`) are hardcoded in
  `costas_sync_search` (decoder.rs:1206).

Other 16 pub fields all have meaningful reads.

### Disposition

WIN (audit). Spawned cleanup hb entries:
- **hb-060**: remove dead `Ft8Config::enable_multithreading` field
- **hb-061**: remove dead `Ft8Config::frequency_range` field

Both mirror the hb-049 / hb-032 cleanup pattern (~7 referencing sites
each: field + Default + bench + test + assertion + maybe README).

### Source-drift cumulative count (4 dead-flag finds across audits)

| Iter | Found | Cleanup hb | Status |
|---|---|---|---|
| hb-020 (2026-05-21 audit) | aggressive_decoding | hb-032 | GRADUATED |
| hb-045 (2026-05-23 audit) | min_snr_db | hb-049 | GRADUATED |
| mr-004 (this iter) | enable_multithreading | hb-060 | pending |
| mr-004 (this iter) | frequency_range | hb-061 | pending |

---

## Iter 5: hb-013 MIN_FREQ_BIN verify

### Finding

`decoder.rs:82`: `const MIN_FREQ_BIN: usize = 0;`. Already at 0. The
"lower MIN_FREQ_BIN to expose decodes below 100 Hz" gap was closed at
some prior point. Confirmed.

### Disposition

SHELVE hb-013 as "already fixed". No code change.

---

## Batch 5 cumulative impact

- 5 iters completed.
- 1 graduation (mr-002 as a meta-research execution).
- 5 SHELVES / cleanups (hb-013 already-fixed; mr-002 / mr-004 / slot-cut
  helper / Doppler-tier as infra WINs that don't change production).
- 6 new hypotheses in bank: **hb-054** (Costas 2-of-3), **hb-055**
  (adaptive OSD depth), **hb-056** (cross-cycle averaging, plan-sized),
  **hb-057** (median DT), **hb-058** (FP filter rules),
  **hb-059** (slot-alignment detection), **hb-060** (remove
  enable_multithreading), **hb-061** (remove frequency_range).
- 8 net new bank entries from this batch (5 from mr-002 + 3 spawned
  from in-batch work).

**The bank is no longer "thinning" — it's now refilled with
architecturally-vetted hypotheses ready for batches 6-8.**

Production behavior unchanged.

## Workflow

Third batch under new discipline. Per-batch branch
`iter/2026-05-25-batch-5-plumbing`. mr-002 ran as background Explore
agent in parallel with foreground plumbing iters. Single push at
batch end. No data-loss incidents.
