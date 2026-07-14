# Pancetta User Guide

For the licensed ham who just built pancetta and wants to get on the air.
This guide is task-oriented: your first 5 minutes, your first QSO, then
"how do I…" answers. (Owner-operator procedures live in `docs/RUNBOOK.md`;
every config key lives in `docs/CONFIG.md`.)

---

## Your first 5 minutes

You've run `cargo build --release` (with `--recursive` on the clone — the
build warns if the C decoder is missing). Now:

### 1. Run the wizard (~2 minutes)

```bash
./target/release/pancetta
```

On first run (no config yet) pancetta walks you through:

- **Station** — callsign, Maidenhead grid (e.g. `FN42`), TX power.
- **Audio** — pick your rig's USB CODEC from a numbered list for **both**
  input and output. This is the #1 thing to get right: the wrong input
  device means zero decodes; the wrong output device means PTT keys the
  rig while your FT8 tones play through the laptop speakers.
- **Rig CAT control** — answer `y` if your radio is connected by USB now.
  Pancetta lists your serial ports (with USB product names), asks for the
  rig model (e.g. `FTdx10`, `IC-7300`), baud rate (38400 for a Yaesu
  FTdx10), and PTT method (**CAT** is right for most modern rigs).
  Answer `n` to stay decode-only; you can add the rig any time with
  `pancetta setup`.

Everything is saved to `~/.pancetta/pancetta.toml`.

### 2. Run the doctor (~10 seconds)

```bash
pancetta doctor
```

Seven independent checks — config, system clock vs NTP, audio input
device, a 2-second audio-level capture, the C decoder, rigctld, and the
git submodule — each with a one-line fix when it fails. **Green doctor =
you will decode.** The two classic first-run killers it catches:

- **Clock offset ≥ 1 s.** FT8 slots are aligned to UTC; a drifted clock
  fails every decode while looking otherwise healthy.
- **Flat-line audio.** The input device exists but no signal reaches it
  (OS-muted, wrong device, rig AF at zero).

### 3. Start and watch the first decode (~30 seconds)

```bash
./target/release/pancetta
```

Tune the rig to an FT8 dial frequency (20 m: **14.074 MHz**, USB). Decodes
arrive in bursts at the end of each 15-second slot — within two slots the
**Band Activity** panel should fill. If it doesn't: `pancetta doctor`
again, then `Shift+D` (diagnostics history) and `Shift+S` (station health)
inside the TUI.

That's the whole path. With a rig configured you are now one keypress from
transmitting.

---

## Your first QSO

FT8 exchanges are fixed-format and take ~1 minute. Pancetta runs the
sequence for you; you pick the station.

### Reading the Band Activity panel

Each row is one decoded transmission: UTC time, SNR (dB), time offset,
audio frequency, and the message. `CQ` rows are stations asking to be
called. Move with `Up`/`Down` (or jump panels with `1`–`5`:
Band Activity / QSO Status / Callers / DX Hunter / TX Placement).

### Calling: Space

Select a CQ row and press **Space**. Pancetta answers with your grid in
the next appropriate slot and then runs the standard exchange (your
callsign here shown as K1ABC):

```
CQ W1AW FN31          ← they call CQ
W1AW K1ABC EM13       ← you answer with your grid       (pancetta sends)
K1ABC W1AW -07        ← they send your report
W1AW K1ABC R-12       ← you roger + their report        (pancetta sends)
K1ABC W1AW RR73       ← they confirm; QSO is complete
W1AW K1ABC 73         ← courtesy sign-off               (pancetta sends)
```

Watch it in the **QSO Status** panel (`2`). The QSO logs automatically to
`~/.pancetta/qsos.adi` (+ the query index `qso.db`). Space is
context-aware: if the selected station is already mid-exchange with you,
it re-sends the *correct next message*, not your grid.

If someone answers **your** CQ, they appear in the **Callers** panel
(`3`) — select and press **Enter** to reply at the right step.

### Keys you must know before transmitting

| Key | Effect |
|---|---|
| `h` | **Halt current TX** — drops PTT within ~150 ms |
| `Shift+Q` | **EMERGENCY STOP** — abort TX, autonomous off, TX policy → Disabled |
| `g` | Cycle TX policy: **Full → Respond-only → Disabled** |
| `k` | Abort the selected QSO (QSO Status panel only) |
| `r` | Re-send your last message in the selected QSO (QSO Status panel only) |
| `Esc` | Clear the stop banner / dismiss any overlay |

Pancetta refuses to CQ as `N0CALL`, the rig interface is **disabled by
default**, and autonomous mode is **off by default** — nothing transmits
until you configured a rig and pressed a key that means "transmit".

---

## How do I…

### …work a specific DX station?

