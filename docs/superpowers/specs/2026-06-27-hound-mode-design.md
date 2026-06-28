# FT8 Hound Mode (DXpedition chaser) — Design Spec

**Date:** 2026-06-27
**Status:** Proposed (awaiting operator review)
**Author:** Claude Opus 4.8 (under K5ARH supervision)

## Goal

Let pancetta work a DXpedition station running **FT8 DXpedition ("Fox/Hound") mode** by acting as
the **Hound** (chaser): call the Fox low, QSY up when the Fox answers us, complete on the Fox's RR73,
and log the contact — manually engaged by the operator on a chosen station. One sentence: *operator
picks a Fox in the DX Hunter, presses a key, and pancetta runs the Hound side of the exchange to
completion.*

## Background — the Hound procedure (public protocol)

FT8 DXpedition mode (K1JT/NCDXF, documented in the WSJT-X user guide) lets one Fox work many Hounds
quickly. The **Hound** side, which is all this spec implements:

1. **Call low.** The Hound calls the Fox with a standard call `<Fox> <us> <grid>`, transmitting in the
   **300–900 Hz** audio "calling" region (Hounds congregate low so the Fox can hear the pile-up).
2. **Fox answers with a report.** The Fox sends `<us> <Fox-or-blank> -NN` (a signal report directed at
   the Hound), typically in its own region above 1000 Hz.
3. **QSY up + Roger.** On seeing its report, the Hound **moves its TX up into the >1000 Hz region** and
   sends `<Fox> <us> R-NN` (Roger + report). This QSY is the defining Hound behavior.
4. **Fox confirms.** The Fox sends `<us> RR73`; the Hound logs the QSO.

It is an **operating procedure on the existing FT8 stack** — same 8-GFSK modulation, same 77-bit
messages, same encoder/decoder. No `protocol.rs`/modulator/decoder changes. (`pancetta-ft8`'s protocol
abstraction already enumerates FT8/FT4/FT2; Hound is *not* a new `Protocol` — it stays FT8.)

## The central new concept: TX offset ≠ partner RX offset

Today the QSO state machine treats a QSO's single `frequency: f64` (audio offset) as **both** our TX
offset **and** the partner's RX offset — legitimate because pancetta answers Tx=Rx (we reply on the
DX's frequency), so the partner's later frames pass the relevance gate
(`qso_manager.rs` `is_message_relevant`, matching within `FREQ_TOLERANCE_HZ`/`ESTABLISHED_*`).

**Hound breaks this symmetry on purpose:** we hear the Fox at its (low-ish, then its own) frequency but
transmit elsewhere — low while calling, then high after QSY. So the design **splits the two
frequencies**:

- `metadata.frequency` — **our TX audio offset** (unchanged meaning; what we modulate on). Hound sets
  it low at open, then mutates it high at QSY.
- `metadata.partner_freq: Option<f64>` — **the Fox's RX audio offset** (where we *hear* the Fox).
  Latched at QSO open from the Fox's decode. The relevance/partner-frequency gate keys on this when
  set. **`None` for every normal QSO → the gate falls back to `metadata.frequency` exactly as today**
  (zero behavior change for non-Hound QSOs — the hard invariant of this design).

This is the one cross-cutting change; everything else is additive state-machine arms + a control path.

## Design

### Data model (`pancetta-qso/src/states.rs`)

Add to `QsoMetadata`:
- `hound: bool` (default `false`) — this QSO uses the Hound procedure.
- `partner_freq: Option<f64>` (default `None`) — the Fox's RX audio offset for the relevance gate.
- `hound_qsyed: bool` (default `false`) — whether we have already QSY'd up (so the QSY fires once).

(We use a parallel `bool` + fields rather than overloading `CallInitiation`, because Hound is
orthogonal to Manual/Auto — a Hound QSO is also `Manual` in v1.)

The per-state `frequency` field in `QsoState` variants continues to carry our TX offset; the QSY mutates
it alongside `metadata.frequency` (same pattern the stuck-hop already uses).

### Engagement path (operator → QSO)

Mirror the existing `CallStation` path end-to-end, additively:
1. **TUI key** on a selected DX-Hunter row → new `TuiCommand::EngageHound { callsign, fox_freq, dx_parity, fox_grid? }` (`pancetta-tui` `tui_runner.rs`). Proposed key: **`h`** (Hound) on the DX Hunter panel (confirm in review).
2. **Relay** (`coordinator/tui_relay.rs`) → new `QsoMessage::EngageHound { callsign, fox_freq, dx_parity }` (`message_bus.rs`), gated by `TxPolicy` exactly like `CallStation` (refused when RespondOnly/Disabled, with an operator warning).
3. **QSO component** (`coordinator/qso.rs`) → calls a new `QsoManager::engage_hound(...)` (thin wrapper over `respond_to_cq_with`) that:
   - latches `hound = true`, `partner_freq = Some(fox_freq)`, `initiated_by = Manual`, `role = Caller`,
   - sets `metadata.frequency` = a **low calling offset** (see freq management),
   - latches `tx_parity = dx_parity.opposite()` and runs the existing half-duplex parity admit gate (same as manual `StartQso`),
   - opens in `RespondingToCq`, emitting the opening `<Fox> <us> <grid>` call.

