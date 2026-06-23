# research/

This directory is the journal + artifacts surface for the decoder research
harness. It is plain markdown + JSON + manifests — no databases, no daemons.

Layout (per
`docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md`):

- `hypothesis_bank.md` — ranked list of ideas. Claude reads/writes each
  iteration.
- `experiments/` — one `.md` per experiment branch. Journal entries survive
  forever, even after the branch is deleted.
- `scorecards/main.json` — current main-branch scorecard (the bar to beat).
- `scorecards/history/` — all past scorecards (merged + shelved).
- `scorecards/<branch>.json` — in-progress, on the experiment branch.
- `baselines/<mode>/` — cached jt9/JTDX decodes per WAV (committed; tiny).
- `corpus/fixtures/<mode>/` — references into `pancetta-ft8/tests/fixtures/wav/`
  + ground-truth JSON.
- `corpus/curated/<mode>/` — manifest of hard real-world WAVs (paths +
  hashes pointing into `~/.pancetta/recordings/`).
- `corpus/synth/manifests/` — synth-corpus generator configs (committed).
- `corpus/synth/wavs/` — generated synth WAVs (gitignored).

WAV files live outside the repo. The manifests reference them by absolute
path + SHA-256.
