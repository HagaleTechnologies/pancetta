# Changelog

All notable changes to Pancetta are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Adaptive TX-offset switching (PAN-72): in `AUTO` TX-frequency mode, an
  in-progress QSO that goes `[autonomous] qso_stall_switch_after` slots (default
  4) without the DX advancing — **silence now counts, not just a repeated
  frame** — moves to a fresh offset picked by the smart frequency allocator. If
  it stalls again on that new offset it reverts to the last offset the QSO was
  known to work on, ping-ponging rather than wandering. New `u` keystroke forces
  the same move on demand without leaving `AUTO`: it nudges the active QSO, or
  the CQ-hunting offset when no QSO is running. In `HOLD` the nudge reports a
  no-op instead of breaking your pinned offset. Replaces the old blind
  identical-repeat `+300 Hz` hop, which never noticed silence and ignored both
  the spectrum and our other active streams. `[autonomous]
  cq_no_response_switch_after` default lowered 5 → 4 to match.
  Safeguards on the switch decision: concurrent QSOs resolved in the same
  slot are reserved against each other so two streams can never collapse onto
  one offset; a queued action is discarded if you switch to `HOLD`, if the QSO
  finished, or if the DX answered before it could be committed; a Hound's
  procedure-mandated QSY becomes the offset a later revert returns to (never
  one it moves away from); a caller answering an unanswered CQ counts as
  progress, so the CQ's stall streak does not carry into the exchange; a `u`
  nudge pressed before enough decode history has accumulated is retried rather
  than dropped; and an applied move refreshes the QSO banner immediately
  instead of waiting for the next state change.
  Further safeguards: a Hound's switch stays inside the Hound region it is
  currently pinned to (the low calling region before the QSY, the response
  region after), never wandering somewhere the Fox is not listening; a revert
  whose known-good offset another QSO has since taken allocates a fresh offset
  instead of stacking two streams; every switch is decided against the current
  slot's own waterfall and decodes rather than the previous slot's; `u` only
  reports "nudging active QSO" for a QSO the engine still has running, not one
  inside its trailing-73 grace window; and while TX is `DISABLED` the stall
  counter no longer advances, so a muted station never drifts off a working
  offset it never transmitted on.
  Final safeguards: a switching QSO now hard-avoids every other live QSO's
  offset — not only the ones relocating in the same slot — instead of relying
  on a scoring penalty that a crowded band can outvote; a Hound's region is
  re-checked at the instant the move is applied, so a Fox answering mid-decision
  can no longer have its mandatory QSY undone; and the offset a QSO just left
  keeps accepting replies (and stays the offset credited as "known good") for
  two slots, so the caller who answers the very transmission that triggered the
  move is no longer rejected — or answered with a duplicate QSO.
  Last pass: a frame already queued for transmission now follows the QSO to its
  new offset instead of going out on the one it just left, and an answer that
  arrives after a move credits whichever offset it actually came back on — so a
  hand-forced `u` nudge that immediately works is no longer "reverted" away
  moments later.
  Final pass: the DX's first signal report now counts as forward progress (it
  could previously be mistaken for a stall and the QSO moved away just as the DX
  answered), an operator-forced nudge on a split QSO credits the offset it moved
  TO rather than the one it left, a Hound response region above 2800 Hz is now
  actually searched instead of silently refusing every move, and a switch is
  scored against the slot the QSO really transmits in rather than slot-blind.

- Saved rig-config bookmarks (PAN-61): the `i` rig picker can now save the current model/port/baud/PTT as a named bookmark (`F3`) and load one back into the form later (`F2`), without retyping. Builds on PAN-59's live rig-config switch.

## [0.9.6] - 2026-09-02

### Added

- Operator-visible QSO security diagnostics: sender-mismatch/impostor rejections
  and self-call refusals now appear as retained yellow `qso.security` Warn rows
  in the Shift+D Diagnostics overlay, while preserving existing rejection,
  logging, and status-line behavior.
