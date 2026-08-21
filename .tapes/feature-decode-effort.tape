# Per-feature GIF: live decode-effort preset cycling. Run from the repo root:
#   vhs .tapes/feature-decode-effort.tape
# Produces assets/feature-decode-effort.gif.
#
# Why this feature and not "a CQ getting scored then worked": the
# `assets/demo-wav` corpus does not produce an FT8 decode under --replay's
# live streaming pipeline. See `.tapes/demo.tape`'s trailing "HONEST
# DISCLOSURE" comment for what has been measured and ruled out. A GIF of
# "priority-based station selection" would require a real decoded CQ to
# score and work, which this fixture can't deliver on demand -- faking that
# would misrepresent the pipeline. Decode-effort cycling (`e` key,
# pancetta-config/src/decoder.rs DecodeEffort::cycle, wired in
# pancetta-tui/src/tui_runner.rs) is a genuinely decode-independent feature:
# it's a live operator control that updates the title-bar "DECODE: <PRESET>
# <n>ms" chip (pancetta-tui/src/ui/mod.rs:712) immediately on keypress, no
# decode required. Same reasoning applies to the multi-stream-TX GIF the
# brief also floats -- two simultaneous TX streams requires two active QSOs,
# which requires decodes, so it's skipped for the same reason (see the task
# report).

Output assets/feature-decode-effort.gif

Set Shell "bash"
Set FontSize 16
Set Width 1200
Set Height 700
Set Padding 20
Set Theme "Dracula"

# Same config-bootstrap pattern as `.tapes/demo.tape` / `.tapes/screenshots.tape`:
# a scratch config pinning effort = "max" as the ring's starting preset.
Hide
Type `DEMO_CFG=$(mktemp -t pancetta-feature-config).toml; printf '[station]\ncallsign = \"K5ARH\"\n\n[decoder]\neffort = \"max\"\n' > $DEMO_CFG`
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
# The extra hidden sleep clears the replay feeder's worst-case 15s UTC
# slot-boundary pre-roll (`replay_preroll_wait`) so the clip opens on a live
# waterfall rather than a blank one. It costs nothing in GIF length -- these
# frames are Hidden.
Wait+Screen@15s /Band Activity/
Sleep 20s
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
