<!-- GENERATED FILE - do not edit by hand.
     Source: pancetta-tui/src/keymap.rs (KEYBINDINGS).
     Regenerate: PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-tui keybindings_doc_is_current -->

# Pancetta TUI keybindings

Press `?` inside the TUI for the same list as an overlay.

## Navigation

| Key | Action |
|---|---|
| `Tab / Shift+Tab` | Switch panel |
| `Up / Down` | Scroll list |
| `Home / End (or < / >)` | Jump to newest (realtime) / oldest |
| `PgUp / PgDn` | Page scroll |
| `1/2/3/4/5` | Jump: Band/QSO/Callers/DX/Placement |

## Calling & QSOs

| Key | Action |
|---|---|
| `Space` | Call selected station |
| `/` | Compose free-text TX (Enter sends, Esc cancels) |
| `Enter` | Callers: reply at shown step; TX Placement: park at selected slice |
| `c / s` | Start / stop CQ |
| `k` | Abort selected QSO (QSO Status panel only) |
| `r` | Re-send last TX (QSO Status panel only) |
| `Shift+H` | Engage Hound on selected DX Hunter station |

## TX control

| Key | Action |
|---|---|
| `Left / Right` | TX offset −/+ 50 Hz (Callers: cycle reply step) |
| `[ / ]` | TX offset −/+ 50 Hz |
| `= / -` | Band up / down |
| `t` | Find clear TX offset (auto-pick + pin) |
| `f` | TX freq mode: HOLD (pin offset) / AUTO (pancetta picks) |
| `o` | Set TX audio offset Hz (blank=Auto) — implies Hold |
| `Shift+F` | Set dial / split freq (RX MHz + optional TX MHz) |
| `Shift+T` | Tune (12 s tone; blocked while TX DISABLED) |
| `h` | Halt current TX |
| `p` | Toggle PTT (blocked while TX DISABLED) |
| `g` | Cycle TX policy: Full → Respond-only → Disabled |

## Modes & views

| Key | Action |
|---|---|
| `v / V` | Cycle activity view: Operate/Hunt/Run/Monitor |
| `z` | Zoom focused panel (again/Esc to restore) |
| `a` | Toggle autonomous mode |
| `Shift+P` | Pause / resume autonomous |
| `Shift+M` | Cycle operating mode (FT8 → FT4; waits for coordinator confirm) |
| `e` | Cycle decode-effort preset: Eco → Standard → Deep → Max → Auto |
| `Shift+X` | Toggle Fox (DXpedition) mode |

## Audio & devices

| Key | Action |
|---|---|
| `m` | Toggle audio monitoring |
| `d` | Device picker |

## Diagnostics & help

| Key | Action |
|---|---|
| `?` | Toggle this help |
| `Shift+D` | Toggle Diagnostics overlay (retained event history) |
| `Shift+S` | Toggle station-health panel (is the station healthy?) |
| `Shift+R` | Toggle Recent-QSOs panel (retained terminal-QSO outcome history) |
| `x` | Clear decoded messages (press twice within 3s) |

## Session & safety

| Key | Action |
|---|---|
| `q` | Quit (with confirm) |
| `Shift+Q` | EMERGENCY STOP (halt TX, autonomous off) |
| `Esc` | Dismiss overlay / cancel modal / clear stop banner |
