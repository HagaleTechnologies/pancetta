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
from a real run with real off-air audio flowing through the pipeline. That
corpus does **not** produce an FT8 decode under `--replay`; see the trailing
"HONEST DISCLOSURE" comment block in `demo.tape` for what has been measured
and ruled out. Don't write README copy that implies these assets show a
captured decode.

## CI

`.github/workflows/demo-assets.yml` re-renders **`demo.tape` only** and
auto-commits `assets/demo.gif` when the render changes. It triggers on PRs
touching `.tapes/**`, `assets/demo-wav/**`, `pancetta-tui/**`,
`pancetta/src/**`, `pancetta-config/src/**`, or `pancetta-ft8/src/**`, and is
skipped on fork PRs (the auto-commit push can't work there). `demo.gif`
therefore doesn't need re-rendering by hand for every PR.

`screenshots.tape` and `feature-decode-effort.tape` are **not** automated —
re-render those by hand with the commands above after any TUI change that
alters what they show.
