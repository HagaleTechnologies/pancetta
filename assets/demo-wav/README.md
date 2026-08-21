# Demo WAV sequence

Four consecutive 15-second FT8 captures (2017-09-23, 08:20:00–08:20:45 UTC),
copied from `pancetta-ft8/tests/fixtures/wav/basicft8/`. Kept as a separate
copy here (rather than referencing the test fixtures directly) so the demo
recording doesn't change if the decoder test corpus does.

A fifth file, `live_now.wav` (same source directory, same format: 16-bit
mono 12kHz, ~15s), was added because the four numbered files above are
documented as non-decoding content — see
`pancetta-ft8/tests/ft8lib_seed_tests.rs:362`
(`seeded_decode_is_a_noop_on_pass_gt_zero`), which excludes
`basicft8/170923_082000.wav` from its sweep specifically because it
"decode[s] nothing at all". `live_now.wav` does not have that problem:
`pancetta-ft8/tests/wav_decode_tests.rs`
(`unlimited_budget_never_skips_s4_s7_stages_in_report`, ~line 802) reads it
as a single 15-second decode window and asserts the decoder's S4
cross-cycle-averaging stage reaches a nonzero decode count under
`Ft8Config::default()` with an unlimited budget — i.e. one window is
sufficient, no multi-slot accumulation required. It sorts alphabetically
after the four numbered files (`l` > `1`) so it plays last in the replay
sequence, after the earlier files have already demonstrated realistic
multi-slot replay behavior.

Used as input to `pancetta --replay assets/demo-wav` when recording the
README's demo GIFs (see `.tapes/`).
