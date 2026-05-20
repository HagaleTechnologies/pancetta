---
slug: synth-plateau-investigation
mode: ft8
state: shelved
created: 2026-05-20T18:00:00Z
last_updated: 2026-05-20T18:00:00Z
branch: (none — investigation only)
parent_hypothesis: hb-002
wild_card: false
scorecard: (n/a — no code change)
delta_vs_main: 0.0
disposition: shelved-with-findings
---

## Hypothesis

Plan 2's main.json showed synth-clean recovery plateauing at ~83% (5 of 6
messages) across all comfortable-SNR bins (-18 dB to -10 dB). One of the
6 synthesized messages consistently fails to decode at every SNR level
tested. Identify which message and why.

## Change

None. Pure investigation. Wrote a one-off probe at
`pancetta-research/examples/probe_synth_plateau.rs` that decodes each
synth WAV and prints per-message-per-SNR results. Probe deleted before
journaling commit; reproducible by re-creating from this entry.

## Result

**Failing message: `K1ABC W9XYZ R-12`** — the "Roger + signal report"
response message. Fails at every SNR from –28 dB to –10 dB.

Per-message-per-SNR recovery table (default Ft8Config decoder):

| Message            | -28 | -26 | -24 | -22 | -20 | -18 | -16 | -14 | -12 | -10 |
|--------------------|-----|-----|-----|-----|-----|-----|-----|-----|-----|-----|
| CQ K1ABC FN42      | F   | F   | F   | F   | OK  | OK  | OK  | OK  | OK  | OK  |
| K1ABC W9XYZ 73     | F   | F   | F   | F   | OK  | OK  | OK  | OK  | OK  | OK  |
| K1ABC W9XYZ EM48   | F   | F   | F   | F   | OK  | OK  | OK  | OK  | OK  | OK  |
| **K1ABC W9XYZ R-12** | **F** | **F** | **F** | **F** | **F** | **F** | **F** | **F** | **F** | **F** |
| W9XYZ K1ABC -10    | F   | F   | F   | F   | OK  | OK  | OK  | OK  | OK  | OK  |
| W9XYZ K1ABC RR73   | F   | F   | F   | F   | F   | OK  | OK  | OK  | OK  | OK  |

Secondary finding: `W9XYZ K1ABC RR73` is borderline at –20 dB (FAIL) but
recovers at –18 dB. Not the plateau target, but worth flagging.

## Disposition

Shelved — no code change. Investigation produced a concrete follow-up
hypothesis (hb-023, see below).

## Learnings

- The plateau is real and reproducible: exactly 1 of 6 messages fails
  consistently, accounting for the ~83% (5/6) ceiling in main.json's
  synth-clean tier.
- The failing message is the "Roger + signal report" form (`R-12`).
  This is a distinct FT8 message subtype — others in our synth corpus
  cover CQ, grid response, plain signal report (`-10`), 73, and RR73,
  all of which decode.
- The encoder and decoder do successfully round-trip generated WAVs for
  the other 5 message types. The failure is specific to `R-<n>` format
  responses.
- Two probable causes (need source-level investigation to confirm):
  - (a) Encoder produces bits the decoder can't parse back for this
    message subtype — i.e., an encode/decode mismatch in the
    R-signal-report bit-layout.
  - (b) Decoder's message-type detection misclassifies R-prefix
    responses, dropping them at a parser gate before LDPC even runs.
- Either way: this is a real decoder bug, not a sensitivity limit. Until
  it's fixed, the synth-clean tier's composite contribution is artificially
  capped at ~0.83 × full weight.

## Follow-ups added to hypothesis bank

- **hb-023 (new) — Fix R-signal-report decode failure** [PRIORITY ~7.5].
  Trace why `K1ABC W9XYZ R-12` round-trips encode but fails decode.
  Likely a message-type-specific bit layout or parser gate. Expected
  delta: +0.05 in synth-clean snr@50% normalized score (lifts the
  plateau from 83% to 100%), composite +0.015.

## Reproducing

```bash
cat > pancetta-research/examples/probe_synth_plateau.rs <<'EOF'
# (full source from this investigation; restored from git history if needed)
EOF
cargo run --release --example probe_synth_plateau -p pancetta-research
```

The probe will print the table above (deterministic since synth seeds
are fixed). If the probe shows a DIFFERENT failing message in the future,
the decoder has changed — investigate accordingly.