If the cross-parity gate defers it, reuse the existing `PendingManualCalls` behavior (a Hound engage is just a manual call with a flag).

### Frequency management

New constants (alongside `TX_OFFSET_MIN_HZ`/`MAX_HZ` in `qso_manager.rs`):
- `HOUND_CALL_MIN_HZ = 300.0`, `HOUND_CALL_MAX_HZ = 900.0` — calling region.
- `HOUND_RESPONSE_MIN_HZ = 1000.0`, `HOUND_RESPONSE_MAX_HZ = 2700.0` — post-QSY region.

- **Opening (call-low) offset:** a value in `[300, 900]`. v1: pick deterministically-varied per QSO
  (e.g. derived from a per-QSO counter/callsign hash into the range — avoids the `Math.random` ban and
  keeps tests deterministic) so concurrent Hound QSOs don't stack on one offset. Configurable default
  midpoint via `[hound]` config.
- **QSY (post-report) offset:** on the report trigger, set our TX offset to a value in `[1000, 2700]`
  (same deterministic-spread approach). pancetta does not coordinate with a real Fox's slot allocator,
  so any offset in the conventional Hound-response region is correct; spreading reduces self-collision
  across simultaneous Hound QSOs.

### State-machine hooks (`pancetta-qso/src/qso_manager.rs`, `exchange.rs`)

1. **Relevance gate** (`is_message_relevant`, ~`qso_manager.rs:2439/2454`): when `metadata.partner_freq`
   is `Some(f)`, match the incoming partner frame's frequency against **`f`** (with the existing
   tolerances) instead of `metadata.frequency`. `None` → unchanged (fallback to `metadata.frequency`).
   This is the ONLY edit to the gate; behavior for non-Hound QSOs is byte-identical.
2. **QSY trigger** at the `(RespondingToCq, SignalReport)` transition (~`qso_manager.rs:2050-2083`) +
   the reply-emit window (~`:1718-1773`, beside the stuck-hop mutation): if `metadata.hound &&
   !metadata.hound_qsyed`, compute the QSY offset, set `metadata.frequency = qsy`, `qso_frequency =
   qsy`, the next state's `frequency = qsy`, and `metadata.hound_qsyed = true`. This **fires regardless
   of `TxFreqMode`** (procedure-mandated, unlike the Auto-gated stuck-hop). The emitted `ReportAck`
   (`<Fox> <us> R-NN`, already produced by `exchange.rs:393-397`) then rides the new high offset.
3. **Completion** on the Fox's RR73: the existing Caller arms
   (`WaitingForConfirmation`/`SendingReport` + RR73/73) already handle it — no new completion logic.
   The relevance gate (now partner_freq-keyed) routes the Fox's RR73 correctly.
4. **Keep-calling:** `rearm_manual_calls_at` re-emits on `metadata.frequency` each slot — automatically
   correct both before (low) and after (high) QSY. The manual watchdog (5 min / 25 calls) retires an
   unanswered Hound call exactly as for any manual call.

### Logging + display

- **Mode stays `FT8`** (Hound is FT8). Do **not** set `SUBMODE`. Flag the Hound contact **both** ways
  (operator decision 2026-06-27): append `"HOUND"` to `QsoMetadata.tags` → rendered into the ADIF
  `COMMENT` field (human-readable) **and** emit an application-defined ADIF field `APP_PANCETTA_HOUND`
  (value `true`) for machine tooling (`adif.rs`). MODE/SUBMODE stay `FT8`, so logbooks/LoTW/eQSL accept
  it as a normal FT8 QSO while both humans and tools can see it was a DXpedition catch.
- **TUI:** a `Hound` badge in the QSO-status ladder (`states.rs` `ladder_view` / `ui/qso_status.rs`)
  and an operator status line on engage ("Hound: calling <Fox> low @ NNN Hz" → "Hound: QSY'd to NNNN
  Hz, sent R-NN"). Additive.

### Config (`pancetta-config`)

New `[hound]` `HoundConfig` (all defaulted; section optional):
- `call_min_hz` / `call_max_hz` (300 / 900)
- `response_min_hz` / `response_max_hz` (1000 / 2700)

Validation: ranges within `[200, 3000]` and min < max; otherwise fall back to defaults with a config
warning (mirrors existing config-validation style).

## Scope / non-goals (v1)

- **Hound only.** Running *as* the Fox (the DXpedition transmitter) is the **next** build (Fox uses the
  existing multi-stream TX primitive). Out of scope here.
- **Manual-initiated only.** Operator engages Hound on a chosen station; no autonomous Fox-hunting. The
  autonomous scorer is untouched.
- **No Fox-coordination protocol.** pancetta follows the call-low/respond-high convention + tracks the
  Fox at its RX frequency; it does not parse a Fox's frequency-assignment messages (real Foxes manage
  their own slots; as a Hound we just need to be heard low and answer high).
