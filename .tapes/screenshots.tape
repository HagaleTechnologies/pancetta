# Static TUI screenshots for the README. Run from the repo root:
#   vhs .tapes/screenshots.tape
# Produces assets/screenshot-{operate,priority,qso,waterfall}.png.
#
# Uses the same `pancetta --replay assets/demo-wav` fixture and config-
# bootstrap pattern as `.tapes/demo.tape`. Every panel below is captured from
# a real run with real off-air audio flowing through the pipeline -- NOT from
# an idle process with no audio device.
#
# The captures are timed to land AFTER the corpus's decode burst, so the
# decode-driven panels (Band Activity, DX Hunter with its "Pri" priority-score
# column) show real rows, not empty chrome. See `.tapes/demo.tape`'s trailing
# notes for the corpus's decode profile: only the last file (`live_now.wav`)
# decodes -- ~26 messages in one window -- because the four numbered captures
# ahead of it are documented non-decoding content
# (assets/demo-wav/README.md). Everything below therefore waits for the
# status bar's message counter to go double-digit rather than sleeping a
# fixed interval.

Set Shell "bash"
Set FontSize 16
Set Width 1200
Set Height 700
Set Padding 20
Set Theme "Dracula"

# Scratch config pinning [decoder] effort = "max". Identical bootstrap to
# `.tapes/demo.tape` -- see there for why the callsign is no longer
# load-bearing (`--replay` suppresses the first-run wizard on its own now), and
# for why the `mktemp` invocation uses an explicit `XXXXXX` template instead of
# `-t <prefix>` (GNU mktemp rejects the latter, and the old form wrote the
# config to a file named `.toml` in the repo root on Linux).
Hide
Type `DEMO_DIR=$(mktemp -d "${TMPDIR:-/tmp}/pancetta-screenshot-config.XXXXXX"); DEMO_CFG="$DEMO_DIR/pancetta.toml"; printf '[station]\ncallsign = \"K5ARH\"\n\n[decoder]\neffort = \"max\"\n' > $DEMO_CFG`
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
# Then wait for the decode burst itself rather than sleeping a fixed
# interval. The burst comes from `live_now.wav`, the LAST 15s of the 75s
# corpus, so its decode window fires at feed_start + 73s (slot_boundary +
# dsp_decode_phase, pancetta/src/coordinator/dsp.rs) -- i.e. anywhere from
# t+73s to t+88s after this anchor depending on how much UTC-slot pre-roll
# the feeder had to absorb. A fixed sleep sized for the worst case would
# waste up to 15s of the ~20s REPLAY_GRACE_PERIOD window the captures have
# to complete in; waiting on the counter instead starts the captures the
# moment the decodes land, leaving the same ~20s no matter the pre-roll.
#
# `Msgs: <n>` is the status bar's decoded-message counter
# (pancetta-tui/src/ui/mod.rs, app.decoded_messages.len()). The regex
# requires TWO digits deliberately: an early window over the pre-roll's
# near-silence can produce a single spurious low-SNR decode, and matching
# `Msgs: 1` would fire the captures ~60s too early on an empty panel. The
# real burst is ~26-28 messages.
Wait+Screen@15s /Band Activity/
Wait+Screen@120s /Msgs: [1-9][0-9]/

# 1. Default Operate view: Band Activity + TX-placement instrument + station
#    card + title-bar DECODE-effort chip, all real chrome from a live run.
Screenshot "assets/screenshot-operate.png"

# 2. DX Hunter panel, zoomed -- the flagship priority-engine capture. Shows
#    the real "Src/Call/Entity/Grid/SNR/Rarity/Rpt/Last/Pri" column layout
#    (Pri = the per-station priority score; pancetta-tui/src/ui/dx_hunter.rs)
#    populated from the decode burst this tape waits for above.
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

# Remove the scratch config directory (off-camera; screenshots are already
# captured) so a local run leaves nothing behind in TMPDIR. The extra sleep
# lets the TUI finish tearing down and hand the terminal back to the shell --
# typed too early, `rm` would land in the TUI instead of at the prompt.
Hide
Sleep 3s
Type `rm -rf "$DEMO_DIR"`
Enter
Sleep 500ms
