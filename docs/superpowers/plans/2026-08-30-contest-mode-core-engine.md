# Contest-Mode Core Engine (GridWithRAck) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop pancetta's own manual QSOs from stalling when a partner acks with `"<to> <from> R <grid>"` (the state-QSO-party / ARRL International Digital Contest convention) instead of a numeric report — the actual PAN-49 bug — implemented as the first slice of the general contest-mode design (data-driven profile/catalog, not a one-off regex).

**Architecture:** A new `pancetta-qso::contest` module holds the profile/catalog data model and a text-shape matcher for the `GridWithRAck` exchange. `pancetta-ft8`'s encoder gains the one missing packing case (ir=1 + plain grid). `pancetta-qso`'s `MessageType` gains a `ContestReply` variant; `QsoManager` gains a minimal engagement API (`engage_contest_profile`, no UI — that's a later plan), a reclassification step that only reinterprets an otherwise-`NonStandard` decode as `ContestReply` when at least one active QSO is contest-engaged, and one new state-machine transition arm mirroring the existing `ReportAck` skip-rung arm.

**Tech Stack:** Rust, tokio (async), existing pancetta-ft8/pancetta-qso crates, `cargo test --workspace --features transmit`.

**Spec:** `docs/superpowers/specs/2026-08-30-contest-mode-design.md` (sections 1–3 for this plan; sections 4–7 are later plans).

## Global Constraints

- FT8 mode paths must remain byte-identical for every QSO that is not contest-engaged (AGENTS.md invariant) — every new code path in this plan is additive and only activates once `engage_contest_profile` has been called for a specific QSO.
- `cargo test --workspace --features transmit` must stay green throughout; run it after every task.
- No new supervised coordinator component (per the approved design) — nothing in this plan touches `pancetta/src/coordinator`.
- Follow existing repo conventions exactly: `SignalReport = i8`, `GridSquare = String` (pancetta-qso/src/states.rs:15,18), the `-15` sentinel for "no real numeric report" (used throughout qso_manager.rs, e.g. lines 3238, 3544, 10260), and `MAXGRID4 = 32400` (pancetta-ft8/src/encoder.rs:37, pancetta-ft8/src/message.rs:2213).

---

### Task 1: pancetta-ft8 encoder — pack `"R"+grid` (ir=1, plain grid)

**Files:**
- Modify: `pancetta-ft8/src/encoder.rs:1146-1165` (`packgrid`)
- Modify: `pancetta-ft8/src/encoder.rs:472-489` (`parse_standard_message`'s non-CQ branch)
- Test: `pancetta-ft8/src/encoder.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing new.
- Produces: `packgrid("R EM40")` now returns `igrid4 | 0x8000` instead of the `MAXGRID4 + 1` "empty" fallback; `Ft8Encoder::encode_message("K5TD K5ARH R EM40", None)` now succeeds instead of silently encoding an empty exchange. Later tasks don't call this directly — it's exercised end-to-end by Task 12's loopback test — but it must land first so the wire format exists before anything is built on top of it.

Today, `parse_standard_message` only ever takes `parts[2]` as `extra` — for `"K5TD K5ARH R EM40"` (4 whitespace tokens), `extra` becomes `"R"` alone and `"EM40"` is silently dropped. Then `packgrid("R")` falls through every branch to `MAXGRID4 + 1` ("empty"). Both must change together.

- [ ] **Step 1: Write the failing tests**

Add to `pancetta-ft8/src/encoder.rs`'s existing `#[cfg(test)] mod tests` block (the one containing the `packgrid` tests you can find via `grep -n "fn packgrid_empty\|mod tests" pancetta-ft8/src/encoder.rs`):

```rust
#[test]
fn packgrid_r_prefixed_grid_sets_ir_bit() {
    // PAN-49: state-QSO-party / ARRL Intl Digital contest ack — "R"+grid
    // instead of a numeric report. ir=1 (bit 0x8000), plain grid value.
    let packed = packgrid("R EM40");
    assert_ne!(packed & 0x8000, 0, "ir bit must be set for an R-grid ack");
    let plain = packgrid("EM40");
    assert_eq!(packed & 0x7FFF, plain, "grid value itself must match the unprefixed encoding");
}

#[test]
fn encode_message_r_grid_ack_round_trips_through_decoder() {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder
        .encode_message("K5TD K5ARH R EM40", None)
        .expect("R+grid ack must be encodable, not silently dropped");
    // A non-empty exchange must not collapse to the "no grid/report" payload.
    // (packgrid("") == MAXGRID4 + 1; confirm we did NOT take that fallback
    // by checking the symbols differ from an actual empty-exchange encode.)
    let empty_symbols = encoder
        .encode_message("K5TD K5ARH", None)
        .expect("plain grid-less exchange must still encode");
    assert_ne!(symbols, empty_symbols, "R+grid must not silently degrade to an empty exchange");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pancetta-ft8 packgrid_r_prefixed_grid_sets_ir_bit encode_message_r_grid_ack_round_trips_through_decoder`
Expected: both FAIL — `packgrid_r_prefixed_grid_sets_ir_bit` on the `assert_ne!(packed & 0x8000, 0, ...)` (currently 0), the round-trip test on `assert_ne!` (both currently encode to the same empty-exchange symbols).

- [ ] **Step 3: Fix `parse_standard_message`**

In `pancetta-ft8/src/encoder.rs`, replace the non-CQ branch's `extra` computation (currently `parts[2].to_string()` / `String::new()`, around line 482-486):

```rust
            let call_to = parts[0].to_string();
            let call_de = parts[1].to_string();
            let extra = if parts.len() > 3 && parts[2] == "R" {
                // "R"+grid contest ack (PAN-49): join both tokens so
                // packgrid sees the combined "R EM40" shape it needs to
                // distinguish from a plain "R-12" numeric report (which
                // arrives as ONE token, no space).
                format!("{} {}", parts[2], parts[3])
            } else if parts.len() > 2 {
                parts[2].to_string()
            } else {
                String::new()
            };

            Ok((call_to, call_de, extra))
```

- [ ] **Step 4: Fix `packgrid`**

In `pancetta-ft8/src/encoder.rs`, add a new branch to `packgrid` right after the existing `if bytes[0] == b'R' && bytes.len() >= 2 { ... } else if let Some(dd) = parse_report(extra) { ... }` block and before the final `MAXGRID4 + 1 // fallback: no grid` line (around line 1162-1164):

```rust
    // Parse "R"+4-char-grid ack (PAN-49): state QSO parties and the ARRL
    // International Digital Contest ack with "R"+grid instead of a numeric
    // report. Checked after the numeric-report branch above (which already
    // falls through harmlessly for this shape) since it's a more specific,
    // less common pattern. ir=1, plain grid value — mirrors the report
    // path's `| 0x8000` bit placement.
    if let Some(grid_part) = extra.strip_prefix("R ") {
        let gbytes = grid_part.as_bytes();
        if gbytes.len() == 4
            && gbytes[0] >= b'A'
            && gbytes[0] <= b'R'
            && gbytes[1] >= b'A'
            && gbytes[1] <= b'R'
            && gbytes[2].is_ascii_digit()
            && gbytes[3].is_ascii_digit()
        {
            let mut igrid4: u16 = (gbytes[0] - b'A') as u16;
            igrid4 = igrid4 * 18 + (gbytes[1] - b'A') as u16;
            igrid4 = igrid4 * 10 + (gbytes[2] - b'0') as u16;
            igrid4 = igrid4 * 10 + (gbytes[3] - b'0') as u16;
            return igrid4 | 0x8000;
        }
    }

    MAXGRID4 + 1 // fallback: no grid
```

(Remove the old standalone `MAXGRID4 + 1 // fallback: no grid` line the new block now ends with — don't leave two.)

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p pancetta-ft8 packgrid_r_prefixed_grid_sets_ir_bit encode_message_r_grid_ack_round_trips_through_decoder`
Expected: both PASS.

- [ ] **Step 6: Run the full pancetta-ft8 suite to confirm no regression**

Run: `cargo test --features transmit -p pancetta-ft8`
Expected: all ~295 tests PASS (this touches shared `packgrid`/`parse_standard_message` code paths used by every standard-message encode).

- [ ] **Step 7: Commit**

```bash
git add pancetta-ft8/src/encoder.rs
git commit -m "feat(ft8): PAN-49 encode \"R\"+grid contest acks (ir=1 + plain grid)"
```

---

### Task 2: pancetta-qso contest module — profile & catalog data model

**Files:**
- Create: `pancetta-qso/src/contest/mod.rs`
- Create: `pancetta-qso/src/contest/profile.rs`
- Create: `pancetta-qso/src/contest/catalog.rs`
- Modify: `pancetta-qso/src/lib.rs` (register the module — add `pub mod contest;` alphabetically near the other `pub mod` lines, e.g. after `pub mod content_score;` at line 154)
- Test: inline `#[cfg(test)]` in `catalog.rs`

**Interfaces:**
- Produces: `pancetta_qso::contest::profile::{ContestProfile, ExchangeShape}`, `pancetta_qso::contest::catalog::builtin_catalog() -> Vec<ContestProfile>`. Task 8 (`engage_contest_profile`) consumes `ContestProfile` by value.

- [ ] **Step 1: Write `pancetta-qso/src/contest/profile.rs`**

```rust
//! Contest profile data model — see docs/superpowers/specs/2026-08-30-contest-mode-design.md §1.

use serde::{Deserialize, Serialize};

/// A catalog or operator-defined description of one contest's FT8 exchange
/// convention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContestProfile {
    /// Stable identifier, e.g. "us-state-qso-party". Stored verbatim into
    /// `ContestInfo::contest_name` (states.rs) when a QSO engages this
    /// profile, and used to key it back out of the catalog.
    pub id: String,

    /// Human-readable name for operator-facing UI (a later plan).
    pub display_name: String,

    /// CQ modifier text this contest is known to use, e.g. "KSQP" for
    /// `CQ KSQP K5ARH EM10`. Intentionally partial for hand-verified
    /// entries — extend via `contest.custom_profiles` config (a later
    /// plan) as more are confirmed.
    pub cq_tag_patterns: Vec<String>,

    /// Which wire/text shape this contest's exchange uses.
    pub exchange_shape: ExchangeShape,

    /// Whether this profile's exchange shape has been field-confirmed
    /// (live traffic or an official rules/setup document), as opposed to
    /// inferred. Every profile in `catalog::builtin_catalog()` is `true`;
    /// reserved for future not-yet-verified entries (e.g. WW Digi Contest,
    /// deliberately excluded from the catalog until confirmed — see the
    /// design doc §Background).
    pub verified: bool,

    /// Provenance for future maintenance — where the format assumption
    /// came from.
    pub source_notes: String,
}

/// Which FT8 exchange convention a [`ContestProfile`] uses.
///
/// Only `GridWithRAck` is implemented so far (PAN-49's actual bug). The
/// design doc (§1) also names `FieldDayClassSection`, `VhfContestGridReport`,
/// and `RstSerialOrState` for a later plan — deliberately not declared here
/// yet (YAGNI: an unimplemented enum variant is a compile-time placeholder).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExchangeShape {
    /// State QSO parties + ARRL International Digital Contest: plain grid
    /// exchanged both ways, then `"<to> <from> R <grid>"` as the ack —
    /// standing in for a numeric signal report.
    GridWithRAck,
}
```

- [ ] **Step 2: Write `pancetta-qso/src/contest/catalog.rs` with its test**

```rust
//! Built-in contest catalog — see docs/superpowers/specs/2026-08-30-contest-mode-design.md §1.

use super::profile::{ContestProfile, ExchangeShape};

/// Contest profiles shipped with pancetta. Operators can add their own via
/// `contest.custom_profiles` in pancetta-config (a later plan).
pub fn builtin_catalog() -> Vec<ContestProfile> {
    vec![ContestProfile {
        id: "us-state-qso-party".to_string(),
        display_name: "US State QSO Party".to_string(),
        cq_tag_patterns: vec!["KSQP".to_string(), "SCQP".to_string()],
        exchange_shape: ExchangeShape::GridWithRAck,
        verified: true,
        source_notes: "Live-confirmed 2026-08-29/30: 285 KSQP (Kansas QSO \
            Party) \"R\"+grid exchanges across dozens of unrelated \
            callsign pairs (~/.pancetta/logs/pancetta.log.2026-08-30), plus \
            the SC QSO Party's own published FT8/FT4 digital-mode \
            instructions. Tag list intentionally partial — most US state \
            QSO parties use their own state abbreviation as the CQ tag; \
            add others via contest.custom_profiles as confirmed."
            .to_string(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_is_nonempty_and_every_entry_is_verified() {
        let catalog = builtin_catalog();
        assert!(!catalog.is_empty());
        for profile in &catalog {
            assert!(
                profile.verified,
                "profile {} must be verified to ship in the built-in catalog",
                profile.id
            );
            assert!(!profile.id.is_empty());
            assert!(!profile.cq_tag_patterns.is_empty());
        }
    }

    #[test]
    fn us_state_qso_party_profile_uses_grid_with_r_ack() {
        let catalog = builtin_catalog();
        let profile = catalog
            .iter()
            .find(|p| p.id == "us-state-qso-party")
            .expect("us-state-qso-party must be in the built-in catalog");
        assert_eq!(profile.exchange_shape, ExchangeShape::GridWithRAck);
        assert!(profile.cq_tag_patterns.contains(&"KSQP".to_string()));
    }
}
```

- [ ] **Step 3: Write `pancetta-qso/src/contest/mod.rs`**

```rust
//! Contest-mode support — see docs/superpowers/specs/2026-08-30-contest-mode-design.md.

pub mod catalog;
pub mod matcher;
pub mod profile;
pub mod tokenizer;
```

(`matcher` and `tokenizer` are created in Tasks 3-4 below — this `mod.rs` declares all four up front since they land in the same PR; `cargo build` after Task 4 is what actually confirms this compiles.)

- [ ] **Step 4: Register the module in `pancetta-qso/src/lib.rs`**

Add `pub mod contest;` near the other `pub mod` declarations (alphabetical order — after `pub mod content_score;`).

- [ ] **Step 5: Run the new tests**

Run: `cargo test -p pancetta-qso contest::catalog`
Expected: both tests in Step 2 PASS. (This will not compile until Tasks 3-4 create `matcher.rs`/`tokenizer.rs` referenced by `mod.rs` — run this step's test together with Task 4's, or temporarily comment out the two `pub mod` lines in `mod.rs` you haven't written yet if you want Task 2 to compile in isolation.)

- [ ] **Step 6: Commit**

```bash
git add pancetta-qso/src/contest/mod.rs pancetta-qso/src/contest/profile.rs pancetta-qso/src/contest/catalog.rs pancetta-qso/src/lib.rs
git commit -m "feat(qso): PAN-49 contest profile data model + built-in catalog"
```

---

### Task 3: pancetta-qso contest module — shared tokenizer

**Files:**
- Create: `pancetta-qso/src/contest/tokenizer.rs`
- Modify: `pancetta-qso/src/contest/mod.rs` (already declares `pub mod tokenizer;` from Task 2 — no change needed)
- Test: inline `#[cfg(test)]` in `tokenizer.rs`

**Interfaces:**
- Produces: `pancetta_qso::contest::tokenizer::{DirectedMessage, tokenize_directed_message}`. Consumed by Task 4's matcher.

- [ ] **Step 1: Write the tests**

```rust
//! Shared tokenizer for directed (non-CQ) decoded FT8 text — see
//! docs/superpowers/specs/2026-08-30-contest-mode-design.md §2.

/// A decoded message's callsigns and whatever trailing text follows them,
/// extracted independent of whether the trailing text matches any known
/// exchange shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectedMessage {
    pub to_station: String,
    pub from_station: String,
    pub trailing: String,
}

/// Extract `(to, from, trailing)` from decoded FT8 text whenever the first
/// two whitespace-separated tokens are present and it isn't a CQ. A plain
/// tokenizer, not a callsign validator or exchange-shape matcher — those
/// are the caller's job.
pub fn tokenize_directed_message(text: &str) -> Option<DirectedMessage> {
    let text = text.trim();
    if text.starts_with("CQ") {
        return None;
    }
    let mut parts = text.split_whitespace();
    let to_station = parts.next()?.to_string();
    let from_station = parts.next()?.to_string();
    let trailing: Vec<&str> = parts.collect();
    Some(DirectedMessage {
        to_station,
        from_station,
        trailing: trailing.join(" "),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_r_grid_ack_shape() {
        let m = tokenize_directed_message("K5ARH K5TD R EM40").unwrap();
        assert_eq!(m.to_station, "K5ARH");
        assert_eq!(m.from_station, "K5TD");
        assert_eq!(m.trailing, "R EM40");
    }

    #[test]
    fn tokenizes_plain_grid_shape() {
        let m = tokenize_directed_message("K5TD K5ARH EM10").unwrap();
        assert_eq!(m.trailing, "EM10");
    }

    #[test]
    fn returns_none_for_cq() {
        assert!(tokenize_directed_message("CQ KSQP W0S DM99").is_none());
    }

    #[test]
    fn returns_none_for_fewer_than_two_tokens() {
        assert!(tokenize_directed_message("K5ARH").is_none());
        assert!(tokenize_directed_message("").is_none());
    }

    #[test]
    fn trailing_is_empty_string_for_blank_exchange() {
        let m = tokenize_directed_message("K5TD K5ARH").unwrap();
        assert_eq!(m.trailing, "");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pancetta-qso contest::tokenizer`
Expected: FAIL to compile — `tokenizer.rs` doesn't exist yet as anything but this file. (This is the "write the file, then confirm the tests inside it run and pass" shape — there's no separate red step here since the implementation is trivial and lives in the same file as the tests; skip straight to Step 3.)

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p pancetta-qso contest::tokenizer`
Expected: all 5 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add pancetta-qso/src/contest/tokenizer.rs
git commit -m "feat(qso): PAN-49 shared directed-message tokenizer"
```

---

### Task 4: pancetta-qso contest module — GridWithRAck matcher

**Files:**
- Create: `pancetta-qso/src/contest/matcher.rs`
- Test: inline `#[cfg(test)]` in `matcher.rs`

**Interfaces:**
- Consumes: `super::tokenizer::tokenize_directed_message` (Task 3).
- Produces: `pancetta_qso::contest::matcher::{ContestMatch, match_grid_with_r_ack}`. Consumed by Task 9's reclassification step in `qso_manager.rs`.

This matcher only recognizes the **ack** shape (`"R"+grid`). The plain first-exchange grid shape (`"<to> <from> <grid>"`) is already correctly classified by the existing `QSO_PATTERNS` pipeline (`exchange.rs`) as `CqResponse`/`Reply` — nothing new needed there, so this matcher deliberately doesn't duplicate it.

- [ ] **Step 1: Write the tests**

```rust
//! GridWithRAck exchange-shape matcher — see
//! docs/superpowers/specs/2026-08-30-contest-mode-design.md §2.

use super::tokenizer::tokenize_directed_message;

/// A decoded message recognized as a `GridWithRAck` contest ack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContestMatch {
    pub to_station: String,
    pub from_station: String,
    pub grid: String,
}

/// Recognize `"<to> <from> R <grid>"` — the state-QSO-party / ARRL
/// International Digital Contest ack, standing in for a numeric report.
/// Grid must be 4 characters: first two `A`-`R`, last two digits — the
/// same shape pancetta-ft8's decoder already accepts (message.rs's
/// `unpackgrid`).
pub fn match_grid_with_r_ack(text: &str) -> Option<ContestMatch> {
    let msg = tokenize_directed_message(text)?;
    let grid_candidate = msg.trailing.strip_prefix("R ")?;
    if !is_valid_grid(grid_candidate) {
        return None;
    }
    Some(ContestMatch {
        to_station: msg.to_station,
        from_station: msg.from_station,
        grid: grid_candidate.to_string(),
    })
}

fn is_valid_grid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 4
        && (b'A'..=b'R').contains(&b[0])
        && (b'A'..=b'R').contains(&b[1])
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_pan_49_repro_text() {
        // The actual decoded line from ~/.pancetta/logs/pancetta.log.2026-08-30
        // (K5TD acking our grid during the 2026-08-29/30 KSQP session).
        let m = match_grid_with_r_ack("K5ARH K5TD R EM40").unwrap();
        assert_eq!(m.to_station, "K5ARH");
        assert_eq!(m.from_station, "K5TD");
        assert_eq!(m.grid, "EM40");
    }

    #[test]
    fn rejects_plain_grid_without_r_prefix() {
        assert!(match_grid_with_r_ack("K5TD K5ARH EM10").is_none());
    }

    #[test]
    fn rejects_numeric_report_with_r_prefix() {
        // "R-12" is ONE token (no space) — must not be misread as a grid ack.
        assert!(match_grid_with_r_ack("K1ABC W9XYZ R-12").is_none());
    }

    #[test]
    fn rejects_malformed_grid_after_r() {
        assert!(match_grid_with_r_ack("K5ARH K5TD R RR73").is_none());
        assert!(match_grid_with_r_ack("K5ARH K5TD R EM4").is_none());
    }

    #[test]
    fn rejects_cq() {
        assert!(match_grid_with_r_ack("CQ KSQP W0S DM99").is_none());
    }
}
```

- [ ] **Step 2: Run the tests to verify they pass**

Run: `cargo test -p pancetta-qso contest::matcher`
Expected: all 5 tests PASS.

- [ ] **Step 3: Run the full contest module together**

Run: `cargo test -p pancetta-qso contest::`
Expected: all tests from Tasks 2-4 PASS (this is also the first point `mod.rs`'s four `pub mod` declarations all resolve — confirms Task 2 Step 3's caveat is now moot).

- [ ] **Step 4: Commit**

```bash
git add pancetta-qso/src/contest/matcher.rs
git commit -m "feat(qso): PAN-49 GridWithRAck contest-ack matcher"
```

---

### Task 5: pancetta-qso exchange.rs — `MessageType::ContestReply` + generation

**Files:**
- Modify: `pancetta-qso/src/states.rs:239-289` (`MessageType` enum — add variant after `ContestExchange`)
- Modify: `pancetta-qso/src/exchange.rs:356-369` (`generate_message` — exhaustive match, add arm)
- Test: `pancetta-qso/src/exchange.rs` inline `#[cfg(test)]`

**Interfaces:**
- Produces: `MessageType::ContestReply { to_station: String, from_station: String, grid: String, is_ack: bool }`. `generate_message` renders it as `"<to> <from> R <grid>"`.
- Note: `is_ack` is always `true` for everything this plan produces (the plain first-exchange grid message stays a normal `CqResponse`/`Reply`, unchanged) — the field exists because a later plan's other `ExchangeShape`s will produce `is_ack: false` cases too, and adding it now avoids a breaking enum-shape change later.

- [ ] **Step 1: Add the variant to `MessageType`**

In `pancetta-qso/src/states.rs`, add after the `ContestExchange` variant (before the closing `NonStandard` variant, so `NonStandard` — the catch-all — stays visually last):

```rust
    /// Contest ack via grid instead of a numeric report:
    /// "K1ABC W9XYZ R EN37" (PAN-49 — state QSO parties, ARRL Intl Digital).
    ContestReply {
        to_station: String,
        from_station: String,
        grid: String,
        is_ack: bool,
    },
```

- [ ] **Step 2: Write the failing test**

Add to `pancetta-qso/src/exchange.rs`'s existing `#[cfg(test)] mod tests` block:

```rust
#[test]
fn generate_message_renders_contest_reply_as_r_grid() {
    let exchange = MessageExchange::new("K5ARH".to_string());
    let msg = MessageType::ContestReply {
        to_station: "K5ARH".to_string(),
        from_station: "K5TD".to_string(),
        grid: "EM40".to_string(),
        is_ack: true,
    };
    let text = exchange.generate_message(&msg).unwrap();
    assert_eq!(text, "K5ARH K5TD R EM40");
}
```

- [ ] **Step 3: Run the test — expect a compile error**

Run: `cargo test -p pancetta-qso generate_message_renders_contest_reply_as_r_grid`
Expected: FAIL to compile — `generate_message`'s match is exhaustive (ends `MessageType::NonStandard { text } => Ok(text.clone())`, no wildcard) and doesn't yet have a `ContestReply` arm.

- [ ] **Step 4: Add the `generate_message` arm**

In `pancetta-qso/src/exchange.rs`, add before the final `MessageType::NonStandard { text } => Ok(text.clone()),` arm (around line 369):

```rust
            MessageType::ContestReply {
                to_station,
                from_station,
                grid,
                is_ack,
            } => {
                if *is_ack {
                    Ok(format!("{} {} R {}", to_station, from_station, grid))
                } else {
                    Ok(format!("{} {} {}", to_station, from_station, grid))
                }
            }
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p pancetta-qso generate_message_renders_contest_reply_as_r_grid`
Expected: PASS.

- [ ] **Step 6: Run the full pancetta-qso suite**

Run: `cargo test --features transmit -p pancetta-qso`
Expected: PASS — this step alone is expected to surface every exhaustive-match compile error the new variant causes (states.rs's `sender_callsign`/`addressee_callsign` are exhaustive with no wildcard; `is_addressed_to`/`is_from` have `_ => false` wildcards so they'll compile but silently misbehave — Task 6 fixes both). If the crate fails to compile here, that's expected and is exactly what Task 6 fixes next — do not attempt to fix it in this task.

- [ ] **Step 7: Commit**

```bash
git add pancetta-qso/src/states.rs pancetta-qso/src/exchange.rs
git commit -m "feat(qso): PAN-49 add MessageType::ContestReply + R+grid generation"
```

---

### Task 6: pancetta-qso states.rs — wire `ContestReply` into the `MessageType` helper methods

**Files:**
- Modify: `pancetta-qso/src/states.rs:868-935` (`is_addressed_to`, `sender_callsign`, `addressee_callsign`, `is_from`)
- Test: `pancetta-qso/src/states.rs` inline `#[cfg(test)]`

**Interfaces:**
- Consumes: `MessageType::ContestReply` (Task 5).
- Produces: correct `is_addressed_to`/`sender_callsign`/`addressee_callsign`/`is_from` behavior for `ContestReply` — this is what makes `qso_manager.rs`'s existing catch-all relevance arm (`_ => message_type.is_addressed_to(&self.config.our_callsign)`, line ~4298) correctly route a `ContestReply` to the right QSO without needing a bespoke relevance arm. Required before Task 9's reclassification step can do anything useful.

- [ ] **Step 1: Write the failing tests**

Add to `pancetta-qso/src/states.rs`'s existing `#[cfg(test)] mod tests` block:

```rust
#[test]
fn contest_reply_is_addressed_to_the_to_station() {
    let msg = MessageType::ContestReply {
        to_station: "K5ARH".to_string(),
        from_station: "K5TD".to_string(),
        grid: "EM40".to_string(),
        is_ack: true,
    };
    assert!(msg.is_addressed_to("K5ARH"));
    assert!(!msg.is_addressed_to("K5TD"));
}

#[test]
fn contest_reply_sender_and_addressee_callsigns() {
    let msg = MessageType::ContestReply {
        to_station: "K5ARH".to_string(),
        from_station: "K5TD".to_string(),
        grid: "EM40".to_string(),
        is_ack: true,
    };
    assert_eq!(msg.sender_callsign(), Some("K5TD"));
    assert_eq!(msg.addressee_callsign(), Some("K5ARH"));
    assert!(msg.is_from("K5TD"));
    assert!(!msg.is_from("K5ARH"));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pancetta-qso contest_reply_is_addressed_to_the_to_station contest_reply_sender_and_addressee_callsigns`
Expected: `contest_reply_is_addressed_to_the_to_station` FAILs (`is_addressed_to` falls to `_ => false`). `contest_reply_sender_and_addressee_callsigns` FAILs to COMPILE (`sender_callsign`/`addressee_callsign` are exhaustive matches with no `ContestReply` arm — this is the compile error Task 5 Step 6 predicted).

- [ ] **Step 3: Add `ContestReply` to all four methods**

In `pancetta-qso/src/states.rs`:

`is_addressed_to` (currently ends with a `_ => false` wildcard, around line 875-880) — add `ContestReply` into the existing `to_station == callsign` group:

```rust
            MessageType::SignalReport { to_station, .. }
            | MessageType::ReportAck { to_station, .. }
            | MessageType::FinalConfirmation { to_station, .. }
            | MessageType::SeventyThree { to_station, .. }
            | MessageType::ContestExchange { to_station, .. }
            | MessageType::ContestReply { to_station, .. } => to_station == callsign,
```

`sender_callsign` (exhaustive, ends `MessageType::NonStandard { .. } => None`, around line 897-902) — add `ContestReply` into the `from_station` group:

```rust
            MessageType::SignalReport { from_station, .. }
            | MessageType::ReportAck { from_station, .. }
            | MessageType::FinalConfirmation { from_station, .. }
            | MessageType::SeventyThree { from_station, .. }
            | MessageType::ContestExchange { from_station, .. }
            | MessageType::ContestReply { from_station, .. } => Some(from_station),
```

`addressee_callsign` (exhaustive, around line 912-917) — same pattern:

```rust
            MessageType::SignalReport { to_station, .. }
            | MessageType::ReportAck { to_station, .. }
            | MessageType::FinalConfirmation { to_station, .. }
            | MessageType::SeventyThree { to_station, .. }
            | MessageType::ContestExchange { to_station, .. }
            | MessageType::ContestReply { to_station, .. } => Some(to_station),
```

`is_from` (has a `_ => false` wildcard, around line 928-933) — same pattern:

```rust
            MessageType::SignalReport { from_station, .. }
            | MessageType::ReportAck { from_station, .. }
            | MessageType::FinalConfirmation { from_station, .. }
            | MessageType::SeventyThree { from_station, .. }
            | MessageType::ContestExchange { from_station, .. }
            | MessageType::ContestReply { from_station, .. } => from_station == callsign,
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pancetta-qso contest_reply_is_addressed_to_the_to_station contest_reply_sender_and_addressee_callsigns`
Expected: both PASS.

- [ ] **Step 5: Run the full pancetta-qso suite**

Run: `cargo test --features transmit -p pancetta-qso`
Expected: PASS — the whole crate now compiles cleanly with the new variant fully wired into every `MessageType` helper.

- [ ] **Step 6: Commit**

```bash
git add pancetta-qso/src/states.rs
git commit -m "feat(qso): PAN-49 wire ContestReply into MessageType addressing helpers"
```

---

### Task 7: pancetta-qso qso_manager.rs — `engage_contest_profile` API

**Files:**
- Modify: `pancetta-qso/src/qso_manager.rs` (new public method on `QsoManager`, near the other single-QSO mutation methods such as `respond_to_cq_manual` around line 1248 — place it directly after that method)
- Test: `pancetta-qso/src/qso_manager.rs` inline, in the `mod tests` block starting at line 5462 (use the `test_config()` helper defined at line 5467 — not a nested test submodule's local `manager()`/`DX`/`OUR` shortcuts, several of which exist further down this file for other test groups)

**Interfaces:**
- Consumes: `pancetta_qso::contest::profile::ContestProfile` (Task 2).
- Produces: `QsoManager::engage_contest_profile(&self, qso_id: QsoId, profile: ContestProfile) -> Result<(), QsoManagerError>` — stamps `progress.metadata.contest_info = Some(ContestInfo { contest_name: profile.id, .. })`. This is what Task 9's reclassification step checks for ("is at least one active QSO contest-engaged"). No UI calls this yet — a later plan wires the TUI modal to it.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn engage_contest_profile_stamps_contest_info() {
    let config = test_config();
    let manager = QsoManager::new(config);
    let qso_id = manager
        .respond_to_cq_manual("K5TD".to_string(), 1203.0, None)
        .await
        .unwrap();

    let profile = pancetta_qso::contest::catalog::builtin_catalog()
        .into_iter()
        .find(|p| p.id == "us-state-qso-party")
        .unwrap();
    manager
        .engage_contest_profile(qso_id, profile)
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    let contest_info = progress
        .metadata
        .contest_info
        .expect("contest_info must be set after engaging a profile");
    assert_eq!(contest_info.contest_name, "us-state-qso-party");
}

#[tokio::test]
async fn engage_contest_profile_errors_for_unknown_qso() {
    let config = test_config();
    let manager = QsoManager::new(config);
    let profile = pancetta_qso::contest::catalog::builtin_catalog()
        .into_iter()
        .next()
        .unwrap();
    let bogus_id = QsoId::new_v4();
    let result = manager.engage_contest_profile(bogus_id, profile).await;
    assert!(matches!(
        result,
        Err(QsoManagerError::QsoNotFound { qso_id }) if qso_id == bogus_id
    ));
}
```

(Add these in the same `mod tests` block as `test_config()`, e.g. directly after `report_stage_watchdog_does_not_retire_compound_partner_waiting_for_confirmation` around line 7060 — check `grep -n "async fn respond_to_cq_manual" pancetta-qso/src/qso_manager.rs` first in case line numbers have drifted from earlier tasks' edits.)

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pancetta-qso engage_contest_profile`
Expected: FAIL to compile — `engage_contest_profile` doesn't exist yet.

- [ ] **Step 3: Implement `engage_contest_profile`**

In `pancetta-qso/src/qso_manager.rs`, add directly after `respond_to_cq_manual` (around line 1248 — search for its closing `}` to find the insertion point):

```rust
    /// Engage a contest profile on an existing QSO. Stamps
    /// `metadata.contest_info` so `process_message_with_parity`'s
    /// reclassification step (PAN-49) knows to try this QSO's engaged
    /// profile's exchange-shape matcher against otherwise-`NonStandard`
    /// decodes, and so ADIF logging picks up `CONTEST_ID` (adif.rs).
    ///
    /// No operator UI calls this yet — a later plan wires the "enter this
    /// contest?" modal to it (docs/superpowers/specs/
    /// 2026-08-30-contest-mode-design.md §4).
    pub async fn engage_contest_profile(
        &self,
        qso_id: QsoId,
        profile: crate::contest::profile::ContestProfile,
    ) -> Result<(), QsoManagerError> {
        let mut qsos = self.qsos.write().await;
        let progress = qsos
            .get_mut(&qso_id)
            .ok_or(QsoManagerError::QsoNotFound { qso_id })?;
        progress.metadata.contest_info = Some(ContestInfo {
            contest_name: profile.id,
            category: String::new(),
            serials: ContestSerials {
                sent: None,
                received: None,
            },
            points: 0,
            multiplier: None,
        });
        Ok(())
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pancetta-qso engage_contest_profile`
Expected: both PASS.

- [ ] **Step 5: Run the full pancetta-qso suite**

Run: `cargo test --features transmit -p pancetta-qso`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add pancetta-qso/src/qso_manager.rs
git commit -m "feat(qso): PAN-49 add QsoManager::engage_contest_profile"
```

---

### Task 8: pancetta-qso qso_manager.rs — reclassify `NonStandard` as `ContestReply` when engaged

**Files:**
- Modify: `pancetta-qso/src/qso_manager.rs` (top of `process_message_with_parity`, around line 2276-2290)
- Test: same `mod tests` block as Task 7

**Interfaces:**
- Consumes: `pancetta_qso::contest::matcher::match_grid_with_r_ack` (Task 4), `progress.metadata.contest_info` (Task 7).
- Produces: `process_message_with_parity` now reinterprets an otherwise-`MessageType::NonStandard` decode as `MessageType::ContestReply` before routing — but ONLY when at least one active QSO is contest-engaged, so a normal (non-contest) session's unclassifiable traffic behaves exactly as it does today (Global Constraint: byte-identical for non-engaged QSOs). Consumed by Task 9's new transition arm.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn r_grid_ack_reclassifies_only_when_a_qso_is_contest_engaged() {
    let config = test_config();
    let manager = QsoManager::new(config);
    let mut rx = manager.subscribe();

    // No QSO engaged yet — an R+grid ack for an unrelated station must stay
    // NonStandard (today's behavior, unchanged) and route nowhere.
    manager
        .process_message(
            MessageType::NonStandard {
                text: "K5ARH K5TD R EM40".to_string(),
            },
            "K5ARH K5TD R EM40".to_string(),
            1203.0,
            Some(-11.0),
        )
        .await
        .unwrap();
    assert!(
        drain(&mut rx).is_empty(),
        "an unengaged QSO must not react to an R+grid ack"
    );

    // Engage a QSO with K5TD, then the same text must reclassify and route.
    let qso_id = manager
        .respond_to_cq_manual("K5TD".to_string(), 1203.0, None)
        .await
        .unwrap();
    drain(&mut rx); // discard the initial call's MessageToSend
    let profile = pancetta_qso::contest::catalog::builtin_catalog()
        .into_iter()
        .find(|p| p.id == "us-state-qso-party")
        .unwrap();
    manager.engage_contest_profile(qso_id, profile).await.unwrap();

    manager
        .process_message(
            MessageType::NonStandard {
                text: "K5ARH K5TD R EM40".to_string(),
            },
            "K5ARH K5TD R EM40".to_string(),
            1203.0,
            Some(-11.0),
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::WaitingForConfirmation { .. }),
        "engaged QSO must advance on the R+grid ack, got {:?}",
        progress.state
    );
}
```

(Uses the `drain`/`manager()`-free style consistent with the `mod tests` block at line 5462 — `test_config()` + `QsoManager::new(config)` + `manager.subscribe()`. Check `grep -n "fn subscribe" pancetta-qso/src/qso_manager.rs` for the exact `subscribe()` signature if it differs from `manager.subscribe()` used here.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p pancetta-qso r_grid_ack_reclassifies_only_when_a_qso_is_contest_engaged`
Expected: FAILS at the second `assert!` — the QSO stays in `RespondingToCq` (today's actual PAN-49 bug: the ack is never recognized).

- [ ] **Step 3: Implement the reclassification step**

In `pancetta-qso/src/qso_manager.rs`, at the top of `process_message_with_parity` (around line 2276-2290), insert right after the parameter list and before `self.maybe_confirm_frequency_drift(&message_type, frequency).await;`:

```rust
        // PAN-49: an otherwise-unclassifiable decode may be a contest ack
        // (e.g. "R"+grid) that `MessageExchange::parse_message` has no
        // pattern for. Only reinterpret it when at least one active QSO is
        // contest-engaged (`engage_contest_profile`) — this keeps every
        // non-contest QSO's classification byte-identical to today.
        let message_type = match &message_type {
            MessageType::NonStandard { .. } => {
                match crate::contest::matcher::match_grid_with_r_ack(&raw_text) {
                    Some(m) => {
                        let any_engaged = {
                            let qsos = self.qsos.read().await;
                            qsos.values()
                                .any(|p| p.state.is_active() && p.metadata.contest_info.is_some())
                        };
                        if any_engaged {
                            MessageType::ContestReply {
                                to_station: m.to_station,
                                from_station: m.from_station,
                                grid: m.grid,
                                is_ack: true,
                            }
                        } else {
                            message_type
                        }
                    }
                    None => message_type,
                }
            }
            _ => message_type,
        };
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p pancetta-qso r_grid_ack_reclassifies_only_when_a_qso_is_contest_engaged`
Expected: still FAILS on the second half — reclassification now happens, but there's no transition arm yet for `(RespondingToCq, ContestReply)`, so the state doesn't advance (Task 9 fixes this). Confirm the FAILURE MESSAGE changed from "got RespondingToCq" with no reclassification evidence to the same state but now reachable — this step's job is only to prove reclassification itself compiles and runs; full green is Task 9's.

- [ ] **Step 5: Commit**

```bash
git add pancetta-qso/src/qso_manager.rs
git commit -m "feat(qso): PAN-49 reclassify NonStandard R+grid as ContestReply when engaged"
```

---

### Task 9: pancetta-qso qso_manager.rs — the actual state-machine fix

**Files:**
- Modify: `pancetta-qso/src/qso_manager.rs:3462-3498` (add a new transition arm alongside the existing `ReportAck` skip-rung arm)
- Modify: `pancetta-qso/src/qso_manager.rs:2719-2726` (grid + contest metadata latch block)
- Test: same `mod tests` block as Tasks 7-8 (this task turns Task 8's test fully green)

**Interfaces:**
- Consumes: `MessageType::ContestReply` (Task 5).
- Produces: `(QsoState::RespondingToCq, MessageType::ContestReply { is_ack: true, .. })` now transitions to `QsoState::WaitingForConfirmation` — the actual PAN-49 fix. `progress.metadata.grids.theirs` and `progress.metadata.contest_info` get stamped so ADIF logging (`adif.rs:525-528,` `metadata.grids.theirs` → `gridsquare`) picks up the exchanged grid and contest tag.

- [ ] **Step 1: Add the new transition arm**

In `pancetta-qso/src/qso_manager.rs`, add directly after the existing `ReportAck` skip-rung arm (ends at line 3498 with its closing `}`) — this is the exact mirror the design calls for, verified against the real code:

```rust
            // PAN-49 skip-rung: a state-QSO-party / ARRL Intl Digital
            // partner acks our grid with "R"+grid instead of a numeric
            // report — same close-shape as the ReportAck skip-rung arm
            // above (RespondingToCq -> WaitingForConfirmation), but with no
            // real dB value to carry. `-15` is the established "no real
            // report" sentinel used throughout this file (e.g. lines 3238,
            // 3544, 10260) for exactly this situation. `is_ack: false`
            // (the plain first-exchange grid, already handled by the
            // ordinary CqResponse path) never reaches this arm because
            // process_message_with_parity's reclassification step
            // (PAN-49) only ever produces `is_ack: true`.
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::ContestReply {
                    from_station,
                    to_station,
                    grid,
                    is_ack: true,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, target_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious ContestReply in RespondingToCq ignored — sender does not match QSO target"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                Ok(QsoState::WaitingForConfirmation {
                    their_callsign: target_callsign.clone(),
                    their_report: -15,
                    our_report,
                    frequency: *frequency,
                    grid_square: Some(grid.clone()),
                    started_at: Utc::now(),
                })
            }
```

- [ ] **Step 2: Add the grid + contest metadata latch**

In `pancetta-qso/src/qso_manager.rs`, right after the existing CqResponse grid-latch block (ends at line 2726 with its closing `}`), add:

```rust
        // PAN-49: latch the grid the moment a contest "R"+grid ack arrives,
        // mirroring the CqResponse grid-latch above — the transition arm's
        // WaitingForConfirmation.grid_square only feeds the Completed-state
        // latch (see the block below), not metadata directly, and ADIF
        // logging (adif.rs) reads metadata.grids.theirs, not QsoState.
        if let MessageType::ContestReply { grid, .. } = &message.message_type {
            if !grid.is_empty() {
                progress.metadata.grids.theirs = Some(grid.clone());
            }
        }
