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

# Provision a scratch config with a real (non-N0CALL) callsign so the
# interactive first-run setup wizard (pancetta/src/main.rs,
# run_first_time_setup) doesn't pop up and hijack the recording -- it only
# fires when station.callsign is still the "N0CALL" default. This writes a
# throwaway file under the OS temp dir; it never touches the operator's
# real ~/.pancetta/pancetta.toml.
#
# Also sets [decoder] effort = "max" (pancetta-config/src/decoder.rs,
# DecodeEffort::Max -- "maximum effort, slowest decode, highest recall").
# The live coordinator's default DecodeEffort::Auto maps to a time-boxed
# per-window budget (~1000ms on typical hardware, see
# pancetta/src/coordinator/effort.rs) that is too tight for the FT8
# decoder's S4 cross-cycle-averaging recovery stage -- the stage that
# pancetta-ft8/tests/wav_decode_tests.rs (~lines 784, 802-812) documents as
# needed to decode assets/demo-wav/live_now.wav. `effort = "max"` removes
# the time box so every recovery stage runs to completion, same as the
# unit test's DecodeBudget::unlimited().
Hide
Type `DEMO_CFG=$(mktemp -t pancetta-demo-config).toml; printf '[station]\ncallsign = \"K5ARH\"\n\n[decoder]\neffort = \"max\"\n' > $DEMO_CFG`
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
# whether any FT8 decode has happened yet -- a reliable startup anchor
# (verified against a live --headless --debug run of this exact WAV set,
# which reaches steady state within ~1s).
Wait+Screen@15s /Band Activity/

# Let the replay run. `start_replay_pipeline` (pancetta/src/coordinator/replay.rs)
# now waits for the next true UTC 15s slot boundary before starting the
# audio feed -- up to 15s of pre-roll -- so the process no longer starts
# feeding audio at an arbitrary phase relative to the decoder's UTC-keyed
# windows. That phase-alignment is what makes a decode reliable, but it
# also shifts the five 15s FT8 decode windows later by up to 15s in the
# worst case. `assets/demo-wav` now holds 5 files (the original 4 numbered
# captures plus `live_now.wav`, added because it's the one fixture in that
# source directory documented to actually decode -- see
# `assets/demo-wav/README.md`), so total audio is ~75s (5x15s) instead of
# the previous ~60s. Sleep through the full pre-roll + decode-window
# sequence regardless.
Sleep 75s

# `--replay` self-terminates ~5s after the input directory is exhausted
# (REPLAY_GRACE_PERIOD in pancetta/src/coordinator/replay.rs). Worst-case
# total process lifetime is now ~95s: up to 15s UTC-slot pre-roll + 75s of
# audio + 5s grace period. Sleep well past that so the recording holds
# long enough to see the process exit back to the shell prompt on its own
# (no manual Ctrl+C); the extra margin also gives the 5th file's decode
# window -- the one most likely to show a live decode -- room to render
# and stay on screen before the process exits.
Sleep 40s
