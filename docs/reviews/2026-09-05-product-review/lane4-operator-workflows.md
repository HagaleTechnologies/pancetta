# Lane 4 — Fitness for purpose & utility: does Pancetta do the operator's job end to end?

Repo `/Users/thagale/Code/pancetta` @ `c90cb34` (main), read-only. Crate-relative cites abbreviated (`qso_manager.rs` = `pancetta-qso/src/qso_manager.rs`; `tx.rs`, `mod.rs`, `hamlib.rs`, `tui_relay.rs`, `autonomous.rs` under `pancetta/src/coordinator/` unless prefixed; `priority.rs`/`autonomous.rs` in `pancetta-qso` are prefixed `q/`). **[V]** = re-verified by me against source after a leaf audit reported it. Five leaf audits (logging, DX/priority, hound/fox/contest/POTA, headless/safety/hardware, interop/modes/decoder) reported; the TUI/config leaf died to a rate limit and was absorbed with targeted greps. Items Lane 5 already owns (slot-tick drift after sleep, config hot-reload unwired, dead audio output not inhibiting PTT, DX-cluster disconnect spin, no upload outbox PAN-57, QRZ password in debug logs) are referenced, not re-derived.

## 1. Executive summary

1. **The core loop is real and good**: decode → tiered priority → parity-correct multi-stream TX → state machine → `qsos.adi`+`qso.db` → per-QSO push to ClubLog/QRZ/cqdx. A supervised evening on 20 m works, and is safer than WSJT-X (drop-stale-TX, PttGuard, 14 s PTT watchdog, emergency stop).
2. **Everything outside that loop is thinner than the docs say.** LoTW/eQSL are argv-only scaffolds; PSKReporter's IPFIX element IDs are wrong **[V]**; Hound implements the *inverse* of the WSJT-X convention **[V]**; contest mode is ~10 % of its spec with no operator entry point; POTA is a callsign-suffix heuristic; headless is receive-only by construction **[V]**.
3. **The "20-year log" problem is unsolved**: no ADIF import (`async_logger.rs:554` uncalled); worked-before seeds one band with a `"20m"` fallback (`qso.rs:2150-2153`); the persistent dupe check is dead in production **[V]** (`qso.rs:2031` builds `QsoManager::new`, never `with_database`) and the in-memory one compares audio *offset* ±50 Hz for ≤1 h.
4. **cqdx.io is a single point of failure for DX utility**: down ⇒ no ATNO, flat rarity, "needed" = "prefix not in home/ADIF set" (swallows KH6/KL7/KP4 for a W5 station); one startup 401/timeout disables it for the session (`mod.rs:1884-1909`). Yet cqdx *spots* never reach the DX Hunter (`mod.rs:1887-1893`, `tui_tx: None`).
5. **Zero alerting**: no bell, push, webhook, or INFO line on an ATNO decode; `ClusterAlertConfig`/`SoundFeedbackConfig` have no consumers.
6. **Safety is strong in-process, weak at the process boundary**: no SIGTERM handler **[V]** (`main.rs:395`); rig loss doesn't inhibit TX; no band-edge gate before PTT; no durable local TX audit headless; `--metrics` exports nothing.
7. **Interop is the best-built non-core surface**: WSJT-X UDP emit/consume byte-correct with golden tests; GridTracker/Log4OM work for FT8. FT4 decodes never emitted; Status leaves DX/TxMessage blank; no "WSJT-X as modem".
8. **Hardware is one rig deep**: 14-name model whitelist (`hamlib.rs:1003-1021`), rig mode never set, CAT PTT only, no DTR/RTS/VOX/CM108, no SDR input, no multi-rig.
9. **Decoder is honest and mid-pack**: +2.1 dB behind jt9 at 50 % FT8, +3.95 dB / 78 % cap FT4, 0.1 % FP; `Max` isn't unlimited; Pi-class default is floor-only decode.
10. **Fit-for-purpose (1–10): Casual supervised FT8 7 · DXer 4 · Contester 2 · POTA activator 2 · Headless/remote 3 · Tinkerer 4.**

## 2. Session walkthroughs