- Release workflow: the `v*`-tag-triggered binary build now also ships a
  Linux aarch64 (Raspberry Pi 4/5 and other 64-bit ARM boards) target,
  gated on an actual fixture decode (not just linkage) in addition to the
  existing non-stub-decoder check — v0.9.6 is the first tagged release to
  include it. (The macOS/Linux-x86_64/Windows release workflow itself,
  and `pancetta info`'s decoder-engine line, shipped in v0.9.5.)
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
- DX watchlist: a short-lived (~2.5 min TTL) per-callsign memory of
  recently heard high-tier CQs (per-band-DXCC-new or ATNO), intended to
  give a station missed while busy, at TX capacity, or having lost that
  cycle's single-pounce-slot competition a fair shot the next time it's
  actively CQing. The `◇` marker it puts on DX Hunter rows isn't cleared
  when a station is pounced on or worked, so it can persist through and
  after a completed QSO until the TTL lapses. Never transmits on its own;
  a watchlisted station is only ever worked the ordinary way, by being
  freshly re-decoded as a CQ.
- Recent-QSOs panel (`Shift+R`): a scrollable, color-coded outcome log
  ("KJ5NJF — Failed: Timeout ...") for the last 50 completed/failed QSOs,
  mirroring the existing Diagnostics panel's conventions. A new
  `[database].persist_qso_timeline` config flag (default off) additionally
  persists full per-QSO state-history timelines to the database — data
  capture only, no in-app viewer yet.
- IARU-region-aware band-plan TX warning: the soft, once-per-session
  out-of-band TX warning now checks against the operator's actual IARU
  region (Region 1/2/3) instead of a US-shaped global table for the 5
  bands with real documented divergence (40m/60m/80m/2m/70cm) — e.g.
  Region 1's 40m stops at 7.2 MHz where the old table extended to
  7.3 MHz. Configurable via the pre-existing but previously-unwired
  `[rig.frequency.band_plan]` region/custom_bands/edge_warnings settings.
  Still never a hard TX block.
- Autonomous Auto-mode TX-frequency switching: after `cq_no_response_switch_after`
  (default 5) consecutive self-CQs with zero decoded responses, the
  autonomous operator switches to a different TX frequency instead of
  retransmitting into a possibly-dead spot indefinitely, choosing a new
  frequency that explicitly avoids the abandoned one via
  `SmartFrequencyAllocator`. If occupancy data isn't fresh enough to pick
  a good alternative, it skips a TX window and listens instead of
  guessing blind. `TxFreqMode::Hold` is unaffected.
- DX Hunter 5-tier priority scoring: replaces the flat weighted-sum
  formula with a strict lexicographic ranking (ATNO > per-band-DXCC-new >
  special-station > per-band-grid-new > everything else), so a needed
  entity always outranks ordinary rarity variation instead of the two
  blending into one continuous score. Special-station detection covers
  US 1x1, UK GB-prefix, and a curated international list. The two
  previously-divergent DX Hunter scorers (live decodes vs. network spots)
  are now unified onto one formula.
- `pancetta pair <CODE>`: a CLI to enroll the station agent against a
  live cqdx pairing endpoint (enroll → sign challenge → complete),
  persisting `paired.json` so `station_agent` picks it up on next start.
  `--force` to overwrite an existing pairing; `--pairing-api-url`/`--name`/
  `--platform` overrides.
- QSO Status panel and the title bar now show DXCC entity — the partner's
  entity next to the Call line in QSO Status, and the station's own
  entity next to the grid square in the title bar (resolved once at
  startup). Omitted entirely, not shown as a placeholder, when
  unresolvable.
- Band Activity now logs the station's own TX attempts (not just received
  decodes) — every frame the TX pipeline sends to PTT, including bare CQ
  calls — interleaved chronologically with the RX side of the exchange,
  marked with a `» TX` Call-column marker and `TX` in the SNR column. The
  row is logged when the frame is queued for transmission, not gated on
  the PTT bus send actually succeeding, so a failed PTT dispatch (e.g.
  Hamlib unreachable) can still show as a `» TX` row despite the radio
  never keying.

### Changed

