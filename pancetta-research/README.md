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

## Design

See `docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md`.

## Implementation plans

- Plan 1 of 3 (foundations): `docs/superpowers/plans/2026-05-18-research-harness-1-foundations.md` — complete
- Plan 2 of 3 (eval pipeline + corpus): `docs/superpowers/plans/2026-05-20-research-harness-2-eval-pipeline.md` — complete
- Plan 3 of 3 (curation + leaderboard + lifecycle): `docs/superpowers/plans/2026-05-20-research-harness-3-iteration-loop.md` — complete
