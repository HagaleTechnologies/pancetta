# Documentation Overhaul Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace stale scaffold documentation with accurate, AI-agent-optimized docs across three layers: inline rustdoc, architecture docs, and human-facing README/FEATURES.

**Architecture:** Four phases — delete stale docs, rewrite core docs (README, FEATURES, ARCHITECTURE), add inline doc comments to all 11 crates, then set up the auto-update mechanism via memory. Each task is independent and produces a clean commit.

**Tech Stack:** Markdown, Rust doc comments (`//!`, `///`), cargo doc, git

---

## File Map

| Task | Action | Files |
|------|--------|-------|
| 1 | Delete | 19 stale doc files (see list below) |
| 2 | Rewrite | `README.md` |
| 3 | Create | `FEATURES.md` |
| 4 | Rewrite | `docs/ARCHITECTURE.md` |
| 5 | Modify | All 11 crate `lib.rs` files — module-level docs |
| 6 | Modify | All 11 crate `lib.rs` files — enable `#![warn(missing_docs)]` |
| 7 | Create/Modify | Memory file + `CLAUDE.md` |

---

### Task 1: Delete Stale Docs

**Files:**
- Delete: 19 files listed below

- [ ] **Step 1: Delete all stale documentation files**

```bash
git rm \
  docs/API.md \
  docs/API_SPECIFICATION.md \
  docs/BUILD_ANALYSIS_REPORT.md \
  docs/BUILD_COMPLETION_REPORT.md \
  docs/BUILD_STATUS_REPORT.md \
  docs/CRITICAL_FIXES.md \
  docs/EXECUTIVE_DECISION.md \
  docs/IMPLEMENTATION_PLAN.md \
  docs/INSTALL.md \
  docs/PRODUCT_REVIEW.md \
  docs/PROJECT_REVIEW_2026-03-31.md \
  docs/REGULATORY_COMPLIANCE.md \
  docs/REQUIREMENTS.md \
  docs/REQUIREMENTS_VALIDATION_REPORT.md \
  docs/TECH_STACK.md \
  docs/TROUBLESHOOTING.md \
  docs/USER_GUIDE.md \
  pancetta-tui/FEATURES.md \
  pancetta-tui/README.md \
  pancetta-qso/README.md
```

Some files may not exist — use `git rm -f --ignore-unmatch` for safety.

- [ ] **Step 2: Verify remaining docs/ contents**

```bash
ls docs/
```

Expected remaining files:
- `ARCHITECTURE.md` (will be rewritten in Task 4)
- `CONFIG.md` (keep)
- `CONTRIBUTING.md` (keep)
- `cqdx-api-requirements.md` (keep, already current)
- `superpowers/` directory (keep, plans and specs)
- `DECISIONS/` directory (keep if exists)

- [ ] **Step 3: Commit**

```bash
git commit -m "docs: delete 19 stale scaffold documentation files

These were generated during initial project setup and never updated.
Keeping only ARCHITECTURE.md (to be rewritten), CONFIG.md,
CONTRIBUTING.md, and cqdx-api-requirements.md.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Rewrite README.md

**Files:**
- Rewrite: `README.md`

**Context:** The current README has placeholder URLs (`yourusername`), generic feature descriptions, and a quick start section that doesn't reflect the actual build process. Read CLAUDE.md first for accurate build/test commands.

- [ ] **Step 1: Read current state**

Read `README.md` and `CLAUDE.md` to understand what's accurate vs stale.

- [ ] **Step 2: Rewrite README.md**

The README should contain these sections in this order. Read the actual codebase to fill in accurate details:

1. **Header** — Project name, one-line description ("Autonomous FT8 ham radio station written in Rust"), no emoji in heading
2. **What It Does** — 3-4 bullet points: decodes FT8, autonomous QSO operation, priority-based station selection, multi-stream TX. Brief, not a feature dump.
3. **Performance** — Table with actual metrics from the codebase (decoder sensitivity, decode speed, memory usage). Read `pancetta-ft8` tests and benchmarks for real numbers. If specific numbers aren't available, omit the table rather than guess.
4. **Quick Start** — From CLAUDE.md:
   ```bash
   cargo build
   cargo test
   cargo run
   ```
   Include the FT8-specific test commands from CLAUDE.md.
5. **Workspace Structure** — Copy the crate table from CLAUDE.md (it's accurate and maintained)
6. **Configuration** — Brief pointer to `docs/CONFIG.md` and `~/.pancetta/config.toml`
7. **License** — MIT (keep existing)

**Do NOT include:**
- Emoji in headings
- Badge images (unless the URLs are real)
- Placeholder URLs — use `https://github.com/HagaleTechnologies/pancetta`
- Features that don't exist yet (Web UI, Mobile, REST API)
- Aspirational performance numbers