- `k` (abort the selected QSO) now fires from any TUI panel — DX Hunter,
  Band Activity, Callers, wherever focus currently is — instead of only
  when the QSO Status panel is focused (PAN-21). The abort target is
  whatever is pinned/highlighted in the QSO Status panel (independent of
  which panel has focus), and that highlight is already visible regardless
  of focus, so the operator can always see what `k` would hit before
  pressing it. `r` (re-send) is unchanged and still gated to the QSO
  Status panel. To make `k` safe to use globally, the `j`/`k` vim-scroll
  aliases were removed from the Diagnostics (Shift+D) and Recent-QSOs
  (Shift+R) overlays — Up/Down arrows are now the only way to scroll
  them — since those were the only two places `j`/`k` collided with
  anything. `k` still aborts the selected QSO even while one of those
  overlays is open (plus the Shift+S station-health panel, a third
  read-only overlay with the same shape), since they're read-only
  informational views rather than modals the operator is editing — their
  titles now say so instead of advertising the removed `jk` scroll keys.
  The always-visible active-QSO banner (`ui::active_qsos`, shown in every
  view and while zoomed, including Monitor view and zoom on a panel other
  than QSO Status) now marks the pinned/selected QSO with `▶` whenever
  more than one QSO is active, so the abort target stays visible in the
  views that don't render the QSO Status table at all — and that QSO is
  now guaranteed a slot in the banner's visible slice even on a narrow
  terminal or a large pileup, rather than only being marked if it
  happened to fit before the row's width budget ran out. `k` also now
  requires a bare, unmodified keypress: Ctrl+K/Alt+K (e.g. an operator's
  readline "kill line" muscle memory) report the same `KeyCode::Char('k')`
  as a bare press in crossterm, and no longer abort the QSO.

### Fixed

- `--replay` now actually decodes. The replay feeder emitted bare mono samples
  while the DSP stage de-interleaves every incoming buffer against `[audio]
  input_channels` (default 2) — so it discarded every second real sample and
  treated the survivors as covering the same wall-clock span, halving the
  effective sample rate and making an FT8 decode impossible regardless of
  timing, decode effort, or content. The feeder now interleaves each sample
  across `input_channels`, mimicking a real capture stream. A `--replay` run
  over `assets/demo-wav` goes from 0 decodes to 26, and the README demo GIF
  and screenshots were re-rendered accordingly (the DX Hunter priority table
  is populated for the first time).
- `--replay` combined with `--no-audio`, `--test-tx`, or `--wav` is now
  rejected at CLI parse time. `--no-audio` hung forever (the `no_audio`
  short-circuit in `start_audio_pipeline` precedes the replay branch, so no
  feeder was spawned and nothing ever triggered the shutdown signal);
  `--test-tx` truncated any corpus longer than 35s before its final decodes;
  and `--wav` silently won an undocumented precedence race in
  `ApplicationCoordinator::run`, running single-file decode-and-exit playback
  while ignoring the requested replay pipeline.
- `--replay` fails fast instead of reporting a success that decoded nothing.
  A configured `[audio] sample_rate` that the DSP stage cannot decimate to
  12 kHz (44100 and 22050 are valid config values) made the DSP worker exit
  on startup while the feeder paced the whole corpus into a dead channel and
  exited 0; and a corpus of well-formed but zero-frame `.wav` files passed
  the existing empty-directory check and fed nothing. Both now bail before
  the feed loop starts, naming the cause.
- A truncated or corrupt `.wav` is now a hard, path-naming error in both
  `--wav` and `--replay` instead of a silently shortened success. WAV samples
  that `hound` failed to read were filtered out and the read continued, so a
  corrupt file decoded as if it were merely shorter — and under `--replay` the
  next corpus file was concatenated straight onto the gap, collapsing archive
  time and shifting the alignment of every FT8 frame after the corruption while
  the run still exited 0.
- `--replay` seeds its 14.074 MHz default dial frequency in builds without
  the optional `pancetta-hamlib` feature too. The seed was inside the
  feature's `cfg` block, so exactly the build that starts no rig at all was
  the one left reporting 0 Hz (BAND 0MHZ in remote snapshots, near-zero RF
  frequency in QSO metadata).
