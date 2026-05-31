# hb-073 — KiwiSDR Real-Doppler Capture Procedure for K5ARH

**Status:** scoped 2026-05-31. Operator-physical work; execute when an
auroral or TEP opening is available.

**Parent hypothesis:** hb-073 (real-Doppler eval tier), spawned from
mr-006 corpus survey (`research/experiments/2026-05-25-corpus-expansion-survey.md`).

**Goal:** acquire 20-50 slot-aligned 12 kHz mono WAVs from public
KiwiSDR receivers positioned at the **DX end** of auroral / TEP / greyline
propagation paths, so pancetta's eval pipeline gets a tier with **real**
Doppler spread + drift (not the crude multiplicative-cosine synth model).

This unblocks **hb-015** (Doppler-resilient sync search via phase-coherent
integration) and **hb-077** (phase-coherent SDR-IQ eval corpus) — both
listed in the active bank as enabler-blocked on this corpus.

## A. What conditions to capture during

### A.1 Polar / auroral propagation

- **Bands:** 10 m (28.074 MHz) or 15 m (21.074 MHz). Auroral effects are
  strongest on the higher HF bands.
- **When:** late afternoon / evening local at the receive end. Look for
  paths that cross or graze the auroral oval.
- **Trigger:** geomagnetic disturbance, K-index ≥ 5 (preferably ≥ 6),
  Kp prediction warning, visible-aurora forecast. Watch
  https://www.swpc.noaa.gov/ and the K-index ticker on
  https://www.solarham.net/ or any DX-cluster aurora flag.
- **Signature:** rapid signal fading (5-20 Hz flutter), Doppler spread of
  10-50 Hz, watery / hollow tone on SSB. FT8 truth decodes will show
  reduced count vs jt9 baseline.
- **Path geometry:** NA ↔ EU, NA ↔ JA, and any path with great-circle
  bearing crossing the auroral zone (north of ~55° geomagnetic latitude).

### A.2 TEP (Trans-Equatorial Propagation)

- **Bands:** 10 m or 15 m. TEP is strongest on the higher bands; 6 m TEP
  is the spectacular case but pancetta's harness is FT8 HF.
- **When:** evening hours (1800-2200 local at the **path midpoint**).
  Peaks at the equinoxes and around the solar maximum; sporadic outside
  that window. SFI ≥ 100 helps.
- **Geometry:** path midpoint must be within ~10° of the magnetic equator
  AND span ~3000-6000 km north-south. Examples:
  - VK/JA ↔ EU (across the Indian Ocean equator)
  - South America ↔ EU
  - South America ↔ NA (the obvious K5ARH-friendly geometry — capture at
    a Caribbean / S. America KiwiSDR pointed at NA traffic)
- **Signature:** rapid fluttery fade, Doppler spread 5-20 Hz, signals
  appear / disappear in minutes.

### A.3 Greyline

- **Bands:** any HF band — 40 m / 30 m / 20 m bias toward longer wavelengths.
- **When:** the ~30 min window at the correspondent's sunrise or sunset.
- **Signature:** less Doppler spread than auroral/TEP, but DT variance and
  drift from ionospheric tilt. Useful as an "in-between" tier.

### A.4 Operator stance

K5ARH (EM10ch) **does not need to transmit** to capture this corpus. The
operator is selecting a remote receiver and running kiwirecorder.py
against it; the WAV is what the DX-end ionosphere did to the propagating
signal. Local TX activity is independent.

## B. KiwiSDR remote-receiver selection

### B.1 Why remote receivers

K5ARH's own rig captures whatever propagation arrives at EM10ch.  For
**Doppler-rich** captures we want the receiver close to the DX end of the
auroral / TEP path so the spread/drift signature dominates the channel.

### B.2 Public KiwiSDR directories

- **Primary index:** http://kiwisdr.com/public/ — sortable by location,
  band, snr.
- **Mirror:** http://rx.linkfanel.net/ (KiwiSDR-only, cleaner UI than the
  master list)
- **Broader SDR list (not all kiwirecorder-compatible):** http://websdr.org/

