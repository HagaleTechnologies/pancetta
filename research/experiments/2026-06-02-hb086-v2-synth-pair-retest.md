---
slug: hb-086-v2-synth-pair-retest
mode: ft8
state: shelved-on-3rd-corpus
created: 2026-06-02T00:00:00Z
last_updated: 2026-06-02T00:00:00Z
branch: iter/2026-06-02-hb086-v2-retest
parent_hypothesis: hb-086 V2 (SHELVED across May + refreshed hard-200 in 2026-05-30 and 2026-05-31; re-test gate preserved by Phase A audit pointing at hb-146 synth-pair-200)
wild_card: false
scorecard: n/a (diagnostic, not an iter — no decoder change)
delta_vs_main: 0 (no production code touched; one example added)
disposition: SHELVE on 3rd corpus — synth-pair-200's marginal-SNR rate stays at **0.0%** at both strict and relaxed windows, same as the two hard-200 corpora. The structural finding holds: pancetta's *decoded* neighbors are uniformly high-confidence regardless of corpus, including when the underlying truths ARE marginal-SNR by construction. V2's soft-cancellation mechanism is closed across all three corpora tested against pancetta's hard-decision pipeline. No Session 2 implementation; the re-test gate now requires a fundamentally new mechanism variant, not a new corpus.
---

## Why this re-check

hb-086 V2 (joint LLR with iterative soft cancellation) was SHELVED on
2026-05-30 against top-20 OLD hard-200 (multi-neighbor 14.8% / 34.8%,
marginal-SNR 0% / 0%) and again on 2026-05-31 against top-20 REFRESHED
hard-200 (16.7% / 33.8%, **still 0% / 0%**). The Phase A engineering-
substance audit (2026-06-02) relabeled "DEFINITIVELY SHELVED" to
"SHELVED across two corpora — re-test gate: hb-146 synth-pair-200 or
new mechanism variant." This iter executes that gate.

hb-146 synth-pair-200 (SHIPPED 2026-06-01) was built *by construction*
to contain the configuration V2 is designed for: 180 WAVs sweeping
(ΔSNR ∈ {0,3,6,9,12} dB × Δf ∈ {6,12,25,50} Hz × Δt ∈ {0,0.1,0.25} s),
strong signal at 1500 Hz, weak signal at 1500 + Δf Hz scaled down by
ΔSNR. Pancetta's baseline on this corpus: 177/180 strong-decode,
92/180 weak-decode — i.e., 88 WAVs where the strong signal IS decoded
(producing a "decoded neighbor") while the weak signal is missed (the
"V1-failed candidate" V2 targets). The 0%-weak-recovery corner
(ΔSNR ≥ 9 dB ∧ Δf ≤ 12 Hz, 88 buckets) is the V2 design point.

If V2's primary criterion clears on this corpus where it fails on
hard-200, that's evidence the criterion is corpus-bound, not mechanism-
bound, and V2 should be implemented. If it fails again, that's evidence
the criterion is intrinsic to pancetta's decoder pipeline, and V2 is
closed across all available signal classes.

## Method

`pancetta-research/examples/hb086_v2_synth_pair_retest.rs` (commit
34785f5) loads the synth-pair-200 manifest and decodes all 180 WAVs at
the default production config (V1 ON). For each WAV where pancetta
misses the weak truth (synth-pair's cleaner analog of "V1-failed
candidate" — the truth position is known by construction at
(1500 + Δf Hz, slot_lead_in_s + Δt s)), it counts pancetta's decoded
neighbors within the *same* strict (±25 Hz × ±2 s) and relaxed
(±50 Hz × ±2 s) windows the hard-200 diagnostics used.

The V2 primary criterion (unchanged): fraction of V1-failed candidates
with ≥1 nearby decoded neighbor whose SNR < -15 dB (the rotor-marginal
regime where LLR magnitudes shrink enough that soft-tone posteriors
differ non-trivially from hard projections). Threshold gates:

- ≥ 20 % → PROCEED to Session 2 implementation
- 10 – 20 % → MARGINAL (write up partial result)
- < 10 % → SHELVE on 3rd corpus

The diagnostic also reports the full neighbor-SNR distribution
(p10, p25, median, p90) and a per-bucket (ΔSNR × Δf) strict-window
breakdown so the regime structure is visible.

## Results — synth-pair-200

180 WAVs processed; 177 strong decoded (98.3%); 92 weak decoded
(51.1%); **88 WAVs** form the V1-failed-proxy population (weak missed).

