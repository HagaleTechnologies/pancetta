# Pancetta product assessment — 2026-09-07

**Review baseline:** [17ffe360fbc1f1f2afac6615aad76d6df39f4c32](https://github.com/HagaleTechnologies/pancetta/tree/17ffe360fbc1f1f2afac6615aad76d6df39f4c32). The workspace declares version 0.9.6; this commit is **24 commits ahead of the published v0.9.6 tag**. Conclusions about this source do not imply that the release archive contains it.

**Scope:** market position, fitness for purpose, operator workflows, aesthetics, usability, interoperability, distribution, maintainability, and priorities for an excellent product. This checkout is a Rust radio station and terminal application; a deployed website and companion applications are outside it. The separate private security review remains the authority for its security findings.

**Decision assumption:** prioritize the owner's working station, then a polished open-source product for other operators; assess commercialization as an option. No customer interviews, willingness-to-pay study, live service certification, or on-air qualification was performed for this report.

## 1. Assessment

Pancetta has a credible foundation for a useful integrated FT8 station. It has working protocol machinery, a real QSO engine, hardware integration, four task-oriented terminal views, station prioritization, log handling, diagnostics, and substantial test coverage. It is beyond a UI prototype. Its documented FTdx10 operating experience and implemented rig switching support continued investment. [README:57](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/README.md#L57), [live rig switching](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/tui_relay.rs#L2392), [loopback integration tests](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/tests/loopback_qso.rs)

It does **not yet establish the broader promise of replacing an experienced operator's station stack with less work and equal confidence**. The principal gaps sit between components: getting configured, knowing whether “needed” is authoritative, seeing critical state in a small terminal, understanding why a contact was chosen, knowing that a log upload succeeded, and recovering without ambiguity. Sections 4–6 supply the evidence.

The strongest product direction is:

> An integrated FT8 workstation that helps an operator choose valuable contacts, makes every transmit decision understandable, and reliably carries each contact into the logbook.

Keep the autonomous-station research objective. Give it an explicit operating profile and qualification criteria. Lead award-oriented positioning with operator-assisted operation: “operator present” and “operator initiated each contact” are different requirements. Current DXCC rules require direct, contemporaneous initiation by both operators for each claimed contact; remote initiation is allowed. Pancetta's recent-keypress presence gate does not by itself demonstrate that condition. This is an award-eligibility distinction, not a conclusion about the legality of a particular station. [ARRL DXCC Rule 6(a)](https://www.arrl.org/dxcc-rules), [presence window](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/mod.rs#L561), [initiation gate](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/autonomous.rs#L615)

Recommended ordering:

1. Make the existing FT8 station workflow trustworthy and understandable.
2. Prove it works repeatedly on the owner's actual Windows/FTdx10 station.
3. Qualify onboarding and recovery with a small external operator cohort.
4. Expand hardware, specialized modes, remote operation, and commercial scope as their qualification evidence becomes available.

A graphical rewrite, additional modes, and an AI operator are not prerequisites for those outcomes.

## 2. Evidence and limits

| Evidence class | Work performed | What it supports |
|---|---|---|
| Current source | Traced configuration, rendering, prioritization, decoder dispatch, QSO history, upload dispatch, rig switching, presence gating, CLI and release definitions | Statements about implementation and integration boundaries |
| Current renderer | Actual Ratatui renderer: Operate/Hunt/Run/Monitor at 80×24, 100×30, 132×40; two additional isolated QSO-detail fixtures | Layout results for those states and dimensions |
| Current configuration parser | Five inputs: minimal station; autonomous toggle; minimal UDP toggle; the two GUIDE UDP recipes | Parse results, not a full CLI or first-run usability trial |
| Existing visual assets | Inspected four repository screenshots | Qualitative design observations; these are historical replay assets |
| Existing measurements | Read decoder comparison and its methodological caveats | Published historical results, not a fresh current-product benchmark |
| Tests in this session | Reused baseline tests from the separate security assessment; ran the product capture helper successfully | Results in §9; full workspace is not green |
| Market research | Primary project/vendor documentation and current ARRL DXCC rules, accessed September 7 | Published alternatives and capabilities, not independently tested competitor quality |

The [September 5 assessment](2026-09-05-product-review/README.md) is useful input, not a current defect inventory. This report rechecks selected claims and synthesizes priorities; it does not certify every item in that review's six lanes.

The [evidence bundle](2026-09-07-product-evidence/README.md) includes reproducible capture code, 12 current view buffers, parser output, and the validation summary. Fixtures have no audio or rig and transmit policy is disabled. These are rendered application states, not proof of a completed on-air QSO. No private station logs, credentials, or security reproductions are included.

## 3. Market position and fitness for purpose

### Competitive alternatives

The competitive unit is the operator's complete workflow, including existing habits and integrations. “Four windows” in the README is a useful hypothesis about friction; it is not evidence that every prospective user needs four programs or wants to replace them. [Current pitch](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/README.md#L29)

| Alternative | Published offering relevant to Pancetta | Product implication |
|---|---|---|
| WSJT-X | Current downloads list 3.0.1 and native distribution options across Windows, macOS and Linux, including ARM64. Its manual includes parallel FT8 decoding and worked-before filtering. [Downloads](https://wsjt.sourceforge.io/downloads.html), [manual](https://wsjt.sourceforge.io/wsjtx-main_en.html) | A replacement needs measured modem quality, familiar interoperability and an easy way back. |
| WSJT-X Improved / PLUS | Alternative layouts, including widescreen; dark styling; PLUS alerts and Cloudlog integration. [Project documentation](https://wsjt-x-improved.sourceforge.io/) | Layout and integration convenience already compete for the same user attention. |
| JTDX | An actively distributed alternative digital-mode application. This review did not benchmark its decoder or audit its code. [Project distribution](https://sourceforge.net/projects/jtdx/) | Include it in interviews and migration trials; do not claim performance superiority without a controlled comparison. |
| GridTracker | Maps, live/history inspection, award-oriented call roster, alerts, ADIF import and offline use. It describes itself as a companion rather than a logging program. [Introduction](https://docs.gridtracker.org/latest/Introduction/What-is-GridTracker.html), [call roster](https://docs.gridtracker.org/latest/Making-GridTracker-Work-For-You/Using-Call-Roster.html) | “Shows needed stations” is insufficient differentiation. Correct need data, explanations and fewer actions matter. |
| JTAlert | Wanted-call/entity/state/grid alerts, log scanning, and integration with established loggers. [Official site](https://hamapps.com/) | Operators will compare the completeness of their own award and logging workflow. |
| MSHV | Multi-answer operation with a queue and one to five transmit slots, a built-in log and ADIF export. [Official documentation](https://lz2hv.org/node/10) | Multi-stream FT8 is not unique to Pancetta. Demonstrate understandable control and successful contacts in appropriate operating contexts. |
| DigiPi | A Raspberry Pi appliance offering browser/phone access to radio tools, including WSJT-X through a remote graphical interface. [Official project](https://digipi.org/) | A small computer behind the radio and remote access are established choices. Distinguish Pancetta through a coherent station experience. |
| Wavelog | Browser logging, awards/statistics and integration paths, including WLGate. [Project](https://www.wavelog.org/), [documentation](https://docs.wavelog.org/) | Reliable exchange with a preferred logger may deliver more value than rebuilding every general-purpose logger feature. |
| HamRadioWeb Companion | The newly registered project advertises browser FT8/FT4 through WSJT-X/JTDX, PSKReporter comparisons, logging and DXCC/grid hunting. These are vendor claims, not independently verified results. [Project page](https://sourceforge.net/projects/hamradioweb/) | Browser operation is an active competitive space, not an uncontested category. |

This research establishes alternatives and feature overlap. It does **not** establish market share, market size, switching intent, retention, or a viable price. Download counters from different sites are not comparable active-user measurements.

### Where Pancetta can win

The opportunity is the quality of the integrated workflow: one consistent interpretation of station readiness, target value, active QSO, transmit placement, log durability, and recovery. The implementation shares coordinator and QSO machinery across these concerns; that creates an opportunity, not proof that the experience is already consistent. [Coordinator](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/mod.rs), [QSO integration](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/qso.rs), [TUI relay](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/tui_relay.rs)

A keyboard-first interface over SSH is valuable for technically comfortable operators. Rust, crate count, internal scoring algorithms and a single executable are engineering attributes; demonstrate their benefit as fewer setup failures, predictable operation and easier support.

The strongest initial audience is an FT8 operator with an existing station and logbook, comfortable with a terminal, who wants less repetitive work. Broader beginner appeal requires the onboarding and distribution work below. An unattended award-harvesting pitch would conflict with the per-contact initiation requirement in §1.

### Goal-by-goal fit

| Goal or need | Assessment at reviewed source | Evidence and remaining qualification |
|---|---|---|
| Decode, call, complete and log ordinary FT8 QSOs | **Implemented; field confidence still narrow** | QSO engine and loopback tests exist; README reports FTdx10 on-air use. This review did not operate hardware. [Tests](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/tests/loopback_qso.rs), [operating evidence](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/README.md#L61) |
| Prioritize personally valuable stations | **Partially met** | Needed/entity/grid scoring and local history exist. Unknown-data semantics and provenance remain weak; P04. |
| Replace routine context switching | **Promising, incomplete** | Four views combine major tasks. Compact clipping and upload/logbook closure still force interpretation or external checks; P01–P06. |
| Supervised autonomous FT8 station | **Mechanisms implemented; acceptance target needs precision** | Recent console activity gates initiation; headless never-seen input is not present. A headless process is not a session with recognized control presence. [Presence](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/mod.rs#L571) |
| Award-oriented operating | **Assisted mode is the clearer fit** | Distinguish worked, confirmed, needed, and unknown; preserve per-contact initiation context. No general eligibility certification is established. |
| Reliable remote station operation | **Not ready to certify here** | Separate private security assessment has unresolved readiness findings. Add disconnect, latency, restart and control-state qualification before widening access. |
| Hound, contest, POTA/SOTA specialization | **Uneven; qualify separately** | Specialized machinery exists, but a portable callsign-suffix heuristic is not an authoritative activation feed. Qualify complete tasks separately from individual features. [Heuristic](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-qso/src/priority.rs#L222) |
| Broad plug-and-play distribution | **Partially met** | Four platform archives; narrow hardware evidence; macOS manual quarantine-removal instructions. [Release](https://github.com/HagaleTechnologies/pancetta/releases/tag/v0.9.6), [installation](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/README.md#L168), [coverage](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/README.md#L281) |
| Sustainable commercial product | **Unproven** | No customer economics, support-cost or willingness-to-pay evidence. Validate demand separately from technical enthusiasm. |

## 4. Prioritized product opportunities

**P0** blocks a credible readiness claim or the intended task for affected users; **P1** materially improves a core workflow; **P2** expands reach or polish after the core works. These are product priorities, not security severity ratings.

### P01 — Preserve critical state at every supported terminal size

**P0 · Observed.** UTC is absent from the header in all four views at 80×24 and 100×30, and present at 132×40 in the tested idle fixture. At 80×24, DX Hunter headings collapse into “G S R R L P”; the footer ends at “q:Qui”; a seven-row empty TX-placement panel occupies much of the display. [80×24 capture](2026-09-07-product-evidence/Operate-80x24.txt), [100×30 capture](2026-09-07-product-evidence/Operate-100x30.txt), [output](2026-09-07-product-evidence/probe-output.txt)

The header appends the clock after other spans and uses saturating padding, which cannot make an overfull row fit. Several panels retain fixed column or height constraints. The minimum rendering threshold of 80×20 is therefore not an assurance of usable operation. [Clock layout](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-tui/src/ui/mod.rs#L842), [minimum dimensions](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-tui/src/ui/mod.rs#L34), [DX columns](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-tui/src/ui/dx_hunter.rs#L83)

**Change:** reserve width for UTC/slot timing, actual TX state and the primary action first. Collapse idle placement and empty secondary panels. Use compact, normal and wide layouts with intentional column choices; show the selected station's full detail in a stable area.

**Acceptance:** operate every view at 80×24, 100×30 and 132×40, including active TX, multiple QSOs, errors and long identifiers. Critical state and stop instructions remain readable; actionable rows have intelligible identities. Either qualify 80×20 separately or state the real minimum. The current 12-state observation is not exhaustive coverage.

### P02 — Make station readiness and recovery persistent

**P0 · Source and judgment.** The application has health reporting and status messages, but each decode overwrites the shared status message. An important message using that surface can disappear as ordinary band traffic arrives. This does not mean every fault is lost: component indicators and logs also exist. [Status replacement](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-tui/src/app.rs#L2166), [health monitoring](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/health.rs#L331)

**Change:** separate durable blockers, acknowledged warnings and transient activity. Present the chosen rig, actual audio devices, clock condition, transmit policy, control presence and stale dependencies. Distinguish setting saved, command sent, and rig reports applied. Keep QSO continuity during reconnect explicit.

**Acceptance:** operators can answer “Can I transmit?”, “What blocks it?”, “What happens next?” and “Did recovery work?” without a debug log. Exercise disconnect, rejected commands, invalid config, restart and stale data; decodes must not erase unresolved blockers.

### P03 — Make configuration recipes executable and behavior truthful

**P0 · Reproduced failures and source/documentation mismatch.**

| Input tested as a complete TOML configuration | Actual result |
|---|---|
| Minimal station identity | Parses |
| [README's minimal autonomous toggle](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/README.md#L107) | Fails: missing slot_parity |
| Minimal UDP toggle | Fails: missing destination |
| GUIDE same-host UDP recipe, including destination | Fails: missing multicast_interface |
| GUIDE cross-machine recipe, including interface and TTL | Fails: missing instance_id |

The last two inputs reproduce the [guide recipes](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/docs/GUIDE.md#L226). They work as editing hints only if omitted required members are already present in a larger configuration. The parser does not supply the struct's Rust defaults for those omitted members. [UDP struct](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-config/src/network.rs#L237), [probe](2026-09-07-product-evidence/README.md)

The serialized default configuration is **962 lines** at this baseline, not 962 required user decisions. Setup and doctor already exist; finish their path into a small, validated, explainable configuration. [Setup](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/main.rs#L1088), [doctor](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/doctor.rs#L37)

The loader warns and records errors for malformed optional sources, then continues; it does not silently ignore them. CLI validation validates the resulting merged configuration. “My intended file parsed” and “the effective fallback configuration validates” should be visibly different outcomes. Full CLI behavior was not separately reproduced in this product pass. [Loader](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-config/src/loader.rs#L254), [validation](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/main.rs#L766)

The guide promises most file edits apply within a second, while the coordinator documents that no general reload-apply task is wired. Recent live rig switching is a separate implemented command path. [Promise](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/docs/CONFIG.md#L512), [implementation](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/health.rs#L347)

**Change:** define supported minimal inputs; parse runnable recipes in CI; show effective values and sources; distinguish changed-on-disk from applied-in-session. Prefer explicit apply now/after QSO/restart-required results. Keep advanced settings outside the first-run path.

**Acceptance:** both UDP recipes work on a clean configuration; documented minimal examples parse; validating a selected malformed file cannot produce an unqualified success; changes show their application state.

### P04 — Treat “needed” as a claim with provenance

**P0 · Source.** Empty needed-DXCC data invokes an all-except-excluded fallback; with no exclusions it returns true. Empty needed-grid data returns false. A successful response with no needs and an unavailable dataset lack distinct representations in these paths. Local history and entity handling do exist; this is not an absence of all offline functionality. [DXCC fallback](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/priority_evaluator.rs#L455), [grid fallback](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/priority_evaluator.rs#L562), [history](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/qso.rs#L2527)

cqdx startup fetches populate caches; failure leaves degraded operation, and the spot poller starts only on startup success in the reviewed path. No live endpoint availability was tested. [Startup data](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/cqdx_bridge.rs#L104), [startup handling](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/mod.rs#L1982)

**Change:** model availability, completeness, age and provenance separately from empty results. Distinguish “not worked locally,” “needed for selected award,” “confirmed,” and “unknown,” with band/mode scope. Retry startup-dependent enrichment with visible recovery. Support imported/local history offline.

**Acceptance:** distinguish no account, outage, stale data, incomplete history, genuinely empty needs, and positive needs. An empty successful response must not mean every station is needed. UI and decisions consume the same resolved facts.

### P05 — Explain recommendations and automation in operator language

**P1 · Existing capability with an experience gap.** The pitch promises needed DXCC/grid, POTA/SOTA and rarity scoring. The screenshot emphasizes numeric scores and terse markers; the POTA/SOTA signal is a callsign-suffix heuristic, which misses bare-call activators and does not establish an activation. [Pitch](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/README.md#L48), [screenshot](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/assets/screenshot-priority.png), [limitation](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-qso/src/priority.rs#L222)

**Change:** give a short reason: “new entity on 20 m in your local log; heard this slot; moderate signal.” Show disqualifiers, retry history and uncertainty. Distinguish a portable candidate from a verified activation. Explain why the operator is waiting/listening and how to override. Put raw scores in expandable detail.

**Acceptance:** selected identity remains stable during resorting; the explanation matches decision inputs; missing metadata cannot produce definitive award/activation badges. Preserve existing callsign-pinned selection and shared TX-placement behavior. [Selection pinning](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-tui/src/app.rs#L2158)

### P06 — Close the contact-to-logbook workflow

**P0 for a station replacement · Source.** ADIF import exists in the QSO library and history initialization exists in the coordinator, but CLI export has no corresponding import command. This is a missing operator workflow, not proof that code cannot ingest ADIF. [Import](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-qso/src/lib.rs#L459), [CLI](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/main.rs#L163), [history](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/qso.rs#L2527)

Per-contact upload paths launch tasks and report results to logs. Reviewed dispatch does not persist an upload outbox or a per-destination recovery workflow. Network failure can leave a locally recorded contact needing manual reconciliation. This does not assert local QSO loss. [Dispatch](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/qso.rs#L7287)

LoTW is genuinely invoked, with batch exit arguments and a managed child process. Repeating “argv-only stub” would be wrong. An invoked uploader is different from durable, operator-visible delivery. [Arguments](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-dx/src/qso_upload.rs#L539), [invocation](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-dx/src/qso_upload.rs#L606)

**Change:** provide import preview, duplicate policy, repair/export and backup verification; add a durable outbox with destinations, attempts, retry/backoff and receipts. Distinguish local save, remote acceptance and later award confirmation. Make software attribution in operator comments optional: the writer appends it when space permits while preserving existing notes. [Attribution](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-qso/src/adif.rs#L253)

**Acceptance:** restart during upload, reject credentials, lose network, deliver duplicates, and restore a backup. Every accepted local contact remains accounted for; retries reconcile; status reflects destination acknowledgement.

### P07 — Certify interoperability at the receiving end

**P0 for affected integration claims · Confirmed source/spec mismatch.** PSKReporter's packet builder labels local reporter data with enterprise IDs 1/3/4/5; published receiver fields are 2/4/8/9. It encodes observed frequency under ID 2; the published frequency ID is 5. Flush logs “Successfully uploaded” after the local UDP send completes. [Builder](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-dx/src/pskreporter.rs#L674), [frequency](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-dx/src/pskreporter.rs#L714), [data](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-dx/src/pskreporter.rs#L733), [send result](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-dx/src/pskreporter.rs#L798), [official protocol](https://pskreporter.info/pskdev.html)

This is a source-supported mismatch, not an observed production rejection. Seeing one's transmitted station spotted by another receiver does not validate Pancetta's own reception-report uploader.

**Change:** qualify protocols with independent fixtures and receiving-side evidence. Use “sent” for unacknowledged UDP delivery. Cover WSJT-X UDP consumers, PSKReporter, each log destination and specialized Fox/Hound exchanges separately. A socket write or mocked response does not establish an integration's completeness.

**Acceptance:** an independent decoder interprets expected fields and frequencies; an authorized field trial confirms end-to-end acceptance without publishing private station data. No external submission was made here.

### P08 — Benchmark the product actually operated

**P0 for performance positioning · Published measurement and source-scope mismatch.** FEATURES and CLI help claim over 95% decoding at −20 dB. The comparison document's historical calibrated native-decoder table reports 3/50 there and cautions that the full curve was not rerun against final defaults. These are not a coherent current performance contract. [FEATURES](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/FEATURES.md#L5), [CLI](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/main.rs#L49), [historical curve](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/docs/decoder-comparison.md#L125)

Production FT8 merges ft8_lib output with the native AP-enhanced decoder. Its native budget is clamped; the separate C path is outside that budget. Neither the historical native-only sensitivity gap nor “Max means unlimited” describes the whole runtime. [Composition](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/ft8.rs#L1), [budget scope](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/ft8.rs#L43), [application](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/ft8.rs#L1539), [Max claim](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/FEATURES.md#L9)

**Change:** repair the failing research fixture, then benchmark the production path on frozen holdout recordings and the target MiniPC. Publish recall, false positives, latency distribution, late/missed windows and CPU/memory at the operating budget. Include FT4 separately. Label native research curves accurately.

**Acceptance:** a versioned artifact records commit, flags, hardware, corpus hashes, settings and reference version. Public claims draw from that artifact. Speed/recall claims account for each other and false positives. This report asserts no current production sensitivity number.

### P09 — Finish the first-session journey

**P1 · Synthesis from existing entry points.** Setup, doctor, audio listing and a rig picker already exist. Live switching, bookmarks, loaded-config-path persistence and reconnect feedback have recently improved the foundation. Rebuilding them would waste useful work. [Setup](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/main.rs#L1088), [audio](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/main.rs#L745), [rig reconnect](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/tui_relay.rs#L2392), [bookmarks](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/tui_relay.rs#L2484)

**Change:** connect the steps: station profile → radio input/output → receive audio and clock verification → first decode → control/transmit conditions → deliberate first contact → saved result. Link doctor results to corrections and retest in place. Start with the owner's FTdx10/Windows combination.

**Acceptance:** a new operator on qualified hardware reaches first decode from the release archive without source edits or developer assistance. Record time and recovery steps. Avoid first-QSO guarantees that depend on propagation and other stations.

### P10 — Make help and visual hierarchy scale with the task

**P1 · Source and visual inspection.** Help contains 41 entries; its overlay caps height to available space and has no scrolling mechanism in the renderer. A shared keybinding table does not ensure all bindings are reachable at normal terminal heights. [Help renderer](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-tui/src/tui_runner.rs#L2667), [entry count](2026-09-07-product-evidence/probe-output.txt)

The replay screenshots are dense: repeated borders, prominent allocator detail, terse abbreviations and competing accents. Waterfall marker labels compete with the frequency scale. These are design judgments, not measured user failure rates. [Operate](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/assets/screenshot-operate.png), [waterfall](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/assets/screenshot-waterfall.png)

**Change:** establish three emphasis levels: active task, relevant context, diagnostics. Standardize status words, spacing, alignment, empty states and selection appearance. Add searchable or scrollable, task-grouped help. Test high-contrast/limited-color presentation and ASCII compatibility where needed. Validate large fonts, color-vision variation and assistive workflows with users; this review did not establish accessibility conformance.

**Acceptance:** every binding is discoverable at supported heights, state is understandable without color alone, and the active contact and primary action dominate. Preserve the four useful activity views and shared keybinding table.

### P11 — Give remote and headless operation an explicit contract

**P0 before broader remote TX · Source and qualification gap.** No recorded console interaction means no operator presence for autonomous initiation. “Runs over SSH,” “runs headless,” “continues an exchange,” and “accepts remotely supervised initiation” are distinct experiences. [Presence](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/mod.rs#L561), [gate](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/autonomous.rs#L615)

**Change:** name supported profiles and show control source, privileges, presence state, next permitted action and disconnect behavior. Specify whether SSH disconnect loses a display or recognized control. Qualify foreground, service, restart and reconnect paths together. Remote expansion also depends on closure of separate private security findings.

**Acceptance:** repeat a disconnect/restart/control-transfer matrix on the qualified station, recording command/stop timing and resulting rig state. Do not solve a usability problem by weakening transmit restrictions.

### P12 — Give releases a supportable installation and upgrade story

**P1 · Release and documentation.** v0.9.6 has four binary archives and checksums. macOS instructions explicitly describe missing notarization and quarantine removal. Documentation acknowledges one exercised radio and no Raspberry Pi hardware validation. [Release](https://github.com/HagaleTechnologies/pancetta/releases/tag/v0.9.6), [installation](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/README.md#L168), [coverage](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/README.md#L281), [release workflow](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/.github/workflows/release.yml)

**Change:** distinguish build-tested, replay-tested and radio-qualified platforms. Provide complete Windows radio-driver, audio and rigctld checks; improve macOS installation trust; qualify Linux/ARM on actual hardware. Add backup-before-upgrade, migration reporting and supported rollback. Signing/notarization may require separate account/cost authorization.

**Acceptance:** each advertised supported configuration has fresh-machine installation and upgrade/rollback records. Label unqualified platforms accurately instead of blocking every release on broad hardware coverage.

### P13 — Demonstrate completed value

**P1 · Documentation and positioning.** Current assets are correctly labeled replay; README explains that receive-only traffic leaves the QSO panel idle. This is honest, but the most valuable workflow remains invisible to prospective operators. [Explanation](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/README.md#L69)

**Change:** add a labeled deterministic two-station simulation showing selection, exchange progression, saved log and upload state; separately provide a consented real-station walkthrough. Retain a receive-only demo for first exploration. Explain ordinary use before research controls and internal counters.

**Acceptance:** a short demo makes clear what the operator chose, what the application did, how control was retained, and where the result was saved. Never relabel fixtures as on-air evidence.

### P14 — Maintain one authoritative capability matrix

**P1 · Revalidated drift.** Docs call Hamlib a stub, describe LoTW as scaffolded, promise general hot reload and claim decoder performance unsupported by the cited current measurement scope. Some caveats are too pessimistic; some promises too strong. [Limitations](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/README.md#L277), [reload](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/docs/CONFIG.md#L512), [performance](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/FEATURES.md#L5)

**Change:** track experimental, implemented, integration-tested and radio-qualified capabilities, with release/environment. Generate help/examples where possible and validate runnable recipes. Write release notes around user-visible behavior and limitations.

**Acceptance:** each headline promise links to implementation and appropriate qualification evidence. Historical defects leave the active backlog after fixes land.

### P15 — Protect focus and improve integration boundaries as needed

**P1 · Architecture-informed recommendation.** Large coordinator modules join UI commands, rig state, persistence and service side effects. They are natural pressure points for changes spanning workflows. Separate crates and message boundaries already exist; this review does not justify wholesale reorganization. [QSO integration](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/qso.rs), [TUI integration](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/tui_relay.rs)

**Change:** extract boundaries when prioritized workflows require them: a shared decision/explanation record, upload outbox, readiness snapshot and configuration-apply result. Give each a small behavioral contract. Propose cross-repository interface changes in dispensa before changing consumers.

At assessment time, [PR #356](https://github.com/HagaleTechnologies/pancetta/pull/356) covers US state/territory display and [PR #350](https://github.com/HagaleTechnologies/pancetta/pull/350) covers adaptive offsets/manual nudging. Neither is counted as shipped here. Recheck before opening duplicate work.

**Acceptance:** workflow changes have a bounded review surface and observable end-to-end behavior. A feature enters the roadmap with a user task, exit test and explicit decision about displaced work.

### P16 — Validate demand before choosing a business model

**P2 · Proposal.** An excellent personal/open-source station is a valid outcome without a paid offering. Commercialization adds support, distribution, availability and acquisition requirements that code quality cannot answer.

Interview operators about their actual station, last setup failure, missed/incorrect contact, log reconciliation and remote-access needs. Observe tasks instead of relying on feature-preference polls. Investigate managed remote access, backup or support only if there is recurring willingness to pay. Preserve useful local operation and log export independently.

**Acceptance:** before commercial investment, demonstrate repeated use, a defined paying segment, actual willingness to pay and plausible support economics. Include hardware-specific troubleshooting and service costs. No revenue forecast is supported by this review.

## 5. What an excellent session would feel like

This is a proposed experience, not completed functionality.

1. **Open a known station.** Name the rig and actual audio devices, show clock condition, and remain receive-only until the operator chooses a permitted transmit profile. Reuse verified configuration.
2. **Choose a goal.** “Work entities missing from my 20 m log” names its history source and freshness. Missing account data produces a local-only/unknown state, not an authoritative-looking award roster.
3. **Choose or approve a target.** Selection survives new decodes. A concise reason explains value; optional detail shows score and uncertainty.
4. **See the next action.** Actual transmission is separate from queued message, slot and waiting reason. Control actions show pending/applied/rejected results.
5. **Finish with an accounted-for contact.** Local save is durable. Destinations show queued, accepted, failed/retryable, or awaiting confirmation. Leaving and returning preserves this state.

Conceptual compact layout:

    N0CALL · 20 m · FT8        RX ONLY        14:22:07 UTC · slot +07
    READY: rig connected · audio verified · clock checked
    Goal: new entities on 20 m · local history updated today
    Selected: [call] · new locally on this band · heard this slot
    Contact: waiting for report        Next: send R-12 on our slot
    Now: receiving                     Queued: [message / none]
    Log: saved locally                 Uploads: 1 accepted, 1 queued
    [primary action]   [details]   [stop]   [? help]

This illustrates information priority, not a pixel specification, binding proposal or performance measurement. Wider layouts add waterfall, callers and placement detail; smaller layouts preserve operational questions first.

The aesthetic target is a calm instrument panel: clear hierarchy, consistent language, restrained accents and stable geometry. Retain terminal speed and density where they help the task.

## 6. Delivery sequence and exit criteria

These are dependency-based phases, not calendar or staffing estimates. Limit concurrent feature work enough to complete each user journey.

| Phase | Work | Exit evidence |
|---|---|---|
| **A — Establish truthful readiness** | P01–P04, P07–P08, P11's contract; repair research fixture; reconcile claims | Compact views preserve critical state; recipes parse; unknown needs remain unknown; protocol fixture conforms; full-suite failure fixed or explicitly isolated by supported-suite policy; private remote readiness addressed before expanded use |
| **B — Complete the owner's FT8 station** | P05–P06, P09–P10; qualified Windows/FTdx10 profile; use landed rig/bookmark work | Repeated owner sessions complete select→exchange→save→reconcile; reconnect/restart/upload failures recover visibly; production decoder benchmark on target hardware |
| **C — Prove another operator can adopt it** | P12–P14; migration, distribution, demo and support | Small external cohort installs from an archive, imports history, understands station state and returns for subsequent sessions; actual support matrix published |
| **D — Expand selectively** | Qualified companion, additional radios/modes, richer activation/award workflows, optional services | A new segment's complete task demonstrated; support cost accepted; cross-repository contracts agreed first |

Do not defer core decode quality until after visual polish. Also do not indefinitely defer configuration, logging and clarity while pursuing another fraction of a decibel. Benchmark and complete workflows alongside each other, with explicit exit criteria.

## 7. Proposed product scorecard

Numerical thresholds are **proposed acceptance targets**, not current measurements or industry benchmarks. Adjust after observed sessions.

| Outcome | Initial target or measurement |
|---|---|
| First useful receive | At least 4 of 5 recruited operators reach first decode on qualified hardware within 15 minutes after required drivers/hardware are ready; include failed attempts |
| Comprehension | Operators identify actual TX state, next action and blockers in scripted scenarios without developer explanation |
| Compact operation | All four views at all three sizes retain critical state; include active/error/long-text cases |
| Configuration trust | Every runnable recipe parses; invalid selected input cannot produce unqualified validation success |
| Contact accounting | No accepted local QSO unaccounted for after specified restart/crash/reconciliation tests; retain evidence |
| Upload recovery | Every failure has a visible recovery path; duplicate delivery remains reconciled |
| Decode quality | Publish whole-product recall, false positives and latency on frozen holdout data and target hardware; set improvements from that baseline |
| Timeliness | Record p50/p95/p99 decode-to-decision and command-to-observed-rig-state latency; count late slots |
| Adoption | Observe at least five external operators over multiple sessions; track return use and abandonment reasons with consent |
| Support | Count interventions, recurring failure categories and recovery time by supported platform |
| Product truth | Each release's capability matrix, examples and executable agree |

QSOs/hour alone can reward irrelevant contacts and conceal failed uploads or confusion. Prefer **completed, durably logged contacts matching the operator's selected goal per attended session**, alongside control, decode-quality and recovery measures. Award confirmation is a later external outcome.

## 8. Corrections to older assumptions

| Claim | Current disposition |
|---|---|
| Rig changes only save config and always require restart | Outdated as a blanket claim. Current handling requests live reconnect and reports its result; bookmarks persist. [Relay](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/tui_relay.rs#L2392) |
| LoTW only builds argv | Incorrect. It invokes TQSL with batch exit handling. Live service delivery was not tested here. [Invocation](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-dx/src/qso_upload.rs#L606) |
| No local log/history capability | Incorrect. Library import and history initialization exist; a complete operator migration/reconciliation workflow remains the gap. [Import](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-qso/src/lib.rs#L459), [history](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/qso.rs#L2527) |
| Historical native-decoder gap measures today's whole app | Unsupported. Production merges two decoders; the curve carries a final-defaults caveat. [Composition](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta/src/coordinator/ft8.rs#L1), [caveat](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/docs/decoder-comparison.md#L145) |
| No prebuilt releases | Incorrect. v0.9.6 has four platform archives; hardware qualification is narrower. [Release](https://github.com/HagaleTechnologies/pancetta/releases/tag/v0.9.6) |
| Thousands of passing tests mean full workspace is green | Incorrect in this environment. Counts and exclusions must accompany the claim; §9. |

## 9. Validation and unresolved questions

Production code was not changed. The capture helper ran as a temporary Cargo example and is retained as documentation evidence outside compiled source.

| Check | Actual result |
|---|---|
| cargo test --workspace --features transmit | **FAIL, exit 101.** Research test novel_classification_tests::curated_tier_classification_is_report_only_and_correct failed reading a baseline cache missing freq_hz at [eval.rs:3090](https://github.com/HagaleTechnologies/pancetta/blob/17ffe360fbc1f1f2afac6615aad76d6df39f4c32/pancetta-research/src/bin/eval.rs#L3090). |
| cargo test --workspace --exclude pancetta-research --features transmit | **3,527 passed, 0 failed, 6 ignored**, across 72 result groups in the earlier security-review worktree at this source baseline. Includes eight temporary security regression tests; this is not an untouched-checkout count. |
| cargo test -p pancetta-hamlib --lib -- --test-threads=1 | **27 passed, 0 failed**, same-baseline session result. |
| Product capture/config helper | **Exit 0.** Four views × three sizes plus two isolated QSO-detail buffers rendered without panic. One parser input succeeded; four failed as listed in P03. Default serialization: 962 lines; help: 41 entries. |

Suite results were reused from the security pass, not rerun for this documentation-only change. Underlying security logs stay private. [Public validation summary and product reproduction](2026-09-07-product-evidence/README.md)

Questions most likely to change the roadmap:

- Is success owner-station use, wider open-source adoption, or a supported commercial offering?
- Which contacts matter: casual FT8, DXCC/band fills, activations, contests, or decoder experiments?
- Must Pancetta replace the existing logger, or is reliable integration preferable?
- Which exact radio/OS combinations receive a support commitment?
- What should headless operation permit, and what observable control-presence mechanism meets that contract?
- What decode-quality tradeoff, if any, would operators accept for the integrated workflow?
- Will users choose Pancetta after a week of operation without a developer present?

These are validation tasks, not reasons to postpone concrete fixes. Immediate work is clear: readable state, executable setup, trustworthy needs, verified interoperability, durable logging recovery and honest production measurements.
