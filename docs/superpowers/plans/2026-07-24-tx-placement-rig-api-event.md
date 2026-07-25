# TX-Placement Rig-API Event (dispensa Q-0026) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a new `placement` `serverEvent` to pancetta's remote_gateway that carries the exact same TX-placement/openness ranking the local TUI's instrument shows, so a remote client (panino, per dispensa Q-0026) can eventually build an equivalent "where can I transmit right now" view.

**Architecture:** Mirror the existing `spectrum` serverEvent (dispensa Q-0024) end-to-end: a new wire DTO in `pancetta-protocol`, a new `ServerEvent` variant, and a translation from the bus message that already exists (`MessageType::TxPlacementUpdate { snapshot: PlacementSnapshot }`) into that variant. Unlike `spectrum`, the placement translation needs no per-call state (no dial-Hz enrichment, no sequence counter) — it is a pure field-for-field reshape, so it slots into the existing stateless `server_event_from_bus` function rather than the async `handle_bus_msg` special-case path `spectrum` needed.

**Tech Stack:** Rust, serde (camelCase wire format per ADR-0003), tokio broadcast channel (existing `remote_gateway` event pump).

## Global Constraints

- Wire field casing is camelCase (ADR-0003) — every multi-word struct field needs an explicit `#[serde(rename = "...")]` or struct-level `#[serde(rename_all = "camelCase")]`, matching every existing DTO in `pancetta-protocol/src/dto.rs`.
- Additive-only: no existing `ServerEvent` variant, `StateSnapshot` field, or DTO field may change shape. This is a new variant only.
- Single-scorer invariant (CLAUDE.md): the new event's data MUST come from the exact same `SmartFrequencyAllocator`-derived `PlacementSnapshot` the autonomous decision path and the local TUI already share — this plan adds a pure reshape of that existing snapshot, never a new computation.
- `offset_hz` values in the wire payload are baseband/audio-passband offsets (same convention pancetta-tui already uses for its local `PlacementView` — see `pancetta/src/coordinator/tui_relay.rs:606-615`), NOT RF-absolute like `spectrum`'s `bin_start_hz`. Do not add `dial_hz` to these values — that would break the field-for-field mirror with the local TUI and is not what dispensa Q-0026's agreed shape (`offsetHz`) describes. A remote client combines this with the existing `frequency` serverEvent's dial Hz if it wants an absolute frequency, same as the local TUI already does implicitly (offset-only display).
- Not carried in `stateSnapshot` — event-only, same choice Q-0024's `spectrum` event made (documented in dispensa's `x-status`: "Not carried in stateSnapshot (event-only)"). A newly-connected client waits up to one FT8 window (~15s) for the first `placement` event; this is an accepted tradeoff, not a gap to fix here.

---

## File Structure

- `pancetta-protocol/src/dto.rs` — add `PlacementSlice` and `Placement` DTO structs (new, wire-only types; no dependency on `pancetta-qso`).
- `pancetta-protocol/src/event.rs` — add `ServerEvent::Placement { placement: Placement }` variant + tests.
- `pancetta-protocol/src/lib.rs` — re-export `Placement`, `PlacementSlice`.
- `pancetta/src/coordinator/remote_gateway/translate.rs` — add `placement_snapshot_to_event`, wire it into `server_event_from_bus`, add tests.
- `pancetta/src/coordinator/remote_gateway/mod.rs` — add one integration-style test proving `handle_bus_msg` turns a `TxPlacementUpdate` bus message into a broadcast `ServerEvent::Placement`. No production-code change needed here — the existing generic `other => match translate::server_event_from_bus(other)` arm already covers it.

---

### Task 1: Wire DTOs — `PlacementSlice` and `Placement`