- [ ] **Step 3: Verify links and commands**

Check that any `cargo` commands in the README actually work:
```bash
cargo build 2>&1 | tail -1
cargo test -p pancetta-ft8 --features transmit 2>&1 | tail -3
```

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs: rewrite README with accurate project state

Replaced scaffold README with real build commands, actual crate
structure, and working quick start. Removed placeholder URLs,
aspirational features, and unverified metrics.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Create FEATURES.md

**Files:**
- Create: `FEATURES.md`

**Context:** This is the human-facing "bragsheet" — what pancetta can do, organized by capability. Read the codebase (especially CLAUDE.md, pancetta-ft8, pancetta-qso, pancetta-dsp) to describe actual capabilities accurately.

- [ ] **Step 1: Read key crates for feature details**

Read these files to understand actual capabilities:
- `CLAUDE.md` — project overview, architecture highlights
- `pancetta-ft8/src/lib.rs` — decoder capabilities (first 50 lines)
- `pancetta-qso/src/autonomous.rs` — autonomous operator modes (first 50 lines)
- `pancetta-qso/src/priority.rs` — priority scoring system (first 50 lines)
- `pancetta-qso/src/frequency.rs` — SmartFrequencyAllocator (first 50 lines)
- `pancetta-dsp/src/lib.rs` — DSP pipeline description (first 30 lines)

- [ ] **Step 2: Write FEATURES.md**

Organize by capability area:

```markdown
# Pancetta Features

## FT8 Decoder

[Description of decoder capabilities — LDPC decoding, OSD, AP decoding, sensitivity stats. Read pancetta-ft8 for real numbers.]

## Autonomous Operator

[Hunt mode, CQ mode, hybrid mode. Priority-based station selection. Read pancetta-qso/src/autonomous.rs.]

## Priority Scoring Engine

[Weighted scoring: needed DXCC > needed grid > POTA/SOTA > rarity. Duplicate suppression, failure backoff. Band-aware dedup. Read pancetta-qso/src/priority.rs.]

## Multi-Stream TX

[SmartFrequencyAllocator, 7 soft-scored criteria, parallel QSOs at different audio frequencies. Read pancetta-qso/src/frequency.rs.]

## DSP Pipeline

[Real-time decimation, bandpass filtering, FT8 windowing. Read pancetta-dsp.]

## Hardware Integration

[Hamlib CAT control, FTdx10 support via rigctld. Read pancetta-hamlib.]

## TUI

[Terminal interface with waterfall display, band activity, DX hunter, QSO status. Read pancetta-tui.]

## cqdx.io Integration

[Rarity scoring, needed DXCC/grid lookups, live spots. Read pancetta-cqdx.]
```

Each section should be 3-6 sentences describing what the feature does and what makes it notable. Do NOT list features that don't work yet — only describe what's actually implemented and tested.

- [ ] **Step 3: Commit**

