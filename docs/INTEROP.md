# Logger interoperability

Pancetta can tell other ham radio programs about its decodes and logged
contacts over two outbound UDP channels: the WSJT-X UDP protocol, and N1MM
Logger+ `<contactinfo>` datagrams, one per logged QSO. Both channels are off
by default.

## Turning the channels on

N1MM contactinfo, one datagram per logged QSO to one IP address and port:

```toml
[logging]
n1mm_udp = "127.0.0.1:12060"
```

WSJT-X UDP, decodes, status and logged QSOs:

```toml
[network.wsjtx_udp]
enabled = true
destination = "127.0.0.1:2237"
```

Both keys are described in [CONFIG.md](CONFIG.md). The [GUIDE.md](GUIDE.md)
GridTracker section covers WSJT-X UDP across two machines and multicast.
Restart Pancetta after changing either setting.

## Compatibility

| Program | Platform | Channel | Vendor documentation says | What works | What does not work | Verified |
|---|---|---|---|---|---|---|
| GridTracker 2 | Windows, macOS, Linux | WSJT-X UDP | A WSJT-X UDP client; setup recipe in the [guide](GUIDE.md) | Not checked | Not checked | unverified |
| GridTracker 2 | Windows, macOS, Linux | N1MM contactinfo | Logs from ADIF-over-UDP; its "ADIF UDP / N1MM" setting sends ADIF text out. No N1MM XML input is documented | Not checked | Not checked | unverified |
| JTAlert | Windows | WSJT-X UDP | A WSJT-X UDP client | Not checked | Not checked | unverified (no Windows station) |
| JTAlert | Windows | N1MM contactinfo | Sends QSOs to N1MM and other loggers; no N1MM input is documented | Not checked | Not checked | unverified (no Windows station) |
| Log4OM 2 | Windows | WSJT-X UDP | A UDP inbound connection takes WSJT-X QSO messages | Not checked | Not checked | unverified (no Windows station) |
| Log4OM 2 | Windows | N1MM contactinfo | A UDP inbound connection of type N1MM, port 12060 (forum reports) | Not checked | Not checked | unverified (no Windows station) |
| WaveLogGate | Windows, macOS, Linux | WSJT-X UDP | Listens on UDP 2333 for WSJT-X QSO packets; its README disagrees with itself on binary versus ADIF-text packets | Not checked | Not checked | unverified |
| WaveLogGate | Windows, macOS, Linux | N1MM contactinfo | Not mentioned | Not checked | Not checked | unverified |

A row gets a date only when someone has run the check list below against the
real program. Its Verified cell then holds the date, the program version and
the host, and the two result columns hold what was seen.
`pancetta-config/tests/interop_table.rs` enforces these rules.

## The contactinfo datagram

Each logged QSO sends one UTF-8 XML document in one UDP datagram. It is built
from the same record that goes into `qsos.adi`, so call, band, reports, grid
and comment match the log. Elements appear in the order the N1MM+
documentation gives. Here is a QSO with K1DEF on 20 m, with
`[station] power_watts = 100`:

```xml
<?xml version="1.0" encoding="utf-8"?>
<contactinfo>
  <app>N1MM</app>
  <contestname>DX</contestname>
  <contestnr>1</contestnr>
  <timestamp>2026-11-17 10:05:28</timestamp>
  <mycall>W1ABC</mycall>
  <band>14</band>
  <rxfreq>1407400</rxfreq>
  <txfreq>1407400</txfreq>
  <operator>W1ABC</operator>
  <mode>FT8</mode>
  <call>K1DEF</call>
  <countryprefix></countryprefix>
  <wpxprefix></wpxprefix>
  <stationprefix>W1ABC</stationprefix>
  <continent>NA</continent>
  <snt>-11</snt>
  <sntnr>0</sntnr>
  <rcv>-12</rcv>
  <rcvnr>0</rcvnr>
  <gridsquare>FN31</gridsquare>
  <exchange1></exchange1>
  <section></section>
  <comment></comment>
  <qth></qth>
  <name></name>
  <power>100</power>
  <misctext></misctext>
  <zone>5</zone>
  <prec></prec>
  <ck>0</ck>
  <ismultiplier1>0</ismultiplier1>
  <ismultiplier2>0</ismultiplier2>
  <ismultiplier3>0</ismultiplier3>
  <points>1</points>
  <radionr>1</radionr>
  <run1run2>1</run1run2>
  <RoverLocation></RoverLocation>
  <RadioInterfaced>1</RadioInterfaced>
  <NetworkedCompNr>0</NetworkedCompNr>
  <IsOriginal>True</IsOriginal>
  <NetBiosName></NetBiosName>
  <IsRunQSO>1</IsRunQSO>
  <StationName></StationName>
  <ID>945987c79d3a4d9b85b48517c589e254</ID>
  <IsClaimedQso>1</IsClaimedQso>
</contactinfo>
```