### (1) Casual evening on 20 m (supervised, TUI)
Automatic: Space on a CQ row runs the whole exchange with parity latched to the DX and an auto-picked offset; `c`/`s` run CQ; callers answered; QSO auto-logged to `~/.pancetta/qsos.adi` + `qso.db` (`qso.rs:2074-2077`); ClubLog/QRZ/cqdx pushed per QSO when configured; `h`/`Shift+Q` stop TX.
Friction:
- **No existing-log import** → nothing is "worked before" until re-worked in Pancetta (`async_logger.rs:554` `import_adif` has no caller). B4 and tier-2/ATNO are Pancetta-history only.
- **Dupe protection in-memory, 1 h, audio-offset-keyed** **[V]**: `check_duplicate` compares `metadata.frequency` (offset) within 50 Hz (`qso_manager.rs:4507`), autonomous calls only (`:1458`), completed QSOs evicted after 1 h (`:5269`); DB branch (`:4522`) unreachable. Documented "24 h / same RF" is false; `TROUBLESHOOTING.md:26-33` describes the dead path.
- **Worked-before seed is one band**, `"20m"` fallback when the dial isn't read yet (`qso.rs:2150-2153`), never re-seeded on QSY (`:2239,2301`).
- **Upload failures invisible**: fire-and-forget spawn, `warn!` to file (`qso.rs:6469-6580`); "QSO logged" toast fires *before* upload (`:2861-2885`); no retry/outbox (Lane 5 / PAN-57).
- **ADIF fidelity**: FT4 logged `MODE=FT4` without SUBMODE (`adif.rs:511-512`; LoTW/ADIF want `MODE=MFSK SUBMODE=FT4`); no DXCC/CQZ/ITUZ/COUNTRY/STATE (`:521-525`); `QSL_SENT/RCVD` hardcoded N (`:531-532`); HashMap field order (`:1082-1090`); forced attribution string in every COMMENT, no toggle (`:258-263,487`); `TIME_ON` = CQ-start (`qso_manager.rs:990-1008`); no CAT ⇒ `FREQ`=offset, `BAND=0MHZ` (`:2876-2880`); `pancetta export` reads `qso.db` not the ADIF, callsign-substring filter only, omits `ADIF_VER`/`PROGRAMID` (`main.rs:649-711`, `adif.rs:113-127,922-925`).
- **LoTW/eQSL don't actually work**: tqsl argv assembled, tests assert "tqsl is never executed" (`qso_upload.rs:812`), no `-p` passphrase, exit 8/9 (dupes) treated as failure (`:614-625`), `-a all` re-signs dupes on every retry; eQSL posts raw `text/plain` rather than the `ADIFData` form field (`:426-431`, self-marked OPERATOR-CONFIRM). `lotw.rs` confirmation download is unwired (`lib.rs:53` only) and its upload posts unsigned ADIF. **No confirmation status anywhere**; `confirmed_qsos` counts a `$.confirmed` key `QsoMetadata` doesn't have (`async_database.rs:627-628`).
- TUI: no logbook browser/edit/delete/notes (`Shift+R` is a read-only 50-row outcome list); no CQ-only/needed-only band-activity filter; no callsign lookup key; no manual log entry for a QSO that didn't close cleanly (grep `EditQso|DeleteQso|Notes|CqOnly|Lookup` in `pancetta-tui/src/{keymap,app}.rs` = 0). Band Activity does carry `worked_before/needed/atno/band_needed` flags (`ui/band_activity.rs:277,507-510`).

### (2) Serious DX chase
- **Scorer with cqdx up**: 5-tier lexicographic (`q/priority.rs:423-444`); per-band DXCC from cqdx band-fills + local `qso.db`; ATNO only from cqdx's `atno` flag (`cqdx_bridge.rs:34-46`); needed matched by prefix `starts_with` on the canonical prefix — JH/JR/7K, M/2E, W/N alternates miss.
- **Scorer with cqdx down**: `is_atno` false; rarity flat 0.5; "needed" degrades to "prefix ∉ {home, K/W/N/AA–AK if DXCC 291, every CALL prefix in qsos.adi}" (`priority_evaluator.rs:66-95`, wired `mod.rs:2005-2016`) — KH6/KL7/KP4/KH2/KH0/KH8/KP2 become "home". Startup failure ⇒ no poller, no retry, whole session (`mod.rs:1884-1909`); running poller does 3 strikes → 5 min backoff (`cqdx_bridge.rs:165-170,240-247`).
- **Single-scorer invariant violated three ways**: DX Hunter `score_tiered` (`tui_relay.rs:171-174,263-268`); TUI re-scores network rows with `DxStationLookupAdapter` (`app.rs:71-116`, no tier-4); autonomous pounce uses the *weighted* `evaluate_cq` gated by `min_dx_score` 0.3 / `min_multi_slot_score` 0.7 (`q/autonomous.rs:1539,2094-2099`; `q/priority.rs:528-532`). You cannot say "pounce ATNO only".
- **Per-mode needed: none** (`mod.rs:1889-1893` `mode: None`; SQL has no mode, `async_database.rs:867-872`).
- **Alerting: none** — no `\x07`, ntfy, Pushover, webhook, email, or INFO log; DX Hunter colouring only (`ui/dx_hunter.rs:150-235`). Headless the only hook is WSJT-X UDP → JTAlert.
- **Watchlist**: 150 s auto-memory of tier≥2 CQs with a `◇` glyph (`q/watchlist.rs`; TTL pinned `autonomous.rs:865`; `remove()` never called); not operator-editable; no alert/boost.
- **Cluster**: telnet works (`pancetta-dx/src/cluster.rs:209-227`, default `dxc.nc7j.com:23`, default off) but mode/band are dropped from `DxMessage::Spot` (`message_bus.rs:871-878`), `filtering` config ignored (`dx_cluster.rs:70`), and **Space on a network spot calls at 1500 Hz on the current dial with no QSY** (`app.rs:3106-3113`, `tui_relay.rs:1115-1125`). Autonomous never sees spots (`q/autonomous.rs:1515-1517`). Disconnect spin: Lane 5.
- **Band hopping**: default off; hop after 20 zero-decode cycles, round-robin ignoring `priority` (`q/autonomous.rs:585-660,641-645`); no timer/UTC/grey-line/propagation/needed-aware trigger. Band-plan region = soft once-per-session warning (`band.rs:59-85`).
- **Hound** **[V]**: calls 300–900 Hz then QSYs *up* to 1000–2700 on report (`qso_manager.rs:39-45,1330-1334,3032-3046`; spec `2026-06-27-hound-mode-design.md:20-26`) — WSJT-X is Fox 300–900, Hounds ≥1000, Hound then *drops* to the Fox's freq. A real Fox never answers this Hound. QSY target is a hash of the Fox call, not its slot (`:3042`); sends `73` after RR73 (`q/exchange.rs:611-618`); ±100 Hz relevance gate (`qso_manager.rs:214,4463-4476`) drops Fox slots 3–5; DXpedition 0.1 frames have the wrong layout **[V]** (`message.rs:2266-2277` `c28 c28 R1 g14` vs real `c28 c28 h10 r5`) and are rejected by `is_plausible` (`:391-394`).
- **DXCC table**: static cty.dat derivative dropping `=exactcall`/slash sub-entities (VP8 → Falklands only `dxcc_table.rs:6167`; 3D2 → Fiji `:5081`); cqdx entity list loaded, unused (`cache.rs:77-85`).
- **PSKReporter upload** **[V]**: receiver template uses 0x8001/0x8003/0x8004/0x8005 for receiverCallsign/receiverLocator/decoderSoftware/antennaInformation (spec 0x8002/0x8004/0x8008/0x8009); reception template uses 0x8002 for frequency (spec 0x8005); no informationSource 0x800B (`pskreporter.rs:683-715`); mode hardcoded "FT8" (`psk_reporter.rs:147`); test asserts only version/length (`:966-1010`). README's "spots across NA+EU" are *others* spotting W5AU — not proof Pancetta's own uploads are accepted. Consume side (`query_spots :204`) unwired.
- Continuity gate: a never-seen call is refused for pounce only in its first heard window, accepted from the next (`q/callsign_continuity.rs:374-404`) — one lost cycle on a rare first sighting.

