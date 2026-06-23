# Pancetta Security Review — 2026-04-29

> **Remediation status (updated 2026-06-17).** Phase 1 (C-1 sender verification,
> I-1 per-callsign rate limit) landed 2026-04-29. Phase 2 + most of Phase 3 landed
> 2026-06-17: **I-5** (FFI in-bounds NUL check, `fa1c42b9`), **I-6** (per-slot
> new-call cap, `48e2d03a`), **I-7** (shellexpand tilde-only, `b67a345d`),
> **I-8/I-9** (VACUUM-INTO escape + LIMIT cap, `da088e6f`), **I-10/I-11/I-12**
> (rigctld command allow-list + device-path/port validation; host stays a *warn*
> for remote-rig operability, `f6d52d59`), **I-13** (cqdx error-body token
> redaction, `ff5a0b3f`), **I-14** (PSKReporter `connect()`-to-peer rather than
> the originally-suggested `bind 127.0.0.1`, which would have broken outbound
> routing; `05547a3c`), **I-15** (DX-cluster spot-text sanitization, `05547a3c`),
> **I-16** (decoded-field sanitization at the bus boundary, `48e2d03a`). Each has
> regression tests; full suite green; workspace clippy zero.
>
> **Still open (deliberate):** **I-2/I-3** (POTA/SOTA + DXCC-prefix validation) —
> lookup-dependent, blocked on cqdx endpoint additions; **I-4** (AP-gate boundary
> instrumentation) — log-only telemetry, needs field data to tune; and the Minor
> items. A deeper follow-on audit (RF/FFI memory-safety focus) is tracked
> separately in `docs/security-deep-analysis-2026-06-17.md`.



**Author:** Claude Opus 4.7 (under K5ARH supervision)
**Scope:** Full architectural security review with focus on hostile FT8
transmissions over the air.
**Codebase as-of:** `b92b49f` (post-waterfall TX-offset redesign).
**Methodology:** Four parallel deep-dives (decoder, network/IPC, file
I/O + process, autonomous-operator manipulation) plus controller
verification of the most consequential findings.

---

## Executive Summary

Pancetta's overall posture is **good** for a Rust codebase using
parameterized SQL, mature TLS libraries, and memory-safe parsing.
However, the autonomous-operator state machine has a **single Critical
flaw** that lets any station on the air complete fake QSOs against an
unattended pancetta — a Part-97-relevant problem.

Beyond that one Critical, the review identified ~16 Important findings
spanning input validation, network I/O hardening, and process control,
plus several Minor cleanups. None of the Important findings are
exploitable without one of: (a) operator misconfiguration, (b) compromise
of the cqdx.io / DX cluster server, or (c) local filesystem write
access. The Critical is the only one exploitable purely from RF.

The decoder itself is **well-defended** against malicious FT8
payloads — the `ap_injection_survived()` gate from Task #33
(2026-04-26) prevents the most natural attack (forging a `to:K5ARH`
decode via AP/OSD). Strong work there.

---

## Threat Model

We considered three attacker classes:

1. **Air-only attacker.** Can transmit arbitrary FT8 frames on a band
   pancetta is monitoring. Cannot touch local files, secrets, or
   network endpoints. **This is the highest-priority class** because
   it's the most accessible (anyone with a transceiver and the
   operator's frequency).
2. **Local attacker / malicious sync.** Can write to `~/.pancetta/`
   (config, logs, ADIF, SQLite). Cannot run arbitrary code as the
   operator's user (otherwise game over independent of pancetta).
3. **Compromised remote service.** cqdx.io server, DX cluster, or
   PSKReporter API turns hostile (or is MITM'd). Token validity
   assumed unless the threat involves token theft.

Out of scope: Attacker with the operator's user account (game over
independent of pancetta), kernel exploits, supply-chain attacks against
crates.io.

---

## Findings

### CRITICAL (1)

#### C-1. QSO state machine advances on message type alone — sender identity not verified

- **File:** `pancetta-qso/src/qso_manager.rs:647-734` (`determine_state_transition`) and `:758-793` (`is_message_relevant`)
- **Class:** Air-only attacker. **No privilege required beyond on-air access.**
- **Effect:** Fake QSOs driven to completion → ADIF / SQLite log poisoning + unwanted PR-message TX (RR73 to phantom stations). Part-97 implications: logging contacts that didn't actually occur and transmitting reception confirmations to stations that never transmitted.

