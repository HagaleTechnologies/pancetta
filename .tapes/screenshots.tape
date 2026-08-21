# Static TUI screenshots for the README. Run from the repo root:
#   vhs .tapes/screenshots.tape
# Produces assets/screenshot-{operate,priority,qso,waterfall}.png.
#
# Uses the same `pancetta --replay assets/demo-wav` fixture and config-
# bootstrap pattern as `.tapes/demo.tape` (Task 6). See that tape's trailing
# "HONEST DISCLOSURE" comment block for the full story: this demo corpus does
# NOT reliably produce an actual FT8 decode under --replay's live streaming
# pipeline (hard per-slot decode-time ceiling + a likely live-streaming-vs-
# batch-decode-harness discrepancy, both out of scope here). These screenshots
# therefore show real panel chrome/layout -- including the DX Hunter panel's
# "Pri" (priority score) column and the TX-placement instrument -- WITHOUT
# populated decode data. That is expected, not a bug in this tape.

Set Shell "bash"
Set FontSize 16
Set Width 1200
Set Height 700
Set Padding 20
Set Theme "Dracula"

# Provision a scratch config with a real (non-N0CALL) callsign so the
# interactive first-run setup wizard doesn't hijack the recording. See
# `.tapes/demo.tape` for the full rationale (identical bootstrap here).
Hide
Type `DEMO_CFG=$(mktemp -t pancetta-screenshot-config).toml; printf '[station]\ncallsign = \"K5ARH\"\n\n[decoder]\neffort = \"max\"\n' > $DEMO_CFG`
Enter
Sleep 500ms
Type "clear"
Enter
Sleep 500ms
Show

Type "cargo run --release -p pancetta -- --replay assets/demo-wav --config $DEMO_CFG"
Enter

# "Band Activity" renders on the very first TUI frame regardless of decode
# state -- the same reliable startup anchor `.tapes/demo.tape` uses.
Wait+Screen@15s /Band Activity/
Sleep 2s

# 1. Default Operate view: Band Activity + TX-placement instrument + station
#    card + title-bar DECODE-effort chip, all real chrome from a live run.
Screenshot "assets/screenshot-operate.png"

# 2. DX Hunter panel, zoomed. Shows the real "Src/Call/Entity/Grid/SNR/
#    Rarity/Rpt/Last/Pri" column layout (Pri = the per-station priority
#    score column; pancetta-tui/src/ui/dx_hunter.rs) -- empty/placeholder
#    rows since this replay run has no populated decode, per the disclosure
#    above. Panel 4 = DX Hunter (see pancetta-tui/src/keymap.rs).
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
