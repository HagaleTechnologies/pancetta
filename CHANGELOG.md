# Changelog

All notable changes to Pancetta are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `LICENSE-APACHE` (project is now dual-licensed MIT OR Apache-2.0; the
  former `LICENSE` file is now `LICENSE-MIT`).
- `SECURITY.md` describing the vulnerability-reporting process and known
  trade-offs (plaintext credentials, rigctld network surface, `unsafe`
  blocks).
- TUI status bar now shows the actual outcome of Space-to-call (e.g.
  "Calling K1ABC — TX queued (1500 Hz)" or "Call K1ABC failed: duplicate
  QSO ..."), instead of the previous optimistic "Calling X..." text that
  hid silent rejections from the QSO state machine.
- Density-glyph fallback for the waterfall on 16-color terminals
  (commonly seen over SSH+tmux). Intensity is now encoded by the glyph
  (`░ ▒ ▓ █`) so the panel remains readable when the terminal collapses
  256-color escapes to plain black.

### Changed

- Crate metadata centralized in `[workspace.package]`. All eleven crates
  now inherit `version`, `edition`, `authors`, `license`, and
  `repository`. Repository URL standardized to
  `https://github.com/HagaleTechnologies/pancetta` (previous values were
  inconsistent and pointed at non-existent repos).
- `pancetta-config::network`: renamed `password_encrypted` and
  `key_password_encrypted` fields to `password` / `key_password` across
  QrzConfig, LotwConfig, LotwCertificateConfig, EqslConfig, ClublogConfig,
  ProxyAuth, and ClientCertConfig. The previous name implied encryption
  that was never implemented.
- `pancetta/examples/tx_test.rs` and `pancetta --test-tx` example default
  callsign changed from a real operator callsign to `N0CALL`.
- `CONTRIBUTING.md` moved from `docs/` to repository root for GitHub
  auto-detection.

### Fixed

- TUI Space-to-call previously passed the dial frequency (e.g.
  14,074,000 Hz) where the modulator expected an audio offset (200–2500
  Hz), causing the modulator to silently reject the request. The TUI now
  passes a clamped audio offset; the DX Hunter path defaults to 1500 Hz
  (FT8 calling convention) since spots only carry a dial frequency.

## Project History (pre-`0.1.0`)

The pre-public commit history is preserved on the `main` branch. Major
milestones, in chronological order:

- **Phase 1** — Loopback QSO: end-to-end CQ-to-73 exchange through the
  full encode → modulate → decode pipeline, with state-machine tests.
- **Phase 2** — Autonomous operator + priority engine: configurable
  weighted scoring, POTA/SOTA detection.
- **Phase 3** — Multi-stream TX: SmartFrequencyAllocator selects TX
  audio frequencies; up to N parallel QSOs in one 15-second slot.
- **Phase 4** — Hardware integration: hamlib CAT control via rigctld
  short-form commands; first real-rig TX validated on a Yaesu FTdx10
  with clean ALC and tail-end PSKReporter spots across NA + EU
  (2026-04-26).

The ongoing `End-to-End QSO` initiative (`docs/superpowers/specs/`) is
moving toward Phase 5: a full autonomous CQ → grid → report → RR73
exchange on real hardware.

[Unreleased]: https://github.com/HagaleTechnologies/pancetta/compare/HEAD