### (3) Running a pileup / Fox
Shift+X = `start_cq_manual(1500.0)` + raise always-answer cap to `fox.max_streams` (`qso.rs:4344-4346,1007-1017`; `fox.rs:33-63`). CQ at 1500 not 300–900; replies Tx=Rx on each caller's own offset (`qso_manager.rs:35-37,238-262`); standard i3=1 only, no `RR73;` 0.1 compound (spec `2026-06-28:45,78-79`); first-come queue; no 60 Hz slot grid; no "move to my freq". It is "CQ with N parallel answers", not a WSJT-X-compatible Fox — real Hounds calling ≥1000 Hz get answered but never see a Fox below 1000.

### (4) Contest (`2026-08-30-contest-mode-design.md`)
Landed: 261-line `pancetta-qso/src/contest/`, one `ExchangeShape::GridWithRAck` (`profile.rs:46-51`), one catalog entry `us-state-qso-party` (`catalog.rs:7-23`), `engage_contest_profile` (`qso_manager.rs:1274-1294`; doc "No operator UI calls this yet" `:1271`; zero callers ⇒ matcher `:2329-2358` unreachable), R+grid ack encoder (`encoder.rs:482-483,1170`), ADIF `CONTEST_ID/STX/SRX` writers (`adif.rs:736-748`) with serials always `None`. Spec-only: FD/VHF/RTTY shapes, "Enter this contest?" modal, free-form fallback, pattern inference, `[contest]` config, Cabrillo (zero hits outside `research/`), dupe sheet, rate, multipliers (PAN-50), serials. Wire formats non-interoperable: FD private layout (`message.rs:2414-2440`), RTTY RU encoder mislabels n3=5 ⇒ received as telemetry (`encoder.rs:842-880`), EU VHF i3=5 undecoded (`message.rs:1783-1785`), `/R` collapsed to `/P` (`:1808-1830`). PAN-52 open. Legacy `ContestExchangeConfig` is dead and unencodable (`q/exchange.rs:43-80,356-366`).

### (5) POTA/SOTA activation from a laptop
Detection = callsign suffix only (`q/priority.rs:235-249`, TODO `:232-234`); `CQ POTA` modifier parsed (`message.rs:589,899-900`) but not scored; no api.pota.app fetch/self-spot; **no `MY_SIG`/`MY_SIG_INFO`/`SIG_INFO`** anywhere (`adif.rs:719` writes only `MY_GRIDSQUARE`) — activation logs need post-editing; no quick-log/hunter-run tooling; no power/battery profile beyond `[decoder] effort="eco"`. Shift+M on a default build lands on a fake "FT2" (FT8 timing, ADIF `MODE:FT2`, toast only — `mod.rs:374-377,389`, `tui_relay.rs:1634-1645`).

