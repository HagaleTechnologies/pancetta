# Pancetta product review — 2026-09-05

Broad, deep review of Pancetta v0.9.6 (`main` @ `c90cb34`) across UI/UX,
aesthetics, flow, marketability, fitness for purpose, utility, and the
engineering quality an operator feels. Six parallel review lanes, each a
full session on the same model, each verifying against source, the built
binary, screenshots, or live GitHub/web data. The lane reports are in this
directory; this file is the synthesis and the hit list.

| Lane | Report | Scope | Items |
|---|---|---|---|
| 1 | [`lane1-tui.md`](lane1-tui.md) | TUI visuals, hierarchy, flow, keyboard, responsiveness (96 screenshots at 4 sizes) | 60 |
| 2 | [`lane2-onboarding-cli-docs.md`](lane2-onboarding-cli-docs.md) | Install → wizard → doctor → CLI → config → packaging, docs-vs-code truth | 59 |
| 3 | [`lane3-market.md`](lane3-market.md) | Alternatives, segments, positioning, regulatory framing, distribution, cqdx tie-in | ~40 |
| 4 | [`lane4-operator-workflows.md`](lane4-operator-workflows.md) | Does it do the job end to end: casual, DXer, Fox, contest, POTA, headless, interop, modes | 64 |
| 5 | [`lane5-engineering-quality.md`](lane5-engineering-quality.md) | Reliability, durability, time, performance, observability, security, CI/release health | 57 |
| 6 | [`lane6-brand-docs-community.md`](lane6-brand-docs-community.md) | README, visual identity, docs architecture, writing, GitHub presence, launch readiness | 60 |

Item references below are `L<lane>-<n>` (Lane 5 uses its own `H-n` ids).
Every item in the lane files carries file:line, command output, screenshot,
or URL evidence; this synthesis does not repeat it.

## 1. Verdict

Pancetta's core is real and, in places, ahead of the field: a bit-exact
FT8 codec, a tiered priority engine, parity-correct multi-stream TX, a
supervisor with restart budgets, fail-closed remote TX, a genuinely new
TX-placement instrument, honest decoder benchmarks, 3,361 green tests. A
supervised evening on 20 m works and is safer than WSJT-X.

Everything one ring out from that core is thinner than the docs say, and
several of the things the product *promises* are not wired: config
hot-reload, `--metrics`, `--log-format json`, LoTW/eQSL upload, ADIF
import, persistent dupe checking, headless autonomy beyond
answer-if-called, cqdx spots into DX Hunter, PSKReporter uploads with the
right element IDs, a Hound mode that matches the air. Marketability is
low today because the decoder trails jt9 by ~2 dB, the "autonomous"
framing sits on the community's DXCC-Rule-6 fault line, and there is no
install channel a median ham will accept.

| Dimension | Score /10 | Lane |
|---|---|---|
| TUI aesthetics / hierarchy / flow / polish | 5 / 5 / 6 / 4 | 1 |
| Install / first-run / CLI / docs / packaging | 6 / 6 / 5 / 5 / 6 | 2 |
| Marketability today | 3 | 3 |
| Fit: casual / DXer / contester / POTA / headless / tinkerer | 7 / 4 / 2 / 2 / 3 / 4 | 4 |
| Reliability / performance / durability / observability / security / maintenance | 6 / 7 / 5 / 6 / 7 / 7 | 5 |
| README / identity / docs architecture / writing / GitHub / launch readiness | 6 / 4 / 5 / 6 / 5 / 3 | 6 |

## 2. Fix first (cross-lane P0 set)

Ordered by operator harm. Effort S < ½ day, M 1–3 days, L > 3 days.

