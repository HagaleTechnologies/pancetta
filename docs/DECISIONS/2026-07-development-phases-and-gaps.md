<!-- Moved from CLAUDE.md 2026-07-17 (docs/claude-md-consolidation) during the CLAUDE.md/AGENTS.md
     consolidation pass. The "Development Phases (End-to-End QSO Initiative)" section referenced by
     the original consolidation task no longer exists in CLAUDE.md as of this move — that initiative's
     phases had already been completed and folded into the subsystem digests below in an earlier
     pass. Only "Known Gaps and TODOs" survived to be moved here; content below is carried over
     verbatim from CLAUDE.md. For current behavior trust the code and specs in
     `docs/superpowers/specs/`. -->

# Known gaps and TODOs

- **cqdx `GET /api/v1/spots?live=true` response envelope key (`groups`) unverified against live API** — a gated live test exists: `CQDX_TOKEN=pat_xxx cargo test -p pancetta-cqdx test_live_spots_envelope -- --ignored --nocapture`.

- **`pancetta-research` has no CI coverage** — every workspace job passes
  `--exclude pancetta-research` (`.github/workflows/ci.yml:98,188,191,194`) and the crate is
  absent from `default-members` (`Cargo.toml:17-29`), so the cross-platform lane's bare
  `cargo check` skips it too. Only `cargo fmt --all` (`ci.yml:113`) touches it. Consequence:
  a dependency bump that breaks the crate goes green without ever compiling it — dependabot
  PR #211 did exactly this, bumping `rand` 0.8→0.10 while leaving `rand_distr` at 0.4 and
  editing zero source files, and passing every check. Any future bump touching this crate
  must be verified locally and the output pasted into the PR; that is the only evidence
  available. Decide separately whether a `cargo check -p pancetta-research` lane is worth
  the CI minutes — the crate is excluded by deliberate design (`AGENTS.md`), so this is an
  architecture decision, not a bug. (Surfaced by PAN-1, 2026-07-29.)

- **~25 duplicated `gaussian_noise` Box-Muller helpers** in `pancetta-research/examples/` —
  byte-identical copies (verified by hashing each function body: one distinct hash across 25
  files), each edited in place during the PAN-1 `rand` 0.10 migration because keeping that
  diff mechanical kept it reviewable. Consolidating them into one crate-level helper is a
  standalone cleanup. (Surfaced by PAN-1, 2026-07-29.)

## Missing-docs lint status (verified 2026-07-17)

Per-crate `#![...(missing_docs)]` levels, dropped from CLAUDE.md's "Documentation Policy" section
to keep that section to a policy statement rather than a point-in-time status list:

- `pancetta-core`: `#![warn(missing_docs)]`
- `pancetta-hamlib`: `#![deny(missing_docs)]`
- `pancetta-protocol`: no `missing_docs` attribute set (defaults to allow)
- All other crates (`pancetta`, `pancetta-agent`, `pancetta-audio`, `pancetta-dsp`, `pancetta-config`,
  `pancetta-qso`, `pancetta-dx`, `pancetta-cqdx`, `pancetta-tui`, `pancetta-ft8`, `pancetta-research`):
  `#![allow(missing_docs)]` with a `// TODO: documentation pass pending` comment.

Switch each crate to `warn`/`deny` as its doc coverage lands.
