# Lane 3 — Market, Alternatives, Positioning, Marketability: Pancetta v0.9.6

Date: 2026-09-05. Repo read at `/Users/thagale/Code/pancetta` (HEAD pushed 2026-09-05). Web claims carry URL + date verified (all 2026-09-05 unless noted) + confidence (H/M/L). Internal claims cite file paths.

## Executive summary (10 lines)

1. **Marketability today: 3/10.** Technically credible (297k LOC Rust, 3,481 tests, honest jt9-referenced decoder numbers), but zero market presence: 1 GitHub star, 1 fork, 15 unique repo viewers in the last 14 days, 2 tagged releases (v0.9.5 2026-07-17, v0.9.6 2026-09-02), no web search footprint for "pancetta ham radio" (H — `gh api repos/HagaleTechnologies/pancetta`, traffic API, WebSearch 2026-09-05).
2. **The three biggest limiters:** (a) the decoder trails jt9 by ~2.1 dB on FT8 and ~3.95 dB on FT4 — the one metric every FT8 op judges software on, and where JTDX/WSJT-X Improved already *beat* mainline (`docs/decoder-comparison.md`); (b) the "autonomous" framing sits directly on the community's most polarizing fault line (ARRL DXCC Rule 6a, POTA Rule 6, FT4GL 2024 scandal) — the word costs trust with the DXer segment Pancetta most needs; (c) no distribution/install story hams expect: no Homebrew/apt/winget, macOS unnotarized, one radio validated, no video, no forum/group, no website beyond README.
3. **Real competitive set:** WSJT-X 2.7.0 GA (Feb 2025) + WSJT-X Improved 3.2.0 (Aug 2026, ~2.8k weekly downloads), MSHV 2766 (Apr 2026, multi-stream, ~600/wk), JTDX (stalled; last GitHub push Apr 2024), WSJT-Z (GPL fork with Auto CQ/Auto Call, 52 stars, active Aug 2026), Hamilton Auto FT8 (free Windows/macOS autopilot companion for WSJT-X/JTDX, v1.0.79), plus GridTracker 2 / JTAlert / Log4OM / Wavelog for the "what do I need" + logging jobs.
4. **Pancetta's only defensible differentiators today:** one-binary headless/SSH station with a real TUI; multi-stream TX (shared only with MSHV); safety-engineered remote TX (Noise-IK, arm gate, dead-man, audit log — nobody else has this); permissive MIT/Apache-2.0 license in a GPL neighborhood; per-QSO direct upload to 5 targets without a helper app; native WSJT-X UDP emit so GridTracker/JTAlert/loggers still work.
5. **Segments it can win now:** Pi/MiniPC-behind-the-rig headless operator; station-automation tinkerer; Rust/SDR developers wanting a permissive FT8 stack. **Could win with work:** serious DXer (needs decoder parity + SuperHound). **Should not chase:** contesters (no contest exchanges, N1MM ecosystem), casual Windows GUI users (WSJT-X + GridTracker is free and good enough), DXpedition Fox (SuperFox is NCDXF-keyed, WSJT-X-only).
6. **Positioning fix:** stop leading with "autonomous." Lead with "the headless FT8 station" and "operator-supervised automation with the strictest guardrails in the hobby." The §97.221 respond-only gate and Shift+Q are assets *if named as such*; "autonomous operator" is a liability.
7. **cqdx.io:** an asset for scoring (rarity/needed feeds nobody else pipes into a decoder), a lock-in/trust concern for logging (closed-source, "All rights reserved", solo-owned, no public pricing; ADIF comment stamps a cqdx.io URL into every uploaded QSO — see `docs/DECISIONS/logging-uploads.md`). Tell it as "works fully standalone; cqdx.io is optional enrichment," and make the attribution stamp opt-in.
8. **Monetization:** none viable from Pancetta itself; the hobby norm is free + donations (GridTracker, WSJT-X, Log4OM, DXLab all free; HRD ~$99 is the outlier). Sustainability path = cqdx.io premium tiers funded by Pancetta as the acquisition channel, not the reverse.
9. **Ranked plays:** (1) close the FT8 decoder gap to ≤1 dB and publish the curve; (2) rename/reframe autonomy + ship the §97.221 unattended gate as the headline safety feature; (3) install channels (brew tap, .deb, winget, notarize) + a 3-minute headless-Pi video; (4) SuperHound support; (5) a second/third validated rig (IC-7300, FT-991A).
10. **Do-not-do list:** don't market unattended CQ, don't build contest modes now, don't gate features behind cqdx.io, don't rename the project (see §6 — the name is fine, the tagline isn't).

---

## 0. Internal context found

- Prior competitor work exists but is *decoder-mechanism* scouting, not market analysis: `research/specs/catalog-other-ft8-projects.md` (2026-06-08 survey of PyFT8, SDRangel libft8, ft8mon, CWSL_DIGI, ft8modem, FT8CN, Pocket-FT8…), `catalog-mshv.md`, `catalog-jtdx-improved.md`, `spec-wsjtx-improved-*.md`, `spec-mshv-multi-answer-sequencer.md`. No positioning, pricing, or segment notes anywhere in `thoughts/`, `docs/`, `wiki/`.
- `README.md` pitch: "four alt-tabbed windows → one binary." "Why not (yet)" is honest: hamlib crate self-described as stub, cqdx client awaiting live validation, LoTW/eQSL "scaffolded" (though `docs/DECISIONS/logging-uploads.md` says both landed 2026-06-18 via TQSL shell-out/eQSL POST — README and decision record disagree; fix the README), one radio (FTdx10) validated, Pi binaries unvalidated on real Pi.
- `docs/fcc-part97-compliance.md` (2026-06-23): the *only* exposure is unattended CQ origination on standard FT8 frequencies (outside §97.221(b) segments). README §"Autonomy with a control operator present" says the software now enforces respond-only after 2 min without a keypress — the recommended "option #2" was shipped. This is a genuine, marketable differentiator no competitor has.
- Features present in source: DX cluster client (`pancetta-dx/src/cluster.rs`), PSKReporter (default off), WSJT-X UDP emit+consume, Hound (manual v1) and Fox (legacy Fox, not SuperFox — `research/hypothesis_bank.md:4992` acknowledges SuperFox is "a non-FT8 waveform pancetta CANNOT decode"), FT4, FT2 (feature-gated), 5-tier DX priority, multi-stream TX, remote gateway + station agent, 5 logbook upload targets. **Absent:** contest exchanges (no contest mode in the QSO engine; `contest` appears only in `itu_zone` doc and incidental code), SuperFox/SuperHound, Windows/macOS package managers, any GUI.
- Release/community metrics (H): 490 commits since 2026-06-23 public history; releases v0.9.5 (2026-07-17), v0.9.6 (2026-09-02); stars 1, forks 1; 14-day traffic: 54 views / 15 uniques; 3,785 clones / 631 uniques (almost certainly CI + dependabot, not humans). GitHub description: "FT8 ham radio station in Rust — decode, priority-score, and work QSOs from a terminal UI; multi-stream TX, CAT control, optional hands-off operation."

---

## 1. The real alternatives and substitutes

### 1a. Modems / stations (what Pancetta replaces)

| Product | Latest / cadence | License / price | Platforms | Community proxy | Decoder | Automation | Remote/headless | Notes |
|---|---|---|---|---|---|---|---|---|
| **WSJT-X 2.7.0 GA** | 2025-02-17; ~1 GA/1-2 yrs, RCs between (machamradio.com/blog/2025/02/19, H) | GPLv3, free | Win/mac/Linux incl. Pi | De-facto standard; groups.io main list (member count not public) | Reference (jt9); SuperFox, Q65 pileup, QMAP added in 2.7 | Auto Seq + Call 1st; **deliberately stops after each QSO** (kk5jy.net/about-ft8, H) | GUI only; Pi via VNC/xrdp; UDP API for companions | Everyone else interoperates *with* it |
| **WSJT-X Improved (DG2YCB)** | v3.2.0 2026-08-18; ~quarterly (sourceforge.net/projects/wsjt-x-improved/files, H) | GPLv3, free | Win/mac/Linux | **2,771 weekly downloads** for 3.2.0 alone | Mainline + A8/4th-pass/MTD improvements; EME features in 3.2 | Same as mainline + contest-mode toggles, NCCC Sprint FT4 | GUI | The "power user default" in 2026; 3 GUI layouts incl. widescreen |
| **JTDX** | GitHub last push 2024-04-22, no GitHub releases; Debian pkg touched Mar 2026 (github.com/jtdx-project/jtdx; tracker.debian.org/pkg/jtdx, M) | GPL, free | Win/Linux/mac | 116 stars; loyal DXer base | Historically "50-100% more decodes in poor conditions" per reviews (radio-hobbyist.com/what-is-jtdx, L — anecdotal) | AutoSeq variants, QSO-partner filter, "Call 1st"-style; no unattended CQ | GUI | Effectively stalled; "JTDX Improved" fork on SourceForge |
| **MSHV 2766 (LZ2HV)** | 2026-04-28; several/yr (sourceforge.net/projects/mshv, H); GitHub pushed 2026-09-04 | GPLv3, free | Win (XP→11), Linux | ~595 weekly downloads; 18 GitHub stars | WSJT-derived | **Multi-answering auto-seq / multi-stream** (the only other multi-stream FT8 app) | GUI | Pancetta's closest feature analogue for multi-stream |
| **WSJT-Z (SQ9FVE)** | wsjtz-3.0.0-2.0.17; releases moved to GitHub May 2026; pushed 2026-08-19 (github.com/sq9fve/wsjt-z, H) | GPLv3, free | Win/Debian | 52 stars, 17 forks | Mainline + multithreaded FT8 | **Auto CQ "unattended with configurable repeat count and band-rotation"**, Auto Call, DXCC/prefix/state/worked-before filters, band hopping, UDP control server | GUI | README warning: "Always monitor your transceiver… unless unattended/automated operation is explicitly permitted" — the closest thing to a "robot" people actually run |
| **Hamilton Auto FT8** | v1.0.79 (2026-06) (autoft8.com, H) | Free (closed) | Win 10/11; experimental macOS | Unknown | n/a (drives WSJT-X 2.5+/JTDX) | Full autopilot: CQ detect→call→exchange→log; filters by SNR/continent/zone/LoTW status/award need | Companion GUI | Explicit stance: "automation is allowed, unattended is not… autopilot, not auto-driver"; POTA/SOTA unattended "explicitly prohibited" |
| **FT8Commander / AutoFT8 (0x9900)** | pushed 2025-12-31 / 2023-03 (gh api, H) | BSD-2/BSD-3 | Python, any | 33 / 7 stars | drives WSJT-X via UDP | Autonomous selection + reply | CLI | The Python "tinkerer" answer |
| **JS8Call 2.5.0** | 2026-01-06, now under JS8Call-improved team (js8call.com/JS8Call-improved, H) | GPL, free | Win/mac/Linux/Pi | Active | FT8-derived keyboard mode | Auto-reply, relay, heartbeat — designed for unattended store-and-forward | GUI + API | Adjacent, not a substitute; shows the community tolerates unattended *when the mode is designed for it* |
| **DigiPi 2.0 (KM6LYW)** | Pi OS Trixie image w/ WSJT-X 2.7.0 (digipi.org, patreon.com/KM6LYW, H) | Image via any-amount Patreon | Pi | Large YouTube following | WSJT-X | WSJT-X's | **Browser-managed Pi appliance** — the incumbent for "Pi behind the rig" | Direct competitor for Pancetta's best segment |
| Mobile/embedded: FT8CN (Android, 628 stars, MIT, last push Jan 2025), Pocket-FT8/pico_ft8_xcvr, hotpaw ft8d (iOS) | — | — | — | — | ft8_lib-class | — | — | Different job (portable); FT8CN's 628 stars show demand for a *non-WSJT-X* station exists |

### 1b. Companions / loggers (what Pancetta must interoperate with)

| Product | Status | Price | Platform | Role | Pancetta today |
|---|---|---|---|---|---|
| **GridTracker 2** (v2.250101-era) | Active; July 2025 review video (gridtracker.org; youtube DrXxE3XjU9c, M) | Free, BSD-3 | Win/Linux/mac | Map, needed-grid/DXCC alerts, call roster, one-click call, log forwarding | Works via `[network.wsjtx_udp]`; double-click-to-call requires 3 config flags (`docs/GUIDE.md`) |
| **JTAlert 2.81.12** | Active (hamapps.com/JTAlert, H) | Free (donation) | **Windows only**, .NET 8 | Alerts + logging bridge to HRD/Log4OM/ACLog/DXKeeper | Works via UDP emit |
| **Log4OM 2** | Active | Free | Windows | Logger with unlimited UDP inbound (WSJT-X/JTDX/N1MM) (log4om.com/integrated, H) | Works via UDP QSOLogged |
| **Wavelog** (Cloudlog successor) | Active, 525 stars, pushed 2026-09-04 | Free, MIT, self-hosted | Web | Modern web logger; WaveLogGate bridges WSJT-X/JTDX (docs.wavelog.org, H) | No native upload; ADIF or UDP→Gate |
| **DXLab Suite** | Active | Free (8 apps) | Windows | DXer's suite | ADIF |
| **HRD** | Active | ~$99 + ~$30/yr maintenance (hamradiodeluxe.com/buy, M) | Windows | Paid logger/digital | ADIF |
| **N3FJP ACLog / N1MM+** | Active | $59.99 pkg / free | Windows | Contest logging; N1MM launches WSJT-X per slice with WW Digi/FT-RU contest types (n1mmwp.hamdocs.com, H) | **No path** — no contest exchange, no N1MM protocol |
| **Remote: wfview** (Icom/Kenwood/Yaesu; releases May/Jun 2026, wfview.org), **SmartSDR/SmartLink** (Flex, v4.2.20), **RemoteHams RCForb**, **RigPi 4** (Aug 2025, MIT) | Active | Free / bundled / paid hardware | Various | Move audio+CAT to a remote PC where WSJT-X runs | Pancetta's model is the inverse (station runs *at* the rig; thin clients connect) — architecturally superior for latency/timing, but no browser client yet (panino is a private React Native repo) |

### 1c. FT8 libraries / SDR decoders (developer substitutes, and where Pancetta's crates could compete)

| Project | License | Activity | Note |
|---|---|---|---|
| kgoba/ft8_lib | MIT | 297 stars, pushed 2025-08-24 | The permissive reference; Pancetta vendors it |
| bodiya/wsjtr → `ft8core` 0.5.0 / `ft8-engine` 0.5.0 on crates.io | **GPL-3.0-only** | 2026-03-28; 305 / 26 downloads | The only other Rust FT8 decoder on crates.io; GPL. `pancetta-ft8` (MIT/Apache) published as a crate would be the *only* permissive Rust FT8 codec — a real developer-segment play |
| jl1nie/RustFT8 | MIT | 7 stars, 2023 | Port of ft8_lib, dormant |
| Reid-n0rc/FT8AF | MIT | 0 stars, Aug 2026 | Rust backend + ft8_lib FFI + SQLite — someone else is building the same shape |
| G1OJS/PyFT8 | GPL-3 | 40 stars, pushed 2026-09-05 | Active Python transceiver |
| SDRangel libft8 (3,960 stars), SDR++ Brown (321; FT8/FT4 + PSKReporter), OpenWebRX+ (623; FT8/FT4/WSPR decoders), SDRconnect ft8-module | GPL/AGPL | Active | Receive-only decoders inside SDR suites — substitutes for *monitoring*, not for making QSOs |

---

## 2. Per-alternative SWOT vs Pancetta (condensed, evidence-based)

**WSJT-X / WSJT-X Improved.** S: reference decoder, universal interop, SuperFox monopoly (NCDXF-issued keys tied to callsign — dx-world.net/super-fox-mode, H), contest modes, docs, QST coverage. W: 4-window workflow (Pancetta's pitch is *correct*), GUI-only, no headless, single-stream, stops after each QSO by design. O for Pancetta: hams *already* running Pi/MiniPC headless via VNC (oh8stn.org, technotes.seastrom.com, DigiPi) are paying a real UX tax. T: WSJT-X Improved's cadence (5 releases in 18 months) and 2.8k/week downloads show the power-user segment is *served and moving*.

**JTDX.** S: DXer loyalty, aggressive decoding, filters. W: stalled (no GitHub release; last push Apr 2024), no manual (SourceForge reviews complain), no contest support. O: its orphaned DXer base is Pancetta's most reachable *serious* segment — but only if Pancetta's decoder is at least JTDX-class. Today it is ~2 dB *behind mainline*, i.e. further behind JTDX.

**MSHV.** S: the only shipping multi-stream FT8 app, active (Apr 2026 + Sep 2026 commits), Linux support. W: dated Qt UI, Windows-first docs, 18 GitHub stars (community is SourceForge/groups.io, not GitHub). O: Pancetta's multi-stream allocator (7-criterion soft scoring) is arguably more sophisticated; a head-to-head "N simultaneous QSOs" demo would land. T: MSHV owns the DXpedition-side multi-answer story.

**WSJT-Z / Auto FT8 / FT8Commander (the "automation layer" cluster).** S: zero-switching-cost — they bolt onto the WSJT-X you already run; Auto FT8 has award/LoTW-status filtering Pancetta lacks. W: WSJT-Z is a GPL fork that must chase mainline; Auto FT8 is closed and Windows-first; both are GUI-driven macro layers, not stations. O: Pancetta's automation is *engineered* (state machine, parity rule, drop-stale-TX, §97.221 gate) vs *scripted*; that's a trust story. T: these define the community's mental model of "FT8 robot" — Pancetta will be lumped with them unless it explicitly positions against unattended CQ.

**GridTracker 2 / JTAlert / Log4OM / Wavelog.** Not competitors — *gatekeepers*. If Pancetta's UDP emit is byte-compatible (built clean-room from permissive sources; some fields flagged ⚑ best-effort in `docs/superpowers/specs/2026-07-13-wsjtx-udp-protocol-notes.md`), the whole logging/alerting ecosystem is inherited for free. If it isn't, nobody will switch. **Verify with GridTracker 2, JTAlert, Log4OM, WaveLogGate and publish a compatibility table** — this is cheap and load-bearing.

**DigiPi.** S: appliance UX (browser), huge YouTube reach, Patreon-funded, turnkey HAT hardware ($). W: still WSJT-X-in-a-browser (VNC-class), not a native headless engine. O: Pancetta could *be* the FT8 engine inside a DigiPi-style image, or ship its own image. T: DigiPi already owns "Pi + FT8" in the mind of the YouTube-taught ham.

---

## 3. Strategic feature matrix (opinionated)

Legend: ✔ live · ◐ partial/scaffolded · ✘ absent. Columns: P=Pancetta, WX=WSJT-X(+Improved), J=JTDX, M=MSHV, Z=WSJT-Z, A=AutoFT8+WSJT-X.

**Table stakes (must have or nobody switches)**

| Capability | P | WX | J | M | Z | A | Verdict for Pancetta |
|---|---|---|---|---|---|---|---|
| Decoder within ~1 dB of jt9 on FT8 | ✘ (+2.1 dB) | ✔ | ✔+ | ✔ | ✔ | ✔ | **Blocking.** Serious ops won't accept "competitive, not class-leading." |
| FT4 parity | ✘ (+3.95 dB, caps 78%) | ✔ | ✔ | ✔ | ✔ | ✔ | Blocking for FT4 users (contests, 6 m) |
| CAT/PTT across common rigs (IC-7300/7610, FT-991A/FTdx10, Flex) | ◐ (one rig proven) | ✔ | ✔ | ✔ | ✔ | ✔ | Hamlib gives reach; proof is missing |
| GUI installers / package managers | ✘ | ✔ | ✔ | ✔ | ✔ | ✔ | Tarball + xattr dance + libasound2 is a wall for the median ham |
| Auto-sequence a QSO, log ADIF | ✔ | ✔ | ✔ | ✔ | ✔ | ✔ | Met |
| WSJT-X UDP compat (GridTracker/JTAlert/loggers) | ✔ (unverified vs real apps) | ✔ | ✔ | ✔ | ✔ | ✔ | Met on paper; publish proof |
| PSKReporter spotting | ✔ | ✔ | ✔ | ✔ | ✔ | ✔ | Met (default off — consider default on, it's the norm) |
| Waterfall / band activity | ✔ (TUI) | ✔ | ✔ | ✔ | ✔ | ✔ | Met, different medium |
| Documentation + user guide | ✔ (GUIDE/CONFIG/RUNBOOK/TROUBLESHOOTING) | ✔ | ✘ | ◐ | ◐ | ◐ | Ahead of JTDX/MSHV |

**Differentiators (where Pancetta can win)**

| Capability | P | WX | J | M | Z | A | Verdict |
|---|---|---|---|---|---|---|---|
| One binary: modem+scorer+cluster+PSKR+upload | ✔ | ✘ | ✘ | ✘ | ✘ | ✘ | Unique; the pitch is right |
| True headless (`--headless`, systemd/launchd units, SSH TUI, 16-color fallback) | ✔ | ✘ | ✘ | ✘ | ✘ | ✘ | Unique; the wedge segment |
| Multi-stream TX with soft frequency allocator | ✔ | ✘ | ✘ | ✔ | ✘ | ✘ | Shared with MSHV only |
| Needed-DXCC/grid/rarity-weighted 5-tier priority ranking fed by live data | ✔ (cqdx-fed) | ✘ (GridTracker does alerting) | ◐ | ✘ | ◐ filters | ✔ filters | Strong, but "needs" today come only from cqdx.io — accept LoTW/ADIF-derived needs locally too |
| Regulatory guardrails as *software* (§97.221 respond-only after idle, E-stop, TX policy cycling, drop-stale-TX, one-parity rule) | ✔ | ✘ | ✘ | ✘ | ✘ (warns) | ✘ (warns) | **Unique and under-marketed.** This is the trust story. |
| Secure remote TX (Noise-IK, arm gate + TTL + heartbeat dead-man, audit log, delegated guests can't key) | ✔ (client private) | ✘ | ✘ | ✘ | ✘ | ✘ | Unique; nobody in ham software has a threat model this explicit. Needs a public client. |
| Per-QSO direct upload to ClubLog/QRZ/LoTW(TQSL)/eQSL/cqdx without helper app | ✔ (LoTW/eQSL "OPERATOR-CONFIRM") | ✘ (needs JTAlert/GridTracker) | ✘ | ✘ | ✘ | ◐ | Strong; finish LoTW verification |
| Decode-effort budget that self-tunes to hardware ("anytime" decoder) | ✔ | ✘ | ✘ | ✘ | ✘ | ✘ | Unique; sells the Pi story |
| `pancetta doctor` (clock/audio/rig/decoder self-test with fixes) | ✔ | ✘ | ✘ | ✘ | ✘ | ✘ | Small but beloved-class feature |
| Permissive license (MIT/Apache) + clean-room provenance | ✔ | ✘ GPL | ✘ | ✘ | ✘ | closed | Unique in the *station* class; matters to embedders/SDR vendors |
| Measured, published decoder benchmark w/ FP rate | ✔ | ✘ | ✘ | ✘ | ✘ | ✘ | Rare honesty; becomes a weapon once the numbers are good |

**Gaps (where Pancetta loses)**

| Capability | P | Who has it | Verdict |
|---|---|---|---|
| SuperFox / SuperHound | ✘ (can't decode waveform) | WX, M, J | Every major DXpedition since N5J (2024) uses it; a DXer *cannot* use Pancetta as their only app |
| Contest exchanges (FT Roundup, WW Digi, ARRL Digital, NA VHF, NCCC Sprint) + N1MM/Cabrillo | ✘ | WX, Z; N1MM integration | Don't chase now; document the gap |
| GUI / map / native Windows feel | ✘ | all | By design; accept, but ship a *browser* client (panino web) |
| Award-status filters (LoTW-confirmed? worked-before per band/mode, WAS/grid) | ◐ (cqdx needed-DXCC only; needed-grid endpoint missing) | A, Z, GridTracker | Local ADIF-derived "needs" would remove the cqdx dependency for this |
| Multiple rigs validated, SDR (Flex DAX, IQ input) | ✘ | all | Community testers needed |
| Mobile/portable | ✘ | FT8CN, hotpaw | Not Pancetta's job |

---

## 4. Segments & jobs-to-be-done

| Segment | JTBD | Incumbent stack | Pancetta fit today | Verdict |
|---|---|---|---|---|
| **Casual FT8 op** (Windows laptop, IC-7300, wants contacts) | "Make it decode, click, log" | WSJT-X + GridTracker (free, YouTube-taught) | Poor: terminal, tarball, one rig, weaker decoder | **Don't chase** until installers + decoder parity |
| **Serious DXer / award chaser** (DXCC, Challenge, WAS, grids) | "Never miss a new one; confirm it" | WSJT-X Improved or JTDX + GridTracker/JTAlert + DXKeeper/Log4OM + ClubLog/LoTW | Mixed: priority engine is exactly their job; but −2 dB and no SuperHound are disqualifying, and "autonomous" smells like the robots they hate | **Could win** after decoder parity + SuperHound + LoTW-verified needs; frame as "DX Hunter," never "robot" |
| **POTA/SOTA activator** | "Rack up 10+ QSOs fast on battery" | WSJT-X on laptop/Pi, FT8CN | Weak: POTA Rule 6 bans fully automated QSOs (docs.pota.app/docs/rules.html, H); Pancetta's *activator* value is low, though its *hunter* scoring (pota_sota weight 0.15) helps chasers | **Don't chase as activator**; keep POTA/SOTA as a hunter weight |
| **Contester** (FT Roundup, WW Digi, ARRL Digital) | "Max rate, correct exchange, Cabrillo" | N1MM+ launching WSJT-X per slice | None: no exchanges, no N1MM, and ARRL contest rules also require "contemporaneous direct initiation" (contests.arrl.org RTTY-RU rules, H) | **Should not chase** (large surface, entrenched ecosystem) |
| **Remote/headless Pi-behind-the-rig operator** | "Station at the antenna, me anywhere, no VNC" | Pi + WSJT-X + VNC/xrdp; DigiPi; wfview/SmartLink + PC | **Best fit.** SSH TUI, headless mode, effort budgets, systemd units, remote gateway | **Win now.** But Pi validation is admitted missing and panino is private — fix both |
| **Station-automation tinkerer / developer** | "Script it, integrate it, hack the decoder" | Python + WSJT-X UDP (FT8Commander), wsjtr (GPL Rust), ft8_lib | Strong: 14-crate layered workspace, permissive license, research harness, UDP + WebSocket protocol | **Win now** — publish crates, write the integration doc |
| **Club / DXpedition Fox** | "Run a pileup at rate; be verifiable" | WSJT-X SuperFox (NCDXF keys), MSHV multi-answer | Legacy Fox only; SuperFox keys are gatekept to WSJT-X | **Should not chase** |
| **SDR-suite users** (SDRangel/SDR++/OpenWebRX) | "Monitor FT8 across bands" | Built-in RX decoders | Pancetta's `--wav`/`--headless` decode could serve skimmer duty | Niche; a "skimmer mode" is cheap offensive play (see §10) |

---

## 5. Ethical / regulatory positioning

**The rules as they stand (all H):**
- FCC §97.109(d)/§97.221: unattended = automatic control; standard FT8 frequencies are outside §97.221(b) segments; unattended stations may only *respond* (≤500 Hz). Pancetta enforces this in software (README) — unique.
- **ARRL DXCC Rule I.6(a)** (adopted July 2019 Board, announced 2019-08-19): "Each contact claimed for DXCC credit must include contemporaneous direct initiation by the operator on both sides of the contact. Initiation of a contact may be locally or by remote." (arrl.org/dxcc-rules; arrl.org/news/arrl-contest-and-dxcc-rules-now-prohibit-automated-contacts). Same language in ARRL contest rules ("Automated operation is not permitted").
- **POTA Rule 6:** "Fully automated QSOs are prohibited: Each contact must include direct action by both operators."
- LoTW/TQSL enforce nothing technically; enforcement is reputational (PSKReporter 24/7 sightings, ClubLog charts). The 2024 FT4GL episode (n0un.net/ft4gl-automation, 2024-06-05: 140 of 142 hours continuous FT8, 246 SSB of 40,947 QSOs; operator denied automation Sept 2024) shows the community *will* forensically audit logs and campaign for DXCC de-listing.

**The discourse:** two camps. Anti (SV5DKL 2018 statement, N0UN "Big FT8 Money Grab," LA3ZA "You have probably worked an FT8 robot" 2019, PE4BAS): robots cheapen awards, DXpeditions monetize LoTW via bots. Pro/pragmatic (KK5JY, updated 2025-12-21: ~100k automated QSOs over ~13.5k hours; "a well-written robot would be indistinguishable from a human… that's the point"; FCC "silent regarding the level of automation while a control operator is present"). Tools that survive (Auto FT8, WSJT-Z) all carry the same mantra: **"automation is allowed, unattended is not."**

**What gets a product shunned:** marketing unattended CQ; 24/7 PSKReporter footprints traced to your software; any wording implying QSOs happen "while you sleep"; DXpedition use for paid-LoTW farming. **What earns trust:** the software *refusing* to originate when nobody is present (Pancetta already does); a visible "operator present" heartbeat; logging the automation level per QSO (an `APP_PANCETTA_AUTO` ADIF field — auditable honesty, like the existing `APP_PANCETTA_HOUND`); a published one-page "Pancetta and the rules" (you have it: `docs/fcc-part97-compliance.md` — surface it on the landing page); never using "robot/bot/autonomous" in the tagline.

**Language recommendation:** replace "autonomous operator" with **"DX Hunter" / "supervised auto-sequencing"**; keep the `[autonomous]` config key for compatibility but document it as "operator-supervised automation." Add the line: *"Pancetta will not originate a CQ unless a licensed operator has touched the console in the last two minutes. This is enforced in code, not in a warning label."* That single sentence out-positions WSJT-Z and Auto FT8, whose guardrail is a README paragraph.

**International note:** the README's guardrail is US-shaped. IARU R1/R3 licensees have different automatic-control rules; the region-aware band-plan work (v0.9.6) shows the pattern — make the idle-gate configurable per jurisdiction with US-strict default.

---

## 6. Distribution, community, brand

**Where hams find software (H/M):** YouTube tutorials (Ham Radio Crash Course, KM6LYW, K0PIR, k8mrd, AB4OB — GridTracker alone has multiple 2024–25 review videos), groups.io lists (WSJTX main, GridTrackerApp, MSHV, RigPi, DXLab, N1MM), SourceForge (still the ham download hub — WSJT-X Improved, MSHV, WSJT-Z, JTDX all live there), eHam reviews (eham.net/reviews/view-product/12632 for WSJT-X), QRZ forums, dxzone.com listings, blogs (kb6nu, pe4bas, ei7gl), QST/CQ product reviews, Hamvention/Friedrichshafen forums, club Zoom talks. **GitHub stars are not the metric hams use** — MSHV has 18 stars and ~600 weekly downloads.

**Current footprint:** README-only, no website, no video (the demo GIF is a *replay*; the README itself has a TODO for a PSKReporter proof capture), no groups.io/Discord, no SourceForge mirror, no dxzone listing, no eHam entry, not indexed by search for "pancetta ham radio" (WebSearch 2026-09-05 returned zero relevant hits).

**Launch minimum (ordered):**
1. Landing page (pancetta.radio or a cqdx.io subpage) with a *real on-air* 3-minute video: SSH into a Pi, `pancetta doctor`, first decode, one supervised QSO, PSKReporter map. The replay GIF is honest but unpersuasive.
2. Install channels: Homebrew tap, `.deb` (Bookworm+), winget/scoop, notarized macOS binary, `cargo install pancetta`.
3. Compatibility table with screenshots: GridTracker 2, JTAlert, Log4OM, WaveLogGate receiving Pancetta UDP.
4. groups.io list (hams' native forum) + GitHub Discussions; SourceForge mirror for discoverability; dxzone + eham listings.
5. Beta program targeting 10 Pi/MiniPC headless operators with IC-7300/FT-991A/FTdx10/Flex — collect rig-validation table.
6. One YouTube reviewer seed (KM6LYW is the natural fit: Pi + digital), one QRZ forum post, one club talk deck.

**Name "Pancetta":** neutral-to-positive. Memorable, pronounceable, no collision in the ham space (search verified), Italian-kitchen family with cqdx's "panino" client; hams tolerate whimsical names (Wavelog, GridTracker, DigiPi, JTAlert). Weakness: says nothing about radio; every first mention needs the tagline. **Keep the name; fix the tagline.** Proposed: *"Pancetta — the headless FT8 station."* Avoid "autonomous" in the tagline; keep it in docs with the guardrail sentence.

**README as landing page:** strong on honesty (jt9 gap, "Why not (yet)"), weak on *why switch* for a normal ham — the first screen is a decoder-deficit admission. Reorder: (1) headless/one-binary demo, (2) guardrails, (3) integrations, (4) honest numbers. Also fix the LoTW/eQSL "scaffolded" wording vs the decision record.

---

## 7. cqdx.io tie-in: asset or moat?

**Facts:** cqdx.io/cqdx.app is a live "single-pane-of-glass DX spotting portal" (PSKReporter + RBN + cluster correlated with ClubLog Most Wanted; public OpenAPI at cqdx.app/api/docs; © Hagale Technologies; repo private, "All rights reserved") (`~/Code/cqdx/README.md`; cqdx.io homepage fetched via curl 2026-09-05, H). Pancetta uses it for: needed-DXCC feed (live), rarity/spots (live, envelope unverified), needed-grid (no endpoint), per-QSO logbook upload (confirmed 201 contract), remote-rig pairing/authorization broker (`pancetta pair`), and stamps *every* QSO COMMENT with "Using cqdx.io & Pancetta -- https://cqdx.io …" (`docs/DECISIONS/logging-uploads.md`).

**Asset:** live rarity + your-needs scoring inside the decoder loop is something no free stack does without GridTracker+ADIF gymnastics; a first-party API lets Pancetta ship endpoints in days. It is also the only plausible sustainability engine (§8).

**Concerns hams will raise:** (1) closed service owned by the same solo developer — bus-factor and "what if it goes away"; (2) the ADIF COMMENT advertisement into *their* logbooks (ClubLog/QRZ/LoTW records) reads as spam and will be called out on groups.io; (3) remote-TX authorization brokered by a cloud service — some will refuse any cloud in the TX path; (4) needs come *only* from cqdx today (needed-grid from nowhere), so "DX Hunter" degrades to rarity-only without an account.

**How to tell the story:** "Pancetta is a complete standalone station. Everything works offline except live rarity. cqdx.io is optional enrichment; bring your own needs by pointing Pancetta at your ADIF/LoTW report." Concretely: (a) make the COMMENT attribution opt-in (default off) or move it to an `APP_PANCETTA_*` field; (b) implement local needed-DXCC/grid derivation from `qsos.adi` + an imported LoTW ADIF so the scorer is useful without cqdx; (c) publish the cqdx API contract Pancetta depends on (already in dispensa) and state a deprecation policy; (d) keep the remote-TX *arm* local (it is — cqdx only brokers authorization; say so loudly).

---

## 8. Monetization / sustainability / license

**Norms (H):** WSJT-X, JTDX, MSHV, WSJT-Z, GridTracker (donations), JTAlert (donations), Log4OM, DXLab, Wavelog: free. Paid outliers: HRD (~$99 + ~$30/yr), N3FJP ($59.99 package), DigiPi (Patreon any-amount for the image; HAT hardware sold by third parties), RigPi (hardware). Verdict: a paid FT8 modem is a non-starter.

**Options ranked:**
1. **cqdx.io premium** (advanced needs analytics, alerting, remote-rig brokering, multi-station) with Pancetta free/OSS as the acquisition funnel — the only model with precedent (QRZ XML subscription, ClubLog donations, HamAlert). Requires trust work in §7 first.
2. **Donations/GitHub Sponsors/Patreon** — covers hosting, not time; fine as a signal.
3. **Hardware bundle** (Pi + audio HAT + preflashed image, DigiPi-style) — plausible later; inventory and support burden for a solo dev.
4. **Dual-license/commercial embedding** of `pancetta-ft8` (permissive already, so no leverage) — instead, the permissive license is a *reach* play: SDR vendors and app developers can embed it where they can't embed GPL wsjtr/ft8mon-derived code. Publish `pancetta-ft8`/`pancetta-dsp` on crates.io.

**License considerations:** MIT/Apache-2.0 with a documented clean-room firewall (`docs/PROVENANCE.md`) is a real differentiator *and* a legal exposure surface: the ham community has a history of scrutinizing WSJT-X derivatives (the JTDX/KI7MT/WSJT-X licensing disputes). The clean-room process is unusually rigorous; keep it, and consider an external audit statement before 1.0. Note that contributors reading GPL code and then contributing is the practical risk — CONTRIBUTING should state the firewall rule.

---

## 9. Positioning statement and 2×2

**For** licensed hams who run their FT8 station on a Pi/MiniPC behind the radio and operate from anywhere over SSH,
**who** are tired of stitching WSJT-X + GridTracker + a logger + a cluster client together over VNC,
**Pancetta is a** single-binary, headless-first FT8/FT4 station
**that** decodes, ranks every CQ by what you still need, runs the QSO, and uploads the log — with the strictest operator-presence guardrails in the hobby.
**Unlike** WSJT-X and its forks (GUI-bound, one QSO at a time, guardrails in a README) or the FT8 "autopilot" add-ons (scripts on top of someone else's modem),
**we** are one process, safe by construction (respond-only when you step away; fail-closed remote TX), permissively licensed, and measured openly against WSJT-X's own decoder.

**2×2 — X: operator model (GUI-at-the-radio → headless/remote-native). Y: automation depth (manual click → engineered station automation).**
- Bottom-left (GUI, manual): WSJT-X, JTDX, MSHV (MSHV drifts right/up for multi-answer).
- Top-left (GUI, automated): WSJT-Z, Auto FT8, FT8Commander — the "robot" quadrant in the community's eyes.
- Bottom-right (headless, manual): DigiPi/VNC Pi rigs, wfview/SmartLink remoting, ft8modem/jt9 scripts.
- **Top-right (headless-native, engineered automation with guardrails): Pancetta — alone.** The strategic risk is being *perceived* as top-left. The differentiating axis to emphasize in every communication is therefore *headless-native + safety*, not *automation*.

---

## 10. Ranked plays

**Defensive (protect what's unique)**
| # | Play | Effort | Why |
|---|---|---|---|
| D1 | Reframe: tagline "the headless FT8 station"; rename "autonomous operator" → "DX Hunter (supervised)"; put the §97.221 gate + E-stop in the first screen of README | 1–2 days | Removes the robot stigma before it forms |
| D2 | Verify & publish WSJT-X UDP compat vs GridTracker 2, JTAlert 2.81, Log4OM 2, WaveLogGate (screenshots, versions) | 2–3 days | Without it, no logger user can switch |
| D3 | Make ADIF COMMENT attribution opt-in; add `APP_PANCETTA_AUTO_LEVEL` field | 0.5 day | Preempts the groups.io backlash; auditable honesty |
| D4 | Rig validation program: IC-7300, FT-991A, IC-7610, Flex (DAX/CAT) — public matrix | 2–4 weeks elapsed, beta testers | "One radio, well tested" is a switching blocker |
| D5 | Real-hardware Pi 4/5 validation + effort-tier numbers published | 1 week | The wedge segment's core claim is currently unproven |

**Offensive (take share)**
| # | Play | Effort | Why |
|---|---|---|---|
| O1 | **Close the FT8 decoder gap to ≤1 dB** (ship W4.3 multipass regime-conditional +32 truths; W3.3b at Eco; multi-interval 11.8/13.5/14.7 s decode; residual sync 2.1→1.3) and re-publish the calibrated curve | 4–8 weeks | The only number DXers read; JTDX's orphaned base is waiting |
| O2 | FT4 decoder workstream (3.95 dB gap, 78% cap) | 3–6 weeks | FT4 users are contest/6 m — needed before any FT4 marketing |
| O3 | SuperHound (decode SuperFox waveform; open-source since 2.7.0-rc7 per sprocketfox.io/machamradio) | 3–6 weeks + clean-room spec | Without it a DXer must keep WSJT-X installed |
| O4 | Install channels: Homebrew tap, .deb, winget, notarized mac, `cargo install` | 1–2 weeks | Table stakes |
| O5 | Public browser client (panino web or a minimal TUI-over-WebSocket page) + open-source panino | 2–4 weeks | The remote story is invisible while the client is private |
| O6 | Local needs engine: derive needed DXCC/grid/band-slot from `qsos.adi` + imported LoTW report; cqdx becomes optional enrichment | 1–2 weeks | Removes lock-in objection; makes DX Hunter work offline |
| O7 | Launch kit: on-air video, landing page, groups.io, SourceForge mirror, dxzone/eham listings, one YouTube reviewer | 2 weeks | Discoverability from zero |

**Innovative (new ground)**
| # | Play | Effort | Why |
|---|---|---|---|
| I1 | Publish `pancetta-ft8` + `pancetta-dsp` on crates.io as the only permissive Rust FT8 codec (wsjtr is GPL) | 1 week | Developer segment; embeds in SDR++/OpenWebRX-class tools; inbound contributors |
| I2 | "Skimmer mode": multi-band RX-only decode → PSKReporter, using the anytime budget on a Pi | 2–3 weeks | Rides SDR-suite demand; zero regulatory risk; PSKReporter footprint = marketing |
| I3 | Operator-presence protocol as an open spec (heartbeat + `AUTO_LEVEL` in ADIF) proposed to ClubLog/LoTW/POTA | 1 week writing + advocacy | Turns the robot debate into a Pancetta-led standard; huge trust dividend |
| I4 | Multi-stream head-to-head demo vs MSHV (N QSOs/slot, clean ALC) | 3 days | The one feature story that makes DXpedition-adjacent ops look twice |
| I5 | DigiPi-style preflashed image ("Pancetta Pi") | 2–3 weeks | Appliance UX for the wedge segment; possible hardware-bundle revenue later |

**Hit list of concrete product changes implied**
- README: reorder (headless demo → guardrails → integrations → honest numbers); fix LoTW/eQSL "scaffolded" contradiction; replace replay GIF with on-air capture; drop the maintainer TODO comment from the public README.
- Tagline/description on GitHub: remove "optional hands-off operation"; use "operator-supervised DX hunting."
- Default `[network.pskreporter] enabled = true` (community norm; also marketing).
- ADIF COMMENT attribution → opt-in; add `APP_PANCETTA_AUTO_LEVEL`.
- Make the 2-minute presence gate jurisdiction-configurable (US default strict).
- Local needs derivation from ADIF/LoTW (O6); needed-grid endpoint or local fallback.
- Publish compatibility table (D2) and rig matrix (D4) in `docs/`.
- Ship W4.3 regime-conditional multipass; FT4 workstream; SuperHound.
- Homebrew/.deb/winget/notarization; `cargo install pancetta`.
- Open-source panino or ship a web client; document the remote threat model publicly (it's a selling point).
- Contest modes: explicitly *out of scope* in README "Why not (yet)" (currently unmentioned).
- crates.io publication of `pancetta-ft8`/`pancetta-dsp`.
- groups.io list + GitHub Discussions; SourceForge mirror; dxzone/eham listings; one on-air video.

---

## Sources (verified 2026-09-05 unless noted)

Internal: `README.md`, `FEATURES.md`, `CHANGELOG.md`, `docs/GUIDE.md`, `docs/fcc-part97-compliance.md`, `docs/decoder-comparison.md`, `docs/PROVENANCE.md`, `docs/cqdx-api-requirements.md`, `docs/DECISIONS/{modes,remote-operation,logging-uploads,2026-07-development-phases-and-gaps}.md`, `research/specs/catalog-other-ft8-projects.md`, `research/hypothesis_bank.md:4992`, `pancetta-dx/src/cluster.rs`, `~/Code/cqdx/README.md`; `gh api` repo/traffic/release data.

External: machamradio.com/blog/2025/02/19/wsjt-x-version-2-7-0-released (H) · sourceforge.net/projects/wsjt-x-improved/files (H) · github.com/jtdx-project/jtdx + /releases (H) · tracker.debian.org/pkg/jtdx (M) · sourceforge.net/projects/mshv (H) · github.com/sq9fve/wsjt-z README (H) · autoft8.com (H) · github.com/0x9900/FT8Commander, /AutoFT8 (H) · js8call.com/JS8Call-improved (H) · digipi.org, patreon.com/KM6LYW (H) · gridtracker.org; youtube.com/watch?v=DrXxE3XjU9c (M) · hamapps.com/JTAlert (H) · log4om.com/integrated (H) · docs.wavelog.org/user-guide/integrations/wsjt-x (H) · n1mmwp.hamdocs.com/manual-supported/contests-setup/setup-digital-contests (H) · wfview.org/download (H) · rigpi.net / groups.io/g/RigPi (M) · help.remotehamradio.com/help/operate-ft8-with-smartsdr-cat-and-dax (M) · crates.io/api/v1/crates/ft8core, /ft8-engine (H) · github.com/kgoba/ft8_lib, jl1nie/RustFT8, Reid-n0rc/FT8AF, G1OJS/PyFT8, f4exb/sdrangel, sannysanoff/SDRPlusPlusBrown, luarvique/openwebrx (H via gh api) · arrl.org/dxcc-rules (H) · arrl.org/news/arrl-contest-and-dxcc-rules-now-prohibit-automated-contacts (H, 2019-08-19) · contests.arrl.org/ContestRules/RTTY-RU-Rules.pdf (H) · docs.pota.app/docs/rules.html (H) · n0un.net/ft4gl-automation (H, 2024-06-05) · kk5jy.net/about-ft8 (H, updated 2025-12-21) · sv5dkl.blogspot.com/2018/04 (M) · la3za.blogspot.com/2019/08/robotic-ft8-contacts (M) · kb6nu.com/dxpedition-to-use-ft8-robots (M) · dx-world.net/super-fox-mode; sprocketfox.io/xssfox/2024/11/22/superfox-pt3; machamradio.com/blog/2024/10/07 (H) · g7vjr.org/2023/09/predicting-qsos-to-2025-using-ai ("about three-quarters of all QSOs are on FT4 and FT8", M) · hamradiodeluxe.com/buy (M) · n3fjp.com/aclog.html (M) · eham.net/reviews/view-product/12632 (M) · radio-hobbyist.com/what-is-jtdx (L, anecdotal decode claims) · cqdx.io homepage via curl (H). WebSearch for "pancetta ham radio FT8" returned no relevant results (H).