```

- [ ] **Step 3: Run Task 8's test — now fully green**

Run: `cargo test -p pancetta-qso r_grid_ack_reclassifies_only_when_a_qso_is_contest_engaged`
Expected: PASS.

- [ ] **Step 4: Write the PAN-49 regression test — replay the actual log line**

Add to the same `mod tests` block:

```rust
/// PAN-49 regression: replays the real decode from
/// ~/.pancetta/logs/pancetta.log.2026-08-30 (K5TD acking our grid during
/// the 2026-08-29/30 Kansas QSO Party session) that stalled a live manual
/// QSO before this fix. Must now advance to WaitingForConfirmation with
/// the grid latched, instead of silently landing in NonStandard forever.
#[tokio::test]
async fn pan_49_k5td_r_grid_ack_advances_the_qso() {
    let config = test_config();
    let manager = QsoManager::new(config);
    let qso_id = manager
        .respond_to_cq_manual("K5TD".to_string(), 1203.0, None)
        .await
        .unwrap();
    let profile = pancetta_qso::contest::catalog::builtin_catalog()
        .into_iter()
        .find(|p| p.id == "us-state-qso-party")
        .unwrap();
    manager.engage_contest_profile(qso_id, profile).await.unwrap();

    manager
        .process_message(
            MessageType::NonStandard {
                text: "K5ARH K5TD R EM40".to_string(),
            },
            "K5ARH K5TD R EM40".to_string(),
            1203.1,
            Some(-11.0),
        )
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::WaitingForConfirmation { grid_square: Some(ref g), .. } if g == "EM40"),
        "expected WaitingForConfirmation with grid EM40, got {:?}",
        progress.state
    );
    assert_eq!(progress.metadata.grids.theirs, Some("EM40".to_string()));
    assert_eq!(
        progress.metadata.contest_info.as_ref().map(|c| c.contest_name.as_str()),
        Some("us-state-qso-party")
    );
}
```

- [ ] **Step 5: Run it**

Run: `cargo test -p pancetta-qso pan_49_k5td_r_grid_ack_advances_the_qso`
Expected: PASS.

- [ ] **Step 6: Run the full pancetta-qso suite**

Run: `cargo test --features transmit -p pancetta-qso`
Expected: PASS — including every pre-existing test, confirming the new arm and latch block are purely additive.

- [ ] **Step 7: Commit**

```bash
git add pancetta-qso/src/qso_manager.rs
git commit -m "fix(qso): PAN-49 advance QSO on R+grid contest ack instead of stalling"
```

---

### Task 10: pancetta-qso — full workspace regression pass

**Files:** none (verification-only task)

**Interfaces:** none.

- [ ] **Step 1: Run the full workspace test suite**

Run: `cargo test --workspace --features transmit`
Expected: PASS. This is the point every existing invariant (single-scorer, drop-stale-TX, the FT8-byte-identical guarantee, etc. — AGENTS.md) gets re-verified against the new code.

- [ ] **Step 2: Run `cargo fmt --check` and `clippy`**

Run: `cargo fmt --check && cargo clippy --workspace --features transmit -- -D warnings`
Expected: both clean. Fix anything flagged (do not skip — `cargo fmt --check` reporting a diff means unformatted code, not "already handled"; run plain `cargo fmt` and re-check clean).

- [ ] **Step 3: Commit if Step 2 produced any changes**

```bash
git add -A
git commit -m "style: cargo fmt after PAN-49 contest-mode core engine"
```

(Skip this step entirely if Step 2 was already clean.)

---

### Task 11: pancetta/tests/loopback_qso.rs — real encode→decode loopback for GridWithRAck

**Files:**
- Modify: `pancetta/tests/loopback_qso.rs` (add a new `#[tokio::test]`, following the file's existing loopback test structure — read 2-3 existing tests in this file first to match its exact setup helpers before writing this one, since they're specific to this integration file and not the `pancetta-qso` unit-test helpers used in Tasks 7-9)

