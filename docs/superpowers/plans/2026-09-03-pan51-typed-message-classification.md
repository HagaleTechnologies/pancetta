# PAN-51 — Typed Ft8Message Classification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the coordinator's lossy text-round-trip re-classification of decoded FT8 messages with a direct conversion from the already-available `pancetta_ft8::Ft8Message.standard_type`, closing PAN-49's root cause structurally.

**Architecture:** A new pure, infallible function `ft8_message_to_qso_type` in `pancetta/src/coordinator/qso.rs` matches on `Ft8Message.standard_type` and constructs the corresponding `pancetta_qso::states::MessageType` directly from `Ft8Message`'s already-parsed fields. It replaces the single real production call to `pancetta_qso::utils::parse_ft8_message` at that file's decode-handling loop. Nothing else changes — `parse_message`/`QSO_PATTERNS`, the `MessageType` enum, the contest-profile reclassification step, and `message_bus`/Cargo.toml all stay exactly as they are.

**Tech Stack:** Rust, pancetta-ft8 crate (`Ft8Message`, `StandardMessageType`, `DecodedMessage`), pancetta-qso crate (`states::MessageType`), pancetta coordinator crate.

**Spec:** `docs/superpowers/specs/2026-09-03-ft8-typed-message-classification-design.md`

## Global Constraints

- No `Cargo.toml` changes anywhere (`pancetta` already depends on both `pancetta-ft8` and `pancetta-qso`).
- `pancetta_qso::states::MessageType` (the enum) and `MessageExchange::parse_message`/`QSO_PATTERNS` are NOT modified.
- The new function is infallible — it returns `pancetta_qso::states::MessageType` directly, not a `Result`. Every `Ft8Message` input produces some classification (worst case `NonStandard`), matching the design's "trust the decoder fully" decision (spec §3).
- Match this file's existing style exactly: fully-qualified paths in signatures (e.g. `pancetta_ft8::Ft8Message`, `pancetta_qso::states::MessageType` — this file never adds top-level `use pancetta_ft8::...`/`use pancetta_qso::...` aliases), local `use` imports inside function bodies where convenient (see `classify_caller_answer` at `pancetta/src/coordinator/qso.rs:834-840` for the exact pattern to mirror).
- Field mapping table (verified against current `parse_message`/`QSO_PATTERNS` behavior, including the RRR-vs-RR73 regression test at `pancetta-qso/src/exchange.rs:1148-1161`):

  | `Ft8Message.standard_type` | → `pancetta_qso::states::MessageType` |
  |---|---|
  | `Some(StandardMessageType::Cq)` | `Cq { callsign, grid }` |
  | `Some(StandardMessageType::Reply)` | `CqResponse { calling_station: to_callsign, responding_station: from_callsign, grid }` |
  | `Some(StandardMessageType::ReplyWithR)` | `ContestReply { to_station: to_callsign, from_station: from_callsign, grid, is_ack: true }` |
  | `Some(StandardMessageType::Report)` | `SignalReport { to_station: to_callsign, from_station: from_callsign, report: signal_report }` |
  | `Some(StandardMessageType::ReportWithR)` | `ReportAck { to_station: to_callsign, from_station: from_callsign, report: signal_report }` |
  | `Some(StandardMessageType::Rrr)` | `FinalConfirmation { to_station: to_callsign, from_station: from_callsign }` |
  | `Some(StandardMessageType::Final73)` | `SeventyThree { to_station: to_callsign, from_station: from_callsign }` |
  | `Some(StandardMessageType::RR73)` | `FinalConfirmation { to_station: to_callsign, from_station: from_callsign }` |
  | `None` | `NonStandard { text: rendered_text.to_string() }` |