- `--replay` no longer publishes to the outside world (read-only traffic —
  cqdx.io's rarity/needed-entity lookups, DX cluster login — still happens,
  so the demo keeps real scoring data). A demo run against an
  already-configured station could otherwise key the real transmitter in
  response to replayed (historical) traffic, and publish those replayed
  decodes — re-stamped with the current clock — as if they were live. Every
  outbound *write* now consults one shared predicate
  (`ApplicationCoordinator::replay_mode`): Hamlib is not started at all (no
  PTT capability); PSKReporter is forced onto its uploads-disabled (noop
  drain) path; cqdx.io spot reporting is suppressed; the WSJT-X UDP companion
  protocol takes its drain-only path, so GridTracker/JTAlert never see a
  replayed decode as a live reception; and the per-QSO logbook upload
  subscriber (ClubLog/QRZ/LoTW/eQSL/cqdx.io) is not spawned, so a QSO
  "completed" off replayed traffic can't be automatically uploaded as a real
  contact. The same predicate now also suppresses the **local** write: under
  `--replay` neither the `~/.pancetta/qsos.adi` ADIF appender nor the
  `~/.pancetta/qso.db` SQLite QSO logger is started at all, so a replayed
  contact leaves no record in the operator's own log either (the startup
  duplicate-history seed still *reads* those files, as it must to make the
  demo behave like a real station). Tagging such a record instead of
  dropping it isn't possible — ADIF has no standard "not a real contact"
  field, only `APP_<PROGRAMID>_*` application-defined fields that are
  private by convention to the originating program and that no other logger,
  TQSL, or upload tool would recognise or honour. The two
  remote-operation consumers of the shared display feed are gated at the same
  predicate: the read-only remote-view gateway never gets a feed and never
  binds its WebSocket listener, and the station agent takes its inert
  drain-only path before loading keys or dialing the relay — so replayed
  decodes/spectrum/QSO state are never broadcast to relay peers, and no remote
  peer can send control frames (QSY/QSO actions) into a demo process. Shutdown
  is covered too: its direct `rigctld` PTT-off — a deliberately independent TCP
  connection that exists so a broken in-process rig state can't leave a real
  transmitter keyed — is skipped under `--replay`, which never started rig
  control, so it can no longer unkey a session belonging to some *other* owner
  of that endpoint. Live behavior is unchanged.
