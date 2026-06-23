---
slug: ap-wiring-fix
mode: ft8
state: won
created: 2026-05-24T02:00:00Z
last_updated: 2026-05-24T02:00:00Z
branch: experiment/ft8/multipass-profile
parent_hypothesis: hb-004 (scoping)
wild_card: false
scorecard: /tmp/ap_k1abc.json
delta_vs_main: 0 composite (no decode delta — confirms wiring is correct AND no-hint AP never fires usefully)
disposition: WIN (infrastructure) — AP-wiring plumbed end-to-end through eval; hb-027 still needs an algo-level change (current AP requires my_call to be set)
---

## Hypothesis

hb-004 audit (2026-05-22) found that the eval harness calls
`Decoder::decode_window`, which constructs `ApContext::default()` →
`ap_active=false` → the AP1/AP2/AP3/AP4 code paths short-circuit.
**AP never fires in eval, by construction.**

Three hypotheses (hb-004, hb-017, hb-027) all depend on AP firing
in eval. Fix the wiring once and unblock all three.

## Change

### pancetta-research

- `pancetta-research/src/decoder.rs`:
  - Added `ap_context: Option<pancetta_ft8::ap::ApContext>` field on the
    research `Ft8Decoder` wrapper.
  - Added `with_ap_context(ctx)` builder.
  - `decode_wav` now branches on `ap_context.is_some()` and calls
    `pancetta_ft8::Ft8Decoder::decode_window_with_ap(samples, ctx)`
    when set; otherwise falls through to `decode_window` (production
    default — keeps every previous experiment bit-identical).

- `pancetta-research/src/bin/eval.rs`:
  - Added `--ap-my-call <CALLSIGN>` and `--ap-recent-calls <C1,C2,...>`
    CLI flags. When either is set, the eval builds an `ApContext`
    with the parsed values and threads it through the research
    decoder. Empty/unparseable callsigns emit a warning and are
    skipped.

No production code change. Production `decode_window` path is
unchanged; only the research harness gains the ability to opt-in.

### Sanity sweep

Baseline (no AP flags) and `--ap-my-call K1ABC` on curated-hard-200:

| Config         | composite | recovered | novel | elapsed |
|----------------|----------:|----------:|------:|--------:|
| baseline       |  0.254489 |     4365  |   952 |   255s  |
| --ap-my-call K1ABC | 0.254489 |  4365  |   952 |   233s  |

Identical decode counts. **This is the expected result and confirms
the wiring is correct:** K1ABC isn't in hard-200's addressee set,
so AP1 (inject my_call at bits 28-55) never produces a candidate
that AP0 didn't already; AP2/AP3/AP4 require recent_calls or
active_qso (also empty); so AP gracefully no-ops without changing
the decode set. Wallclock variation (-22s) is within shared-machine
noise.

## Disposition

**WIN (infrastructure).** AP code paths now reachable from the
research harness. hb-004 unblocked for sensitivity sweeps; hb-017
unblocked for caller-pool experiments; hb-027 partially unblocked
but needs an additional algo change.

## Learnings

- **The current AP design assumes "I'm operating and I know my own
  call."** The AP1/AP2/AP3 branches in `par_try_ap_decode` all
  short-circuit `if ctx.ap_context.my_call.is_none()`. AP2's
  `recent_calls` injection requires my_call to also be set. This
  means hb-027 ("rolling callsign window from prior slots") needs
  either (a) a synthetic my_call for evaluation purposes or (b) an
  algo-level refactor to allow callsign-prior injection without
  knowing the operator's own call. The (b) path is more honest
  about the actual use case (scanning + biasing toward observed
  callsigns regardless of address-to).
- **Sanity-check with a no-match scenario is the right first move
  for new infrastructure.** Setting my_call=K1ABC (a callsign
  unlikely to be addressed in arbitrary corpus WAVs) and observing
  zero decode delta validates that (1) the wiring works, (2) the
  AP code paths don't randomly hallucinate extra decodes when they
  have nothing to match, and (3) the previous "baseline" results
  weren't accidentally invoking AP via some other path.

## Follow-ups added to hypothesis bank

- **hb-004** unblocked. Can now run AP-gate threshold sweeps with
  an ApContext that triggers AP1 firing (e.g., my_call set to a
  callsign known to be addressed in the corpus).
- **hb-017** unblocked. Can populate recent_calls with a callsign
  list and measure AP2 firing rates.
- **hb-027 partial.** Spawned [[hb-043-ap-mycall-less-injection]]
  (NEW): refactor AP to allow callsign-prior injection without
  requiring my_call to be set. This is the actual hb-027
  precondition.

## Reproducing

```bash
# Baseline (no AP)
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 --output /tmp/baseline.json

# AP on with synthetic my_call
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 \
    --ap-my-call K1ABC --output /tmp/ap_k1abc.json

# Future: AP with both my_call AND recent_calls
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 \
    --ap-my-call YOURCALL --ap-recent-calls K1ABC,W9XYZ,DL5XYZ \
    --output /tmp/ap_with_hints.json
```
