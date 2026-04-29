# Pancetta Operations Runbook

Operational procedures for running pancetta in its various modes.
Audience: the station operator (probably you, with a license and a
working rig). Assumes the build is current on `main` and config lives
at `~/.pancetta/config.toml`.

If you're new here, read [`README.md`](../README.md) first for setup;
this document is for *running*, not installing.

---

## Modes of operation

Pancetta has four operating modes, distinguished by config:

| Mode | `[rig.interface].enabled` | `[autonomous].enabled` | Behavior |
|---|---|---|---|
| **Decode-only** *(safe)* | `false` | (irrelevant) | RX + TUI; no PTT, no audio output. Safe to run anywhere. |
| **Manual TX** | `true` | `false` | Decode + Space-bar to call selected stations. Operator drives every TX. |
| **Auto-CQ** | `true` | `false` | Manual + F2 starts a repeating CQ. Still operator-initiated. |
| **Autonomous** *(Phase 5)* | `true` | `true` | Decode + autonomous decision engine drives CQ, response, full QSO progression. |

The autonomous-mode toggle is **config-only** — there is no runtime
key binding to enable or disable it. To switch in or out of autonomous
mode, edit config and restart pancetta.

---

## Phase 5 — first autonomous QSO loop

This is the procedure for the first time you run pancetta with
`[autonomous].enabled = true` against a real antenna. Goal: complete
a full CQ → grid → report → RR73 exchange end-to-end without operator
intervention. Until that lands once successfully, treat every step as
operator-supervised.

### Pre-flight

1. **Build is current.**
   ```bash
   git fetch origin && git rev-parse HEAD == git rev-parse origin/main
   ```
   If the local main isn't at `origin/main`, either pull or stash; you
   want the autonomous run on a known-clean tree.

2. **Test suite is green locally.**
   ```bash
   scripts/check.sh --fast      # fmt + clippy, ~30s
   ```
   Or full check (`scripts/check.sh`, ~10 min) if you've made any code
   changes since the last verified build.

3. **Hardware sanity.**
   - Rig powered on, USB cable seated, audio interface visible:
     `pancetta --list-audio` shows your CODEC.
   - `rigctld --list | grep -i ftdx10` confirms the model number
     (`1042` for the FTdx10).
   - Dummy load attached — *don't* go live on antenna for the first
     run. We'll switch to antenna once the local pipeline behaves.