**The bug:**

```rust
// qso_manager.rs:781-786 — pattern explicitly discards target_callsign:
(QsoState::RespondingToCq { target_callsign: _, .. },
 MessageType::SignalReport { to_station, .. }) =>
    to_station == &self.config.our_callsign,
```

```rust
// qso_manager.rs:667-686 — destructures `report` only; from_station wildcarded:
(QsoState::RespondingToCq { target_callsign, frequency, .. },
 MessageType::SignalReport { report, .. }) => {
    // advances state without checking who actually sent the report
    Ok(QsoState::SendingReport { ... })
}
```

The state machine advances based on `(current_state, MessageType::variant)`
and a frequency-tolerance check. **It never verifies that the message
came from the DX callsign the QSO was started with.** Repeats for
`ReportAck`, `FinalConfirmation`, `SeventyThree`.

**Attack steps:**
1. Pancetta heard a CQ from `K9ZZ`, transmitted `K9ZZ K5ARH FN42`.
2. Attacker (any callsign) transmits `K5ARH NF4KE -12` on the same
   audio offset in the next slot of the right parity.
3. Pancetta's `to_station == our_callsign` check passes → state
   advances to `SendingReport`. Pancetta TXs `NF4KE K5ARH R-10`.
4. Attacker replies `K5ARH NF4KE RR73`.
5. Pancetta logs a completed QSO with `K9ZZ` (or wherever the
   metadata pointed), even though the entire latter half of the
   exchange came from `NF4KE`.

**Compounding factor:** the 50 Hz frequency tolerance in
`is_message_relevant` (line 766) means the attacker doesn't need to
land exactly on pancetta's TX offset — anywhere within ±50 Hz works.
In multi-QSO mode that's enough to bleed across concurrent QSOs.

**Fix sketch:**

```rust
// in determine_state_transition:
(QsoState::RespondingToCq { target_callsign, frequency, .. },
 MessageType::SignalReport { from_station, to_station, report, .. }) => {
    if from_station != target_callsign { return Ok(current_state.clone()); }
    if to_station   != &self.config.our_callsign { return Ok(current_state.clone()); }
    // ...advance...
}
```

Repeat for every state-advance arm. Also tighten frequency tolerance
from 50 Hz → 15 Hz (≈ FT8 half-tone) and require the message be
addressed to us. The `from_station` check is the load-bearing one;
frequency narrowing is defense in depth.

**Verification:** confirmed live by reading code at the cited lines.

---

### IMPORTANT (16)

Numbered for cross-reference; severity within "Important" loosely
correlates to position. Each entry: file:line, attack class, fix.

#### I-1. No per-callsign rate limit on autonomous CQ responses

- `pancetta-qso/src/autonomous.rs:1035-1082` — `feed_decoded_messages`
  picks the highest-scoring CQ each cycle. The duplicate check at
  `qso_manager.rs:407` operates on a 24h window for *successful*
  QSOs; it does not prevent initiating multiple attempts to a station
  that never completes.
- **Air-only attack:** spam `CQ FAKE FN42` every cycle. Each one looks
  like a fresh CQ, scores enough to clear `min_dx_score`, and pancetta
  initiates. With max_concurrent_qsos > 1, the log fills with abandoned
  fakes; with =1, attacker can hold pancetta's only QSO slot hostage.
- **Fix:** track per-callsign timestamps in `AutonomousOperator` and
  skip if seen within last ~60s.

#### I-2. POTA/SOTA detector accepts any `/P` suffix without validation

- `pancetta-qso/src/priority.rs:118-121` — `is_pota_sota_candidate`
  returns true for any callsign ending `/P`, granting a 0.15 weight
  boost.
- **Air-only attack:** fake activator transmits `CQ N0CALL/P FN42`.
  Boosted priority makes pancetta prefer this fake over real callers.
  Wasted TX time, not log poisoning per se.
- **Fix:** validate against POTA/SOTA spotting databases (live lookup
  or cached daily). For now, gate `/P` boost behind cqdx-confirmed
  POTA reference in the message free-text.

#### I-3. DXCC prefix not extracted before `is_needed_dxcc` lookup

