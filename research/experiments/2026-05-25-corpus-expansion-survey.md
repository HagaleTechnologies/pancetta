---
slug: corpus-expansion-survey
mode: ft8
state: completed
created: 2026-05-25T20:45:00Z
last_updated: 2026-05-25T21:20:00Z
branch: iter/2026-05-25-batch-11
parent_hypothesis: mr-006
wild_card: false
scorecard: n/a (external-source survey)
delta_vs_main: n/a
disposition: COMPLETED — "don't expand the corpus for composite now." One worthwhile class (real Doppler → hb-073); two task-named sources disqualified.
---

## Task

mr-006: survey publicly-available real-world FT8 audio sources and decide
which (if any) corpus classes are worth acquiring, applying the mr-007
architecture-fit lens (pancetta = offline single-call eval, pass-1, no
AP-context value, precision-wall-limited). Run as a background general
agent in parallel with the rest of batch 11.

## Findings (full report from the agent)

**Harness needs:** 12 kHz / mono / 16-bit / slot-aligned WAV.

**Per-class architecture-fit verdict:**
- (a) DXpedition pile-ups — **dead.** Modern DXpeditions transmit
  *SuperFox*, a non-FT8 GFSK waveform (24 sync syms + 127 polar-code
  syms, OTP-authenticated) pancetta cannot decode at all; the Hound
  uplinks are ordinary FT8 already represented by hard-*; "density" is an
  AP/QSO-context problem and AP is worthless offline (ceiling 1/8576).
- (b) Contest QSB / fast operating — **marginal.** Not cleanly separable
  from hard-200; capture opportunistically, don't build a tier.
- (c) Polar/auroral/TEP **real Doppler** — **the one worth acquiring.**
  Native 12 kHz WAV via KiwiSDR + kiwirecorder.py (slot-aligned cron).
  Real spread+drift the crude synth model lacks; JTDX's documented
  Doppler edge proves the gap is decoder-addressable. → spawned hb-073.
- (d) Splatter/interference — **dead.** No decoder lever; the live FP
  filter (hb-052) is the only relevant defense and it's already shipped.
- (e) HF-mobile flutter — **dead.** Subsumed by Doppler; rarest to obtain.

**Disqualified sources (named in the original mr-006 method):**
- **PSKReporter** stores spots/metadata only — there is NO audio archive.
- **arXiv 2512.23160** "Weak Signal Learning Dataset" is spectrograms /
  vectors, not raw WAV — unusable by a WAV decoder.

**Slot alignment:** don't build a preprocessor. Capture slot-aligned at
the source (kiwirecorder cron at :12/:27/:42/:57). For already-misaligned
audio, the in-decoder fix is the dormant `time_padding` plumb (hb-040/
hb-012) — but batch-11 hb-012 showed its corpus payoff is ~0 because the
curated recordings are continuous multi-slot captures the forward scan
already covers. Cheap always-do: **capture future operator recordings
slot-aligned** so the next "wild" data is usable.

## Outcome

- mr-006 COMPLETED.
- Spawned **hb-073** (real-Doppler eval tier, 0.40) — enabler for hb-015.
- **hb-015** noted as blocked on hb-073; bump to ~0.42 once the real
  tier lands.
- Recorded source disqualifications (PSKReporter, arXiv dataset) so a
  future corpus push doesn't re-chase them.

## Bottom line

The existing tiers are not the bottleneck. Corpus expansion is worth
scheduling only as the prerequisite for the hb-015 structural-sync line,
and only when there's appetite for a multi-session structural experiment.
Three of five surveyed classes are architecture-dead; the wild-card debt
is better addressed by an algorithmic structural bet (hb-015 once hb-073
exists) than by more parameter sweeps.

## mr-007 cross-batch tally (updated)

| Source | Candidates | Architecture-fit shelved |
|---|---:|---:|
| mr-001 (WSJT-X-Improved) | 5 | 3 (hb-045/047/046) |
| mr-002 (JTDX) | 5 | 1 effective (hb-055 at pick-time, batch 10) |
| mr-003 (academic LDPC) | 5 | 0 (hb-063 graduated, the rest queued) |
| mr-006 (corpus) | 5 classes | 3 dead + 2 dead sources |
mr-007 applied at harvest keeps yield honest; the corpus survey's main
value was *preventing* wasted acquisition effort.
</content>
