# Lane 6 — Brand, README, Docs Architecture, Community, GitHub Presence

Repo: `/Users/thagale/Code/pancetta` @ main (v0.9.6, 2026-09-05). Evidence paths are absolute or repo-relative; GitHub data pulled live via `gh` on 2026-09-05.

## 1. Executive summary

1. **README 6/10.** Honest, substantive, measured — the "Why not (yet)" and jt9-benchmark framing are best-in-class for ham software. But it is 2,420 words (spec target ~1,200), the hero GIF is a 93-second recording whose first ~75 s show near-empty panels, the QSO panel is idle in every asset, and a maintainer TODO comment ships in the file.
2. **Visual identity 4/10.** The logo is a 7-row full-block "P" as SVG `<text>` — font-dependent, no wordmark, no relationship to the TUI palette, unreadable at the README's 64 px. Every screenshot/GIF/tape is branded `K5ARH` while the README credits `W5AU`; the title bar shows placeholder grid `AA00aa`.
3. **Docs architecture 5/10.** Excellent curated core (GUIDE/CONFIG/RUNBOOK/generated KEYBINDINGS) buried under 112 superpowers specs/plans, 12 dated point-in-time notes in `docs/` root, a 13-page agent `wiki/`, and a stale `FEATURES.md`. Three documents each claim to be "authoritative" with circular precedence.
4. **Writing 6/10.** README/GUIDE voice is confident and operator-first. CHANGELOG and DECISIONS are engineer-to-engineer with 100+-word sentences, PAN-xx/PR/Task-W1.4 residue, unexplained acronyms. Factual contradictions between docs (crate count 11 vs 14; tests ~200 vs ~295; FEATURES.md ">95% at −20 dB" vs measured −19.2 dB @ 50%).
5. **GitHub presence 5/10.** Good description/topics/asset naming/checksums; 87% community profile. But: 1 star, 1 fork, 15 unique visitors in 14 days; Discussions off while org SUPPORT.md says "start a Discussion"; no custom social preview; release notes are raw CHANGELOG dumps; no FUNDING; no Pages; two Codes of Conduct.
6. **Launch readiness 3/10.** No 1.0 criteria, roadmap, landing page, video, community channel, hardware-tested list, or external-user evidence; unsigned macOS/Windows binaries; no package-manager path; the AI-assisted development story is undisclosed to a community that will ask.
7. Biggest storytelling gap: the promise is "work QSOs" and no asset shows a QSO. Second: the K5ARH/W5AU split reads as two different people.
8. The cqdx.io section reads as first-party lock-in; the README never says "works fully standalone" even though the screenshots prove it (needed-DXCC markers appear with no cqdx configured).
9. "Pancetta" is memorable, ham-pun-adjacent (cured pork → "ham"), has zero collision in ham radio, and `pancetta.io`/`pancetta.radio`/`getpancetta.com` are unregistered — but the README never lands the joke and Google returns zero results for "pancetta ham radio".
10. Tracked already: GitHub #274 (spec-vs-assets reconcile), #272 (provenance wording), PAN-56 (toolchain pin, touches CONTRIBUTING). Everything else below is untracked.

## 2. Ranked hit list

Format: `[Pn] [S/M/L] title — what's wrong (evidence) — proposed change — why it matters`. `TRACKED:` marks items already in Linear/GitHub.

### P0 — fix before any outreach

