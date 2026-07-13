# WSJT-X UDP Message Protocol — Clean-Room Reference Notes

**Provenance:** every byte-level fact below was verified from permissively-licensed
reimplementations (Apache-2.0 / MIT / BSD-3), real captured packets embedded in those
repos' test suites, Qt's own serialization documentation, GridTracker's BSD-3 source,
or WSJT-X's release notes / user guide (documentation, not source). WSJT-X
`Network/NetworkMessage.hpp` (GPLv3) is the normative upstream reference; it was NOT
read and nothing was copied from it, consistent with this repo's clean-room policy
(README §Provenance). Facts not verifiable without GPL source are marked ⚑ in §8 and
must be treated as best-effort by the implementer.

**Role naming:** pancetta plays the *WSJT-X role* (upstream confusingly calls it the
"client"; the listening companion app — GridTracker, JTAlert, a logger — is the
"server"). Current protocol state as of WSJT-X 3.0.1 (May 2026).

---

## 1. Transport

| Item | Value |
|---|---|
| Protocol | UDP/IPv4 (IPv6 possible; everything in the wild is IPv4) |
| Default port | **2237** |
| Datagram = 1 message | No batching, no fragmentation handling. Keep datagrams comfortably under MTU; py-wsjtx rejects > 2048 bytes, so treat ~1400 B as a practical ceiling (only `LoggedADIF` gets close) |
| Direction OUT (pancetta → apps) | Heartbeat, Status, Decode, Clear, QSOLogged, Close, WSPRDecode, LoggedADIF |
| Direction IN (apps → pancetta) | Heartbeat, Clear, Reply, Close, Replay, HaltTx, FreeText, Location, HighlightCallsign, SwitchConfiguration, Configure, AnnotationInfo |

**Who binds what.** The WSJT-X-role process (pancetta) opens **one** UDP socket on an
**ephemeral local port**, sends all outbound messages to the single configured
destination (`host:2237` unicast, or `group:2237` multicast), and reads inbound
requests **from that same socket** — companion apps reply to the *source address:port*
of the datagrams they receive. Verified in GridTracker (BSD-3): it stores `remote`
from each received packet and sends Reply/HaltTx/Configure to exactly that addr:port;
k0swe/wsjtx-go (Apache-2.0) does the same. So pancetta must keep its sending socket
open and poll it for requests — there is no separate listening port on the WSJT-X side.

