# Batch 95 — TUI worked-before wiring + real rig S-meter

Last two gaps from the Batch 93 TUI assessment.

## 1. worked_before (band-activity / DX hunter displays)

**Source chosen**: the coordinator's existing
`Arc<CachedStationLookup>` (`pancetta/src/priority_evaluator.rs`) — the
*same instance* the autonomous priority scorer reads through the
`WorkedStationLookup::is_duplicate` trait method for its duplicate
penalty. It is already:

- seeded at QSO-component startup from `~/.pancetta/qso.db` via
  `AsyncQsoDatabase::get_worked_callsigns(band)` →
  `seed_worked_from_list` (coordinator/qso.rs ~250), with the index
  itself rebuilt from the ADIF source of truth when stale;
- updated in-memory on every completed QSO via `record_worked`
  (coordinator/qso.rs ~446).

So no new query layer was needed — the "in-memory HashSet loaded at
startup + update-on-log" design the batch prompt preferred already
existed; it just wasn't plumbed to the TUI. The relay thread (not the
TUI render loop) performs the lookup, so rendering never blocks on
sqlite or any lock heavier than a parking_lot read.

**Plumbing**: `tui_relay.rs` clones the Arc into the relay thread and
calls a new `worked_before_for(lookup, callsign, freq_hz)` helper:

- decode path: keyed on the current operating frequency (dial MHz from
  the relay's atomic, converted to Hz) → new
  `DecodedMessageView.worked_before` field → `add_decoded_message`
  carries it into the `DxStation` entry (the app.rs:729 TODO site).
- DX-cluster spot path: keyed on the spot's own frequency → new
  `worked_before` field on `TuiMessage::DxSpot` → `add_dx_spot`.

**Rendering**: DX hunter already had the affordance
(`station.worked_before` → muted callsign + score penalty in
dx_hunter.rs). Band Activity now mutes worked-before callsigns the same
way (directed-at-us styling still wins).

**Canonicalization choice**: uppercase-exact match on the full logged
callsign, band-scoped — i.e., *exactly* `is_duplicate`'s semantics. We
deliberately do NOT strip /P-style suffixes, even though
`callsign_continuity.rs` has base-callsign stripping elsewhere, because
`record_worked`/`seed_worked_from_list` store full callsigns and the
scorer matches them un-stripped. Stripping on the TUI side only would
make the TUI flag stations the scorer still treats as new. Consistency
with the scorer is guaranteed by construction (same Arc, same method);
the seam test `worked_before_matches_scorer_duplicate_semantics`
asserts agreement case-by-case, including the /P non-strip.

**Known soft spot** (documented, accepted): a `DxStation` entry's flag
is computed at decode/spot arrival time; completing a QSO mid-session
flips the lookup immediately (`record_worked`) but an already-rendered
DX-list entry only refreshes on that station's next decode/spot.
cqdx network-only spots (`merge_spot_groups`) default to false — the
relay never sees them per-callsign with a frequency we can key on; they
upgrade when heard locally.

## 2. S-meter (`TuiMessage::SignalStrengthUpdate`)

**Decision: wire the real rig S-meter.** The capability already
existed end-to-end except for the producer:
`RigControl::get_s_meter()` → rigctld `\get_level STRENGTH` (real read,
cached in `last_signal_strength`), and the bus had
`RigControlMessage::SignalStrengthResponse` with zero producers.

- `coordinator/hamlib.rs` polling loop: every 4th frequency tick (one
  STRENGTH read per 2s — modest, same serial CAT link as TX) sends
  `SignalStrengthResponse` to the TUI. Failed reads skip silently (no
  fake data, doesn't count as poll failure).
- `tui_relay.rs`: maps it to `TuiMessage::SignalStrengthUpdate`.
- Honesty fixes along the way:
  - field renamed `dbm` → `db_over_s9` on both enums: hamlib STRENGTH
    is dB relative to S9 (0 = S9, -54 ≈ S0), not dBm. rigctld.rs
    comment corrected too.
  - **latent bug fixed**: `App::update_signal_strength` wrote the dB
    value into `audio_level` (a 0.0–1.0 RMS ratio) — it would have
    broken the audio gauge the moment a real reading arrived. S-meter
    now lives in its own `signal_strength_db` field.
- Rendering: `station_info.rs` audio row shows `S-meter: S9+20` (hamlib
  convention via `format_s_meter`, 6 dB/S-unit), `---` when no rig or
  the last reading is >10s stale.

Note: the mock rig's `simulate_s_meter` produces dBm-flavored values
(-50..-120) which render as ~S0 under the S9-relative convention —
cosmetic only, mock-bench display.

## Tests added

- `pancetta` (tui_relay): `worked_before_matches_scorer_duplicate_semantics`,
  `worked_before_handles_missing_callsign`,
  `worked_before_updates_live_on_record_worked`.
- `pancetta-tui` (app): `add_decoded_message_carries_worked_before_into_dx_station`,
  `s_meter_update_does_not_clobber_audio_level`,
  `s_meter_display_goes_stale`, `format_s_meter_follows_hamlib_convention`.

Pre-existing, untouched: 7 clippy errors in `pancetta/benches/pipeline_bench.rs`
(confirmed present on clean main via stash test; `cargo test --workspace`
does not build that bench target).
