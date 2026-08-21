# Demo recordings

`.tape` scripts for [VHS](https://github.com/charmbracelet/vhs). Run all of
them from the repo root:

```bash
brew install vhs   # once
cargo build --release -p pancetta   # warm the binary; the tapes assume it
vhs .tapes/demo.tape
vhs .tapes/screenshots.tape
vhs .tapes/feature-decode-effort.tape
```

Output lands in `assets/`.

| Tape | Produces | What it captures |
|------|----------|------------------|
| `demo.tape` | `assets/demo.gif` | The README hero GIF: a full `--replay` session from startup through self-terminating exit (~110s worst case). |
| `screenshots.tape` | `assets/screenshot-operate.png`, `-priority.png`, `-qso.png`, `-waterfall.png` | Four static panel captures — the Operate view, the DX Hunter panel (priority-score column), QSO Status, and the Monitor waterfall. |
| `feature-decode-effort.tape` | `assets/feature-decode-effort.gif` | A short, decode-independent feature clip: cycling the decode-effort preset ring with `e` and the title-bar `DECODE: <PRESET> <n>ms` chip updating live. |

All three drive `pancetta --replay assets/demo-wav`, so every asset comes
from a real run with real off-air audio flowing through the pipeline.

The corpus decodes only on its **last** slot: `live_now.wav` yields ~26
messages in one window, while the four numbered captures ahead of it are
documented non-decoding content (`assets/demo-wav/README.md`). So the first
~60s of any replay run correctly shows empty decode panels and the payoff
lands at the end — `demo.gif` captures that arrival, and `screenshots.tape`
waits for it (`Wait+Screen /Msgs: [1-9][0-9]/`) before capturing. Sizing any
new tape's timing off a fixed sleep instead will capture empty panels.

(Until PR #263 the corpus decoded *nothing* under `--replay`. Root cause: the
replay feeder emitted bare mono while the DSP stage de-interleaved against
`[audio] input_channels`, silently halving the effective sample rate. See
`demo.tape`'s trailing comment block.)

## CI

`.github/workflows/demo-assets.yml` re-renders **`demo.tape` only** and
auto-commits `assets/demo.gif` when the render changes. It is **manual-only**
(`workflow_dispatch`) — nothing regenerates the GIF automatically:

```bash
gh workflow run demo-assets.yml --ref <branch>   # or the Actions tab
```

Dispatch it after a change under `.tapes/**`, `assets/demo-wav/**`,
`pancetta-tui/**`, `pancetta/src/**`, `pancetta-config/src/**`, or
`pancetta-ft8/src/**` — the paths that actually change what the GIF shows.
That list lives in the workflow's header comment as the filter to restore if
this ever goes automatic again. It used to be a `pull_request` trigger with
exactly that filter, but the render is never byte-reproducible (embedded
timestamps, live waterfall), so it auto-committed on nearly every push and
regenerated the GIF 8 times in ~3 hours on one PR.

`screenshots.tape` and `feature-decode-effort.tape` are **not** automated at
all — re-render those by hand with the commands above after any TUI change
that alters what they show.
