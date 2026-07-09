---
slug: w44-cross-cycle-content-guard
mode: ft8
state: shelved
created: 2026-07-09T00:00:00Z
last_updated: 2026-07-09T00:00:00Z
branch: worktree-decoder-tp-sensitivity
parent_hypothesis: decoder-tp-sensitivity plan Task W4.4 (spec §7)
wild_card: false
delta_vs_main: negligible/mixed-sign at the calibration threshold (0.3); real recall
  cost with no matching unverified-novel win at every higher threshold tested
disposition: DECLINE the default flip. `Ft8Config::cross_cycle_content_guard` stays
  `None`. Mechanism ships, gated, correctly implemented, and proven functionally
  correct by TDD — but the corpus the task brief named (chrono_replay) cannot
  exercise it at all (verified empirically), and against the corpus that actually
  does (hard-200/hard-1000), no threshold in a 0.0-0.95 sweep produces a net win
  under the plan's own precision-fix framing.
---

## Hypothesis

Task W4.4 (spec §7): `group_for_cross_cycle` groups cross-cycle candidates
GEOMETRICALLY ONLY (same freq grid position, t0 a slot apart, sync-score band) —
never checking whether two candidates actually carry the same message before
summing their tone energies. Spec cites a measured cost: "+8 spurious novel
decodes alongside +14 genuinely recovered ones." Add a cheap content guard
(LLR-sign correlation between the candidate and the group seed's independently
demodulated soft symbols) and calibrate a threshold (~0.3 starting point) that
cuts the spurious groupings without losing much of the genuine recovery.

## Correction to the task brief: chrono_replay cannot exercise this mechanism

The task brief instructed calibrating on `chrono_replay` ("the corpus this
mechanism actually exercises"). This is a **factual error**, verified empirically
before spending any A/B budget on it (same "verify-before-trusting-the-plan-text"
discipline as the W4.3 narrative correction earlier this workstream):

- Every `chrono_replay` manifest entry is an **independent single 15.0s WAV file**
  (verified via `wave.open(...).getnframes()/getframerate()` on the first entry —
  exactly 180,000 frames @ 12kHz = 15.0s). `group_for_cross_cycle` only ever
  groups candidates found within ONE `decode_window` call's candidate list (one
  spectrogram, one audio buffer). A single 15s FT8 slot recording contains at
  most one transmission cycle per station — there is no possible "repeat 188
  steps later" within one file's own buffer.
- `hard_200`/`hard_1000` (the corpus the ORIGINAL hb-056/hb-074/hb-075 cross-cycle
  measurements actually used, and where spec §7's own cited "+8 novel/+14
  recovered" number comes from) are **90-second multi-slot recordings** (verified
  the same way — 1,080,064 frames @ 12kHz = 90.0s ≈ 6 FT8 slots per file). This is
  the only corpus in the harness that can structurally exercise
  `group_for_cross_cycle` at all.
- Empirical proof: ran `chrono-replay` (mini33, 33 real WAVs) with
  `--no-cross-cycle-averaging` vs. the default (`cross_cycle_averaging=true`,
  i.e. the mechanism this task modifies fully engaged) — **every recall/novel
  field was byte-identical** (`truth_decodes_recovered=0/0`, `novel_decodes=828/828`,
  `novels_verified=650/650`, `novels_unverified=178/178`). The cross-cycle
  mechanism is a proven, total no-op on this corpus; testing a guard on its
  grouping step there would measure nothing by construction.

I substituted `curated-hard-200` + `curated-hard-1000` (the corpus the mechanism
was actually born and measured on) for the real A/B, consistent with this
workstream's established practice of verifying before trusting stale plan
prose (see the W4.3 narrative-correction file in this same directory).

## TDD (RED/GREEN)

Two new unit tests in `pancetta-ft8/src/decoder.rs` (`decoder::tests` module),
building a synthetic `Spectrogram` directly (private-field struct literal, same
pattern as existing Costas-kernel tests in the same file):