Where each value comes from:

| Element | Value | Source |
|---|---|---|
| `timestamp` | UTC, `YYYY-MM-DD HH:MM:SS` | `QsoMetadata::contact_started_at()`, the instant ADIF writes as `QSO_DATE`/`TIME_ON` |
| `mycall`, `operator`, `stationprefix` | our callsign | `metadata.our_callsign` |
| `band` | N1MM MHz token | band of the RF frequency: 160m `1.8`, 80m `3.5`, 60m `5.3`, 40m `7`, 30m `10`, 20m `14`, 17m `18`, 15m `21`, 12m `24`, 10m `28`, 6m `50`, 2m `144`, 70cm `432` |
| `rxfreq`, `txfreq` | RF frequency in 10 Hz units, rounded | `metadata.frequency` (the emitted event carries dial plus offset); both get the same value |
| `mode` | `FT8`, `FT4` or `FT2` | `metadata.mode` |
| `call` | contra callsign | `metadata.their_callsign` |
| `continent` | two-letter code or empty | `pancetta_core::dxcc::continent_for_callsign(call)` |
| `snt`, `rcv` | signed SNR such as `-11`, `+03`, or empty | `AdifQso.rst_sent` / `rst_rcvd`, the same text `qsos.adi` holds |
| `gridsquare` | their grid or empty | `AdifQso.gridsquare` |
| `comment` | notes, `HOUND`, opt-in attribution, XML-escaped | `AdifQso.comment` |
| `power` | watts, or empty when 0 | `[station] power_watts` |
| `zone` | CQ zone, or `0` when unknown | `AdifQso.cqz`, or the stored `CQZ` the record carries instead |
| `IsRunQSO` | `1` when we called CQ, else `0` | `metadata.role == QsoRole::Cqer` |
| `ID` | 32 lowercase hex characters | `metadata.qso_id.simple()` |
| everything else | the constants shown | not carried by Pancetta; values copied from the N1MM+ documentation example |

- The element set follows the N1MM+ documentation. It was not checked against
  a packet captured from a running N1MM Logger+.
- All timestamps are UTC.
- The 60 m token `5.3`, the 70 cm token `432` and the mode strings `FT8`,
  `FT4` and `FT2` are unverified: the N1MM+ page lists none of them.
- `app` is `N1MM`, because programs written for N1MM may filter on it.

## Limits

- One destination. A LAN broadcast address such as `192.168.1.255:12060`
  reaches every host on that network.
- The destination must be an IP address and port. Hostnames, port 0 and
  the unspecified addresses `0.0.0.0` and `[::]` are rejected at startup.
- Changing the setting needs a restart.
- Nothing is sent under `--replay`.
- A QSO is skipped, with a warning in the log, when the partner's callsign is
  unknown or the frequency is outside a known amateur band (for example, when
  no rig dial is known).
- Contacts brought in with `pancetta import` are not sent.
- The grid is the logged grid, without QRZ enrichment.
- A lost UDP datagram is not sent again.

## Verifying a row

GridTracker 2 and WaveLogGate run on macOS, so they can be checked on the
station Mac mini (sophon):

1. Install the program and note its version.
2. Turn on the channel in Pancetta (see above) and in the program.
3. Complete one live QSO. A `--replay` run sends nothing.
4. Record what the program showed in the What works and What does not work
   cells.
5. Set the Verified cell to `YYYY-MM-DD, <program version>, macOS on sophon`.

For WaveLogGate, check first whether it accepts the binary WSJT-X QSOLogged
message on port 2333, since its README contradicts itself on that.

JTAlert and Log4OM 2 run only on Windows. Their rows stay
`unverified (no Windows station)` until a Windows host is available.

## Sources

- N1MM Logger+ external UDP broadcasts:
  https://n1mmwp.hamdocs.com/appendices/external-udp-broadcasts/ (read
  2026-11-17)
- GridTracker 2: https://gridtracker.org/ (read 2026-11-17)
- WaveLogGate: https://github.com/wavelog/WaveLogGate (read 2026-11-17)
- Log4OM: https://www.log4om.com/ (read 2026-11-17)
- JTAlert: https://hamapps.com/JTAlert/ (read 2026-11-17)
