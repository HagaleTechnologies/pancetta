# World-Class Onboarding & How-To-Use — Design Spec

**Date:** 2026-07-12
**Status:** Approved direction; Phase 1 has a detailed plan
(`docs/superpowers/plans/2026-07-12-onboarding-phase1-funnel.md`); Phases 2-4
each get their own plan when picked up.

## Goal

**(Clarified by the operator 2026-07-13:)** the 5-minute clock starts **when the
build completes**: from that moment, a licensed ham must be able to configure
the station and be **on the air** (rig connected, able to work a QSO) in under
5 minutes — wizard → doctor-green → decoding → TX-capable, no source-diving, no
dead ends. Getting *to* a completed build fast (prebuilt binaries, honest build
docs) is a supporting goal, not the headline. Ongoing "how do I…" workflows are
answerable from the product and docs alone.

## Evidence base (2026-07-12 five-track audit)

Full findings live in the audit session; load-bearing facts:

- **Empirical timing (M4 Mac mini, warm network):** clone→build→run ≈ 4 min —
  but only into the silently degraded `ft8lib_stub` build, because the README's
  plain `git clone` misses the ft8_lib submodule and `pancetta-ft8/build.rs`
  falls back silently. Mid-tier hardware: 8-15 min. Source builds can never
  reliably hit 5 minutes; **prebuilt binaries are the only true 5-minute path.**
- **Both README run commands are broken** (bare `pancetta` not on PATH;
  `cargo run --release` ambiguous between the `pancetta` and `pancetta-audio`
  binaries — reproduced live).
- **The first-run wizard can brick startup permanently:** it accepts a
  lowercase grid / letters-only callsign, saves it, and every subsequent launch
  fails validation *before* the wizard can re-trigger.
- **CONFIG.md documents schemas that don't exist:** `[autonomous_operator]` +
  `mode = "hybrid"` + `[priority_weights].snr` vs the real `[autonomous]`,
  `slot_parity`, `[autonomous.priorities].signal_strength`; `[duplicate_checking]`
  is documented but never wired to config at all. Unknown TOML keys are
  silently ignored, so users "enable autonomous" and nothing happens.
- **Three keybinding surfaces disagree** (README table ~40% incomplete and
  wrong about `Enter`; `?` overlay omits `e` and `Shift+M`; FEATURES.md says
  `D` for device picker — actual `d`). The emergency stop (`Shift+Q`) is
  documented on only one surface.
- **No releases, no tags, no screenshots, no badges, no GitHub topics**;
  CONTRIBUTING.md's bottom half is fabricated boilerplate (wrong org URL, fake
  Discord, placeholder maintainers, wrong license claim).
- **What already works and must not regress:** the wizard itself (TTY-gated,
  device pickers), safe-by-default posture (rig disabled ⇒ mock, autonomous
  off, N0CALL CQ refusal), no-config decode-only operation, `pancetta setup` /
  `test-rig` / `test-audio --list` / `config --generate` / `--wav` (all real,
  all undocumented), and every fix from the 2026-06-14 UX audit.

## North-star funnel definitions

- **THE 5-minute path (post-build, the operator's stated goal):** build (or
  download) completes → first launch → wizard (station + audio + rig) →
  `pancetta doctor` green → decoding, TX-capable — **≤ 5 minutes** for a ham
  with CAT/audio cables already attached. Owned by Phase 4.
- **Supporting: install path (binary):** download release artifact → run.
  No Rust toolchain, no compile. Owned by Phase 3.
- **Supporting: source path:** `git clone --recursive` → documented build →
  first launch, with zero dead ends and zero silent degradation. Owned by
  Phase 1.

## Phases

### Phase 1 — Unbreak the funnel (detailed plan exists; execute first)

No dead ends for a literal README follower. Fix run commands + `--recursive`;
make the stub fallback loud at build time and visible in the TUI diagnostics;
wizard validates/normalizes callsign+grid at the prompt; startup offers the
wizard instead of exiting when the saved config fails validation; bare
`cargo run` disambiguated; CONTRIBUTING.md truth pass; README/CLAUDE.md crate
counts corrected. Deliverable: a stranger following the README verbatim
reaches a decoding TUI with the real C decoder, or is told loudly why not.

### Phase 2 — Docs tell the truth (plan: `2026-07-13-onboarding-phase2-docs-truth.md`)

- Regenerate CONFIG.md's `[autonomous]` section from the real schema; either
  wire `[duplicate_checking]` into config plumbing or delete its docs (decide
  at plan time; wiring is the better UX).