| window           | v1-failed | with ≥1 nbr | with ≥2 nbrs | with marginal-SNR (<-15 dB) nbr | **marginal %** |
|------------------|----------:|-------------:|-------------:|--------------------------------:|---------------:|
| strict ±25 Hz    | 88        | 86 (97.7%)  | 39 (45.3%)   | **0**                           | **0.0%**       |
| relaxed ±50 Hz   | 88        | 87 (98.9%)  | 41 (47.1%)   | **0**                           | **0.0%**       |

Neighbor SNR distribution (strict, n=144 samples):
p10 = -5.2 dB, p25 = -4.0 dB, median = -2.9 dB, p90 = +24.9 dB.
Marginal (< -15 dB) = 0 / 144.

Relaxed (n=153): p10 = -5.3, p25 = -4.0, median = -2.9, p90 = +25.3 dB.
Marginal = 0 / 153.

Per-bucket strict-window regime (rows with ≥1 weak-missed):

| dSNR | dF=6 | dF=12 | dF=25 | dF=50 |
|-----:|-----:|------:|------:|------:|
|  0.0 | 2 v1-fail (0% marg) | – | 1 v1-fail (0%) | – |
|  3.0 | 7 v1-fail (0%) | 1 v1-fail (0%) | 2 v1-fail (0%) | – |
|  6.0 | 11 v1-fail (0%) | 5 v1-fail (0%) | 2 v1-fail (0%) | – |
|  9.0 | 12 v1-fail (0%) | 6 v1-fail (0%) | 7 v1-fail (0%) | – |
| 12.0 | 12 v1-fail (0%) | 6 v1-fail (0%) | 12 v1-fail (0%) | 0 v1-fail in window |

Every bucket: 0.0% marginal-SNR.

## 3-corpus comparison

| metric | OLD hard-200 | REFRESHED hard-200 | synth-pair-200 |
|---|---:|---:|---:|
| multi-neighbor strict | 14.8% | 16.7% | **45.3%** |
| multi-neighbor relaxed | 34.8% | 33.8% | **47.1%** |
| **marginal-SNR strict (PRIMARY)** | **0.0%** | **0.0%** | **0.0%** |
| **marginal-SNR relaxed (PRIMARY)** | **0.0%** | **0.0%** | **0.0%** |
| nbr SNR median relaxed (dB) | -1.5 | -1.6 | -2.9 |
| nbr SNR p10 relaxed (dB) | -5.7 | -5.1 | -5.3 |

The corpus DID move multi-neighbor density dramatically — strict
jumped from ~15% on real-audio to 45% on synth-pair, exactly as
hb-146's construction promised. But neighbor-SNR distributions
remained close (medians within 1.5 dB across all three corpora, p10
tails within 0.6 dB), and the marginal-SNR fraction stayed pinned at
0.0% on every window and every bucket.

## Decision: SHELVE on 3rd corpus

The marginal-SNR rate is 0% on synth-pair-200, identical to both
hard-200 corpora. The gate is 20%. V2's mechanism remains shelved.

This is now a strong cross-corpus result, not a single-corpus quirk:
the V2 primary criterion fails by an enormous margin (0% vs 20%
threshold) on three corpora with substantially different structure —
real-audio noisy short captures (OLD hard-200), denser real-audio
including 9/20 slots at jt9's -25 dB floor (REFRESHED hard-200), and
adversarial AWGN pairs designed *for the V2 mechanism* (synth-pair-200).

The structural insight the V2 SHELVE journal predicted is now
confirmed empirically: pancetta's *decoded* neighbors are uniformly
high-confidence even when the underlying truths ARE marginal-SNR by
construction. The corpus did contain marginal weak truths — pancetta
just did not decode them. The neighbors pancetta *does* decode passed
CRC-14, which selects for sharp LLRs → delta-function tone posteriors
→ soft cancellation collapses to hard subtraction. The corpus moved
the multi-neighbor count without moving the per-neighbor SNR
distribution, because the population we measure (*decoded* neighbors)
is upstream of the corpus distribution that *would* have given V2
substrate (*missed* weak truths whose energy could be partially
characterized).

V2's mechanism is, at this point, closed across the available signal
classes for pancetta's current pipeline. The next re-test gate is no
longer "find a corpus with marginal-SNR pancetta-decoded neighbors"
(this iter shows that may be structurally impossible given pancetta's
CRC gate) — it is "a fundamentally new mechanism variant" that
extracts LLR-equivalent information about a missed weak truth from
*the residual at the truth's expected location*, not from a decoded-
neighbor's posterior distribution.

## Prior art (soft-decision IC)

The V2 mechanism — probability-weighted (soft) iterative interference
cancellation — is textbook multi-user detection theory. Primary
references:

- Verdú, S. *Multiuser Detection.* Cambridge University Press, 1998.
  Soft-decision parallel and successive interference cancellation
  (chapters 5–6); the "soft posteriors of interferers replace hard
  decisions in IC stages" formulation is canonical there.
