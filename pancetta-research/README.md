# pancetta-research

**Local-only crate. Builds and runs from your dev machine only. No GitHub
Actions, no CI, no cron — burns Actions minutes for no benefit. If you find
yourself wiring this into CI, stop.**

This crate is the iteration harness for improving the pancetta decoder. It is
deliberately excluded from the workspace `default-members`, so `cargo build`
and `cargo test` from the repo root skip it entirely.

## Quick start

```bash
# Build everything
cargo build --release -p pancetta-research

# 1. Generate the synth corpus (60 WAVs: 6 messages × 10 SNR steps)
cargo run --release -p pancetta-research --bin gen-synth -- \
    --config research/corpus/synth/manifests/clean.config.json \
    --output research/corpus/synth/manifests/clean.manifest.json

# 2. Curate the operator's real-world WAVs into 3 ranked manifests
cargo run --release -p pancetta-research --bin curate -- \
    --source-dir ~/.pancetta/recordings \
    --output-prefix research/corpus/curated/ft8

# 3. Cache jt9 baseline over all tiers (one-time, ~45 min total)
cargo run --release -p pancetta-research --bin baseline -- --tier fixtures --mode ft8
cargo run --release -p pancetta-research --bin baseline -- --tier synth --mode ft8
cargo run --release -p pancetta-research --bin baseline -- --tier curated-hard-200 --mode ft8
cargo run --release -p pancetta-research --bin baseline -- --tier curated-hard-1000 --mode ft8
cargo run --release -p pancetta-research --bin baseline -- --tier wild-50 --mode ft8

# 4. Score the current decoder against all tiers
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures,synth-clean,curated-hard-200,curated-hard-1000,wild-50 \
    --mode ft8 \
    --output research/scorecards/main.json

# 5. Rank all scorecards in research/scorecards/
cargo run --release -p pancetta-research --bin leaderboard

# 6. Diff two scorecards
cargo run --release -p pancetta-research --bin compare -- \
    research/scorecards/main.json research/scorecards/history/2026-05-20-experiment-X.json

# Experiment lifecycle (research-env.sh)
./scripts/research-env.sh --status              # list experiments + state
./scripts/research-env.sh --pin <slug>          # protect artifacts from purge
./scripts/research-env.sh --finalize <slug>     # move branch scorecard to history/
./scripts/research-env.sh --cleanup             # dry-run purge of expired artifacts
./scripts/research-env.sh --cleanup --execute   # actually purge
./scripts/research-env.sh --preflight           # disk-cap check before eval
```

WSJT-X must be installed locally for `baseline` to find `jt9`. On macOS,
the default expected path is `/Applications/wsjtx.app/Contents/MacOS/jt9`;
override with `--jt9-path /path/to/jt9` if needed.

## Why this is local-only

The full corpus (~7.5 GB of operator recordings in `~/.pancetta/recordings/`)
lives on the operator's machine, not in git. The harness builds a curated
subset, runs the decoder against it, and produces scorecards. Running this in
CI would (a) burn Actions minutes on an iteration loop that is inherently
operator-driven and (b) not have access to the real-world WAV corpus anyway.

## RNG stream stability across `rand` majors

The seed-derivation tree (`seed_from_u64` plus the `wrapping_add`/`wrapping_mul`
folding and the XOR salts) is version-independent, and every generator here is
deterministic *within* a given `rand` major: same seed + same config →
byte-identical WAVs. That guarantee does **not** automatically extend across a
`rand` major version, because the mapping from the raw RNG stream to a drawn
value is not part of `rand`'s stability promise.

The 2026-07-29 bump from `rand` 0.8 / `rand_distr` 0.4 to `rand` 0.10 /
`rand_distr` 0.6 (PAN-1) was measured rather than assumed. Comparing both
versions at seed 42:

| Primitive | Across 0.8 → 0.10 |
|---|---|
| `StdRng::next_u64` (core stream) | **stable** — byte-identical |
| `rand_distr::Normal::sample::<f64>` | **stable** — bit-identical |
| `random::<f64>()` (was `gen()`) | **stable** — bit-identical |
| `random_range(f32_lo..f32_hi)` | **stable** |
| `random_range(f64_lo..=f64_hi)` | changed by ~1–2 ULP |
| `random_range(0..n)` / `(0..=n)`, integer | **changed** — different draws entirely |

So the practical consequence is narrower than "regenerate everything":

- **Pure-AWGN output regenerates bit-identically.** A `gen-noise` corpus with
  `--birdie-fraction 0` and a `gen-synth` AWGN corpus both reproduce their
  pre-bump `wav_sha256` values exactly, because those paths only ever call
  `Normal::sample`. Verified on a 4-WAV corpus: all four hashes matched.
- **Anything drawing an integer range does not.** `select_birdie_indices`
  (`gen_noise.rs`) is a Fisher-Yates shuffle over `random_range(0..=i)`, so
  *which* files receive birdies changes. Verified with `--count 8 --seed 42
  --birdie-fraction 0.5`: the selection moved from `[0,1,0,0,1,1,0,1]` to
  `[0,1,0,1,1,0,1,0]` and 4 of 8 `wav_sha256` values changed. The *count* is
  unaffected — exactly 4 birdies on both sides, which
  `birdie_selection_count_is_exact_across_fractions` pins as a test.
- `curate.rs` shuffles its pool the same way and does not persist its seed in
  `CuratedManifest`, so a curated selection made before the bump cannot be
  reproduced after it at all — re-curate from the source pool.

**Regenerate any locally-held corpus that used a non-zero birdie fraction, and
treat `wav_sha256` values in any pre-bump manifest as stale.** Nothing in-tree
pins a golden hash, so nothing fails loudly if you skip this. The statistical
contracts *are* preserved and are enforced by tests: noise-floor RMS, AWGN σ at
the 2500 Hz reference bandwidth, SNR scaling, Gaussianity, and exact birdie
counts all hold identically on both sides of the bump.

## Design

See `docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md`.

## Implementation plans

- Plan 1 of 3 (foundations): `docs/superpowers/plans/2026-05-18-research-harness-1-foundations.md` — complete
- Plan 2 of 3 (eval pipeline + corpus): `docs/superpowers/plans/2026-05-20-research-harness-2-eval-pipeline.md` — complete
- Plan 3 of 3 (curation + leaderboard + lifecycle): `docs/superpowers/plans/2026-05-20-research-harness-3-iteration-loop.md` — complete