- `test_cross_cycle_content_guard_rejects_different_messages`: two
  `CostasCandidate`s at a geometrically compatible grid position (same
  freq_bin/freq_sub, `t0` exactly `SLOT_TIME_STEPS_FT8` apart, matching
  sync_score) backed by ENGINEERED maximally-different content — for every FT8
  data symbol, candidate A's dominant tone is gray index `j = sym_idx % 8`,
  candidate B's is the bit-complement `7 - j` (flips all 3 codeword bits' sign
  every symbol).
  - RED (pre-fix / `content_guard=None`): `group_for_cross_cycle` groups them
    anyway — `groups_no_guard.len() == 1` — reproducing the exact bug spec §7
    measured.
  - Sanity: `llr_sign_correlation(&pp, &spec, &a, &b)` returns `< -0.9`
    (near-total sign disagreement, confirming the engineered content really is
    maximally different, not a tautological setup).
  - GREEN (post-fix, `content_guard=Some((0.3, &pp, &spec))`): `groups_guarded`
    is empty — the pairing is rejected.
- `test_cross_cycle_content_guard_allows_same_message`: mirror case, both
  candidates use the SAME dominant-tone pattern (a genuine repeat). Correlation
  is `> 0.9`; the guard still groups them (`guarded.len() == 1`,
  `guarded[0]` contains both indices) — proves the guard doesn't break the
  legitimate case it's meant to protect.

Both new tests pass; the pre-existing `test_cross_cycle_grouping_and_linear_sum`
(updated to pass `None` at its one call site) is unchanged and still green.

## Implementation

- `Ft8Config::cross_cycle_content_guard: Option<f32>` (new field, default
  `None`) — `pancetta-ft8/src/decoder.rs`.
- `llr_sign_correlation(pp, spectrogram, a, b) -> Option<f32>`: independently
  demodulates each candidate's own grid position via the existing
  `par_extract_symbols_from_spectrogram` + `par_compute_soft_llrs_db` (no
  cross-contamination between the two candidates' extraction), then returns
  the mean sign-agreement over the 174 codeword-bit LLRs (`+1.0` = total
  agreement, `-1.0` = total disagreement).
- `group_for_cross_cycle` gained a `content_guard: Option<(f32, &ProtocolParams,
  &Spectrogram)>` parameter. When `Some`, every geometrically-compatible
  candidate `b` is additionally checked against the group's seed `a` (the first
  member accepted into the group — already the existing loop invariant, no
  restructuring needed) via `llr_sign_correlation`, and only accepted if the
  correlation is `>= threshold`. `None` preserves the exact pre-W4.4 code path
  (byte-identical — confirmed by both existing regression tests and the "guard
  vs. no-guard, `--no-cross-cycle-averaging`-off default config" byte-identical
  scorecard runs below).
- Both call sites updated: the production `cross_cycle_averaging_pass` method
  (constructs `content_guard` from `self.config.cross_cycle_content_guard`),
  and the one existing unit test (passes `None`).
- `pancetta-research/src/decoder.rs`: `with_cross_cycle_content_guard(Option<f32>)`
  builder, mirroring the existing `with_cross_cycle_coherent*` pattern.
- `pancetta-research/src/bin/eval.rs`: `--cross-cycle-content-guard <f32>` CLI
  flag (mirrors `--residual-min-sync-score`'s "outer `Option` tracks whether the
  CLI set it" pattern). Also added `--no-cross-cycle-coherent` (previously
  missing — only the `true`-setting flag existed) to support the non-coherent
  diagnostic run below; a small, permanent, in-pattern addition.

## A/B workflow

Binary: `cargo build --release -p pancetta-research --bin eval --features research-eval`.

### Corpus-correctness check (chrono-replay is a no-op — see above)

```
./target/release/eval --tier chrono-replay \
  --chrono-replay-manifest research/corpus/curated/ft8/chrono_replay_mini33.manifest.json \
  --mode ft8 --output /tmp/w44/mini33_ctrl.json --no-cross-cycle-averaging

./target/release/eval --tier chrono-replay \
  --chrono-replay-manifest research/corpus/curated/ft8/chrono_replay_mini33.manifest.json \
  --mode ft8 --output /tmp/w44/mini33_default.json
```
Result: `truth_decodes_recovered 0/0`, `novel_decodes 828/828`,
`novels_verified 650/650`, `novels_unverified 178/178` — byte-identical.
Confirms cross-cycle averaging (hence any content guard on its grouping step)
never engages on this corpus.

### Real A/B: `curated-hard-200` (against the ACTUAL current production
default: `cross_cycle_coherent=true`, `cross_cycle_coherent_mrc=true` — the
hb-074/hb-075 coherent MRC-weighted path, not the older plain hb-056
non-coherent variant spec §7's "+8/+14" number was measured against)

