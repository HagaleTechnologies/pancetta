# PAN-54: Decode Priority Hygiene Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop structurally-bogus decodes (the unresolved AP-hash placeholder `<...>`, a Maidenhead grid square mistaken for a callsign, and the operator's own callsign in compound or hash-render form) from ever outranking a real station in pancetta's priority list, and close the two concrete bugs that let them through today.

**Architecture:** Four small, independently-testable fixes at the lowest shared layer each bug actually lives in:
1. A structural callsign-plausibility check (`pancetta_core::callsign::is_plausible_callsign`), reusing the crate's existing `resolve_hash_render`/`is_grid_shape` machinery.
2. A one-line fix in `pancetta-qso`'s decode-admission filter (`callsign_continuity.rs`) that currently lets a bare grid square masquerade as a pseudo-callsign, which is how a solo `<...>` CQ earns false "observed" trust and survives to broadcast.
3. A new bottom tier (`PriorityTier::Suspect`) plus a score clamp in `pancetta-qso/src/priority.rs` — the single formula both the autonomous engine's continuous `dx_score` sort and the TUI's tiered display call into — so anything that fails the plausibility check is forced to the bottom regardless of what `WorkedStationLookup` says about it.
4. A one-line fix in the autonomous engine's CQ-candidate loop that swaps a plain string-equality self-callsign check for the crate's own compound/hash-render-aware `callsigns_match`, which was already imported and used two lines away but skipped here.

**Tech Stack:** Rust workspace (pancetta-core, pancetta-qso, pancetta-tui crates), `cargo test`.

**Spec:** Linear ticket [PAN-54](https://linear.app/hagaletechnologies/issue/PAN-54) ("Bad/unworkable decodes surface at high priority"). This plan **corrects one of that ticket's four findings** based on deeper investigation — see "Deviation from the ticket" below — and **intentionally narrows scope** on a fifth, harder problem the ticket didn't originally call out. Both are explained below so the reasoning travels with the plan.

## Deviation from the ticket

PAN-54 finding #4 theorized the own-callsign filter goes stale after a live config hot-reload (`Arc<RwLock<Config>>` cached once at task-spawn). Investigation found **no hot-reload manager is ever instantiated in the `pancetta` binary** — `pancetta-config::hot_reload::HotReloadManager` is a real, tested library feature, but nothing in `pancetta/src/coordinator/` ever constructs or starts one. Every config value, not just the callsign, is read once at process start and requires a restart to change — that's consistent, not a callsign-specific bug. The real, provable bug in this area is narrower and worse: the CQ-candidate loop's self-callsign check (`autonomous.rs:1519`) uses plain `eq_ignore_ascii_case`, while the *exact same file* already imports and uses the compound/hash-render-aware `callsigns_match` two call sites away (`autonomous.rs:1364`, `:1371`) for QSO-partner matching. `callsigns_match` resolves an i3=4 nonstandard-callsign hash-render (`"<W5AU>"`) to the plain callsign it represents before comparing — plain `eq_ignore_ascii_case` does not. A self-decode that the FT8 decoder renders as a resolved hash (`<W5AU>`) or a compound form (`W5AU/P`) sails past the current check and gets treated as a workable third-party CQ. This is Task 4 below, and is the most likely concrete explanation for the operator's reported "K5ARH showed up in the list, while K5ARH was still configured" sighting.

## Deliberately out of scope

The ticket's "plausible-but-likely-bogus callsign" example (`WX0E`) is, if it's genuinely decoder noise rather than a real ham, **structurally indistinguishable from a real callsign** — it has the right shape, so no shape-based check (this plan's `is_plausible_callsign`) can catch it. The codebase already has infrastructure built for exactly this discrimination problem (`pancetta-qso/src/content_score.rs`'s `MessageContentScore`, AUC 0.886 on a 6969-sample corpus) but it is currently wired *only* into the autonomous TX-decision gate (`autonomous.rs:2164-2230`), not into `score_cq_detailed`/`classify_tier`. Wiring it into scoring/display would require widening `PriorityScorer`'s public signature and touching `DecodedMessageInfo`, `DecodedMessageView`, and whichever coordinator call site precomputes `DecodedMessageView.priority_score` — a materially larger, separate change. This plan fixes everything that's a clear bug or a pure-shape problem; recommend filing a follow-up ticket for content-score-based display ranking once this lands.

## Global Constraints

- `pancetta-qso` already depends on `pancetta-core` (`pancetta-qso/Cargo.toml:15`) — no new Cargo dependency needed anywhere in this plan.
- `PriorityTier` derives `Ord` from **declaration order** (documented in its own doc comment) — inserting a new lowest tier means declaring it *first*, which shifts every other variant's `as_display_u32()` display band by +1000. Task 3 must find and fix every hardcoded band-boundary literal this shifts, not just the ones this plan already found.
- Every new/changed function gets a real unit test in the same file's existing `#[cfg(test)] mod tests` block, following that file's existing test style — no new test infrastructure.
- Run `cargo test -p pancetta-core -p pancetta-qso -p pancetta-tui` after each task; do not move to the next task with red tests.

---

## Task 1: Add `is_plausible_callsign` to `pancetta-core`

**Files:**
- Modify: `pancetta-core/src/callsign.rs`
- Test: `pancetta-core/src/callsign.rs` (existing `#[cfg(test)] mod tests` block at line 129)

**Interfaces:**
- Consumes: nothing new — uses the file's existing private `resolve_hash_render(call: &str) -> Option<&str>` (line 121-127).
- Produces: `pub fn is_grid_shape(t: &str) -> bool` and `pub fn is_plausible_callsign(callsign: &str) -> bool`, both used by Task 2 (`pancetta-qso/src/callsign_continuity.rs`) and Task 3 (`pancetta-qso/src/priority.rs`).

- [ ] **Step 1: Write the failing tests**

Add to `pancetta-core/src/callsign.rs`'s existing `mod tests` block (after the `callsigns_match_identical_and_compound_equivalence` test, before its closing brace's sibling tests):

