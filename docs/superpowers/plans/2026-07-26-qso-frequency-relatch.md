# QSO Frequency Relatch Implementation Plan

> **SUPERSEDED 2026-07-27 — DO NOT EXECUTE.** Task 1's design breaks an adversarial
> anti-spoofing test (`b10_partner_call_used_by_other_station_discarded`). See
> `docs/superpowers/specs/2026-07-26-qso-frequency-relatch-design.md`'s superseded notice
> and `docs/superpowers/plans/2026-07-27-qso-frequency-relatch-v2.md` for the corrected plan.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop the QSO engine from silently dropping a legitimate reply from an
already-identified DX partner just because it arrived on a different audio frequency
than we called them on, and make the QSO automatically relatch to the DX's real
frequency when that happens — matching what the manual `RespondToCaller` override
already does by hand.

**Architecture:** Two coordinated changes in `pancetta-qso/src/qso_manager.rs`: (1)
`is_message_relevant()` stops rejecting on frequency distance for the seven match arms
that already verify sender identity via `is_partner`/`is_us`, replacing that check with
a passband sanity bound — but only for non-Hound QSOs (`metadata.partner_freq.is_none()`).
(2) `determine_state_transition()` gains the incoming message's real decoded frequency
and a Hound flag as new parameters, and those same seven arms use the incoming frequency
(instead of the stale carried-forward value) when constructing their next state — again,
only for non-Hound QSOs.

**Tech Stack:** Rust, tokio (async), the existing `pancetta-qso` test harness (`#[tokio::test]`/`#[test]` inline unit tests in `qso_manager.rs`).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-26-qso-frequency-relatch-design.md` — read it
  first; this plan implements it exactly.
- Hound/Fox mode (`metadata.partner_freq.is_some()`) must be **completely unaffected** —
  every code path this plan touches is gated on `partner_freq.is_none()` /
  `!is_hound`, and `is_message_relevant_hound_keys_on_partner_freq` +
  `hound_qsy_on_fox_report_full_exchange` must pass with zero changes to those tests.
- The passband sanity bound is `200.0..=2900.0` Hz (matches the existing
  `freq_min_hz`/`freq_max_hz` convention in `pancetta-qso/src/frequency.rs` and
  `pancetta-qso/src/autonomous.rs`).
- No new config surface, no new operator-facing control — this is internal QSO-engine
  correctness only.
- Test with `cargo test -p pancetta-qso` for fast iteration; final task must also run
  `cargo test --workspace --features transmit` per this repo's CLAUDE.md, plus
  `cargo fmt --check` and `cargo clippy` before the final commit.

---

### Task 1: Relax the frequency relevance gate for identity-verified, non-Hound combinations

**Files:**
- Modify: `pancetta-qso/src/qso_manager.rs` (`is_message_relevant`, ~line 3285-3468; add
  a new helper just above it)
- Test: same file, `#[cfg(test)] mod tests` (existing tests live around line 6879-7051)

