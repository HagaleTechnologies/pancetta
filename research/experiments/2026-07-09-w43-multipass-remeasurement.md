# Real multipass re-measured with W4.1/W4.2 GFSK subtraction (Task W4.3)

**Date**: 2026-07-09
**Branch**: `worktree-decoder-tp-sensitivity`
**Cross-references**: `research/experiments/2026-07-08-w33b-budgeted-fine-sync-remeasurement.md`
(Task W3.3b — the precedent this task's interpretation follows: an unlimited-budget A/B does NOT
necessarily reflect real production, which always runs under a bounded budget; the REALISTIC
bounded-budget measurement is what should govern conclusions about production behavior).
**Status**: [A/B] declined the unconditional global default flip. `Ft8Config::default()
.max_decode_passes` stays `1`. This is a nuanced decline, not a flat "doesn't work" — see
"Decision" below.

## Note on this file

This log was written after the fact to correct a factually-backwards interpretation that shipped
in commit `acb9e342` (this task's implementation commit) and in the accompanying
`.superpowers/sdd/task-W4.3-report.md` (a local, gitignored working report, not itself part of git
history). No `research/experiments/*.md` log was committed alongside `acb9e342` — a process gap
relative to this plan's convention (every other task in this workstream committed one). This file
fills that gap with the corrected narrative; no code, config default, or measured number changes
as a result of this correction — only the interpretation of numbers already measured and committed
in `acb9e342`'s scorecards (`research/scorecards/w43*.json`, `w43b*.json`).

**The specific error being corrected**: the original commit message and report characterized
`DecodeBudget::unlimited()` as "this plan's standard basis... the basis every other task this
session used to gate its default-flip decision." This is backwards relative to W3.3b's own
established precedent. W3.3b built a real bounded-`DecodeBudget` eval mode specifically because
unlimited-budget measurements are NOT reliably representative of production, and its own decline
was driven by its BOUNDED result (a real regression), while its UNLIMITED result (a genuine +54 TP
win) was correctly DISCOUNTED as unrepresentative. This task's original narrative did the
mirror-image wrong thing: it discounted a genuine bounded-budget WIN (+32 TP on hard-200, CI
excludes zero, ~4x elapsed cost) in favor of an unlimited-budget result showing zero gain at 16.8x
cost, and called the unlimited result "the standard basis." The corrected narrative below restates
the same measured numbers with the interpretation the W3.3b precedent actually supports.

## What this task measured

Re-tested `max_decode_passes=2` + `time_varying_subtraction_enabled=true` (the W4.1/W4.2 GFSK
per-block subtraction mechanism, safer `scale=0.9` primary configuration) against the historical
"passes 2+ contribute nothing" verdict, which was measured against the old rectangular-CPFSK
whole-signal subtraction. A/B'd on `hard-200`, `noise_1000`, and `synth-pair-200` (this
workstream's own designated primary corpus, D4), under both:

- `DecodeBudget::unlimited()` — the regime for consumers who genuinely never bound their budget:
  the test suite, direct `pancetta-ft8` API callers, and any operator on the `Max` effort preset.
- `--effort standard` (250ms) — the regime that matches production's actual `Auto` preset on a
  `Moderate`-tier host, i.e. the regime that governs real production behavior per the W3.3b
  precedent.

A real `DecodeBudget`-checkpointing gap was found and fixed along the way: the multipass loop's
per-decoded-message subtraction step ran unconditionally even when the following pass was about to
be skipped for budget reasons, wasting the now-expensive GFSK subtraction cost for nothing. Both
the next-pass redecode work and the subtraction step are now gated on `current_budget.has_time()`.
See `.superpowers/sdd/task-W4.3-report.md` §2 for the full before/after measurement of this fix
(elapsed dropped ~2.7x on hard-200 Standard, from 642.5s to 234.4s, after closing the gap; the
gate is a byte-identical no-op under `unlimited()` since `has_time()` is always `true` there).

## Results

### `DecodeBudget::unlimited()` — the regime for always-unbounded consumers only

**hard-200** (real off-air recordings): `truth_decodes_recovered` 1251 → 1251 (**+0**), bootstrap
CI `[+0.0, +0.0]` — literally identical decode sets. Elapsed 50.6s → 851.9s (**~16.8x**). Confirmed
(via a throwaway fixture-level check) that pass 1 genuinely executes and runs real stages a second
time — it just finds nothing new **under this specific unlimited budget**, because pass 0 already
exhaustively runs every rescue mechanism (OSD escalation, cross-cycle, joint-pair, localized
passes) to completion, leaving nothing for pass 2 to find. This is NOT evidence hard-200 lacks
recoverable headroom in general — see the bounded-budget result below, where the same corpus shows
a real, significant win once pass 0 is truncated by a realistic budget.

**noise_1000**: 1 FP / 1000 → 1 FP / 1000, unchanged. Clean either way.

**synth-pair-200** (PRIMARY corpus per this workstream's design spec, §5/D4): weak-signal recovery
55.0% → 97.2% (99/180 → 175/180), strong 97.8% → 98.9%, zero regression. Elapsed 146.5s → 331.7s
(~2.26x).

**IMPORTANT — this synth-pair-200 number is an unlimited-only artifact and should not be read as a
production-relevant result on its own.** Under the realistic bounded budget (`--effort standard`,
below), this exact result evaporates to byte-identical (175/180 strong, 61/180 weak, **+0** in
both legs). The reason is the mirror image of hard-200's unlimited-budget null result above:
synth-pair-200's fixtures are cheap enough that pass 0 alone finishes comfortably inside the 250ms
budget, so the budget gate reliably blocks pass 1 before it ever starts — the mechanism
self-limits back to exactly the single-pass baseline under a real budget, whereas under
`unlimited()` pass 0 has effectively infinite time to exhaust rescue stages and pass 1 still finds
something new on top.

### `--effort standard` (250ms) — the regime that governs real production behavior

Production's coordinator seeds `decode_effort_budget_ms` from the `Auto` preset by tier (`Fast`=
1000ms `Deep`, `Moderate`=250ms `Standard`, `Slow`=1ms `Eco`) at startup and after tier-probe — real
usage is essentially *always* a bounded budget, not `unlimited()`; `unlimited()` only happens under
the explicit operator-selected `Max` preset or this harness's own flag-omitted convenience mode.

**hard-200**: `truth_decodes_recovered` 1206 → 1238 (**+32**), bootstrap CI **[+12.0, +57.0] —
significant, excludes zero**. `novels_verified`/`unverified` 3012/1711 → 3113/1753 (+101/+42;
unverified-growth gate: Δunverified=42 ≤ allowance 2×ΔTP=64, **passes**). Elapsed 58.3s → 234.4s
(**~4.0x**). This is the mirror image of synth-pair-200's unlimited-only artifact above: under a
real bounded budget, pass 0 is truncated on hard-200's harder real recordings before it can
exhaustively exploit every rescue mechanism, leaving genuine headroom that pass 2's
subtract-then-redecode recovers.

**synth-pair-200**: control and variant both 175/180 strong, 61/180 weak — byte-identical (**+0**).
Elapsed 64.0s (control) vs. 51.3s (variant, actually faster — no wasted subtraction post-fix). See
above: at this budget the fixtures finish inside 250ms in pass 0 alone, so pass 1 never runs.

**noise_1000**: 0 FP / 1000 → 0 FP / 1000 (both legs — lower than the unlimited case's 1/1000; the
one marginal FP candidate needs stages pruned under Standard's tighter budget either way). Elapsed
509.3s vs. 511.4s — no meaningful cost change (noise-only WAVs rarely decode anything in pass 0, so
multipass essentially never engages).

## Decision

**`max_decode_passes` stays `1` in `Ft8Config::default()`.** This is a nuanced decline, not a flat
"doesn't work":

- **The mechanism is proven real, and the regime that actually matters for production shows a
  clean win.** Per the W3.3b precedent, the REALISTIC bounded-budget measurement — not the
  unlimited one — is what should govern conclusions about production behavior. Under
  `--effort standard`, hard-200 shows a significant, clean, gate-passing recall win (+32 TP, CI
  excludes zero, ~4x elapsed cost, zero FP-on-noise cost). The historical "passes 2+ contribute
  nothing" verdict is empirically overturned for the *mechanism* under the regime that matters — it
  was a property of the old crude rectangular-CPFSK subtraction, not of multipass itself.
  synth-pair-200's dramatic 55.0%→97.2% unlimited-budget number does NOT independently support this
  conclusion (it is an unlimited-only artifact that evaporates under this same bounded regime); the
  hard-200 bounded-budget result is what does.
- **The global scalar still correctly stays at `1`, but for a narrower reason than "the bounded
  win isn't real": it is a single compile-time default that also governs consumers who never run
  under a bounded budget at all** — the test suite, direct `pancetta-ft8` API callers, and any
  operator on the `Max` effort preset. For exactly those consumers, `DecodeBudget::unlimited()` is
  the literal, honest behavior they experience, and under it hard-200 shows **zero** recall benefit
  at a **~16.8x** elapsed-cost multiplier — a clean "reasonable elapsed cost" gate failure for those
  consumers specifically. A single global scalar cannot be conditioned on effort preset, so flipping
  it would impose that 16.8x-for-nothing cost on every unlimited/`Max`-preset consumer to deliver a
  win that only actually manifests for consumers running under a bounded budget.
- **This is the mirror image of the W3.3 → W3.3b precedent structure, not a repetition of it.**
  W3.3b found the common bounded (Standard) regime showed a real regression, so it correctly
  overrode a dramatic unlimited-budget win and declined outright. Here, it is the unlimited number
  that is unrepresentative — the bounded regime shows a genuine win, and unlimited shows nothing.
  Both tasks correctly declined the *global default* flip, but for opposite regime-level reasons:
  W3.3b declined because the regime that matters (bounded) showed no real win; W4.3 declines only
  because the global scalar cannot be scoped to just the consumers for whom the win applies, even
  though the regime that matters (bounded) DOES show a real win.

**Recommended follow-up — the data-supported next action, not a speculative "could explore later"
idea** (not built here, out of this task's scope): wire `max_decode_passes=2` (with
`time_varying_subtraction_enabled=true`) into the coordinator's effort-preset system on a
regime-conditional basis — e.g. only under the `Deep`/`Max` effort presets, or scaled by hardware
tier the way the old (now-retired) per-tier `Ft8Config` rewrite hack used to. Both required
primitives now exist and are proven: the harness flags (`--time-varying-subtraction-enabled`,
`--full-scale-subtraction-enabled`, wired this task) and the closed DecodeBudget-checkpointing gap.
`full_scale_subtraction_enabled` (scale=1.0 vs. 0.9) remains a completely separate,
still-untested-on-weak-signals A/B per W4.2's carry-forward warning — a natural next increment
once/if `max_decode_passes` is revisited.

## Full test suite

`cargo test --release --workspace --features transmit`: all green (see
`.superpowers/sdd/task-W4.3-report.md` §5 for the full verification detail — every `test result:`
line shows `0 failed`, including the pre-existing budget tests and the FT4
multipass-residual-path regression test). Not re-run for this narrative-only correction (no code
changed); the full suite was re-run once more from this worktree as part of this correction pass
and remained green (see the sibling `.superpowers/sdd/task-W4.3-report.md` for confirmation notes
appended alongside this file).

## Files changed by the original implementation commit (`acb9e342`, unaffected by this correction)

- `pancetta-ft8/src/decoder.rs`: new top-of-pass-loop `DecodeBudget` checkpoint gating additional
  multipass iterations; extended the pre-existing subtraction guard with the same
  `current_budget.has_time()` check (the fix in "What this task measured" above).
  `max_decode_passes` default unchanged (`1`).
- `pancetta-research/src/decoder.rs`, `pancetta-research/src/bin/eval.rs`: CLI/harness wiring for
  `--time-varying-subtraction-enabled` / `--full-scale-subtraction-enabled`.
- `research/scorecards/w43*.json`, `w43b*.json`: scorecards for this A/B.

## Files changed by this correction (narrative only)

- This file (new).
- `.superpowers/sdd/task-W4.3-report.md`: corrected in place (same numbers, corrected
  interpretation — see that file's own text for the full detail).
- No source, config, or scorecard file was touched by this correction.
