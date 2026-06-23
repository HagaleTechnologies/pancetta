---
slug: exact-format-display-tests
mode: ft8
state: won
created: 2026-05-23T00:00:00Z
last_updated: 2026-05-23T00:00:00Z
branch: experiment/ft8/exact-format-display-tests
parent_hypothesis: hb-029
wild_card: false
scorecard: (n/a — pure test addition, no behavior change)
delta_vs_main: 0 (test-only)
disposition: WIN (regression net) — 9 new exact-format tests for every StandardMessageType variant
---

## Hypothesis

hb-023 (2026-05-21) caught a Display impl bug where ReportWithR
emitted "K1ABC W9XYZ R -12" with a stray space instead of the
canonical "K1ABC W9XYZ R-12". The bug had been silently present and
was only caught when the synth-eval text matcher couldn't find R-prefix
decodes. **No unit test had asserted the exact format of any
StandardMessageType variant.**

hb-029: add exact-format `assert_eq!` tests for every variant. Catches
the next hb-023-class bug at unit-test time instead of after a
multi-cycle eval miss.

## Change

Test-only — no production behavior change.

- `pancetta-ft8/src/message.rs`: added 9 new exact-format tests in
  the `message::tests` mod:
  - `test_cq_exact_format` — "CQ W1ABC FN42"
  - `test_cq_with_modifier_exact_format` — "CQ DX W1ABC FN42"
  - `test_reply_exact_format` — "K1ABC W9XYZ EM48"
  - `test_reply_with_r_exact_format` — "K1ABC W9XYZ R EM48"
  - `test_report_negative_exact_format` — "K1ABC W9XYZ -10"
  - `test_report_positive_exact_format` — "K1ABC W9XYZ +05"
  - `test_rrr_exact_format` — "K1ABC W9XYZ RRR"
  - `test_final73_exact_format` — "K1ABC W9XYZ 73"
  - `test_rr73_exact_format` — "K1ABC W9XYZ RR73"
  - (ReportWithR already covered by the 2 hb-023 tests)

Each assertion is `assert_eq!` (exact string match) — `.contains()`
wouldn't catch hb-023-style stray-whitespace bugs.

## Result

```
test result: ok. 189 passed; 0 failed; 0 ignored
```

Was 180 lib tests; now 189 (+9 new). All pass.

## Disposition

**WIN (regression net).** No production behavior change. The new
tests would have caught hb-023's R-prefix-with-space bug instantly
instead of needing a synth-eval discrepancy to surface it.

## Learnings

- **Three test classes catch three bug classes.** `.contains()`-based
  tests (the historical pattern in message.rs) catch "is something
  there?" bugs. `assert_eq!` catches "is it formatted correctly?"
  bugs. Eval-level round-trip tests (encode → modulate → decode →
  match) catch "does it survive the pipeline?" bugs. All three are
  needed; we now have all three for StandardMessageType.

- **The ReplyWithR test is the most interesting.** It asserts
  "K1ABC W9XYZ R EM48" — with a SPACE between R and EM48. That
  matches ft8_lib's reference (`vendor/ft8_lib/ft8/message.c:1104`
  uses `stpcpy(dst, "R ")` for the grid case). Different from
  ReportWithR which uses NO space ("R-12"). Easy to confuse — the
  test now locks both conventions.

- **Total of 11 exact-format Display tests** now cover every
  StandardMessageType variant. Future Display changes will fail
  these tests immediately.

## Follow-ups added to hypothesis bank

None. Pure cleanup. If a future i3=0 (DXpedition) or contest message
format becomes important, similar exact-format tests should be
added for their variants too — but those code paths are less
exercised in operational FT8.

## Reproducing

```bash
cargo test --features transmit -p pancetta-ft8 --lib \
    -- message::tests::test_.*_exact_format message::tests::test_report_with_r_display
```