- Warn on unknown top-level config sections at load (serde-level or a
  post-parse key sweep) — kills the "silently ignored knob" class.
- Single source of truth for keybindings: one table in `pancetta-tui` that
  generates the `?` overlay and a `docs/KEYBINDINGS.md`; README embeds or
  links it. CI check that the generated doc is current (same pattern as the
  existing `merge_with` guard: drift fails a test, not a human).
- RUNBOOK fixes (autonomous `a` key, real log paths, `pancetta.toml` naming),
  FEATURES.md corrections, `docs/README.md` index separating curated docs
  from working notes, defaults.toml regeneration or demotion from
  "source of truth" billing.

### Phase 3 — Releases, discoverability, presentation (plan: `2026-07-13-onboarding-phase3-releases.md`)

- Tag `v0.9.5`; add a release workflow (evaluate `cargo-dist` first, else
  hand-rolled matrix build) shipping macOS (arm64), Windows x64, Linux x64
  binaries with the ft8_lib submodule baked in. SECURITY.md already promises
  tagged releases "once they exist".
- README top: one TUI screenshot + an asciinema/GIF of decode→QSO, CI +
  license badges, a "Download" section ahead of "Build from source".
- GitHub metadata: topics (`ham-radio`, `ft8`, `rust`, `tui`, `sdr`,
  `amateur-radio`), homepage, fix the community-profile docs link (points at
  `master`, 404s).
- Fill `description`/`repository` metadata on `pancetta-agent` and
  `pancetta-protocol` so `publish = false` is a choice, not a blocker.

### Phase 4 — The five-minute on-air path (plan: `2026-07-13-onboarding-phase4-five-minute-on-air.md`) — owns THE goal

- `pancetta doctor`: one command that checks clock sync (NTP offset), audio
  device presence + level, submodule/stub status, rigctld reachability, config
  validity — and prints the fix for each failure. This becomes the universal
  "it doesn't work" answer and the first line of every troubleshooting doc.
- Input-device fallback gets the same TUI-error + badge treatment the output
  side already has (`coordinator/audio.rs:219-229` is the template); rigctld
  spawn/model failures route through `MessageType::Error` with a
  platform-appropriate hint so "RIG: ✗" always carries a reason.
- Wizard mentions the rig step: final wizard screen prints "To set up CAT
  control later: pancetta setup" (README currently claims the wizard covers
  rig model — it doesn't).
- Task-oriented user guide, `docs/GUIDE.md`: "Your first 15 minutes", then
  how-do-I sections (work a DX station, enable autonomous *supervised* — with
  the §97.221/ARRL-rules framing, upload logs, switch bands/modes, Hound mode,
  multi-stream TX). RUNBOOK stays the owner-operator procedure doc; GUIDE is
  for strangers.
- Document the hidden CLI surface: `setup`, `test-rig [--ptt]`,
  `test-audio --list`, `config --generate`, `export`, `--wav`.

## Success metrics

- **Post-build → on-air ≤ 5 min** (wizard incl. rig → doctor green → decoding,
  TX-capable) — THE goal; Phase 4 exit criterion, measured by the scripted
  five-minute drill with a stopwatch.
- Binary install path: download→first-launch < 2 min (Phase 3 exit criterion).
- Source path: clone→decoding with real C decoder, zero dead ends, on the
  README's literal commands (Phase 1 exit criterion; CI-checkable for the
  command validity half).
- Zero config keys documented-but-ignored or wired-but-undocumented (Phase 2;
  enforceable by a schema↔docs drift test).
- Every failure a first-session user can hit (wrong audio device, no rig, bad
  config, stub build, clock skew) is visible in the TUI with a printed fix
  (Phases 1+4).

## Risks / notes

- **Keep the stub fallback** — CI worktrees and research flows rely on
  building without the submodule; Phase 1 makes it loud, never fatal.
- **Autonomy framing:** all user-facing "enable autonomous" docs adopt
  supervised-autonomy language (control operator present; §97.221 and ARRL
  contemporaneous-initiation rules). The competitor audit shows this is the
  reputational line between "great tool" (Hamilton posture) and "robot villain"
  (WSJT-Z posture).
- **WSJT-X UDP emit + consume** (GridTracker/JTAlert compatibility, including
  remote QSO initiation from GridTracker) was green-lit by the operator
  2026-07-13 as its own initiative — see
  `docs/superpowers/specs/2026-07-13-wsjtx-udp-design.md` and its plan. It is
  deliberately NOT one of these onboarding phases.