- `pancetta-qso/src/priority.rs` — `is_needed_dxcc(callsign)` looks
  up the full callsign string, not the DXCC entity prefix.
- **Air-only attack:** `CQ ZL/N0CALL FN42` — pancetta has no DXCC
  prefix parsing, so cqdx returns "unknown" and this falls through
  without the correct rare-entity handling. Inverse of I-2: a real
  rare DXCC could be missed if the prefix wasn't extracted.
- **Fix:** extract prefix before calling cqdx (e.g., `ZL/N0CALL` →
  `ZL` → New Zealand entity).

#### I-4. AP/OSD boundary case

- `pancetta-ft8/src/decoder.rs:1451-1462, 2543-2622` — at sync_score
  ~6.6 dB and `suspicion_score == 0`, an AP-injected decode can pass
  with `confidence == MIN_AP_DECODE_CONFIDENCE = 0.55`. CRC + LDPC +
  ap_injection_survived close most of the door, but the gate tightness
  at the boundary is not measured.
- **Air-only attack:** speculative; would require crafting a frame
  that survives all three gates simultaneously.
- **Fix:** instrument candidates that pass confidence gate but fail
  suspicion (currently `suspicion_score >= 2` under scrutiny). If a
  pattern emerges in the field, raise SCRUTINY_THRESHOLD or the
  suspicion threshold.

#### I-5. ft8_lib FFI boundary — no defensive null-terminator check

- `pancetta-ft8/src/ft8_lib_ffi.rs:255, 322` — `CStr::from_ptr` on a
  35-byte stack buffer trusts ft8_lib to null-terminate within bounds.
- **Class:** Air-only, but only if ft8_lib has a bug. ft8_lib is
  trusted, but it has not been hardened against deliberately-crafted
  inputs (it was written for QSO traffic).
- **Fix:** scan the buffer for a null byte before calling `from_ptr`;
  if absent, log and discard.

#### I-6. Recent-calls AP pool DoS

- `pancetta/src/coordinator/ft8.rs:244-259` — pool truncates at 20,
  but a flood of unique callsigns per slot drives many
  `RecentCallAp::new()` constructions. Unbounded over time, only
  bounded per slot.
- **Air-only attack:** spam unique novel callsigns. Causes CPU pressure
  in the decoder thread. Not a memory leak (pool is capped).
- **Fix:** rate-limit new-call additions per slot (e.g., cap at 50 unique
  new calls per slot before short-circuiting to "drop the rest").

#### I-7. `shellexpand::full()` expands `$VAR` references in user config

- `pancetta-config/src/loader.rs:514-517` — `shellexpand::full(content)`
  before TOML parse expands env vars in the raw text.
- **Local-attacker class:** if attacker can write to
  `~/.pancetta/config.toml`, they can write `notes = "$PANCETTA_CQDX_TOKEN"`.
  When pancetta logs the parsed config (debug level, or in error
  paths), the secret prints to logs.
- **Fix:** swap to `shellexpand::tilde()` for `~` only (the original
  intent), or remove expansion entirely. If env var expansion is
  desired, allow-list the variables (e.g., `$HOME`, `$USER`) and forbid
  any token-pattern variable name.

#### I-8. SQL `VACUUM INTO '{path}'` with operator-controlled path component

- `pancetta-qso/src/async_database.rs:521-528` — the backup path is
  formatted into the SQL string with single-quote delimiters. If the
  operator's `config.backup.backup_directory` contains a `'`, the
  string escapes.
- **Local-attacker class:** requires config write access. An attacker
  with that already has many other vectors, but this one specifically
  enables arbitrary SQL execution during a backup operation.
- **Fix:** escape the path (replace `'` → `''`), or refuse paths
  containing single quotes, or use `:memory:` and `ATTACH DATABASE`
  with bind parameters.

#### I-9. `LIMIT {}` unbounded — DoS via `u32::MAX`

- `pancetta-qso/src/async_database.rs:471-472` — `format!("LIMIT {}", limit)`
  is type-safe against SQL injection (`limit` is `u32`), but unbounded.
  A malicious config setting `limit = u32::MAX` produces
  `LIMIT 4294967295` — SQLite returns all rows, allocator OOMs on
  large logs.
