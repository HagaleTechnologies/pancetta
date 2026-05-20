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

# Generate the synth corpus (60 WAVs: 6 messages × 10 SNR steps)
cargo run --release -p pancetta-research --bin gen-synth -- \
    --config research/corpus/synth/manifests/clean.config.json \
    --output research/corpus/synth/manifests/clean.manifest.json

# Cache jt9 baseline over fixtures + synth (once; tiny JSON per WAV; committed)
cargo run --release -p pancetta-research --bin baseline -- --tier fixtures --mode ft8
cargo run --release -p pancetta-research --bin baseline -- --tier synth --mode ft8

# Score current decoder against all tiers
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures,synth-clean --mode ft8 --output research/scorecards/main.json

# Diff two scorecards
cargo run --release -p pancetta-research --bin compare -- \
    research/scorecards/main.json research/scorecards/experiment-X.json

# Disk hygiene check
./scripts/research-env.sh --preflight
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
- Plan 3 of 3 (curation + leaderboard + lifecycle): written after plan 2 lands
