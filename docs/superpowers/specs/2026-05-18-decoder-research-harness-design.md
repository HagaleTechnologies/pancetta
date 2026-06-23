# Decoder Research Harness

**Date:** 2026-05-18
**Author:** K5ARH (with Claude)
**Status:** Draft — pending review

## Goal

Set up a framework that lets us iteratively improve the pancetta
decoder until we beat WSJT-X (–24 dB) and JTDX (–26 dB) — and keep
going. Built around an experiment-per-branch workflow with a
persistent hypothesis bank, a composite scorecard, and a Claude-driven
loop that turns ideas into tested, journaled changes. FT8 first, but
mode-agnostic so FT4 / JS8Call / JT9 / JT65 / MSK144 plug in later
without re-architecture.

The harness runs locally on the operator's Mac M4. Compute can burst
to other in-house machines later via rsync + SSH; the design supports
this without requiring it. **All iteration is local-only — never in
GitHub Actions** (we don't want to burn our Actions minutes on
research compute).

## Non-Goals

- Web UI, dashboard, or background daemon. Everything is files +
  CLI binaries + markdown journals.
- Database. JSON scorecards + markdown journals on disk; git is the
  history.
- CI integration. The research crate is excluded from `default-members`
  and never wired into `.github/workflows/`.
- Headless Claude execution. The loop runs inside a normal Claude Code
  session so we use the subscription, not API tokens.