### (6) Unattended headless on a Pi for a week
What it does **[V]**: `autonomous.enabled` default false (`pancetta-config/src/autonomous.rs:244`); when enabled, `operator_present_now` is false forever headless (`mod.rs:551-560`; sole writer `tui_runner.rs:939`; window 120 s hardcoded `:546`), and the presence gate drops every `qso_id: None` opening — CQ **and** pounce (`autonomous.rs:549-563`). Only TX path: answering someone who calls you directly (`qso.rs:928`). Realistic week ≈ 0 QSOs. Pancetta's own `fcc-part97-compliance.md` §2 says answering a human CQ is defensible under §97.221(c) — code is stricter than its own analysis, with no config knob. Respond-only is a sensible *floor*; as the *only* headless mode it makes the Pi story pointless.
What the operator sees: `~/.pancetta/logs` (14-day, lossy non-blocking appender `main.rs:1772`), 30 s/60 s stats lines. `--metrics` exports nothing (feature non-default `Cargo.toml:113`, zero `counter!/gauge!`). WS gateway read-only, localhost, default off (`remote_gateway/mod.rs:85-147`); station agent needs cqdx relay pairing. No web UI/MQTT/email/ntfy/daily summary.
Failure handling: rig disconnect detected (`hamlib.rs:1518-1519,1728-1742`) but **TX not inhibited** (`tx_hard_mute_reason` ignores `RigConnState`; `SetPtt` failure `error!` only `hamlib.rs:2189`, `tx.rs:1126-1157`); rigctld not respawned unless the hamlib task dies (`hamlib.rs:1196-1235`); audio loss reopened with backoff (`audio.rs:406-460`) but TX not inhibited (Lane 5 covers dead-output PTT); **no runtime clock monitor** (`doctor.rs:231-251` only; Lane 5 covers slot-tick drift); disk-full silent (`audit.rs:14-17`); `Ft8Transmitter` is `DegradeOnly` (`mod.rs:1372-1374`) ⇒ TX-worker panic kills TX until process restart; **no SIGTERM handler** (`main.rs:395`) ⇒ `systemctl stop` mid-TX abandons PTT and orphans rigctld, mitigated only by next start's PTT-off at connect (`hamlib.rs:1849-1857`).
Audit trail: `TxFrameLogged` → TUI only (`tx.rs:911-925`); `agent-audit.log` remote events only (`audit.rs:144-149`); local record = `info!` line without dial freq in a lossy log (`tx.rs:3901-3907`). **No durable per-transmission audit.**

### (7) Multi-band / multi-rig / SDR / devices / install
rigctld TCP only (no FFI); model must be one of 14 names (`hamlib.rs:1003-1021`) else no CAT (escape: external `RIGCTLD_HOST`, `:1136-1203`); rig mode never set (`set_mode` uncalled in `pancetta/src`; `tui_relay.rs:1592` is QSO-engine mode) so QSY/band-hop leaves DATA-U to the operator; PTT is CAT `T 1` only — `PttMethod` Serial/VOX/… is wizard cosmetics (`rig.rs:234-256`; consumed only `main.rs:1087,1404-1425`); no DTR/RTS/CM108/GPIO; no multi-rig; no RX-SDR+TX-rig; audio is cpal, mono forced (`stream.rs:43,573-583`), no TX gain, no Digirig/SignaLink handling; rigctld stderr → `/dev/null` (`hamlib.rs:1227-1228`). Two instances: `--config` exists but `~/.pancetta/{qso.db,qsos.adi,logs,tui_state.json}` hardcoded (`mod.rs:1929-1930`, `main.rs:1754-1757`), no lock. Packaging: systemd user unit + launchd plist (logs to `/tmp` vs its own comment); no Windows service unit; no Docker. `pancetta-hamlib/src/models.rs:308-310` has a conflicting model table (FTdx10=1045), unused.

### (8) Interoperability
WSJT-X UDP: emits Heartbeat/Status/Decode/Clear/QSOLogged/Close/LoggedADIF; honours Reply(4)/Replay(7)/HaltTx(8) fail-closed (`wsjtx_udp/mod.rs:348-525`); byte-exact golden tests vs WSJT-X 2.2.2 (`codec.rs:514-578`); default off, unicast `127.0.0.1:2237`, IPv4 multicast. Gaps: Decode FT8-only (`mod.rs:798-800`); Status blanks DXCall/Report/DXGrid/TxMessage, RxDF=0, `TRPeriod` sentinel, `SpecialOperationMode`=0 even in Hound (`:734-754`); inbound Decode(2) dropped (`codec.rs:504`) ⇒ no "WSJT-X as modem"; no N1MM UDP. Tinkerer: `WebApiConfig` (`network.rs:766-900`) and `WsprConfig` are dead config; no MQTT/webhook/socket; only the localhost WS feed with serde JSON and no in-repo schema. Config hot-reload: Lane 5.