1. **[P0][S] Two callsigns, one author** — README:9 "Tony Hagale, W5AU"; every screenshot title bar, both GIFs, `.tapes/demo.tape:41` (`callsign = "K5ARH"`), `docs/fcc-part97-compliance.md:103`, `docs/DECISIONS/qso-engine.md:100`, and `docs/superpowers/specs/2026-08-20-readme-visual-identity-design.md:34,90` say K5ARH. — Pick one public identity; re-render all assets; grep-fix docs. — Hams read callsigns first. Two calls looks like a fork, a ghost author, or an unlicensed demo.
2. **[P0][S] Maintainer TODO comment ships in README** — README:65-67 `<!-- TODO (maintainer): drop a PSKReporter map capture… -->`. Visible in raw/source, crates.io README, every fork. — Move to an issue; ship `assets/pskreporter.png` or delete. — Leaks process; marks the strongest ham-trust signal as missing 16 days after the spec called it out.
3. **[P0][M] Hero GIF never shows the product working** — `assets/demo.gif`: 92.9 s, 2,323 frames. 0-~8 s shell typing; ~8-75 s two decodes of LC1TVY at −24 dB, QSO Status `STANDBY / TX: Never / Monitoring for calls…`; payoff (28 decodes) ~75-88 s; final frame is a bare shell prompt. `.tapes/README.md` admits "the first ~60s… correctly shows empty decode panels"; `.tapes/demo.tape:76,83` `Sleep 75s` / `Sleep 45s`. — Cut to 12-20 s: open on a populated Operate view, show DX Hunter ranking, press `e`, end on a populated frame. Use a corpus that decodes from slot 1 (loop `live_now.wav`). — GitHub autoplays; a stranger sees typing and empty boxes and scrolls past.
4. **[P0][M] QSO panel idle in every asset** — `screenshot-qso.png` is 1200×700 of dashes, `No QSOs yet`, `-50 dB`; README:75-78 apologizes for it. — Record one loopback QSO (two-process replay; `loopback_qso` test already proves encode→decode) or capture a real FTdx10 QSO with a scrubbed log. Until then drop the screenshot and the paragraph. — The pitch is "work, log"; the only asset for that step is an empty form with an apology under it.
5. **[P0][S] `TX: FULL` green chip in a demo that cannot transmit** — every screenshot/GIF frame; CHANGELOG 0.9.6 Fixed says `--replay` starts no Hamlib and has no PTT capability. — Under `--replay` render `TX: REPLAY` / `TX: OFF (replay)`. — A skeptical ham sees an unattended-looking robot with TX enabled; contradicts the replay-safety story.
6. **[P0][S] Visible entity-resolution error in a marketing screenshot** — `screenshot-priority.png` and `screenshot-operate.png` row `KG4OJT … FM18 … Guantanamo Bay` (only 2×2 KG4AA-KG4ZZ are GTMO; 2×3 is US 4-land). — Fix the prefix rule / BigCTY exception; re-render. — The exact detail a DXer checks; undermines "a priority engine, not a list" in the image meant to prove it.

### P1 — README and identity

