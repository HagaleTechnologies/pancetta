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
- `corpus/curated/<mode>/` — manifest of hard real-world WAVs (absolute
  paths + SHA-256 into a stable recording store such as
  `~/.pancetta/corpus` or the maersk NAS archive; never the live capture
  buffer).
- `corpus/synth/manifests/` — synth-corpus generator configs (committed).
- `corpus/synth/wavs/` — generated synth WAVs (gitignored).

WAV files live outside the repo. The manifests reference them by absolute
path + SHA-256.

## PAN-213: recording paths in the survey and kill-switch manifests

The 2026-05-30 survey (`corpus/surveys/2026-05-30/`) and the hb-104 kill
switch (`corpus/curated/ft8/hb104_kill_switch.json`) must list recordings
that still exist. `~/.pancetta/recordings/` is the live capture buffer —
the NAS offload empties it and the 10 GB cap evicts it — so it is not a
stable home for a referenced recording. The guard rejects any WAV path
inside it (tilde or absolute, any home, POSIX or Windows separators, dot
segments) as well as non-absolute WAV paths, and requires all six input
files by name so removing one cannot make the check pass:

```sh
python3 scripts/pan213_manifests.py check --root .
```

The plain check does no WAV I/O and runs anywhere. `--verify-files`
additionally opens every referenced WAV, resolves symlinks (rejecting
targets inside the live buffer), and compares the streamed SHA-256 with
the stored `wav_sha256`:

```sh
python3 scripts/pan213_manifests.py check --root . --verify-files
```

Regenerating the survey or the kill switch (`scripts/batch86_select_slots.py`,
`pancetta-research/examples/score_all_today.rs`) can reintroduce
capture-buffer paths — rerun the check after every regeneration, even when
only corpus data changed.

Repairing broken references is digest-verified and staged. Each live-buffer
reference is rewritten to the same-basename copy in the local corpus, or,
for names the corpus cannot resolve, to the first matching file across all
`YYYY-MM` month shards under the NAS archive root (nested paths included;
the stored `wav_sha256` must match the bytes; a wrong digest lets the
search continue). Rows already at a stable absolute path are verified and
retained. Unresolved recordings are dropped and recorded. The staged tree
carries the four rewritten manifests plus `per_wav.json` and
`summary.json` byte for byte, is audited with the check above, and only a
passing audit emits `research/corpus/PAN-213-recovery.json` — the drop
record with the old path, expected digest, manifest and zero-based row
index of every occurrence, and whether no candidate existed or the
candidates had different digests:

```sh
pan213_stage="$(mktemp -d)/repair"
python3 scripts/pan213_manifests.py repair --root . \
  --corpus-dir "$HOME/.pancetta/corpus" \
  --nas-dir /Users/Shared/maersk/offload/pancetta-wav-corpus \
  --output-dir "$pan213_stage"
python3 scripts/pan213_manifests.py check --root "$pan213_stage" --verify-files
```

Run the repair only on the station (or an authorized snapshot of both
stores), confirm the NAS root is the mounted archive and that offload kept
basenames, compare the staged rows against the originals, then import the
staged files and the drop report into the repository. CI's
`research-scan-tests` job runs the fixture tests and the strict offline
check on every checkout.

The 2026-11-22 station repair on sophon rewrote 2,073 recordings (2,193
references) to digest-verified copies in `~/.pancetta/corpus`. The other 93
(all `ft8_20260420_*`, referenced only by `existing_top100.entries.json`)
had no copy in the local corpus or in any NAS month shard (the archive
starts at 2026-07) and were dropped; `corpus/PAN-213-recovery.json` lists
each one with its expected digest and original row index.
