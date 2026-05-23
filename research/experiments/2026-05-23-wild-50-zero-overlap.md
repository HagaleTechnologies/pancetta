---
slug: wild-50-zero-overlap
mode: ft8
state: shelved
created: 2026-05-23T00:00:00Z
last_updated: 2026-05-23T00:00:00Z
branch: experiment/ft8/wild-50-zero-overlap
parent_hypothesis: hb-025
wild_card: false
scorecard: research/scorecards/sweep/wild50-tr-3.0.json
delta_vs_main: 0 (production unchanged; finding is diagnostic + spawns hb-040)
disposition: SHELVED — wild-50 outliers are recording-misalignment; `time_range` config is dead code
---

## Hypothesis

main.json shows wild-50 tier: 0/96 truth recovered, concentrated in
2 outlier WAVs with 49+43 truth decodes each. Diagnose: matching-logic
bug, decoder edge case, or sampling artifact.

## Result

**The two outliers are slot-misaligned recordings.** Both WAVs have
ALL jt9 truth decodes at extreme negative `dt`:

| WAV (sha8...)    | truth count | dt range          |
|------------------|-------------|-------------------|
| 92e31566...      | 49          | [-2.50, -1.40] s  |
| 28f0ce9e...      | 43          | [-2.40, -1.90] s  |

Pancetta's effective decode search window starts at t=0 in the audio
buffer. Signals at dt=-2.5 are before the buffer's t=0 (or in a
different slot entirely — the WAVs are 12.64s = one slot length each).
pancetta has no way to find them.

**Secondary finding (more important): `Ft8Config::time_range` is
dead code.** The field exists at decoder.rs:126 (default 2.0) but
isn't actually used anywhere in the decode pipeline. The spectrogram's
`time_padding` field is hardcoded to 0 (decoder.rs:1102 and 3767).
Setting `time_range = 3.0` in this experiment had ZERO effect on
wild-50 decode counts — confirming the config field is inert.

| time_range | wild-50 rec | wild-50 novel |
|------------|-------------|---------------|
| 2.0 (default) | 0           | 4             |
| 3.0           | 0           | 4             |

## Disposition

**SHELVED.** Two findings:
1. **Recording-pipeline issue:** the wild-50 outliers are misaligned
   to the FT8 slot grid (~2.5s offset). Operational fix is to align
   the recording, not patch the decoder. The wild-50 0/96 score
   reflects a quirk of how those WAVs were captured, not a decoder
   gap.
2. **Dead config field:** `Ft8Config::time_range` is unused; setting
   it does nothing. This is a documentation/maintainability footgun
   (same pattern as the now-shelved `aggressive_decoding` flag from
   hb-020).

No production change in this experiment. Both findings spawn
follow-ups.

## Learnings

- **wild-50's 0/96 is a sampling artifact, not a decoder limitation.**
  The curate binary's random sampling drew 2 WAVs that happen to be
  misaligned. With more wild samples (wild-200, wild-500) the effect
  would dilute. The hard-200 and hard-1000 tiers don't have this
  issue because their curation explicitly filters for
  pancetta-decodable content (interest_score weighs pancetta decode
  count).

- **`time_range` being dead code is the more important finding.** It
  looks like a knob; it isn't. Same surface-area-vs-actual-behavior
  gap as `aggressive_decoding` (hb-020). At minimum, the field's doc
  comment should say "NOT IMPLEMENTED — set via spectrogram
  time_padding (currently always 0)" or the field should be removed.

- **The hb-025 prior was partially right and partially wrong.** The
  bank entry suggested (a) format differences, (b) timing skew, or
  (c) decoder edge case. Reality: (b) timing skew (negative dt
  decodes) was correct, but the cause is misaligned recordings, not
  a decoder bug. The DECODER could be extended to search a wider
  window (spawning hb-040), but that's a workaround for bad recording
  data, not a sensitivity improvement.

## Follow-ups added to hypothesis bank

- **hb-040 (new)** — Either plumb `Ft8Config::time_range` through to
  the spectrogram's `time_padding` (so misaligned recordings can be
  decoded) OR remove the dead field. Priority ~0.35.
  - If plumbed: would allow decoding of signals with `dt < 0` by
    padding the audio buffer with leading silence and adjusting the
    Costas search start. Potentially recovers wild-50 outliers AND
    benefits operational recordings that don't start exactly on slot
    boundary.
  - If removed: cleanup, drops a misleading API knob. Smaller change.
  Estimated effort: 0.5-1 session for either path.

## Reproducing

```bash
# Inspect the outlier WAVs:
cat research/baselines/ft8/92e3156682cbe2412a571420001ce0c854a8924759f6b1245469ace950b3be52.json \
  | python3 -c "import json,sys; d=json.load(sys.stdin); print([x['dt_s'] for x in d['decodes']])"

# Confirm time_range is dead:
cargo run --release -p pancetta-research --bin eval -- \
    --tier wild-50 --mode ft8 --time-range 3.0 \
    --output research/scorecards/sweep/wild50-tr-3.0.json
# Result: still 0/96 recovered, identical to time_range=2.0
```