```rust
    // --- is_grid_shape / is_plausible_callsign ------------------------------

    #[test]
    fn is_grid_shape_matches_maidenhead_field_square() {
        assert!(is_grid_shape("FN42"));
        assert!(is_grid_shape("PM95"));
        assert!(!is_grid_shape("FN4")); // too short
        assert!(!is_grid_shape("FN42A")); // too long (6-char grid, not 4)
        assert!(!is_grid_shape("W1ABC")); // not grid-shaped at all
        assert!(!is_grid_shape("44NN")); // digits/letters swapped
    }

    #[test]
    fn is_plausible_callsign_accepts_real_shapes() {
        assert!(is_plausible_callsign("W5AU"));
        assert!(is_plausible_callsign("g8bcg")); // case-insensitive
        assert!(is_plausible_callsign("  K1ABC/P  ")); // trimmed, portable suffix
        assert!(is_plausible_callsign("<W5AU>")); // resolved AP-hash render
    }

    #[test]
    fn is_plausible_callsign_rejects_placeholder_and_noise() {
        assert!(!is_plausible_callsign("")); // empty
        assert!(!is_plausible_callsign("<...>")); // unresolved AP-hash placeholder
        assert!(!is_plausible_callsign("FN42")); // grid square, not a callsign
        assert!(!is_plausible_callsign("K")); // too short / no digit
        assert!(!is_plausible_callsign("12345")); // no letters
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-core --lib callsign:: -- --nocapture`
Expected: FAIL with "cannot find function `is_grid_shape`" / "cannot find function `is_plausible_callsign`" (not yet defined).

- [ ] **Step 3: Implement `is_grid_shape` and `is_plausible_callsign`**

Add to `pancetta-core/src/callsign.rs`, after `resolve_hash_render` (after line 127, before the `#[cfg(test)]` block):