### (9) Modes
FT8 prod; FT4 prod but +3.95 dB / 78 % cap with a diagnosed, unworked cause (FT8-tuned sync thresholds, `research/experiments/2026-07-07-ft4-tier.md:160-175`); FT2 non-standard, feature-gated, leaks as a fake label; JS8/MSK144/WSPR/Q65/FST4 enum labels only (`pancetta-core/src/types/mode.rs:37-43`). Recommendation: FT4 to parity first; MSK144/Q65 only with a VHF story; JS8 never; WSPR only as an RX propagation input after PSKReporter is fixed.

### (10) Decoder as a product feature
62.5 % of jt9's hard-200 truths, +2.10 dB at 50 % FT8, 0.1 % FP (`decoder-comparison.md:19-27,141-143`). AP1–4, cross-cycle, joint-pair, 3-round coherent multipass on by default (`decoder.rs:6905-6970,1879,1912,1797`); callsign priors research-only; neural OSD on but inert at `osd_depth=Some(0)` (`:1813-1814,14437`); W4.3 real multipass (+32 truths) measured, unshipped (`:282-295`). `Max` clamps to 2000/800 ms (`ft8.rs:1544-1555`) vs FEATURES.md:9 "unlimited"; Pi-class `Auto` = 1 ms floor-only (`effort.rs:56-67`, `tier_probe.rs:40-47`); budget checked only between candidates; ft8_lib primary runs unbudgeted. Operator sees a `DECODE` chip (`ui/mod.rs:700-714`) but no decode count vs reference; `benchmark-decode` is ft8_lib-only (`main.rs:895-898`); jt9 side-by-side lives in `pancetta-research` with a macOS-hardcoded path (`decoder.rs:1183`). No live path to consume WSJT-X decodes.

### (11) Safety / regulatory
Strong: parity latch; drop-stale (`tx.rs:3573`); PttGuard RAII (`tx.rs:2268-2295`); 14 s PTT watchdog (`hamlib.rs:1753,1806-1830`); hamlib-restart hard mute; remote arm fail-closed (`tx.rs:3227`); N0CALL refusal at QSO start (`qso_manager.rs:981,1106,1433,1712`). Weak: ID compliance "by construction" only (no check a frame carries the plain call; `/` free-text unchecked); no band-edge/privilege gate before PTT (`BandPlanConfig` display-only, `tui_relay.rs:2213`); SWR polled while keyed but display-only (`hamlib.rs:1692-1704`), no ALC/power readback or auto-reduce (`set_power_level` uncalled), amplitude fixed 0.5 (`tx.rs:1689`); no PTT readback (`get_ptt` unused). The operator must trust: rig mode/power set by hand, the log file not being lossy, nobody stopping the service mid-TX.

## 3. Ranked hit list  `[Pn][S/M/L] title — gap (evidence) — change — value`   (PAN-x = tracked; L5 = Lane 5 owns)

**P0**
1. [P0][S] **Hound convention inverted** — `qso_manager.rs:39-45,3032-3046`; spec `:20-26` — call ≥1000 Hz, on report move TX to the Fox's *answering* offset, suppress post-RR73 `73`, widen relevance gate to the Fox slot span — unusable → the #1 DXpedition tool.
2. [P0][M] **DXpedition 0.1 frames mis-parsed + rejected** — `message.rs:2266-2277,391-394` — implement `c28 c28 h10 r5`, accept in Hound QSOs — Fox `RR73;` multiplex completes QSOs.
3. [P0][S] **PSKReporter IPFIX element IDs wrong** — `pskreporter.rs:683-715`, `psk_reporter.rs:147` — spec IDs + informationSource + live mode; golden-packet test — your RX reports probably aren't landing.
4. [P0][S] **Persistent dupe check dead; in-memory keyed on audio offset** — `qso.rs:2031`; `qso_manager.rs:4507,5269` — `with_database`, compare on band, honour `time_window_hours` — no re-calling stations worked earlier today.
5. [P0][M] **No ADIF import of an existing log** — `async_logger.rs:554` uncalled — `pancetta import <adi>` → `qso.db` + worked/needed seeds — B4/tiers/ATNO meaningful on day one.
6. [P0][S] **No SIGTERM handler** — `main.rs:395` — `SignalKind::terminate` → shutdown path (PTT-off ×3, kill rigctld) — `systemctl stop` can't dead-key the rig.
7. [P0][S] **Rig loss doesn't inhibit TX** — `hamlib.rs:2189`, `tx.rs:1126-1157` — add `RigConnState` (and audio-alive, L5) to `tx_hard_mute_reason` — no keying a dead PTT.
8. [P0][M] **cqdx one-shot startup, no local ATNO/needed fallback** — `mod.rs:1884-1909`; `priority_evaluator.rs:66-95` — retry with backoff; derive ATNO/per-band-needed from `qso.db` + imported log; drop prefix-exclusion fallback — DX utility survives a cqdx outage.

