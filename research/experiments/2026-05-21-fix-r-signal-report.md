---
slug: fix-r-signal-report
mode: ft8
state: won
created: 2026-05-21T00:00:00Z
last_updated: 2026-05-21T19:54:53Z
branch: experiment/ft8/fix-r-signal-report
parent_hypothesis: hb-023
wild_card: false
scorecard: research/scorecards/history/2026-05-21-fix-r-signal-report.json
delta_vs_main: +0.0279
disposition: WIN — graduate to main
---

## Hypothesis

The synth message `K1ABC W9XYZ R-12` (Roger + signal report) round-trips
through the FT8 encoder but fails to decode at every SNR from -28 dB to
-10 dB. Other message subtypes (CQ, grid, plain signal report, 73, RR73)
decode at SNR ≥ -18 dB. Either an encoder/decoder bit-layout mismatch
or a message-type parser gate dropping R-prefix responses.

Expected delta: +0.05 in synth-clean snr@50% normalized score (5/6 → 6/6),
composite +~0.015.

## Change

Single-line fix in `pancetta-ft8/src/message.rs::Display` for
`StandardMessageType::ReportWithR`. The formatter previously emitted
`" R"` then `" {:+03}"`, producing `"K1ABC W9XYZ R -12"` with an extra
space between `R` and the signed report. Reference ft8_lib
(`vendor/ft8_lib/ft8/message.c::unpackgrid`) writes a bare `'R'`
immediately followed by the signed report — `"R-12"`, no space. Aligned
pancetta to the reference convention.

Two new unit tests asserting the canonical format
(`test_report_with_r_display_no_space_before_report` and
`test_report_with_r_display_positive_report`). The decoder structurally
decoded R-prefix messages correctly the whole time — only the text
representation was wrong, so eval text-match (`d.message.contains(truth)`)
was missing every R-report decode.

Follow-up cleanups in the same commit:
- `pancetta/tests/loopback_qso.rs` — removed the dual-format assertions
  and outdated "decoder formats as 'R -12'" comments. The test now
  asserts the canonical "R-12" form only.
- `pancetta-qso/src/exchange.rs` — updated the regex comment (regex
  itself unchanged; it already permissively accepts both forms).

## Result

**Composite: 0.4955 → 0.5234 (+0.0279)**, ~2× the expected delta. No
regressions.

| Tier             | Metric          | Main   | Branch | Δ        |
|------------------|-----------------|--------|--------|----------|
| synth-clean      | -20 dB recovery | 4/6    | 5/6    | +1       |
| synth-clean      | -18 to -10 dB   | 5/6    | 6/6    | +1 each  |
| synth-clean      | SNR@50% (db)    | -20.0  | -20.0  | 0        |
| curated-hard-200 | decode_rate     | 0.3911 | 0.4468 | +0.0557  |
| curated-hard-200 | recovered       | 3354   | 3832   | +478     |
| curated-hard-200 | novel           | 1154   | 676    | -478     |
| curated-hard-1000| decode_rate     | 0.3714 | 0.4214 | +0.0500  |
| curated-hard-1000| recovered       | 10437  | 11843  | +1406    |
| curated-hard-1000| novel           | 3720   | 2314   | -1406    |
| wild-50          | decode_rate     | 0.0    | 0.0    | 0        |
| fixtures         | pass_rate       | 1.0    | 1.0    | 0        |

`recovered + novel` is conserved on both curated tiers (+478 - 478,
+1406 - 1406): no new decodes happened. The same correctly-decoded
messages simply shifted from `novel` (text didn't match the jt9 truth
string) to `recovered` (text now matches) because the formatting bug
no longer suppresses the match.

## Disposition

**WIN.** Promote to main. The synth plateau is fully lifted; the
curated tiers gained ~5 absolute percentage points each at zero
algorithmic cost; no regressions.

## Learnings

- **Text-match-based eval can mask correctness wins.** The decoder was
  correctly decoding ~1900 R-prefix messages across the curated corpus
  for as long as it has supported i3=1 standard messages — the matcher
  just couldn't see them because the Display impl inserted a stray
  space between `R` and the signed report. Pancetta's "novel" decode
  count on Hard-200/1000 was inflated by ~1900 phantom-novel decodes
  that were actually true positives jt9 also found.

- **Pancetta's true vs_wsjtx_pct was always higher than measured.**
  Hard-200 was 44.7% (not 39.1%); Hard-1000 was 42.1% (not 37.1%). This
  retrospectively recalibrates the "5-10% of WSJT-X decode rate"
  characterization in memory — that number is for the autonomous run,
  not the harness, but the harness baseline was also undercounting.

- **The bug also distorts hb-024's premise.** hb-024 wanted to
  cross-validate novel decodes as a precision check. ~50% of the
  pre-fix novel decodes were not "novel" at all — they were
  formatting-mismatched true-positives. The remaining novel count
  (676 on Hard-200, 2314 on Hard-1000) is a better target for
  cross-validation work.

- **The Display impl is now a candidate for stricter testing.** Until
  this experiment, no unit test asserted the exact text format of any
  Standard message subtype — they just used `.contains()` for callsign
  and grid fragments. Adding exact-format tests for `Reply`,
  `ReplyWithR`, `Report`, `Rrr`, `Final73`, `RR73`, and the
  EU-VHF / DXpedition i3=0 variants would catch the next analogue of
  this bug.

- **Reference ft8_lib remains the canonical formatting source.** When
  a Display difference is suspected, vendor/ft8_lib/ft8/message.c
  unpackgrid/unpack_text and pack_basecall are authoritative.

## Follow-ups added to hypothesis bank

- **hb-029 (new)** — Exact-format Display tests for every standard /
  i3=0 / contest message subtype. Audit each path against ft8_lib
  reference output. Low effort, high regression-safety value.
  Estimated effort: 1 session. Priority ~0.45.

## Reproducing

```bash
# Unit tests (asserts the fixed format):
cargo test --features transmit -p pancetta-ft8 -- test_report_with_r_display

# Loopback (integration):
cargo test -p pancetta --test loopback_qso --features pancetta-ft8/transmit

# Full eval (1100+ WAVs, ~10 min on the dev box):
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures,synth-clean,curated-hard-200,curated-hard-1000,wild-50 \
    --mode ft8 --output research/scorecards/fix-r-signal-report.json
cargo run --release -p pancetta-research --bin compare -- \
    research/scorecards/main.json research/scorecards/fix-r-signal-report.json
```