The **DX Hunter** panel (`4`) scores every decoded station by what *you*
need (new DXCC, grid, POTA/SOTA, rarity). Select and press **Space** — same
exchange as above, but pancetta places your TX and picks the slot parity to
match theirs. `c` starts a repeating CQ of your own; `s` stops it.

### …hold my TX frequency vs. letting pancetta pick?

- `f` toggles **HOLD** (your offset is pinned) vs **AUTO** (pancetta picks
  a clear one).
- `t` auto-finds a clear 25 Hz-aligned offset and moves your cursor there.
- `←`/`→` or `[`/`]` nudge the TX offset ±50 Hz; `o` types an exact offset
  in Hz (200–2900; blank = back to Auto; setting one implies Hold).
- `Shift+F` sets the **dial** frequency (and optional split TX dial) via CAT.

### …enable autonomous operation (the supervised way)?

Press `a` to toggle autonomous mode (or set `[autonomous] enabled = true`
in `~/.pancetta/pancetta.toml`; `Shift+P` pauses/resumes). Pancetta then
hunts, calls, completes, and logs QSOs using the priority weights under
`[autonomous.priorities]` (`needed_dxcc`, `needed_grid`, `pota_sota`,
`rarity`, `signal_strength`, penalties — see `docs/CONFIG.md`).

**Compliance framing (US operators; see
`docs/fcc-part97-compliance.md`):** with you present and able to
intervene — at the keyboard, or watching over SSH/screen share — this is
*local/remote control* under §97.109 and is fully compliant on the normal
FT8 frequencies, including originating CQ. **Unattended** operation is
*automatic control* (§97.109(d)), and the standard FT8 frequencies are
outside §97.221(b)'s automatic-control segments — so an unattended station
must at most **respond** to calls (§97.221(c)), never originate CQ.
Practical rule: **stay present while autonomous runs** (the ARRL's
contemporaneous-initiation posture), and if you must step away, press `g`
until the policy reads **Respond-only** — or `Shift+Q` to stop TX
entirely. The licensee remains responsible either way (§97.103).

### …upload my logs (QRZ / LoTW / ClubLog)?

Every QSO is appended to `~/.pancetta/qsos.adi` (durable ADIF — back it
up). For live per-QSO uploads, enable the blocks in
`~/.pancetta/pancetta.toml` (then `chmod 600` the file — credentials are
plaintext):

```toml
[network.qrz_logbook]
enabled = true
api_key = ""        # logbook Settings → API access key (per-logbook key)

[network.clublog]
enabled  = true
email    = ""       # your ClubLog account email
password = ""       # an Application Password is recommended
callsign = ""       # empty = each QSO's own station call
api_key  = ""       # ClubLog application API key
```

**LoTW:** per-QSO upload is wired via TQSL signing (not a raw ADIF POST) —
enable it in `[network.lotw]`:

```toml
[network.lotw]
enabled          = true
tqsl_path        = ""   # path to your installed tqsl binary
station_location = ""   # must match a "Station Location" name in TQSL
```

Each completed QSO is signed with your TQSL certificate (pancetta shells
out to `tqsl`) and uploaded — no raw ADIF POST. If you'd rather sign
manually, or don't have `tqsl` installed, point TQSL/WSJT-X's importer at
`~/.pancetta/qsos.adi` instead. Bulk export with filters:
`pancetta export --output mylog.adi`.

### …switch bands or modes?

- `=` / `-` step the band up/down (CAT moves the dial; active QSOs are
  torn down safely — turning the rig's physical dial does the same).
- `Shift+F` for any arbitrary dial frequency, including split.
- `Shift+M` cycles the station operating mode: **FT8 → FT4 → FT2** (FT2
  needs the `ft2` cargo feature, off by default — without it the label
  switches but timing stays FT8). It can be refused while a QSO is in
  flight — finish or `k` the QSO first.
- `e` cycles decode effort (Eco → Standard → Deep → Max → Auto) if you're
  CPU-bound.

### …use Hound mode (work a DXpedition Fox)?

Select the Fox in the DX Hunter panel (`4`) and press **Shift+H**.
Pancetta engages the WSJT-X Fox/Hound convention: calls above 1000 Hz,
then obeys the Fox's QSY instruction after being called. `Shift+X`
toggles Fox mode itself (running your own pileup — read
`docs/superpowers/specs/` on Fox mode before using this in anger).

### …see why something isn't working?

1. `pancetta doctor` from a shell — config, clock, audio, decoder, rig.
2. `Shift+D` in the TUI — retained diagnostics history (why TX was
   dropped, why a QSO failed, rig errors).
3. `Shift+S` — the station-health panel (one screen: is the station
   healthy right now?).
4. Badges in Station Info: `⚠ TX→system default` (TX audio going to your
   speakers, not the rig) and `⚠ RX→fallback device` (configured input
   device not found).
5. Logs: `~/.pancetta/logs/` (daily rotation, 14 kept).

Press `?` any time for the complete key list.
