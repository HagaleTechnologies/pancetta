<!-- Moved from CLAUDE.md 2026-07-17 (docs/claude-md-consolidation) during the CLAUDE.md/AGENTS.md
     consolidation pass. The "Development Phases (End-to-End QSO Initiative)" section referenced by
     the original consolidation task no longer exists in CLAUDE.md as of this move — that initiative's
     phases had already been completed and folded into the subsystem digests below in an earlier
     pass. Only "Known Gaps and TODOs" survived to be moved here; content below is carried over
     verbatim from CLAUDE.md. For current behavior trust the code and specs in
     `docs/superpowers/specs/`. -->

# Known gaps and TODOs

- **cqdx `GET /api/v1/spots?live=true` response envelope key (`groups`) unverified against live API** — a gated live test exists: `CQDX_TOKEN=pat_xxx cargo test -p pancetta-cqdx test_live_spots_envelope -- --ignored --nocapture`.

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
