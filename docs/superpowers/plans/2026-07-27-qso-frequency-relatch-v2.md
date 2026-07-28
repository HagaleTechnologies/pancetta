# QSO Frequency Relatch v2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop the QSO engine from getting permanently stuck when a legitimate DX partner
replies from a stable-but-different audio frequency than we called them on, WITHOUT
weakening the existing anti-spoofing frequency gate at all — by requiring a second,
confirming sighting at the same new frequency before trusting and relatching.

**Architecture:** One new field on `QsoMetadata` (`pending_freq_drift: Option<f64>`) and one
new private method, `QsoManager::maybe_confirm_frequency_drift`, called at the very start of
`process_message_with_parity` — before the existing `find_qsos_for_message`/
`is_message_relevant`/`determine_state_transition` pipeline, which is not modified in any
way. The new method tracks a per-QSO pending off-latch frequency candidate and only
relatches (mutating both `metadata.frequency` and the active `QsoState` variant's own
embedded `frequency` field, mirroring the existing Hound-QSY code's technique) once a second
message from the same identified partner repeats at the same new frequency within 15 Hz.

**Tech Stack:** Rust, tokio (async), the existing `pancetta-qso` inline test harness
(`#[tokio::test]`/`#[test]` in `pancetta-qso/src/qso_manager.rs`'s `sender_verification_tests`
module), plus the existing `pancetta-qso/tests/adversarial_3party.rs` integration test file
(read-only for this plan — used only to confirm zero regression).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-27-qso-frequency-relatch-v2-design.md` — read it
  first; this plan implements it exactly.
- `is_message_relevant()` and `determine_state_transition()` in
  `pancetta-qso/src/qso_manager.rs` must receive **zero changes** — no edits to either
  function's body, and no new tests inside `mod tests` (the module containing
  `determine_state_transition`'s direct unit tests, ~line 4478-6778) or changes to any
  existing test anywhere in the file.
- `pancetta-qso/tests/adversarial_3party.rs::b10_partner_call_used_by_other_station_discarded`
  must pass with **zero changes to that file**.
- The audio passband bound is `200.0..=2900.0` Hz; the confirm-match tolerance is `15.0` Hz;
  the "already fine, don't bother tracking" tolerance is `100.0` Hz — all fixed values per
  the spec, matching this file's existing `ESTABLISHED_FREQ_TOLERANCE_HZ`/`FREQ_TOLERANCE_HZ`
  constants (which live locally inside `is_message_relevant` and are intentionally NOT
  touched — the new method redeclares the same two numeric values as its own local
  constants; do not attempt to hoist/share them, that would touch the function this plan
  must leave alone).
- Hound/Fox QSOs (`metadata.partner_freq.is_some()`) must be completely unaffected by the
  new method — it must skip them entirely and take no action.
- Test with `cargo test -p pancetta-qso` for fast iteration; final task must also run
  `cargo test --workspace --features transmit`, `cargo fmt --check`, and `cargo clippy`
  before the final commit, per this repo's CLAUDE.md.

---

### Task 1: Add the pending-drift field and the two-strike confirm mechanism

**Files:**
- Modify: `pancetta-qso/src/states.rs` (`QsoMetadata` struct, ~line 319-380 — add one field)
- Modify: `pancetta-qso/src/qso_manager.rs` (new private method near
  `process_message_with_parity`, ~line 1769; one new call site inside that same function;
  19 existing `QsoMetadata { .. }` struct-literal construction sites need the new field
  added — the compiler will flag every one as a missing-field error, so this step is
  self-verifying, not purely manual)
- Modify: `pancetta-qso/src/adif.rs`, `pancetta-qso/src/adif_log_writer.rs`,
  `pancetta-qso/src/async_logger.rs` (2 sites), `pancetta-qso/src/async_database.rs`
  (3 sites), `pancetta-qso/src/statistics.rs` (construction sites — add the field)
- Modify: `pancetta/src/coordinator/qso.rs` (4 sites), `pancetta/src/coordinator/wsjtx_udp/mod.rs`
  (1 site) — construction sites in the OTHER crate that also build `QsoMetadata` literals;
  found via a workspace-wide grep, not just `pancetta-qso/src` — do not skip these, a
  crate-scoped grep alone would miss them (this happened once already on this exact plan;
  see `feedback_plan_construction_site_grep_scope` in project memory if available)
- Test: `pancetta-qso/src/qso_manager.rs`, `mod sender_verification_tests`
  (~line 6778-7119) — add 5 new tests near the existing `is_message_relevant_*` tests

**Interfaces:**
- Produces: `QsoMetadata.pending_freq_drift: Option<f64>` (new pub field,
  `#[serde(default)]`). `QsoManager::maybe_confirm_frequency_drift(&self, message_type:
  &MessageType, frequency: f64)` (new private async method — no return value; it mutates
  `self.qsos` directly under its own write-lock scope).
- Consumes: `QsoState::their_callsign()`, `QsoState::frequency()`, `QsoState::is_active()`,
  `MessageType::sender_callsign()`, `MessageType::is_addressed_to()`,
  `QsoManager::is_partner()` (all pre-existing, unchanged).

- [ ] **Step 1: Add the field to `QsoMetadata`**

In `pancetta-qso/src/states.rs`, find the `partner_freq` field (search
`pub partner_freq: Option<f64>,` — it will be near other similarly-shaped `Option<f64>`
fields in the struct) and add immediately after it:

```rust
    /// The last off-latch frequency seen from this QSO's partner that didn't yet match
    /// the relevance gate's tolerance. `None` normally. Set when an identity-matching
    /// message arrives outside tolerance but inside the FT8 passband; cleared either
    /// when a SECOND message confirms the same frequency (triggering a relatch) or
    /// when a message arrives back within the existing tolerance. See
    /// `QsoManager::maybe_confirm_frequency_drift`. A single off-frequency
    /// identity-matching message can't be safely trusted on its own — FT8 decoded text
    /// has no cryptographic identity, so frequency proximity to the partner's last
    /// confirmed location is the only defense against a spoofed callsign claim (see
    /// `adversarial_3party.rs::b10_partner_call_used_by_other_station_discarded`).
    #[serde(default)]
    pub pending_freq_drift: Option<f64>,