```rust
/// Is `t` shaped like a bare 4-character Maidenhead grid square (two field
/// letters, two square digits — e.g. `FN42`)? Promoted from
/// `pancetta_qso::callsign_continuity`'s identical private check so both
/// crates share one definition (PAN-54).
pub fn is_grid_shape(t: &str) -> bool {
    let chars: Vec<char> = t.chars().collect();
    chars.len() == 4
        && chars[0].is_ascii_alphabetic()
        && chars[1].is_ascii_alphabetic()
        && chars[2].is_ascii_digit()
        && chars[3].is_ascii_digit()
}

/// Is `callsign` structurally plausible as a real amateur-radio callsign,
/// as opposed to decoder noise or a placeholder token?
///
/// This is a SHAPE check, not a semantic one — it cannot and does not
/// detect a well-formed decode that happens to be a false positive (that
/// discrimination is `pancetta_qso::content_score`'s job). It exists so a
/// decode that cannot possibly BE a real callsign never outranks a genuine
/// station in `pancetta_qso::priority`'s scoring, regardless of what a
/// coincidental DXCC-prefix/rarity lookup says about it (PAN-54).
///
/// Rejects: empty/whitespace-only input, the unresolved AP-hash placeholder
/// `"<...>"`, anything under 3 or over 10 characters (once hash-resolved),
/// anything without at least one digit AND one letter, and a bare 4-char
/// Maidenhead grid square mistaken for a callsign.
pub fn is_plausible_callsign(callsign: &str) -> bool {
    let upper = callsign.trim().to_uppercase();
    let Some(resolved) = resolve_hash_render(&upper) else {
        return false;
    };
    let len = resolved.len();
    if !(3..=10).contains(&len) {
        return false;
    }
    if is_grid_shape(resolved) {
        return false;
    }
    let has_digit = resolved.bytes().any(|b| b.is_ascii_digit());
    let has_alpha = resolved.bytes().any(|b| b.is_ascii_alphabetic());
    has_digit && has_alpha
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta-core --lib callsign:: -- --nocapture`
Expected: PASS, all `is_grid_shape_*` and `is_plausible_callsign_*` tests green.

- [ ] **Step 5: Commit**

```bash
git add pancetta-core/src/callsign.rs
git commit -m "feat(pancetta-core): add is_plausible_callsign structural gate (PAN-54)"
```

---

## Task 2: Close the `<...>`-with-grid trust-set leak

**Files:**
- Modify: `pancetta-qso/src/callsign_continuity.rs`
- Test: `pancetta-qso/src/callsign_continuity.rs` (existing test module)

**Interfaces:**
- Consumes: `pancetta_core::callsign::is_grid_shape` (Task 1).
- Produces: no new public API — this is a behavior fix inside the existing `pub fn callsigns_in` / `pub fn accept` (unchanged signatures).

**Context:** `callsigns_in()` takes up to 2 tokens after `CQ`/a CQ-modifier and keeps each one that `looks_like_callsign()` accepts. For a decode like `"CQ <...> FN42"`, the first token (`<...>`) is correctly rejected (no digit, no letter) but the *second* token — the grid square `FN42` — passes `looks_like_callsign` (it has both a digit and a letter) and gets returned as if it were a second candidate callsign. That pseudo-callsign then gets recorded into the filter's `observed`/`rolling` trust sets (`note_window_raw_calls`, called from `pancetta/src/coordinator/ft8.rs:2170`), which is how a *repeating* solo `<...>`-with-grid CQ earns false "observed" continuity and survives `accept()` to be broadcast to every consumer (TUI, autonomous engine, PSKReporter, cqdx). Fixing `looks_like_callsign` to reject grid-shaped tokens closes this at its root: `callsigns_in("CQ <...> FN42")` becomes `[]`, and `accept()` already rejects any message with `calls.is_empty()` (line 343-345) outside the very short cold-start window.

- [ ] **Step 1: Write the failing tests**

