<!-- Moved from CLAUDE.md 2026-07-17 (docs/claude-md-consolidation) during the CLAUDE.md/AGENTS.md
     consolidation pass. The "Development Phases (End-to-End QSO Initiative)" section referenced by
     the original consolidation task no longer exists in CLAUDE.md as of this move — that initiative's
     phases had already been completed and folded into the subsystem digests below in an earlier
     pass. Only "Known Gaps and TODOs" survived to be moved here; content below is carried over
     verbatim from CLAUDE.md. For current behavior trust the code and specs in
     `docs/superpowers/specs/`. -->

# Known gaps and TODOs

- **DX Hunter — per-band-needed is only an approximation of the operator's currently-tuned band, not each row's own band**: entity-name resolution and ATNO surfacing (this bullet's original items 1-2) shipped 2026-06-23/26, well before this doc was written — corrected 2026-07-18. What remains: `is_needed_dxcc`/`is_atno` (`pancetta/src/priority_evaluator.rs`) take no band parameter, so the single global `needed_dxcc`/`needed_atno` set reflects whichever band the operator is tuned to at the moment, not the band of an individual DX Hunter row (e.g. a cluster spot on a different band than the current dial). `docs/cqdx-api-requirements.md`'s per-band `GET /api/v1/entities/needed?band=` is speced and already implemented client-side (`pancetta-cqdx/src/client.rs`), but is still marked "PROPOSED" server-side and unverified live. A local, cqdx-independent per-row alternative (cross-reference the QSO DB's worked callsigns, resolved to DXCC entity via the existing offline resolver, aggregated per band) is fully buildable without waiting on that endpoint.
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
