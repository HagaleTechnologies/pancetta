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
#
# CAUTION -- this does NOT mean unbounded decode time in the live runtime.
# `DecodeEffort::Max` resolves to a `decode_effort_budget_ms == 0` ("unlimited")
# sentinel, but `pancetta/src/coordinator/ft8.rs` (see `decode_budget_ceiling_ms`,
# ~lines 57-63, and its use at ~lines 667-686) still clamps every decode window
# to a hard per-slot ceiling regardless of effort preset -- 2000ms for FT8's
# 15s slot. That ceiling is NOT the same guarantee as
# pancetta-ft8/tests/wav_decode_tests.rs's `DecodeBudget::unlimited()`, which
# truly never times out. In other words: `effort = "max"` sets the preset, but
# the coordinator's live decode loop still enforces a fixed wall-clock cutoff
# every window, preset notwithstanding. (Investigated 2026-08-20 while chasing
# why this demo doesn't show a decode -- see the final comment block below and
# the task report for full detail.)
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

# HONEST DISCLOSURE about what assets/demo.gif (as committed) actually shows,
# for whoever writes the README next (do not overclaim beyond this):
#
# The committed GIF shows real pipeline operation -- startup, TX-placement /
# priority-scoring, band monitoring, and a clean self-terminating exit -- but
# it does NOT show an actual FT8 decode. `Msgs: 0` / "No message
# Monitoring..." holds for the entire recording despite `RF present` and the
# `DECODE: MAX <n>ms` badge confirming effort=max is active end-to-end.
#
# Two known, un-fixed reasons, tracked as follow-up investigation and
# explicitly NOT part of this plan:
#   1. The effort=max ceiling gap documented in the comment above this tape's
#      bootstrap block: "max" still enforces a hard 2000ms/slot ceiling in
#      pancetta/src/coordinator/ft8.rs, not a true unbounded decode.
#   2. Even accounting for (1), timing the exact fixture
#      (basicft8/live_now.wav) through the unit test's genuinely unbounded
#      DecodeBudget::unlimited() (wav_decode_tests.rs) shows it decodes in
#      ~1s -- comfortably under the 2000ms ceiling on time alone. So there is
#      a likely separate discrepancy between the live streaming --replay
#      decode path (cross-cycle decoder state accumulated across consecutive
#      real-time windows) and the offline batch-test harness (which hands the
#      decoder one fixture window directly) -- not yet root-caused.
#
# Three attempts across this branch (slot-alignment fix, adding the
# documented-decodable live_now.wav fixture, effort=max) each landed a real,
# correct fix but none produced a visible decode. This is accepted as final
# for this plan -- README copy should describe the GIF as showing live
# pipeline startup / TX-scoring / clean shutdown, and must NOT claim or imply
# it shows a captured FT8 decode. See
# .superpowers/sdd/2026-08-20-readme-visual-identity-plan/replay-demo-max-effort-report.md
# for the full investigation.