```

- [ ] **Step 2: Try building the workspace to find every construction site**

Run: `cargo build --workspace --exclude pancetta-research 2>&1 | grep -A 2 "missing field"`
Expected: a list of every `QsoMetadata { .. }` struct literal missing the new field —
this should include (but verify against the actual compiler output, which is
authoritative over this list): `pancetta-qso/src/adif.rs`,
`pancetta-qso/src/adif_log_writer.rs`, `pancetta-qso/src/async_logger.rs`,
`pancetta-qso/src/async_database.rs`, `pancetta-qso/src/statistics.rs`,
`pancetta-qso/src/qso_manager.rs` (multiple sites, including
`sender_verification_tests::normal_metadata`), `pancetta/src/coordinator/qso.rs`,
`pancetta/src/coordinator/wsjtx_udp/mod.rs`.

At every flagged site, add `pending_freq_drift: None,` next to the existing
`partner_freq: None,` line (they should read naturally as a pair). Re-run the build
after each batch of fixes until it succeeds with no missing-field errors.

- [ ] **Step 3: Run the build to verify it's clean**

Run: `cargo build --workspace --exclude pancetta-research`
Expected: clean build, no errors.

- [ ] **Step 4: Write the failing tests**

Add these 5 tests inside `mod sender_verification_tests` in
`pancetta-qso/src/qso_manager.rs`, anywhere after the existing `manager_with_call`/
`normal_metadata` helper functions (e.g. right after the closing brace of
`is_message_relevant_identity_verified_still_gated_for_hound` if present from an earlier
attempt, or otherwise after `is_message_relevant_partner_freq_none_falls_back_to_state_freq`
— find the right spot with
`grep -n "fn is_message_relevant_partner_freq_none_falls_back_to_state_freq" pancetta-qso/src/qso_manager.rs`).

```rust
    /// 2026-07-26 incident regression (v2, two-strike confirm): a single off-latch
    /// SignalReport must NOT advance the QSO — it only notes a pending drift
    /// candidate. This must be byte-identical to today's existing (unmodified) drop
    /// behavior; `is_message_relevant`/`determine_state_transition` are untouched by
    /// this fix, so this test is really proving the new pre-pass doesn't change
    /// first-sighting behavior at all.
    #[tokio::test]
    async fn single_off_frequency_sighting_does_not_advance_or_relatch() {
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
            matches!(progress.state, QsoState::RespondingToCq { .. }),
            "a single off-frequency sighting must not advance the QSO; got {:?}",
            progress.state
        );
        assert_eq!(
            progress.metadata.pending_freq_drift,
            Some(937.5),
            "the first sighting must be noted as a pending drift candidate"
        );
    }

    /// The confirming second sighting at the SAME new frequency relatches and lets the
    /// QSO advance normally through the completely-unmodified existing pipeline —
    /// exactly the real 2026-07-26 LU7LRP timeline (two SignalReport decodes at
    /// 937.5 Hz, ~30s apart).
    #[tokio::test]
    async fn second_matching_sighting_confirms_and_relatches() {
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
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "LU7LRP".into(),
                    report: -11,
                },
                "K5ARH LU7LRP -11".into(),
                937.5,
                Some(-17.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::SendingReport { .. }),
            "the confirmed second sighting must advance the QSO; got {:?}",
            progress.state
        );
        assert_eq!(
            progress.metadata.frequency, 937.5,
            "metadata.frequency must relatch to the confirmed frequency"
        );
        assert_eq!(
            progress.metadata.pending_freq_drift, None,
            "pending_freq_drift must clear once confirmed"
        );
        if let QsoState::SendingReport { frequency, .. } = progress.state {
            assert_eq!(
                frequency, 937.5,
                "the state's own embedded frequency field must also relatch \
                 (is_message_relevant reads this field, not metadata.frequency)"
            );
        }
    }

    /// A different second frequency does NOT confirm — the candidate simply resets to
    /// the newest off-latch sighting instead, and the QSO stays stuck (matching
    /// today's behavior). This is the direct proof this mechanism can't be tricked by
    /// two DIFFERENT spoofed frequencies in a row either.
    #[tokio::test]
    async fn different_second_frequency_does_not_confirm() {
        let manager = manager_with_call("K5ARH");
        let qso_id = manager
            .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
            .await
            .unwrap();

        for freq in [937.5, 1100.0] {
            manager
                .process_message(
                    MessageType::SignalReport {
                        to_station: "K5ARH".into(),
                        from_station: "LU7LRP".into(),
                        report: -11,
                    },
                    "K5ARH LU7LRP -11".into(),
                    freq,
                    Some(-19.0),
                )
                .await
                .unwrap();
        }

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::RespondingToCq { .. }),
            "a differing second sighting must not confirm/advance; got {:?}",
            progress.state
        );
        assert_eq!(
            progress.metadata.pending_freq_drift,
            Some(1100.0),
            "the candidate must reset to the newest off-latch sighting"
        );
    }

    /// A normal in-tolerance message arriving after a pending candidate clears it —
    /// no spurious relatch or advance from a stale candidate once the drift resolves
    /// itself (or was noise).
    #[tokio::test]
    async fn in_tolerance_message_clears_a_stale_pending_candidate() {
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
        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.pending_freq_drift,
            Some(937.5)
        );

        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "LU7LRP".into(),
                    report: -9,
                },
                "K5ARH LU7LRP -09".into(),
                1550.0,
                Some(-12.0),
            )
            .await
            .unwrap();

        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::SendingReport { .. }),
            "the in-tolerance message must advance the QSO normally; got {:?}",
            progress.state
        );
        assert_eq!(
            progress.metadata.pending_freq_drift, None,
            "the stale candidate must clear once a normal in-tolerance message arrives"
        );
    }

    /// A passband-violating decode must not overwrite a legitimate pending candidate —
    /// it's likely a garbage decode, not a real drift signal, and shouldn't reset real
    /// tracking. The genuine confirming sighting must still work afterward.
    #[tokio::test]
    async fn out_of_passband_decode_does_not_reset_pending_candidate() {
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

        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "LU7LRP".into(),
                    report: -11,
                },
                "K5ARH LU7LRP -11".into(),
                -50.0,
                Some(-25.0),
            )
            .await
            .unwrap();
        assert_eq!(
            manager.get_qso(qso_id).await.unwrap().metadata.pending_freq_drift,
            Some(937.5),
            "an out-of-passband decode must not overwrite a legitimate pending candidate"
        );

        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "LU7LRP".into(),
                    report: -11,
                },
                "K5ARH LU7LRP -11".into(),
                937.5,
                Some(-17.0),
            )
            .await
            .unwrap();
        assert!(
            matches!(
                manager.get_qso(qso_id).await.unwrap().state,
                QsoState::SendingReport { .. }
            ),
            "the real confirming sighting must still relatch and advance after the noise"
        );
    }
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cargo test -p pancetta-qso frequency_sighting -- --nocapture` and
`cargo test -p pancetta-qso second_matching_sighting -- --nocapture` and
`cargo test -p pancetta-qso confirm -- --nocapture`
Expected: COMPILE ERROR or FAIL — the method `maybe_confirm_frequency_drift` doesn't
exist yet and nothing calls it, so no drift tracking happens; `pending_freq_drift`
assertions will fail (it stays `None`) and the confirm/relatch tests won't see the QSO
advance. This is the expected RED state.

- [ ] **Step 6: Implement `maybe_confirm_frequency_drift`**

In `pancetta-qso/src/qso_manager.rs`, add this new private method. Place it near
`process_message_with_parity` (~line 1769) — immediately before or after that method is
fine; it's a natural companion.

```rust
    /// Two-strike confirm-before-relatch: track a pending off-latch frequency
    /// candidate per QSO, and only relatch (mutate both `metadata.frequency` and the
    /// active state's own embedded `frequency` field) once a SECOND message from the
    /// same identified partner repeats at the same new frequency. Runs BEFORE the
    /// existing `find_qsos_for_message`/`is_message_relevant` routing, which stays
    /// completely unmodified — see
    /// `docs/superpowers/specs/2026-07-27-qso-frequency-relatch-v2-design.md`.
    ///
    /// A single off-frequency identity-matching message can't be safely distinguished
    /// from a spoofed frame claiming the partner's callsign (FT8 decoded text carries
    /// no cryptographic identity) — see
    /// `adversarial_3party.rs::b10_partner_call_used_by_other_station_discarded`. Only
    /// a REPEATED match at the same new frequency is trusted.
    async fn maybe_confirm_frequency_drift(&self, message_type: &MessageType, frequency: f64) {
        // Must match is_message_relevant's ESTABLISHED_FREQ_TOLERANCE_HZ (100.0) and
        // the concept of FREQ_TOLERANCE_HZ (15.0) for "counts as the same spot" —
        // redeclared here as local constants rather than shared, since this fix
        // deliberately does not touch is_message_relevant at all.
        const ESTABLISHED_FREQ_TOLERANCE_HZ: f64 = 100.0;
        const DRIFT_CONFIRM_TOLERANCE_HZ: f64 = 15.0;
        const AUDIO_PASSBAND_MIN_HZ: f64 = 200.0;
        const AUDIO_PASSBAND_MAX_HZ: f64 = 2900.0;

        let Some(sender) = message_type.sender_callsign() else {
            return;
        };
        if !message_type.is_addressed_to(&self.config.our_callsign) {
            return;
        }

        let mut qsos = self.qsos.write().await;
        for progress in qsos.values_mut() {
            if !progress.state.is_active() {
                continue;
            }
            if progress.metadata.partner_freq.is_some() {
                continue; // Hound/Fox — has its own QSY mechanism, untouched.
            }
            let Some(their_callsign) = progress.state.their_callsign().map(|s| s.to_string())
            else {
                continue; // Pre-establishment (CallingCq/Idle) — not this mechanism's scope.
            };
            if !Self::is_partner(sender, &their_callsign) {
                continue;
            }
            let Some(qso_freq) = progress.state.frequency() else {
                continue;
            };
            let distance = (qso_freq - frequency).abs();

            if distance <= ESTABLISHED_FREQ_TOLERANCE_HZ {
                progress.metadata.pending_freq_drift = None;
                continue;
            }

            if !(AUDIO_PASSBAND_MIN_HZ..=AUDIO_PASSBAND_MAX_HZ).contains(&frequency) {
                continue; // Out-of-band decode — don't let noise reset a real candidate.
            }

            let confirmed = progress
                .metadata
                .pending_freq_drift
                .is_some_and(|f| (f - frequency).abs() <= DRIFT_CONFIRM_TOLERANCE_HZ);

            if confirmed {
                progress.metadata.frequency = frequency;
                progress.metadata.pending_freq_drift = None;
                match &mut progress.state {
                    QsoState::RespondingToCq { frequency: state_freq, .. }
                    | QsoState::WaitingForReport { frequency: state_freq, .. }
                    | QsoState::SendingReport { frequency: state_freq, .. }
                    | QsoState::WaitingForConfirmation { frequency: state_freq, .. }
                    | QsoState::SendingConfirmation { frequency: state_freq, .. } => {
                        *state_freq = frequency;
                    }
                    _ => {}
                }
                info!(
                    target: "qso.freq_gate",
                    partner = %their_callsign,
                    old_freq = qso_freq,
                    new_freq = frequency,
                    "confirmed frequency drift (2 consistent sightings) — relatching QSO"
                );
            } else {
                progress.metadata.pending_freq_drift = Some(frequency);
                debug!(
                    target: "qso.freq_gate",
                    partner = %their_callsign,
                    candidate_freq = frequency,
                    latched_freq = qso_freq,
                    "identity-verified message outside tolerance — noting drift candidate \
                     (needs 1 more confirming sighting)"
                );
            }
        }
    }
