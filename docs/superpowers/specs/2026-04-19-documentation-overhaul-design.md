# Documentation Overhaul Design Spec

**Goal:** Create world-class documentation optimized for AI agents (primary reader) and humans (secondary), with a sustainable update mechanism.

**Date:** 2026-04-19

---

## Three-Layer Documentation Architecture

### Layer 1: Inline Doc Comments (AI agents, primary)

Module-level `//!` comments on every `lib.rs` and major `mod.rs` with:
- One-line purpose
- Data flow: what feeds in, what comes out, in pipeline terms
- Key types with `[`backtick links`]`
- Crate relationships: receives from / sends to

Struct/trait-level: one sentence of purpose, document non-obvious fields only.

Function-level: only when behavior isn't obvious from signature. Always document panics, safety invariants, non-obvious returns.

Enable `#![warn(missing_docs)]` in all crates.

### Layer 2: CLAUDE.md + ARCHITECTURE.md (AI agents + developers)

**CLAUDE.md** stays concise and actionable (build commands, known gaps, project phases). Already maintained — continue as-is.

**ARCHITECTURE.md** rewritten as a living document:
- Crate dependency graph (which crates depend on which)
- End-to-end data flow: audio input → DSP → FT8 decode → autonomous decision → TX → QSO log
- Key abstractions and where to find them (coordinator, message bus, priority scorer)
- Concise — aim for ~200 lines, not a textbook

### Layer 3: README.md + FEATURES.md (humans, bragsheet)

**README.md** rewritten:
- Fix placeholder URLs (yourusername → HagaleTechnologies)
- Accurate performance numbers from actual benchmarks
- Working quick-start instructions
- Brief architecture overview with crate table
- Link to FEATURES.md for the full showcase

**FEATURES.md** (new):
- Organized by capability, not implementation
- What pancetta can do, how well it does it, what makes it unique
- Decoder performance stats, autonomous operator capabilities, multi-stream TX

## Doc Triage

### Keep & Rewrite
- `docs/ARCHITECTURE.md` — full rewrite to reflect current 11-crate structure
- `docs/CONFIG.md` — update to match current pancetta-config
- `docs/cqdx-api-requirements.md` — already current

### Keep As-Is
- `docs/CONTRIBUTING.md`

### Delete (16 files)
- `docs/API.md` — no public API
- `docs/API_SPECIFICATION.md` — no public API
- `docs/BUILD_ANALYSIS_REPORT.md` — stale scaffold
- `docs/BUILD_COMPLETION_REPORT.md` — stale scaffold
- `docs/BUILD_STATUS_REPORT.md` — stale scaffold
- `docs/CRITICAL_FIXES.md` — historical, in git
- `docs/EXECUTIVE_DECISION.md` — scaffold artifact
- `docs/IMPLEMENTATION_PLAN.md` — superseded by superpowers plans
- `docs/INSTALL.md` — consolidate into README
- `docs/PRODUCT_REVIEW.md` — scaffold artifact
- `docs/PROJECT_REVIEW_2026-03-31.md` — point-in-time snapshot
- `docs/REGULATORY_COMPLIANCE.md` — fold relevant parts into ARCHITECTURE.md
- `docs/REQUIREMENTS.md` — scaffold
- `docs/REQUIREMENTS_VALIDATION_REPORT.md` — scaffold
- `docs/TECH_STACK.md` — consolidate into ARCHITECTURE.md
- `docs/TROUBLESHOOTING.md` — defer until pancetta ships
- `docs/USER_GUIDE.md` — defer until pancetta ships
- `pancetta-tui/FEATURES.md` — stale if exists
- `pancetta-tui/README.md` — stale if exists
- `pancetta-qso/README.md` — stale if exists

## Inline Doc Comment Standard

### Module-level (`//!` at top of lib.rs/mod.rs)

```rust
//! # crate-name
//!
//! One-line purpose.
//!
//! ## Data Flow
//! `UpstreamCrate` -> **this crate** -> `DownstreamCrate`
//!
//! ## Key Types
//! - [`MainStruct`] -- primary entry point, does X
//! - [`ImportantTrait`] -- implemented by Y
//!
//! ## Crate Relationships
//! - Receives from: `pancetta-audio`
//! - Sends to: `pancetta-ft8`
```

### Struct/trait level
One sentence of purpose. Document non-obvious fields only. Skip self-explanatory field names.

### Function level
Only when behavior isn't obvious from name + signature. Always document: panics, safety invariants, non-obvious return semantics.

## Auto-Update Mechanism

**Post-session doc sweep** via memory rule:

Before finishing a development session or after completing a major feature/fix, review and update documentation affected by the changes:

1. Inline doc comments on modified public items (if behavior changed)
2. CLAUDE.md if known gaps, build instructions, or project phases changed
3. ARCHITECTURE.md if crate relationships or data flows changed
4. README/FEATURES.md if user-facing capabilities changed

No hooks or gates — a disciplined habit enforced through AI memory.

## Implementation Scope

### Phase 1: Cleanup
- Delete 16+ stale docs
- Fix README placeholder URLs and stale content

### Phase 2: Rewrite Core Docs
- Rewrite ARCHITECTURE.md from scratch
- Rewrite README.md
- Create FEATURES.md

### Phase 3: Inline Docs
- Add/update module-level docs on all 11 crates
- Enable `#![warn(missing_docs)]`
- Add doc comments to undocumented public items (structs, traits, functions)

### Phase 4: Mechanism
- Save auto-update memory rule
- Update CLAUDE.md with doc maintenance expectations