**P1**
9. [P1][M] **Pounce ignores tiers** — `q/autonomous.rs:1539,2094-2099` — `pounce_min_tier` policy, weighted score as tiebreak — "only call what I need".
10. [P1][S] **cqdx spots never reach DX Hunter** — `mod.rs:1887-1893` — pass the relay sender; dedupe vs cluster.
11. [P1][M] **Alerting** — dead `ClusterAlertConfig`/`SoundFeedbackConfig` — bell + ntfy/webhook/Pushover on tier≥N decode/spot; INFO line headless.
12. [P1][S] **Upload outcome invisible** (L5/PAN-57 for outbox) — `qso.rs:6469-6580` — per-target status in Recent-QSOs + diagnostics.
13. [P1][S] **LoTW never exercised** — `qso_upload.rs:496-505,614-625,812` — `-p`, tqsl 8/9 = dupe-ok, fake-tqsl integration test, doctor check.
14. [P1][S] **eQSL POST shape** — `qso_upload.rs:426-431` — `ADIFData` form field; verify on a real account.
15. [P1][S] **FT4 ADIF `MODE=FT4`** — `adif.rs:511-512` — `MODE=MFSK SUBMODE=FT4` (also WSJT-X LoggedADIF).
16. [P1][S] **Worked-before seed one band / "20m" fallback** — `qso.rs:2150-2153,2239` — seed all bands; re-seed on QSY.
17. [P1][S] **Spot → call without QSY** — `app.rs:3106-3113`, `tui_relay.rs:1115-1125` — SetFrequency(+mode) then call at DX offset; confirm on band change.
18. [P1][S] **Cluster drops mode/band; filters ignored** — `message_bus.rs:871-878`, `dx_cluster.rs:70` — carry mode/band; wire `filtering`.
19. [P1][M] **Contest has no entry point** (PAN-49/50/52) — `qso_manager.rs:1271` — modal + `[contest]` config for the landed profile; then FD/RTTY RU with correct wire layouts (`message.rs:2414`, `encoder.rs:842`).
20. [P1][S] **POTA activation fields** — `adif.rs:719` — `[station] my_pota_ref/my_sota_ref` + runtime set; write `MY_SIG*`; score `CQ POTA`.
21. [P1][S] **No band-edge/privilege gate before PTT** — `tui_relay.rs:2213` display-only — hard block in `tx_hard_mute_reason`, license-class table, override.
22. [P1][S] **No durable local TX audit** — `tx.rs:911-925,3901-3907` — blocking append `{utc,dial,offset,mode,text,origin,qso_id}` to `~/.pancetta/tx-audit.log`.
23. [P1][S] **Headless presence gate blocks pounce** — `autonomous.rs:549-563` vs compliance doc §2 — `[autonomous] unattended_policy = respond_only|answer_cq|off`; window configurable; remote heartbeat counts as presence.
24. [P1][S] **Rig mode never set** — `set_mode` uncalled — set DATA-U/PKT-USB on startup/QSY/band-hop; `[rig] data_mode`.
25. [P1][M] **`Ft8Transmitter` DegradeOnly** — `mod.rs:1372-1374` — restartable with PTT-off teardown.
26. [P1][S] **`--metrics` exports nothing** — `Cargo.toml:113`, zero emitters — decode/TX/QSO counts, rig/audio/cqdx alive; default-on headless.
27. [P1][S] **No runtime clock monitor** (L5 for slot-tick drift) — `doctor.rs:231-251` — periodic SNTP/median-DT; warn + inhibit TX >1 s.
28. [P1][S] **Hamlib model whitelist** — `hamlib.rs:1003-1021` — accept numeric model IDs; delete `models.rs:308-310`.
29. [P1][M] **FT4 at 78 % cap** — `ft4-tier.md:160-175` — per-protocol sync thresholds; verify FT4 AP whitening (W1.2).
30. [P1][S] **FT4 never emitted over UDP** — `wsjtx_udp/mod.rs:798-800` — capture the `+` glyph, ship.