7. **[P1][M] README is 2× its own design target** — 350 lines / 2,420 words vs spec "~1,200 words" (`readme-visual-identity-design.md:88`). Quick start is 90 lines (README:163-252) with prebuilt + source interleaved and "step 3"/"step 4" referenced (README:189,195) before defined (README:231,240). — README keeps prebuilt install (≤10 lines) + link to `docs/INSTALL.md`; move effort-preset table (README:92-98) to CONFIG.md and cqdx caveats (README:139-144) to a cqdx doc. — Converting tier per your own research is 800-1,500 words.
8. **[P1][S] Three pitches, no hook** — README:8 tagline (feature list), README:17 "lives in your terminal", README:29 heading literally "The pitch". — One outcome tagline ("The FT8 station that picks the right station to call — and runs on the Pi behind your radio"); delete line 17; rename "The pitch" → "Why one binary". — First-10-seconds comprehension currently requires reading to line 36.
9. **[P1][S] Self-referential release-note rot** — README:165-166 "Starting with the first tagged release after this note was added…" (that release, v0.9.6, has shipped). — "Every release since v0.9.6 ships…". 
10. **[P1][S] Badges under-sell** — CI + license only (README:12-15). — Add latest release, downloads, platforms, MSRV; one row. — Prebuilt binaries are your biggest adoption lever and the README hides them below the fold.
11. **[P1][S] License badge vs GitHub detection** — badge `MIT OR Apache-2.0`; `licenseInfo` = Apache-2.0 only. — Add a root `LICENSE` stub explaining dual licensing. 
12. **[P1][M] Logo is `<text>` glyphs, not paths** — `assets/logo.svg` `font-family="'IBM Plex Mono',…monospace"` with `██` characters; rendering differs per viewer font (the "loop right edge" fight in spec:106-108 is exactly this). At 64 px it's an orange staircase. — Convert to `<rect>` geometry; add wordmark lockup + 1:1 icon for favicon/social. — A font-dependent logo is not a logo.
13. **[P1][S] Logo color has no relationship to the product** — terracotta `#c1592e` vs a cyan/yellow/green TUI with no orange anywhere. — Adopt terracotta as TUI accent (title, selection) or derive the logo from the TUI cyan. — Brand is repetition; today there is none.
14. **[P1][S] Placeholder grid in every asset** — `AA00aa` in all title bars. — Set a grid in `.tapes/demo.tape:41`. — Reads as "not configured".
15. **[P1][S] Truncation/garbling visible in assets** — `screenshot-operate.png`: `No one cal`, `|=live ①cand` overlap, `Station: NY6C — --- new`; `screenshot-waterfall.png`: clock clipped `MON15:12:47 UT`; DX Hunter headers `Gri`/`Rar`/`Las`. — Render at ≥140 columns or fix layout minimums. — Screenshots make layout bugs permanent.
16. **[P1][S] Rarity column empty in the "priority engine" screenshot** — `Rarity` = `-` on all 27 rows (no cqdx in tape). — Configure cqdx read-only for renders (read traffic still runs under replay per CHANGELOG) or hide the column when unpopulated. 
17. **[P1][S] Waterfall screenshot is grayscale blocks** — 16-color density fallback look; `TX ▼50▼` marker unreadable. — 256-color VHS theme; label `TX 1500`. — The waterfall is the visual every ham compares to WSJT-X.
18. **[P1][S] Screenshot captions are technical, not narrative** — README:83,85. — One benefit sentence each.
19. **[P1][S] Jargon with no glossary; Rust identifiers in prose** — parity, ALC, OSD, LDPC, CRC, glibc, PTT, CAT, rigctld, ATNO, POTA/SOTA, DXCC, Hound/Fox undefined; `TxPolicy`, `TxOrigin::Remote` at README:118-123. — `docs/GLOSSARY.md`; scrub type paths from README. 
20. **[P1][S] Trust section written as invariants, not promises** — README:105-127 is near-verbatim AGENTS.md:65. — Three operator promises (always stoppable / never originates without you / never keys a stale QSO), link to compliance doc, add a non-US line. — This section decides whether a ham trusts the robot.
21. **[P1][S] The name's story is never told** — pancetta = cured pork = "ham"; Italian family with `panino`; `cqdx` breaks the family. — One line under the title. Register `pancetta.radio` (whois: available; `pancetta.io` also available) before publicity. 

### P1 — Docs architecture and truth

