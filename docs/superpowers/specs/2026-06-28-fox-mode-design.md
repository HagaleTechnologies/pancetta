# FT8 Fox Mode (run as the DXpedition) — Design Spec

**Date:** 2026-06-28
**Status:** Proposed (self-approved under operator's overnight "use best judgement" authorization; operator to review on waking)
**Author:** Claude Opus 4.8 (autonomous, under K5ARH standing authorization)

## Goal

Let pancetta run **as the DXpedition Fox**: call CQ and work **many Hound callers concurrently**,
transmitting multiple FT8 signals per 15s slot, sequencing each caller report→R-report→RR73 to a
logged completion. Manual-engaged (a Fox-mode toggle), the counterpart to the shipped Hound (chaser).

## Key finding: Fox is mostly REUSE

The hard parts already exist (verified against the codebase):
- **Multi-stream TX:** `coalesce_transmit_requests` already folds concurrent **same-parity** QSOs'
  `MessageToSend`s into one `MultiTransmitRequest` (#40); the TX worker keys up to
  `MAX_RETAINED_TX_STREAMS = 8` summed signals in one slot. Fox's ~5 streams fit.
- **Answer many callers:** the always-answer-callers path (#39, `classify_caller_answer` /
  `maybe_answer_caller` in `coordinator/qso.rs`) already auto-opens a reply QSO to each station calling
  us, gated by a concurrent-answer cap (`max_concurrent_qsos`) + TxPolicy + parity. Each runs as an
  independent QSO state machine.
- **Per-stream offset de-confliction:** the just-shipped TX-offset feature made `respond_to_caller`
  compute its offset via `compute_manual_tx_offset` (held → **`deconflict_offset`** → Tx=Rx) and set
  `partner_freq` (Tx≠Rx). So when the Fox answers N callers, each reply already lands on a distinct,
  de-conflicted audio offset while still hearing each Hound on its own freq. **Free.**
- **CQ:** `start_cq_manual` creates a tracked `CallingCq` QSO that re-emits a CQ each slot.

So a Fox slot = `[CQ stream] + [report→Hound1] + [report→Hound2] + …` — all same-parity QSOs the
coalescer already multi-streams. Interop with real WSJT-X Hounds works because FT8 decodes the whole
passband (we hear Hounds wherever they call/QSY) and we reply on our own Fox-stream offsets.

## What's genuinely NEW (thin)

1. **A Fox-mode flag + engage** (the toggle): while ON, pancetta (a) runs a repeating CQ, (b) raises
   the concurrent caller-answer cap to `fox_max_streams`, (c) tags the session as Fox for the TUI.
2. **The concurrent-answer cap raise:** today `max_concurrent_qsos` bounds #39. Fox mode uses
   `fox_max_streams` (config, default 5) as the cap while engaged. Implementation: a `fox_mode:
   Arc<AtomicBool>` + a `fox_max_streams` the answer-path consults (use `fox_max_streams` when
   `fox_mode` is on, else the normal cap).
3. **The Fox CQ loop coexisting with answering:** the `CallingCq` QSO (CQ stream) runs alongside the N
   answer QSOs; all same parity → coalesced. Confirm the CQ QSO + answer QSOs don't fight the
   parity-admit gate (they're all the Fox's single TX parity — they should all admit).

That's it. No new modulation, no new message formats (standard `<Hound> <report>` / `<Hound> RR73` via
the existing exchange), no new codec.

## Design

### State + config
- `fox_mode: Arc<AtomicBool>` on the coordinator (default false), mirrors `gateway_enabled`/atomics.
- `[fox]` `FoxConfig { max_streams: usize (default 5), … }` in `pancetta-config` (validated 1..=8 to
  respect `MAX_RETAINED_TX_STREAMS`). Threaded to the answer-path cap (a `QsoManagerConfig`/coordinator
  value, like the Hound regions were).

### Engage (TUI → coordinator) — mirror Hound's `EngageHound` path
- A TUI **Fox-mode toggle** key (propose **`x`** for foX — verify free; else `Shift+X`) →
  `TuiCommand::ToggleFoxMode` → `QsoMessage::SetFoxMode { on: bool }` (or a coordinator command) →
  sets `fox_mode` atomic; when turning ON, kick off the repeating CQ (reuse `start_cq_manual` / the
  CQ-repeat the manual `c` path uses) and raise the answer cap; when OFF, stop the CQ (reuse `StopCq` /
  cancel the `CallingCq` QSO) and restore the cap.
- TX-policy gated like every initiation (Fox is heavy TX): refuse engage under RespondOnly/Disabled.

### Answer path (reuse #39, capped at fox_max_streams)
- While `fox_mode`, `maybe_answer_caller` uses `fox_max_streams` as the concurrency cap (so up to N
  Hounds are worked at once) instead of the default. Each answer is a normal `respond_to_caller` →
  offset de-conflicted + `partner_freq` set + multi-streamed by the coalescer. No change to the
  report→R-report→RR73 sequencing (the engine already does it) or logging.

### TUI
- A Fox-mode indicator (title-bar chip "FOX (N max)") + the existing Callers panel / TX strip already
  show the queue + the N keyed streams. Additive.

## Scope / non-goals (v1)
- **Manual-engaged Fox**, answers callers as they arrive up to `fox_max_streams` (no fancy
  priority-pick-N-best-per-slot; the caller pool + cap is sufficient for v1 — a smarter selector is a
  follow-up if needed).
- **Standard messages only** (no SuperFox codec — that's the separate, deferred build).
- **No Fox→Hound frequency *assignment* protocol** (we don't tell Hounds exact slots; we reply on our
  de-conflicted offsets; real Hounds QSY into the Fox region by convention and we hear them anywhere).
- Autonomous Fox (unattended) is out of scope — manual engage only (matches Hound + the FCC posture:
  Fox originates CQ, so it needs a present control operator).

## Risks / careful points
1. **CQ QSO + N answer QSOs all same parity** must all admit (no parity-gate blocking) and coalesce
   into one slot. The parity-admit gate adopts the side for same-parity — verify the CQ + answers
   share the Fox's parity and don't queue each other.
2. **Cap raise must be reversible** (turning Fox off restores `max_concurrent_qsos`).
3. **MAX_RETAINED_TX_STREAMS=8** is the hard ceiling; `fox_max_streams` validated ≤ 8 (CQ stream + N
   answers ≤ 8, so default 5 answers + CQ = 6, fine).
4. **Regression:** `fox_mode` off ⇒ everything behaves exactly as today (#39 cap unchanged). Guard.

## Testing
- Engine/coord: Fox-mode on → CQ emitted; two callers answered in ONE slot, multi-streamed, on
  distinct de-conflicted offsets; each sequences to RR73 + logs. Cap: an (N+1)th caller is NOT
  answered until a slot frees. Fox-off → normal #39 cap (regression).
- coord_sim: Fox engage → CQ + 2 callers → mock rig keys ≥2 distinct offsets in one slot.

## Open questions (self-answered for the overnight build; flag for operator review)
1. **Toggle key:** `x` (foX) if free, else `Shift+X`. (Will verify.)
2. **Priority pick-N vs first-come:** v1 = first-come up to cap (simplest; revisit if the operator
   wants best-callers-first).
3. **Fox CQ cadence/text:** standard `CQ <call> <grid>` every Fox slot via the existing CQ-repeat.