**P2**
31. [P2][S] Status(1) blanks DX/Report/TxMessage/RxDF/TRPeriod/SpecialOp (`mod.rs:734-754`) — fill from active QSO.
32. [P2][M] "WSJT-X as modem": consume Decode(2) (`codec.rs:504`), bind 2237.
33. [P2][S] ADIF hygiene: deterministic order (`adif.rs:1082-1090`), `PROGRAMVERSION`, `ADIF_VER` in export (`:113`), DXCC/CQZ/ITUZ/COUNTRY (`:521-525`), drop hardcoded `QSL_*=N`.
34. [P2][S] Attribution COMMENT toggle (`adif.rs:258-263,487`).
35. [P2][S] `TIME_ON` = first exchange (`qso_manager.rs:990-1008`).
36. [P2][S] `pancetta export` from `qsos.adi` with date/band/mode filters (`main.rs:649-711`).
37. [P2][S] Per-mode needed/worked sets (`mod.rs:1889-1893`, `async_database.rs:867-872`).
38. [P2][S] Needed matching by entity id, use loaded entity list (`cqdx_bridge.rs:34-46`, `cache.rs:77-85`).
39. [P2][S] DXCC table: keep `=exactcall`/slash sub-entities, version stamp, refresh (`gen_dxcc_table.py`).
40. [P2][S] Operator watchlist (call/prefix/entity/grid) with alert/boost; configurable TTL; `remove()` on completion (`q/watchlist.rs`, `autonomous.rs:865`).
41. [P2][M] Band scheduler: UTC/grey-line/needed-aware, honour `priority` (`q/autonomous.rs:641-645`).
42. [P2][M] Fox: 300–900 CQ, 60 Hz slots, 0.1 `RR73;` frames, pick-best queue (`qso.rs:4344`).
43. [P2][M] Contest: serials, per-band dupe sheet, rate meter, Cabrillo (PAN-50).
44. [P2][S] `/R` vs `/P` (`message.rs:1808-1830`).
45. [P2][S] Fake FT2 on default builds (`mod.rs:374-377,389`) — hide unless compiled.
46. [P2][S] Neural OSD inert (`decoder.rs:1813-1814,14437`) — enable at depth≥1 under Deep/Max or remove.
47. [P2][S] `Max` clamped (`ft8.rs:1544-1555`) vs FEATURES.md:9 — fix docs or allow with overrun warning.
48. [P2][S] Ship W4.3 multipass under Deep (`decoder-comparison.md:282-295`).
49. [P2][S] In-product decode yardstick: per-slot count; `benchmark-decode --jt9-path` (`main.rs:895-898`).
50. [P2][S] PTT methods: DTR/RTS, VOX, CM108 (`rig.rs:234-256` cosmetics).
51. [P2][S] TX gain + ALC/power readback (`set_power_level` uncalled; `tx.rs:1689`).
52. [P2][S] rigctld stderr to log (`hamlib.rs:1227-1228`).
53. [P2][S] Multi-instance: `--config`-driven data dir, lock, per-instance ports (`mod.rs:1929-1930`).
54. [P2][S] ENOSPC handling; blocking appender for ADIF/audit (`audit.rs:14-17`, `main.rs:1772`).
55. [P2][S] launchd log path mismatch; Windows service template.
56. [P2][M] Self-hosted LAN control path (token-auth WS + arm) so panino works without the cqdx relay.
57. [P2][S] TUI: CQ-only/needed-only band filter; logbook browse/edit/notes; manual log entry; QRZ lookup key (grep = 0 in `keymap.rs`/`app.rs`).

**P3**
58. [P3][S] Continuity gate one-window delay on never-seen calls (`q/callsign_continuity.rs:374-404`) — let tier≥2 through immediately.
59. [P3][S] QRZ XML → DXCC/state/name into ADIF (grid-only today, `qso.rs:6215-6283`).
60. [P3][S] Confirmation pull (LoTW scaffold `lotw.rs`, cqdx confirmed feed) → `confirmed` column, "needed = not confirmed".
61. [P3][S] Daily summary via email/ntfy headless.
62. [P3][S] N1MM+ contact UDP.
63. [P3][S] Publish WS-feed JSON schema in-repo; MQTT bridge for HA/Node-RED.
64. [P3][S] PAN-72 (adaptive TX-offset switching) — already tracked, endorse.

## 4. Capability matrix

