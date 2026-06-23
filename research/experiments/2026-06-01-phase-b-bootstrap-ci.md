---
slug: phase-b-bootstrap-ci
mode: meta
state: complete
created: 2026-06-01T13:00:00Z
last_updated: 2026-06-01T13:30:00Z
branch: iter/2026-06-02-phase-b-ci-helper
parent_hypothesis: engineering-audit Phase B — quantify the noise floor on per-tier delta claims
prior_session: research/ideation/2026-06-01-metric.md
wild_card: false
scorecard: n/a (tooling change, no decoder change)
delta_vs_main: zero (no decoder change)
disposition: LANDED — bootstrap CI helper live in `pancetta-research::bootstrap_ci`; wired into `compare` binary; new `per_wav_records` field on `TierResult` carries unbiased per-WAV input
---

## Headline

**Nonparametric bootstrap 95 % CIs now ship with every `compare` run.**
Per-tier recall / novel deltas come with a "NOT significant" tag when 0 lies
inside the CI — a hard guard against celebrating same-config rayon/OSD
noise as a graduation-worthy win.

## Motivation

Recent batches (12-16) graduated several mechanisms on hard-200 recall
deltas in the +3 .. +12 range. Those numbers have no error bars
attached. The dominant noise sources at that scale:

- **Rayon thread scheduling.** The decoder's per-WAV pipeline runs under
  rayon; tie-breaking in sync-candidate / OSD enumeration can land
  differently between runs at identical config.
- **OSD enumeration tie-breaks.** Equal-LLR error patterns differ by
  scan order; rare cases land on a different codeword.
- **Eval-time corpus drift.** WAVs occasionally get deleted between runs
  (curate script, disk-cap purges), and the baseline regeneration cycle
  changes truth counts on adjacent WAVs.

A single eval run cannot disambiguate "+5 recovered = real" from "+5
recovered = noise". Phase B provides the statistical instrument that
can.

## Design

For two scorecards A and B on the same tier:

1. Build per-WAV `(recovered_A[w], recovered_B[w])` table aligned by
   WAV hash. Same for novel.
2. Bootstrap: sample N (=1000) times with replacement, compute
   `Σ recovered_B − Σ recovered_A` on each resample.
3. Report mean, 2.5th / 97.5th percentile, `significant = 0 ∉ CI`.

Standard nonparametric bootstrap, deterministic on seed
(`StdRng::seed_from_u64`).

## Implementation

Three commits on `iter/2026-06-02-phase-b-ci-helper`:

1. `feat(research): bootstrap CI for per-tier recall/novel deltas`
   - `pancetta-research/src/bootstrap_ci.rs`: `DeltaCi`,
     `bootstrap_recall_delta`, `bootstrap_novel_delta`.
   - `pancetta-research/src/scorecard.rs`: new
     `PerWavRecord { wav_hash, truth, recovered, novel }` and
     `TierResult::per_wav_records: Vec<PerWavRecord>` (backwards-
     compatible — empty on older scorecards).
   - `pancetta-research/src/bin/eval.rs`: curated tiers emit
     `per_wav_records` for every WAV (not just top-20 failures).
   - 9 unit tests: identical inputs → CI contains 0; uniform shift →
     CI excludes 0; high-variance scatter → CI straddles 0 despite
     positive net delta; seed determinism; mismatched-length panic.

2. `feat(research): wire bootstrap CI into compare binary`
   - New `BOOTSTRAP CI:` section. Flags: `--no-bootstrap`,
     `--bootstrap-n`, `--bootstrap-seed`.
   - Aligns A/B by `wav_hash`; A-only / B-only WAV counts reported
     as caveat.
   - When neither side carries `per_wav_records`, prints an explicit
     "re-eval with Phase-B build" banner — no silent-skip.

3. `research(meta): Phase B — CI helper landed + smoke test`
   - This journal entry + RUNBOOK update.

## Smoke test

`compare research/scorecards/history/main.2026-05-30-pre-refresh.json
research/scorecards/main.json`

Both scorecards predate Phase B → no `per_wav_records` available →
explicit "skipped" banner emits. Demonstrates the safe-fallback path.

To exercise the CI itself we generated three synthetic scorecard pairs
(`/tmp/pancetta-phase-b-*.json`) with controlled per-WAV deltas:

| Scenario | Net Δ rec | 95 % CI | Verdict |
|----------|-----------|---------|---------|
| broad +1/+2 across many WAVs | +98 | [+77, +120] | **significant** |
| ~30 WAVs +1/+3, rest unchanged | +30 | [+9, +52] | **significant** |
| ~10 WAVs ±1, rest unchanged | +3 | [-6, +12] | **NOT significant** |

The +3 row is the critical case: a celebration-worthy "+3 hard-200 rec"
on the eval print is statistically indistinguishable from same-config
noise. Going forward, graduation requires `significant=true` on the
headline tier (or, for marginal cases, a corroborating significant
delta on hard-1000 or wild-50).

## Retroactive applicability to batch 16

The user's audit question: "what does the CI look like for the recent
+5 hard-200 wins?" We can't retroactively compute it on shipped
graduations because:

- `main.json` and the archived pre-graduation scorecards predate the
  Phase-B `per_wav_records` field.
- We'd need to re-eval each graduated branch's main-merge-base AND its
  graduated state with the Phase-B build to recover the per-WAV table.

That's a separate experiment — not in scope here. The right
forward-looking discipline:

1. Every future batch's eval scorecards carry `per_wav_records` by
   construction (eval already emits it on `iter/2026-06-02-phase-b-ci-helper`+).
2. Compare automatically prints the CI. If `NOT significant`, the
   default disposition becomes SHELVE not GRADUATE unless the
   mechanism's effect is corroborated elsewhere (synth tier, secondary
   recall metric, or a multi-seed re-run).

## Limitations

- Per-WAV records add ~50 KB to each scorecard for hard-200 (200
  records × ~250 bytes). Negligible for disk; sliver for git noise.
- Bootstrap assumes WAVs are exchangeable units. Reasonable for the
  curated tiers (independent QSOs from independent recording sessions)
  but breaks for highly correlated synthetic tiers — that's why
  bootstrap is gated to tiers with `per_wav_records` and synth/fixture
  tiers (which don't fit the bootstrap unit) are correctly skipped.
- 95 % CI is the convention here; tests support arbitrary N via
  `--bootstrap-n`. The percentile method is the simplest reasonable
  choice for bounded-integer deltas; BCa / studentized would be next
  steps if endpoints start mattering at finer precision.

## Follow-ups

- Wire bootstrap-CI gating into the auto-loop's graduate/shelve
  decision (`scripts/research-env.sh` or a wrapper). Currently the CI
  is informational only — the operator still makes the call.
- Add CI on the *composite delta* (= the headline scalar). Composite
  is a weighted sum of tiers; the natural bootstrap is over the
  same per-WAV resampling but propagated through composite weights.
- Re-eval the 27 graduated branches with the Phase-B build to get a
  retroactive significance audit. Expensive but valuable for the
  "session cumulative +0.014" claim.

## Status

LANDED on `iter/2026-06-02-phase-b-ci-helper`. Three commits, all
tests green. Ready to fold into the next batch's iter cycle.
