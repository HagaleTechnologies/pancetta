# Batch 76 — cross-day callsign trust (iteration 3)

## Phase A: trust DB from ft8_lib truth over all raw days

- truth files: 24864, distinct days: 12, unique callsigns: 5516
- trust set at K>=1: 5516 callsigns
- trust set at K>=2: 1343 callsigns
- trust set at K>=3: 369 callsigns
- trust set at K>=4: 129 callsigns
- trust set at K>=5: 40 callsigns

Days-per-callsign histogram (first 10):
- 1 day(s): 4173
- 2 day(s): 974
- 3 day(s): 240
- 4 day(s): 89
- 5 day(s): 26
- 6 day(s): 11
- 7 day(s): 2
- 8 day(s): 1

## Phase B: separation on 5/30 scan (leave-5/30-out trust)

Scan decodes: TP=37902, FP=17125 (pre-Batch-72 osd=Some(2) scan; 0 paths unmatched)

### K>=1 (trust set 3572 callsigns)

| Class | all_trusted | some_trusted | none_trusted | no_callsign |
|---|---:|---:|---:|---:|
| TP | 14136 (37.3%) | 14509 (38.3%) | 9223 (24.3%) | 34 (0.1%) |
| FP | 3686 (21.5%) | 3490 (20.4%) | 9712 (56.7%) | 237 (1.4%) |

Hypothetical gate 'reject if has callsigns and none trusted': kills 9712 FPs (56.7%), loses 9223 TPs (24.3%)

### K>=2 (trust set 700 callsigns)

| Class | all_trusted | some_trusted | none_trusted | no_callsign |
|---|---:|---:|---:|---:|
| TP | 4445 (11.7%) | 10187 (26.9%) | 23236 (61.3%) | 34 (0.1%) |
| FP | 1244 (7.3%) | 2463 (14.4%) | 13181 (77.0%) | 237 (1.4%) |

Hypothetical gate 'reject if has callsigns and none trusted': kills 13181 FPs (77.0%), loses 23236 TPs (61.3%)

### K>=3 (trust set 186 callsigns)

| Class | all_trusted | some_trusted | none_trusted | no_callsign |
|---|---:|---:|---:|---:|
| TP | 2390 (6.3%) | 6895 (18.2%) | 28583 (75.4%) | 34 (0.1%) |
| FP | 626 (3.7%) | 1471 (8.6%) | 14791 (86.4%) | 237 (1.4%) |

Hypothetical gate 'reject if has callsigns and none trusted': kills 14791 FPs (86.4%), loses 28583 TPs (75.4%)

### K>=4 (trust set 52 callsigns)

| Class | all_trusted | some_trusted | none_trusted | no_callsign |
|---|---:|---:|---:|---:|
| TP | 1842 (4.9%) | 3462 (9.1%) | 32564 (85.9%) | 34 (0.1%) |
| FP | 299 (1.7%) | 825 (4.8%) | 15764 (92.1%) | 237 (1.4%) |

Hypothetical gate 'reject if has callsigns and none trusted': kills 15764 FPs (92.1%), loses 32564 TPs (85.9%)

### K>=5 (trust set 15 callsigns)

| Class | all_trusted | some_trusted | none_trusted | no_callsign |
|---|---:|---:|---:|---:|
| TP | 983 (2.6%) | 1107 (2.9%) | 35778 (94.4%) | 34 (0.1%) |
| FP | 132 (0.8%) | 303 (1.8%) | 16453 (96.1%) | 237 (1.4%) |

Hypothetical gate 'reject if has callsigns and none trusted': kills 16453 FPs (96.1%), loses 35778 TPs (94.4%)


## Per-day truth density (root-cause data)

| Day | Slots | Truth decodes | Decodes/slot |
|---|---:|---:|---:|
| 20260419 | 1380 | 3828 | 2.77 |
| 20260420 | 489 | 7166 | 14.65 |
| 20260421 | 3 | 0 | 0.00 |
| 20260424 | 5063 | 3044 | 0.60 |
| 20260425 | 5760 | 0 | 0.00 |
| 20260426 | 5709 | 528 | 0.09 |
| 20260427 | 8 | 65 | 8.12 |
| 20260428 | 3768 | 4454 | 1.18 |
| 20260429 | 9 | 94 | 10.44 |
| 20260517 | 33 | 312 | 9.45 |
| 20260530 | 2066 | 39668 | 19.20 |
| 20260531 | 576 | 10810 | 18.77 |

## Verdict

1. **Hard gate: SHELVE.** Even at the loosest setting (K>=1, "callsign seen on
   any other day"), rejecting none-trusted decodes loses 24.3% of TPs to kill
   56.7% of FPs. At the plan's K>=3 it loses 75.4% of TPs. Not shippable as a
   gate at any K.
2. **Signal is real as a *feature*.** At K>=1: P(TP | all callsigns trusted) =
   79.3% vs P(TP | none trusted) = 48.7% — a ~30-point conditional-precision
   spread with 75% coverage of TPs. This is hb-103-v2 feature material
   (continuous trust score), not a boolean gate.
3. **Root cause of sparseness**: cross-day callsign overlap is low. 4173 of
   5516 unique callsigns (76%) appear on exactly 1 day. Only 3 days are dense
   (5/30, 5/31, 4/20 at 14-19 decodes/slot); 4/24-4/26 (16.5k slots, 66% of the
   corpus) are near-dead (<=0.6 decodes/slot). Band population churns across
   weeks; a trust DB built from 41 days of *this* corpus covers only ~38% of a
   dense day's decodes at K>=2.
4. **Side-signal**: messages with NO parseable callsign are 14x more FP-prone
   (FP 1.4% vs TP 0.1% of class). Consistent with hb-062.
5. **Caveat**: the 5/30 scan used the pre-Batch-72 default (osd_depth=Some(2)),
   so the FP population (17,125) is ~2.4x today's. With osd=Some(0) shipped,
   the absolute FP pool this feature could attack is much smaller.

Disposition: cross-day trust DB recorded as a feature candidate for hb-103 v2
(alongside decode_time_into_window from Diagnostic V) and as an AP-candidate
source for hb-237. No production change.