When a Kiwi is listed at `http://<hostname>:8073/`, kiwirecorder talks to
it via the same hostname on port 8073.

### B.3 Candidate sites by propagation class

These are **starting points** — confirm online status and current activity
on http://kiwisdr.com/public/ before each capture session. KiwiSDR
operators change, sites go offline, etc.

**Auroral (high-latitude, EU-NA aurora-zone receivers):**
- Northern Norway: search public list for "Norway", "Tromsø", "Bodø",
  "Lofoten". Example historical: `bod-sdr.no:8073`, `tromso-kiwisdr.no:8073`.
- Finland / Sweden: search "Finland", "Sweden", "Lapland".
- Iceland: search "Iceland" — TF-prefixed sites.
- Greenland / Svalbard: rare but maximally aurora-exposed when up.
- Alaska: any "Alaska" / "Anchorage" / KL7 site for the NA-Pacific aurora
  view.

**TEP (equatorial-Atlantic / equatorial-Pacific receivers):**
- Caribbean: KP4 (Puerto Rico), PJ (Curaçao). Often listed by callsign.
- Brazil: PY-prefixed sites — São Paulo, Rio.
- Pacific equatorial: KH6 (Hawaii) for trans-Pacific TEP; rarer FK / VK9
  sites.
- West Africa: 9G / TR / TZ — rarely listed but maximally on-equator.

**Greyline (any well-located site at terminator):**
- Pick a candidate that's at sunrise or sunset at the moment of capture;
  the public list shows local time at the receiver.

### B.4 Selection checklist per session

Before launching kiwirecorder, confirm:

1. Site responds to the JS8Call-style band-display at
   `http://<hostname>:8073/` in a browser.
2. Slot count: KiwiSDRs allow ~4-8 simultaneous clients. The public list
   shows "X/N slots free." If 0 free, pick a different site.
3. The site has an HF antenna with FT8-band coverage. Most do; some are
   LF-only or 60-m-only.
4. Site's local time vs band — 10/15 m needs daylight or twilight at the
   site for ionospheric support.

## C. kiwirecorder.py capture command

### C.1 Tooling install (one-time, on the operator's Mac or Windows MiniPC)

```bash
# Clone the KiwiSDR client library (provides kiwirecorder.py).
cd ~/src     # or wherever the operator stages tools
git clone https://github.com/jks-prv/kiwiclient.git
cd kiwiclient
# Python 3 required. Install kiwiclient's dependencies if any
# (the repo's README is the source of truth; current dependency set
# is small — numpy + samplerate are the typical ones).
python3 -m pip install --user numpy samplerate
```

There is **no** pancetta-side wrapper script today. If we want one later,
it would live at `pancetta-research/scripts/kiwi-capture.sh` and call
`kiwirecorder.py` with the canonical flags below — but a wrapper adds
no value until the operator has run a few sessions and identified the
ergonomic gaps.

### C.2 Canonical capture command

```bash
python3 ~/src/kiwiclient/kiwirecorder.py \
  -s <kiwisdr-hostname> -p 8073 \
  -f 14074 -m usb --lp 200 --hp 3000 \
  --resample 12000 \
  --tlimit 1800 \
  --filename "ft8-$(date -u +%Y%m%dT%H%M%SZ)-<site-tag>"
```

Flag-by-flag (pancetta-eval requirements):

| Flag | Value | Why |
|---|---|---|
| `-s` | hostname | KiwiSDR host from B.3 |
| `-p` | `8073` | Standard KiwiSDR port |
| `-f` | `14074` (or `21074`, `28074`, `7074`) | FT8 dial freq in **kHz**; choose the band per propagation class (10 m or 15 m for auroral/TEP) |
| `-m` | `usb` | FT8 = USB |
| `--lp` `--hp` | `200` `3000` | Filter pass-band 200-3000 Hz (covers all FT8 audio offsets) |
| `--resample` | `12000` | **Required** — pancetta-research expects 12 kHz mono i16 |
| `--tlimit` | `1800` | 30-min session per WAV; bump to `3600` for hour-long sessions |
| `--filename` | `ft8-<UTC>-<site>` | UTC timestamp + site tag. kiwirecorder appends `.wav` automatically; defaults to mono 16-bit. |