- **Local-attacker class:** config write access.
- **Fix:** `let limit = limit.min(10_000);` or similar reasonable cap.

#### I-10. rigctld port allow-list too permissive

- `pancetta/src/coordinator/hamlib.rs:95-99` — accepts any path
  starting with `/dev/tty` (not just `ttyUSB`/`ttyACM`/`ttyS`); accepts
  any `host:port` (even `host:99999999` or `evil.com:4532`).
- **Local-attacker class:** config write. Could redirect rigctld to
  arbitrary remote rigs / TTY devices.
- **Fix:** regex `/dev/tty(USB|ACM|S)\d+`, host-portion ∈ `{127.0.0.1,
  localhost}`, port range `1..=65535`.

#### I-11. rigctld config does not reject non-localhost `host`

- `pancetta-hamlib/src/rigctld.rs:34-44` — the `RigctldConfig.host`
  field defaults to `127.0.0.1` but accepts any string from config.
- **Local-attacker class:** config write. Combined with I-10, allows
  pancetta to drive a rig at `attacker.example:4532`.
- **Fix:** validate at deserialize time (allow-list).

#### I-12. `RigctldClient::send_raw_command` lacks command allow-list

- `pancetta-hamlib/src/rigctld.rs:289-300` — strips newlines and
  non-printable ASCII but doesn't validate command grammar. All current
  internal callers pass safe short-form commands; an external feature
  that exposed user input to this method would be a vulnerability.
- **Class:** internal-developer hazard.
- **Fix:** maintain an allow-list of safe rigctld commands inside the
  function (e.g., only `f`, `m`, `t`, `T 0`, `T 1`, `+x`, etc.); reject
  anything else.

#### I-13. cqdx.io error response body embedded in `CqdxError::Server` raw

- `pancetta-cqdx/src/client.rs:191-195` — server error responses are
  passed through as-is. If cqdx ever echoes the bearer token in an
  error message (server bug), the token ends up in pancetta logs.
- **cqdx-server class:** belt-and-suspenders. The primary fix is
  server-side never-echo-token.
- **Fix:** sanitize: truncate to 200 chars, regex-strip patterns
  matching `pat_[A-Za-z0-9_]+`.

#### I-14. PSKReporter UDP socket binds `0.0.0.0:0`

- `pancetta-dx/src/pskreporter.rs:804` — outbound-only socket bound
  to all interfaces.
- **Class:** local-attacker on multi-tenant host could send crafted
  packets to the ephemeral port if it ever started receiving. Today
  it doesn't, but the bind is unnecessarily exposed.
- **Fix:** `bind("127.0.0.1:0")`.

#### I-15. DX cluster spot comments unsanitized → ANSI in TUI / log forging

- `pancetta-dx/src/cluster.rs:540-579` — spot comment field is
  plaintext from telnet, may contain ANSI escapes, NUL bytes, or
  long strings.
- **DX-cluster class:** cluster server compromise or MITM (telnet has
  no TLS).
- **Fix:** strip control characters (anything < 0x20 except spaces),
  truncate to 200 chars before forwarding to TUI / log / ADIF.

#### I-16. Decoded message strings reach TUI/ADIF without sanitization

- Common pattern across `pancetta/src/coordinator/dsp.rs` →
  `message_bus.rs::DecodedMessage` → `pancetta-tui/src/...` →
  `pancetta-qso/src/adif.rs`.
- ADIF format is length-prefixed so it's robust against in-field
  newlines structurally, but a callsign with an embedded NUL or
  control character could still be ugly in TUI rendering or log
  output. The decoder's `is_plausible` and `looks_like_callsign` cover
  most cases.
- **Fix:** add a `sanitize_for_display` step at the message-bus
  boundary that rejects (or replaces) control characters in
  callsign/grid/comment fields before any UI/log consumer sees them.

---

### MINOR (≥6)

- **Free-text decode log spam.** `pancetta-ft8/src/message.rs:368-374`
  rejects all i3=0/n3=0 free-text via `is_plausible()` and logs each
  rejection. Hostile transmitter can flood logs.
  *Fix:* downgrade to debug-level for free-text rejections.
- **`.unwrap()` in test code.** `pancetta-ft8/src/message.rs:2275, 2295,
  2346`. Affects tests only.
  *Fix:* `.expect("...")` for clearer failure messages.