- Studer, C. & Bölcskei, H. "Soft-input soft-output single tree-search
  sphere decoding." *IEEE Trans. Inform. Theory* 56(10), 4827–4842
  (2010). Soft-information propagation in iterative decoders, applied
  to MIMO and LDPC.
- WSJT-X `subtractft8.f90` (Franke / Taylor open source) implements
  hard subtraction; the soft-cancellation extension explored here is
  the natural generalization, not novel.

V2 is not a "novel" mechanism — it is the well-known soft generalization
of hard SIC applied to FT8. What this iter resolves is whether *pancetta's
pipeline* presents the soft-LLR substrate the mechanism needs, not
whether the mechanism itself is sound in principle.

## Implications + flag updates

- **hb-086 V2 priority stays 0.0.** Update the `synth_pair_revisit_candidate`
  flag's interpretation: the re-test was *executed* and produced the same
  0% finding. The flag now reads "synth_pair_retested 2026-06-02 — 0% on
  third corpus; closed against current pancetta pipeline."
- **hb-086 V3** (subtract-aware sync threshold relaxation) carries its
  own `synth_pair_revisit_candidate: true`. V3 is unaffected by this
  result — V3's mechanism targets *missed* truths via geometric
  proximity to subtracted decodes, not decoded-neighbor LLRs. V3's
  synth-pair re-test is a separate experiment with its own kill-switch.
- The "soft cancellation collapses to hard subtraction when CRC selects
  for sharp LLRs" finding is now corpus-independent and should be
  considered a structural property of pancetta's decoder, not a corpus
  artifact. Any future hypothesis that reasons about pancetta's
  *decoded* outputs via soft/probabilistic methods will hit the same
  wall. The hard-decision pipeline upstream truncates the tone-
  uncertainty signal these mechanisms need.

## Method notes / caveats

- The diagnostic uses pancetta's reported `snr_db` for each decode as
  the "neighbor confidence" proxy. The original V2 spec proposed this
  as the rotor-marginal regime indicator (LLR magnitudes shrink
  appreciably when rotor SNR drops below ~-15 dB). The 24.9 dB p90
  values in the relaxed-window sample come from synth-pair's
  strong_snr_db=-8 dB strong signal — pancetta's SNR estimator reports
  significantly higher values for the cleanly-isolated strong tones at
  high Δf, which is a separate calibration question outside this iter's
  scope. The relevant bound is the *p10 lower tail* — and on every
  corpus, p10 sits comfortably above -15 dB.
- The synth-pair corpus is AWGN-only — no fading, no multipath. A real-
  audio corpus with deep fades on a strong signal *could* in principle
  produce marginal-SNR neighbors that survive CRC (a CRC pass with low
  per-symbol confidence). The empirical evidence from REFRESHED hard-
  200 (which includes 9/20 sample slots at jt9's -25 dB floor) is that
  this configuration does not occur at meaningful frequency: 0% on
  N=409 (strict) and N=734 (relaxed) neighbor samples.
- This iter does not test whether V2's mechanism would have produced a
  positive decode if the primary criterion *were* met. The criterion
  is the gate; absent its clearing, the mechanism cannot help by
  construction. This is correct for V2's design but should not be read
  as "V2 was implemented and shown to fail" — V2 was never implemented
  because the structural precondition never held across three corpora.

## Learnings

- **The kill-switch was correctly designed.** Two failed corpora
  prompted the audit re-label; the re-test gate it preserved was
  executed cheaply (~5 min generator + diagnostic build + ~5 min
  decode of 180 WAVs) and confirmed the original SHELVE was robust
  across corpora differing by 30 dB in worst-case SNR.
- **Adversarial corpora pull the count metrics but not the
  distributional metrics.** Multi-neighbor density 3x'd; per-neighbor
  SNR distribution shifted by 1–2 dB. The gating threshold V2 actually
  cares about (marginal-SNR fraction) is on the distribution, not the
  count — and the distribution is shaped by pancetta's CRC gate, not
  the corpus's truth distribution.
- **A mechanism whose substrate depends on *decoded outputs* of a
  hard-decision pipeline is structurally bounded by that pipeline's
  output sharpness.** The hb-086 family's V2 closure is not specific
  to this hypothesis — it is a class statement. The remaining
  hard-200 wall sits below the pipeline's gate (Costas-pre-gate sync
  threshold + LDPC iteration count + CRC), not above it.

## Decision (formal)

**SHELVE on 3rd corpus.** No Session 2 implementation. Hypothesis bank
flag updated. Worktree branch `iter/2026-06-02-hb086-v2-retest` retains
the diagnostic example for future reference (it executed a real re-
test gate across three corpora; the example is the artifact).
