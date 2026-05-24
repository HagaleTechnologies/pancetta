---
slug: passband-architecture-audit
mode: ft8
state: shelved
created: 2026-05-24T04:00:00Z
last_updated: 2026-05-24T04:00:00Z
branch: iter/2026-05-24-batch-3
parent_hypothesis: hb-047
wild_card: false
scorecard: (n/a — architecture-fit audit per mr-007)
delta_vs_main: 0 (no code change, no eval run)
disposition: SHELVE hb-047 — architecture audit shows minimal attach-point in pancetta's design. First test of mr-007 procedure.
---

## Hypothesis

hb-047 (spawned 2026-05-24 from mr-001): WSJT-X-Improved v3.1.0 ships
auto-tightened passband detection — clamps candidate search to the
detected transceiver passband for "better decoding performance and
fewer false decodes when Wide Graph limits are poorly set."

## mr-007 architecture-fit audit (NEW procedure — first test)

Per mr-007 (just spawned in iter 2 of this batch): before committing
an iter slot, audit the technique against pancetta's architecture.

### Where would the technique attach?

Candidate enumeration: `costas_sync_search` at decoder.rs:1208
iterates `for f0 in MIN_FREQ_BIN..max_freq_bin`. `MIN_FREQ_BIN = 0`
(hardcoded, line 85); `max_freq_bin` = `(4000.0 / pp.tone_spacing) as
usize` ≈ 640 bins (line 1204). So pancetta currently scans the FULL
0-4000 Hz audio passband for every WAV.

### Does pancetta have the failure mode the technique fixes?

WSJT-X-Improved's fix targets "fewer false decodes when Wide Graph
limits are poorly set" — i.e., scanning outside the actual audio
passband produces noise-decodes. Two-part audit:

1. **Speed bottleneck:** per the hb-021 profile, pancetta's pass-time
   distribution is preprocess+spectrogram+sync_search = 1.3% of pass
   time, LDPC = 55%, subtract = 43%. Narrowing the candidate search
   range would reduce sync_search time but **the sync_search isn't
   the bottleneck.** Maximum speedup ≈ 0.5% wallclock if we cut the
   search range in half.

2. **Precision bottleneck:** pancetta's precision wall (per hb-014/
   034/035/041) is that **candidates that DO surface ARE noise** —
   not that candidates from outside the true passband leak in. The
   cap (300) is what limits the candidate population; whatever
   passes the sync_score threshold gets sorted into the top 300
   regardless of which frequency bin it came from. Passband
   narrowing would only matter if there were so many noise
   candidates from outside the passband that they DISPLACED real
   candidates in the top 300. Given that pancetta's sync_score
   distribution past rank 300 is very flat (hb-033 saturation
   finding), even if passband narrowing dropped 100 noise candidates
   from the pool, the top-300 selection would be essentially
   unchanged.

3. **Corpus fit:** Wild-50 is the natural target (heterogeneous SDR
   captures). But wild-50 has 0/96 jt9-overlap recovery AND only 4
   novel decodes total. There's no measurable signal here for any
   technique to demonstrably improve.

### Verdict

The technique attaches at sync_search, but neither the speed nor the
precision impact is meaningful at pancetta's current operating point.
The most-favorable-case yield is <1% wallclock + indeterminate
precision gain on wild-50's tiny novel count.

## Disposition

**SHELVE hb-047 via mr-007 architecture-fit audit.** No code change,
no eval run. The technique solves a problem that's a tiny fraction
of pancetta's actual bottleneck.

This is the FIRST application of the mr-007 procedure (architecture-
fit check at audit time vs iter time). It saved an iter slot:
implementing a passband detector would have been ~100 LOC + an eval
to find the same finding.

## Learnings

- **mr-007 works.** Two of mr-001's six harvested hypotheses (hb-045,
  hb-047) shelve as architecture-mismatch. With mr-007 in place,
  these can be filtered at harvest time in future cycles, freeing
  iter slots for hypotheses that actually attach to pancetta's
  bottlenecks.
- **Pancetta's "where is the bottleneck" map is now clearer than
  WSJT-X-Improved's release-notes' implied target.** WSJT-X-Improved
  is optimizing a different system shape (Fortran pipeline, different
  pass scheduling, different cap+threshold structure). Mr-001's
  external survey is still valuable — it generated hb-044 (sub-sample
  DT, still pending), hb-046 (two-stage scheduling, still pending),
  hb-048 (a7 cross-correlation, still pending). But the rate of
  "directly portable" was lower than expected.
- **mr-002 (JTDX audit) is now more attractive.** JTDX is closer to
  pancetta in spirit (weak-signal focus, sensitivity over speed)
  than WSJT-X-Improved.

## Follow-ups added to hypothesis bank

- **hb-047 → CLOSED (SHELVED, via mr-007 audit).**
- **Recommend mr-002 (JTDX audit) as the next external-source
  harvest** — more architecturally similar to pancetta.
- **No new hypothesis spawned** beyond the mr-002 recommendation
  already in the bank.

## Reproducing

The audit is purely a read of decoder.rs + prior journals. No
commands to reproduce.
