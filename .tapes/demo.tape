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

# HONEST DISCLOSURE about what assets/demo.gif (as committed) actually shows.
# Do not overclaim beyond this.
#
# WHAT IT SHOWS: real pipeline operation against real off-air audio -- startup,
# a live waterfall fed by the replayed capture, the TX-placement /
# priority-scoring instrument, a healthy audio path, and a clean
# self-terminating exit.
#
# WHAT IT DOES NOT SHOW: an actual FT8 decode. `Msgs: 0` / "No message
# Monitoring..." holds for the whole recording.
#
# Ruled out as the cause (measured on 2026-08-21 against this exact fixture
# set, `--headless --replay assets/demo-wav` with effort=max):
#
#   * Feed pacing / slot phase alignment. The feeder used to derive a single
#     truncated millisecond timer period, running audio 6.67% fast at the
#     default 48000Hz/512 config and walking the replayed audio out from under
#     the decoder's wall-clock-keyed windows. Fixed (absolute-deadline pacing);
#     the run now logs "fed 3600000 samples (75.00s of audio) in 75.00s
#     wall-clock", and windows #2-#5 fire at exactly slot_boundary+13s.
#   * The grace period truncating the tail. It was 5s -- shorter than the
#     13s decode phase alone -- so the final slot never reached the decoder
#     (3 of 4 windows on the old 60s corpus). Now 20s; all 5 windows reach
#     the FT8 stage.
#   * The per-slot decode-time ceiling. `decode_budget_ceiling_ms`
#     (pancetta/src/coordinator/ft8.rs) clamps each window to 2000ms for FT8
#     regardless of effort preset, but the windows here complete in
#     ~100-480ms and report 0 messages -- they are not budget-limited, so the
#     ceiling is not what's suppressing the decode.
#
# What remains unexplained: the window covering `live_now.wav` (window #5,
# the highest-RMS window, correctly phase-aligned) reports
# "0 messages decoded (0 ft8lib + 0 native + 0 cross-seq)" with
# best_score=4.3 over 200 sync candidates, even though
# pancetta-ft8/tests/wav_decode_tests.rs decodes that same file offline as a
# single window. The four numbered captures are separately documented as
# non-decoding content (see assets/demo-wav/README.md). Why the live
# streaming path and the offline harness disagree on `live_now.wav` is NOT
# root-caused -- treat it as an open question, not a known mechanism.
#
# Note also that FT8 decoding is inherently probabilistic: even a corpus that
# decodes reliably would not guarantee a decode in every recording. README
# copy should describe this GIF as showing live pipeline startup, waterfall,
# TX-scoring and clean shutdown, and must NOT claim or imply it shows a
# captured FT8 decode.