**TX safety**
1. **Ctrl+C starts a CQ** — raw mode delivers `Char('c')+CONTROL` to an unguarded arm; only `k` checks modifiers. Guard every letter arm; map Ctrl+C to quit-confirm. [S] L1-1
2. **The quit-confirm and out-of-band modals swallow Shift+Q** — the emergency stop is dead for the duration of a dialog. Check Shift+Q before any modal. [S] L1-2
3. **Autonomous slot tick never re-anchors to UTC** — after lid-close/wake or an NTP step (Pi 4 has no RTC) every autonomous TX fires at a fixed phase into the slot: late partial transmissions every cycle. Replace `interval_at` with `sleep_until(next_slot_start(Utc::now()))`. [M] L5 H-1
4. **Dead audio output and rig loss do not inhibit PTT** — autonomous keeps keying into silence or a dead CAT link. Add both to `tx_hard_mute_reason`. [S] L5 H-3, L4-7
5. **No SIGTERM handler** — `systemctl stop` mid-TX abandons PTT and orphans rigctld. [S] L4-6
6. **`tx_late_max_ms` default 8000 permits provably undecodable TX** and the key-time cursor is uncapped. Default ≤3000, re-check at key time. [M] L5 H-7

**Config and data**
7. **Partial TOML sections fail to parse, the loader silently replaces the whole file with defaults (N0CALL), and `config --validate` still prints PASS** — every "add this block" instruction in the docs triggers it (`[autonomous] enabled = true`, the GridTracker UDP recipe, CONFIG.md's own minimum config). `#[serde(default)]` on ~50 structs, fatal parse errors, validate exits non-zero, CI test that deserializes every TOML snippet in `docs/`. [M] L2-1/2/3
8. **Config hot-reload is documented (CONFIG.md, RUNBOOK's "suspend autonomous TX by editing the file") but never constructed** — the RUNBOOK procedure does nothing on an unattended transmitter. Wire it (fix the debounce-skips-writes bug first) or delete every claim. [M] L2-5, L5 H-2
9. **Persistent dupe check is dead in production and the in-memory one keys on audio offset for ≤1 h; there is no ADIF import** — nothing is "worked before" until re-worked in Pancetta; B4/ATNO/tiers are Pancetta-history only. [M] L4-4/5, L2-52
10. **QSO commit path never fsyncs; startup index rebuild unlinks `qso.db` under an open pool; DB→ADIF migration deletes the only complete copy** — power loss or a crash after a QSO can lose it from both stores. [M] L5 H-8/9/10, L2-7
11. **QRZ XML password leaks into debug logs** via `reqwest::Error`'s URL on any transport failure, at the log level every troubleshooting guide asks users to paste. `without_url()`. [S] L5 H-50

**Integrations that are confidently wrong**
12. **Hound implements the inverse of the WSJT-X convention** (calls 300–900 Hz, moves *up* on report) and DXpedition 0.1 frames use the wrong layout and are rejected — a real Fox never answers this Hound. [M] L4-1/2
13. **PSKReporter IPFIX element IDs are wrong** (receiver and reception templates) — Pancetta's own RX reports are probably not landing; the README's "spots across NA+EU" are others spotting you. [S] L4-3
14. **cqdx startup is one-shot** (a single 401/timeout disables it for the session) and the offline fallback treats KH6/KL7/KP4 as "home" — DX utility does not survive a cqdx hiccup. Retry with backoff; derive ATNO/needed locally. [M] L4-8

**Presentation that undermines trust**
15. **Help overlay truncates silently below 46 rows** and the entries cut are `d x q Shift+Q Esc`; **the title bar has no overflow strategy** and the UTC clock is the first casualty at ≤100 columns. [S/M] L1-3/4
16. **Two callsigns** — README says W5AU, every screenshot, GIF, tape, and several docs say K5ARH; **`TX: FULL` green chip in a replay that cannot transmit**; **KG4OJT labelled Guantanamo Bay** in the priority screenshot; **hero GIF is 93 s, ~75 s of empty panels**; **no asset ever shows a QSO**; a maintainer TODO ships in the README. [S each, M for the assets] L6-1..6

## 3. Themes and consolidated hit list

Each theme lists the items that matter most; the lane files hold the rest.
`(PAN-n)` = already tracked in Linear. Almost everything else is untracked.

### A. TX safety and regulatory posture
- Items 1–6 above.
- One `SlotClock` service as the only slot authority (UTC-derived boundaries, skew detector, sleep/wake + NTP-step events) consumed by autonomous, DSP, TX, queued-call TTL; retires H-1/H-7/H-16/H-21/H-32/H-39 as a class. [L] L5 move 1
- No band-edge/privilege gate before PTT (band plan is display-only). [S] L4-21
- No durable local TX audit headless (TxFrameLogged goes to the TUI only; the log appender is lossy). Blocking append to `~/.pancetta/tx-audit.log`. [S] L4-22
- No runtime clock monitor; doctor's 1.0 s threshold is looser than the decode window (~0.3 s). Rolling median DT → title-bar chip, warn/inhibit. [S] L4-27, L5 H-22
- rigctld down while keyed: PTT-off failure and the 14 s watchdog are log-only; nothing says "PTT may be STUCK ON". [S] L5 H-19
- Rig mode is never set (DATA-U left to the operator after QSY/band-hop); PTT is CAT-only, `PttMethod` Serial/VOX/CM108 are wizard cosmetics; no ALC/power readback; amplitude fixed. [S–M] L4-24/50/51
- Headless presence gate blocks *pounce* as well as CQ, stricter than the project's own compliance analysis (§97.221(c) answering a human CQ). Add `unattended_policy = respond_only | answer_cq | off`; count a remote heartbeat as presence; keep unattended CQ origination impossible. [S] L4-23 — and keep Lane 3's rule: never *market* unattended operation.
- `Ft8Transmitter` is `DegradeOnly`: a TX-worker panic kills TX until restart. [M] L4-25
- `k` aborts a QSO from any panel with no confirm; case-pair keys map to unrelated actions (`h`/`H`, `t`/`T`, `p`/`P`, `s`/`S`, `x`/`X`). Move destructive/rare actions behind Shift+confirm or a palette; free `j/k`. [L] L1-41/42 (PAN-72 adds another keystroke — pick it against this)

### B. Config, logbook, and data durability
- Items 7–11 above.
- Wizard/`setup`/`--generate` write the full 962-line schema (~85 % runtime-inert GUI scaffolding, `secret_encrypted`, `[ui.window]`), destroying comments; `setup` on an unparseable file silently starts from defaults and overwrites. Non-default-only `toml_edit` writes. [M] L2-10/11/33
- Unknown-key warning is top-level only and default-path only; `--config` never sweeps. [S] L2-16
- Config search path is undocumented and starts with **cwd**. [S] L2-32
- WAV recorder runs unconditionally with a 10 GB hardcoded cap (~1.7 GB/day churn) — hostile to the Pi/SD-card story; `[audio.recording]` is never read. Wire it, default off. [S] L2-6, L5 H-29 (GH #265 partial)
- Index backups default to a cwd-relative `backups/` every launch (7 stale files sit in the repo root); the ADIF source of truth is never backed up. [S] L5 H-30
- Uploads are fire-and-forget: no outbox, retry, persisted status, or TUI surface; "QSO logged" toast fires before upload. Durable outbox in SQLite drained with backoff; per-target status in Recent QSOs. [M] L5 H-11, L4-12 (PAN-57)
- LoTW is argv-only (tests assert tqsl never runs; no `-p`; exit 8/9 dupes treated as failure); eQSL posts `text/plain` not the `ADIFData` form field; no confirmation pull anywhere. [S each] L4-13/14/60
- ADIF fidelity: FT4 logged as `MODE=FT4` (LoTW wants `MFSK`/`SUBMODE`); no DXCC/CQZ/ITUZ/COUNTRY; `QSL_*` hardcoded N; HashMap field order; `TIME_ON` = CQ start; no CAT ⇒ `BAND=0MHZ`; forced cqdx attribution in every COMMENT with no toggle; `export` reads `qso.db` not the ADIF. [S each] L4-15/33/34/35/36, L3 D3
- Worked-before seeds one band with a `"20m"` fallback and never re-seeds on QSY; policy differs before/after restart. [S] L4-16, L5 H-31
- POTA/SOTA: detection is a callsign-suffix heuristic; no `MY_SIG*`/`SIG_INFO` fields; no api.pota.app. [S] L4-20
- ADIF-unwritable is logged once and the session silently degrades to DB-only. [S] L5 H-17
- Contest serial persisted with truncate-write, errors swallowed. [S] L5 H-33

### C. DX utility that survives without cqdx
- Item 14 above.
- Pounce uses the weighted scorer gated by `min_dx_score`, not the tiers the display shows — you cannot say "pounce ATNO only". Single-scorer invariant is violated three ways. [M] L4-9
- cqdx spots are fetched but never reach DX Hunter (`tui_tx: None`). [S] L4-10
- **Zero alerting**: no bell, ntfy/Pushover/webhook, push, or even an INFO line on an ATNO decode; `ClusterAlertConfig`/`SoundFeedbackConfig` have no consumers. [M] L4-11, L1-20
- Space on a cluster spot calls at 1500 Hz on the *current* dial with no QSY; cluster mode/band are dropped; `filtering` config is ignored; disconnect spins a worker at 100 % for the session. [S each] L4-17/18, L5 H-4
- No per-mode needed/worked sets; needed matched by prefix `starts_with` (JH/JR/7K, M/2E, W/N alternates miss); cqdx entity list loaded but unused; DXCC table drops `=exactcall` and slash sub-entities. [S–M] L4-37/38/39
- Watchlist is a 150 s auto-memory the operator cannot edit; no alert or boost. [S] L4-40
- Band hopping is round-robin after 20 dead cycles; no UTC/grey-line/propagation/needed-aware scheduler. [M] L4-41
- Local needs engine: derive needed DXCC/grid/band-slot and confirmed status from `qsos.adi` + imported LoTW/cqdx confirmations so cqdx is enrichment, not dependency. [M] L3 O6, L4 bet 1

### D. Integrations, Fox/Hound, contest, modes
- Items 12–13 above.
- Fox is "CQ at 1500 with N parallel answers", not a WSJT-X Fox (no 300–900, 60 Hz slots, `RR73;` multiplex). Multi-stream TX is a structural edge here once fixed. [M] L4-42
- Contest mode is ~10 % of its spec with no operator entry point (`engage_contest_profile` has zero callers); FD/RTTY-RU/EU-VHF wire layouts are non-interoperable; no serials/dupe sheet/rate/Cabrillo. Either ship the modal + `[contest]` config for the landed profile, or state contests as out of scope in "Why not (yet)". [M] L4-19/43/44 (PAN-49/50/52)
- WSJT-X UDP is the best-built non-core surface (byte-exact goldens, fail-closed Reply/HaltTx) but FT4 decodes are never emitted, Status(1) blanks DX/Report/TxMessage/RxDF, inbound Decode(2) is dropped (no "WSJT-X as modem"), no N1MM contact UDP. Publish a verified compatibility table vs GridTracker 2, JTAlert, Log4OM, WaveLogGate. [S–M] L4-30/31/32/62, L3 D2
- FT2 leaks as a fake mode label on default builds. [S] L4-45
- FT4 caps at 78 % recall with a diagnosed, unworked cause (FT8-tuned sync thresholds). [M] L4-29, L3 O2
- Decoder: `Max` is clamped (docs say unlimited); Pi-class `Auto` is a 1 ms floor-only budget; neural OSD is on but inert at depth 0; W4.3 multipass (+32 truths) measured and unshipped; no in-product decode yardstick. [S–M] L4-46/47/48/49, L5 H-24
- Rig breadth: a 14-name model whitelist, one radio validated, a conflicting unused model table, rigctld stderr to `/dev/null`, no multi-instance (data paths hardcoded despite `--config`). [S each] L4-28/52/53, L3 D4
- `--metrics` exports nothing (feature non-default, zero emitters, binds 0.0.0.0 when it does); `--log-format json` is a no-op; `WebApiConfig`/`WsprConfig` are dead config. [S] L5 H-12/13, L4-26
- Remote gateway `bind_addr` not validated as loopback; the TX allow-list is now cloud-fed and the real local trust root (`clients/<keyId>.pub`) is un-provisionable — fail-closed today, but the documented security argument is wrong. [S] L5 H-51/54

### E. TUI hierarchy, feedback, and responsiveness
- Items 1, 2, 15 above.
- Status message is one overwritten String with no TTL, clobbered by every decode ("Decoded: …" ×28 in one burst), and truncates first at ≤100 cols. Toast ring with levels/TTL; errors sticky. [M] L1-5, L5 H-20
- Band Activity ignores need status (fields are on the row; callsign is green unless worked-before) while DX Hunter shows `+▲◇` in red for the same station. [M] L1-7, bet 5
- Cyan means ≥6 things; yellow = focus AND warning AND CQ. Semantic theme tokens consumed by every renderer including overlays (which hardcode colours, so Light theme is half-applied); colour-blind palette; glyph-back every colour. [M] L1-12/16/50
- QSO Status needs ≥15 rows and renders fragments below; DX Hunter/Callers don't adapt columns (`G S R R L P` headers at 80 cols); Hunt view collapses under 30 rows; 80x24 is effectively unusable. Responsive tiers (Compact/Standard/Wide) with per-panel `render(area, tier)`. [L] L1-8/9/10/31/59, bet 4
- Idle QSO Status shows red "-50 dB" gauges; Station Health's TX-output indicator is semantically inverted (red on a healthy station); station card entity lookup has no fallback ("--- new" beside "Norway"). [S each] L1-6/13/14
- Waterfall z-order collisions ("TX 1▼0▼"), grayscale-only, no gain; TX-placement axis unlabelled; top-10 zoom ships placeholder "Live"/"Quiet" columns. [S–M] L1-11/24/25/37
- Three TX-offset ranges on screen at once (200–2900 / 300–2700 / hint). [S] L1-15
- Debug-format leaks (`View: Hunt`, `Rig: NotConnected`, `Reply step: Rr73`), raw `very_rare`, "1.8k k" distance, inconsistent casing/units/vocabulary (Freq/DF/Dial, Last/Age). [S] L1-19/22/23/35/46
- No always-on slot clock/parity bar; "Pancetta TUI" spends 12 title columns on itself; `DECODE: MAX 2243ms ✂` is dev telemetry. Replace with `[E ▮▮▮▮▯▯ 6s │ TX:O]`. [M] L1-29/51/52, bet 1
- Static footer; no context-sensitive hints; no command palette; view indicator absent in Operate. [M] L1-39/40, bet 3
- Frame tearing on view switch (no synchronized output); unconditional 30 fps redraw costs ~23 % of a core idle. [S] L1-49, L5 H-27
- No logbook browser/edit/notes, no CQ-only/needed-only filter, no callsign lookup key, no manual log entry. [M] L4-57
- Row-highlight styles inconsistent; Callers hint row overwrites the panel border; empty states split across cells; emoji glyphs misalign in tmux/Windows Terminal. [S each] L1-17/18/30/34

### F. Onboarding, CLI, packaging
- Items 7–8 above.
- `doctor` says "Result: ready" with no config (4 WARNs, exit 0); the "index rebuilt when missing" migration never fires on a fresh drop-in (ordering bug). Distinct `NOT CONFIGURED / DECODE-ONLY / ON-AIR READY` verdicts and exit codes; `--json`; `--fix`; `support-bundle`. [S–M] L2-4/7, L5 H-23, L2 move 3
- `--help` long_about is marketing fiction (`<1ms latency`, `>95% at −20 dB`, `<100MB`) contradicting the README; dead flags (`--log-format`, `run --callsign/--frequency/--power`); two "not yet implemented → exit 1" subcommands; 15 global flags (incl. `--test-tx`) echoed into every subcommand's help; `config --show` is a 5-line summary; no completions/man page; `--version` has no SHA/target. [S each] L2-13/23/24/25/42/43
- Headless: invalid config exits with an anyhow trace (systemd crash-loop); ANSI escapes when piped; no-config runs as N0CALL with no banner. [S] L2-14/26/29
- Wizard: saves N0CALL on Enter-through; invalid device selection keeps `"default"`; rig model is free text validated only by doctor; serial picker lists `debug-console` as "PCI" and `test-rig` reports OK on it; `test-rig` exits 0 on failure and CAT PTT is "not implemented" while README says `--ptt` keys TX. [S each] L2-18..22
- Packaging: ad-hoc-signed macOS (Gatekeeper `xattr` dance), unsigned Windows (SmartScreen), sha256 only (no attestation/SBOM/signing), no brew tap / binstall / winget / deb / nix / `pancetta upgrade`, no Windows service unit, launchd plist self-contradicts on log path, `scripts/build_release.sh` is a fabricated fossil, stripped binary with no symbol artifacts. [S–M] L2-39/40, L5 H-36/37/38, L3 O4, L6-41/42
- Windows — the deploy target — is never compiled or tested in CI outside the tag build. [M] L5 H-15
- Kernel time exceeds user time during decode (31 threads on 12 cores, two full-width pools); fine on M4, a risk on a Pi; tier probe runs during live decode with no load guard. [M] L5 H-14/24
- Per-decode INFO from two sites → 20–45 MB/day; count-capped retention only; lossy non-blocking appender. [S] L5 H-28

### G. Docs truth and architecture
- Documented-but-false (fix or delete): hot-reload; CONFIG.md's minimum config and env-var table (`--no-rig`, 13 undocumented real vars); rig defaults; RUNBOOK's `--list-audio`, `config --show --path`, and "press `T`" (that's TUNE, 12 s carrier); GUIDE's `Shift+M → FT2`; bug template's log path; `identity` "read-only" (writes a keypair). Full table in L2 §4.
- `FEATURES.md` is stale marketing (">95 % at −20 dB" vs measured −19.2 dB @ 50 %; old weighted-sum scorer; "~200 tests"). Delete; replace with a 10-row status table. [S] L6-23, L2-13
- `SECURITY.md` stale ×4 and its `unsafe` claim is false (0 `SAFETY:` comments on 30 FFI blocks); two security reviews reason about `panic=abort` while the build ships `unwind`; threat-model drift on the allow-list. [S] L5 H-34/52, L6-26
- `CONTRIBUTING.md` is template residue (nonexistent Discord, Rust 1.70+, `cargo build --all`, 48 h review promise, "73 de Pancetta Team"); two Codes of Conduct. [M] L6-43/38, L2-37 (PAN-56 adjacent)
- Circular "authoritative" precedence across `docs/README.md`, AGENTS.md, and specs; 12 dated point-in-time notes in `docs/` root; RUNBOOK contains a Claude-driven decoder research loop; `wiki/` is a third parallel knowledge base marked public; agent-facing text leaks into human docs; date/PAN/PR residue in user docs. [M] L6-22/27/28/32/45/31
- Missing pages hams link each other: FAQ, "vs WSJT-X"/migration, hardware-tested matrix, PRIVACY (what leaves the machine per integration, incl. the baked ClubLog key), GLOSSARY, non-US regulatory notes, upgrade notes. [M] L6-30
- CHANGELOG `[Unreleased]` empty with shipped user-visible commits; entries are 35-line engineer paragraphs; breaking changes (`j`/`k` removal, `password_encrypted` rename) unflagged. [S] L6-46/47, L5 H-45, L2-38
- Crate count (11 vs 14) and test count (~200 vs ~295) drift between docs. [S] L6-24

### H. Brand, positioning, launch
- **Stop leading with "autonomous."** Tagline → "the headless FT8 station"; "autonomous operator" → "DX Hunter (supervised)"; put the §97.221 presence gate and Shift+Q on the first screen as *promises* ("will not originate a CQ unless a licensed operator touched the console in the last two minutes — enforced in code"). That sentence out-positions WSJT-Z and Auto FT8, whose guardrail is a README paragraph. [S] L3 D1, L6-20
- README: 2× its own word target; three pitches and no hook; Quick start references steps before defining them; badges undersell (no release/downloads/platforms); self-referential "first tagged release after this note" rot; jargon with no glossary; Rust type paths in prose; the name's story (cured pork → ham; Italian family with panino) never told. Proposed outline in L6 §3. [M] L6-7..21
- Assets: cut the hero to 12–20 s opening on a populated view; record one real QSO (loopback or FTdx10); fix the K5ARH/AA00aa/`TX: FULL`/KG4OJT/truncation defects before re-rendering; 256-colour waterfall; drop the PSKReporter TODO by shipping the capture. [M] L6-1..6/14..18
- Logo is `<text>` glyphs (font-dependent), terracotta with no relationship to the cyan TUI, no wordmark; register `pancetta.radio` (available) before publicity. [M] L6-12/13/21
- cqdx.io reads as lock-in: longest README section, never says "works fully standalone", never discloses same-author ownership, no free/paid statement, `panino` unlinked, attribution stamped into every uploaded QSO. Three-column Feature / Standalone / With cqdx.io table; one disclosure line; attribution opt-in; "Pancetta runs the radio, cqdx.io knows what you need, panino lets you watch from your phone." [S] L3 §7, L6-54/55
- GitHub: 1 star, 15 unique visitors/14 d, Discussions off while org SUPPORT.md says "start a Discussion", no social preview, release notes are raw CHANGELOG dumps, 0.9.x not marked pre-release, missing topics (`ft4 raspberry-pi hamlib headless wsjtx`), no FUNDING/Pages/crates.io/`cargo-binstall`. [S each] L6-34..41
- AI-assisted development is undisclosed to a community that will find AGENTS.md in five minutes. Disclose as rigor in a 6-line "How it's built" (human-owned design/invariants/on-air validation; AI-assisted implementation; independent bot review + CI on every PR; clean-room rule binds agents too). [S] L6-44
- Launch kit: landing page + 90-s on-air video (Pi behind the rig, SSH, doctor, DX Hunter ranks an ATNO, Shift+Q), groups.io list, SourceForge mirror (still the ham download hub — MSHV has 18 GitHub stars and ~600 weekly downloads), dxzone/eham listings, one YouTube reviewer (KM6LYW is the natural fit), a 10-operator beta for rig validation, a blog post "why I built a robot FT8 station and what it will never do", public 1.0 criteria. [M] L3 §6, L6 moves 2/3/5
- Segments: win now with Pi/MiniPC-behind-the-rig operators and Rust/SDR tinkerers (publish `pancetta-ft8` on crates.io — the only permissive Rust FT8 codec; `wsjtr`/`ft8core` are GPL); could win serious DXers after decoder parity + SuperHound; do not chase contesters, casual Windows GUI users, POTA activators, or DXpedition Fox (SuperFox keys are NCDXF-issued, WSJT-X-only). [L3 §4, §10]
- Do-not-do: don't market unattended CQ; don't gate features behind cqdx.io; don't rename the project; don't build contest modes now; don't publish a paid modem (the hobby norm is free + donations; sustainability = cqdx.io premium with Pancetta as the funnel). [L3 §8, §10]

## 4. Strategic bets (merged across lanes)

1. **Close the decoder gap to ≤1 dB on FT8 and fix FT4** (ship W4.3 regime-conditional multipass, multi-interval decode, per-protocol sync thresholds), re-publish the calibrated curve. It is the only number DXers read, and JTDX's orphaned base is waiting. Add an in-product yardstick (decodes vs WSJT-X on the same audio). [L] L3 O1/O2, L4-29/48/49
2. **UTC as the only slot authority + fail-closed TX on every dependency** (`SlotClock`, audio/rig/clock/band-edge in `tx_hard_mute_reason`, SIGTERM-safe shutdown, durable TX audit). Then the Pi-behind-the-rig story is true and the guardrails are marketable. [M–L] L5 moves 1/3, L4 bet 4
3. **"Bring your log."** ADIF import, fsync'd ledger, upload outbox with visible status, per-band/per-mode worked+confirmed from LoTW/cqdx, ATNO/needed derived locally, LoTW/eQSL verified with fixtures. Tiers, B4, dupes, and alerts become trustworthy without cqdx; cqdx becomes an accelerator. [L] L4 bet 1, L5 move 2, L3 O6
4. **Tier-native hunting with alerting and a band scheduler.** Pounce policy in tier terms, operator watchlist, bell/ntfy/webhook/panino push on tier≥N decode or spot, spot → QSY(+mode) → call, grey-line/needed-aware band moves. "Wake me and work it." [M–L] L4 bet 2
5. **A Hound/Fox that matches the air.** Fix the inverted convention and 0.1 frames; 300–900 Fox with 60 Hz slots and `RR73;` multiplex. Multi-stream TX is a real edge here; today it is the one place the code is confidently wrong. Evaluate SuperHound after (open waveform since 2.7.0-rc7; clean-room spec required). [M–L] L4 bet 3, L3 O3
6. **TUI design system: slot clock, semantic tokens, responsive tiers, context footer + command palette, Band Activity as the decision surface** (need-aware colouring, filters, flash/bell on directed-at-me). Golden snapshots per theme per tier so it cannot drift again. [L] L1 bets 1–5
7. **Un-brickable config and one-line install.** `#[serde(default)]` everywhere, fatal parse errors, non-default-only writes, doctor verdicts; `curl | sh`, brew tap, binstall, winget, deb, notarized macOS, attestation + SBOM, `pancetta upgrade`, Windows CI lane. [M] L2 moves 1/2/3/5, L5 move 5
8. **Reposition and launch.** Headless-first tagline, presence-gate promise up front, cqdx-optional story, one real QSO asset, 20-s hero, hardware-tested program, Discussions on, pre-release cadence every 2–3 weeks, public 1.0 criteria, video + groups.io + one reviewer. [M] L3 §10, L6 moves 1–5

## 5. Suggested sequencing

**Now (days, all S):** items 1, 2, 4, 5, 7 (serde defaults + validate exit code), 11, 13; delete or wire the hot-reload docs; K5ARH→W5AU + grid + `TX: REPLAY` + drop the README TODO; `FEATURES.md` delete; SECURITY/CONTRIBUTING truth pass; enable Discussions; mark 0.9.x pre-release; fix the three TX-offset ranges; help overlay scroll; `doctor` NOT-CONFIGURED verdict.

**Next (weeks, M):** `SlotClock` (3, 6); durable ledger + outbox (9, 10, PAN-57); ADIF import + local needs (bet 3 core); Hound fix (12); cqdx retry + spots → DX Hunter (14, L4-10); toast ring + title-bar overflow + Band Activity need colouring (E); install channels + notarization + Windows CI lane; README rewrite to the L6 outline with a real QSO asset and 20-s hero; PSKReporter verified against pskreporter.info; UDP compatibility table.

**Later (quarter, L):** decoder parity work (bet 1); responsive TUI tiers + tokens + palette (bet 6); alerting + band scheduler (bet 4); Fox rebuild and SuperHound evaluation (bet 5); rig validation program + hardware matrix; headless metrics/control page; launch kit.

## 6. Do not regress (consensus across lanes)

Single-scorer TX-placement instrument; callsign-pinned focus and two-tier highlight; keymap as single source of truth with the drift test; non-modal E-stop banner, two-press `x`, compose mode; UTC everywhere in the TUI; the supervisor design (restart budgets, `TxInhibitGuard`, PTT release on Hamlib teardown, dependency-scoped QSO-drop reasons); fail-closed remote TX with capability/`jti`/dead-man; panic containment that survived 1×1 terminal resizes; `write_secure_atomic` and the `merge_with` structural guard; `pancetta doctor`'s real SNTP and one-line fixes; `--replay`/`--wav` zero-hardware isolation; release gate refusing stub-decoder binaries with a real ARM fixture decode; secrets hygiene (redacting `Debug`, form-field credentials, 0600 keys); the honesty register in README and `docs/decoder-comparison.md`; `docs/GUIDE.md`; generated `KEYBINDINGS.md`; the `.tapes/` reproducible-asset pipeline on real off-air audio; clean-room provenance; the acknowledgments.

## 7. Method and caveats

- Six lanes ran as independent sessions on the same model with read-only access to the repo, an isolated `$HOME`, the release binary built at `c90cb34`, `--replay assets/demo-wav` only (no rig, no TX, no audio hardware beyond `test-audio --list`/`doctor`), vhs for screenshots, `gh`/WebSearch/WebFetch for external data. Lanes 4 and 5 fanned out to leaf audits and re-verified the high-stakes claims (`[V]` in Lane 4).
- Lane 1's 96 screenshots (4 sizes × 24 states) are not committed; they live in the review session's scratch directory and can be regenerated from the tapes described in `lane1-tui.md` §6.
- Two web fetches in Lane 3 were not retried after a rate limit (WSJT-X mainline download volume; the ARRL 2019 news article body — its rule wording is confirmed via arrl.org/dxcc-rules directly).
- Scores are the lanes' own; they were not normalized against each other.
- Overlaps between lanes were deduplicated in §2–§3; where two lanes disagree in emphasis (Lane 3 "never market unattended" vs Lane 4 "allow pounce-as-response headless"), both are recorded and are compatible: respond-only stays the floor, unattended CQ origination stays impossible, answering a human CQ becomes a configurable policy.