| Feature | Status | Evidence |
|---|---|---|
| Decode FT8 / FT4 | live / live (78 % cap) | `protocol.rs:187,215`; `decoder-comparison.md:154-161` |
| FT2 | feature-gated, leaks as fake label | `pancetta/Cargo.toml:115-117`; `mod.rs:374-377` |
| JS8/MSK144/WSPR/Q65/FST4 | missing (labels; `WsprConfig` dead) | `mode.rs:37-43`; `network.rs:20` |
| Manual call / CQ / auto-sequence | live | `tui_relay.rs`, `qso_manager.rs` |
| Autonomous pounce + CQ (supervised) | live, weighted scorer not tiers | `q/autonomous.rs:1539` |
| Autonomous headless | respond-to-direct-callers only **[V]** | `autonomous.rs:549-563` |
| Multi-stream TX | live | `q/frequency.rs`, `tx.rs` |
| Local ADIF + SQLite | live (no fsync; random field order) | `adif_log_writer.rs:60-107`; `adif.rs:1082` |
| ADIF import | missing (uncalled fn) | `async_logger.rs:554` |
| Persistent dupe check | dead in prod **[V]** | `qso.rs:2031` |
| B4 per band | live, one-band seed | `priority_evaluator.rs:403-414`; `qso.rs:2150` |
| ClubLog / QRZ logbook | live-unverified | `qso_upload.rs:100-166,188-251` |
| cqdx logbook | live, mock-tested | `client.rs:262-306` |
| LoTW upload | scaffolded (argv only) | `qso_upload.rs:812` |
| eQSL upload | scaffolded (body shape) | `qso_upload.rs:426-431` |
| Confirmation pull | missing | `lib.rs:53` |
| Upload retry/outbox/feedback | missing (PAN-57, L5) | `qso.rs:6469-6580` |
| QRZ XML enrichment | live (grid only) | `qso.rs:6215-6283` |
| Needed-DXCC per band | live w/ cqdx; degraded offline | `cqdx_bridge.rs:190-215` |
| ATNO | cqdx-only | `cqdx_bridge.rs:34-46` |
| Needed grid | cqdx scaffolded; local per-band live | `client.rs:165-168` |
| Per-mode needed | missing | `mod.rs:1889-1893` |
| Operator watchlist | missing (auto 150 s memory) | `q/watchlist.rs` |
| Audible / push alerts | config-only, dead | `network.rs:703-760`; `ui.rs:705-720` |
| DX cluster | live (mode/band dropped; filters ignored) | `cluster.rs:209`; `dx_cluster.rs:70` |
| Spot → QSY → call | scaffolded (no QSY) | `app.rs:3106-3113` |
| cqdx spots → DX Hunter | dead wiring | `mod.rs:1887-1893` |
| PSKReporter upload | live path, wrong element IDs **[V]** | `pskreporter.rs:683-715` |
| PSKReporter consume | scaffolded | `pskreporter.rs:204` |
| Band hopping | config-only default off, dumb | `q/autonomous.rs:585-660` |
| Band-plan region | live, soft warning | `band.rs:59-85` |
| RF split | live manual Shift+F | `remote-operation.md:8` |
| Hound | live, inverted **[V]** | `qso_manager.rs:39-45` |
| Fox | partial (CQ@1500 + cap) | `qso.rs:4344` |
| Contest recognition | scaffolded, unreachable | `qso_manager.rs:1271` |
| Cabrillo/dupe sheet/rate/mults | missing (PAN-50) | grep |
| POTA/SOTA scoring | heuristic suffix | `q/priority.rs:235-249` |
| POTA activation refs / spot API | missing | `adif.rs:719` |
| WSJT-X UDP emit | live (FT8 only) | `wsjtx_udp/mod.rs:795-814` |
| WSJT-X Reply/HaltTx/Replay | live, fail-closed | `mod.rs:348-525` |
| WSJT-X as modem | missing | `codec.rs:504` |
| N1MM UDP | missing | grep |
| HTTP API / MQTT / webhook | missing (`WebApiConfig` dead) | `network.rs:766-900` |
| WS read-only gateway | live, localhost, off | `remote_gateway/mod.rs:85-147` |
| Station agent remote TX | live via cqdx relay | `station_agent/mod.rs` |
| Prometheus | scaffolded (no emitters) | `Cargo.toml:113` |
| Rig breadth | 14-name whitelist | `hamlib.rs:1003-1021` |
| Rig mode set | missing | `set_mode` uncalled |
| PTT CAT / DTR-RTS / VOX / CM108 | live / missing ×3 | `rig.rs:234-256` |
| SDR RX | missing | `pancetta-audio/src` |
| Multi-rig / multi-instance | missing | `mod.rs:1929-1930` |
| PTT watchdog / PttGuard | live | `hamlib.rs:1753`; `tx.rs:2268` |
| SIGTERM | missing **[V]** | `main.rs:395` |
| TX inhibit on rig/audio loss | missing | `tx.rs:1126-1157` |
| Band-edge gate before PTT | missing | `tui_relay.rs:2213` |
| Local TX audit | missing headless | `tx.rs:911-925` |
| Runtime clock monitor | missing | `doctor.rs:231-251` |
| systemd / launchd / Windows | live / live (log path) / docs-only | `packaging/` |
| Effort presets | live (`Max` clamped) | `effort.rs:56-67`; `ft8.rs:1544` |
| AP1–4, cross-cycle, joint, multipass | live default | `decoder.rs:6905,1879,1912,1797` |
| Callsign priors / deep search | research-only | `pancetta-research` |
| Neural OSD | inert | `decoder.rs:1813-1814` |
| jt9 side-by-side | research-only | `pancetta-research/src/decoder.rs:1173` |
| TUI logbook edit / filters / lookup | missing | `keymap.rs`, `app.rs` grep |

## 5. Five world-class bets

1. **"Bring your log."** `pancetta import`, per-band/per-mode worked+confirmed from LoTW/cqdx confirmations, ATNO/band-slot derived locally, LoTW/eQSL verified with fixtures. Tiers, B4, dupes and alerts become trustworthy without cqdx; cqdx becomes an accelerator, not a dependency.
2. **Tier-native autonomy + alerting.** Pounce policy in tier terms, operator watchlist, bell/ntfy/webhook/panino push on tier≥N decode *or* spot, a band scheduler that moves the dial (and rig mode) to where a needed entity is spotted. "Wake me and work it."
3. **A Hound/Fox that matches the air.** Fix the inverted convention, 0.1 frames, follow the Fox's slot; 300–900 Fox with 60 Hz slots and `RR73;` multiplex. Multi-stream TX is a real structural edge for Fox — today it's the one place the code is confidently wrong.
4. **Headless a control operator can lawfully leave.** SIGTERM-safe; rig/audio/clock/band-edge in `tx_hard_mute_reason`; durable TX audit; real Prometheus + LAN status/control page; `unattended_policy` with pounce-as-response; daily summary. Then the Pi-behind-the-rig story is true.
5. **Interop as a moat.** Complete Status(1)/FT4 over UDP, optional WSJT-X-as-modem, N1MM contact UDP, MQTT/HA bridge, published schema. Let GridTracker/JTAlert/N1MM users adopt the brain first and the decoder second — and use their decode counts as the yardstick that drives decoder work.
