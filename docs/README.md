# Pancetta documentation index

Two kinds of documents live here. **Curated docs** are maintained,
current, and safe to trust; **working notes** are point-in-time design
and analysis artifacts — accurate when written, not maintained since.
When a working note disagrees with a curated doc or the code, the
working note loses.

## Curated — read these

| Doc | What it is |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Crate relationships, component diagram, channel topology |
| [CONFIG.md](CONFIG.md) | Configuration reference for `~/.pancetta/pancetta.toml` |
| [RUNBOOK.md](RUNBOOK.md) | Operator procedures: modes, supervision, Phase 5 checklist |
| [KEYBINDINGS.md](KEYBINDINGS.md) | Generated TUI keybinding reference (source: `pancetta-tui/src/keymap.rs`; drift-tested) |
| [DECISIONS/](DECISIONS/) | Decision digests by subsystem (tx-scheduling, qso-engine, modes, remote-operation, tui, logging-uploads, config-and-platform) + ADR-001 |
| [fcc-part97-compliance.md](fcc-part97-compliance.md) | Regulatory / TX-safety notes |
| [decoder-comparison.md](decoder-comparison.md) | Pancetta decoder vs. peer FT8 decoders |
| [cqdx-api-requirements.md](cqdx-api-requirements.md) | Cross-repo API contract with cqdx.io |

## Working notes / point-in-time (unmaintained)

Design plans, audits, and analyses, kept for context:

- Decoder: `decoder-analysis.md`, `ap-decoding-design.md`, `neural-osd-design.md`
- QSO engine: `qso-state-machine-analysis.md`, `qso-engine-bugs.md`, `qso-scenario-catalog-2026-06-16.md`
- Platform: `audio-robustness-plan.md`, `observability-diagnostics-plan.md`, `task-supervision-plan.md`
- Audits: `ux-audit-2026-06-14.md`, `security-review-2026-04-29.md`, `security-deep-analysis-2026-06-17.md`, `security-review-remote-rig-2026-07-02.md`
- `engineering/` — methodology audits; `operations/` — one-off procedures

## Process directories

- `superpowers/specs/` + `superpowers/plans/` — design specs (authoritative for current behavior) and their implementation plans
- `archive/` — superseded plans, kept for history