**Unicast vs multicast.** With unicast, only one app can own `0.0.0.0:2237` (JTAlert
historically monopolized it and offered a "rebroadcast on another port" workaround).
The modern convention is **multicast**: send to an administratively-scoped group
(commonly **239.255.0.0**, or **224.0.0.73** which GridTracker's docs recommend for
separate computers), and every companion app binds port 2237 with
`SO_REUSEADDR`/`SO_REUSEPORT` and joins the group. GridTracker joins the group **on
every IPv4 interface** and sets multicast TTL 3 for its own sends. WSJT-X ≥ 2.6
defaults to sending multicast only on the **loopback interface**; reaching another
host requires selecting the real NIC and an adequate TTL — pancetta controls its own
socket, so: set `IP_MULTICAST_IF` to the LAN interface and `IP_MULTICAST_TTL` ≥ 1
(3 is a safe copy of GridTracker's choice) when the destination is a multicast group.

**Heartbeat cadence and ID.** Heartbeat(0) is sent every **15 s** by the WSJT-X side
(start immediately at boot). Companion servers *may* heartbeat back (wsjtx-srv does;
**GridTracker never sends heartbeats at all**), so do not gate any inbound processing
on having heard a client heartbeat. The `Id` field (first field of every message)
identifies the instance: default `WSJT-X`; a second instance launched with
`--rig-name=xxx` is `WSJT-X - xxx` (user-guide-documented convention; GridTracker
displays the part after the last `" - "`). Companion apps key all state by `Id` and
echo it back in requests — pancetta should pick a stable, configurable Id (default
`WSJT-X - pancetta`) and ⚑ should ignore inbound messages whose Id doesn't match its
own (inferred; §8).

## 2. Framing

Every datagram:

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 4 | Magic | `0xadbccbda` (bytes on the wire: `ad bc cb da`) |
| 4 | 4 | Schema | u32 BE. **2** = what every real WSJT-X 2.x/3.x sends; 3 also legal |
| 8 | 4 | Message type | u32 BE, table below |
| 12 | … | Id (utf8 string) | then type-specific fields, packed, no padding, no length prefix, no trailer |

Receivers: drop datagrams whose magic ≠ `0xadbccbda`; accept schema 2 **and** 3
(parse identically — see §7); ignore unknown message types. Emit schema **2**.

Message types 0–16:

| # | Name | Dir (WSJT-X perspective) | # | Name | Dir |
|---|---|---|---|---|---|
| 0 | Heartbeat | Out + In | 9 | FreeText | In |
| 1 | Status | Out | 10 | WSPRDecode | Out |
| 2 | Decode | Out | 11 | Location | In |
| 3 | Clear | Out + In | 12 | LoggedADIF | Out |
| 4 | Reply | In | 13 | HighlightCallsign | In |
| 5 | QSOLogged | Out | 14 | SwitchConfiguration | In |
| 6 | Close | Out + In | 15 | Configure | In |
| 7 | Replay | In | 16 | AnnotationInfo (added WSJT-X 2.7.0/3.0.x era) | In |
| 8 | HaltTx | In | | | |

## 3. Wire encoding (Qt QDataStream conventions, big-endian)

All multi-byte integers big-endian. All floats IEEE 754.

| Type | Encoding |
|---|---|
| `bool` | 1 byte: `00` false, `01` true (accept any nonzero as true) |
| `u8 / i8` | 1 byte |
| `u32 / i32` | 4 bytes BE (i32 two's complement) |
| `u64` | 8 bytes BE |
| `f64` (double) | 8 bytes, IEEE 754 binary64, BE |
| **utf8 string** | u32 BE byte-length, then that many UTF-8 bytes. **Null string = length `0xFFFFFFFF` and no bytes.** Empty string may be encoded as length `0x00000000` (WSJT-X does both — real captures show `ffffffff` for unset DX call and `00000000` for empty TxMessage). Decode both to `""`; emitting either is accepted everywhere. These are serialized *as UTF-8 QByteArrays*, **not** Qt's default UTF-16 QString — do not double-encode |
| **QTime** | u32 BE = **milliseconds since midnight UTC** |
| **QDateTime** | `qint64` Julian Day Number (8 bytes BE) + u32 ms-since-midnight + u8 timespec (`0`=local, `1`=**UTC** ← what WSJT-X sends, `2`=offset-from-UTC, `3`=timezone) + **iff timespec == 2: i32 offset in seconds**. Never emit 2 or 3; tolerate 2 on input. Example: JDN 2459153 = 2020-10-30 |
| **QColor** (HighlightCallsign only, IN) | u8 spec (`0`=Invalid — used to clear highlighting, `1`=RGB) + u16 alpha + u16 red + u16 green + u16 blue + u16 pad(0). 16-bit channels are the 8-bit value replicated. Total 11 bytes |
| "not set" sentinel for u32 fields | `0xFFFFFFFF` |

**Parsing discipline** (matches GridTracker and wsjtx-go): read fields sequentially;
treat **end-of-datagram before a field = field absent** (older peer), and **ignore
trailing bytes** you don't understand (newer peer). This is the protocol's actual
versioning mechanism — fields are only ever appended (Bill Somerville, wsjt-devel).

## 4. Message layouts

Field order is top-to-bottom; every message starts with the §2 header (magic, schema,
type, `Id: utf8`), omitted from the tables. "u32*" = `0xFFFFFFFF` means unset.

### OUT — pancetta emits

**Heartbeat (0)** — every 15 s

| Field | Type | Pancetta value |
|---|---|---|
| MaxSchema | u32 | `3` |
| Version | utf8 | e.g. `2.6.1` — see JTAlert note §7 |
| Revision | utf8 | may be empty (JTDX omits the field entirely; parsers tolerate absence) |

**Status (1)** — on state change

| Field | Type | Notes |
|---|---|---|
| DialFrequency | **u64** | Hz |
| Mode | utf8 | `"FT8"`, `"FT4"`, … |
| DXCall | utf8 | null if none |
| Report | utf8 | e.g. `"-15"` |
| TxMode | utf8 | usually = Mode |
| TxEnabled | bool | |
| Transmitting | bool | |
| Decoding | bool | |
| RxDF | u32 | Rx audio offset, Hz |
| TxDF | u32 | Tx audio offset, Hz |
| DECall | utf8 | own call |
| DEGrid | utf8 | own grid |
| DXGrid | utf8 | |
| TxWatchdog | bool | |
| SubMode | utf8 | null unless JT65 etc. |
| FastMode | bool | |
| SpecialOperationMode | **u8** | `0` = none. Non-zero enum values ⚑ — always emit 0 |
| FrequencyTolerance | u32* | `0xFFFFFFFF` unless relevant |
| TRPeriod | u32* | seconds; `0xFFFFFFFF` = default |
| ConfigurationName | utf8 | e.g. `"Default"` |
| TxMessage | utf8 | added v2.3.0 — the message currently being transmitted |

(⚑ 2.7.0-rc7 release notes claim highlight/score-count fields were added to Status;
no non-GPL implementation parses them and strict 21-field parsers work against
2.7/3.0 traffic. Emit exactly the 21 fields above.)

**Decode (2)** — per decode

| Field | Type | Notes |
|---|---|---|
| New | bool | true = live decode; false = replayed in response to Replay(7) |
| Time | u32 (QTime) | ms since midnight UTC of the decode's slot |
| SNR | i32 | dB |
| DeltaTime | f64 | seconds |
| DeltaFrequency | u32 | Hz (audio offset) |
| Mode | utf8 | **single-character mode glyph**, not the mode name: `"~"` = FT8 (byte-verified from a captured packet). Other glyphs ⚑ (FT4 `"+"` unconfirmed from non-GPL sources — capture one before emitting FT4) |
| Message | utf8 | decoded text, e.g. `"CQ K1ABC FN42"` |
| LowConfidence | bool | |
| OffAir | bool | true when decoding a .wav, not live RX |

**Clear (3)** OUT: header only (no fields). Emit when the band-activity list is
invalidated (e.g. band change). *(Inbound Clear has one extra field — below.)*

**QSOLogged (5)** — when a QSO is logged

| Field | Type | Notes |
|---|---|---|
| DateTimeOff | QDateTime | UTC (timespec 1) |
| DXCall | utf8 | |
| DXGrid | utf8 | |
| TxFrequency | **u64** | Hz (actual TX freq = dial + offset) |
| Mode | utf8 | |
| ReportSent | utf8 | |
| ReportReceived | utf8 | |
| TxPower | utf8 | |
| Comments | utf8 | |
| Name | utf8 | |
| DateTimeOn | QDateTime | |
| OperatorCall | utf8 | |
| MyCall | utf8 | |
| MyGrid | utf8 | |
| ExchangeSent | utf8 | contest |
| ExchangeReceived | utf8 | |
| ADIFPropagationMode | utf8 | e.g. `"ION"`; GridTracker ignores it, wsjtx-go requires it on current versions — emit it (empty ok) |

**Close (6)**: header only. Emit on graceful shutdown (GridTracker marks the instance
closed).

**LoggedADIF (12)**: one field, `ADIF: utf8` — a complete ADIF fragment: header
(`<adif_ver:5>3.1.0`, `<programid:6>WSJT-X`, `<EOH>`) + one `<EOR>`-terminated record,
`\n`-separated. Emit together with QSOLogged(5); loggers variously consume one or the
other.

### IN — pancetta consumes

**Heartbeat (0)**: same layout as OUT. Liveness/schema info only; **do not require it**
(GridTracker never sends one).

**Clear (3)** IN: one field `Window: u8` — `0` clear Band Activity, `1` clear Rx
Frequency window, `2` both (⚑ enum via Apache-2.0 paraphrase only). Tolerate absence.

**Reply (4)** — *the remote-initiation message; what GridTracker sends on double-click*

| Field | Type | GridTracker sends |
|---|---|---|
| Time | u32 (QTime) | echoed verbatim from the Decode |
| SNR | i32 | echoed |
| DeltaTime | f64 | echoed |
| DeltaFrequency | u32 | echoed |
| Mode | utf8 | echoed (the glyph) |
| Message | utf8 | echoed |
| LowConfidence | bool | echoed |
| Modifiers | **u8** | **always `0x00`** |

The whole message is a verbatim echo of one previously-emitted Decode plus the target
instance's Id. Match it against your recent-decode list (Message + Time +
DeltaFrequency uniquely identify a decode in practice). Semantics in §5. Modifiers =
"emulate a keyboard-modified double-click"; release notes document the *example* that
ALT means "reply without changing your Tx frequency offset"; exact bit assignment ⚑.
GridTracker always sends 0.

**Replay (7)**: header only. Respond by re-emitting every decode still in the
band-activity window as Decode(2) with `New=false`, in order, then (convention) a
Status.

**HaltTx (8)**: one field `AutoTxOnly: bool`. `false` = stop TX immediately; `true` =
only disable auto-transmit (clear TX-enable at period end). GridTracker's "halt all
TX" button sends `false`.

**FreeText (9)**: `Text: utf8`, `Send: bool`. Set the free-text message; if Send,
transmit it next TX period.

**Location (11)**: `Location: utf8` — e.g. a Maidenhead grid for the session.

**HighlightCallsign (13)**: `Callsign: utf8`, `BackgroundColor: QColor`,
`ForegroundColor: QColor`, `HighlightLast: bool`. Invalid color (spec byte 0) = clear
highlighting. Safe to parse-and-ignore.

**SwitchConfiguration (14)**: `ConfigurationName: utf8`. Parse-and-ignore acceptable.

**Configure (15)**: `Mode: utf8`, `FrequencyTolerance: u32*`, `Submode: utf8`,
`FastMode: bool`, `TRPeriod: u32*`, `RxDF: u32*`, `DXCall: utf8`, `DXGrid: utf8`,
`GenerateMessages: bool`. Empty strings / `0xFFFFFFFF` = "no change". GridTracker uses
this (not Reply) for its "set call/grid without transmitting" path, and only when your
Status showed `TxEnabled == false`.

**AnnotationInfo (16)**: `DXCall: utf8`, `SortOrderProvided: bool`, `SortOrder: u32`
(`0xFFFFFFFF` removes the entry). Fox-mode caller-priority hints; parse-and-ignore.

### Golden test vectors (real captured WSJT-X packets, from Apache-2.0 wsjtx-go tests)

```
Heartbeat, WSJT-X 2.2.2:  adbccbda 00000002 00000000 00000006 57534a542d58   ("WSJT-X")
                          00000003 ("2.2.2"→00000005 322e322e32) ("0d9b96"→00000006 306439623936)
Decode (FT8 "~", "JA2EJP N4BP 73", SNR -5, DT 0.2, DF 1302, new=1):
  adbccbda 00000002 00000002 00000006 57534a542d58 01 0259baf8 fffffffb
  3fc99999a0000000 00000516 00000001 7e 0000000e 4a4132454a50204e344250203733 00 00
Status/QSOLogged/LoggedADIF vectors: parser_test.go in k0swe/wsjtx-go v4
(Apache-2.0) — copy freely with attribution.
```

## 5. Semantics

**Reply(4) contract** (restated from release notes + user guide, no GPL text):
- Match the echoed decode against retained decodes. **If it doesn't match a retained
  decode, do nothing** (silently drop). Retain decodes until you emit Clear.
- If the matched decode is a **CQ or QRZ** message: behave like a user double-click on
  that line — set DX call/grid from the message, generate the standard QSO messages,
  pick TX parity opposite the caller's slot, move RxDF to the caller's DF; move TxDF
  too unless Hold-Tx-Freq behavior applies. TX-enable: WSJT-X activates Enable Tx on
  double-click **only if the "double-click on call sets Tx enable" option is on** —
  for pancetta, whether Reply may arm TX must AND with the existing `TxPolicy` /
  armed-TX gate (Reply is remote initiation; route it through the same fail-closed
  path as other remote TX, never as `TxOrigin::Local`).
- If it is **not** CQ/QRZ: process the same way **but never enable TX** (explicit
  upstream behavior since ~1.9).
- Requests of this kind are honored by WSJT-X only when the operator checked
  **"Accept UDP requests"**; pancetta must have an equivalent master switch, default
  off.

**Status triggers**: emitted "when internal state changes" ⚑ (precise upstream trigger
list unverified). Safe rule: emit whenever any field of the Status struct changes,
plus one immediately after startup and after each Heartbeat. GridTracker **drops a
datagram identical to the previous one from the same source port**, so identical
back-to-back Statuses are harmless.

**Ordering matters for GridTracker**: an instance becomes *valid* only after its first
**Status**; Decode/QSOLogged/Clear from a never-seen-Status instance are
**discarded**. Emit Status before the first Decode of a session.

**Id targeting**: apps echo your Id in every request and route multi-instance traffic
by it. Process only messages bearing your Id (⚑ inferred).

**Discovery**: companion apps never need pancetta's address configured — they learn it
from the source address of received packets. All you configure is the one destination
(unicast host or multicast group) + port.

## 6. GridTracker on machine B, pancetta on machine A

Verified from GridTracker's BSD-3 source and its official docs:

- **Config on B**: GridTracker Settings → General: port **2237**; enable **Multicast**
  and enter the group IP (docs suggest **224.0.0.73** for separate computers; any
  239.255.x.x works — port stays 2237). GridTracker binds 2237 with `reuseAddr` and
  joins the group on **every** IPv4 interface. Restart both programs after changing
  network settings. Unicast also works cross-host (point pancetta at `B:2237`).
- **Config on A (pancetta)**: send to group:2237 with `IP_MULTICAST_IF` = the LAN NIC
  and TTL ≥ 1 (WSJT-X's loopback-only default is the classic cross-host failure mode).
- **Reply path cross-host**: GridTracker sends Reply/HaltTx/Configure **unicast to the
  source addr:port** of your packets — works across a routed LAN with no config, but
  NOT through NAT toward A unless A's stateful firewall keeps the UDP flow open (A
  transmits every ≤15 s, so mappings stay warm). On B, the OS firewall must permit
  inbound UDP 2237 / multicast.
- **Click-to-call gating**: GridTracker's `initiateQso` requires only that (a) the
  callsign came from your Decode messages, and (b) the instance is known — i.e. **at
  least one Status received**. It does NOT gate Reply on `TxEnabled`, DECall, or
  heartbeats. `TxEnabled==false` in your Status matters only for its alternate
  "set DX call/grid" path, which then uses Configure(15) instead. DECall/DEGrid from
  Status feed its map/distance display — populate them.
- Required WSJT-X-side settings per GridTracker's Appendix B (behavioral parity):
  "Prompt me to log QSO", "Clear DX call and grid after logging", "Accept UDP
  requests".

## 7. Version / compatibility notes

- **Schema history**: 1 = ancient 1.x; **2 = wire value used by every WSJT-X 2.x and
  3.x release observed** (captures from 2.2.2, 2.3.1); 3 = negotiable via Heartbeat
  MaxSchema (WSJT-X advertises MaxSchema 3). Per Bill Somerville (wsjt-devel), schema
  numbers cover *field encodings*, not message layout, and layouts evolve only by
  appending fields. Every permissive implementation parses 2 and 3 identically; ⚑ the
  exact 2↔Qt_5_2 / 3↔Qt_5_4 mapping is upstream-comment lore. **Emit 2, accept 2–3.**
  GridTracker in 2026 doesn't check the schema field at all.
- **Message-set evolution**: 1.8: Reply modifiers byte. 2.0: two-way Clear, QSOLogged
  operator/my-call/grid + exchanges, SpecialOperationMode in Status. 2.1:
  HighlightCallsign highlight-last flag. 2.3.0: Status TxMessage. 2.6: multicast
  default restricted to loopback (sender side). 2.7.0: AnnotationInfo(16). **3.0.0/
  3.0.1: no UDP protocol changes at all** (wsjtx-go needed only AnnotationInfo to
  claim 3.0.2 support; no schema bump).
- **JTAlert** (closed source): needs "Accept UDP requests"; supports multicast since
  ~2.50 (2021). ⚑ Likely reads the Heartbeat `Version` string — advertising an
  implausible version may trigger warnings (unverified). If targeting JTAlert, mimic a
  plausible `Version` like `2.6.1`.
- **Loggers** (Log4OM, N3FJP, HRD, CQRLOG): consume QSOLogged(5) and/or LoggedADIF(12)
  from the same multicast group; emitting both covers all of them.

## 8. Sources and flagged facts

| Source | License | Used for |
|---|---|---|
| github.com/k0swe/wsjtx-go (v4) | Apache-2.0 (verified) | full field layouts, encoder/parser byte rules, magic/null constants, captured golden packets, JTDX tolerances, AnnotationInfo |
| github.com/bmo/py-wsjtx | MIT (verified) | QDateTime incl. timespec=2→offset-i32, Reply builder, packet size limits, schema 2–3 acceptance |
| gitlab.com/gridtracker.org/gridtracker | BSD-3-Clause (verified) | receiver behavior, instance validity (Status-gated), Reply/HaltTx/Configure construction (modifiers=0), multicast socket handling, dedupe, reply-to-source routing |
| github.com/schlatterbeck/wsjtx-srv | BSD | independent field lists, schema-3 sends, sample Status doctest bytes |
| github.com/MarcFontaine/wsjtx-udp | BSD-3 | cross-check message definitions (`reply_modifiers :: Word8`) |
| doc.qt.io/archives/qt-4.8/datastreamformat.html | Qt docs (GFDL 1.3) | QByteArray null rule, QColor layout, IEEE-754 float rule |
| docs.gridtracker.org (Appendix B, Settings) | GridTracker project docs | required checkboxes, 224.0.0.73 cross-machine advice, firewall notes |
| wsjt.sourceforge.io Release_Notes.txt / 2.7.0 | WSJT-X release notes (documentation) | protocol change history, Reply modifiers purpose + ALT example, non-CQ Reply never enables TX, TxMessage addition, multicast loopback default, 3.0.x = no protocol change |
| wsjt.sourceforge.io/wsjtx-doc/wsjtx-main-3.0.0.html | WSJT-X User Guide | UDP Server settings, Accept UDP requests, Outgoing interfaces/TTL, `--rig-name` → `WSJT-X - xxx`, double-click semantics |
| wsjt-devel thread (narkive `y4rTl5y8`) | public mailing list | Somerville: schema = field encodings; append-only evolution |
| hamapps.groups.io multicast thread; n3fjp.com integration page | vendor docs/forums | JTAlert multicast/rebroadcast history, 239.255.0.0 examples |
| sourceforge.net `.../Network/NetworkMessage.hpp` | **GPLv3 — NOT read** | cited as normative upstream only |

**⚑ Facts not verifiable from non-GPL sources (treat as best-effort/optional):**
1. Reply `Modifiers` bit assignments (which bit = Shift/Ctrl/Alt). Verified only: u8,
   emulates modified double-clicks, ALT = keep-Tx-offset example, GridTracker sends 0.
2. Clear `Window` values 0/1/2 meanings (Apache-2.0 paraphrase; consistent, low risk).
3. SpecialOperationMode enum values beyond 0.
4. Schema 2/3 ↔ Qt stream-version mapping (irrelevant: fields encode identically).
5. Whether 2.7.0+ actually appends highlight/score-count fields to Status (resolved by
   "ignore trailing bytes, emit classic 21 fields").
6. WSJT-X ignoring inbound messages with mismatched Id (inferred).
7. Full Decode mode-glyph table (only `"~"` = FT8 byte-verified; capture an FT4 packet
   on-air before emitting FT4 decodes).
8. Precise upstream Status trigger list; JTAlert's Heartbeat-version checking.