```

Then wire it in. Find, in `process_message_with_parity`:

```rust
    ) -> Result<(), QsoManagerError> {
        let timestamp = Utc::now();

        // Find relevant QSO(s)
        let qso_ids = self.find_qsos_for_message(&message_type, frequency).await;
```

Replace with:

```rust
    ) -> Result<(), QsoManagerError> {
        let timestamp = Utc::now();

        self.maybe_confirm_frequency_drift(&message_type, frequency).await;

        // Find relevant QSO(s)
        let qso_ids = self.find_qsos_for_message(&message_type, frequency).await;
```

(This is the ONLY change to `process_message_with_parity`; `find_qsos_for_message`,
`is_message_relevant`, and `determine_state_transition` receive no edits at all.)

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p pancetta-qso sender_verification_tests`
Expected: PASS — the whole `sender_verification_tests` module green, including the 5 new
tests.

- [ ] **Step 8: Commit**

```bash
git add pancetta-qso/src/states.rs pancetta-qso/src/qso_manager.rs \
  pancetta-qso/src/adif.rs pancetta-qso/src/adif_log_writer.rs \
  pancetta-qso/src/async_logger.rs pancetta-qso/src/async_database.rs \
  pancetta-qso/src/statistics.rs \
  pancetta/src/coordinator/qso.rs pancetta/src/coordinator/wsjtx_udp/mod.rs
git commit -m "$(cat <<'EOF'
feat(qso): two-strike confirm before relatching a QSO's tracked frequency

2026-07-26 incident: LU7LRP replied from a stable, legitimate, independent
TX offset (937.5 Hz vs the 1500 Hz we called at), and is_message_relevant's
distance gate silently dropped every reply, leaving the QSO stuck
re-sending its opening call.

A v1 attempt relaxed the gate for identity-verified messages, but FT8
decoded text has no cryptographic identity -- that reopened the exact
spoofing hole adversarial_3party.rs's b10 test exists to catch (a station
transmitting text falsely claiming a partner's callsign from a different
frequency).

This version never touches is_message_relevant or determine_state_transition.
A new pre-pass (maybe_confirm_frequency_drift, called before the existing
routing) tracks a pending off-latch candidate per QSO and only relatches
-- mirroring the existing Hound-QSY mutation technique -- once a SECOND
message from the same identified partner repeats at the same new
frequency. A single off-frequency sighting behaves byte-identically to
today (still dropped); only a confirmed repeat trusts the new frequency.

EOF
)"
```

---

### Task 2: Adversarial regression proof and full validation

**Files:**
- None modified — this task only runs tests and, if anything unexpectedly needs a fix,
  makes the smallest possible correction and documents why.

**Interfaces:**
- Consumes: everything built in Task 1.

- [ ] **Step 1: Confirm the adversarial anti-spoofing test is untouched and green**

Run: `git diff --stat main -- pancetta-qso/tests/adversarial_3party.rs` (or the
appropriate base ref for this branch)
Expected: no output — zero changes to this file.

Run: `cargo test -p pancetta-qso --test adversarial_3party b10_partner_call_used_by_other_station_discarded -- --nocapture`
Expected: PASS. This is the direct, load-bearing proof that the v2 design's security
property holds — if this fails, STOP and escalate (do not weaken this test or its
assertions under any circumstance; that would repeat the exact mistake v1 made).

- [ ] **Step 2: Confirm the rest of `adversarial_3party.rs` is unaffected**

Run: `cargo test -p pancetta-qso --test adversarial_3party`
Expected: PASS, full file green (this file covers other adversarial scenarios beyond
b10 — B11, B12, etc. — confirm none of them regressed either).

- [ ] **Step 3: Confirm Hound/Fox mode is completely unaffected**

Run: `cargo test -p pancetta-qso is_message_relevant_hound_keys_on_partner_freq hound_qsy_on_fox_report_full_exchange -- --nocapture`
Expected: PASS, both tests, zero changes needed to either.

- [ ] **Step 4: Full `pancetta-qso` crate suite**

Run: `cargo test -p pancetta-qso`
Expected: PASS, full crate suite green (403+ tests as of the last known-good baseline,
plus the 5 new tests from Task 1 — net addition, zero regressions).

- [ ] **Step 5: Full workspace validation**

Run, in order, confirming each is clean before the next:

```bash
cargo fmt --check
```
Expected: no output. If it prints a diff, run plain `cargo fmt` and re-check clean.

```bash
cargo clippy -p pancetta-qso --all-targets
```
Expected: no warnings introduced by this change. (Corrected post-review: `pancetta-qso`
has no `transmit` feature — that flag belongs to `pancetta-ft8`/`pancetta`, not this
crate — the original command as written errors immediately rather than running.)

```bash
cargo test --workspace --features transmit
```
Expected: full workspace suite PASS (per this repo's CLAUDE.md — this also re-runs
`pancetta`'s `loopback_qso` integration tests, which exercise `pancetta-qso` through the
real coordinator, and confirms the two out-of-crate `QsoMetadata` construction sites in
`pancetta/src/coordinator/` still compile and behave correctly).

- [ ] **Step 6: Commit**

If Steps 1-5 all pass with no code changes needed, there is nothing new to commit for
this task — just confirm the final state:

```bash
git log --oneline -3
git status --short
```
Expected: Task 1's commit is the tip, working tree clean.

If any step required a fix, commit it with a message explaining exactly what regressed
and why, then re-run the full sequence from Step 1.
