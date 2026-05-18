# pancetta-research

**Local-only crate. Builds and runs from your dev machine only. No GitHub
Actions, no CI, no cron — burns Actions minutes for no benefit. If you find
yourself wiring this into CI, stop.**

This crate is the iteration harness for improving the pancetta decoder. It is
deliberately excluded from the workspace `default-members`, so `cargo build`
and `cargo test` from the repo root skip it entirely.

## Quick start

```bash
# Build the harness
cargo build --release -p pancetta-research

# Run a fixtures-only eval (smoke test; ~10 s)
cargo run --release -p pancetta-research --bin eval -- \
    --tier fixtures \
    --mode ft8 \
    --output research/scorecards/main.json

# Run the disk hygiene check
./scripts/research-env.sh --preflight
```

## Why this is local-only

The full corpus (~7.5 GB of operator recordings in `~/.pancetta/recordings/`)
lives on the operator's machine, not in git. The harness builds a curated
subset, runs the decoder against it, and produces scorecards. Running this in
CI would (a) burn Actions minutes on an iteration loop that is inherently
operator-driven and (b) not have access to the real-world WAV corpus anyway.

## Design

See `docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md`.

## Implementation plans

- Plan 1 of 3 (foundations): `docs/superpowers/plans/2026-05-18-research-harness-1-foundations.md` — this one
- Plan 2 of 3 (eval pipeline + corpus): written after plan 1 lands
- Plan 3 of 3 (curation + leaderboard + lifecycle): written after plan 2 lands