Verify the result is 12 kHz / mono / i16 with `soxi <file>.wav` or
`ffprobe <file>.wav` before ingestion — pancetta's curate / eval will
reject anything else.

### C.3 Slot-aligned launch (FT8 :00 / :15 / :30 / :45 boundaries)

kiwirecorder starts when invoked. FT8 slots start at :00 / :15 / :30 / :45
of each minute. To land aligned, pause until ~3-5 seconds **before** the
next slot boundary (slack covers TCP setup + Kiwi negotiation; ~3s is the
empirically-honest pad).

Inline aligner — paste before the kiwirecorder invocation:

```bash
python3 -c '
import time, datetime as dt
now = dt.datetime.utcnow()
sec_into_min = now.second + now.microsecond / 1e6
# Next 15-second boundary, minus 3s slack for connect:
slots = [12, 27, 42, 57, 72]   # 72 wraps to next minute :12
target = next(s for s in slots if s > sec_into_min)
sleep_for = target - sec_into_min
print(f"sleeping {sleep_for:.2f}s until t-3 before next slot")
time.sleep(sleep_for)
'
```

Then immediately:

```bash
python3 ~/src/kiwiclient/kiwirecorder.py -s <host> ... # as above
```

### C.4 Session plan

- **Per opening (one auroral / TEP / greyline window):** capture 2-4
  back-to-back 30-min sessions on different bands (10 m + 15 m, or one
  band + a second site for cross-comparison). Yields 4-8 WAVs.
- **Across openings:** target 5-8 distinct opening windows over days /
  weeks until 30-60 WAVs are in the bank. Diversity matters more than
  count — different geomagnetic conditions, different paths, different
  TOD.
- **Discard criteria:** if the WAV has < 3 jt9-baseline decodes across
  its 30-min span, the path was probably closed for that session. Keep
  the WAV (it may still expose synthesis differences) but don't count it
  toward the 30-WAV target.

## D. Ingestion procedure (post-capture)

These steps mirror the existing curated-tier ingestion flow used for
`hard_200` and `wild_50`.

### D.1 Stage WAVs

```bash
# Standard recordings dir the curate binary already scans:
mv ft8-*.wav ~/.pancetta/recordings/
```

### D.2 Generate jt9 baseline for each new WAV

```bash
# pancetta-research's baseline tool caches jt9 decode output keyed by
# WAV SHA-256 under research/baselines/ft8/<sha>.json. Re-run for the
# new files only:
cd /Users/thagale/Code/pancetta
./scripts/research-env.sh --baseline ~/.pancetta/recordings/ft8-*.wav
```

(If the helper flag above doesn't exist yet, the operator can invoke
`cargo run --release -p pancetta-research --bin baseline-jt9 -- <wav>` per
WAV. The wrapper covers the common case; the per-file invocation is the
fallback.)

### D.3 Run pancetta against each WAV

```bash
# Plain decode + scorecard against the new captures, scoped to the
# wild-doppler candidates only:
cd /Users/thagale/Code/pancetta
cargo run --release -p pancetta-research --bin eval -- \
  --tier wild-50 \
  --mode ft8 \
  --output /tmp/wild-doppler-survey.json
```

(During the diagnostic phase, the `wild-50` tier auto-picks up any new
WAVs the curate binary scored; once a dedicated `wild-doppler-50` tier
is curated per D.5, switch to that.)

### D.4 Diagnostic — find the Doppler-rich subset

For each new WAV compare `jt9 baseline count` vs `pancetta recovered`.
Flag a WAV as "Doppler-rich" if:

- jt9 produced ≥ 5 decodes AND
- pancetta missed ≥ 30% of them (i.e. recovery ≤ 0.70)

These are the WAVs where the channel exhibits the synth model's blind
spot. Use the existing `per_wav_top_failures` field on the scorecard to
read recovery per WAV.

