# DXCC entity display (#171) + Band Activity own-TX logging (#172)

## Context

Two small, independent TUI polish items, both nice-to-have and non-blocking. Bundled in one spec
because they were brainstormed together, but they touch disjoint code and will land as separate
PRs (#171 first, #172 second — #172 is the larger of the two).

DX Hunter (`pancetta-tui/src/ui/dx_hunter.rs`) already established the entity-resolution pattern
this design reuses: prefer cqdx's authoritative `entity_name` when available, fall back to the
offline prefix table (`crate::dxcc::entity_for_callsign`, `pancetta-tui/src/dxcc.rs:28`) for
locally-decoded stations cqdx hasn't reported. `station_card.rs` shows a narrower version of the
same idea (cqdx-only, no offline fallback) for the currently-focused DX station.

## Part A — #171: DXCC entity in QSO Status + station info

### QSO Status panel

`qso_status.rs::render_qso_info` builds the "Status: ... Call: ..." line
(`qso_status.rs:232-252`) from `app.qso_status()` (a `QsoStatus`, `app.rs:306-358` — no
DXCC-related field, and none is added; this stays a pure render-time lookup).

Add a small helper, following the DX Hunter fallback chain but widened one step further to prefer
`DxStation.entity_name` (cqdx) exactly as `station_card.rs` does, then fall back to the offline
table:

```rust
// qso_status.rs
fn resolve_entity(app: &App, call_sign: &str) -> String {
    app.dx_stations
        .get(call_sign)
        .and_then(|d| d.entity_name.clone())
        .or_else(|| crate::dxcc::entity_for_callsign(call_sign).map(str::to_string))
        .unwrap_or_else(|| "---".to_string())
}
```

Append to the existing "Call:" span in `render_qso_info` only when `qso.active` and `call_sign` is
`Some`: `Call: JA1ABC (Japan)` — entity in a muted/parenthetical style so it doesn't compete
visually with the callsign itself. When unresolvable, omit the parenthetical entirely (not
`(---)`)  — the QSO Status panel has less horizontal room than DX Hunter's dedicated column, so a
present-but-unresolved entity should stay invisible rather than add noise.

### Own-station display

Title bar (`pancetta-tui/src/ui/mod.rs::render_title_bar`, lines 598-642) is the "our own station"
display — always-visible callsign/grid/freq/band/mode. Add the entity next to the existing
grid-square span.

`StationInfo` (`app.rs:361-369`) gains one field:

```rust
pub struct StationInfo {
    // ...existing fields...
    pub entity_name: Option<String>,
}
```

Computed once at `App::new` construction (`app.rs:1118-1127`), not per-frame — the home callsign
is fixed for the process lifetime, unlike the QSO partner:

```rust
entity_name: crate::dxcc::entity_for_callsign(&config.station.call_sign).map(str::to_string),
```

Render as `K5ARH  EM10 (United States)` in the title bar, same "omit if `None`" rule as above.

### Testing

- `resolve_entity` unit tests: cqdx `DxStation.entity_name` wins when present; falls back to the
  offline table when absent; returns `"---"` for an unresolvable callsign (matches the existing
  `dx_hunter.rs` fallback tests' shape).
- `StationInfo::entity_name` populated correctly from a known-prefix callsign at construction.

## Part B — #172: Band Activity logs our own TX

### New bus event

`message_bus::MessageType` (`pancetta/src/message_bus.rs`) gains a new variant, additive —
`TxQueueStatus`/`TxItem` (the transient "now sending" snapshot `qso_status.rs`'s "Now:" line
already reads) is untouched:

```rust
TxFrameLogged {
    text: String,
    freq_hz: f64,
    qso_id: Option<String>,   // None = CQ / manual, matches TxItem's existing convention
    timestamp: chrono::DateTime<chrono::Utc>,
},
```

Emitted from the coordinator at the same PTT-key point(s) in `tx.rs` where `send_tx_queue_status`
already fires (Step 5, PTT assert) — one `TxFrameLogged` per actual keyed frame, timestamped at
key-time rather than at TUI-receive-time so the ordering is accurate even under scheduling jitter.
No filtering by `qso_id` — per the "log all our TX, including CQ" scope decision, every keyed
frame is logged regardless of origin.

`pancetta/src/coordinator/tui_relay.rs` gets a new arm translating this into
`TuiMessage::TxFrameLogged { text, freq_hz, qso_id, timestamp }`, and `tui_runner.rs` routes it to
a new `App::add_tx_frame(...)` — same shape as the existing #165 `QsoHistoryEntry` wiring
(`TuiMessage::QsoHistoryEntry` → `App::push_qso_history`), which is the precedent this reuses.

### TUI storage — reuse `DecodedMessageView`

Rather than a parallel row type, `DecodedMessageView` (`app.rs:112-172`) gains one field:

```rust
pub struct DecodedMessageView {
    // ...existing fields...
    pub is_own_tx: bool,
}
```

Every existing construction site (wherever RX decodes are turned into `DecodedMessageView`, feeding
`App::add_decoded_message`) sets `is_own_tx: false` explicitly.

`App::add_tx_frame` builds one from a `TxFrameLogged` event:

```rust
fn add_tx_frame(&mut self, text: String, freq_hz: f64, qso_id: Option<String>, timestamp: DateTime<Utc>) {
    self.decoded_messages.push_back(DecodedMessageView {
        timestamp,
        frequency: freq_hz,
        mode: self.station_info.mode.clone(),
        snr: 0,                 // sentinel; renderer shows "TX" instead, see below
        delta_time: 0.0,
        delta_freq: 0.0,
        call_sign: None,        // full exchange text is in `message`; no need to parse a callsign out
        grid_square: None,
        message: text,
        distance: None,
        bearing: None,
        slot_parity: None,
        is_directed_at_us: true, // pins into the same tier as the RX half of the exchange (ordering decision below)
        worked_before: false,
        needed: false,
        atno: false,
        band_needed: false,
        priority_score: None,
        is_own_tx: true,
    });
    while self.decoded_messages.len() > 1000 {
        self.decoded_messages.pop_front();
    }
}
```

Reuses the exact same deque, cap (1000), and prune logic RX rows already go through
(`app.rs:1156, 1605-1607`) — no changes to `App::displayed_messages()` (`app.rs:2645-2669`) or
`App::get_selected_station`, both of which keep walking the same list/ordering they always have.

**Ordering decision:** `is_directed_at_us: true` unconditionally. Band Activity already pins
directed-at-us rows to the top, newest-first, chronologically — treating every one of our own TX
frames as "directed" means a QSO's full back-and-forth (our RPT, their RR73, our 73, all
interleaved with what we heard) reads as one coherent chronological block in that tier, which is
exactly the "see both sides of the exchange" ask. This was chosen over a full re-sort of the whole
list specifically to avoid touching the shared ordering function other features (selection,
Hunt-view CQ filtering) depend on.

### Rendering

`band_activity.rs` checks `is_own_tx` per row and:
- Renders a `»` marker (not color-only) in the leading/Time column.
- Uses a distinct style — reusing the theme's warning/TX color already established by the QSO
  Status "🔴 TX" live line, for visual consistency across panels.
- Renders `TX` instead of a numeric SNR in the SNR column (the `snr: 0` sentinel would otherwise be
  indistinguishable from a real 0 dB decode) — same pattern as `dx_hunter.rs::format_dx_snr`'s
  existing "---" -for-missing-SNR precedent.

### Testing

- `add_tx_frame` unit test: produces a `DecodedMessageView` with `is_own_tx: true`,
  `call_sign: None`, correct `message`/`frequency`/`timestamp` passthrough.
- Deque cap/prune still holds at 1000 with a mix of RX and TX rows.
- A pure row-marker formatter (mirroring `format_qso_history_line`'s testable-without-a-render-
  backend style) verifying the `»` marker and `TX` SNR substitution apply only when `is_own_tx`.
- `TxFrameLogged` → `tui_relay.rs` → `TuiMessage::TxFrameLogged` field passthrough test.

## Out of scope

- Re-deriving a `call_sign` from the TX message text (e.g. parsing "K5ARH JA1ABC RR73" to extract
  "JA1ABC") — the full text is already visible in the Msg column; not worth the parsing surface for
  a nice-to-have panel.
- Changing `App::displayed_messages()`'s two-tier ordering scheme itself, or any other feature that
  depends on it.
- Threading DXCC entity data into the coordinator-side `ActiveQsoBanner`/`ActiveQsoSnapshotItem`
  DTOs — #171 stays entirely TUI-side.