**Interfaces:**
- Produces: `QsoManager::is_identity_verified_combo(state: &QsoState, message_type:
  &MessageType) -> bool` (private associated fn, used only by `is_message_relevant` in
  this task, and referenced again in Task 2's design discussion but not called there).

- [ ] **Step 1: Write the failing tests**

Add these three tests near the existing `is_message_relevant_*` tests (right after
`is_message_relevant_partner_freq_none_falls_back_to_state_freq`, before its closing
context — find it via `grep -n "fn is_message_relevant_partner_freq_none_falls_back_to_state_freq" pancetta-qso/src/qso_manager.rs`):

```rust
    /// 2026-07-26 incident regression: LU7LRP replied with a genuine SignalReport
    /// 562 Hz from where we called it (an independent, fixed TX offset — not RF
    /// drift), and the old distance-based gate silently dropped it. An
    /// identity-verified arm on a non-Hound QSO must now accept a reply far outside
    /// the old 100 Hz bound, as long as it's within the FT8 audio passband.
    #[test]
    fn is_message_relevant_identity_verified_accepts_far_frequency_non_hound() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "LU7LRP".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        let report = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "LU7LRP".into(),
            report: -11,
        };
        let md = normal_metadata("K5ARH", 1500.0);
        assert!(
            manager.is_message_relevant(&state, &md, &report, 937.5),
            "identity-verified reply 562 Hz from the latch must be relevant on a non-Hound QSO"
        );
    }

    /// The identity-verified relaxation must still reject a frequency outside the
    /// FT8 audio passband — a decode-garbage guard, not an identity check.
    #[test]
    fn is_message_relevant_identity_verified_rejects_outside_passband() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "LU7LRP".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        let report = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "LU7LRP".into(),
            report: -11,
        };
        let md = normal_metadata("K5ARH", 1500.0);
        assert!(
            !manager.is_message_relevant(&state, &md, &report, -50.0),
            "a frequency below the passband must still be rejected"
        );
        assert!(
            !manager.is_message_relevant(&state, &md, &report, 3200.0),
            "a frequency above the passband must still be rejected"
        );
    }

    /// Hound/Fox mode reuses this SAME (RespondingToCq, SignalReport) arm with a
    /// legitimately large, by-design frequency gap (Hound calls low, Fox replies
    /// high) — the identity-verified relaxation must NOT apply when
    /// `partner_freq.is_some()`. This duplicates part of
    /// `is_message_relevant_hound_keys_on_partner_freq`'s coverage deliberately, as
    /// the direct boundary check for this task's change.
    #[test]
    fn is_message_relevant_identity_verified_still_gated_for_hound() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "KH8B".into(),
            frequency: 600.0,
            started_at: Utc::now(),
        };
        let fox_report = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "KH8B".into(),
            report: -10,
        };
        let mut md = normal_metadata("K5ARH", 600.0);
        md.partner_freq = Some(1800.0);
        assert!(
            !manager.is_message_relevant(&state, &md, &fox_report, 600.0),
            "Hound: a frame at our own TX offset (far from the Fox's partner_freq) must \
             still be rejected even though it's an identity-verified combo and within \
             the audio passband"
        );
    }
```

Also **retarget** the existing test (it currently asserts the exact behavior this task
removes for this specific arm). Find:

```rust
    #[test]
    fn is_message_relevant_partner_freq_none_falls_back_to_state_freq() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "K9ZZ".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        let legit = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "K9ZZ".into(),
            report: -12,
        };
        // partner_freq = None — normal QSO regression path.
        let md = normal_metadata("K5ARH", 1500.0);
        // Frame at the QSO frequency → relevant (unchanged from pre-Hound).
        assert!(
            manager.is_message_relevant(&state, &md, &legit, 1500.0),
            "regression: frame at state.frequency must be relevant when partner_freq=None"
        );
        // Frame far from QSO frequency → not relevant (unchanged).
        assert!(
            !manager.is_message_relevant(&state, &md, &legit, 2000.0),
            "regression: frame far from state.frequency must NOT be relevant when partner_freq=None"
        );
    }
```

Replace with (same intent — distance gate still applies where identity isn't
independently re-verified by the match arm — but using `ReportAck` against
`RespondingToCq`, which has no explicit identity-checked arm and falls through to the
unchanged `_ => is_addressed_to` fallback, so this test's claim stays true after this
task):

```rust
    #[test]
    fn is_message_relevant_partner_freq_none_falls_back_to_state_freq() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "K9ZZ".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        // RespondingToCq + ReportAck ("skip-rung") has no explicit identity-checked
        // arm in is_message_relevant — it falls through to the generic fallback, so
        // this combination is NOT touched by the identity-verified relaxation and
        // must still gate on distance-from-latched-frequency exactly as before.
        let legit = MessageType::ReportAck {
            to_station: "K5ARH".into(),
            from_station: "K9ZZ".into(),
            report: -12,
        };
        // partner_freq = None — normal QSO regression path.
        let md = normal_metadata("K5ARH", 1500.0);
        // Frame at the QSO frequency → relevant (unchanged from pre-Hound).
        assert!(
            manager.is_message_relevant(&state, &md, &legit, 1500.0),
            "regression: frame at state.frequency must be relevant when partner_freq=None"
        );
        // Frame far from QSO frequency → not relevant (unchanged).
        assert!(
            !manager.is_message_relevant(&state, &md, &legit, 2000.0),
            "regression: frame far from state.frequency must NOT be relevant when partner_freq=None"
        );
    }
```

- [ ] **Step 2: Run tests to verify the new ones fail**

Run: `cargo test -p pancetta-qso is_message_relevant_identity_verified -- --nocapture`
Expected: the two new "accepts"/"still_gated_for_hound" tests FAIL (the far-frequency
reply is currently rejected; the Hound one currently passes already — note if it
already passes, that's fine, it's a boundary duplicate, not required to fail here).
The "rejects_outside_passband" test currently PASSES already (both -50 and 3200 are
already outside the *old* 100 Hz bound too) — that's fine, it'll keep passing after
the implementation step; it's here to lock in the *new* mechanism's own behavior.
Confirm specifically that `is_message_relevant_identity_verified_accepts_far_frequency_non_hound` fails.

- [ ] **Step 3: Implement the gate relaxation**

Add this helper immediately above the `fn is_message_relevant(` line (~3285):

```rust
    /// The exact (state, message) combinations whose match arm in
    /// `is_message_relevant` already verifies `Self::is_partner(from, target) &&
    /// self.is_us(to)` before firing — i.e. the sender's identity is unambiguous
    /// regardless of frequency. Used to relax the distance-based frequency gate for
    /// these combinations on non-Hound QSOs (2026-07-26 fix).
    fn is_identity_verified_combo(state: &QsoState, message_type: &MessageType) -> bool {
        matches!(
            (state, message_type),
            (QsoState::WaitingForReport { .. }, MessageType::ReportAck { .. })
                | (
                    QsoState::WaitingForReport { .. },
                    MessageType::FinalConfirmation { .. } | MessageType::SeventyThree { .. }
                )
                | (QsoState::RespondingToCq { .. }, MessageType::SignalReport { .. })
                | (QsoState::RespondingToCq { .. }, MessageType::CqResponse { .. })
                | (QsoState::SendingReport { .. }, MessageType::ReportAck { .. })
                | (
                    QsoState::SendingReport { .. },
                    MessageType::FinalConfirmation { .. } | MessageType::SeventyThree { .. }
                )
                | (
                    QsoState::WaitingForConfirmation { .. },
                    MessageType::FinalConfirmation { .. } | MessageType::SeventyThree { .. }
                )
        )
    }

```

Then, inside `is_message_relevant`, find:

```rust
        if !matched {
            return false;
        }

        // Apply the frequency gate AFTER the callsign/to/state match (B15). A
```

Replace with:

```rust
        if !matched {
            return false;
        }

        // Identity-verified arms (see `is_identity_verified_combo`) already prove the
        // sender IS this QSO's partner via callsign + direction, independent of
        // frequency — for a normal (non-Hound) QSO the distance-to-latched-frequency
        // check on top of that only serves to drop a legitimate DX transmitting on
        // its own independently-chosen offset (2026-07-26 incident: LU7LRP replied
        // 562 Hz from where we called it). Replace the distance check with a
        // passband sanity bound — a decode-garbage guard, not an identity check.
        //
        // Hound/Fox mode is explicitly excluded (`metadata.partner_freq.is_some()`):
        // it reuses this SAME (RespondingToCq, SignalReport) arm with a legitimately
        // large, by-design gap (Hound calls low, Fox replies high), and relies on the
        // distance-to-partner_freq check below to reject a frame at the Hound's own
        // offset (almost certainly a pileup collision, not the Fox) — see
        // `is_message_relevant_hound_keys_on_partner_freq`.
        if metadata.partner_freq.is_none() && Self::is_identity_verified_combo(state, message_type)
        {
            const AUDIO_PASSBAND_MIN_HZ: f64 = 200.0;
            const AUDIO_PASSBAND_MAX_HZ: f64 = 2900.0;
            if !(AUDIO_PASSBAND_MIN_HZ..=AUDIO_PASSBAND_MAX_HZ).contains(&frequency) {
                debug!(
                    target: "qso.freq_gate",
                    frequency,
                    "identity-verified message outside FT8 audio passband sanity bound \
                     ({AUDIO_PASSBAND_MIN_HZ}-{AUDIO_PASSBAND_MAX_HZ} Hz) — rejected"
                );
                return false;
            }
            return true;
        }

        // Apply the frequency gate AFTER the callsign/to/state match (B15). A
```

(Everything below that line — the existing `if let Some(qso_freq) = state.frequency()
{ ... }` block through the final `true` — stays exactly as-is, untouched.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta-qso is_message_relevant -- --nocapture`
Expected: PASS — all `is_message_relevant_*` tests green, including the 3 new ones and
the retargeted one. Also run the two Hound tests by name to be certain:
`cargo test -p pancetta-qso is_message_relevant_hound_keys_on_partner_freq` and
`cargo test -p pancetta-qso hound_qsy_on_fox_report_full_exchange` — both PASS
unchanged.

- [ ] **Step 5: Commit**

```bash
git add pancetta-qso/src/qso_manager.rs
git commit -m "$(cat <<'EOF'
fix(qso): stop dropping identity-verified replies on non-Hound frequency mismatch

is_message_relevant() rejected a SignalReport/ReportAck/FinalConfirmation/
SeventyThree from an already-identified DX partner purely for arriving
outside a 100 Hz window of our latched frequency -- even though the match
arm itself had already verified sender+direction via is_partner/is_us.
2026-07-26 incident: LU7LRP replied 562 Hz from where we called it (a
legitimate independent TX offset), and the report was silently dropped,
leaving the QSO stuck re-sending its opening call.

For the seven arms that already verify identity this way, replace the
distance check with a passband sanity bound (200-2900 Hz) when
partner_freq is None (non-Hound). Hound/Fox mode is untouched -- it reuses
one of these same arms with a legitimately large by-design gap and keeps
relying on the distance-to-partner_freq check.

EOF
)"
```

---

### Task 2: Relatch the QSO's tracked frequency on the same non-Hound identity-verified arms

**Files:**
- Modify: `pancetta-qso/src/qso_manager.rs` (`determine_state_transition` signature +
  seven arms, ~line 2511-3256; its one production call site in
  `process_message_for_qso`, ~line 2115-2122; six existing direct test callers)
- Test: same file, inline tests

**Interfaces:**
- Consumes: `Self::is_identity_verified_combo` is NOT reused here (Task 1's helper is
  specific to the relevance gate; this task changes `determine_state_transition`
  directly per-arm since each arm's own `if is_hound { ... } else { ... }` is simpler
  and keeps the diff local to each arm).
- Produces: `determine_state_transition(&self, current_state: &QsoState, message_type:
  &MessageType, signal_strength: Option<f32>, initiated_by: CallInitiation,
  incoming_frequency: f64, is_hound: bool) -> Result<QsoState, QsoManagerError>` — two
  new trailing parameters. Every caller (1 production, 6 test) must be updated.

- [ ] **Step 1: Write the failing tests**

Add near the existing `legitimate_signal_report_advances_state` test (find via `grep -n
"fn legitimate_signal_report_advances_state" pancetta-qso/src/qso_manager.rs`):

```rust
    /// 2026-07-26 fix: on a non-Hound QSO, the transition arm must use the
    /// INCOMING message's real decoded frequency for the next state, not blindly
    /// carry forward the QSO's stale latched frequency — this is what actually
    /// lets our next reply go out where the DX really is (matching what the manual
    /// RespondToCaller override already does by hand).
    #[tokio::test]
    async fn signal_report_relatches_frequency_to_incoming_when_not_hound() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "LU7LRP".into(),
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        let report = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "LU7LRP".into(),
            report: -11,
        };
        let new_state = manager
            .determine_state_transition(&state, &report, None, CallInitiation::Manual, 937.5, false)
            .await
            .unwrap();
        match new_state {
            QsoState::SendingReport { frequency, .. } => {
                assert_eq!(frequency, 937.5, "must relatch to the incoming message's real frequency");
            }
            other => panic!("expected SendingReport, got {other:?}"),
        }
    }

    /// Hound QSOs must NOT relatch here -- `determine_state_transition` keeps the
    /// old carried-forward frequency; the QSY to the real response offset is owned
    /// entirely by process_message_for_qso's dedicated hound_qsyed block, which
    /// overwrites this value regardless immediately afterward.
    #[tokio::test]
    async fn signal_report_does_not_relatch_frequency_when_hound() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::RespondingToCq {
            target_callsign: "KH8B".into(),
            frequency: 600.0,
            started_at: Utc::now(),
        };
        let report = MessageType::SignalReport {
            to_station: "K5ARH".into(),
            from_station: "KH8B".into(),
            report: -10,
        };
        let new_state = manager
            .determine_state_transition(&state, &report, None, CallInitiation::Manual, 1800.0, true)
            .await
            .unwrap();
        match new_state {
            QsoState::SendingReport { frequency, .. } => {
                assert_eq!(frequency, 600.0, "Hound QSOs must keep the old carried-forward frequency here");
            }
            other => panic!("expected SendingReport, got {other:?}"),
        }
    }

    /// Second-arm coverage (spec requires at least one relatch test beyond the
    /// incident's own RespondingToCq+SignalReport arm): SendingReport+ReportAck must
    /// relatch identically on a non-Hound QSO.
    #[tokio::test]
    async fn report_ack_relatches_frequency_to_incoming_when_not_hound() {
        let manager = manager_with_call("K5ARH");
        let state = QsoState::SendingReport {
            their_callsign: "LU7LRP".into(),
            their_report: Some(-11),
            our_report: -18,
            frequency: 1500.0,
            started_at: Utc::now(),
        };
        let ack = MessageType::ReportAck {
            to_station: "K5ARH".into(),
            from_station: "LU7LRP".into(),
            report: -18,
        };
        let new_state = manager
            .determine_state_transition(&state, &ack, None, CallInitiation::Manual, 937.5, false)
            .await
            .unwrap();
        match new_state {
            QsoState::WaitingForConfirmation { frequency, .. } => {
                assert_eq!(frequency, 937.5, "must relatch to the incoming message's real frequency");
            }
            other => panic!("expected WaitingForConfirmation, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run tests to verify they fail to compile**

Run: `cargo test -p pancetta-qso relatches_frequency -- --nocapture`
Expected: COMPILE ERROR — `determine_state_transition` takes 4 arguments, these three
new tests (`signal_report_relatches_frequency_to_incoming_when_not_hound`,
`signal_report_does_not_relatch_frequency_when_hound`,
`report_ack_relatches_frequency_to_incoming_when_not_hound`) pass 6. This is the
expected failure at this step (a signature mismatch, not a runtime assertion
failure) — proceed to Step 3.

- [ ] **Step 3: Thread the two new parameters through the signature and the seven arms**

Find:

```rust
    async fn determine_state_transition(
        &self,
        current_state: &QsoState,
        message_type: &MessageType,
        signal_strength: Option<f32>,
        initiated_by: CallInitiation,
    ) -> Result<QsoState, QsoManagerError> {
```

Replace with:

```rust
    async fn determine_state_transition(
        &self,
        current_state: &QsoState,
        message_type: &MessageType,
        signal_strength: Option<f32>,
        initiated_by: CallInitiation,
        incoming_frequency: f64,
        is_hound: bool,
    ) -> Result<QsoState, QsoManagerError> {
```

Now update exactly seven arms — in each, the ONLY change is the `frequency: *frequency,`
line inside the `Ok(QsoState::...)` constructor becomes `frequency: if is_hound {
*frequency } else { incoming_frequency },`. Locate each by its unique surrounding
`warn!`/comment text (all in this same function, ~line 2511-3256):

1. **`WaitingForReport` + `FinalConfirmation`/`SeventyThree` (A5 early-close)** — find
   `"spurious RR73/73 in WaitingForReport ignored (CQer, A5)"`, then a few lines below
   it:
   ```rust
                Ok(QsoState::Completed {
                    their_callsign: their_callsign.clone(),
                    their_report: -15,
                    our_report,
                    frequency: *frequency,
                    grid_square: None,
                    completed_at: Utc::now(),
                    duration_seconds: duration,
                })
   ```
   Change `frequency: *frequency,` → `frequency: if is_hound { *frequency } else { incoming_frequency },`.

2. **`WaitingForReport` + `ReportAck`** — find `"spurious ReportAck in WaitingForReport ignored (CQer)"`, then below it:
   ```rust
                Ok(QsoState::WaitingForConfirmation {
                    their_callsign: their_callsign.clone(),
                    their_report: *report,
                    our_report,
                    frequency: *frequency,
                    grid_square: their_grid.clone(),
                    started_at: Utc::now(),
                })
   ```
   Same change to the `frequency:` line.

3. **`RespondingToCq` + `SignalReport`** (the exact arm from the incident) — find
   `"DX returned our call without a report — advancing grid -> signal report"`... no,
   that's the CqResponse arm's log line. For SignalReport, find the comment `"Response to CQ, waiting for report"` immediately above the arm, then:
   ```rust
                Ok(QsoState::SendingReport {
                    their_callsign: target_callsign.clone(),
                    their_report: Some(*report),
                    our_report,
                    frequency: *frequency,
                    started_at: Utc::now(),
                })
   ```
   Same change.

4. **`RespondingToCq` + `CqResponse`** (stuck-at-grid fix) — find
   `"DX returned our call without a report — advancing grid -> signal report"`, then
   below it:
   ```rust
                Ok(QsoState::SendingReport {
                    their_callsign: target_callsign.clone(),
                    their_report: None,
                    our_report,
                    frequency: *frequency,
                    started_at: Utc::now(),
                })
   ```
   Same change.

5. **`SendingReport` + `ReportAck`** — find the comment `"Received report acknowledgment"` immediately above the arm, then:
   ```rust
                Ok(QsoState::WaitingForConfirmation {
                    their_callsign: their_callsign.clone(),
                    their_report: their_report.unwrap_or(-15),
                    our_report: *our_report,
                    frequency: *frequency,
                    grid_square: None,
                    started_at: Utc::now(),
                })
   ```
   Same change.

6. **`SendingReport` + `FinalConfirmation`/`SeventyThree`** — find `"spurious RR73/73 in SendingReport ignored"`, then below it:
   ```rust
                Ok(QsoState::Completed {
                    their_callsign: their_callsign.clone(),
                    their_report: their_report.unwrap_or(-15),
                    our_report: *our_report,
                    frequency: *frequency,
                    grid_square: None,
                    completed_at: Utc::now(),
                    duration_seconds: duration,
                })
   ```
   Same change.

7. **`WaitingForConfirmation` + `FinalConfirmation`/`SeventyThree`** — find the comment `"Received final confirmation"` immediately above the arm, then:
   ```rust
                Ok(QsoState::Completed {
                    their_callsign: their_callsign.clone(),
                    their_report: *their_report,
                    our_report: *our_report,
                    frequency: *frequency,
                    grid_square: grid_square.clone(),
                    completed_at: Utc::now(),
                    duration_seconds: duration,
                })
   ```
   Same change.

Do **not** touch any other arm (the CallingCq arms, the three REGRESSION arms, the
RespondingToCq+ReportAck skip-rung arm, or the RespondingToCq+FinalConfirmation/
SeventyThree GAP-1 arm) — they keep `frequency: *frequency,` exactly as today, per the
spec's scope.

- [ ] **Step 4: Update the one production call site**

Find (in `process_message_for_qso`):

```rust
        let new_state = self
            .determine_state_transition(
                &old_state,
                &message.message_type,
                message.signal_strength,
                qso_initiated_by,
            )
            .await?;
```

Replace with:

```rust
        let new_state = self
            .determine_state_transition(
                &old_state,
                &message.message_type,
                message.signal_strength,
                qso_initiated_by,
                message.frequency,
                progress.metadata.hound,
            )
            .await?;
```

- [ ] **Step 5: Update the six existing direct test callers**

Each currently calls with 4 arguments and needs `, <freq>, false` appended, where
`<freq>` is whatever frequency value is already used by that test's `state` fixture
(so behavior is unchanged — none of these six exercise a case where the incoming
frequency differs from the state's latched frequency):

- `spoofed_signal_report_does_not_advance_state` (`determine_state_transition(&state, &spoof, None, CallInitiation::Auto)`) → append `, 1500.0, false`
- `legitimate_signal_report_advances_state` (`&state, &legit, None, CallInitiation::Auto`) → append `, 1500.0, false`
- `spoofed_report_ack_does_not_advance_to_completion` (`&state, &spoof, None, CallInitiation::Auto`) → append `, 1500.0, false`
- `spoofed_final_confirmation_does_not_complete_qso` (`&state, &spoof, None, CallInitiation::Auto`) → append `, 1500.0, false`
- `legitimate_final_confirmation_completes_qso` (`&state, &legit, None, CallInitiation::Auto`) → append `, 1500.0, false`
- The call inside the test using the `FREQ`/`DX`/`OUR` constants (search
  `"DX re-sends their report (didn't copy our R)"` for the surrounding comment) — this
  exercises `(SendingReport, SignalReport)` REGRESSION 2, an arm this plan does NOT
  modify, so the two new arguments are inert here; append `, FREQ, false`.

For example, the first becomes:

```rust
        let new_state = manager
            .determine_state_transition(&state, &spoof, None, CallInitiation::Auto, 1500.0, false)
            .await
            .unwrap();
```

- [ ] **Step 6: Run tests to verify everything passes**

Run: `cargo test -p pancetta-qso`
Expected: PASS — full `pancetta-qso` crate suite green, including the two new tests
from Step 1, all six updated direct callers, and
`hound_qsy_on_fox_report_full_exchange` (confirms the Hound QSY post-processing block
still overwrites the frequency regardless of what this task's arms produce for a Hound
QSO).

- [ ] **Step 7: Commit**

```bash
git add pancetta-qso/src/qso_manager.rs
git commit -m "$(cat <<'EOF'
fix(qso): relatch QSO frequency to the incoming message on non-Hound advances

determine_state_transition() had no access to the incoming message's real
decoded frequency, so every identity-verified transition arm blindly
carried forward the QSO's stale latched frequency into its next state --
even once Task 1 let a far-frequency reply route through. Thread
incoming_frequency + is_hound through the function and its 7 callers (1
production, 6 test); the same seven arms now use the incoming frequency
for non-Hound QSOs, matching what the manual RespondToCaller override
already does by hand. Hound QSOs are unaffected: process_message_for_qso's
existing hound_qsyed block already overwrites this value unconditionally
right after.

EOF
)"
```

---

### Task 3: End-to-end regression test and full validation

**Files:**
- Test: `pancetta-qso/src/qso_manager.rs` (new test using the public `process_message`
  API, near the other `#[tokio::test]` integration-style tests)

**Interfaces:**
- Consumes: `QsoManager::process_message` (public API, already used elsewhere in this
  file's tests) and `QsoManager::respond_to_cq_manual` (or equivalent existing
  QSO-creation helper already used by nearby tests — check
  `respond_to_cq_manual(DX.into(), FREQ, None)` usage near line 8570 for the exact
  signature/pattern to copy).

- [ ] **Step 1: Write the end-to-end regression test**

```rust
    /// End-to-end regression for the 2026-07-26 LU7LRP incident: start a manual QSO
    /// by responding to a CQ at 1500 Hz, then feed the DX's real SignalReport
    /// decoded at 937.5 Hz (their own independent TX offset) through the PUBLIC
    /// process_message API -- exactly the path live decodes take. Before this fix,
    /// the report was silently dropped by is_message_relevant and the QSO stayed
    /// stuck in RespondingToCq.
    #[tokio::test]
    async fn end_to_end_signal_report_from_different_frequency_advances_qso() {
        let manager = manager_with_call("K5ARH");
        let qso_id = manager
            .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
            .await
            .unwrap();

        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "LU7LRP".into(),
                    report: -11,
                },
                "K5ARH LU7LRP -11".into(),
                937.5,
                Some(-19.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::SendingReport { .. }),
            "QSO must advance to SendingReport, not stay stuck in RespondingToCq; got {:?}",
            progress.state
        );
        if let QsoState::SendingReport { frequency, .. } = progress.state {
            assert_eq!(
                frequency, 937.5,
                "QSO must relatch to the DX's real frequency, not stay at 1500.0"
            );
        }
    }
```

`respond_to_cq_manual(callsign, frequency, grid)` is confirmed to exist with exactly
this shape — it's already used the same way (`respond_to_cq_manual(DX.into(), FREQ,
None)`) by the existing test around line 8570 that also chains a `process_message`
call afterward; the test above copies that exact pattern.

- [ ] **Step 2: Run it to verify it fails before this branch's fixes (sanity check only if not already committed)**

This step is a sanity check, not required if Tasks 1-2 are already committed on this
branch — the point is confirming this specific test genuinely exercises the original
bug. If you want to confirm: `git stash`, run
`cargo test -p pancetta-qso end_to_end_signal_report_from_different_frequency`, expect
FAIL (`QSO must advance to SendingReport... got RespondingToCq`), then `git stash pop`.

- [ ] **Step 3: Run it to verify it passes**

Run: `cargo test -p pancetta-qso end_to_end_signal_report_from_different_frequency -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Full regression suite**

Run, in order, and confirm each is clean before proceeding to the next:

```bash
cargo fmt --check
```
Expected: no output (clean). If it prints a diff, run plain `cargo fmt` and re-check.

```bash
cargo clippy -p pancetta-qso --all-targets --features transmit
```
Expected: no warnings introduced by this change.

```bash
cargo test -p pancetta-qso
```
Expected: full crate suite PASS.

```bash
cargo test --workspace --features transmit
```
Expected: full workspace suite PASS (per this repo's CLAUDE.md; this also re-runs
`pancetta`'s `loopback_qso` integration tests, which exercise `pancetta-qso` through
the real coordinator — confirm nothing else in the workspace depended on the old
drop-on-distance behavior).

- [ ] **Step 5: Commit**

```bash
git add pancetta-qso/src/qso_manager.rs
git commit -m "$(cat <<'EOF'
test(qso): end-to-end regression for the LU7LRP frequency-relatch incident

Exercises the fix through the public process_message API exactly as live
decodes do: open a manual QSO responding to a CQ at 1500 Hz, feed the DX's
real SignalReport decoded at 937.5 Hz, and confirm the QSO advances to
SendingReport with its frequency relatched to 937.5 -- instead of staying
stuck in RespondingToCq re-sending the opening call, which is what
happened live on 2026-07-26.

EOF
)"
```