- Compound-callsign QSOs (e.g. `YS/WE9G`, `8G81PA`, `3E40CDW`) now actually
  complete instead of queuing and silently re-arming every slot for the full
  5-minute watchdog window (PAN-17). The FT8 encoder gained an i3=4
  nonstandard-callsign path (58-bit exact pack + 12-bit hash, mirroring the
  decoder's existing i3=4 support) alongside the standard/free-text paths; a
  grid square can't fit alongside a compound callsign (only 2 report bits —
  blank/RRR/RR73/73), so it's dropped rather than failing the whole message,
  while a numeric report/ack — which would otherwise silently become a
  DIFFERENT, misleading message — now fails loudly instead. A
  compound-callsign operator can call CQ with their own grid too. The
  decode side now recognizes and routes a reply FROM a compound-callsign
  station (previously TX-only: the message would key the radio but a reply
  could never be decoded back into the QSO engine) — the decoder seeds its
  i3=4 hash table both from our own callsign and from every standard-format
  callsign it decodes (not just our own), so a compound-call DX's reply, or
  a standard-callsign caller replying to a compound-callsign operator's own
  CQ, resolves instead of rendering as the unrecoverable `<...>` placeholder;
  a resolved hash render (`<CALL>`) is normalized to the plain callsign
  before it flows into QSO state, so it can't self-sabotage the very
  watchdog that checks callsign representability. A QSO that reaches a
  report-bearing stage (a genuine numeric SignalReport/ReportAck, never
  just RRR/RR73/73) against a still-compound partner now also retires
  immediately instead of re-arming a doomed encode — the same PAN-17
  symptom relocated to the report stage. Separately, a QSO whose DX or
  configured OWN callsign can never be represented in any FT8 format
  (invalid characters or >11 chars — e.g. the decoder's own `<...>`
  hash-miss placeholder leaking into the partner field) is retired
  immediately with a distinct "cannot transmit this message" reason instead
  of being indistinguishable from a plain DX-never-answered timeout. This
  also covers a compound-callsign station's caller whose plaintext
  callsign we've never otherwise heard: their hash genuinely cannot
  resolve (an inherent i3=4 protocol limitation, matching WSJT-X — not a
  pancetta gap; see `docs/DECISIONS/qso-engine.md`'s "Round 4" note), and
  now retires cleanly on the next watchdog pass instead of hanging.

- A station calling our CQ while it already has a separate, established active QSO with us (e.g. we just called their CQ and are awaiting their report) no longer opens a second, parallel QSO object for that same station. Message routing previously let a `CallingCq` QSO's "any station" relevance arms accept a frame that also belonged to the other QSO, producing two independently-cadenced active QSOs for one real station — each keying its own TX, which could put two frames on the air for the same station in one TX window (PAN-14). Two follow-on gaps in the same routing path, found in code review, are closed alongside it: (1) two still-unpartnered `CallingCq` QSOs (from repeated `c` presses, or Fox mode engaging while a CQ is already live) could both claim the same reply — now only the earliest-created CQ advances; (2) a station whose QSO with us just completed could be immediately re-claimed by an unrelated `CallingCq` QSO on a stray/duplicate frame — now reserved for `COMPLETED_QSO_REWORK_GRACE` (45 s), mirroring the existing active-or-recently-completed pattern used elsewhere.
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
- PAN-12 follow-up (PAN-15): `engage_hound` now clears any `pending_freq_drift`
  candidate that could accumulate in the window between QSO construction and
  the `metadata.hound` stamp, so it can no longer get permanently stuck for a
  Hound QSO's life; a confirmed split-TX `partner_freq` relatch that lands
  within `MIN_TX_SEPARATION_HZ` of our own TX offset now logs a warn instead
  of silently keying on top of the station we're trying to hear; the
  frequency-gate tolerance constants used by `is_message_relevant`,
  `classify_relevance`, and `maybe_confirm_frequency_drift_at` are now a
  single shared definition instead of three independently-hardcoded copies.

- Decode-pipeline crashes now recover automatically: the coordinator restarts DSP and FT8 decoder
  tasks under the existing bounded supervisor policy, keeps adjacent stages alive during backoff,
  and records stranded in-flight QSOs as `SupervisorRestart` failures.

- Station agent: the relay's `CAPACITY` terminal code (sent on a 9th-client
  connection attempt) is now handled like the other 11 relay.v1 terminal
  codes instead of being silently unrecognized.

- Bogus/unworkable decodes (the FT8 AP-hash placeholder `<...>`, a grid
  square mistaken for a callsign, and the operator's own callsign in
  compound/hash-render form) no longer surface at high priority in the DX
  Hunter list. `pancetta-core` gained shape-only `is_grid_shape`/
  `is_plausible_callsign` checks; a new bottom `PriorityTier::Suspect`
  clamps any decode failing that check to the floor regardless of
  `WorkedStationLookup` state; a `callsign_continuity.rs` leak that let a
  bare grid square earn false "observed" trust is closed; and the
  autonomous CQ-candidate loop now uses the same compound/hash-render-aware
  `callsigns_match` the rest of the file already had, instead of plain
  string equality, so a self-decode rendered as `<CALL>` or `CALL/P` no
  longer gets treated as workable third-party DX (PAN-54).

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

[Unreleased]: https://github.com/HagaleTechnologies/pancetta/compare/v0.9.6...HEAD
[0.9.6]: https://github.com/HagaleTechnologies/pancetta/compare/v0.9.5...v0.9.6
[0.9.5]: https://github.com/HagaleTechnologies/pancetta/releases/tag/v0.9.5
