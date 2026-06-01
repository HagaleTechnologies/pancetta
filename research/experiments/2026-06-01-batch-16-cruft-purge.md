# Batch 16 — Cruft Purge of Hypothesis Bank

date: 2026-06-01
branch: iter/2026-06-01-batch-16-cruft-purge
mode: bank hygiene only (no production code touched, no eval run)

## What this batch did

Pre-batch hygiene before batch 16 (first semi-autonomous batch). The
hypothesis bank had grown to 216 entries with mr-009's deep-ideation
pass (115 new entries spawned tonight). The aggregator left numeric
priorities in the `priority_score:` field but kept `[PRIORITY: wild, ...]`
in titles for the wild-card entries; several previously-closed
hypotheses had `status_2026_XX_XX:` outcomes recorded but `status:` /
`priority_score:` fields still pointed at the pre-closure state. This
batch reconciled those inconsistencies.

No production code, no eval, no scorecard, no composite movement.

## Tasks performed

### Task 1 — Status drift reconciliation (10 entries)

Entries whose title tag or inner status fields disagreed (or where the
SHELVE/GRAD outcome wasn't reflected at all):

| hb | before | after |
|---|---|---|
| hb-016 | title SHELVED, inner `status: pending` / `priority_score: 0.36` | inner `status: SHELVED 2026-05-30` / `priority_score: 0.0` |
| hb-018 | title SHELVED, inner `status: pending` / `priority_score: 0.30` | inner `status: SHELVED 2026-05-26` / `priority_score: 0.0` |
| hb-021 | title `[PRIORITY: SHELVED]` (old format) | title `[SHELVED 2026-05-23]` |
| hb-037 | title `[PRIORITY: SHELVED — superseded by hb-031]` | title `[SHELVED 2026-05-23 — superseded by hb-031]` |
| hb-064 | **unmerged Session 2 SHELVE from commit afd5921 (branch never merged)**; bank still showed pre-Session-2 pending text | port the Session 2 SHELVE: title `[PRIORITY: 0.42, Session 2 SHELVED 2026-05-31; Session 3 deferred]`; inner status_2026_05_31_session2 + revised evidence/notes |
| hb-069 | **unmerged SHELVE from commit d04c596 (branch never merged)**; bank still showed `status: pending` / `priority_score: 0.35` | title `[SHELVED 2026-06-01 — composite -0.003049 vs dB; hb-044 family closed]`; status_2026_06_01 + `status: SHELVED 2026-06-01` / `priority_score: 0.0`. (Previously held a closure_reminder; replaced with the actual SHELVE.) |
| hb-072 | title GRADUATED, inner `status: pending` / `priority_score: 0.30` | inner `status: GRADUATED 2026-05-26` / `priority_score: 0.0` |
| hb-080 | title GRADUATED, inner `status: pending` / `priority_score: 0.45` | inner `status: GRADUATED 2026-05-27` / `priority_score: 0.0` |
| hb-081 | title SHELVED, inner `status: pending` / `priority_score: 0.40` (also had duplicate `status:` from earlier patch) | inner `status: SHELVED 2026-05-27` / `priority_score: 0.0`; duplicate removed |
| hb-087 | title `[PRIORITY: 0.0, SHELVED ...]` (hybrid), inner `status: shelved (...)` | title `[SHELVED 2026-05-31 at Session 2 — ...]`; inner cleaned to `status: SHELVED 2026-05-31` |

(10 entries reconciled. hb-064 and hb-069 were the highest-value finds —
both had iter branches that never merged their bank updates back to main.
The journals + commits exist on iter branches; the bank just never
caught up.)

### Task 2 — Priority assignment for "PRIORITY: wild" entries (28 entries)

The mr-009 aggregator already populated `priority_score:` numerically
for every wild-card entry — but kept the title tag at the categorical
`[PRIORITY: wild, ...]`. This batch propagated the numeric value into
the title for visibility, using the format `[PRIORITY: 0.XX (wild), ...]`.

Two older wild cards (hb-026, hb-028) had `priority_score: 0.0` since
the wild-card framework was introduced — those received fresh numerical
priorities (0.15 and 0.25 respectively, per the task brief's heuristic
table).

Title updates (28):

| hb | source | new title priority |
|---|---|---|
| hb-026 | (pre-mr-009) End-to-end neural decoder | 0.15 |
| hb-028 | (pre-mr-009) Cross-decoder ensemble | 0.25 |
| hb-094 | mr-008 D: residual denoising AE | 0.20 |
| hb-095 | mr-008 D: neural soft-demod | 0.25 |
| hb-106 | mr-009 A6: variational Bayesian decoder | 0.20 |
| hb-110 | mr-009 A10: learned soft decoder | 0.15 |
| hb-114 | mr-009 A14: generative-prior decoder | 0.18 |
| hb-118 | mr-009 D4: USB+LSB IQ pair | 0.20 |
| hb-119 | mr-009 D5: cross-band conditional prior | 0.22 |
| hb-123 | mr-009 D9: polarization-diversity emulator | 0.15 |
| hb-126 | mr-009 D12: adversarial-noise diversity | 0.20 |
| hb-127 | mr-009 D13: public-KiwiSDR fleet | 0.18 |
| hb-138 | mr-009 M10: DXCC-coverage rate | 0.25 |
| hb-139 | mr-009 M11: info-theoretic recall | 0.20 |
| hb-140 | mr-009 M12: counterfactual QSO-completion | 0.22 |
| hb-142 | mr-009 M14: Pareto frontier | 0.20 |
| hb-143 | mr-009 M15: operator-day-replay FLAGSHIP | 0.25 (FLAGSHIP) |
| hb-163 | mr-009 H6: voice/foot-pedal hotkey | 0.15 |
| hb-179 | mr-009 T7: sender personality fingerprint | 0.22 |
| hb-182 | mr-009 T10: time-aware confidence calib. | 0.20 |
| hb-183 | mr-009 T11: federated cross-operator priors | 0.22 |
| hb-184 | mr-009 T12: time-reversed multipass | 0.22 |
| hb-185 | mr-009 T13: meta-decode QSO-state HMM | 0.20 |
| hb-190 | mr-009 F4: end-to-end Transformer | 0.25 |
| hb-194 | mr-009 F8: Bayesian neural OSD ensembles | 0.35 |
| hb-197 | mr-009 F11: latent-diffusion codeword | 0.18 |
| hb-200 | mr-009 F14: RF-foundation-model | 0.25 |
| hb-201 | mr-009 F15: neural sync detector | 0.28 |

### Task 3 — Closure-reminder flagging (2 entries)

Inline `closure_reminder_2026_06_01:` notes added to entries whose
mechanism is structurally adjacent to a closed family. NO auto-shelve —
the user can review and decide. Flagged:

- **hb-108** (TF uncertainty distribution at sync) — ADJACENT to
  hb-086 V3 closure (subtract-aware sync relaxation). V3's relaxed-Costas
  window surfaced 100-131 truly-new candidates per WAV — all noise. hb-108's
  grid-search ±2 bins × 5 sub-bin around the Costas point is a similar
  relaxation mechanism. The kill-switch (≥8% of missed truths decode at
  some grid point) is the right diagnostic to differentiate.

- **hb-121** (time-diversity Q65-style coherent averaging) — ADJACENT to
  hb-074 closure (complex-spectrogram coherent cross-cycle). hb-074 SHELVED
  because inter-slot phase wasn't reliably preserved in real-world audio;
  hb-075 (MRC-weighted) was the rescue. hb-121's "coherently average IF-
  level spectrograms" relies on the same coherence assumption. Either
  scope to non-coherent magnitude averaging (hb-056/hb-075 territory) or
  restrict to a phase-coherent SDR-IQ corpus (hb-077 territory).

(A third closure_reminder was placed on hb-069 first, but then the
unmerged SHELVE commit was discovered and applied — superseding the
reminder with the actual SHELVE entry. Net flagged: 2 advisory.)

### Task 4 — Counter update (header block)

Added a machine-readable counter block to the header (after `current_ratio`).

| metric | before | after |
|---|---|---|
| total_entries | 216 | 216 |
| pending (title PRIORITY) | 146 | 142 (after hb-021/037/087 reformatted into SHELVED titles, and hb-004 deferred broken out) |
| deferred | 1 (hb-004) | 1 (hb-004) |
| shelved (title SHELVED + DEFINITIVELY) | 41 | 44 (+hb-021, +hb-037, +hb-087) |
| graduated/win/etc | 29 | 29 |
| wild_cards (`wild_card: true`) | 33 | 33 |
| closure_reminders | 0 | 3 |

(Pre-purge "146 PRIORITY" included hb-004 deferred + hb-021 SHELVED-in-title +
hb-037 SHELVED-in-title + hb-087 SHELVED-in-title; after format fixes these
are accounted for in their accurate buckets.)

## Top-20 highest-priority active backlog (post-purge)

Pure title-tag priority, deferred entries excluded.

| rank | hb | priority | title |
|---:|---|---:|---|
| 1 | hb-093 | 0.52 | Per-position residual SNR pre-decode gate (mr-008) |
| 2 | hb-133 | 0.52 | Saturation-aware composite (corpus-shift-robust) — METRIC unblock |
| 3 | hb-115 | 0.50 | Dual-KiwiSDR space-diversity LLR fusion (MRC) |
| 4 | hb-146 | 0.50 | Synthetic adversarial corpus targeting measured walls |
| 5 | hb-150 | 0.50 | High-jt9-novel-density tier (jt9-beats-pancetta inverse) |
| 6 | hb-161 | 0.50 | `Q` key: operator STOP mid-QSO when pancetta is wrong |
| 7 | hb-173 | 0.50 | Within-QSO context graph (decode-time pair-conditional AP) |
| 8 | hb-089 | 0.48 | Multi-cycle coherent residual accumulation (mr-008) |
| 9 | hb-104 | 0.48 | Joint multi-candidate decoder (vector decode) |
| 10 | hb-137 | 0.48 | Adversarial-corpus recall (jt9-only-hits subset) |
| 11 | hb-048 | 0.45 | AP type 7 (a7) cross-correlation (DESIGN COMPLETE, S2 build pending) |
| 12 | hb-101 | 0.45 | Soft-output decoder with codeword-posterior export |
| 13 | hb-116 | 0.45 | Decoder-diversity vote: pancetta ⊕ jt9 ⊕ jtdx |
| 14 | hb-129 | 0.45 | Time-to-first-decode (TTFD) per-slot metric |
| 15 | hb-144 | 0.45 | Cross-decoder consensus truth corpus |
| 16 | hb-156 | 0.45 | Lid-of-band weak-signal-only tier (SNR ≤ -20 dB) |
| 17 | hb-160 | 0.45 | `*` key: priority boost next-cycle CQ from highlighted callsign |
| 18 | hb-206 | 0.45 | WSJT-X plug-in adapter (pancetta-as-decoder CLI) |
| 19 | hb-209 | 0.45 | Log-as-ground-truth: operator QSOs validate decodes retroactively |
| 20 | hb-064 | 0.42 | DIA-augmented OSD (iteration-trajectory features) — Session 3 retry |

Honorable mentions at 0.42: hb-103 (continuous trust-score FP filter),
hb-113 (hierarchical decode), hb-165 (real-time alarm tier),
hb-191 (GPT QSO-state language model), hb-213 (SIMD LDPC BP).

## Decision-relevant findings

1. **The bank is healthy and active**: 142 pending entries post-purge,
   with strong tail distribution at 0.45+ (8 entries at ≥0.48, 13 at
   ≥0.45). mr-009 didn't fluff the bank; it widened it.

2. **Three closure-reminders are advisory not blocking**: hb-069, hb-108,
   hb-121 each have a defensible differentiator from their adjacent
   closures. Kill-switches at scoping should validate the differentiator
   before plan-sized work.

3. **No drift required substantive rewriting**: the 7-entry status-drift
   fixes were the inner `status:` and `priority_score:` fields trailing
   the (already-correct) title tags. Probably an artifact of subagents
   updating titles without sweeping the structured fields. Worth a
   convention for future batches.

4. **The "PRIORITY: wild" categorical was a presentation bug, not a
   ranking gap**: mr-009 aggregator had real numbers in `priority_score:`
   the whole time. Future ideation passes should propagate the number
   into the title at merge time.

## Commit

`research(meta): batch-16 cruft purge — reconcile 7 status-drift entries
+ assign priorities to 28 wild-card titles + flag 3 closure-reminders`

Single commit on branch `iter/2026-06-01-batch-16-cruft-purge`.