### D.5 Curate the `wild-doppler-50` tier

```bash
cd /Users/thagale/Code/pancetta
cargo run --release -p pancetta-research --bin curate -- \
  --source-dir ~/.pancetta/recordings \
  --output-prefix research/corpus/curated/ft8 \
  --filter doppler-rich \
  --top-n 50 \
  --label wild_doppler_50
```

(The curate binary today produces `hard_*`, `wild_50`, `wild_100`. The
`--filter doppler-rich` flag is **not yet implemented**; an interim path
is to hand-edit a candidate list and emit
`research/corpus/curated/ft8/wild_doppler_50.manifest.json` matching the
shape of `wild_50.manifest.json`. Adding the flag to `curate.rs` is a
follow-up — out of scope for the scoping session.)

Once the manifest exists at
`research/corpus/curated/ft8/wild_doppler_50.manifest.json`, the eval
tier stub added in this session (see Deliverable 2 in the scoping journal)
picks it up automatically: `cargo run --release -p pancetta-research
--bin eval -- --tier wild-doppler-50 --output /tmp/card.json`.

### D.6 Truth check the tier

Run the new tier against the current `main` decoder and confirm the
scorecard reports reasonable numbers (recovery, novels, fixture/synth
parity). Treat the first run as a **calibration** — the tier value
materialises when used as the denominator for hb-015.

## E. What this unlocks

- **hb-015** (Doppler-resilient sync search via phase-coherent
  integration, priority 0.38) is explicitly blocked on hb-073 in the
  bank. With the `wild_doppler_50` tier live, hb-015 has a real
  denominator and the bank entry can bump to ~0.42 per mr-006's note.
- **hb-077** (phase-coherent SDR-IQ eval corpus, priority 0.20) is a
  sibling enabler whose scope overlaps with the KiwiSDR captures here.
  The same captures (or a parallel set with a phase-coherent SDR like
  HackRF + GPSDO) give hb-077 its corpus. With the procedure here,
  hb-077's operator-pending block is partially lifted — KiwiSDR feeds
  IQ via `--ncomp`, so the same recording infrastructure that produces
  audio WAV can produce IQ pairs.
- **Future Doppler hypotheses** (frequency-ramp Costas, drift-tracking
  sync, etc.) inherit the same denominator.

## F. Operator action items (checklist)

- [ ] Install kiwiclient + numpy + samplerate on the capture host.
- [ ] Bookmark http://kiwisdr.com/public/ and verify 2-3 candidate sites
      per propagation class (auroral, TEP, greyline) are currently live.
- [ ] Watch for an auroral opening (K ≥ 5, evening) or TEP opening
      (10 m strong on equatorial paths, evening at midpoint).
- [ ] Run the slot-aligned aligner + kiwirecorder one-liner, target
      30-min capture, pick a remote receiver at the DX end of the path.
- [ ] Repeat across 5-8 distinct openings until 30-60 WAVs accumulated.
- [ ] Move WAVs into `~/.pancetta/recordings/`.
- [ ] Run jt9 baseline + pancetta eval; flag Doppler-rich WAVs.
- [ ] Hand-curate `research/corpus/curated/ft8/wild_doppler_50.manifest.json`
      (or implement `curate --filter doppler-rich` as a small follow-up).
- [ ] Notify Claude: "wild-doppler tier live, hb-015 unblocked." That
      triggers the next research cycle to pick up hb-015 at its bumped
      priority.

## References

- `research/experiments/2026-05-25-corpus-expansion-survey.md` — mr-006
  parent survey.
- `research/hypothesis_bank.md` — hb-073, hb-015, hb-077 entries.
- `pancetta-research/src/bin/eval.rs` — tier dispatch; `wild-doppler-50`
  stub added in the scoping commit.
- KiwiSDR client docs: https://github.com/jks-prv/kiwiclient
- KiwiSDR public list: http://kiwisdr.com/public/
- NOAA SWPC (Kp / aurora): https://www.swpc.noaa.gov/
