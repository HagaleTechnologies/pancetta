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
  - pancetta/src/coordinator/raw_capture.rs
  - pancetta/tests/wav_recording.rs
verified:
  commit: f31db03
  date: 2026-11-10
links:
  - modes
---
Every buffer sent on `audio_to_dsp_tx` is **interleaved multi-channel frames**,
not mono samples. `coordinator/dsp.rs` de-interleaves unconditionally against
`[audio] input_channels` (default **2**):

```rust
let mono: Cow<[f32]> = if input_channels > 1 {
    Cow::Owned(samples.chunks(input_channels as usize).map(|ch| ch[0]).collect())
} else { Cow::Borrowed(&samples[..]) };
```

(A `Cow` since PAN-393, so the interleaved batch stays available to the raw
48 kHz capture; the values are the same.)

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

## Stub pacing

The stub audio source (`PANCETTA_STUB_AUDIO=1`) sends `buffer_size` samples
every `floor(buffer_size * 1000 / sample_rate)` ms. At 48 kHz,
`buffer_size = 512` sends every 10 ms, which is 6.7% fast in mono (51,200
frames/s) and half rate with the default `input_channels = 2` (25,600
frames/s). `buffer_size = 4096` sends every 85 ms, 0.4 to 0.5% fast
(measured 48,238 frames/s). Anything that checks wall-clock continuity, such
as the PAN-393 raw capture (it abandons a slot whose sample position drifts
250 ms from the wall clock), needs `input_channels = 1` and
`buffer_size = 4096` in stub tests; `pancetta/tests/wav_recording.rs` uses
exactly that.

## Decimation after the channel split (PAN-320)

The decimation stage downstream of the channel-0 extraction has two arms
(`DecimationStage` in `coordinator/dsp.rs`): the historical subsampler
(counter starts 0, retains indices 3, 7, 11, …) and the opt-in 31-tap FIR
(`[audio] decimation_antialias_filter = true`, 48 kHz only). Three framing
pitfalls the tests pin:

- **Channel 0 alone feeds the decoder — never a channel average.** The
  A/B harness (`pancetta-research` `decimation_ab`) uses the same channel-0
  extraction; a stereo capture with signal only in the right channel must
  decode like silence in both arms.
- **Every destructive `ft8_buffer` flush must also reset the stage.** A
  filter history (or subsample counter phase) that survives a dial/mode/
  gain/clock-jump flush splices two unrelated audio streams into one
  window — the same class of bug as mixing two dials (PAN-70), one layer
  down. `destructive_capture_flush` is the single action for this.
- **A causal filter delays the stream by (taps−1)/2 input samples.** The
  FIR's outputs lag 15 samples (312.5 µs); anything that maps buffer
  positions to wall clock (the `boundary_anchored_slice` anchor) must
  compensate at that mapping, not shift the scheduler.

The raw48 capture (PAN-393) taps the stream *before* decimation, so
recordings are arm-independent — that is the property that makes the
decimation A/B comparable.