4. **Config sanity.**
   ```bash
   pancetta config --show
   ```
   Confirm:
   - `[station].callsign` is your real call (not `N0CALL` / `NOCALL`)
   - `[station].grid_square` is your real grid (4 chars minimum)
   - `[rig.interface].enabled = true`, port and baud match your CAT
     setup (FTdx10 default: model `1042`, baud `38400`)
   - `[audio]` device names match `--list-audio` output verbatim
   - `[autonomous].enabled = false` for now (we toggle it last)
   - `[autonomous].max_concurrent_qsos = 1` for first run (don't pile
     QSOs on top of each other while you're learning the behavior)
   - `[autonomous].min_dx_score = 0.3` or higher — lower values
     respond to more stations, including weak/uninteresting ones

5. **Decode-only smoke test.**
   ```bash
   pancetta
   ```
   Listen on 20m FT8 (14.074 MHz dial). Within 60 seconds you should
   see decoded messages in the Band Activity panel. If not:
   - Audio level meter at bottom-right of TUI should bounce when
     stations transmit. Flat = audio device wrong or muted.
   - System clock must be NTP-synced within ±1s. If decode rate is
     zero on a known-active band, suspect clock drift first.

   Press `q` to exit cleanly.

### First manual TX (still on dummy load)

6. **Manual Space-bar test.** With `[rig.interface].enabled = true`
   but `[autonomous].enabled = false`, run pancetta. Wait for a CQ
   to appear in Band Activity, highlight it (↑/↓), press Space.
   Confirm in this order:
   - Status bar: `Calling KXXXX — TX queued (Hz)`
   - Rig display: PTT engages just before the next opposite slot
     boundary (`:00`/`:30` if DX was odd; `:15`/`:45` if DX was even)
   - Watch the rig: ALC stays clean, no flat-line; audio level looks
     normal.
   - PTT releases ~12.7s + 50ms after audio start. The slot-aware
     scheduler ensures we never key during the DX's slot.

7. **Late-press test.** Wait for another CQ. Press Space about 5
   seconds into the slot (so ~5s after the heard CQ ended). Confirm:
   - TX still happens *that same slot* (the opposite-parity one),
     using audio-cursor skip-ahead — not deferred 30s.
   - Rig audio is shorter than 12.64s (only the back portion of the
     waveform is emitted).

   This is the WSJT-X-style late-start path. If TX lands 30s later
   instead of in the current slot, something's off — check
   `[station].tx_late_max_ms` (default 8000) and `[station].ptt_lead_ms`
   (default 80).

### Switch to antenna

8. **Switch to antenna.** Disconnect dummy load, connect antenna,
   confirm SWR is sane on a brief tune-up tone. (Don't tune through
   pancetta — use the rig's tune button.) Pancetta does not currently
   tune the rig.

9. **One real manual QSO.** Repeat steps 6–7 against an actual on-air
   CQ. Watch PSKReporter (within ~30s of TX) to confirm we got out.
   Look for `0 ≤ DT ≤ 0.3` in the spotting station's report — that
   means our timing is within the WSJT-X tolerance window.

### Enable autonomous mode

10. **Set `[autonomous].enabled = true`** in `~/.pancetta/config.toml`.
    Save. Restart pancetta.

    The startup log line should include `Starting autonomous operator
    component`. If you see `Autonomous operator disabled in
    configuration`, the config didn't save or pancetta is reading a
    different file (`pancetta config --show --path` prints the path).

11. **Watch for one cycle.** With antenna live, watch the TUI for at
    least 5 minutes. Expected behavior:
    - Decoder runs every 15s slot, populates Band Activity.
    - Autonomous status (in the TUI) cycles through Hunting / Calling
      CQ / In QSO depending on what's heard.
    - When a high-priority station appears (rare DXCC, etc.) the
      operator initiates a response within one slot.
    - Watch PTT — it should engage on the right slot parity, every
      time.

12. **First end-to-end QSO.** Wait for the operator to start a QSO.
    The full progression is: heard CQ → respond with our grid →
    receive their report → send our R-report → receive their RR73 →
    send 73. Five MessageToSend events, all on the same latched
    parity. The QSO should land in `~/.pancetta/qsos.adi` with all
    fields populated.

    ```bash
    tail -1 ~/.pancetta/qsos.adi
    ```

    Spot-check: callsign, grid, RST sent/received, start/end time,
    band, mode (`FT8`).

### Failure modes & abort

- **Ctrl-C or `q`** at any time releases PTT cleanly via the
  `PttGuard` in `tx.rs` — even mid-transmission. The drop handler
  spawns a fire-and-forget PTT-off message before the task exits.
- **TX into the DX's slot.** Should be impossible after the
  slot-aware-TX work, but if you ever observe it on a PSKReporter
  spot (DT > 5s on a DX you just answered), file a bug — that's a
  regression of the parity latch.
- **Self-decode ping-pong.** If the autonomous operator ever responds
  to a CQ from your own callsign, abort immediately. The `our_callsign`
  filter in `pancetta-qso/src/qso_manager.rs` is supposed to prevent
  this; a bug there would create an infinite ping-pong on alternating
  parities. Disable autonomous mode in config and report.
- **PTT stuck on.** Should not happen after `Ctrl-C` (PttGuard fires).
  If it ever does, kill the rig PTT manually via the radio's panel
  and report what state pancetta was in.

### Post-session

- Check the full session log:
  ```bash
  ls -lt ~/.pancetta/log/ | head -5
  ```
- Skim for `WARN` / `ERROR` lines:
  ```bash
  grep -E "WARN|ERROR" ~/.pancetta/log/$(date +%Y%m%d).log | head -50
  ```
- Confirm completed QSOs landed in ADIF:
  ```bash
  grep -c "<eor>" ~/.pancetta/qsos.adi
  ```

---

## Day-to-day operation

Once Phase 5 is verified, daily startup is just:

```bash
pancetta
```

The autonomous operator will pick up where it left off (worked-station
history is persisted in the QSO database, rebuilt from ADIF on each
start).

To suspend autonomous operation without exiting pancetta, edit config
and toggle `[autonomous].enabled = false`. The hot-reload in
`pancetta-config` should pick up the change within a few seconds; if
not, restart.

---

## Troubleshooting (operational)

For first-run / install issues, see [`README.md#troubleshooting`](../README.md#troubleshooting).

### Autonomous mode runs but never transmits

- Check `[autonomous].min_dx_score` — too high (close to 1.0) and
  nothing on a quiet band will clear the threshold.
- Check `[autonomous].cq_after_idle_cycles` — if 0 was set, validation
  rejects the config; if very large (50+), CQ takes a long time to
  start on a quiet band.
- Check `~/.pancetta/log/<date>.log` for `Auto-responding to CQ from`
  log lines. If absent, no CQ has cleared filters yet.
- Check `[autonomous].response_filters.allowed_callsigns` — if set,
  *only* those callsigns clear the filter.

### Logged QSOs don't appear in `qsos.adi`

- ADIF write is fail-soft: a failed open logs a `WARN` and the QSO
  goes to the SQLite index only. Check the log for `ADIF writer init
  failed`.
- The SQLite index at `~/.pancetta/qso.db` is rebuildable from ADIF on
  startup. If ADIF is the truth and the DB is stale, delete the DB and
  restart — the index gets replayed from ADIF.

### Pancetta is decoding fewer signals than WSJT-X on the same band

This is the open decoder-sensitivity gap. The synthetic test fixtures
show pancetta beats `ft8_lib` (115.8% on cross-validation), but no
direct on-air comparison against WSJT-X has been measured rigorously.
If you observe this, capture a 15-minute recording (raw audio, both
pancetta and WSJT-X side-by-side) and we'll get a real number.

### Pre-push hook is slow

`scripts/check.sh` runs the full lane (`fmt`, `clippy`, workspace
tests, examples build, `cargo deny`) which takes ~10 minutes on a cold
cargo cache. For documentation- or config-only changes, bypass with:

```bash
git push --no-verify
```

The system prompt forbids this for code changes; reserve it for
genuinely no-Rust commits.

---

## Related docs

- [`README.md`](../README.md) — first-time install, build, config keys at a glance.
- [`docs/CONFIG.md`](CONFIG.md) — every config key, with examples and defaults.
- [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) — crate layout, message bus, data flows.
- [`docs/superpowers/specs/2026-04-27-dx-slot-aware-tx-design.md`](superpowers/specs/2026-04-27-dx-slot-aware-tx-design.md) — slot-aware TX design.
- [`CLAUDE.md`](../CLAUDE.md) — project state, known gaps, development phases.
