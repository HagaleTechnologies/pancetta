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
  - pancetta/src/coordinator/dsp.rs
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

Effort-override gotcha (PAN-158): **every `Ft8Config` field `apply_effort_overrides`
writes must also be a `DecoderRebuildKey` field** (`coordinator/ft8.rs`), or the hot
loop adopts the new budget but keeps decoding with the old decoder. Eco and Standard
had identical keys until `fine_sync_enabled` joined it; the
`every_effort_override_field_is_a_rebuild_trigger` guard catches a missing one. Eco's
fine sync is also budget-qualified: it keys on the published budget, not the config,
so it applies only while the 1 ms floor-only budget is published. Eco with a
`budget_ms` override other than 1 runs without it until a live effort cycle publishes
Eco's own 1 ms; an explicit `budget_ms = 1` keeps it on. See
[Eco-preset fine sync](../../docs/DECISIONS/config-and-platform.md#eco-preset-fine-sync-pan-158-2026-10-25).

Second gotcha (PAN-90): **a new config struct needs container-level
`#[serde(default)]` and a `Default` impl**, or `pancetta-config/tests/partial_tables.rs`
fails — every table must accept a partial table. Record/map entry types are the
only allowlisted exceptions. Config snippets added to `README.md` or top-level
`docs/*.md` are loaded by `tests/docs_snippets.rs` and must parse with zero warnings.

Queue recovery gotcha (PAN-150): restoring a missing native queue rule does not
schedule a retry. Rerun the failed handler after restoration. An interrupted
bot-owned pause can repair its missing provenance marker on handoff rerun;
human pauses remain in force. See
[Interrupted native queue recovery](../../docs/DECISIONS/config-and-platform.md#interrupted-native-queue-recovery-pan-150-2026-10-20)
for ownership checks and recovery limits.

Toolchain gotcha (PAN-56): **`rust-toolchain.toml` pins an exact Rust release
(`1.98.0`, matching the Catalyst runner image), and the `dtolnay/rust-toolchain`
steps in `ci.yml`, `docs.yml`, `demo-assets.yml` and `release.yml` pass the same
version as `toolchain:`.** Bump the channel and all of those inputs in one PR. A missed input still builds with the
pinned compiler, because rustup's override wins, but that job downloads a second
toolchain and its rust-cache key churns. See
[Exact Rust toolchain pin](../../docs/DECISIONS/config-and-platform.md#exact-rust-toolchain-pin-pan-56-2026-10-21).

DSP gotcha (PAN-70): **any change between two known dial readings, even 1 Hz
within a band, discards the DSP's in-flight audio**, because each decode window
carries one dial snapshot. A 0 read is "unknown" and changes nothing, and only a
band change also resets the waterfall noise floor. The guarantee covers
transitions the DSP observes between reads. Audio batches carry no dial tag,
so queued audio and a retune and return between two reads are not caught.
Retuning can cost a decode window. Don't add a deadband without measured CAT
jitter. See
[Dial-change in-flight-window flush](../../docs/DECISIONS/config-and-platform.md#dial-change-in-flight-window-flush-pan-70-2026-10-24).
