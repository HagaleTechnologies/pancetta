# README & Visual Identity Overhaul — Design

**Status:** Approved (design), pending implementation plan
**Date:** 2026-08-20

## Problem

Pancetta's README is technically thorough (~3,500+ words) but has zero visual
assets — no screenshots, no GIFs, no logo — and buries the hook (what the
project does, why it's different) under prerequisite tables and install
instructions. Research into how comparable open-source TUI/CLI projects
present themselves (see summary below) shows this is the single biggest gap
relative to projects that convert visitors into users.

## Research summary

Three research passes (TUI/CLI README conventions, landing-page vs.
README-only tradeoffs, ham-radio software adoption channels) converged on:

- **Visual within ~20 words of the top** is the norm for successful TUI
  READMEs (zellij, glow, superfile); ours has none at any depth.
- **800–1,500 words** is the converting length tier; ours is ~2.5x that.
  Detail (troubleshooting, provenance essay, workspace table) belongs in
  `docs/`, not the README.
- **VHS** (`charmbracelet/vhs`) is the standard tool for CLI/TUI demo GIFs —
  scriptable `.tape` files, CI-regeneratable via `vhs-action`, avoids stale
  screenshots.
- **Landing page is premature** at this project stage — every studied
  project (zellij, helix, atuin, ratatui) added a site only after a stable
  core loop and external contributors. Revisit once cqdx.io integration
  drives non-GitHub visitors.
- **GitHub wiki is an anti-pattern** for canonical docs (no PR review, no
  versioning with code). `docs/` stays the source of truth.
- **Ham-radio-specific trust signals**: author callsign (K5ARH) up front,
  PSKReporter screenshots as proof-of-performance (the community's currency,
  stronger than benchmark numbers), and an explicit "control operator
  remains present, always interruptible" framing for autonomous TX — this
  defuses the FCC/etiquette objection that sank MSHV's reputation on
  band-discipline grounds (not a legality argument, an etiquette one).
  Nobody mocks WSJT-X/K1JT-tier work; the honest contrast target is
  JTDX/PSK-era tooling, not the whole field.
- **Discovery channels** for this audience run through groups.io, YouTube
  (KM4ACK/Temporarily Offline for the headless-Pi niche specifically),
  awesome-hamradio lists, and word of mouth — not generic SEO.

## Scope

Three phases, all in scope; Phase 3 items are decided individually rather
than as a bundle.

### Phase 1 — Visual assets + replay-demo mode

Create `assets/` in the repo: 3–4 static TUI screenshots, one hero GIF
(10–15s: decodes arriving, priority scores, a QSO completing), 2–3
per-feature GIFs placed next to the claims they prove, and a PSKReporter
map capture from the existing FTdx10 on-air validation.

Tooling: VHS, `.tape` scripts committed to the repo, `vhs-action` wired into
CI so GIFs regenerate rather than rot.

**Replay-demo mode.** VHS needs deterministic, scriptable input. Investigation
found:
- `pancetta --wav <file>` (`pancetta/src/coordinator/mod.rs:1618`) already
  exists but short-circuits before the pipeline starts — it's decode-only
  stdout, no TUI, no QSO engine, no priority scores.
- `PANCETTA_STUB_AUDIO=1` (`pancetta/src/coordinator/audio.rs:67-124`)
  already injects synthetic audio at real-time cadence into the same
  `audio_to_dsp_tx` channel a real `cpal::Device` would feed — downstream of
  hardware entirely, and already wired through the full
  audio→DSP→FT8→QSO→TUI pipeline.
- WAV read/resample helpers already exist in `wav_playback.rs` and are
  directly reusable.
- **Determinism caveat**: slot parity and every displayed timestamp derive
  from `Utc::now()`/`SystemTime::now()` with no injectable clock — decode
  *content* reproduces run-to-run (no RNG in the decode/QSO-decision path),
  but frame-for-frame TUI output does not. A VHS `.tape` must key waits on
  visible text/state (`Wait /DE K5ARH/`), not literal frame timing.

**Decision: build `--replay <wav-dir>` (small effort — composes two
existing code paths, no new crate architecture) rather than hand-recording
a live session or using the standalone `--wav` flag.** Hand-recording works
today with zero engineering cost but isn't reproducible for CI regeneration;
`--wav` shows no TUI content at all. `--replay` is the only path that gets a
full TUI (waterfall, priority scores, live QSO) into a scriptable, re-runnable
recording, and the corpus used for `docs/decoder-comparison.md` is
immediately available as input.

### Phase 2 — README rewrite (~1,200 words)

Structure: tagline + K5ARH attribution → minimal badges (CI, license) →
hero GIF → pain-point pitch (one binary vs. WSJT-X + logger + GridTracker +
cluster client juggling) → "why pancetta" bullets (decode benchmark,
priority engine, multi-stream TX, headless/Pi-class, `doctor`) →
control-operator/autonomy framing → compressed quick start → "why not
(yet)" honesty section → docs links → tightened acknowledgments.

Content displaced from the README gets real homes:
- Troubleshooting section → `docs/TROUBLESHOOTING.md`
- Provenance/clean-room essay → `docs/PROVENANCE.md`
- Full workspace crate table → pointer to `docs/ARCHITECTURE.md` (already
  has the detailed version)

Logo: a mark selected from the exploration canvas
(https://claude.ai/code/artifact/d6fc7587-70b7-4d25-a734-7fc79cdc4f70) —
12 concepts across 4 directions (monogram/waterfall, pancetta-pun icon,
pure ham-radio iconography, terminal-native marks). Selection and any
refinement pass happens before Phase 2 is implemented.

### Phase 3 — Distribution (decide item by item)

Candidates, not commitments:
- PR to `DD5HT/awesome-hamradio`
- Prebuilt release binaries via `cargo-dist` (biggest lever for non-Rust
  hams — "clone and cargo build" filters out most of the hobby)
- groups.io group + posts to `SoftwareControlledHamRadio`/`DigitalHamRADIO`
- Mastodon ham instances (mastodon.hams.social, mastodon.radio)
- Headless-Pi demo aimed specifically at KM4ACK/Temporarily Offline
- Domain registration — explicitly declined for now; cqdx.io covers hosting
  if a landing page is ever warranted

## Out of scope

- A dedicated landing page / docs site (revisit post-cqdx.io integration)
- GitHub wiki as canonical docs
- Any change to the autonomous-operator TX-arm gating logic itself — this
  work only *documents and demonstrates* existing invariants
  (fail-closed arm gate, parity discipline, drop-stale-TX), it does not
  change them

## Open decisions carried into implementation

- Final logo pick (and whether it needs a professional polish pass after
  the exploration canvas)
- Exact Phase 3 items to execute now vs. defer