Add to `pancetta-qso/src/callsign_continuity.rs`'s `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn callsigns_in_does_not_treat_a_grid_square_as_a_pseudo_callsign() {
        // The unresolved AP-hash placeholder plus a trailing grid square must
        // extract to NO callsigns at all, not "the grid square as a fake one".
        assert_eq!(callsigns_in("CQ <...> FN42"), Vec::<String>::new());
    }

    #[test]
    fn accept_does_not_grant_continuity_via_grid_square_pseudo_callsign() {
        // Strict mode (cold_start_threshold = 0). Mirrors the real pipeline's
        // per-window ordering (coordinator/ft8.rs:2168-2170): `accept()` runs
        // against the trust state built by PRIOR windows, then
        // `note_window_raw_calls` records this window's raw text for the
        // NEXT window.
        let filter = CallsignContinuityFilter::new(500);
        let msg = "CQ <...> FN42".to_string();

        // Window 1: nothing in any trust set yet — rejected.
        assert!(!filter.accept(&msg));
        filter.note_window_raw_calls(std::slice::from_ref(&msg));

        // Window 2: before the fix, "FN42" was wrongly extracted as a
        // pseudo-callsign in window 1 and recorded into `observed` by
        // `note_window_raw_calls` above — this second `accept()` call would
        // incorrectly return `true` via `in_observed`.
        assert!(!filter.accept(&msg));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-qso --lib callsign_continuity:: -- --nocapture`
Expected: FAIL on both new tests — `callsigns_in("CQ <...> FN42")` currently returns `["FN42"]` (not empty), and in `accept_does_not_grant_continuity_via_grid_square_pseudo_callsign`, window 2's `accept()` call currently returns `true` (the `FN42` pseudo-callsign recorded into `observed` during window 1 satisfies `in_observed` on the second call), so the second `assert!` fails.

- [ ] **Step 3: Implement the fix**

In `pancetta-qso/src/callsign_continuity.rs`, delete the local `is_grid_shape` function (lines 98-105):

```rust
fn is_grid_shape(t: &str) -> bool {
    let chars: Vec<char> = t.chars().collect();
    chars.len() == 4
        && chars[0].is_ascii_alphabetic()
        && chars[1].is_ascii_alphabetic()
        && chars[2].is_ascii_digit()
        && chars[3].is_ascii_digit()
}
```

Add this import near the top (after the existing `use std::collections::{HashSet, VecDeque};` / `use std::path::Path;` / `use std::sync::RwLock;` block) — bare name, so `has_high_risk_fp_pattern`'s existing degenerate-grid check (which already calls `is_grid_shape` unqualified) keeps working unchanged:

```rust
use pancetta_core::callsign::is_grid_shape;
```

Then in `looks_like_callsign` (originally lines 151-168), add the grid exclusion right after the length check:

```rust
fn looks_like_callsign(t: &str) -> bool {
    let len = t.len();
    if !(3..=10).contains(&len) {
        return false;
    }
    if is_grid_shape(t) {
        return false;
    }
    let mut has_digit = false;
    let mut has_alpha = false;
    for c in t.chars() {
        if c.is_ascii_digit() {
            has_digit = true;
        } else if c.is_ascii_alphabetic() {
            has_alpha = true;
        } else if c != '/' {
            return false;
        }
    }
    has_digit && has_alpha
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta-qso --lib callsign_continuity:: -- --nocapture`
Expected: PASS, including every pre-existing test in this file (confirm no regression — `has_high_risk_fp_pattern`'s grid-degenerate tests must still pass since they call the same, now-promoted, `is_grid_shape`).

- [ ] **Step 5: Commit**

```bash
git add pancetta-qso/src/callsign_continuity.rs
git commit -m "fix(pancetta-qso): stop grid square masquerading as a pseudo-callsign (PAN-54)"
```

---

## Task 3: Gate priority scoring on `is_plausible_callsign`

**Files:**
- Modify: `pancetta-qso/src/priority.rs`
- Modify: `pancetta-tui/src/app.rs:4131` and `:5471` (band-boundary literals that shift when `Suspect` is inserted)
- Test: `pancetta-qso/src/priority.rs` (existing test module)

**Interfaces:**
- Consumes: `pancetta_core::callsign::is_plausible_callsign` (Task 1).
- Produces: new `PriorityTier::Suspect` variant (used nowhere outside this crate today, but is part of `PriorityTier`'s existing public enum, so any external match arms would need `_ =>` — grep confirmed none exist outside this crate).

**Context:** This is the single shared formula both consumers ultimately call into: the autonomous engine's `pending_cqs.sort_by(dx_score desc)` (`autonomous.rs:1560-1564`) sorts by `PriorityScorer::score_cq_detailed(...).total` via the `DxEvaluator::evaluate_cq` trait method, and the coordinator precomputes `DecodedMessageView.priority_score` (consumed by the TUI's primary display path, `app.rs:1824`) via the same `PriorityScorer::score_tiered`/`classify_tier` the TUI also calls directly as its no-precompute fallback (`app.rs:1837-1843`). Fixing `priority.rs` once fixes both.

- [ ] **Step 1: Write the failing tests**

Add to `pancetta-qso/src/priority.rs`'s `#[cfg(test)] mod tests` block, near the other `classify_tier_*` tests:

```rust
    #[test]
    fn classify_tier_implausible_callsign_is_suspect_even_when_needed_and_atno() {
        // A garbage decode that happens to resolve to an "unworked" DXCC via
        // WorkedStationLookup must NOT escape the bottom tier.
        let mut lookup = TestLookup::new();
        lookup.needed_dxcc.insert("<...>".to_string());
        lookup.atno.insert("<...>".to_string());
        let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));
        let score = scorer.score_tiered("<...>", Some("FN42"), 0, 14_074_000.0);
        assert_eq!(score.tier, PriorityTier::Suspect);
    }

    #[test]
    fn classify_tier_grid_shaped_callsign_is_suspect() {
        let lookup = TestLookup::new();
        let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));
        let score = scorer.score_tiered("FN42", None, 0, 14_074_000.0);
        assert_eq!(score.tier, PriorityTier::Suspect);
    }

    #[test]
    fn score_cq_detailed_implausible_callsign_totals_zero_even_when_needed_and_atno() {
        let mut lookup = TestLookup::new();
        lookup.needed_dxcc.insert("<...>".to_string());
        lookup.atno.insert("<...>".to_string());
        let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));
        let breakdown = scorer.score_cq_detailed("<...>", Some("FN42"), 0, 14_074_000.0);
        assert_eq!(breakdown.total, 0.0);
    }

    #[test]
    fn suspect_tier_sorts_below_standard() {
        let suspect = TieredScore {
            tier: PriorityTier::Suspect,
            secondary: 1.0, // even at its own max, must stay below Standard's min
        };
        let weak_standard = TieredScore {
            tier: PriorityTier::Standard,
            secondary: 0.0,
        };
        assert!(suspect < weak_standard);
        assert!(suspect.as_display_u32() < weak_standard.as_display_u32());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-qso --lib priority:: -- --nocapture`
Expected: FAIL — `PriorityTier::Suspect` doesn't exist yet (compile error).

- [ ] **Step 3: Implement the `Suspect` tier and gate**

In `pancetta-qso/src/priority.rs`, add the import at the top (after `use serde::{Deserialize, Serialize};`):

```rust
use pancetta_core::callsign::is_plausible_callsign;
```

Replace the `PriorityTier` enum and its doc comment (originally lines 41-57):

```rust
/// Lexicographic priority tier (#164 redesign, `Suspect` added PAN-54).
/// Declaration order below is ascending priority — Rust's derived `Ord` on
/// a fieldless enum ranks by declaration order, so `Atno > PerBandDxccNew >
/// SpecialStation > PerBandGridNew > Standard > Suspect` falls out for free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PriorityTier {
    /// Tier 6 (lowest): callsign failed `is_plausible_callsign` — decoder
    /// noise, an unresolved AP-hash placeholder (`<...>`), or a token that
    /// isn't callsign-shaped at all. Never outranks a real station
    /// regardless of what `WorkedStationLookup` says about it (PAN-54).
    Suspect,
    /// Tier 5: everything else, varying only by rarity/signal quality.
    Standard,
    /// Tier 4: per-band grid-square new-one.
    PerBandGridNew,
    /// Tier 3: special stations (event/gov/research/UN).
    SpecialStation,
    /// Tier 2: per-band DXCC new-one (never worked this entity on this band).
    PerBandDxccNew,
    /// Tier 1 (highest): all-time new one — never worked on any band.
    Atno,
}
```

Update `as_display_u32`'s doc comment (originally lines 70-76):

```rust
    /// Encode as a single sortable u32 for display: tier dominates via a
    /// 1000-wide band per tier, secondary breaks ties within a tier.
    /// Ranges (PAN-54 shifted every band +1000 to make room for Suspect):
    /// Suspect 0-999, Standard 1000-1999, PerBandGridNew 2000-2999,
    /// SpecialStation 3000-3999, PerBandDxccNew 4000-4999, Atno 5000-5999.
    pub fn as_display_u32(&self) -> u32 {
        (self.tier as u32) * 1000 + (self.secondary.clamp(0.0, 1.0) * 999.0).round() as u32
    }
```

In `classify_tier` (originally lines 390-408), add the short-circuit as the very first line of the function body:

```rust
    fn classify_tier(&self, callsign: &str, grid: Option<&str>, freq_hz: f64) -> PriorityTier {
        if !is_plausible_callsign(callsign) {
            return PriorityTier::Suspect;
        }
        let needed = self.lookup.is_needed_dxcc(callsign)
            || self.lookup.is_dxcc_needed_on_band(callsign, freq_hz);
        // ... rest unchanged
```

In `score_cq_detailed`, change the final `let total = raw_score.clamp(0.0, 1.0);` line (originally line 369) to:

```rust
        let total = if is_plausible_callsign(callsign) {
            raw_score.clamp(0.0, 1.0)
        } else {
            0.0
        };
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta-qso --lib priority:: -- --nocapture`
Expected: PASS, including every pre-existing `classify_tier_*` test (they use real callsigns like `JA1ABC`, `VP8STI`, `W1A`, `W1XYZ` — all plausible, so the new gate must not change their outcomes).

- [ ] **Step 5: Fix the two band-boundary literals this shift breaks**

`pancetta-tui/src/app.rs:4131` — change:
```rust
        assert!(score >= 4000, "expected ATNO tier range, got {score}");
```
to:
```rust
        assert!(score >= 5000, "expected ATNO tier range, got {score}");
```

`pancetta-tui/src/app.rs:5471` — change:
```rust
        assert!(
            entry.priority_score >= 4000,
            "expected ATNO tier range, got {}",
            entry.priority_score
        );
```
to:
```rust
        assert!(
            entry.priority_score >= 5000,
            "expected ATNO tier range, got {}",
            entry.priority_score
        );
```

Then re-grep to confirm nothing else hardcodes a tier band boundary:

Run: `grep -rn "priority_score.*[0-9]\{4\}\|as_display_u32" pancetta-tui/src pancetta/src/coordinator | grep -v "\.rs:.*//"`

Inspect every hit; if any other numeric-literal comparison against a `priority_score`/`as_display_u32()` value assumes the old 0-4999 range, update it the same way. Do not proceed until this grep's hits are all accounted for.

- [ ] **Step 6: Run the full TUI test suite to confirm the shift is fully accounted for**

Run: `cargo test -p pancetta-tui --lib`
Expected: PASS. Any remaining failure here is a missed band-boundary literal — find and fix it before moving on.

- [ ] **Step 7: Commit**

```bash
git add pancetta-qso/src/priority.rs pancetta-tui/src/app.rs
git commit -m "feat(pancetta-qso): add Suspect priority tier for implausible callsigns (PAN-54)"
```

---

## Task 4: Fix the self-callsign blind spot in the CQ-candidate loop

**Files:**
- Modify: `pancetta-qso/src/autonomous.rs:1519`
- Test: `pancetta-qso/src/autonomous.rs` (existing test module, using the existing `cq_message` helper at line 5191)

**Interfaces:**
- Consumes: `pancetta_core::callsign::callsigns_match` (already imported at `autonomous.rs:21`, already used at `:1364`/`:1371` — no new import needed).
- Produces: no signature change — `AutonomousOperator::feed_decoded_messages_at` and `pending_cqs` are unchanged in shape.

**Context:** `feed_decoded_messages_at`'s CQ-candidate loop rejects our own CQ with `call.eq_ignore_ascii_case(&self.our_callsign)` — a plain string match. Two call sites earlier in the same file (`:1364`, `:1371`), the identical "is this call actually us" question is answered with `callsigns_match`, which additionally (a) resolves an i3=4 nonstandard-callsign hash-render (`"<W5AU>"` → `"W5AU"`) before comparing, and (b) treats a compound callsign (`"W5AU/P"`, `"EA8/W5AU"`) as equivalent to the bare call. The plain check at `:1519` misses both cases: a self-decode that the FT8 decoder renders as a resolved hash or a portable/compound form is NOT recognized as "us" and gets pushed into `pending_cqs` as if it were a workable third-party CQ.

- [ ] **Step 1: Write the failing tests**

Add to `pancetta-qso/src/autonomous.rs`'s `#[cfg(test)] mod tests` block, near `watchlist_gains_per_band_dxcc_new_cq_even_when_at_tx_capacity` (which already demonstrates the `cq_message`/`feed_decoded_messages_at`/`pending_cqs` test pattern):

```rust
    #[test]
    fn feed_decoded_messages_excludes_own_callsign_in_compound_form() {
        let config = AutonomousConfig::default();
        let mut op = AutonomousOperator::new(config, "W5AU".to_string(), Some("EM10".to_string()));
        let evaluator = NullDxEvaluator;
        op.feed_decoded_messages_at(&[cq_message("W5AU/P", "EM10")], &evaluator, Utc::now());
        assert!(
            op.pending_cqs.is_empty(),
            "a compound form of our own callsign must not become a CQ candidate"
        );
    }

    #[test]
    fn feed_decoded_messages_excludes_own_callsign_as_resolved_hash_render() {
        let config = AutonomousConfig::default();
        let mut op = AutonomousOperator::new(config, "W5AU".to_string(), Some("EM10".to_string()));
        let evaluator = NullDxEvaluator;
        op.feed_decoded_messages_at(&[cq_message("<W5AU>", "EM10")], &evaluator, Utc::now());
        assert!(
            op.pending_cqs.is_empty(),
            "a resolved AP-hash render of our own callsign must not become a CQ candidate"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-qso --lib autonomous:: feed_decoded_messages_excludes_own_callsign -- --nocapture`
Expected: FAIL — both `pending_cqs` end up with one entry (the current plain-equality check doesn't recognize either form as "us").

- [ ] **Step 3: Implement the fix**

In `pancetta-qso/src/autonomous.rs`, change line 1519 from:

```rust
                    if call.eq_ignore_ascii_case(&self.our_callsign) {
```

to:

```rust
                    if callsigns_match(call, &self.our_callsign) {
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta-qso --lib autonomous:: -- --nocapture`
Expected: PASS, including every pre-existing `autonomous.rs` test (confirm no regression in the existing CQ-candidate/watchlist tests, which use plain-form callsigns that both matchers already agreed on).

- [ ] **Step 5: Commit**

```bash
git add pancetta-qso/src/autonomous.rs
git commit -m "fix(pancetta-qso): recognize compound/hash-render self-decodes in CQ loop (PAN-54)"
```

---

## Final verification

- [ ] **Step 1: Full workspace test run**

Run: `cargo test --workspace --features transmit`
Expected: PASS, no regressions anywhere (this touches `pancetta-core`, `pancetta-qso`, and `pancetta-tui`, all of which are exercised by the workspace-wide feature-gated test run per `AGENTS.md`).

- [ ] **Step 2: Fmt and clippy**

Run: `cargo fmt --check && cargo clippy --workspace --features transmit -- -D warnings`
Expected: clean. If `fmt --check` reports a diff, run plain `cargo fmt` and re-check clean before trusting it (do not skip this — a non-empty `--check` diff means unformatted code, not "already handled").

- [ ] **Step 3: Update the PAN-54 ticket**

Add a comment to PAN-54 (via `linearis`) summarizing: the hot-reload finding was corrected (no hot-reload manager is wired up at all; the real self-callsign bug was a compound/hash-render blind spot, now fixed), the four shipped fixes, and the deliberately-deferred content-score-based ranking for well-formed-but-statistically-suspicious decodes (recommend as a follow-up ticket if the operator still sees "plausible but wrong" decodes ranking high after this ships).