```bash
git add FEATURES.md
git commit -m "docs: create FEATURES.md capability showcase

Organized by capability area: decoder, autonomous operator, priority
scoring, multi-stream TX, DSP, hardware, TUI, cqdx.io integration.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Rewrite docs/ARCHITECTURE.md

**Files:**
- Rewrite: `docs/ARCHITECTURE.md`

**Context:** The current file describes a hexagonal/DDD/CQRS architecture with REST API, Web UI, and Mobile — none of which exist. It needs to be completely rewritten to describe the actual architecture. Read CLAUDE.md for accurate crate descriptions.

- [ ] **Step 1: Read the actual architecture**

Read these files:
- `CLAUDE.md` — workspace structure table, architecture highlights
- `pancetta/src/coordinator/mod.rs` — first 50 lines (coordinator overview)
- `pancetta/src/coordinator/pipeline.rs` — first 50 lines (data pipeline)

- [ ] **Step 2: Rewrite docs/ARCHITECTURE.md**

The document should contain (~150-200 lines):

**Section 1: Crate Dependency Graph**

```
Layer 0 (no internal deps):
  pancetta-core    — shared types, error handling
  pancetta-audio   — real-time audio I/O (cpal + ringbuf)
  pancetta-ft8     — FT8 encoder/decoder/modulator/OSD
  pancetta-dsp     — DSP pipeline (FFT, filtering, resampling)
  pancetta-tui     — terminal UI (ratatui)
  pancetta-config  — configuration with hot-reload

Layer 1 (depends on core/ft8):
  pancetta-qso     — QSO management, priority scoring, autonomous operator
  pancetta-hamlib  — Hamlib CAT control FFI
  pancetta-dx      — DX hunting, DXCC, PSKReporter
  pancetta-cqdx    — cqdx.io HTTP client

Layer 2 (orchestrator):
  pancetta         — coordinator, message bus, runtime (depends on everything)
```

**Section 2: End-to-End Data Flow**

Describe the pipeline from audio input to QSO completion:

```
Audio In (cpal) → pancetta-audio → raw 48kHz stereo samples
  → pancetta-dsp → decimation (4:1), bandpass filter → 12kHz mono
  → 15-second FT8 windows → pancetta-ft8 → LDPC decode, OSD, AP
  → decoded messages → Coordinator → pancetta-qso autonomous operator
  → decision (respond/CQ/ignore) → pancetta-ft8 encoder
  → 8-GFSK modulated audio → pancetta-audio → Audio Out
  → pancetta-hamlib → PTT control via rigctld
```

**Section 3: Coordinator**

Describe the coordinator's role: central orchestrator in `pancetta/src/coordinator/`. Decomposed into submodules: `mod.rs` (main), `pipeline.rs` (audio/DSP/FT8 pipeline), `components.rs` (QSO/hamlib/cqdx startup), `hamlib.rs`, `health.rs`, `shutdown.rs`, `wav_playback.rs`, `util.rs`. Message bus connects all components.

**Section 4: Key Abstractions**

- `WorkedStationLookup` trait (priority.rs) — band-aware duplicate detection, rarity, needed DXCC/grid
- `PriorityScorer` — weighted scoring engine
- `AutonomousOperator` — hunt/CQ/hybrid decision engine
- `SmartFrequencyAllocator` — TX frequency selection with 7 soft criteria
- `CachedStationLookup` — coordinator-level bridge between cqdx.io data and the scoring engine

**Do NOT include:**
- Hexagonal architecture, DDD, CQRS (not how pancetta is built)
- REST/WS API, Web UI, Mobile (don't exist)
- UML diagrams or overly formal notation

- [ ] **Step 3: Commit**

```bash
git add docs/ARCHITECTURE.md
git commit -m "docs: rewrite ARCHITECTURE.md to reflect actual system

Replaced stale hexagonal/DDD scaffold with real crate dependency
graph, end-to-end data flow, coordinator description, and key
abstractions. ~150 lines of accurate architecture documentation.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Inline Module-Level Docs for All Crates

**Files:**
- Modify: `pancetta-core/src/lib.rs`
- Modify: `pancetta-audio/src/lib.rs`
- Modify: `pancetta-ft8/src/lib.rs`
- Modify: `pancetta-dsp/src/lib.rs`
- Modify: `pancetta-tui/src/lib.rs`
- Modify: `pancetta-config/src/lib.rs`
- Modify: `pancetta-qso/src/lib.rs`
- Modify: `pancetta-hamlib/src/lib.rs`
- Modify: `pancetta-dx/src/lib.rs`
- Modify: `pancetta-cqdx/src/lib.rs`
- Modify: `pancetta/src/lib.rs`