```
./target/release/eval --tier curated-hard-200 --mode ft8 --output ctrl.json
./target/release/eval --tier curated-hard-200 --mode ft8 --output guard000.json --cross-cycle-content-guard 0.0
./target/release/eval --tier curated-hard-200 --mode ft8 --output guard03.json  --cross-cycle-content-guard 0.3
./target/release/eval --tier curated-hard-200 --mode ft8 --output guard05.json  --cross-cycle-content-guard 0.5
./target/release/eval --tier curated-hard-200 --mode ft8 --output guard06.json  --cross-cycle-content-guard 0.6
./target/release/eval --tier curated-hard-200 --mode ft8 --output guard07.json  --cross-cycle-content-guard 0.7
./target/release/eval --tier curated-hard-200 --mode ft8 --output guard08.json  --cross-cycle-content-guard 0.8
./target/release/eval --tier curated-hard-200 --mode ft8 --output guard095.json --cross-cycle-content-guard 0.95
```

| threshold      | recovered | novel | verified | unverified |
|----------------|----------:|------:|---------:|-----------:|
| ctrl (`None`)  |      1251 |  4956 |     3161 |       1795 |
| 0.0            |      1251 |  4956 |     3161 |       1795 |
| **0.3 (brief's target)** | **1251** | **4955** | **3160** | **1795** |
| 0.5            |      1250 |  4953 |     3160 |       1793 |
| 0.6            |      1250 |  4953 |     3160 |       1793 |
| 0.7            |      1250 |  4949 |     3157 |       1792 |
| 0.8            |      1247 |  4940 |     3151 |       1789 |
| 0.95           |      1243 |  4925 |     3141 |       1784 |

**At 0.3, the effect is noise-level: recall unchanged, unverified-novel count
unchanged, verified-novel count down 1 out of 3161 (0.03%).** As the threshold
rises, recall and total novel count fall together, roughly in lockstep — there
is no operating point in this sweep where `unverified` drops meaningfully
without a proportional recall cost. At the strict end (0.95) the guard clearly
functions (proves it isn't silently vacuous): it costs 8 real recovered
decodes to cut only 11 unverified novels — a bad trade.

### Confirmation at 5x scale: `curated-hard-1000`

```
./target/release/eval --tier curated-hard-1000 --mode ft8 --output ctrl.json
./target/release/eval --tier curated-hard-1000 --mode ft8 --output guard03.json --cross-cycle-content-guard 0.3
./target/release/eval --tier curated-hard-1000 --mode ft8 --output guard05.json --cross-cycle-content-guard 0.5
./target/release/eval --tier curated-hard-1000 --mode ft8 --output guard08.json --cross-cycle-content-guard 0.8
```

| threshold | recovered | novel | verified | unverified |
|-----------|----------:|------:|---------:|-----------:|
| ctrl      |      1251 | 26769 |    18332 |       8437 |
| 0.3       |      1251 | 26770 |    18333 |       8437 |
| 0.5       |      1250 | 26769 |    18337 |       8432 |
| 0.8       |      1247 | 26684 |    18282 |       8402 |

At 0.3 the sign even flips (novel +1 vs. hard-200's -1) — confirms it's pure
noise at that operating point, not a real, reproducible effect. At 0.5/0.8 the
same pattern as hard-200 repeats: recall falls alongside novel counts, never a
clean unverified-only win.

### Diagnostic: does the guard work as designed in the regime the original bug
was measured in? (non-coherent-only, `--no-cross-cycle-coherent`, i.e. the
plain hb-056 config, NOT what ships today)

```
./target/release/eval --tier curated-hard-200 --mode ft8 --output noncoh_ctrl.json --no-cross-cycle-coherent
./target/release/eval --tier curated-hard-200 --mode ft8 --output noncoh_guard03.json --no-cross-cycle-coherent --cross-cycle-content-guard 0.3
```
`recovered 1148/1148`, `novel 4572/4573`, `verified 2923/2923`,
`unverified 1649/1650` — still no improvement at 0.3, even in the original
bug's own regime. (The TDD tests above independently prove the mechanism DOES
correctly reject engineered maximally-different content; the real corpus
apparently just doesn't contain enough total groups, or the mismatched ones
that do occur, at a correlation clearly below 0.3, to move the needle — the
whole "+8 novel" cost spans only a handful of WAVs out of 200/1000, so this is
a small-numbers regime regardless of coherent/non-coherent path.)

## Decision

**DECLINE the default flip.** `Ft8Config::cross_cycle_content_guard` stays
`None`. Reasoning:

1. **Corpus correction**: chrono_replay (the brief's named calibration corpus)
   cannot exercise this mechanism at all — verified empirically, not assumed.
   Hard-200/hard-1000 (the mechanism's actual birth corpus) is the only valid
   substitute, and that's what this A/B used.
2. **The bug's cost was measured against a component that isn't what ships.**
   Spec §7's "+8 novel/+14 recovered" is the hb-056 plain non-coherent number.
   Production ships `cross_cycle_coherent=true` + `cross_cycle_coherent_mrc=true`
   (hb-074/hb-075), which independently already drove the SAME mechanism's
   precision problem down to +22 recovered/+1 novel per its own graduation log
   — i.e. the residual spurious-grouping problem in the ACTUAL default
   configuration is already nearly solved before this task starts.
3. **Against the real default, no threshold in [0.0, 0.95] clears the bar.**
   0.3 (the brief's calibration target) shows a noise-level, sign-flipping,
   non-reproducible effect across the two corpora tested. Every threshold high
   enough to move `unverified_novels` meaningfully (0.7+) does so by cutting
   real recovered decodes in the same or greater proportion — never a clean
   precision-only win. This is exactly the failure mode a
   "reduce spurious novels while retaining genuine recovery" success bar is
   meant to catch.
4. **The mechanism itself is proven correct and shippable, just not currently
   useful as a DEFAULT.** The TDD tests prove it correctly separates engineered
   maximally-different content from engineered identical content. It's fully
   wired (config field, builder, CLI flag) for any future retest — e.g. if a
   future change to `cross_cycle_coherent_mrc`'s weighting reopens a precision
   gap this guard could then close, the lever is already in place with zero
   further plumbing.

This mirrors the W4.3 precedent in spirit (measure honestly against the regime
that actually matters, don't rubber-stamp a stale plan assumption) while
landing on the opposite conclusion for a different, well-supported reason: here
the *mechanism the task set out to fix* turns out to already be almost fully
fixed by earlier, independent work (hb-074/hb-075) before this task began.

## Full test suite

`cargo test --workspace --features transmit`: all green — every `test result:`
line across every workspace crate/binary/doctest shows `0 failed` (no
`FAILED` lines anywhere in the run log). Ran once after the implementation +
tests landed, before this log was written.

`cargo fmt -p pancetta-ft8 -p pancetta-research -- --check`: clean.
`cargo clippy -p pancetta-ft8 --features transmit --tests -- -D warnings`: clean.
`cargo clippy -p pancetta-research --lib --bins --tests --features research-eval -- -D warnings`: clean.

## Files changed

- `pancetta-ft8/src/decoder.rs`: `Ft8Config::cross_cycle_content_guard` field +
  default; `llr_sign_correlation` helper; `group_for_cross_cycle` signature +
  guard logic; production call-site wiring in `cross_cycle_averaging_pass`;
  2 new unit tests + 1 existing call-site update.
- `pancetta-research/src/decoder.rs`: `with_cross_cycle_content_guard` builder.
- `pancetta-research/src/bin/eval.rs`: `--cross-cycle-content-guard` +
  `--no-cross-cycle-coherent` CLI flags, struct field, wiring.

## Learnings / follow-ups

- **Always durability-check a plan's named corpus against the mechanism's
  actual code path before spending A/B budget on it** — a corpus name alone
  ("chrono_replay", "the cross-cycle corpus") doesn't guarantee the mechanism
  under test can structurally fire there. A 30-second `wave.open(...)` check on
  the first manifest entry would have caught this before any eval run.
- **A default flip's cost/benefit must be measured against the CURRENT
  compounding of prior defaults, not the mechanism's own historical
  introduction-time measurement.** Spec §7 cited a number from before
  hb-074/hb-075 shipped; re-deriving it against today's actual default would
  have caught the "almost solved already" finding without needing the full
  threshold sweep — worth remembering for any future spec-cited number in this
  plan that predates later-landed work in the same file.
- If a future change reopens a real precision gap in cross-cycle grouping (e.g.
  a change to the coherent MRC weighting, or a corpus with denser genuine
  repeats than hard-200/1000's ~7% incidence), `cross_cycle_content_guard` is
  fully wired and ready for a fresh calibration pass — no further plumbing
  needed, just re-run this same sweep methodology.