- `GridSquare` and `SignalReport` (`pancetta-qso/src/states.rs:15,18`) are plain type aliases (`String`, `i8` respectively) — copy the `Option<String>`/`Option<i8>` field value directly, no parsing.
- Missing-field defensiveness: `Ft8Message.from_callsign`/`to_callsign`/`grid_square`/`signal_report` are all `Option`. When `standard_type` implies a field must be present but it's `None` (a decoder invariant violation, not expected in production — CRC-14 validation already guarantees only CRC-valid decodes reach `DecodedMessage`, the same trust the current text-based path already extends), use `.unwrap_or_default()` (empty `String`, `0i8`) rather than panicking. This must never crash the coordinator's decode loop on a malformed input.

---

## Task 1: `ft8_message_to_qso_type` — Cq, Reply, ReplyWithR

**Files:**
- Modify: `pancetta/src/coordinator/qso.rs` (new function, placed immediately before `classify_caller_answer` at line 834 — i.e. right after the `CallerAnswer` struct block above it, so the two related classifiers sit together)
- Test: `pancetta/src/coordinator/qso.rs` (new `#[cfg(test)] mod ft8_message_classification_tests`, placed immediately after `mod caller_answer_tests` ends — search for `mod caller_answer_tests {` to find it, then find that module's closing `}` at the matching brace depth)

**Interfaces:**
- Produces: `fn ft8_message_to_qso_type(message: &pancetta_ft8::Ft8Message, rendered_text: &str) -> pancetta_qso::states::MessageType` — a free function (not a method), callable as `ft8_message_to_qso_type(&decoded_msg.message, &raw_text)`.

- [ ] **Step 1: Write the failing tests for the first three variants**

Add this new module in `pancetta/src/coordinator/qso.rs`, placed immediately after the closing `}` of `mod caller_answer_tests` (search for `mod caller_answer_tests {`, then find where that module's brace closes):

```rust
#[cfg(test)]
mod ft8_message_classification_tests {
    use super::*;

    fn base_message() -> pancetta_ft8::Ft8Message {
        pancetta_ft8::Ft8Message::default()
    }

    #[test]
    fn cq_maps_to_cq() {
        let msg = pancetta_ft8::Ft8Message {
            standard_type: Some(pancetta_ft8::message::StandardMessageType::Cq),
            from_callsign: Some("W1ABC".to_string()),
            grid_square: Some("FN42".to_string()),
            ..base_message()
        };
        let result = ft8_message_to_qso_type(&msg, "CQ W1ABC FN42");
        assert_eq!(
            result,
            pancetta_qso::states::MessageType::Cq {
                callsign: "W1ABC".to_string(),
                grid: Some("FN42".to_string()),
            }
        );
    }

    #[test]
    fn reply_maps_to_cq_response() {
        let msg = pancetta_ft8::Ft8Message {
            standard_type: Some(pancetta_ft8::message::StandardMessageType::Reply),
            to_callsign: Some("W1ABC".to_string()),
            from_callsign: Some("K1DEF".to_string()),
            grid_square: Some("FN31".to_string()),
            ..base_message()
        };
        let result = ft8_message_to_qso_type(&msg, "W1ABC K1DEF FN31");
        assert_eq!(
            result,
            pancetta_qso::states::MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "K1DEF".to_string(),
                grid: Some("FN31".to_string()),
            }
        );
    }

    /// PAN-49: the structural fix. Before this design, no regex in
    /// `QSO_PATTERNS` produced `ContestReply` from inbound text at all --
    /// a real `ReplyWithR` decode fell through to `NonStandard` and the
    /// QSO never advanced. This proves the fix is now direct and correct
    /// on the first pass, not a workaround.
    #[test]
    fn reply_with_r_maps_to_contest_reply_pan_49_regression() {
        let msg = pancetta_ft8::Ft8Message {
            standard_type: Some(pancetta_ft8::message::StandardMessageType::ReplyWithR),
            to_callsign: Some("K1ABC".to_string()),
            from_callsign: Some("W9XYZ".to_string()),
            grid_square: Some("EN37".to_string()),
            ..base_message()
        };
        let result = ft8_message_to_qso_type(&msg, "K1ABC W9XYZ R EN37");
        assert_eq!(
            result,
            pancetta_qso::states::MessageType::ContestReply {
                to_station: "K1ABC".to_string(),
                from_station: "W9XYZ".to_string(),
                grid: "EN37".to_string(),
                is_ack: true,
            }
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta --lib ft8_message_classification_tests -- --nocapture`
Expected: FAIL to compile — `ft8_message_to_qso_type` is not defined.

- [ ] **Step 3: Write the function (all 9 branches now, not just 3 — the remaining branches are exercised by Task 2's tests)**

Add this immediately before `classify_caller_answer` (i.e. right before the `fn classify_caller_answer(` line at `pancetta/src/coordinator/qso.rs:834`):

```rust
/// Classify a decoded FT8 message directly from its already-parsed
/// `Ft8Message.standard_type`, instead of re-deriving classification
/// from rendered text via `parse_message`/`QSO_PATTERNS` (PAN-51).
///
/// `standard_type` is `None` when the decode doesn't match any of the
/// eight well-known shapes -- that decision was made against the raw
/// 77-bit payload at decode time, strictly more information than a
/// regex on rendered text can ever recover, so `None` here means
/// `NonStandard`, full stop. This is the structural fix for PAN-49: a
/// `ReplyWithR` decode used to fall through to `NonStandard` because no
/// regex in `QSO_PATTERNS` matched an "R+grid" shape; here it's handled
/// directly (see `ContestReply`'s own doc comment, which already
/// described exactly this shape).
///
/// `rendered_text` is the ALREADY-COMPUTED `DecodedMessage.text`, passed
/// in rather than re-derived via `Ft8Message`'s `Display` impl -- one
/// render, not two, and the `NonStandard` fallback carries exactly what
/// was actually logged for this decode.
fn ft8_message_to_qso_type(
    message: &pancetta_ft8::Ft8Message,
    rendered_text: &str,
) -> pancetta_qso::states::MessageType {
    use pancetta_ft8::message::StandardMessageType;
    use pancetta_qso::states::MessageType as Mt;

    let from = || message.from_callsign.clone().unwrap_or_default();
    let to = || message.to_callsign.clone().unwrap_or_default();
    let grid = || message.grid_square.clone();
    let report = || message.signal_report.unwrap_or_default();

    match message.standard_type {
        Some(StandardMessageType::Cq) => Mt::Cq {
            callsign: from(),
            grid: grid(),
        },
        Some(StandardMessageType::Reply) => Mt::CqResponse {
            calling_station: to(),
            responding_station: from(),
            grid: grid(),
        },
        Some(StandardMessageType::ReplyWithR) => Mt::ContestReply {
            to_station: to(),
            from_station: from(),
            grid: grid().unwrap_or_default(),
            is_ack: true,
        },
        Some(StandardMessageType::Report) => Mt::SignalReport {
            to_station: to(),
            from_station: from(),
            report: report(),
        },
        Some(StandardMessageType::ReportWithR) => Mt::ReportAck {
            to_station: to(),
            from_station: from(),
            report: report(),
        },
        Some(StandardMessageType::Rrr) | Some(StandardMessageType::RR73) => {
            Mt::FinalConfirmation {
                to_station: to(),
                from_station: from(),
            }
        }
        Some(StandardMessageType::Final73) => Mt::SeventyThree {
            to_station: to(),
            from_station: from(),
        },
        None => Mt::NonStandard {
            text: rendered_text.to_string(),
        },
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta --lib ft8_message_classification_tests -- --nocapture`
Expected: PASS (3 tests: `cq_maps_to_cq`, `reply_maps_to_cq_response`, `reply_with_r_maps_to_contest_reply_pan_49_regression`)

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/coordinator/qso.rs
git commit -m "feat(qso): classify decoded FT8 messages from Ft8Message.standard_type

Adds ft8_message_to_qso_type, replacing text-round-trip
re-classification with a direct conversion from the decoder's own
typed classification. Structurally fixes PAN-49 (ReplyWithR falling
through to NonStandard) rather than patching around it.

Not yet wired into the coordinator's decode path — see next commit."
```

---

## Task 2: `ft8_message_to_qso_type` — remaining variants + None

**Files:**
- Modify: `pancetta/src/coordinator/qso.rs` (test module only — the function from Task 1 already implements all branches)

**Interfaces:**
- Consumes: `ft8_message_to_qso_type` from Task 1 (unchanged signature).

- [ ] **Step 1: Write the remaining failing tests**

Add these five tests inside `mod ft8_message_classification_tests` from Task 1 (after `reply_with_r_maps_to_contest_reply_pan_49_regression`):

```rust
    #[test]
    fn report_maps_to_signal_report() {
        let msg = pancetta_ft8::Ft8Message {
            standard_type: Some(pancetta_ft8::message::StandardMessageType::Report),
            to_callsign: Some("K1DEF".to_string()),
            from_callsign: Some("W1ABC".to_string()),
            signal_report: Some(-15),
            ..base_message()
        };
        let result = ft8_message_to_qso_type(&msg, "K1DEF W1ABC -15");
        assert_eq!(
            result,
            pancetta_qso::states::MessageType::SignalReport {
                to_station: "K1DEF".to_string(),
                from_station: "W1ABC".to_string(),
                report: -15,
            }
        );
    }

    #[test]
    fn report_with_r_maps_to_report_ack() {
        let msg = pancetta_ft8::Ft8Message {
            standard_type: Some(pancetta_ft8::message::StandardMessageType::ReportWithR),
            to_callsign: Some("W1ABC".to_string()),
            from_callsign: Some("K1DEF".to_string()),
            signal_report: Some(-12),
            ..base_message()
        };
        let result = ft8_message_to_qso_type(&msg, "W1ABC K1DEF R-12");
        assert_eq!(
            result,
            pancetta_qso::states::MessageType::ReportAck {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -12,
            }
        );
    }

    /// Regression companion to `exchange.rs`'s own
    /// `parse_message` test at line ~1148-1161: bare "RRR" (no digits,
    /// syntactically distinct from a grid) must classify the same as
    /// RR73, matching today's regex behavior exactly.
    #[test]
    fn rrr_maps_to_final_confirmation_same_as_rr73() {
        let msg = pancetta_ft8::Ft8Message {
            standard_type: Some(pancetta_ft8::message::StandardMessageType::Rrr),
            to_callsign: Some("K5ARH".to_string()),
            from_callsign: Some("K9HJZ".to_string()),
            ..base_message()
        };
        let result = ft8_message_to_qso_type(&msg, "K5ARH K9HJZ RRR");
        assert_eq!(
            result,
            pancetta_qso::states::MessageType::FinalConfirmation {
                to_station: "K5ARH".to_string(),
                from_station: "K9HJZ".to_string(),
            }
        );
    }

    #[test]
    fn rr73_maps_to_final_confirmation() {
        let msg = pancetta_ft8::Ft8Message {
            standard_type: Some(pancetta_ft8::message::StandardMessageType::RR73),
            to_callsign: Some("K1DEF".to_string()),
            from_callsign: Some("W1ABC".to_string()),
            ..base_message()
        };
        let result = ft8_message_to_qso_type(&msg, "K1DEF W1ABC RR73");
        assert_eq!(
            result,
            pancetta_qso::states::MessageType::FinalConfirmation {
                to_station: "K1DEF".to_string(),
                from_station: "W1ABC".to_string(),
            }
        );
    }

    #[test]
    fn final73_maps_to_seventy_three() {
        let msg = pancetta_ft8::Ft8Message {
            standard_type: Some(pancetta_ft8::message::StandardMessageType::Final73),
            to_callsign: Some("W1ABC".to_string()),
            from_callsign: Some("K1DEF".to_string()),
            ..base_message()
        };
        let result = ft8_message_to_qso_type(&msg, "W1ABC K1DEF 73");
        assert_eq!(
            result,
            pancetta_qso::states::MessageType::SeventyThree {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
            }
        );
    }

    #[test]
    fn none_standard_type_maps_to_non_standard_with_rendered_text() {
        let msg = pancetta_ft8::Ft8Message {
            standard_type: None,
            text: Some("HELLO WORLD".to_string()),
            ..base_message()
        };
        let result = ft8_message_to_qso_type(&msg, "HELLO WORLD");
        assert_eq!(
            result,
            pancetta_qso::states::MessageType::NonStandard {
                text: "HELLO WORLD".to_string(),
            }
        );
    }

    /// The `NonStandard` fallback must use the PASSED-IN rendered text,
    /// not re-derive it from `Ft8Message` -- proven by making the two
    /// diverge (a real `Display` impl could never do this, but the
    /// conversion function must not assume that and re-render anyway).
    #[test]
    fn none_standard_type_uses_the_passed_in_text_not_a_re_render() {
        let msg = pancetta_ft8::Ft8Message {
            standard_type: None,
            text: Some("SOMETHING ELSE ENTIRELY".to_string()),
            ..base_message()
        };
        let result = ft8_message_to_qso_type(&msg, "EXACT LOGGED TEXT");
        assert_eq!(
            result,
            pancetta_qso::states::MessageType::NonStandard {
                text: "EXACT LOGGED TEXT".to_string(),
            }
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta --lib ft8_message_classification_tests -- --nocapture`
Expected: at this point the tests should actually already PASS, since Task 1 already wrote the full function body covering all branches. If any of these 7 new tests fail, that's a real bug in Task 1's implementation to fix now, before proceeding — do not skip straight to "already passes" without running this.

- [ ] **Step 3: Run tests to confirm passing (after fixing anything Step 2 found)**

Run: `cargo test -p pancetta --lib ft8_message_classification_tests -- --nocapture`
Expected: PASS — 10 tests total (3 from Task 1 + 7 from this task).

- [ ] **Step 4: Commit**

```bash
git add pancetta/src/coordinator/qso.rs
git commit -m "test(qso): cover remaining Ft8Message.standard_type variants

Report, ReportWithR, Rrr, RR73, Final73, and the None -> NonStandard
fallback (including that it uses the passed-in rendered text, not a
re-render)."
```

---

## Task 3: Wire the coordinator's decode path to the new conversion

**Files:**
- Modify: `pancetta/src/coordinator/qso.rs:3446-3583` (the `parse_ft8_message` call site inside the decode-handling loop)

**Interfaces:**
- Consumes: `ft8_message_to_qso_type` from Task 1/2.

- [ ] **Step 1: Read the current call site to confirm it matches this plan's assumption**

Run: `grep -n "parse_ft8_message" pancetta/src/coordinator/qso.rs`

Expected: one hit, inside the `MessageType::DecodedMessage(ref decoded_msg) => { ... }` arm of the decode loop (search for `MessageType::DecodedMessage(ref decoded_msg) => {` to find the enclosing block). The current code reads:

```rust
                                    // Parse the FT8 message to determine its type
                                    match pancetta_qso::utils::parse_ft8_message(
                                        &raw_text,
                                        &our_callsign,
                                    ) {
                                        Ok(msg_type) => {
                                            // ... (a long body using msg_type) ...
                                        }
                                        Err(e) => {
                                            debug!(
                                                "Could not parse FT8 message '{}': {}",
                                                raw_text, e
                                            );
                                        }
                                    }
```

If the live file no longer matches this shape (e.g. the surrounding code has changed since this plan was written), stop and re-derive the correct edit from the actual current content rather than applying this step blindly — the `Ok(msg_type) => { ... }` body must be preserved byte-for-byte regardless of how the match wrapping around it changes.

- [ ] **Step 2: Replace the match with a direct call**

Change:

```rust
                                    // Parse the FT8 message to determine its type
                                    match pancetta_qso::utils::parse_ft8_message(
                                        &raw_text,
                                        &our_callsign,
                                    ) {
                                        Ok(msg_type) => {
```

to:

```rust
                                    // PAN-51: classify directly from the decoder's own
                                    // typed Ft8Message.standard_type instead of
                                    // re-parsing the rendered text — see
                                    // ft8_message_to_qso_type's doc comment.
                                    let msg_type =
                                        ft8_message_to_qso_type(&decoded_msg.message, &raw_text);
                                    {
```

(The opening `{` replaces the `Ok(msg_type) => {` arm opener — the body between it and the matching close is UNCHANGED, still valid as a plain block.)

Then delete the now-unreachable `Err(e) => { ... }` arm and its closing `}` that used to close the `match`:

```rust
                                        Err(e) => {
                                            debug!(
                                                "Could not parse FT8 message '{}': {}",
                                                raw_text, e
                                            );
                                        }
                                    }
```

— delete this whole block (the `Err` arm plus the `match`'s closing `}`). The block that WAS the `Ok(msg_type) => { ... }` body now just needs its own closing `}` to match the new `{` opener from the replacement above — that closing brace already exists (it was the `Ok` arm's own closing `}`, immediately before `Err(e) => {`); do not add or remove a brace there, only delete the `Err` arm text itself.

- [ ] **Step 3: Build to confirm it compiles**

Run: `cargo build -p pancetta --lib`
Expected: clean build. If there's a brace-mismatch error, re-check Step 2 — the body block needs exactly one open/close pair replacing the old `match { Ok() => {..} Err() => {..} }` structure.

- [ ] **Step 4: Run the full pancetta lib test suite**

Run: `cargo test -p pancetta --lib`
Expected: all tests pass, including the existing decode-path tests and the new `ft8_message_classification_tests` module.

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/coordinator/qso.rs
git commit -m "fix(qso): wire the decode path through ft8_message_to_qso_type

Replaces the parse_ft8_message text round-trip at the coordinator's
real decode call site. parse_message/QSO_PATTERNS remain unchanged
and still serve exchange.rs's tests, sim.rs's round-trip harness, and
autonomous.rs's text-only collision detection."
```

---

## Task 4: Full verification and cleanup

**Files:** none (verification only)

- [ ] **Step 1: Run the full pancetta-qso test suite** (confirm `parse_message`/`QSO_PATTERNS` and all their existing tests are genuinely untouched)

Run: `cargo test -p pancetta-qso --lib`
Expected: all pass, same pass count as before this plan started (no regression, no tests removed).

- [ ] **Step 2: Run the full workspace test suite**

Run: `cargo test --workspace --features transmit --exclude pancetta-research`
Expected: all green.

- [ ] **Step 3: Format check**

Run: `cargo fmt --check`
Expected: clean. If not, run `cargo fmt` and re-check.

- [ ] **Step 4: Clippy**

Run: `cargo clippy -p pancetta -p pancetta-qso --lib --bins --tests`
Expected: clean (or only the pre-existing unrelated `result_large_err` warnings in `hamlib.rs` — no new warnings from this change).

- [ ] **Step 5: Confirm no dead code**

Run: `grep -rn "parse_ft8_message" pancetta/src pancetta-qso/src`
Expected: `parse_ft8_message` itself still exists in `pancetta-qso/src/lib.rs` (it's a public utility other consumers may still use — do not delete it), but its only caller inside the `pancetta` coordinator crate is gone. Confirm no orphaned `use` imports or unused variables were left behind at the old call site (the `our_callsign` variable is still used elsewhere in the same function for other purposes — e.g. `maybe_auto_resend_73`, `classify_caller_answer`, `maybe_answer_caller` calls already present in the body — so no unused-variable warning is expected there).

- [ ] **Step 6: Update Linear**

Run: `linearis issues update PAN-51 --status Done` (only after the PR from this plan is merged — do not run this step until Task 3's commit is pushed, reviewed, and landed).
