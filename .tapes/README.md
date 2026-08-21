# Demo recordings

`.tape` scripts for [VHS](https://github.com/charmbracelet/vhs). To
re-render after a TUI change:

```bash
brew install vhs   # once
vhs .tapes/demo.tape
```

Output lands in `assets/`. CI regenerates these automatically on TUI
changes (see `.github/workflows/demo-assets.yml`) -- these are not required
to be re-run by hand for every PR. (`demo-assets.yml` is not yet landed as
of this writing -- it's a later task on the README-visual-identity plan;
until then, re-render by hand per the steps above.)