**Context:** Most crates already have `//!` docs but they vary in quality. The goal is to ensure every crate has the standard template with Data Flow, Key Types, and Crate Relationships sections. Crates with good existing docs (ft8, dsp, hamlib, config) just need the template sections added if missing. Crates with minimal docs (cqdx, pancetta main) need full rewrites.

- [ ] **Step 1: Read each crate's lib.rs and understand its role**

For each of the 11 crates, read the first 50 lines of `lib.rs` and any `mod.rs` files to understand what the crate does, what its key types are, and how it connects to other crates.

- [ ] **Step 2: Update/add module-level docs**

For each crate, ensure the `//!` doc block at the top of `lib.rs` follows this template:

```rust
//! # crate-name
//!
//! One-line purpose description.
//!
//! ## Data Flow
//! `UpstreamCrate` -> **this crate** -> `DownstreamCrate`
//!
//! ## Key Types
//! - [`MainType`] -- what it does
//! - [`ImportantTrait`] -- what it's for
//!
//! ## Crate Relationships
//! - Receives from: `pancetta-X`
//! - Sends to: `pancetta-Y`
```

Specific guidance per crate:

**pancetta-core** — Foundational types and errors. No upstream/downstream (everything depends on it). Key types: error types, shared domain types.

**pancetta-audio** — Real-time audio I/O via cpal. Receives from: hardware audio device. Sends to: `pancetta-dsp` (raw samples). Key types: `AudioManager`, `AudioManagerConfig`.

**pancetta-ft8** — FT8 protocol implementation. Receives from: `pancetta-dsp` (FT8 windows). Sends to: coordinator (decoded messages). Also encodes TX messages. Key types: `Ft8Decoder`, `Ft8Encoder`, `Ft8Config`.

**pancetta-dsp** — DSP pipeline. Receives from: `pancetta-audio` (48kHz stereo). Sends to: `pancetta-ft8` (12kHz mono FT8 windows). Key types: `DspPipeline`, `BandpassFilter`.

**pancetta-tui** — Terminal UI. Receives from: coordinator (decoded messages, waterfall data, QSO state). Sends to: coordinator (user commands). Key types: `App`, `TuiRunner`, `Waterfall`.

**pancetta-config** — Configuration with hot-reload. Used by: all crates at startup. Key types: `Config`, `ConfigLoader`.

**pancetta-qso** — QSO management and decision engine. Receives from: coordinator (decoded messages). Sends to: coordinator (TX decisions). Key types: `AutonomousOperator`, `PriorityScorer`, `SmartFrequencyAllocator`, `QsoStateMachine`.

**pancetta-hamlib** — Hamlib CAT control. Receives from: coordinator (PTT/freq commands). Sends to: coordinator (rig state). Key types: `Rig`, `RigBuilder`.

**pancetta-dx** — DX hunting utilities. Receives from: `pancetta-cqdx` or local data. Sends to: `pancetta-qso` (rarity scores). Key types: `RarityScorer`, `DxccEntity`.

**pancetta-cqdx** — cqdx.io HTTP client. Receives from: cqdx.io API. Sends to: coordinator (entities, spots, rarity). Key types: `CqdxClient`, `CqdxCache`, `SpotGroup`.

**pancetta** (main lib) — Coordinator and runtime. Orchestrates all crates. Key types: `Coordinator`, `CachedStationLookup`.

**Important:** Do NOT delete existing useful doc content. Merge the template sections into what's already there. If a crate already has excellent docs (e.g., pancetta-ft8 with 50+ lines), just add the Data Flow / Crate Relationships sections if missing.

- [ ] **Step 3: Verify docs compile**

```bash
touch */src/lib.rs pancetta/src/lib.rs
cargo doc --no-deps 2>&1 | tail -5
```

Expected: builds without errors.

- [ ] **Step 4: Commit**