- **Mode stays "FT8"** on the wire/rig-api. The `mode` field on the rig-api snapshot (dispensa Q-0009
  forward note) is deferred to the **FT4** build (the first genuinely-separate mode).
- **No split-rig interaction.** Hound QSY is an *audio-offset* move within the passband; it does not
  touch the rig dial or the RX≠TX rig-level split feature.

## Risks / careful points

1. **Tx≠Rx asymmetry (the big one).** Audit every site that treats `metadata.frequency` as the
   partner's RX freq: the relevance gate (handled via `partner_freq`), `qso_filter.rs` partner-freq
   filtering (mirror the same `partner_freq`-when-set logic), and `effective_tx_dial` RF-stamp logging
   (this uses our TX offset = `metadata.frequency`, which after QSY is our real TX offset — **correct**;
   the Fox RX freq is never used for RF logging). The plan must include a focused audit task.
2. **QSY must fire once and regardless of `TxFreqMode`** — guarded by `hound_qsyed` + not gated on Auto.
3. **Non-Hound regression:** `partner_freq = None` must reproduce today's behavior exactly. A property
   test (Hound flag off ⇒ identical transitions to a normal Caller QSO) guards this.
4. **Parity:** Hound vs Fox is alternating-slot like any QSO; reuse the existing half-duplex parity
   admit gate. No new parity logic.

## Testing

- **Unit:** offset-selection helpers (in-range, spread); the `partner_freq` relevance-gate branch
  (Fox frame at fox_freq passes; an impostor at our TX offset does not advance).
- **Engine (sim):** a full Hound exchange through the real `QsoManager` —
  engage → call-low → Fox report (at fox_freq) → assert QSY into [1000,2700] + `R-NN` emitted on the new
  offset → Fox RR73 → `Completed` + `tags` contains `HOUND`. Mirror the Phase-5 scenario style in
  `pancetta-qso/tests/autonomous_scenarios.rs`.
- **Regression:** a non-Hound Caller QSO is byte-identical (property test, `partner_freq=None`).
- **Coordinator sim (`pancetta/tests/coord_sim.rs`):** Hound engage → rig-level PTT keys on the low
  offset, then on the high offset after QSY (the offset actually changes on the wire).
- **Security:** sender-verification still holds (the Fox is the latched partner; impostors rejected) —
  reuse the adversarial pattern.

## File-touch summary

| File | Change |
|------|--------|
| `pancetta-qso/src/states.rs` | `QsoMetadata.{hound, partner_freq, hound_qsyed}`; ladder Hound badge |
| `pancetta-qso/src/qso_manager.rs` | `engage_hound` ctor; relevance-gate `partner_freq` branch; QSY-on-report; Hound freq constants + offset helpers |
| `pancetta-qso/src/exchange.rs` | (likely none — R+report message already exists; verify) |
| `pancetta-qso/src/qso_filter.rs` | `partner_freq`-when-set filtering (the acknowledged Hound TODO at :21) |
| `pancetta/src/message_bus.rs` | `QsoMessage::EngageHound` |
| `pancetta/src/coordinator/{tui_relay.rs,qso.rs}` | relay + handler for `EngageHound` (TX-policy gated) |
| `pancetta-tui/src/tui_runner.rs` (+ `ui/qso_status.rs`, `app.rs`) | `h` key → `TuiCommand::EngageHound`; Hound badge/status |
| `pancetta-config/src/{lib.rs,hound.rs}` | `HoundConfig` + validation |
| `pancetta-qso/src/adif.rs` (verify) | `HOUND` tag → ADIF comment/app field |
| tests: `pancetta-qso/tests/`, `pancetta/tests/coord_sim.rs` | engine + coord + regression + unit |

## Resolved decisions (operator, 2026-06-27)

1. **Engage key = `Shift+H`** on the DX Hunter panel (Space=call, Enter=details remain). *(Shipped as
   `Shift+H`, not `h`: lowercase `h` was already bound to StopTx — this is the spec's documented fallback.)*
2. **Offsets = deterministic spread** — per-QSO offset derived from a counter/callsign hash into the
   region (300–900 low, 1000–2700 high); spreads concurrent Hound QSOs, fully deterministic.
3. **Log flag = BOTH** — `"HOUND"` in `tags`→ADIF `COMMENT` **and** an app-defined `APP_PANCETTA_HOUND`
   ADIF field. MODE/SUBMODE stay `FT8`.