- **WAV path disclosure.** `--wav /etc/shadow` produces an error
  message confirming file existence. Local attacker; minimal info.
- **PSKReporter / cqdx response field length caps absent.** Hostile
  servers could return absurdly large strings. Length-cap in serde
  deserializers as defense in depth.
- **Bus `TransmitRequest.message_text` length unchecked.** Internal
  type, but an upstream bug could route huge strings to the encoder.
  *Fix:* assert length ≤ FT8_MAX_TEXT.
- **WAV parser dependency.** `hound` is a parsing crate; theoretically
  exploitable on hostile WAV files. Test-only feature; low priority.

---

## Areas Investigated With No Findings

- **LDPC/CRC bit-pattern panics** — bounds-checked access throughout
  the decoder; no panic surface on hostile bit patterns observed.
- **TLS validation** — reqwest + rustls defaults; no
  `danger_accept_invalid_certs` anywhere.
- **SQLite parameterized queries** — sqlx bind-parameter usage is
  consistent except the LIMIT and VACUUM INTO cases noted above.
- **PTT safety on shutdown** — three-path drop (message bus → direct
  rigctld → PttGuard) is well-designed.
- **Audio device enumeration** — no observable injection vector.
- **Self-CQ-response prevention** — `our_callsign` self-check uses
  `eq_ignore_ascii_case`, correct.
- **Frequency allocator** — TX-only; no RX routing influence; no
  injection vector.
- **Configuration hot-reload race** — proper `RwLock`; reload validated
  before apply.

---

## Remediation Roadmap

### Phase 1 — Pre-Phase-5 (before unattended on-air operation)

These must land before pancetta runs autonomously on a real antenna
for any extended period. C-1 is the only Critical and is squarely in
this bucket.

1. **C-1 Sender verification** — every `determine_state_transition`
   arm must check `from_station == expected DX callsign`. Add a
   matching guard to `is_message_relevant`. Tighten frequency
   tolerance from 50 Hz → 15 Hz.
2. **I-1 Per-callsign rate limit** in `AutonomousOperator` — at
   minimum 60s between responses to the same callsign, regardless of
   QSO completion state.

### Phase 2 — Hardening (within next sprint)

3. **I-7 shellexpand audit** — drop `shellexpand::full`, use
   `tilde` or remove entirely.
4. **I-10/I-11/I-12 rigctld input validation** — tighten port path
   regex, host allow-list, command allow-list.
5. **I-8/I-9 SQL formatting paths** — escape `'` in VACUUM INTO,
   cap LIMIT to 10000.
6. **I-15/I-16 String sanitization at message bus boundary** —
   strip control chars from decoded callsigns/comments and DX cluster
   spots before they reach UI/log/ADIF.

### Phase 3 — Defense in depth

7. **I-2/I-3 POTA/SOTA + DXCC prefix validation.** Lookup-based
   fixes; depend on cqdx endpoint additions.
8. **I-4 AP gate boundary instrumentation.** Log-only; tune from
   field data.
9. **I-5 FFI null-terminator check.** Cheap defensive add.
10. **I-13 cqdx error sanitization.** Belt-and-suspenders.
11. **I-14 PSKReporter localhost bind.**
12. **I-6 Recent-calls per-slot cap.**
13. **All Minor items.** As convenient.

---

## Notes for the Operator

- **The decoder is well-defended.** The AP-survival gate from Task
  #33 is doing real work; the most natural attack (forging a
  to:K5ARH decode) is blocked by it. Don't loosen those gates without
  measuring impact.
- **The state machine is the soft target.** Once a decode reaches
  the QSO state machine, all assumptions about authenticity drop
  away. C-1 is real and Phase 5 (unattended on-air) should not
  proceed without fixing it.
- **The cqdx.io operator and pancetta operator are the same person.**
  This collapses several "compromised remote service" findings into
  "don't run a buggy server." Specifically I-13 and the spot-string
  cap issues are belt-and-suspenders given that trust relationship.
- **The "ham trust model"** is part of the design — the FT8 protocol
  itself has no authentication, so we live with that. The fixes
  above are about **internal consistency** (don't log a QSO that
  doesn't match the message stream we observed) and **operator
  integrity** (don't claim achievements that weren't earned).