**Files:**
- Modify: `pancetta-protocol/src/dto.rs`
- Test: same file, `#[cfg(test)] mod tests` block at the bottom of `dto.rs` (create if it doesn't already exist — check first: `grep -n "mod tests" pancetta-protocol/src/dto.rs`; if it exists, add to it, matching existing style).

**Interfaces:**
- Produces: `pub struct PlacementSlice { pub offset_hz: f64, pub score: f64, pub clear_first: bool, pub clear_second: bool }` and `pub struct Placement { pub slices: Vec<PlacementSlice>, pub openness: Vec<u8>, pub bin_hz: f64, pub range: (f64, f64) }`, both `#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]` with `#[serde(rename_all = "camelCase")]`. Task 2 imports both from `crate::dto`.

- [ ] **Step 1: Add the two DTO structs**

Open `pancetta-protocol/src/dto.rs`. Find the existing `Spectrum` struct (search `pub struct Spectrum`) and add the new structs directly after it, matching its doc-comment style:

```rust
/// One ranked TX-placement candidate — dispensa Q-0026.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlacementSlice {
    pub offset_hz: f64,
    pub score: f64,
    pub clear_first: bool,
    pub clear_second: bool,
}

/// Per-window TX-placement/openness snapshot — dispensa Q-0026. Mirrors
/// `pancetta_qso::frequency::PlacementSnapshot` field-for-field, minus
/// `clear_both_slots`/`noise_floor` (local-TUI-only, not part of the agreed
/// wire contract). `offset_hz` values are baseband/audio-passband offsets,
/// NOT RF-absolute (unlike `Spectrum::bin_start_hz`) — consumers combine
/// with the `frequency` serverEvent's dial Hz for an absolute value, the
/// same convention the local TUI already uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Placement {
    pub slices: Vec<PlacementSlice>,
    pub openness: Vec<u8>,
    pub bin_hz: f64,
    pub range: (f64, f64),
}
```

- [ ] **Step 2: Write the failing test**

Add to `dto.rs`'s test module (create `#[cfg(test)] mod tests { use super::*; ... }` at the bottom of the file if one doesn't already exist):

```rust
#[test]
fn placement_fields_are_camel_case() {
    let p = Placement {
        slices: vec![PlacementSlice {
            offset_hz: 1500.0,
            score: 42.5,
            clear_first: true,
            clear_second: false,
        }],
        openness: vec![3, 2, 0],
        bin_hz: 5.86,
        range: (200.0, 3000.0),
    };
    let j = serde_json::to_string(&p).unwrap();
    assert!(j.contains(r#""offsetHz":1500.0"#), "expected offsetHz in: {j}");
    assert!(j.contains(r#""clearFirst":true"#), "expected clearFirst in: {j}");
    assert!(j.contains(r#""clearSecond":false"#), "expected clearSecond in: {j}");
    assert!(j.contains(r#""binHz":5.86"#), "expected binHz in: {j}");
    assert!(j.contains(r#""range":[200.0,3000.0]"#), "expected range array in: {j}");
    let round: Placement = serde_json::from_str(&j).unwrap();
    assert_eq!(round, p);
}
```

