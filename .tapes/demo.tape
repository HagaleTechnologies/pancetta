# Hero demo GIF for the README. Run from the repo root:
#   vhs .tapes/demo.tape
# Produces assets/demo.gif.

Output assets/demo.gif

Set Shell "bash"
Set FontSize 16
Set Width 1200
Set Height 700
Set Padding 20
Set Theme "Dracula"

# Provision a scratch config with a real (non-N0CALL) callsign and
# [decoder] effort = "max" (pancetta-config/src/decoder.rs, DecodeEffort::Max
# -- "maximum effort, slowest decode, highest recall"). This writes a
# throwaway file under the OS temp dir; it never touches the operator's real
# ~/.pancetta/pancetta.toml.
#
# NOTE: the callsign here is no longer load-bearing. `--replay` now suppresses
# the interactive first-run setup wizard directly (pancetta/src/main.rs --
# `offer_wizard_on_load_failure` and the `run_first_time_setup` gate both test
# `cli.replay.is_none()`), so a bare `--replay` run can't be hijacked by the
# wizard even with a default N0CALL config. The scratch config is kept only to
# pin `effort = "max"` and to keep the recording independent of whatever the
# operator has in their real config.
Hide
Type `DEMO_CFG=$(mktemp -t pancetta-demo-config).toml; printf '[station]\ncallsign = \"K5ARH\"\n\n[decoder]\neffort = \"max\"\n' > $DEMO_CFG`
Enter
Sleep 500ms
# Force the TUI's persisted active view back to Operate. `active_view` lives in
# ~/.pancetta/tui_state.json (pancetta-tui/src/app.rs load_persisted_view /
# view.rs ActiveView::as_str), NOT in the scratch --config above, so it
# survives between runs: a previous recording or an operator session that
# left Hunt/Run/Monitor active would make this tape start in a view with no
# "Band Activity" panel and the Wait+Screen anchor below would time out.
# (A fresh CI runner has no such file and defaults to Operate, which is why
# this only ever bites locally.) This overwrites a one-keypress UI
# preference; re-cycle with `v` afterwards if you care.
Type `mkdir -p ~/.pancetta && printf '{"active_view":"Operate","decode_effort":"MAX"}' > ~/.pancetta/tui_state.json`
Enter
Sleep 500ms
Type "clear"
Enter
Sleep 500ms
Show

Type "cargo run --release -p pancetta -- --replay assets/demo-wav --config $DEMO_CFG"
Enter

# Give the pipeline a moment to spin up and start decoding. "Band Activity"
# is a panel title that renders on the very first TUI frame, independent of
# whether any FT8 decode has happened yet -- a reliable startup anchor.
Wait+Screen@15s /Band Activity/

# Let the replay run. `start_replay_pipeline` (pancetta/src/coordinator/replay.rs)
# waits for the next true UTC 15s slot boundary before starting the audio
# feed -- up to 15s of pre-roll -- then paces the feed against an absolute
# per-chunk deadline so the replayed audio advances at exactly 1.000x real
# time. `assets/demo-wav` holds 5 files (4 numbered captures plus
# `live_now.wav`), so total audio is 75s (5x15s). Sleep through the full
# pre-roll + feed regardless.
Sleep 75s

# `--replay` self-terminates REPLAY_GRACE_PERIOD (20s) after the input
# directory is exhausted, so worst-case total process lifetime is ~110s:
# up to 15s UTC-slot pre-roll + 75s of audio + 20s grace. Sleep past that so
# the recording holds long enough to see the process exit back to the shell
# prompt on its own (no manual Ctrl+C).
Sleep 45s

# WHAT assets/demo.gif (as committed) SHOWS. Keep this accurate; do not
# overclaim beyond it.
#
# Real pipeline operation against real off-air audio: startup, a live
# waterfall fed by the replayed capture, the TX-placement / priority-scoring
# instrument, a healthy audio path, a real FT8 decode burst late in the run,
# and a clean self-terminating exit.
#
# The decode burst comes from `live_now.wav`, the last file in the corpus.
# Measured 2026-08-21 with `--headless --replay assets/demo-wav` at
# effort=max: window #6 (the window covering that file) reports
# "26 messages decoded (25 ft8lib + 1 native + 0 cross-seq)" -- CQ WA3YUV
# FM18, CQ AG6X DM12, CQ AD9DU EN52 and ~23 more. The four numbered captures
# ahead of it are separately documented as non-decoding content (see
# assets/demo-wav/README.md), so the first several slots correctly show an
# empty Band Activity panel; that is the fixture, not a fault.
#
# ROOT CAUSE OF THE OLD "--replay NEVER DECODES" BUG (fixed, PR #263):
# channel framing. `coordinator/dsp.rs` unconditionally de-interleaves every
# buffer it receives against `[audio] input_channels` (default 2) --
# `samples.chunks(input_channels).map(|ch| ch[0])` -- which is exactly right
# for a real cpal capture stream. The replay feeder was handing it bare mono
# samples, so DSP threw away every second REAL sample while still treating
# the survivors as covering the same wall-clock span: the decoder saw the
# audio at half its true sample rate (every tone an octave up, every symbol
# half length). No FT8 decode was possible through that, which is why no
# amount of timing, effort, or content work ever helped. The feeder now
# interleaves each mono sample across `input_channels`
# (`interleave_mono`, pancetta/src/coordinator/replay.rs), so DSP's existing
# extraction recovers the original stream byte-for-byte.
#
# Two earlier fixes were necessary but not sufficient, and remain load-bearing:
#
#   * Feed pacing / slot phase alignment. The feeder used to derive a single
#     truncated millisecond timer period, running audio 6.67% fast at the
#     default 48000Hz/512 config and walking the replayed audio out from under
#     the decoder's wall-clock-keyed windows. Fixed (absolute-deadline pacing);
#     the run logs "fed 3600000 samples (75.00s of audio) in 75.00s
#     wall-clock", and every window fires at exactly slot_boundary+13s.
#   * The grace period truncating the tail. It was 5s -- shorter than the
#     13s decode phase alone -- so the final slot never reached the decoder.
#     Now 20s. This one matters more than ever: the decode burst is in the
#     LAST slot, so a short grace period would cut off the only decode.
#
# Note that FT8 decoding is inherently probabilistic. The corpus is fixed and
# this decode reproduces run to run, but README copy should describe the GIF
# as showing the live pipeline decoding a replayed off-air capture -- not as
# a guarantee about on-air performance.
