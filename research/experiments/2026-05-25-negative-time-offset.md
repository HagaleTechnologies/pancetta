---
slug: negative-time-offset
mode: ft8
state: shelved
created: 2026-05-25T21:15:00Z
last_updated: 2026-05-25T21:15:00Z
branch: iter/2026-05-25-batch-11
parent_hypothesis: hb-012
wild_card: false
scorecard: n/a (premise-correcting analysis — no code, no eval)
delta_vs_main: n/a
disposition: SHELVE hb-012 — premise invalid for pancetta's corpus; the full-buffer Costas scan already covers all interior timing offsets. Operational-only value (unmeasurable in harness).
---

## Hypothesis

hb-012 (0.44): the Costas sync search starts at `t0=0` and never looks
at negative time offsets, so early-arriving DX signals (clock skew, long
path) that begin before the nominal slot boundary are missed. Extend the
search to negative DT for +0.01..+0.03 real decode rate on DX recordings.
(Origin: decoder_sensitivity.md "A3: sync search starts at t0=0 — extend
for early-arriving DX.")

## Investigation

Confirmed the literal facts:
- `compute_spectrogram` hardcodes `time_padding: 0` (decoder.rs:1199).
- `costas_sync_search` loops `t0 in 0..=max_time_step` (decoder.rs:1280)
  — no negative offsets.
- The `Spectrogram::time_padding` field is wired into the candidate-time
  math but never set nonzero, so the infrastructure is dormant.

**But the premise is wrong for pancetta's eval corpus.** The curated
recordings are NOT single slot-aligned 15 s windows — they are **90-second
continuous multi-slot captures** (`~/.pancetta/recordings/*.wav`, ~6 FT8
slots each), and `decode_wav` passes the **entire** sample buffer to
`decode_window` (no per-slot windowing). `compute_spectrogram` builds the
spectrogram over the whole buffer (`num_blocks = audio.len()/block_size`),
and the Costas search scans `t0` from 0 to `num_steps - msg_span - 1`
across all ~1124 time steps.

Consequence: a signal whose timing is "negative DT relative to its own
slot" still sits at some **positive** `t0` within the continuous buffer
(the recording spans many slots back-to-back), so the existing forward
scan **already finds it**. The truth set (jt9 baselines, ~43 decodes per
90 s WAV) confirms the decoder is already recovering signals across all
six slots at arbitrary sub-slot offsets.

The only genuinely unrecoverable case is a signal in the **first** slot
that began *before the recording started* — there is no audio for it, so
no search range can recover it. That is a negligible edge case with no
data to act on.

## Cross-check with mr-006

The mr-006 corpus survey (same batch, run in parallel) reached a
compatible conclusion from the opposite direction: negative-time search
has "~+4 wild-50 novels (≈0 composite)" value and is "operational
robustness more than composite." wild-50 (the only slot-misaligned tier)
has no jt9 truth, so any recovery there is unmeasurable; hb-025 already
showed wild-50's 0/96 is driven by 2 outlier WAVs.

## Decision

**SHELVE.** No code. The hypothesis assumed single-slot windows
(t0=0 = slot start); pancetta decodes full multi-slot recordings where
t0=0 = recording start and the forward scan covers every interior
timing offset. Nothing to gain on the corpus.

## Learnings / follow-ups

- **Resolves hb-040 too:** `time_range` / `time_padding` stay at their
  no-op defaults because negative-time search isn't needed for the
  continuous-recording corpus. hb-040 ("plumb or remove time_range")
  can be closed as "leave as-is; not needed" or a pure cleanup later.
- **Operational follow-up (not a harness experiment):** IF the live
  FTdx10 coordinator path feeds the decoder *single-slot* windows (ring
  buffer per slot) rather than continuous audio, THEN capture jitter /
  early-DX could matter on-air, and the dormant `time_padding` plumb is
  the fix. This is Phase-5 on-air validation territory, not measurable
  in the offline harness — do not ship unvalidated. Filed under operator
  follow-ups, not the bank.
- General lesson: verify the corpus's actual shape (single-slot vs
  continuous) before reasoning about timing-search hypotheses. Several
  decoder_sensitivity.md notes predate the multi-slot curated corpus and
  carry single-slot assumptions.
</content>
