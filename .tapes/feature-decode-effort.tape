# Per-feature GIF: live decode-effort preset cycling. Run from the repo root:
#   vhs .tapes/feature-decode-effort.tape
# Produces assets/feature-decode-effort.gif.
#
# Why this feature and not "a CQ getting scored then worked": timing, not
# capability. `--replay` DOES decode now -- the channel-framing bug that
# suppressed every decode is fixed (see `.tapes/demo.tape`'s trailing notes)
# -- but the `assets/demo-wav` corpus only decodes on its LAST slot
# (`live_now.wav`); the four numbered captures ahead of it are documented
# non-decoding content (assets/demo-wav/README.md). So a decode-driven clip
# would have to sit through ~75s of empty panels before anything happened,
# and a full "scored, then worked" QSO progression needs a responding station
# that a fixed recording cannot provide at all.
#
# Decode-effort cycling (`e` key, pancetta-config/src/decoder.rs
# DecodeEffort::cycle, wired in pancetta-tui/src/tui_runner.rs) is instead
# decode-INDEPENDENT: a live operator control that updates the title-bar
# "DECODE: <PRESET> <n>ms" chip (pancetta-tui/src/ui/mod.rs:712) immediately
# on keypress, so it makes a tight clip with no waiting. The same reasoning
# applies to the multi-stream-TX GIF the brief also floats -- two
# simultaneous TX streams needs two live QSO partners, which no recording
# can supply.

Output assets/feature-decode-effort.gif

Set Shell "bash"
Set FontSize 16
Set Width 1200
Set Height 700
Set Padding 20
Set Theme "Dracula"

# Same config-bootstrap pattern as `.tapes/demo.tape` / `.tapes/screenshots.tape`:
# a scratch config pinning effort = "max" as the ring's starting preset. See
# `.tapes/demo.tape` for why the `mktemp` invocation uses an explicit `XXXXXX`
# template instead of `-t <prefix>` (GNU mktemp rejects the latter, and the old
# form wrote the config to a file named `.toml` in the repo root on Linux).
Hide
Type `DEMO_DIR=$(mktemp -d "${TMPDIR:-/tmp}/pancetta-feature-config.XXXXXX"); DEMO_CFG="$DEMO_DIR/pancetta.toml"; printf '[station]\ncallsign = \"K5ARH\"\n\n[decoder]\neffort = \"max\"\n' > $DEMO_CFG`
Enter
Sleep 500ms
# Reset the persisted active view AND decode-effort preset -- see
# `.tapes/demo.tape` for the full rationale. Both live in
# ~/.pancetta/tui_state.json rather than the scratch --config, so the `e`-ring
# below only starts at Max (as its comment claims) if the persisted preset is
# reset too.
Type `mkdir -p ~/.pancetta && printf '{"active_view":"Operate","decode_effort":"MAX"}' > ~/.pancetta/tui_state.json`
Enter
Sleep 500ms
Type "clear"
Enter
Sleep 500ms

Type "cargo run --release -p pancetta -- --replay assets/demo-wav --config $DEMO_CFG"
Enter

# "Band Activity" renders on the first TUI frame regardless of decode state --
# the same reliable startup anchor `.tapes/demo.tape` uses. Stay Hidden through
# startup so the GIF opens directly on the feature, not on boilerplate.
#
# The extra hidden wait clears the replay feeder's UTC slot-boundary pre-roll
# (`replay_preroll_wait`) AND runs on to the corpus's decode burst, so the clip
# opens on a populated station -- live waterfall, filled Band Activity and DX
# Hunter -- rather than a blank one. It costs nothing in GIF length: these
# frames are Hidden.
#
# `Msgs: <n>` is the status bar's decoded-message counter; the two-digit
# requirement skips a possible single spurious pre-roll decode. See
# `.tapes/screenshots.tape` for the full derivation of this anchor. The burst
# lands ~73-88s in, leaving the ~20s REPLAY_GRACE_PERIOD window -- ample for
# the ~6s of key presses below.
Wait+Screen@15s /Band Activity/
Wait+Screen@120s /Msgs: [1-9][0-9]/
Show

# Cycle the full decode-effort ring: Max -> Auto -> Eco -> Standard -> Deep
# (config bootstrap starts at Max, so 5 presses of `e` show all 5 presets
# and the title-bar chip's ms-budget value changing with each one, then
# return to a state one press before Max).
Type "e"
Sleep 1200ms
Type "e"
Sleep 1200ms
Type "e"
Sleep 1200ms
Type "e"
Sleep 1200ms
Type "e"
Sleep 1200ms

Hide
Type "q"
Sleep 300ms
Type "y"
Sleep 500ms

# Remove the scratch config directory so a local run leaves nothing behind in
# TMPDIR. Already off-camera (`Hide` above). The extra sleep lets the TUI
# finish tearing down and hand the terminal back to the shell -- typed too
# early, `rm` would land in the TUI instead of at the prompt.
Sleep 3s
Type `rm -rf "$DEMO_DIR"`
Enter
Sleep 500ms