**Interfaces:**
- Consumes: everything from Tasks 1-9 — this is the one test that proves the real encode → modulate → demodulate → decode → classify → advance → log path works end-to-end, not just the unit-level pieces.

This mirrors `test_loopback_compound_callsign_qso_advances_state_machine` (pancetta/tests/loopback_qso.rs:1222-1399) — same `Station::new`/`encode_and_modulate`/`decode`/`utils::parse_ft8_message`/`QsoManagerConfig` shape, verified by reading that test directly. Unlike that test, this one only needs ONE real `QsoManager` (K5ARH's) — the PAN-49 bug and fix are entirely about how K5ARH's own engine handles a decode, not how K5TD's software produced the ack, so `dx_codec` here is used purely to encode K5TD's real over-the-air text, with no `QsoManager` of its own.

- [ ] **Step 1: Write the failing test**

Add to `pancetta/tests/loopback_qso.rs`, near `test_loopback_compound_callsign_qso_advances_state_machine`:

```rust
#[tokio::test]
async fn test_loopback_pan_49_contest_r_grid_ack_advances_qso() {
    use pancetta_qso::QsoManager;

    let freq = 1203.0;

    // dx_codec encodes K5TD's real over-the-air "R"+grid ack (Task 1's
    // packgrid/parse_standard_message fix). It runs no QSO engine of its
    // own -- PAN-49 is entirely about how OUR (K5ARH's) engine handles the
    // decode, not how K5TD's software produced it.
    let mut dx_codec = Station::new("K5TD", "EM40");
    let mut us_codec = Station::new("K5ARH", "EM10");

    let config = QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        our_grid: Some("EM10".to_string()),
        timeouts: TimeoutConfig {
            cq_timeout: 120,
            report_timeout: 120,
            confirmation_timeout: 120,
            max_qso_duration: 600,
            cleanup_interval: 600,
            manual_call_watchdog_minutes: 5,
            manual_call_max_calls: 10,
            repetitive_tx_timeout_secs: 100_000,
        },
        contest_mode: None,
        auto_sequence: AutoSequenceConfig {
            enabled: false,
            auto_respond_cq: false,
            auto_send_reports: false,
            auto_send_confirmations: false,
            action_delay_ms: 0,
        },
        duplicate_checking: DuplicateCheckConfig {
            enabled: false,
            ..DuplicateCheckConfig::default()
        },
        ..Default::default()
    };
    let manager = QsoManager::new(config);
    manager.start().await.unwrap();

    // We manually respond to K5TD's CQ and engage the state-QSO-party
    // profile for this QSO (no UI yet -- a later plan wires this to the
    // "enter this contest?" modal).
    let qso_id = manager
        .respond_to_cq_manual("K5TD".to_string(), freq, None)
        .await
        .unwrap();
    assert!(matches!(
        manager.get_qso(qso_id).await.unwrap().state,
        QsoState::RespondingToCq { .. }
    ));
    let profile = pancetta_qso::contest::catalog::builtin_catalog()
        .into_iter()
        .find(|p| p.id == "us-state-qso-party")
        .unwrap();
    manager
        .engage_contest_profile(qso_id, profile)
        .await
        .unwrap();

    // Real encode -> modulate -> decode of K5TD's "R"+grid ack. Before
    // Task 1's fix this silently encoded as an empty exchange.
    let audio = dx_codec.encode_and_modulate("K5ARH K5TD R EM40", freq);
    let decoded = us_codec.decode(&audio);
    assert!(!decoded.is_empty(), "must decode the R+grid contest ack");
    let decoded_text = &decoded[0].text;
    assert_eq!(decoded_text, "K5ARH K5TD R EM40");

    // Today's classifier has no pattern for this shape -- it parses as
    // NonStandard, exactly like the real 2026-08-29/30 PAN-49 decode did.
    let parsed = pancetta_qso::utils::parse_ft8_message(decoded_text, "K5ARH").unwrap();
    assert!(matches!(parsed, MessageType::NonStandard { .. }));

    // Feed it through the real QSO engine, exactly as the coordinator
    // does. PAN-49's fix reclassifies and advances it because this QSO is
    // contest-engaged.
    manager
        .process_message(parsed, decoded_text.clone(), freq, Some(-11.0))
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(
            progress.state,
            QsoState::WaitingForConfirmation { grid_square: Some(ref g), .. } if g == "EM40"
        ),
        "expected WaitingForConfirmation with grid EM40, got: {:?}",
        progress.state
    );
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p pancetta --test loopback_qso test_loopback_pan_49_contest_r_grid_ack_advances_qso`
Expected: PASS — there's no new production code in this task, only a new integration test proving Tasks 1-9's pieces compose end-to-end through the real encode/modulate/decode pipeline. If it fails, the failure points at a real integration gap between the unit-level assumptions in Tasks 1-9 and the real pipeline — investigate and report before editing production code further; do not weaken the test to paper over a genuine gap.

- [ ] **Step 3: Run the full loopback suite**

Run: `cargo test -p pancetta --test loopback_qso`
Expected: PASS — no regression to any existing loopback scenario.

- [ ] **Step 4: Commit**

```bash
git add pancetta/tests/loopback_qso.rs
git commit -m "test(qso): PAN-49 full encode-to-decode loopback for GridWithRAck contest ack"
```

---

## Self-Review Notes

- **Spec coverage:** §1 (data model/catalog) — Task 2. §2 (recognition: tokenizer, matcher, general shape of "only when engaged") — Tasks 3, 4, 8. §3 (generation) — Tasks 1, 5. §2's "general fallback" (free-form UI for any unclassifiable directed message) and §4-7 (operator UX, pattern inference, Cabrillo) are explicitly out of scope for this plan per the approved decomposition — later plans.
- **Placeholder scan:** no TBD/TODO; every step has real, verified code and real file:line references gathered by reading the actual source (not guessed).
- **Type consistency:** `ContestProfile`/`ExchangeShape` (Task 2) → consumed identically in Tasks 7, 8, 9, 11. `MessageType::ContestReply { to_station, from_station, grid, is_ack }` (Task 5) → identical field names/types used in Tasks 6, 8, 9, 11. `ContestMatch { to_station, from_station, grid }` (Task 4) → consumed identically in Task 8.
- One deliberately deferred correctness gap, called out so it isn't lost: this plan sets `contest_info` only via the new `engage_contest_profile` API, which nothing calls automatically yet (no auto-detection from a heard `CQ KSQP` tag, no UI). That's intentional — Plan 3 (operator UX) is what makes engagement reachable outside a test.