22. **[P1][S] Circular "authoritative" precedence** — `docs/README.md:3-7` curated wins; `docs/README.md:34` + `AGENTS.md:75` specs "authoritative for current behavior"; `AGENTS.md:94` code and docs win. — One rule: code > curated docs > DECISIONS > specs/plans (history). 
23. **[P1][M] `FEATURES.md` is stale marketing** — `FEATURES.md:5` ">95% decode accuracy at SNR −20 dB" vs `docs/decoder-comparison.md:141` SNR@50% = −19.2 dB; `:25` weighted-sum 35/20/15/10/5 vs 0.9.6 lexicographic tiers (CHANGELOG:65-72); `:45` "DX hunter… sourced from cqdx.io spots"; typo "Ordererd"; `:41` hard-codes model 1042/38400. Linked from README:317 as "feature status". — Delete; replace with a 10-row status table in README or `docs/STATUS.md`. — The one doc a stranger opens to verify claims is wrong on the headline number.
24. **[P1][S] Crate/test count drift** — `docs/ARCHITECTURE.md:3` "11-crate" vs README:304/AGENTS.md:15 "14-crate" (14 on disk); `FEATURES.md:5`/`ARCHITECTURE.md:210` "~200 tests" vs README:62/AGENTS.md:44 "~295". — Fix ARCHITECTURE; derive counts or drop them.
25. **[P1][S] hamlib status contradiction acknowledged instead of fixed** — README:277-280 says "the project's own status table still calls the crate an integration stub" (AGENTS.md:25) while README:61 says TX validated and SECURITY.md:74 says bindings are dead code. — Update AGENTS row; remove the self-referential caveat. 
26. **[P1][S] SECURITY.md stale** — `:23-24` "Tagged releases will be added… once they exist" (two exist); `:64-66` "does not transmit audio contents anywhere off the local machine" is imprecise now the station agent streams spectrum/decodes to relay peers (CHANGELOG 0.9.6 Added). — Version table; reword to "raw audio never leaves; decoded messages/spectrum summaries do when remote operation is enabled".
27. **[P1][M] Point-in-time notes pollute `docs/` root** — 22 files; 12 dated/one-off (`security-review-2026-04-29.md`, `ux-audit-2026-06-14.md`, `qso-tx-deep-review-2026-07-18.md`, `qso-engine-bugs.md`, three `*-plan.md`…). GitHub `homepageUrl` lands on this listing. — Move to `docs/history/notes/`; ≤10 curated files at root.
28. **[P1][S] Operator RUNBOOK contains a developer research loop** — `docs/RUNBOOK.md:361-448` "Decoder Research Iteration Loop… (Claude-driven)", `as of 2026-05-31`. — Move to `pancetta-research/README.md`.
29. **[P1][S] `docs/README.md` isn't the entry point** — README:307-320 lists 12 docs individually and never links `docs/README.md`. — README Docs: 4 links + "All docs →". 
30. **[P1][M] Missing user docs** — no FAQ; no "Pancetta vs WSJT-X"/migration (GridTracker/JTAlert UDP interop buried at CONFIG.md:437); no hardware-tested matrix (only FTdx10 anywhere; GUIDE.md:30 names IC-7300 as an example only); no non-US "what it will never do" page; no glossary; no privacy/data-flow page (PSKReporter, cqdx, DX cluster, and a **baked-in shared ClubLog application key** per GUIDE.md:178-180 — disclose); no upgrade notes. — Add FAQ, COMPARED-TO-WSJT-X, HARDWARE, PRIVACY, GLOSSARY. — These are the pages hams link each other in groups.io threads.
31. **[P1][S] Date and log residue in user docs** — `RUNBOOK.md:356,389,399,421-427`, `ARCHITECTURE.md:167,216-217,296`, `fcc-part97-compliance.md:3` "Date: 2026-06-23" on a doc README cites as current; CHANGELOG cites `Task W1.4`, `PR #263`, `PAN-17`, "Round 4 note". — Replace with "verified against v0.9.6"; dates live in git.
32. **[P1][S] `wiki/` is agent memory in a public repo** — 13 pages + `manifest.json`, `wiki.toml` `visibility = "public"`; duplicates DECISIONS (modes/tx-scheduling/qso-engine/remote-operation/tui/config-and-platform in both). — Promote the better-written wiki pages into DECISIONS and delete `wiki/`, or move under `.agents/`. — Third parallel knowledge base.
33. **[P1][S] TRACKED #274 — spec vs delivered assets** — spec promised PSKReporter capture, 2-3 feature GIFs, `vhs-action` CI regen; delivered one hero, one feature GIF, manual-only regen (`.tapes/README.md` §CI). — Amend spec status or deliver.

### P1 — GitHub presence and releases