If `dto.rs` has no existing `mod tests`, add `use serde_json;` is unnecessary (already a direct dependency, used unqualified elsewhere in the crate) — just add the module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_fields_are_camel_case() {
        // ... (as above)
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p pancetta-protocol placement_fields_are_camel_case`
Expected: FAIL to compile (`Placement`/`PlacementSlice` not found) — this is the expected "red" state since Step 1 hasn't been checked yet if done out of order; if Step 1 was already applied, the test should instead compile and PASS immediately since the structs are straightforward. Either way, confirm no compile errors remain after Step 1 is in place.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pancetta-protocol placement_fields_are_camel_case`
Expected: `test dto::tests::placement_fields_are_camel_case ... ok` (or wherever the test module path resolves — check the actual output path).

- [ ] **Step 5: Commit**

```bash
git add pancetta-protocol/src/dto.rs
git commit -m "feat(protocol): add Placement/PlacementSlice wire DTOs (dispensa Q-0026)"
```

---

### Task 2: `ServerEvent::Placement` variant

**Files:**
- Modify: `pancetta-protocol/src/event.rs`
- Modify: `pancetta-protocol/src/lib.rs` (re-export)

**Interfaces:**
- Consumes: `Placement` from `crate::dto` (Task 1).
- Produces: `ServerEvent::Placement { placement: Placement }`, tag value `"placement"` (via the enum's `#[serde(tag = "event", rename_all = "camelCase")]`). Task 3 constructs this variant.

- [ ] **Step 1: Add the variant**

In `pancetta-protocol/src/event.rs`, update the import line at the top:

```rust
use crate::dto::{DecodedView, DxRow, PendingCall, Placement, QsoProgress, Spectrum};
```

Add the new variant to the `ServerEvent` enum, directly after the existing `Spectrum` variant:

```rust
    /// Per-window TX-placement/openness ranking — nested as
    /// `{"event":"placement","placement":{…}}`. Additive, dispensa Q-0026.
    Placement {
        placement: Placement,
    },
```

- [ ] **Step 2: Write the failing test**

In the `#[cfg(test)] mod tests` block at the bottom of `event.rs`, add a case to the `event_tag_values_are_camel_case` test's `cases` array (find the array literal `let cases: &[(&str, ServerEvent)] = &[` and add an entry, e.g. right after the `"spectrum"` entry):

```rust
            (
                "placement",
                ServerEvent::Placement {
                    placement: Placement {
                        slices: vec![],
                        openness: vec![],
                        bin_hz: 5.86,
                        range: (200.0, 3000.0),
                    },
                },
            ),
```

Also add a dedicated field-casing test, mirroring `signal_strength_field_is_camel_case`:

```rust
    #[test]
    fn placement_field_is_camel_case() {
        let e = ServerEvent::Placement {
            placement: Placement {
                slices: vec![crate::dto::PlacementSlice {
                    offset_hz: 1500.0,
                    score: 10.0,
                    clear_first: true,
                    clear_second: true,
                }],
                openness: vec![3],
                bin_hz: 5.86,
                range: (200.0, 3000.0),
            },
        };
        let j = serde_json::to_string(&e).unwrap();
        assert!(j.contains(r#""event":"placement""#), "expected placement tag in: {j}");
        assert!(j.contains(r#""offsetHz""#), "expected offsetHz in: {j}");
        assert!(j.contains(r#""binHz""#), "expected binHz in: {j}");
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p pancetta-protocol placement`
Expected: compile error until Step 1 is applied (the `Placement` import / variant don't exist yet). If Step 1 is already in place, expect a clean compile and passing tests immediately — confirm by re-running after Step 1.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta-protocol placement`
Expected: both `event_tag_values_are_camel_case` and `placement_field_is_camel_case` pass. Also run the full crate suite to make sure nothing else broke: `cargo test -p pancetta-protocol`.

- [ ] **Step 5: Re-export from `lib.rs`**

In `pancetta-protocol/src/lib.rs`, update:

```rust
pub use dto::{DecodedView, DxRow, PendingCall, Placement, PlacementSlice, QsoProgress, Spectrum};
```

- [ ] **Step 6: Commit**

```bash
git add pancetta-protocol/src/event.rs pancetta-protocol/src/lib.rs
git commit -m "feat(protocol): add ServerEvent::Placement variant (dispensa Q-0026)"
```

---

### Task 3: Bus→event translation

**Files:**
- Modify: `pancetta/src/coordinator/remote_gateway/translate.rs`

**Interfaces:**
- Consumes: `pancetta_qso::frequency::PlacementSnapshot` (existing, already carried by `MessageType::TxPlacementUpdate { snapshot }` — see `pancetta/src/message_bus.rs:181-182`), `pancetta_protocol::{Placement, PlacementSlice}` (Tasks 1-2), `pancetta_protocol::ServerEvent::Placement` (Task 2).
- Produces: `pub(crate) fn placement_snapshot_to_event(snapshot: &pancetta_qso::frequency::PlacementSnapshot) -> ServerEvent`. Task 3 also wires this into the existing `pub(crate) fn server_event_from_bus(msg: &MessageType) -> Option<ServerEvent>` — no other task depends on the wiring directly, but Task 4's test exercises it through `handle_bus_msg`.

- [ ] **Step 1: Write the failing test**

In `pancetta/src/coordinator/remote_gateway/translate.rs`, inside the existing `#[cfg(test)] mod tests` block, add (this crate already has `pancetta_qso` as a dependency via `message_bus.rs`, so the import is available):

```rust
    #[test]
    fn placement_snapshot_to_event_maps_fields_and_drops_local_only_fields() {
        use pancetta_qso::frequency::{FrequencyCandidate, PlacementSnapshot};

        let snapshot = PlacementSnapshot {
            slices: vec![FrequencyCandidate {
                offset_hz: 1500.0,
                score: 42.5,
                clear_both_slots: true,
                clear_first: true,
                clear_second: true,
                noise_floor: -120.0,
            }],
            openness: vec![3, 2, 0],
            bin_hz: 5.86,
            range: (200.0, 3000.0),
        };

        let event = placement_snapshot_to_event(&snapshot);
        match event {
            ServerEvent::Placement { placement } => {
                assert_eq!(placement.slices.len(), 1);
                assert_eq!(placement.slices[0].offset_hz, 1500.0);
                assert_eq!(placement.slices[0].score, 42.5);
                assert!(placement.slices[0].clear_first);
                assert!(placement.slices[0].clear_second);
                assert_eq!(placement.openness, vec![3, 2, 0]);
                assert_eq!(placement.bin_hz, 5.86);
                assert_eq!(placement.range, (200.0, 3000.0));
            }
            other => panic!("expected Placement, got {other:?}"),
        }
    }

    #[test]
    fn server_event_from_bus_handles_tx_placement_update() {
        use pancetta_qso::frequency::PlacementSnapshot;

        let snapshot = PlacementSnapshot {
            slices: vec![],
            openness: vec![],
            bin_hz: 5.86,
            range: (200.0, 3000.0),
        };
        let msg = MessageType::TxPlacementUpdate { snapshot };
        let event = server_event_from_bus(&msg);
        assert!(
            matches!(event, Some(ServerEvent::Placement { .. })),
            "expected Some(Placement), got {event:?}"
        );
    }
```

Note: check `FrequencyCandidate`'s exact field list first (`grep -n "struct FrequencyCandidate" -A 8 pancetta-qso/src/frequency.rs`) in case field order or names have shifted since this plan was written — the plan's field names (`offset_hz`, `score`, `clear_both_slots`, `clear_first`, `clear_second`, `noise_floor`) match the crate as of this writing.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta --lib placement_snapshot_to_event`
Expected: compile error, `placement_snapshot_to_event` not found (function doesn't exist yet).

- [ ] **Step 3: Implement the translation function**

In `pancetta/src/coordinator/remote_gateway/translate.rs`, update the top import line:

```rust
use pancetta_protocol::{DecodedView, PendingCall, Placement, PlacementSlice, QsoProgress, ServerEvent, Spectrum};
```

Add the function directly after `spectrum_row_to_event` (before `server_event_from_bus`):

```rust
/// Convert the qso-crate `PlacementSnapshot` into the wire `placement`
/// server event (dispensa Q-0026). Field-for-field copy, dropping
/// `clear_both_slots`/`noise_floor` (local-TUI-only, not part of the agreed
/// wire contract) — same allocator-sourced data `coordinator/tui_relay.rs`
/// converts for the local `PlacementView`, so remote and local clients see
/// identical rankings (single-scorer invariant).
pub(crate) fn placement_snapshot_to_event(
    snapshot: &pancetta_qso::frequency::PlacementSnapshot,
) -> ServerEvent {
    ServerEvent::Placement {
        placement: Placement {
            slices: snapshot
                .slices
                .iter()
                .map(|c| PlacementSlice {
                    offset_hz: c.offset_hz,
                    score: c.score,
                    clear_first: c.clear_first,
                    clear_second: c.clear_second,
                })
                .collect(),
            openness: snapshot.openness.clone(),
            bin_hz: snapshot.bin_hz,
            range: snapshot.range,
        },
    }
}
```

- [ ] **Step 4: Wire it into `server_event_from_bus`**

In the same file, find `pub(crate) fn server_event_from_bus` and add a new match arm (order doesn't matter; put it near `MessageType::ModeStatus` for locality with other simple mappings):

```rust
        MessageType::TxPlacementUpdate { snapshot } => Some(placement_snapshot_to_event(snapshot)),
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p pancetta --lib placement`
Expected: both new tests pass. Also run `cargo test -p pancetta --lib remote_gateway` to confirm nothing in the surrounding module broke.

- [ ] **Step 6: Commit**

```bash
git add pancetta/src/coordinator/remote_gateway/translate.rs
git commit -m "feat(gateway): translate TxPlacementUpdate bus messages into placement serverEvent (dispensa Q-0026)"
```

---

### Task 4: End-to-end pump test

**Files:**
- Modify: `pancetta/src/coordinator/remote_gateway/mod.rs`

**Interfaces:**
- Consumes: `handle_bus_msg` (existing async function in this file), `MessageType::TxPlacementUpdate` (existing bus message), `ServerEvent::Placement` (Task 2), `placement_snapshot_to_event`/`server_event_from_bus` (Task 3, exercised indirectly).
- Produces: nothing new for later tasks — this is the final verification task for this plan.

- [ ] **Step 1: Write the failing test**

In `pancetta/src/coordinator/remote_gateway/mod.rs`'s test module, add a test mirroring `spectrum_row_bus_message_increments_seq_and_uses_dial_freq` (find it and place this test directly after it):

```rust
    #[tokio::test]
    async fn tx_placement_update_bus_message_becomes_placement_event() {
        use pancetta_qso::frequency::{FrequencyCandidate, PlacementSnapshot};

        let (evt_tx, mut rx) = broadcast::channel::<ServerEvent>(16);
        let snapshot_state = RwLock::new(empty_snapshot());
        let op_freq = AtomicU64::new(14_074_000);
        let lookup = crate::priority_evaluator::CachedStationLookup::new();
        let spectrum_seq = AtomicU64::new(0);

        let placement_snapshot = PlacementSnapshot {
            slices: vec![FrequencyCandidate {
                offset_hz: 1500.0,
                score: 42.5,
                clear_both_slots: true,
                clear_first: true,
                clear_second: true,
                noise_floor: -120.0,
            }],
            openness: vec![3, 2, 0],
            bin_hz: 5.86,
            range: (200.0, 3000.0),
        };
        let msg = MessageType::TxPlacementUpdate {
            snapshot: placement_snapshot,
        };

        handle_bus_msg(
            &msg,
            &op_freq,
            &lookup,
            "K5ARH",
            &evt_tx,
            &snapshot_state,
            &spectrum_seq,
        )
        .await;

        let event = rx.recv().await.unwrap();
        match event {
            ServerEvent::Placement { placement } => {
                assert_eq!(placement.slices.len(), 1);
                assert_eq!(placement.slices[0].offset_hz, 1500.0);
                assert_eq!(placement.openness, vec![3, 2, 0]);
            }
            other => panic!("expected Placement, got {other:?}"),
        }
    }
```

Check the exact signature of `handle_bus_msg` first (`grep -n "async fn handle_bus_msg" -A 10 pancetta/src/coordinator/remote_gateway/mod.rs`) in case parameter order has shifted since this plan was written — match whatever the live signature is.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta --lib tx_placement_update_bus_message_becomes_placement_event`
Expected: FAIL if Task 3 wasn't yet applied (no `Placement` arm reached); if Tasks 1-3 are already done, expect an immediate PASS — this task exists to prove the full pump end-to-end, not to drive new production code.

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test -p pancetta --lib tx_placement_update_bus_message_becomes_placement_event`
Expected: `test coordinator::remote_gateway::tests::tx_placement_update_bus_message_becomes_placement_event ... ok`

- [ ] **Step 4: Run the full workspace test suite**

Run: `cargo test --workspace --features transmit`
Expected: all tests pass, no regressions. (Per this repo's `pancetta-hamlib` note, if that crate's tests are included and flaky, re-run with `cargo test -p pancetta-hamlib --lib -- --test-threads=1` separately — but the primary command above is the standard gate.)

- [ ] **Step 5: Run fmt and clippy**

Run: `cargo fmt --check`
Expected: clean (no diff). If not, run `cargo fmt` and re-check.

Run: `cargo clippy --workspace --exclude pancetta-research --features transmit` (exact CI command, `.github/workflows/ci.yml`'s `clippy` job)
Expected: clean, no warnings.

- [ ] **Step 6: Commit**

```bash
git add pancetta/src/coordinator/remote_gateway/mod.rs
git commit -m "test(gateway): cover TxPlacementUpdate -> placement serverEvent through handle_bus_msg"
```

---

## Post-Plan (controller only — NOT a subagent task)

These steps are cross-repo/PR-authority actions and must be done by the controlling session, never by an implementer subagent (this repo's convention: implementers never push or open PRs).

1. Push the branch and open a pancetta PR referencing dispensa Q-0026.
2. In the `dispensa` repo (sibling clone, already pulled fresh): update `contracts/rig/rig-api.v1.schema.json` — add `$defs.placementSlice` and `$defs.placementFrame` (mirroring `spectrumFrame`'s structure), add a `placement` entry to the `serverEvent` `oneOf` list, and bump the `x-status` header with a new "v1.3 AMENDED <date> (Q-0026, pancetta authored, shipped)" note, following the exact style of the existing v1.2/v1.1 amendment entries.
3. In `dispensa/questions/0026-tx-placement-openness-data-not-in-rig-api.md`, append a new `### pancetta (<date>) — placement serverEvent shipped` answer section (do not change the `Status:` header value — leave it `Answered`, not `Resolved`, since panino still isn't building a consumer against it yet per its 2026-07-04 answer; this mirrors how Q-0024 stayed `Answered` even after both sides shipped code, pending real usage/validation).
4. Do NOT touch panino — out of scope per Q-0026's answer ("not requesting a build now").

## Q-0024 verdict

No engineering work is available to fold in here. Q-0024 (the `spectrum` event) is fully shipped and merged on both sides (pancetta PR #196, panino PR #43); the pancetta-side emission path (`translate::spectrum_row_to_event`, wired through `handle_bus_msg`'s `SpectrumRow` arm) and its test coverage (`spectrum_row_bus_message_increments_seq_and_uses_dial_freq` plus the `event.rs` round-trip tests) already exist and pass — there is nothing latent to audit that this session hasn't already read and confirmed correct while grounding this plan. The remaining gap is genuinely a real-hardware step: an operator running a live station and visually confirming the spectrum data renders correctly in panino's waterfall panel. That is a "Meatspace Pending" item, not a code task — flag it for the operator rather than manufacturing a busywork "audit" task here.