```bash
git add */src/lib.rs pancetta/src/lib.rs
git commit -m "docs: add standardized module-level docs to all 11 crates

Every crate now has Data Flow, Key Types, and Crate Relationships
sections in its lib.rs doc comments. AI agents reading any crate
immediately know where it sits in the pipeline.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Enable `#![warn(missing_docs)]`

**Files:**
- Modify: All 11 crate `lib.rs` files

**Context:** Only pancetta-hamlib currently enforces `#![deny(missing_docs)]`. Two crates have it commented out with TODO. The goal is to enable `#![warn(missing_docs)]` in all crates so the compiler flags undocumented public items going forward. This will produce warnings, not errors — existing undocumented items won't break the build.

- [ ] **Step 1: Enable warn(missing_docs) in all crates**

For each of the 11 crate `lib.rs` files:
- If there's a commented-out `// #![warn(missing_docs)]`, uncomment it
- If there's no missing_docs lint at all, add `#![warn(missing_docs)]` near the top (after any existing `#![...]` attributes)
- If there's already `#![deny(missing_docs)]` (pancetta-hamlib), leave it as deny

- [ ] **Step 2: Build and count warnings**

```bash
touch */src/lib.rs pancetta/src/lib.rs
cargo build 2>&1 | grep "missing_docs" | wc -l
```

Report the count. These warnings are expected and acceptable — they'll be addressed incrementally as code is touched, not all at once.

- [ ] **Step 3: Commit**

```bash
git add */src/lib.rs pancetta/src/lib.rs
git commit -m "chore: enable #![warn(missing_docs)] in all crates

Compiler will now warn on undocumented public items. Existing
warnings will be addressed incrementally as code is modified.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: Auto-Update Mechanism

**Files:**
- Create: `~/.claude/projects/-Users-thagale-Code-pancetta/memory/feedback_doc_updates.md`
- Modify: `~/.claude/projects/-Users-thagale-Code-pancetta/memory/MEMORY.md`
- Modify: `CLAUDE.md`

- [ ] **Step 1: Save doc-update memory rule**

Write to `~/.claude/projects/-Users-thagale-Code-pancetta/memory/feedback_doc_updates.md`:

```markdown
---
name: Update docs with every significant change
description: After completing major work, review and update affected documentation before finishing
type: feedback
---

Before finishing a development session or after completing a major feature/fix, review and update documentation affected by the changes:

1. Inline doc comments on modified public items (if behavior changed)
2. CLAUDE.md if known gaps, build instructions, or project phases changed
3. docs/ARCHITECTURE.md if crate relationships or data flows changed
4. README.md or FEATURES.md if user-facing capabilities changed

**Why:** Documentation was stale for months before a major overhaul. Incremental updates prevent drift.

**How to apply:** After the final commit of a feature/fix, check if any docs need updating. Include doc updates in the same commit or a follow-up commit. Don't skip this for "small" changes — small changes accumulate.
```

- [ ] **Step 2: Add pointer to MEMORY.md**

Add to `~/.claude/projects/-Users-thagale-Code-pancetta/memory/MEMORY.md`:

```
- [Doc Updates](feedback_doc_updates.md) — update docs (inline, CLAUDE.md, ARCHITECTURE.md) with every significant change
```

- [ ] **Step 3: Add doc maintenance section to CLAUDE.md**

Add a new section to `CLAUDE.md` after "Build Hygiene":

```markdown
## Documentation Maintenance

After completing significant work, update affected documentation:

- **Inline docs**: Update `///` and `//!` comments on modified public items
- **CLAUDE.md**: Update known gaps, build instructions, or project phases
- **docs/ARCHITECTURE.md**: Update if crate relationships or data flows changed
- **README.md / FEATURES.md**: Update if user-facing capabilities changed

All crates have `#![warn(missing_docs)]` enabled — the compiler will flag undocumented public items.
```

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: add documentation maintenance guidelines to CLAUDE.md

Establishes post-session doc sweep habit: update inline docs,
CLAUDE.md, ARCHITECTURE.md, and README/FEATURES as needed.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```
