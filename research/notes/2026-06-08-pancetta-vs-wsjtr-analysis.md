# Pancetta vs wsjtr — deep architectural & strategic analysis

**Date**: 2026-06-08 (Batch 45)
**Subject**: pancetta vs Brian Bodiya KC1WIH's `wsjtr` (https://github.com/bodiya/wsjtr)
**Recommendation**: **stay separate, cross-pollinate aggressively, treat wsjtr's docs as primary references, do NOT merge code (license blocker)**

## Executive summary

These two Rust FT8 decoders are **complementary, not competitive**.

- **wsjtr** (Brian Bodiya, KC1WIH) — meticulously-documented narrow-scope
  WSJT-X port. ~98-99% parity via cross-sequence A7 + AP-2. Decoder-only.
  GPLv3.
- **pancetta** — 12-crate autonomous on-air FT8 station. Richer research
  apparatus, neural OSD, ~12 batches of bootstrap-CI-gated sensitivity
  work, six layers of FP filtering. Has the autonomous station logic
  wsjtr explicitly avoids. MIT/Apache-2.0.

## License = structural blocker for merge

- **Pancetta**: MIT OR Apache-2.0 (workspace)
- **wsjtr**: GPL-3.0-only (derived from WSJT-X GPLv3)

Pancetta cannot adopt GPLv3 code without relicensing the entire
workspace. That would force every downstream user to GPLv3 —
incompatible with cqdx.io and any commercial/mobile future.

Wsjtr could absorb pancetta (MIT-or-Apache is GPL-compatible) but
reverse is structurally infeasible.

**Idea exchange is fine** — algorithms, file structures, parameter
values, design patterns aren't copyrightable. *Code copy is not.*

## Scope comparison

| Dimension | pancetta | wsjtr |
|---|---|---|
| Primary goal | Autonomous on-air FT8 station | Decode library + supplement |
| Crates | 12 | 6 |
| Decoder LOC | ~13.5k | ft8core + jt9r split |
| QSO state machine | YES (pancetta-qso) | NO |
| Coordinator/runtime | YES (~6.5k LOC) | NO |
| Hardware coupling | YES (hamlib, audio) | NO |
| Mobile use case | NO | YES (FT-Activ8 Android) |
| WSJT-X parity claim | 5-10% of decode rate | 98-99% |
| Tests | ~295 + proptest + hard-200/1000 + bootstrap-CI | per-window comparison |

## Decoder algorithm differences

### wsjtr is AHEAD on classical decode plumbing

| Stage | pancetta | wsjtr | Verdict |
|---|---|---|---|
| Costas sync | 1×7-symbol correlation | **3-position correlation + sync_bc/sync_abc partials** | wsjtr ↑ |
| Sync normalization | dB neighbor diff | **40th-percentile across freq bins** | wsjtr ↑ |
| Refinement | NMS only | **5×5 grid (dt, freq) refinement via time-domain Goertzel** | wsjtr ↑ |
| Downsampler | spectrogram bins | **cached 192k-point FFT + per-candidate band extract + 3200-point IFFT (200 Hz baseband)** | wsjtr ↑ (significant gap) |
| BP precision | f32 LLR domain | **f64 tanh domain** | wsjtr ↑ |
| OSD init | LLR-based reliability | **zsum snapshots at iter 1 & 2** | wsjtr ↑ |

### pancetta is AHEAD on FP discipline + research apparatus

| Component | pancetta | wsjtr |
|---|---|---|
| FP filters | **6 layers** (hb-052 callsign continuity + hb-058 /R + hb-103 content score + hb-217 RR73 fix + degenerate_grid + digit_run) | none |
| Neural OSD | **YES** (80KB weights, 600× OSD trial reduction) | none |
| FP discipline | **bootstrap-CI gates every graduation** | none |
| Research corpus | hard-200 / hard-1000 / lid_of_band | per-WAV comparison |

### Cross-sequence A7 — wsjtr's signature mechanism

- **pancetta hb-048**: SHELVED — was within-WAV mechanism, didn't surface truths
- **wsjtr A7**: cross-WINDOW — uses callsigns from PREVIOUS same-parity 15s slot
- **THESE ARE DIFFERENT MECHANISMS** — pancetta has not implemented cross-window
- Per wsjtr: +5-6% unique decodes

This is the hb-237 we promoted in Batch 44.

## Top 3 extractions for next 2-3 batches

### 1. wsjtr `sync_bc` partial Costas metric (Batch 46 target) — NEW

Port "use only Costas positions 2 and 3 when position 1 falls outside
the recorded window" trick. Directly targets pancetta's slot-edge
negative-dt 48.3% recall miss bucket (Batch 40 finding).

Source: `wsjtr docs/jt9r.md §Sync Detection`
- Algorithm is publicly documented — pancetta writes implementation
  from docs, not GPL code (license-clean)
- Effort: ~1-2 sessions
- Expected: +50-150 RR73-class slot-edge truths on hard-1000
- **NEW BANK ENTRY: hb-242 (sync_bc partial Costas metric)**

