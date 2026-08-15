# Changelog

All notable changes to Pancetta are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Operator-visible QSO security diagnostics: sender-mismatch/impostor rejections
  and self-call refusals now appear as retained yellow `qso.security` Warn rows
  in the Shift+D Diagnostics overlay, while preserving existing rejection,
  logging, and status-line behavior.
- GitHub release workflow: pushing a `v*` tag builds prebuilt binaries for
  macOS (Apple Silicon), Linux x86_64, and Windows x86_64 (MinGW) and attaches
  them to a draft release. CI refuses to ship any binary built without the
  real `ft8_lib` C decoder.
- `pancetta info` now reports the decode engine: `ft8_lib C decoder: native-C`
  or a loud `STUB` line with the fix command.
- Station agent: concurrent multi-client relay sessions (up to 8, the relay's
  own client cap), each with an independent Noise session over the one relay
  websocket. One-controller-at-a-time semantics — `takeControl` free-grabs
  control (disarming a displaced controller's live arm), `releaseControl`
  gives it up, and a single legacy client that never sends either still arms
  exactly as before (implicit grab). `Disarm` and `Heartbeat` remain accepted
  from any connected peer regardless of controller state (fail-safe TX-OFF).
  Connected clients also now get a live read stream (decodes/QSO
  progress/status) over the relay, sharing the same translation pump the
  localhost remote gateway uses.

### Fixed

- FT8's unresolved-hashed-callsign placeholder `"<...>"` (an i3=4 nonstandard-callsign
  frame whose 12-bit hash has no local hash-table entry) no longer appears as a
  DX-Hunter/Callers entry, and `is_needed_dxcc` no longer scores that exact literal
  placeholder as a needed DXCC entity — it previously fell through to the scorer's
  largest weight and consistently outranked real, workable stations. The guard is
  scoped to the literal placeholder only, so a real callsign whose prefix is simply
  absent from the bundled offline BigCTY table (e.g. newer than the table, but
  confirmed needed by cqdx) still scores normally; the resolved hash form
  `<CALLSIGN>` is likewise unaffected and keeps listing/scoring normally. (PAN-16)
- Manual split-TX QSOs whose offset was held, collision-nudged, or passband-clamped now recover after two consistent DX replies at a new frequency without moving the station's chosen TX offset ([#245](https://github.com/HagaleTechnologies/pancetta/issues/245)). Genuine Hound/Fox behavior is unchanged.

- Decode-pipeline crashes now recover automatically: the coordinator restarts DSP and FT8 decoder
  tasks under the existing bounded supervisor policy, keeps adjacent stages alive during backoff,
  and records stranded in-flight QSOs as `SupervisorRestart` failures.

- Station agent: the relay's `CAPACITY` terminal code (sent on a 9th-client
  connection attempt) is now handled like the other 11 relay.v1 terminal
  codes instead of being silently unrecognized.

- `LICENSE-APACHE` restored to the canonical Apache-2.0 text — the previous
  file paraphrased §6 and §9 and carried a corrupted appendix, which is both
  a legal-hygiene problem and the reason GitHub reported the repo license as
  `NOASSERTION`.
- `CHANGELOG.md` link footer (the `[Unreleased]` compare URL was malformed).

### Removed

- `.env.example`, which described a Docker/Grafana deployment that has never
  existed in this repository (`git ls-files | grep -i docker` is empty).

## [0.9.5] - 2026-06-24

### Added

- Per-band cqdx needs: the cqdx bridge now re-fetches the needed-DXCC set
  for the operating band whenever the dial moves to a new band, and
  tracks an ATNO ("all-time new one") subset. The priority scorer applies
  a configurable `atno_bonus` (default 0.15) on top of `needed_dxcc` for
  ATNO entities. All inert when cqdx is unconfigured.
- DX Hunter need markers: a callsign-prefix `!` (ATNO) / `+` (needed
  DXCC) marker, sourced from the same `CachedStationLookup` the scorer
  uses, rendered alongside the existing `★` notable marker.
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

- Bumped all crate versions to `0.9.5`.
- PSKReporter reports the real build version instead of a hard-coded
  `0.1.0`.
- Removed three unused `MessageBus` methods (`broadcast_message`,
  `remove_channel`, `ComponentMessage::new_high_priority`) ahead of the
  public release.
- Hardened credential redaction, SSRF host-parsing (cqdx base-URL via
  `reqwest::Url`), LoTW temp-file creation (`O_EXCL`, mode 0600), and log
  retention (cap 14 files).
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

- **Bus error on launch**: the audio device-selection modal underflowed
  its height arithmetic (`area.height - 2`) when the terminal reported a
  tiny or 0×0 size (common over a remote/Jump Desktop session at launch).
  In release this wrapped to a huge `usize` → out-of-bounds render →
  SIGBUS. Now uses saturating arithmetic, skips the overlay when the area
  is too small, and `overflow-checks` is enabled for the (non-hot-path)
  TUI crate so any future underflow is a catchable unwind rather than a
  hard crash.
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

[Unreleased]: https://github.com/HagaleTechnologies/pancetta/compare/v0.9.5...HEAD
[0.9.5]: https://github.com/HagaleTechnologies/pancetta/releases/tag/v0.9.5
