---
slug: localized-baseline-audit
mode: ft8
state: shelved
created: 2026-05-24T03:30:00Z
last_updated: 2026-05-24T03:30:00Z
branch: iter/2026-05-24-batch-3
parent_hypothesis: hb-045
wild_card: false
scorecard: (n/a — audit only)
delta_vs_main: 0 (no code change)
disposition: SHELVE hb-045 — technique doesn't apply to pancetta's architecture; pancetta already does per-candidate-local SNR estimation. Spawn hb-049 to remove dead `min_snr_db` config field.
---

## Hypothesis

hb-045 (spawned 2026-05-24 from mr-001): WSJT-X-Improved v3.1.0
ships "optimized baseline calculation, effective for FT4, FT2 and
FT8 STD." The 2019 WSJT-X mainline `lib/ft8/baseline.f90` change
"Improve FT8 SNR estimates in two ways" computes noise floor
per-window rather than globally — a single strong signal doesn't
drag the floor up across the whole band. Pancetta should benefit
similarly, especially on wild-50 (heterogeneous spectra, 0/96
jt9-overlap recovery today).

## Change

Audit-only — no code changes. Goal: understand where pancetta
computes the noise floor / SNR threshold today, then decide if the
WSJT-X technique would help.

## Audit findings

### Pancetta's current SNR estimation

`par_estimate_snr_spectrogram` (decoder.rs:3093) and
`par_estimate_snr_fft` (decoder.rs:3117) both compute SNR PER-
CANDIDATE. For each data symbol, they take the BEST tone magnitude
(signal) and the WORST tone magnitude (noise) across the 8 tones
of that symbol, average across data symbols.

This is ALREADY a local SNR estimate — and it's local to each
candidate AND each symbol within the candidate. The "global noise
floor dragged up by a single strong signal" failure mode that
WSJT-X-Improved's windowed-baseline fixes can't happen in pancetta's
current architecture because pancetta never uses a global noise
floor anywhere.

### The `estimate_noise_floor` function exists but is unused

`estimate_noise_floor` (decoder.rs:3181) computes a global median
power — but it's only called from a test (decoder.rs:3716). Not
wired into the decode pipeline.

### `Ft8Config::min_snr_db` is DEAD CODE

Field declared at decoder.rs:114, defaulted to `MIN_DECODE_SNR = -25.0`
at decoder.rs:215. Grepping for usages: zero reads anywhere in the
decode pipeline. The candidate selection is purely by
`sync_score` (Costas correlation), not by `snr_db`. Same pattern as
the recently-removed `aggressive_decoding` flag (hb-020 / hb-032).

### Where SNR is actually used

`snr_db` is computed per successful decode and reported in the
`DecodedMessage`. It's consumed downstream (UI display, ADIF
logging) but does NOT influence which candidates are tried or
which decodes are kept.

## Disposition

**SHELVE hb-045.** The WSJT-X-Improved "localized baseline"
technique solves a problem pancetta doesn't have. Pancetta's SNR
estimation is already per-candidate-local; there's no global
noise floor to "localize." The mr-001 source (audit of WSJT-X-
Improved release notes) didn't account for pancetta's different
architecture.

## Learnings

- **External-source hypotheses need a follow-up architecture audit
  before implementation.** WSJT-X-Improved fixes a specific failure
  mode (global noise floor + strong signal); pancetta's design
  avoids that failure mode by construction. The bank entry's
  evidence_for line was right about the technique but wrong about
  whether pancetta needs it.
- **Two consecutive iters in this batch (hb-042, hb-045) shelved as
  "surface-vs-actual gap" findings.** hb-042: score-cap is dual to
  count-cap. hb-045: WSJT-X's noise-floor problem doesn't exist in
  pancetta. Suggests the meta-research → harvest loop needs an
  architecture-fit check at harvest time before the entries land
  in the bank. Could be added as mr-007 ("audit harvested
  hypotheses against pancetta's actual architecture before
  promoting from candidate to active").
- **`Ft8Config::min_snr_db` is dead code (same as aggressive_decoding
  was).** Spawned [[hb-049]] to remove it; mr-004 (source-code drift
  audit) was supposed to catch these — running it now would catch
  this and possibly more.

## Follow-ups added to hypothesis bank

- **hb-045 → CLOSED (SHELVED).** Architecture mismatch.
- **hb-049 (NEW, priority 0.40):** Remove dead `min_snr_db` config
  field. Mirror of hb-032's `aggressive_decoding` removal — field
  declared and defaulted but never read in the decode pipeline.
- **mr-007 (NEW meta-research):** Architecture-fit audit of harvested
  hypotheses before promoting from candidate to active. Add to
  mr-001's procedure to prevent another hb-045-class miss.
- **mr-004 escalated:** the quarterly source-drift audit would have
  caught hb-049 (min_snr_db) at the same time as hb-032
  (aggressive_decoding). Worth running soon.

## Reproducing

```bash
grep -n "min_snr_db" pancetta-ft8/src/decoder.rs
grep -n "estimate_noise_floor" pancetta-ft8/src/decoder.rs
grep -n "par_estimate_snr_spectrogram" pancetta-ft8/src/decoder.rs
```
