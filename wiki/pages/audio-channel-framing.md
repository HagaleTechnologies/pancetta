---
id: audio-channel-framing
title: What will bite you about audio channel framing into the DSP stage?
kind: gotcha
status: current
maintainer: agent
sources:
  - pancetta/src/coordinator/dsp.rs
  - pancetta/src/coordinator/audio.rs
  - pancetta/src/coordinator/replay.rs
verified:
  commit: docs/readme-visual-identity
  date: 2026-08-21
links:
  - modes
---
Every buffer sent on `audio_to_dsp_tx` is **interleaved multi-channel frames**,
not mono samples. `coordinator/dsp.rs` de-interleaves unconditionally against
`[audio] input_channels` (default **2**):

```rust
let mono: Vec<f32> = if input_channels > 1 {
    samples.chunks(input_channels as usize).map(|ch| ch[0]).collect()
} else { samples };
```

That is correct for a real `cpal` capture stream (rig CODECs are typically
2-channel, right channel near-silent). It is silently *destructive* for any
synthetic producer that emits bare mono: DSP keeps every Nth real sample and
still treats the survivors as covering the same wall-clock interval, so the
decoder sees the audio at `1/N` of its true sample rate — every tone shifted
up, every symbol shortened. FT8 cannot decode through that at all.

## Symptom

The pipeline looks completely healthy — audio flowing, waterfall painting,
windows firing at exactly `slot_boundary + 13s`, decode completing well inside
its budget — and reports `0 messages decoded` forever. Nothing in the logs
points at audio, because nothing about audio is *failing*; it is being
resampled by a factor of two by an operation that thinks it is throwing away
an idle channel.

## A real instance of this bug (fixed 2026-08-21, PR #263)

`--replay` never produced a single decode since it was written. Multiple prior
investigations chased and fixed real but insufficient causes (feed-pacing
drift from a truncated millisecond timer; a grace period shorter than the 13s
decode phase) and explicitly ruled out others (the per-slot decode-time
ceiling), documenting the root cause as unknown. It was this: the replay
feeder handed `dsp.rs` mono. Fix: `interleave_mono` in
`pancetta/src/coordinator/replay.rs` duplicates each sample across
`input_channels` before sending. `assets/demo-wav` went from 0 decodes to 26
with no other change.

## If you add an audio producer

Anything that feeds `audio_to_dsp_tx` must emit `input_channels`-interleaved
frames. Note that **`PANCETTA_STUB_AUDIO` (`coordinator/audio.rs`) still emits
bare mono** and therefore still generates its 1500 Hz tone at an effective
3000 Hz — harmless for its "is the path alive" purpose, deliberately left
alone, and exactly the trap to avoid copying.

Fix framing at the producer, not in `dsp.rs`: the de-interleave there is the
live-audio contract and special-casing it would make synthetic sources diverge
from real hardware behavior.
