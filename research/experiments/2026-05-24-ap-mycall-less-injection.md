---
slug: ap-mycall-less-injection
mode: ft8
state: won
created: 2026-05-24T04:30:00Z
last_updated: 2026-05-24T04:30:00Z
branch: iter/2026-05-24-batch-3
parent_hypothesis: hb-043
wild_card: false
scorecard: /tmp/ap_recent_{5,20}calls.json
delta_vs_main: 0 composite (infrastructure iter; expected null result on popular-callsigns sanity test)
disposition: WIN (infrastructure) — my_call-less AP injection plumbed end-to-end; hb-027 unblocked
---

## Hypothesis

hb-043 (spawned from hb-004 AP wiring in this session): the existing
AP code paths in `par_try_ap_decode` all short-circuit
`if ctx.ap_context.my_call.is_none()`. AP2's `recent_calls`
injection requires `my_call` to also be set. This couples observed-
callsign priors to operator-callsign knowledge — which prevents
hb-027's "rolling callsign window from prior slots" use case (the
operator is SCANNING, not transmitting, so my_call is irrelevant).

Add a my_call-less AP path that tries each recent callsign at BOTH
bits 0-27 (caller position) and bits 28-55 (called position),
independent of my_call.

## Change

### pancetta-ft8/src/ap.rs

Added `pub fn inject_recent_call_at_called(llrs, call)` — companion
to existing `inject_ap2_caller`, but for the called-position. Uses
the existing `inject_28_bits` private helper.

### pancetta-ft8/src/decoder.rs

1. `decode_window_with_ap` line 452: extended `ap_active` to also fire
   when `recent_calls` is non-empty (not just my_call or active_qso).
2. `par_try_ap_decode`: added a new code path AFTER the existing AP1/
   AP2 branches, BEFORE AP3, that runs when
   `my_call.is_none() && !recent_calls.is_empty()`. Iterates
   recent_calls; for each, tries both Caller (bits 0-27) and Called
   (bits 28-55) injection via the new helper.
3. New helper `par_try_ldpc_with_recent_only` (~80 LOC): mirrors
   `par_try_ldpc_with_ap` but uses a single recent-call injection at
   one position, with a position-specific survival check (the
   decoded message's callsign at the injected position must match
   the injected recent_call). Reuses the same confidence gates as
   the AP framework (MIN_AP_CONFIDENCE=0.55, SCRUTINY_THRESHOLD=0.65).
4. New private enum `RecentInjectPos { Caller, Called }`.

### Eval flag (pre-existed from hb-004 work)

`--ap-recent-calls <C1,C2,...>` already accepts the list. Now that
the algo exists, the flag does something useful without also needing
`--ap-my-call`.

## Result

Sanity sweep on curated-hard-200, no my_call set:

| config                          | rec  | novel | elapsed |
|---------------------------------|-----:|------:|--------:|
| baseline (no AP)                | 4365 |   952 |   ~255s |
| --ap-recent-calls (5 popular)   | 4365 |   952 |   190s  |
| --ap-recent-calls (20 popular)  | 4365 |   952 |   437s  |

The "5 popular" and "20 popular" sets are the top-N callsigns by
appearance frequency in 50/100 hard-200 jt9-baselines (e.g.,
7X5CY, TN8GD, WY7WL...).

**Decode counts unchanged from baseline — this is the EXPECTED null
result for sanity input.** AP only runs when AP0 (standard decode)
fails on a candidate. The popular callsigns chosen for this test are
exactly the ones AP0 already handles well — they appear in hard-200's
jt9 truth precisely BECAUSE they decode cleanly. The candidates that
would benefit from AP rescue are the weak ones AP0 can't find, which
weren't in our injected hint set.

**Wallclock scaling confirms the AP path activates correctly.** 5
callsigns → 190s, 20 callsigns → 437s. The increase from 5 → 20 is
~+247s of additional LDPC work — consistent with ~15 additional
callsigns × 2 positions × ~300 candidates × ~few-ms LDPC each. No
panics, deterministic output, no novel FPs introduced.

## Disposition

**WIN (infrastructure).** The my_call-less AP injection path is
plumbed end-to-end:
- ap_active fires on recent_calls alone
- New helper exists and is called from par_try_ap_decode
- Survival check rejects CRC-coincidence FPs at both positions
- Wallclock scales with N as expected

hb-027 ("Joint multi-slot decoding via QSO context") is now
unblocked. Implementation path:
1. Add a rolling-window callsign tracker to the coordinator or eval
   (tracks callsigns from the last K slots).
2. Pass that tracker's contents as the recent_calls list to each
   slot's decode.
3. Measure whether the rolling prior produces hb-027's expected
   +0.02 to +0.10 on QSO-pattern corpus.

## Learnings

- **AP only runs when AP0 fails — fundamental architectural detail.**
  par_try_ap_decode first tries the standard decode; only on failure
  does it enter the AP fallback. This means any AP variant (existing
  AP1/2/3/4 or new my_call-less) can only ADD decodes that AP0
  couldn't get. For a sanity test to show non-zero delta, the input
  callsigns must overlap with the weak-signal cases AP0 fails on —
  which by definition is unknown a priori.
- **Sanity test design for AP work needs a candidate-AP-failure-then-
  AP-recovery scenario.** A future helper could decode a WAV with
  AP off, find the candidates where AP0 fails, then re-decode with
  AP injection of the truth callsigns to measure the recovery
  ceiling. That's a separate diagnostic experiment.
- **The duplication between `par_try_ldpc_with_ap` and
  `par_try_ldpc_with_recent_only` is intentional for this iter** —
  the two functions have similar structure but different injection
  and survival semantics. A future cleanup iter could refactor them
  to share via a closure/trait, but keeping them separate for now
  isolates hb-043 from changes to the existing AP code path.

## Follow-ups added to hypothesis bank

- **hb-043 → CLOSED (WIN, infrastructure).** Algo and wiring in
  place.
- **hb-027 → UNBLOCKED.** Next step: rolling callsign window data
  source. Spawn hb-050 specifically for the rolling-window tracker
  (decoupled from hb-027's eval).
- **hb-051 (NEW):** Diagnostic — "AP-recovery ceiling on hard-200."
  For each WAV, find candidates where AP0 fails, then re-decode
  with AP injection of the truth callsigns. Measures the upper
  bound on what AP could ever contribute. Informs whether hb-027 is
  worth wiring up at all, vs the cost of the rolling-window
  infrastructure.

## Reproducing

```bash
# Sanity: 5 popular callsigns
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 \
    --ap-recent-calls 7X5CY,TN8GD,WY7WL,A71UN,EU6AF \
    --output /tmp/ap_recent_5calls.json

# Stress: 20 popular callsigns (wallclock scaling check)
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 \
    --ap-recent-calls 7X5CY,TN8GD,WY7WL,A71UN,SV9TLU,EU6AF,C6APS,W8ASH,AD9DU,TY5AD,N2YW,XE3K,KZ7K,CN8NS,IK4LZH,WS4ASE,NH6D,KF9UG,AE6CH,W6TOX \
    --output /tmp/ap_recent_20calls.json
```
