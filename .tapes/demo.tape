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
Hide
Type `DEMO_CFG=$(mktemp -t pancetta-demo-config).toml; printf '[station]\ncallsign = \"K5ARH\"\n' > $DEMO_CFG`
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

# Let the replay run. The four 15s FT8 decode windows land at
# approximately t=24s/37s/52s (measured from process start). NOTE: across
# repeated live runs of this exact demo-wav set, whether any of those
# windows actually decodes a message is inconsistent (observed both 0 and
# 1 decoded messages across otherwise-identical runs) -- --replay streams
# audio at real-time cadence from process start, which is NOT aligned to
# true UTC FT8 slot boundaries, so the decoder's sync search sometimes
# catches this corpus's one marginal signal (SNR -14) and sometimes
# doesn't, independent of anything this tape controls. This is a
# real characteristic of the current demo-wav + --replay pairing, not a
# tape-timing bug -- see assets/demo-wav/README.md and the Task 6 report
# for detail. Sleep through the full decode-window sequence regardless.
Sleep 45s

# `--replay` self-terminates ~5s after the input directory is exhausted
# (REPLAY_GRACE_PERIOD in pancetta/src/coordinator/replay.rs) -- total
# process lifetime for this WAV set is ~62s. Hold long enough to see the
# process exit back to the shell prompt on its own; no manual Ctrl+C is
# needed or wanted.
Sleep 20s