- Slack / email / push notifications.
- A complete autoresearch reimplementation. Inspired by
  [karpathy/autoresearch](https://github.com/karpathy/autoresearch),
  but we don't replicate it.
- Non-FT8 modes in v1. The seams are there; the impls aren't.

## Success Criteria

The harness is working when:

1. Claude can run one full experiment cycle end-to-end (propose →
   implement → eval → decide → journal) in a single normal Claude
   Code session.
2. A new operator (or future-Claude on a fresh checkout) can read
   `research/experiments/` and `research/scorecards/` and understand
   what has and hasn't worked, without external context.
3. The composite scorecard ranks pancetta vs WSJT-X and JTDX on the
   same WAVs, with apples-to-apples baselines cached.
4. Repo size stays bounded (<500 MB committed); on-disk artifacts
   stay under 100 GB with auto-cleanup; no large files leak into git
   history.
5. The shipping decoder (`pancetta-ft8`) is unchanged in behavior
   until an experiment lands on main. The research crate is a sibling,
   not a replacement.

## Composite Metric

Each scorecard reports many sub-metrics. The single ranking number is:

```
score = 0.50 * real_decode_rate_hard_200            # already in [0, 1]
      + 0.30 * normalized_snr_clean                 # map [-30, -10] dB → [1, 0]
      + 0.15 * fixtures_pass_rate                   # already in [0, 1]
      + 0.05 * normalized_snr_doppler               # same map as clean
```

Where `normalized_snr = clamp((-actual_snr_db - 10) / 20, 0, 1)`. A
decoder hitting 50 % recovery at –20 dB scores 0.5; at –30 dB scores
1.0; at –10 dB scores 0.0. Exact normalization is an implementation
detail; the plan can refine it.

Hard rule: if `fixtures_pass_rate` drops below the main-branch
baseline, the experiment is auto-flagged as a regression. It can
still merge, but only with an explicit `--accept-fixture-regression`
flag and a paragraph in the journal explaining why.

Weights are tunable but the *change* itself is a meta-experiment that
must be journaled like any other.

## Architecture

### New crate + new directory

```
pancetta-research/             # new Rust crate; excluded from default-members
  src/
    bin/
      eval.rs                  # run decoder over corpus tiers; emit scorecard.json
      compare.rs               # diff two scorecards
      leaderboard.rs           # rank all scorecards by composite metric
      curate.rs                # rank real WAVs by "interesting-ness"
      baseline.rs              # run jt9 / JTDX CLI over corpus; cache decodes
    lib.rs                     # shared: corpus loaders, scorecard schema, metrics,
                               # DecoderUnderTest trait

research/                      # not a crate; plain markdown + JSON + manifests
  hypothesis_bank.md           # ranked list of ideas; Claude reads/writes each iteration
  experiments/                 # one .md per experiment branch
  baselines/                   # cached jt9/JTDX decodes per WAV (committed, small)
    <mode>/                    # ft8/ today; ft4/ js8/ later
  corpus/
    curated/
      <mode>/                  # ft8/ today
        hard_200.manifest.json
        hard_1000.manifest.json
        wild_50.manifest.json
        in_repo/*.wav          # ~30 reproducibility WAVs (~10 MB)
    fixtures/                  # links/references to pancetta-ft8/tests/fixtures/wav/
    synth/
      manifests/*.json         # committed; tiny; reproduce wavs byte-for-byte from seed
      wavs/                    # gitignored: generated audio; regenerable from manifest
  scorecards/
    main.json                  # current main-branch scorecard (the bar to beat)
    history/                   # all past scorecards (merged + shelved); leaderboard reads here
      YYYY-MM-DD-<slug>.json   # date-slug name (branch names get deleted on shelve)
    <branch>.json              # in-progress per-experiment scorecard (on its branch)

training/                      # existing — neural_osd lives here; more siblings later
  neural_osd/
  <future_model>/

scripts/
  research-env.sh              # preflight, audit, cleanup, pin, status
```

### CI exclusion

- Workspace `default-members` omits `pancetta-research`. `cargo build`
  / `cargo test` from the root skip it.
- No `.github/workflows/*.yml` file references `pancetta-research`,
  `research-env.sh`, `eval`, `compare`, `leaderboard`, `curate`, or
  `baseline`.
- `scripts/research-env.sh --guard-ci` greps `.github/workflows/` and
  fails the build if any of those names appear. `scripts/check.sh`
  invokes this guard as one of its steps. Belt + suspenders.
- Research-eval tests live behind `#[cfg(feature = "research-eval")]`;
  the feature is never enabled by default-CI test passes.
- `pancetta-research/README.md` opens with a loud "local-only" banner.

### On-disk lifecycle

| Surface | Cap | Action at threshold |
|---|---|---|
| `research/` tracked in git | 500 MB total | Audit flags; oldest baseline caches re-derivable, can be purged |
| `~/.pancetta/research_artifacts/` + `research/corpus/synth/` + `training/*/data/` (none in git) | **100 GB total** | Soft warn at 80 GB, hard pause loop at 95 GB |

Layout outside the repo:

```
~/.pancetta/research_artifacts/
  weights/<experiment>/         # large model weights (.pt, .safetensors)
  training_data/<dataset>/      # large .npy files
  synth_corpus/<config_hash>/   # generated synthetic WAVs (key = synth config)
  baseline_cache/               # raw jt9/JTDX stdout (input to the small JSONs we commit)
```

Cleanup rules:

| Event | Action |
|---|---|
| Experiment merges to main | Mark its weights "promoted" if shipped via baked Rust array; else purge in 30 days |
| Experiment shelved | Purge all on-disk artifacts in 14 days. The journal markdown stays on main forever |
| Pinned manually | `research-env.sh --pin <experiment>` overrides purge |
| Synth corpus regen | Cache keyed by `(generator_version, seed, snr_distribution_hash)`. Reuse if key matches; LRU oldest unused when approaching cap |
| Training data | Same LRU + cache-key rule as synth |

**Preflight alarms.** Every harness invocation (`eval`, `baseline`,
loop start) runs `scripts/research-env.sh --preflight`:

1. Report current usage across all 4 buckets.
2. Print "OK" if total < 80 GB.
3. Print "WARN: cleaning N artifacts" and run scheduled purges if 80–95 GB.
4. Print "STOP: 95+ GB, pausing — run --cleanup manually" if ≥ 95 GB
   and refuse to proceed.

Also runs `git rev-list --objects --all | sort -k2` against known-large
patterns and yells if any blob in git history exceeds the cap.

### Repo hygiene rules

- **No WAVs in git** except the ~30-WAV in-repo reproducibility set
  (~10 MB).
- **No .npy in git.** Training data is gitignored; the generator
  script + seed reproduces it byte-for-byte.
- **No research-only model weights in git.** Shipping weights are
  baked into Rust source (current `neural_osd_weights.rs` pattern);
  research weights live outside the repo.
- **Branch hygiene:** Every experiment branch is code + journals +
  scorecards. Never embedded WAVs, .npy, or large weights.
- **Shelved-experiment ritual:** Code dies, learning survives. The
  journal markdown is cherry-picked to main (`journal: <slug>`); the
  branch is deleted; artifacts are scheduled for purge.

### `scripts/research-env.sh` subcommands

```
research-env.sh --preflight        # check caps, run scheduled purges (always before eval)
research-env.sh --audit            # print usage report only
research-env.sh --cleanup          # interactive purge of expired/old artifacts
research-env.sh --pin <experiment> # keep artifacts past their default retention
research-env.sh --status           # list experiments, state, artifact disk usage
research-env.sh --guard-ci         # scan .github/workflows for forbidden references
```

## Corpus

Three tiers, distinct ground-truth schemes, partitioned by mode.

### Tier 1 — Synthetic

- **Generator:** extend `training/neural_osd/generate_data.py` pattern.
  Encode known messages with `pancetta-ft8`, modulate, mix with AWGN
  at controlled SNR (–30 to –10 dB in 0.5 dB steps), apply optional
  impairments: frequency offset, time offset, Doppler drift,
  Watterson-channel multipath/fading.
- **Manifests:** `research/corpus/synth/manifests/<config>.json`
  (committed) lists every `(wav_path, encoded_message, snr_db,
  impairments)`. Generated WAVs themselves live at
  `research/corpus/synth/wavs/<config>/*.wav` and are gitignored —
  regenerable byte-for-byte from manifest + seed.
- **Ground truth:** known per manifest. CRC + exact-message match.
- **Size:** ~5–10k WAVs per config (clean, doppler, multipath live
  side-by-side).
- **Role:** sensitivity curve — message recovery rate vs SNR, per
  channel model. Headline numbers: `snr_at_50pct_recovery` and
  `snr_at_90pct_recovery`, per channel.

### Tier 2 — Standard fixtures

- **Source:** existing `pancetta-ft8/tests/fixtures/wav/` — 13 WAVs across
  four subdirs: `generated/` (3, our encoded test signals), `wsjt/` (3,
  WSJT-X golden), `basicft8/` (5, ft8_lib reference), `jtdx/` (2,
  JTDX-recorded off-air).
- **Ground truth:** hand-labeled in `research/corpus/fixtures/ft8/truth.json`.
  Each fixture entry lists the expected decoded messages; the fixtures
  tier passes iff every expected message appears (or, for off-air WAVs
  where the decoder can't recover full content, the entry uses
  `expect: ["any-decode"]` to require ≥1 message).
- **Role:** regression smoke test. Pass/fail per fixture. A candidate
  that drops fixture decodes is suspicious and must explain itself.

### Tier 3 — Curated real

- **Source:** `~/.pancetta/recordings/` (22,464 operator recordings on
  this machine; ~7.5 GB; never committed).
- **Curation:** `curate` binary scores every WAV on:
  - jt9 decode count (band-activity proxy)
  - JTDX decode count
  - `delta(jt9, JTDX)` (disagreement → marginal/interesting)
  - decoded SNR distribution (many weak decodes = good test material)
  - decodes where pancetta currently disagrees with jt9 (failures +
    novel hits — both interesting)
  - estimated band-noise floor (high = hard)
- **Manifest tiers** produced by `curate`:
  - `hard_200.manifest.json` — top-200 hardest WAVs; canonical eval set
  - `hard_1000.manifest.json` — broader set for sensitivity / regression
  - `wild_50.manifest.json` — random sample for sanity (not just hardest)
- **Manifest format:** paths into `~/.pancetta/recordings/` + SHA-256
  + curation-score breakdown. The top 30 hardest WAVs are *copied*
  to `research/corpus/curated/<mode>/in_repo/` (~10 MB) so a fresh
  clone has *something* to run against without the operator's archive.
- **Ground truth:** union of (jt9, JTDX, candidate) CRC-valid decodes.
  A decode is **high-confidence truth** if ≥ 2 of {jt9, JTDX, pancetta}
  found it, *or* if it matches a callsign present in adjacent slots
  (QSO-pattern verification). Single-decoder hits are **candidate-novel**
  — tracked but don't penalize competitors.
- **Baseline cache:** `research/baselines/<mode>/<wav_hash>.json` —
  jt9 + JTDX decodes per WAV. Computed once (committed; tiny). Re-runs
  only when WAVs are added to the curated tier.
- **Role:** real-world decode rate. Headline: fraction of high-confidence
  truth decoded, plus novel-decode count (extra credit, displayed but
  not ranked).

### Reference baselines

`baseline` binary runs `jt9` (WSJT-X CLI) and `jtdx-cli` (or JTDX
binary in scriptable mode) over every WAV in fixtures + curated; emits
one tiny JSON per WAV with decoder identity, decoded messages, and
elapsed time. Re-runs only when:

- new WAVs are added to a curated manifest,
- a new baseline tool is added (e.g., JTDX upgrade), or
- explicitly forced via `--rebuild`.

WSJT-X / JTDX must be installed locally; the binary fails with a
clear message if not. Install instructions go in `pancetta-research/README.md`.

## Eval binary

```bash
cargo run --release -p pancetta-research --bin eval -- \
  --tier synth,fixtures,curated-hard-200,curated-hard-1000 \
  --mode ft8 \
  --output research/scorecards/<branch>.json \
  [--quick]                              # synth-clean + Hard-50 only, ~30s cycle
  [--synth-config research/corpus/synth/clean.manifest.json,doppler.manifest.json] \
  [--baseline research/baselines/]       # default; cached jt9/JTDX
  [--seed 42]                            # deterministic synth + tie-breaking
```

Single binary, no daemon. Exit 0 on success regardless of score — a
"worse" scorecard isn't an error, it's data.

### Scorecard JSON shape

```json
{
  "schema_version": 1,
  "generated_at": "2026-05-18T14:22:01Z",
  "mode": "ft8",
  "git": {
    "branch": "experiment/ft8/multi-pass-subtract",
    "head_sha": "abc1234",
    "main_merge_base": "59edf18",
    "dirty": false
  },
  "build": {
    "rustc_version": "1.85.0",
    "release": true,
    "features": ["transmit", "research-eval"]
  },
  "harness": {
    "harness_version": "0.1.0",
    "host": "darwin/arm64",
    "cores_used": 10,
    "elapsed_seconds": 187.4
  },
  "config": {
    "decoder": { /* full effective DecoderConfig snapshot */ },
    "seed": 42,
    "tiers_run": ["synth-clean", "synth-doppler", "fixtures", "curated-hard-200", "curated-hard-1000"]
  },
  "tiers": {
    "synth-clean": {
      "wavs_processed": 5000,
      "by_snr_db": [
        { "snr_db": -28.0, "attempts": 100, "decoded": 3,  "fp": 0 },
        { "snr_db": -27.5, "attempts": 100, "decoded": 7,  "fp": 0 },
        { "snr_db": -10.0, "attempts": 100, "decoded": 100, "fp": 0 }
      ],
      "snr_at_50pct_recovery_db": -22.5,
      "snr_at_90pct_recovery_db": -18.0,
      "false_positives_total": 0
    },
    "synth-doppler": { /* same shape */ },
    "fixtures": {
      "fixtures_total": 13,
      "fixtures_passed": 12,
      "fixtures_failed": 1,
      "failures": [
        { "wav": "wsjt/170709_135615.wav", "expected": ["any-decode"], "got": [] }
      ],
      "pass_rate": 0.9231
    },
    "curated-hard-200": {
      "wavs_processed": 200,
      "truth_decodes_total": 643,
      "truth_decodes_recovered": 287,
      "decode_rate": 0.4463,
      "novel_decodes": 12,
      "false_positives": 0,
      "wsjtx_decoded": 412,
      "jtdx_decoded": 591,
      "vs_wsjtx_pct": 69.7,
      "vs_jtdx_pct": 48.6,
      "per_wav_top_failures": [
        { "wav_hash": "abc...", "truth": 7, "recovered": 0, "wsjtx": 4, "jtdx": 7 }
      ]
    },
    "curated-hard-1000": { /* same shape */ }
  },
  "composite": {
    "weights": { "real_decode_rate_hard_200": 0.50, "snr_50pct_synth_clean": 0.30, "fixtures_pass_rate": 0.15, "snr_50pct_synth_doppler": 0.05 },
    "score": 0.6234,
    "main_baseline_score": 0.6011,
    "delta_vs_main": 0.0223
  },
  "regressions": {
    "fixture_regression": false,
    "false_positive_introduced": false,
    "snr_curve_regression_dB": 0.0
  },
  "notes": "Multi-pass subtract pass 2 enabled. Decoder config dump in config.decoder."
}
```

Key properties: self-describing (schema_version), reproducible (git
SHA, rustc, seed, features, host), granular (per-SNR bins for synth,
per-WAV failures for real), pre-computed composite (no leaderboard
recompute needed), regression flags.

### `compare` binary output

```
A: research/scorecards/main.json          (sha 59edf18, score 0.6011)
B: research/scorecards/multi-pass-subtract.json  (sha abc1234, score 0.6234, +0.0223)

WINS:
  synth-clean       SNR@50%       -22.0 dB → -22.5 dB   (-0.5 dB sensitivity)
  curated-hard-200  decode_rate   0.4218 → 0.4463       (+0.0245)
  curated-hard-200  novel_decodes 0      → 12

REGRESSIONS:
  (none)

CONFIG DIFF:
  decoder.multi_pass.enabled:    false → true
  decoder.multi_pass.max_passes: 0     → 3
```

### Where scorecards live

- `research/scorecards/main.json` — committed; updated only when main
  moves. The bar to beat.
- `research/scorecards/<branch-name>.json` — on the experiment branch
  only; this path doesn't exist on main.
- `research/scorecards/history/<YYYY-MM-DD>-<slug>.json` — every past
  scorecard from merged *and* shelved experiments. Date-slug naming
  (not branch-name) so the file persists after the branch is deleted.
  Leaderboard binary reads everything here.
- On merge or shelve, the branch-local `<branch>.json` is renamed to
  `history/<YYYY-MM-DD>-<slug>.json` as part of the merge/cherry-pick
  to main. Convention enforced by `research-env.sh --finalize <slug>`.

## Experiment lifecycle

### State machine

```
hypothesis_bank.md (ranked ideas)
        │   Claude picks top idea (or wild-card draw)
        ▼
   [propose]    ──>  experiments/YYYY-MM-DD-<slug>.md created with hypothesis
        │           git worktree + branch experiment/<mode>/<slug> created
        │           journal: state = planned
        ▼
   [implement] ──>  code change on branch, tests added
        │           journal: state = implementing → evaluated
        ▼
   [eval]      ──>  scripts/research-env.sh --preflight  (caps OK?)
        │           cargo run -p pancetta-research --bin eval
        │           scorecard written; compare vs main shown
        ▼
   [decide]    ──>  composite + regression flags determine outcome
        │
        ├── WIN ────────> [merge]
        │   - PR to main with journal + scorecard cherry-picked
        │   - artifacts marked "promoted"; hypothesis bank updated
        │
        ├── LOSS ───────> [shelve]
        │   - journal documents what failed, why, and follow-ups
        │   - journal + scorecard cherry-picked to main
        │     (scorecard moves to research/scorecards/history/)
        │   - branch deleted; artifacts scheduled for 14-day purge
        │
        └── INCONCLUSIVE > [defer]
            - journal records what we'd need to disambiguate
            - branch kept 7 days; auto-shelves after
```

### Journal markdown

`research/experiments/YYYY-MM-DD-<slug>.md` (committed; survives
forever):

```markdown
---
slug: multi-pass-subtract
mode: ft8
state: shelved | evaluated | merged | deferred
created: 2026-05-18T14:00:00Z
last_updated: 2026-05-18T16:32:00Z
branch: experiment/ft8/multi-pass-subtract
parent_hypothesis: hb-014
wild_card: false
scorecard: research/scorecards/multi-pass-subtract.json
delta_vs_main: +0.0223
disposition: merged
---

## Hypothesis
[…]

## Change
[…]

## Result
Composite: 0.6011 → 0.6234 (+0.0223). Real decode rate Hard-200 went
0.42 → 0.45. Synth clean SNR@50% improved 0.5 dB. No fixture regressions,
no new FPs.

## Disposition
Merged to main 2026-05-18. Sets new baseline.

## Learnings
- Subtract+redecode pays off most on busy band recordings (>10 truth decodes).
- min_snr_to_subtract_db was critical — without it, low-confidence first-pass
  decodes get subtracted and corrupt the residual.
- Second pass yielded ~80% of gain; third pass marginal. Configured default 2.

## Follow-ups added to hypothesis bank
- hb-031: Pre-subtract candidate ranking by AP confidence not just sync SNR
- hb-032: Track residual energy; abort pass when residual matches noise floor
- hb-033: Multi-pass interacts with neural OSD — does OSD on residuals help?
```

The **Learnings** + **Follow-ups** sections are the value capture.
Every experiment exits with: what we tried, what happened, what we
now believe, what to try next.

### Worktree & branch hygiene

- Worktree path: `~/.pancetta/worktrees/experiment-<slug>/` (outside
  the main repo to keep editor noise low).
- `CARGO_TARGET_DIR` env var (set by `research-env.sh`) points all
  worktrees at a shared target dir so we don't have 5–15 GB of
  `target/` per worktree.
- On shelve or merge: `git worktree remove`, `git branch -D`,
  artifacts scheduled for purge.
- `research-env.sh --status` shows active experiments (with disk usage),
  shelved-but-not-yet-purged, pinned, last 10 in history.

## Hypothesis bank + decision loop

### `research/hypothesis_bank.md` shape

```markdown
# Hypothesis Bank

last_updated: 2026-05-18T17:00:00Z
current_focus_mode: ft8
wild_card_ratio_target: 0.20
wild_cards_run: 3
exploitation_run: 14
current_ratio: 0.176

## Active (ranked by score)

### hb-014 — Multi-pass subtract-and-redecode  [PRIORITY: 9.2]
  mode: ft8
  status: in-progress (branch: experiment/ft8/multi-pass-subtract)
  priority_score: 9.2
  estimated_effort: 2-3 sessions
  expected_delta: +0.05 to +0.15 real decode rate
  defensible_prior: yes (WSJT-X does this; biggest known gap per memory)
  wild_card: false
  evidence_for:
    - Real decode rate 5-10% of WSJT-X on identical bands
    - Multi-pass is the documented WSJT-X advantage in busy conditions
  evidence_against:
    - Risk of compounding FPs across passes
  related: hb-031, hb-032, hb-033
  notes: |
    Subtract residual at decoded candidate's freq+time, re-run sync on residual.
    Bound passes (max 3). Confidence threshold before subtracting.

### hb-029 — Random gate disabled  [PRIORITY: 4.1]  [WILD]
  mode: ft8
  status: pending
  wild_card: true
  notes: |
    What if we just disable the AP-survival false-positive gate entirely
    and see what falls out? Cheap eval — easy to revert.

[... more entries ...]

## Shelved (kept for reference)

### hb-002 — Wider FFT window  [SHELVED 2026-05-15]
  outcome: -0.01 composite, +0.3 dB SNR50 synth, real decode rate flat
  learning: FFT precision wasn't the bottleneck; gain was eaten by added
            latency in sync window correlation. Revisit if we ever overhaul sync.

## Graduated (merged to main)

### hb-001 — Neural OSD v1  [MERGED 2026-03-12]
  outcome: +0.04 composite. Sets baseline.
```

### Priority scoring

```
priority = 0.40 * expected_delta_normalized          # 0-1, mapped from expected_delta
         + 0.25 * defensible_prior_strength          # 0=wild, 0.5=plausible, 1=strong
         + 0.20 * (1 - effort_normalized)            # cheap experiments rank higher
         + 0.15 * synergy_with_recent_learnings     # 0-1: connects to recent wins/learnings
```

Wild-cards skip this rubric; they're drawn separately when the
wild-card budget says so.

### Decision loop — one iteration

```
1. preflight: scripts/research-env.sh --preflight
2. read state:
   - research/hypothesis_bank.md (active list, wild-card ratio)
   - research/scorecards/main.json (current bar)
   - last 5 entries in research/experiments/*.md (recent learnings)
3. choose next experiment:
   IF current_wild_card_ratio < target:
     draw a wild-card hypothesis (or invent one)
   ELSE:
     pick highest priority_score hypothesis with status: pending AND mode = focus
4. create branch + worktree + journal entry (state: planned)
5. implement: apply the change, add tests, commit; journal → evaluating
6. eval: cargo run -p pancetta-research --bin eval ...
        cargo run -p pancetta-research --bin compare main.json <branch>.json
7. decide (WIN / LOSS / INCONCLUSIVE per state machine)
8. update hypothesis bank: mark status, spawn follow-ups, re-rank,
   update wild_card_ratio counters
9. commit hypothesis_bank.md change + journal change
10. report: "Experiment <slug> [merged/shelved], composite <delta>.
    Next: <hypothesis>. Continue? (y/N)"
11. If user continues (or /loop is running), goto 1.
```

### Wild-card rule

If `current_wild_card_ratio < wild_card_ratio_target`, the next
experiment must be wild-card. Wild-card = (a) hypothesis with no
defensible prior, OR (b) intentionally chaotic — random
hyperparameter, "what if we just remove this filter," port a paper
idea we haven't validated. Wild-cards have the same lifecycle but
lower regression sensitivity (we expect them to often lose; we're
paying for the surprise).

### Kill criteria

So the loop doesn't grind forever on dead ends:

- A hypothesis is auto-shelved if 2 consecutive related experiments
  produce regressions.
- A hypothesis is auto-deprioritized if `expected_delta` repeatedly
  misses by >2×.
- A whole branch family ("multi-pass approaches") is flagged for
  human review if 3+ siblings shelve with no win.

### Bootstrap

Phase 0 task before the loop starts: a `bootstrap-bank` session in
which Claude reads decoder source, the neural OSD README, CLAUDE.md
"Known Gaps", this spec, and memory entries about decoder sensitivity,
then writes the initial ~15–25 hypothesis entries (mix of well-defensible
ideas and a few wild-cards). The operator reviews and edits the
initial bank before any experiments run.

## Multi-mode extensibility

FT8 first; FT4 / JS8 / JT9 / JT65 / MSK144 plug in later. **The
framework is mode-agnostic from day one.**

What changes when mode N+1 arrives — and nothing else:

1. **Mode-tagged everything.** Every hypothesis, experiment, scorecard,
   branch carries `mode: <mode>`. Branch naming: `experiment/<mode>/<slug>`.
2. **Corpus tiers partition by mode** under `research/corpus/{synth,fixtures,curated}/<mode>/`.
3. **DecoderUnderTest trait** in `pancetta-research/src/lib.rs`:
   ```rust
   trait DecoderUnderTest {
       fn mode(&self) -> Mode;
       fn decode_wav(&self, path: &Path) -> Vec<Decode>;
       fn config_snapshot(&self) -> serde_json::Value;
   }
   ```
   Today: one impl, `Ft8Decoder`. Tomorrow: `Ft4Decoder`, `Js8Decoder`.
4. **Baseline tooling is mode-aware.** `baseline --mode ft8` dispatches
   to jt9 with FT8 flags; future `--mode ft4` to jt9 with FT4 flags;
   future `--mode js8` to JS8Call's CLI.
5. **Composite metric per-mode.** Cross-mode rollup is
   `mean(per_mode_composite)` in the leaderboard.
6. **Hypothesis bank scoped to focus mode by default.** A `current_focus_mode`
   header in the bank controls which mode the loop is currently working.
   Cross-mode hypotheses (`mode: cross-mode` — shared DSP, sync,
   OSD framework) stay visible regardless of focus.

What we don't build now:

- No FT4 / JS8Call / JT9 / JT65 / MSK144 decoders.
- No baselines for those modes.
- `Mode` enum starts with only `Mode::Ft8`.
- Adding mode #2 is its own future spec.

Why bother now if FT8-only:

1. Renaming everything to add `/ft8/` partitions later is repo churn;
   doing it once now is free.
2. Cross-mode learnings (e.g., "OSD redesign helps all LDPC modes")
   need a hypothesis-bank category from day one.
3. Forcing function on architecture cleanliness — if the harness can
   pretend to be mode-agnostic before mode #2 exists, we won't
   accidentally bake FT8 assumptions into eval/scorecard code.

## Future scaling (design-around, don't-build-now)

Single-machine slow-and-steady is v1. These paths exist by virtue of
the file-based design, so we don't paint ourselves into a corner.

- **Parallel experiments, same machine.** Multiple worktrees + shared
  `CARGO_TARGET_DIR`. Hypothesis-bank lockfile prevents two processes
  claiming the same idea.
- **Parallel experiments, multiple machines.** rsync `research/` +
  `~/.pancetta/research_artifacts/` between hosts; `claimed_by: <hostname>`
  field on in-progress entries; `research-env.sh --remote-eval <host>`
  ships a branch + runs eval over SSH.
- **Cloud GPU burst for training.** Same pattern, training-only.
  `--remote-train <provider>` rsyncs training/ + data, runs over
  their CLI, rsyncs the .pt back. Operator's strong preference is
  that this stays Claude-orchestrated in a normal session — fine,
  this design is exactly that.
- **Distributed sweeps.** Each `(config_point)` becomes a mini-experiment,
  claimed by host via the bank lockfile.
- **LLM-in-the-loop ideas.** Local qwen3 could later summarize scorecard
  diffs into journal prose, or read learnings + suggest new hypotheses.
  Both run through standard Claude Code tool calls; the harness
  doesn't need to know.

**Explicit non-goals for v1:** no web UI, no daemon, no DB, no
notifications, no CI integration.

## Testing the harness itself

The harness has tests too. In `pancetta-research/tests/`:

- **Schema round-trip:** scorecard serializes/deserializes deterministically.
- **Compare correctness:** known-A vs known-B scorecard produces the
  expected diff string.
- **Curate stability:** running curate twice on the same corpus +
  baseline cache produces the same manifest.
- **Composite math:** weighted-sum implementation matches spec.
- **Reproducibility:** running `eval --seed 42` twice produces
  byte-identical scorecards (modulo `generated_at` and
  `elapsed_seconds`).

These tests run under `cargo test -p pancetta-research --features research-eval`.
They are not run by CI (`--features research-eval` is never enabled by
the CI lane). They are run locally by `scripts/check.sh --research`
when the operator opts in.

## Open questions (resolved during brainstorm)

- Primary metric: **composite of real decode rate + synth sensitivity**.
- Ground truth: **synthetic = known; fixtures = hand-labeled; real =
  union(jt9, JTDX, candidate) with QSO-pattern verification**.
- Iteration unit: **experiment-per-branch with leaderboard, plus
  persistent hypothesis bank**.
- Compute: **Mac M4 local for everything in v1; cloud-burst path
  exists in the design**.
- CI: **excluded by construction; local-only forever**.
- Multi-mode: **mode-agnostic from day one; FT8 is the only impl in v1**.
- Hygiene: **500 MB committed cap, 100 GB on-disk cap with auto-cleanup
  + alarms; no large blobs in branches; shelved experiments preserve
  learnings, discard code**.
- Wild-card budget: **20 % of experiments are wild-cards by default**.

## Related docs

- [`CLAUDE.md`](../../../CLAUDE.md) — project overview, known decoder gaps
- [`training/neural_osd/README.md`](../../../training/neural_osd/README.md) —
  precedent for the synth-data → PyTorch → export-weights pattern we'll
  reuse for ML experiments
- [`docs/superpowers/specs/2026-04-20-decoder-phase-a-design.md`](2026-04-20-decoder-phase-a-design.md) —
  prior decoder work
- [karpathy/autoresearch](https://github.com/karpathy/autoresearch) —
  loose inspiration for the closed-loop research style
