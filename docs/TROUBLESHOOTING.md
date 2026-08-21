# Troubleshooting

[← Back to the README](../README.md)

### "Audio init failed" appears in the TUI status

Most often: cpal can't find the input device named in your config.
Run `pancetta test-audio --list` to see the names cpal sees and copy one
verbatim into `[audio].input_device`. Wireless USB CODECs sometimes
present a transient name on first plug-in; unplug, replug, restart.

### No decodes appear, even with strong signals

1. Confirm audio is actually flowing: the audio-level meter on the
   bottom-right of the TUI should bounce when stations are on. If it's
   flat, your input device is wrong or muted at the OS level.
2. Confirm slot timing: FT8 slots are aligned to UTC second `:00` and
   `:15` etc. If the host clock is more than ~1 second off, decodes will
   fail systematically. NTP fixes this; `chrony` is the recommended
   daemon on Linux.
3. Confirm the band — set the dial on your rig (CAT auto-syncs at
   startup), or use the `=` / `-` band keys in the TUI. Listening on the
   wrong band against a CW segment looks identical to "no signal" from
   the decoder's point of view.

### `Call X failed: duplicate QSO`

Pancetta refuses to call the same station within the configured
`duplicate_checking.time_window_hours` rolling window (by default, the
in-memory check scopes this to within 50 Hz of the same frequency —
see `[duplicate_checking]` in [`CONFIG.md`](CONFIG.md)).
Adjust the window in config, or remove the prior QSO from
`~/.pancetta/qso.db` if it was a test. The duplicate check is
intentional — it prevents embarrassing repeat-calls during a contest
or grid hunt.

### `rigctld` won't connect

Pancetta spawns `rigctld` automatically when `[rig.interface].enabled`
is true. Check:

- The serial device path in `[rig.interface].port` exists (`ls /dev/tty.*`
  on macOS, `ls /dev/ttyUSB*` on Linux, Device Manager on Windows).
- The hamlib model number matches your radio (`rigctl --list`).
- The baud rate matches the radio's CAT port setting (38400 is correct
  for the Yaesu FTdx10 default).
- No other process holds the serial device (e.g. WSJT-X is not running).

If `rigctld` itself works (`rigctld -m 1042 -r /dev/tty... -s 38400`)
but Pancetta refuses to spawn it, check the log line that begins
`Refusing to spawn rigctld with suspicious port path` — Pancetta now
allow-lists `/dev/tty*`, `/dev/cu.*`, `COM<N>`, and `host:port` only.