### 2. Cross-sequence A7 — true wsjtr-style (Batch 46-47)

Already promoted as hb-237 (priority 0.60) in Batch 44. Design notes
in `research/notes/2026-06-08-hb237-cross-seq-a7.md`.

Plan-sized (5-6 sessions, ~550 LOC). Big strategic win because it
directly addresses the autonomous-station QSO loop.

### 3. wsjtr-style cached-bandpass downsampler + fine-sync (Batch 48)

Port WSJT-X's `ft8_downsample.f90` algorithm. Pancetta currently uses
spectrogram bins throughout decode; wsjtr does true complex baseband at
200 Hz. **This is the single biggest documented sensitivity gap between
pancetta and WSJT-X.**

Source: `wsjtr docs/jt9r.md §Downsampling`
- Effort: ~3-4 sessions
- Expected: closes 1-2 dB of the 5-10% WSJT-X sensitivity gap
- **NEW BANK ENTRY: hb-243 (cached-bandpass downsampler)**

### Lower-priority but cheap

- **f64 tanh-domain BP** (1-batch A/B test) — wsjtr's BP precision win
- **40th-percentile sync normalization** (1-batch) — wsjtr's adaptive threshold
- **zsum-snapshot OSD initialization** (1-batch) — wsjtr's OSD setup

## What pancetta has that wsjtr should know about

If K5ARH and Brian connect:

1. **FP-filter pipeline** (hb-052/058/103/217) — addresses wsjtr's
   documented Feb-2026 false-decode root causes
2. **Neural OSD** — 600× OSD trial reduction at 80KB weights;
   pancetta is permissive-licensed so wsjtr CAN copy the code directly
3. **Bootstrap-CI graduation policy** — gates against shipping noise as feature

## Strategic recommendation

Mainline WSJT-X has had no FT8 decoder commits in 12+ months. This is
a **succession-of-the-decoder moment**, and two credible Rust ports
are now pulling in different directions:

- **wsjtr** → fidelity reference port (98-99% parity, mobile-ready, GPLv3)
- **pancetta** → operational autonomous station (priority scoring,
  multi-stream TX, neural OSD, FP discipline, MIT/Apache-2.0)

**Lean into the differentiation.** Don't try to out-WSJT-X wsjtr —
losing race because wsjtr's narrower scope moves faster on parity.

**Own the autonomous-station niche** wsjtr explicitly avoids ("AP
strategies 3-6 not implemented because they require QSO state
tracking"). Pancetta HAS QSO state tracking. AP modes 3-6 are
pancetta's natural extension and would close most of the remaining
5-10% gap *while leveraging the autonomous-operator infrastructure
that already exists*.

## Practical next steps

1. **Add wsjtr to project_meatspace_pending watchlist** — subscribe to
   GitHub releases, watch `docs/jt9r.md` for new content
2. **Reach out to KC1WIH** for cross-link — "I'm building an autonomous
   station; you've got the cleanest decoder port; here's what I've
   found on FPs and neural OSD, want to compare notes?"
3. **Do NOT** restructure pancetta's workspace to match wsjtr's. 12-crate
   split is justified by autonomous-station scope.
4. **Do** clean up pancetta-ft8 codec/decoder/encoder split when
   pancetta goes to crates.io (wsjtr provides a working template)

## Strategic posture (one sentence)

**Two cooperating Rust FT8 projects with non-overlapping scopes,
cross-citing each other's docs, sharing algorithmic findings, never
merging code, never depending on each other.**

That is the right shape for a small-community open-source ecosystem
post-WSJT-X stagnation.

## Source references

- `/Users/thagale/Code/pancetta/Cargo.toml` — workspace structure + license
- `/Users/thagale/Code/pancetta/pancetta-ft8/Cargo.toml` — decoder deps
- `/Users/thagale/Code/pancetta/pancetta-ft8/src/decoder.rs` — 7420 LOC main decoder
- `/Users/thagale/Code/pancetta/pancetta-ft8/src/a7.rs` — shelved within-window A7 (hb-048)
- `/Users/thagale/Code/pancetta/pancetta-ft8/src/osd.rs` — OSD with neural-OSD bias hook
- `/Users/thagale/Code/pancetta/research/hypothesis_bank.md` — 252 hypotheses, 6400+ lines
- `/Users/thagale/Code/pancetta/CLAUDE.md` — architecture overview
- `/Users/thagale/Code/pancetta/LICENSE-MIT` — MIT half of dual license
- wsjtr GitHub: https://github.com/bodiya/wsjtr (GPLv3)
- wsjtr `docs/jt9r.md` — primary reference: sync_bc, cached-bandpass downsampler, zsum OSD init
- wsjtr `docs/wsjtr.md` — primary reference: multipass-from-raw, DT-refinement-during-subtract
- wsjtr `docs/cross_sequence_decoding.md` — primary reference: cross-window A7

Generated by general-purpose research agent (full transcript at
`/private/tmp/.../a4485db86b4f29448.output`).
