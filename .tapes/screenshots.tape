# Static TUI screenshots for the README. Run from the repo root:
#   vhs .tapes/screenshots.tape
# Produces assets/screenshot-{operate,priority,qso,waterfall}.png.
#
# Uses the same `pancetta --replay assets/demo-wav` fixture and config-
# bootstrap pattern as `.tapes/demo.tape`. Every panel below is captured from
# a real run with real off-air audio flowing through the pipeline -- NOT from
# an idle process with no audio device.
#
# The `assets/demo-wav` corpus does not, however, produce an FT8 decode under
# `--replay`. See `.tapes/demo.tape`'s trailing "HONEST DISCLOSURE" block for
# what has been measured and ruled out (feed pacing, grace period, and the
# per-slot decode-time ceiling are all ruled out; the remaining cause is not
# root-caused). These screenshots therefore show real panel chrome and layout
# -- including the DX Hunter panel's "Pri" (priority score) column and the
# TX-placement instrument -- with unpopulated decode rows. That is the honest
# current state of this fixture, not a bug in this tape.

Set Shell "bash"
Set FontSize 16
Set Width 1200
Set Height 700
Set Padding 20
Set Theme "Dracula"

# Scratch config pinning [decoder] effort = "max". Identical bootstrap to
# `.tapes/demo.tape` -- see there for why the callsign is no longer
# load-bearing (`--replay` suppresses the first-run wizard on its own now).
Hide
Type `DEMO_CFG=$(mktemp -t pancetta-screenshot-config).toml; printf '[station]\ncallsign = \"K5ARH\"\n\n[decoder]\neffort = \"max\"\n' > $DEMO_CFG`
Enter
Sleep 500ms
# Reset the persisted active view to Operate -- see `.tapes/demo.tape` for the
# full rationale. This tape cycles views with `v` and quits from a non-Operate
# view, so without this reset it would poison the *next* tape's startup anchor
# (and its own, on a re-run).
Type `mkdir -p ~/.pancetta && printf '{"active_view":"Operate","decode_effort":"MAX"}' > ~/.pancetta/tui_state.json`
Enter
Sleep 500ms
Type "clear"
Enter
Sleep 500ms
Show

Type "cargo run --release -p pancetta -- --replay assets/demo-wav --config $DEMO_CFG"
Enter

# "Band Activity" renders on the very first TUI frame regardless of decode
# state -- the same reliable startup anchor `.tapes/demo.tape` uses. It fires
# almost immediately, though, which is NOT yet a running pipeline: the replay
# feeder waits for the next true UTC 15s slot boundary before it sends a
# single sample (`replay_preroll_wait`, pancetta/src/coordinator/replay.rs),
# so for up to 15s after this anchor the waterfall is blank, Level reads 0.0%
# and the AUD badge is dim. Screenshotting there captures an idle process, not
# the station running.
#
# Sleep past the worst-case pre-roll (15s) PLUS the 13s the DSP stage takes to
# emit its first decode window (slot_boundary + dsp_decode_phase, see
# pancetta/src/coordinator/dsp.rs), so the status bar's "DSP:<n>" counter has
# incremented at least once and every capture shows real audio flowing through
# a working pipeline rather than a blank waterfall and a red zero. 15 + 13 +
# margin => 32s. Total audio is 75s and the panel navigation that follows
# takes ~5s, so all four captures land comfortably inside the feed.
Wait+Screen@15s /Band Activity/
Sleep 32s

# 1. Default Operate view: Band Activity + TX-placement instrument + station
#    card + title-bar DECODE-effort chip, all real chrome from a live run.
Screenshot "assets/screenshot-operate.png"

# 2. DX Hunter panel, zoomed. Shows the real "Src/Call/Entity/Grid/SNR/
#    Rarity/Rpt/Last/Pri" column layout (Pri = the per-station priority
#    score column; pancetta-tui/src/ui/dx_hunter.rs) -- with empty rows,
#    since this replay run produces no decode, per the disclosure above.
#    Panel 4 = DX Hunter (see pancetta-tui/src/keymap.rs).
Type "4"
Sleep 300ms
Type "z"
Sleep 500ms
Screenshot "assets/screenshot-priority.png"

# 3. QSO Status panel, zoomed. Panel 2 = QSO Status.
Type "z"
Sleep 300ms
Type "2"
Sleep 300ms
Type "z"
Sleep 500ms
Screenshot "assets/screenshot-qso.png"

# 4. Monitor view: the original big-waterfall view (v/V cycles
#    Operate -> Hunt -> Run -> Monitor -> Operate; 3 presses from Operate
#    reaches Monitor). pancetta-tui/src/view.rs.
Type "z"
Sleep 300ms
Type "v"
Sleep 500ms
Type "v"
Sleep 500ms
Type "v"
Sleep 800ms
Screenshot "assets/screenshot-waterfall.png"

# Clean shutdown: q opens a confirm overlay, y confirms.
Type "q"
Sleep 300ms
Type "y"
Sleep 1s
