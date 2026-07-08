---
id: config-and-platform
title: Why the config-merge guardrail, hardware tiers, and decoder budgets?
kind: decision-digest
status: current
maintainer: agent
sources:
  - docs/DECISIONS/config-and-platform.md
  - pancetta-config/src/network.rs
  - pancetta/src/coordinator/effort.rs
verified:
  commit: eac7aab
  date: 2026-07-07
links:
  - overview
---
This covers three platform decisions: hand-written `ConfigSection::merge_with`
impls, hardware-tier auto-classification, and the budget-governed anytime
decoder. They exist because config layering must preserve operator overrides,
because decode thoroughness should scale to the host CPU, and because one decode
window must be stoppable early under a wall-clock budget yet still return
everything decoded so far. Full digest:
`docs/DECISIONS/config-and-platform.md`.

## Digest

The sharpest gotcha: **`merge_with` is a manually-maintained field list**, so a
struct field added later is silently dropped back to its compiled-in default (a
real, operator-reported bug fixed 2026-07-05). Nothing structurally enforces
that a new field gets a merge line — this class of bug can recur; add the
`self.field = other.field` line when you add a field. Tier presets and the
effort→budget mapping (`effort.rs`) are normative in the digest and code — cited,
not restated. Two known limitations (S3 escalation ordering; hot-reload does not
re-seed the effort budget) are documented there.