34. **[P1][M] Release notes are raw CHANGELOG dumps** — `gh release view v0.9.6` body ≈180 lines incl. a 35-line `--replay` internals paragraph; no highlights, install snippet, checksum instruction, or upgrade notes. — Template: 3 highlights → per-OS install → verify sha256 → full changelog in `<details>`. — Release pages are where video/blog links land.
35. **[P1][S] Not pre-release; 10-week gap; Unreleased rot** — v0.9.5 (GH 2026-07-17; CHANGELOG:74 says 06-24), v0.9.6 (09-02), `isPrerelease: false`; 5 PRs (#342-346) merged since with `[Unreleased]` empty (CHANGELOG:8). — Mark 0.9.x pre-release; 2-3-week cadence; CI check that PRs touch Unreleased.
36. **[P1][S] Discussions off while org SUPPORT.md says "start a Discussion"** — `hasDiscussionsEnabled: false`; 12 open issues all self-filed replay debt (#264-#275), unlabeled. — Enable Discussions (Q&A / Show-and-tell / Hardware reports); label issues; pin a start-here issue. — "Does it work with my IC-7300?" is a Discussion and today it bounces.
37. **[P1][S] No custom social preview** — `usesCustomOpenGraphImage: false`. — 1280×640: lockup + populated TUI frame + tagline. 
38. **[P1][S] Two Codes of Conduct** — `CONTRIBUTING.md:17-40` home-grown vs org Contributor Covenant (community profile). — Delete the section; link the org file.
39. **[P1][S] Community profile `issue_template: null`** despite `.github/ISSUE_TEMPLATE/*.md`. — Verify detection; add `config.yml` (`blank_issues_enabled: false`, Discussions contact link).
40. **[P1][S] Topics miss real searches** — 8 topics; add `ft4`, `raspberry-pi`, `hamlib`, `headless`, `wsjtx`, `dx-cluster`, `pskreporter`.
41. **[P1][S] No FUNDING.yml, Pages, crates.io, Homebrew, AUR, or binstall** — `publish = false` (Cargo.toml:42). — Add `cargo-binstall` metadata (free with current asset names), a tap, FUNDING.yml. — "brew install pancetta" is a tweet.
42. **[P1][S] Unsigned macOS/Windows binaries** — README:185-187 `xattr -d`; Windows will SmartScreen. — Notarize; document Windows verification; state the plan. — `xattr -d` is where a non-technical ham stops.

### P1 — CONTRIBUTING and the AI-authorship story

43. **[P1][M] CONTRIBUTING.md is template residue** — `:51` "Discord (optional): Join our community" (none exists); `:48` "Rust 1.70+" vs `rust-toolchain.toml` (TRACKED PAN-56); `:74-83,155-172` `--all`/`-D warnings`/`--no-default-features` (README uses `--workspace --features transmit`; `--no-default-features` is broken per #270); `:408-418` ">80% coverage/tarpaulin" not in CI; `:455-477` embedded PR template differs from `.github/PULL_REQUEST_TEMPLATE.md`; `:509` "review within 48 hours"; `:558` "73 de Pancetta Team". — Rewrite to ~120 lines; keep `:143-149` (clean-room) and `:305-360` (check.sh, host-capability tests). — A contributor following this file gets a failing build and a Discord that doesn't exist.
44. **[P1][M] AI-assisted development undisclosed** — `AGENTS.md` (113 lines), `docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md` (336 lines, Codex, 25-round policy), `wiki/`, `RUNBOOK.md:406` "Claude-driven", bot review threads on every PR — none referenced from README/CONTRIBUTING. — README "How it's built" (6 lines) + CONTRIBUTING "Review process": design, invariants, on-air validation and merge decisions are the human licensee's; implementation is AI-assisted; every PR passes an independent automated reviewer + CI (~295 FT8 tests, bit-exact vs ft8_lib); clean-room rule binds agents too; expect bot comments, here's the convergence policy. Frame as rigor and transparency; never lead with it, never hide it. — The community will find AGENTS.md in five minutes. Disclosed it's a strength ("more review than most ham software gets"); discovered, it's the story.
45. **[P1][S] Agent-facing text leaks into human docs** — `docs/README.md:32-35`, `AGENTS.md:96` "You are never alone in this repo", gaps doc HTML header about CLAUDE.md consolidation, `wiki/INDEX.md` last entry. — Agent instructions stay in AGENTS.md; human docs link once.

### P2 — CHANGELOG and writing

46. **[P2][M] CHANGELOG entries are engineer paragraphs** — `--replay` Fixed entry ≈35 lines/450 words (CHANGELOG:161-195); `k` rebind ≈30 lines (:87-113); Rust paths, PAN-xx, PR numbers, review rounds throughout. — Operator sentence in bold, mechanism in `<details>` or PR link.
47. **[P2][S] Breaking changes unflagged** — `j`/`k` removed and `k` made global (:100-113) buried in Changed; `password_encrypted` → `password` rename (:315-320) breaks configs with no Migration line; no Removed/Deprecated/BREAKING markers anywhere. — `⚠ BREAKING` prefix + "Upgrading" per release.
48. **[P2][S] "Project History" stale** — CHANGELOG:158-160 "moving toward Phase 5"; Phase 5 is a RUNBOOK procedure now. — One "Origins" paragraph or drop.
49. **[P2][S] Sentence length / passive** — README:165-173 one 70-word sentence; CONFIG has 27 passive hits vs GUIDE 12. — ≤25-word median in README.
50. **[P2][S] Provenance explained three times** — README:316,340 + CONTRIBUTING:143-149 + PROVENANCE.md (TRACKED #272 wording). — One explanation, one link.
51. **[P2][S] FCC doc US-only, dated** — no Region 1/3 section; header date. — "Outside the US" pointers (Ofcom/ACMA/ISED); "reviewed against v0.9.6".
52. **[P2][S] `pancetta-research` disclaimer repeated** — README:289-290, AGENTS.md:31, gaps doc. — One canonical statement.
53. **[P2][S] Say KEYBINDINGS is generated + drift-tested** — README:102 just links it; free trust signal.

### P2 — cqdx.io cross-brand

54. **[P2][M] cqdx.io section reads as lock-in** — README:132-161 (longest feature section) presents rarity/needed/spots/logbook/remote as cqdx features; never says standalone works; never discloses same-author ownership (AGENTS.md:58 does); never says free/paid; `panino` has no link (README:153). Screenshots prove offline `+▲` needed markers without cqdx. — 3-column table Feature / Standalone / With cqdx.io; one disclosure line "built by the same author; never required"; caveats → `docs/cqdx.md`. — Hams distrust funnels to an author-owned web service.
55. **[P2][S] Three product names, no map** — Pancetta / cqdx.io / panino. — "Pancetta runs the radio. cqdx.io knows what you need. panino lets you watch and QSY from your phone." Mention `pancetta pair` (0.9.6) which README omits.

### P3 — polish

56. **[P3][S]** README:292-305 "Building, testing, lint" → CONTRIBUTING.
57. **[P3][S]** GIF sizes 638 KB / 449 KB fine; a 15-s cut lands <300 KB.
58. **[P3][S]** `.tapes/README.md` "regenerated 8× in 3 h" war story → DECISIONS.
59. **[P3][S]** `docs.yml` builds rustdoc to a 30-day artifact nobody finds → publish to Pages `/api/`, link from README:322.
60. **[P3][S]** `pancetta/Cargo.toml:3` description differs from GitHub's; keywords lack `ft4`. Align before crates.io.

## 3. Proposed README outline (~1,100 words) and docs IA

### README

```
[logo lockup 200px]  Pancetta
One-line outcome tagline
[badges: CI · release · downloads · platforms · license]

[hero GIF ≤20 s: populated Operate → DX Hunter ranking → press e → still frame]

## Why one binary                      (80 words: four-window problem; the Pi behind the radio)
## What you get                        (5 bullets ≤25 words, each linking a doc:
                                        jt9-measured decoder · priority tiers · multi-stream TX · headless/Pi · doctor)
## You stay the control operator       (3 promises + Shift+Q; link compliance doc; US and non-US line)
## Install                             (prebuilt: 3 commands per OS in <details>; "From source →")
## First five minutes                  (wizard → doctor → first decode; link GUIDE)
## Works alone, better with cqdx.io    (3-column table; ownership disclosure; Pancetta/cqdx/panino map)
## Where it stands                     (keep "Why not (yet)"; add "Tested hardware: FTdx10 — report yours →")
## How it's built                      (6 lines: Rust, clean-room, tests, human-owned design + AI-assisted
                                        implementation, independent review on every PR)
## Docs                                (Guide · Config · Troubleshooting · FAQ · vs WSJT-X · All docs →)
## Community                           (Discussions · releases · security · contributing · sponsors)
## Acknowledgments & License           (keep, tightened; one provenance link)
```

### Docs information architecture

```
docs/
  README.md              index + precedence rule (code > curated > DECISIONS > history)
  GUIDE.md               (exists, excellent)
  INSTALL.md             prebuilt per-OS, source, Pi, signing status (lifts README:163-252)
  CONFIG.md, KEYBINDINGS.md (generated), TROUBLESHOOTING.md (+ "doctor said X" index)
  FAQ.md                 legal? real FT8? vs WSJT-X? Pi? needs cqdx? AI?
  COMPARED-TO-WSJT-X.md  feature/decoder/interop table; migration (ADIF, GridTracker UDP)
  HARDWARE.md            tested matrix + report template
  OPERATING.md           control-operator promises, autonomy modes, FCC + non-US (fcc doc as appendix)
  PRIVACY.md             what leaves the machine per integration; baked-in ClubLog key
  GLOSSARY.md
  RUNBOOK.md             ops only (research loop → pancetta-research/README.md)
  ARCHITECTURE.md (fix crate count), PROVENANCE.md (+ how it's built), decoder-comparison.md,
  cqdx.md (lifted caveats), DECISIONS/
  history/               specs/, plans/, notes/ — banner: "design history, not current behavior"
CONTRIBUTING.md          ~120 lines; links org CoC; describes bot review + convergence policy
CHANGELOG.md             operator sentence + <details>; ⚠ BREAKING + Upgrading per release
FEATURES.md              delete
wiki/                    fold into DECISIONS or move under .agents/
```

Docs site: not yet — the spec's reasoning holds (no external contributors; 15 uniques/14 d). Trigger: first external hardware report or first groups.io thread. Then mdBook straight from `docs/` on Pages at `pancetta.radio`, rustdoc under `/api/`.

## 4. Five world-class moves

1. **Ship the QSO.** One asset showing CQ → report → RR73 → logged (real FTdx10 session or two-process loopback replay), with the PSKReporter map beside it. Everything else here is polish next to the fact that the promise is never shown.
2. **90-second video with a face and a callsign.** Headless Pi behind the radio, SSH in, `pancetta doctor`, DX Hunter ranks an ATNO, Shift+Q stops it. Pitch to KM4ACK / Temporarily Offline / Ham Radio 2.0 as the spec's Phase 3 planned. YouTube is the discovery channel for this demographic; SEO is not.
3. **"Why I built a robot FT8 station — and what it will never do."** Blog post (cqdx.io or `pancetta.radio`): control-operator promises first, the §97.221 analysis, the jt9 gap stated plainly, the AI-assisted build disclosed as rigor. Post to groups.io `SoftwareControlledHamRadio`, mastodon.radio, r/amateurradio. Becomes FAQ + README "How it's built" + the answer to every skeptic thread.
4. **Hardware-tested program.** `docs/HARDWARE.md` + "Report your rig" issue template + Discussions category. Seed with FTdx10; ask the first five testers for IC-7300/IC-705/FT-991A/K3/QDX. Each report is a testimonial, a compatibility row, and a star. Cross-link PAN-60 (serial re-enumeration) as "we listen".
5. **Define 1.0 in public.** Pinned `ROADMAP.md`: 1.0 = config/CLI/on-disk stable 6 months; FT8 within 1 dB of jt9; ≥3 rigs verified by others; signed binaries on all four targets; Homebrew + binstall. Pre-release every 2-3 weeks until then. A measured, dated promise is the same instinct that made "~2.1 dB more SNR" the right headline.

## 5. Already excellent — do not regress

- **The honesty register**: "competitive, not yet class-leading"; FP cost stated beside recall; demoting the +11.6% ft8_lib number; "Why not (yet)"; "One recording of one small corpus proves nothing about recall". This is the brand.
- **"Part 97 is your responsibility, not the software's"** (README:248-251) and the NOCALL refusal.
- **`docs/GUIDE.md`** — timed steps, "Green doctor = you will decode", the two first-run killers. Model the rest on it.
- **`docs/decoder-comparison.md`** — methodology-first, corpora and seeds named. Cite it; don't summarize it away.
- **Generated, drift-tested `KEYBINDINGS.md`**; the `.tapes/` reproducible-asset pipeline on real off-air audio — keep "no mockups" as a standing rule.
- **Release asset hygiene** — target-triple names, `.sha256` per artifact, CI-verified real decoder, Pi glibc requirement stated.
- **Clean-room provenance** (`docs/PROVENANCE.md`, CONTRIBUTING:143-149) — rare, correct, a real differentiator vs GPL-derived FT8 tools.
- **Acknowledgments** — K1JT/K9AN, YL3JG, WSJT-X with exact roles and "does not link or vendor". Do not shorten.
- **`docs/README.md`'s curated-vs-working-notes split** — right instinct; make the precedence non-circular.
- **0.9.6 replay-safety work** — "a demo can never key a real transmitter or pollute a log" is a trust asset; tell it in one FAQ sentence, not 35 CHANGELOG lines.

## Appendix — evidence snapshot (2026-09-05)

- `gh repo view`: stars 1, forks 1, watchers 1, 12 open issues, Discussions off, Wiki off, custom OG image no, homepage → `/tree/main/docs`, license detected Apache-2.0, 8 topics, created 2026-03-02.
- Traffic (14 d): 54 views / 15 uniques; referrers Google 7, github.com 3.
- Releases: v0.9.5 (2026-07-17), v0.9.6 (2026-09-02); 8 assets each incl. sha256; not pre-release; body = CHANGELOG dump.
- Community profile 87%: CoC via org `.github` (Contributor Covenant); SECURITY via repo; SUPPORT via org ("start a Discussion"); `issue_template: null`.
- Assets: 4 PNG 1200×700; `demo.gif` 1200×700, 92.9 s, 2,323 frames, 638 KB; `feature-decode-effort.gif` 6.1 s, 449 KB; `logo.svg` 653 B, `<text>` glyphs, `#c1592e`.
- Name: GitHub collisions `0ct0sec/M5PANCETTA` (45★, wardriving), `sedrickkeh/PANCETTA` (NLP dataset), user `pancetta`; none in ham radio. Web search "pancetta ham radio FT8": zero hits. whois: `pancetta.io`, `pancetta.radio`, `getpancetta.com` unregistered; `.dev`/`.app` unresolved.
- Linear PAN (17 listed): no brand/docs/community tickets; PAN-56 (toolchain) touches CONTRIBUTING:48. GitHub #274, #272 are the only docs items; #270 contradicts CONTRIBUTING:170.
- Docs inventory: `docs/` root 22 files (8 curated per docs/README.md, 12 point-in-time, 2 indices); `DECISIONS/` 10; `superpowers/specs` 55 + `plans` 57; `engineering/` 2; `operations/` 1; `archive/` 3; `wiki/pages` 13; root: README, FEATURES, CHANGELOG, CONTRIBUTING, SECURITY, AGENTS (CLAUDE.md → `@AGENTS.md`), THIRD-PARTY-NOTICES.
