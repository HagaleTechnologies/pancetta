# Operator TX-Offset Control + Multi-TX De-confliction — Design Spec

**Date:** 2026-06-28
**Status:** Proposed (awaiting operator review)
**Author:** Claude Opus 4.8 (under K5ARH supervision)

## Goal

Let the operator place pancetta's TX audio offset where they want (the WSJT-X "Hold Tx Freq" way of
operating), and stop concurrent multi-TX QSOs from stacking on the same offset. One sentence: *our TX
audio offset is chosen by operator intent first, collision-avoidance second, and only falls back to
"answer on the DX's frequency" when nothing else applies.*

## Background / problem (from the 2026-06-27 offset trace)

Today, on every **manual** call/answer (`StartQso` / `respond_to_caller` / pounce), pancetta latches
`QsoMetadata.frequency` = the **DX's decoded RX frequency** (Tx=Rx) — `qso_manager.rs` (the
`respond_to_cq_with` body) sourced from `app.rs::get_selected_station`. Consequences the operator hit:

1. **The operator's chosen offset is ignored.** The TUI has an `app.tx_offset_hz` field but **no
   keybinding writes it and it is never plumbed into the call path** — it's dead. `TxFreqMode`
   (Hold/Auto, toggled by `f`) exists and the *autonomous* path honors a held offset
   (`allocate_smart_frequency` → `config.tx_offset_hz` when `!tx_freq_auto()`), but the **manual path
   never consults any of it.** Operator feedback (2026-06-28): "I don't typically operate Tx=Rx in
   WSJT-X" — they run Hold Tx Freq and pick their own spot. So Tx=Rx-by-default is the wrong default
   for how they work; it should be a fallback.
2. **Multi-TX stacks.** Each concurrent manual QSO latches its own DX's freq, so two DX close in the
   passband (or two network spots both defaulting to 1500 Hz) **collide on the same TX offset**. The
   own-frequency separation rule (criterion #7, `min_separation_hz = 75`) lives **only** in the
   autonomous `SmartFrequencyAllocator` — the manual path never runs it.

The enabler that makes the fix safe now: Hound's **`QsoMetadata.partner_freq`** already decouples "the
DX's RX frequency" (used by the relevance gate) from "our TX offset" (`frequency`). So we can move our
TX offset anywhere and still route the DX's replies, exactly as Hound does — FT8 decodes the whole
passband, so the DX still hears us off-frequency.

## Design

### TX-offset selection (the one decision point)

At **manual QSO open** (`StartQso` / `respond_to_caller`; pounce keeps Tx=Rx — see non-goals), choose
our TX audio offset `tx_off` by this priority, then **always set `partner_freq = Some(dx_freq)`** when
`tx_off != dx_freq` so the relevance gate tracks the DX:

1. **Operator-held offset (primary).** If `TxFreqMode::Hold` AND an operator TX offset is set AND this
   is the **first/only** active TX QSO → `tx_off = operator_offset`. (This is the WSJT-X Hold-Tx-Freq
   way the operator actually works.)
2. **De-conflict (multi-TX).** Otherwise, compute the candidate (`operator_offset` if held, else
   `dx_freq`) and if it lands within `MIN_TX_SEPARATION_HZ` (75 Hz) of any already-active QSO's TX
   offset, **nudge** to the nearest clear slot in `[300, 2700]` (step 25 Hz, away from occupied
   offsets). A single chosen offset is meaningless for N streams, so multi-TX always de-conflicts
   rather than honoring one offset for all.
3. **Fallback = Tx=Rx.** If not held and no collision → `tx_off = dx_freq` (today's behavior). This is
   the only path that leaves `partner_freq = None` (Tx=Rx, byte-identical to today).

**Regression invariant:** with `TxFreqMode::Auto` (or no operator offset) AND no collision, every
manual QSO is Tx=Rx with `partner_freq = None` — identical to today. The new behavior only activates
when the operator holds an offset or when streams would otherwise collide.

### De-confliction helper (shared)

Extract a pure `deconflict_offset(candidate: f64, occupied: &[f64], min_sep: f64, range) -> f64` that
returns `candidate` if clear, else the nearest in-range offset ≥ `min_sep` from all `occupied`
(deterministic search outward in 25 Hz steps). Unit-tested in isolation. Reuse it for the manual path;
optionally have the autonomous allocator's criterion #7 delegate to it later (not required for v1).
`occupied` = the TX offsets of currently-active QSOs (from the QSO manager's active set /
`active_tx_qsos` freqs).

### Operator TX-offset control (TUI)

The operator needs a way to SET the held offset (today there's no control). v1 proposal:
- **`f`** already toggles `TxFreqMode` (Hold/Auto) — keep.
- **Add a "set TX offset" input**: a small modal (mirror the `Shift+F` frequency modal pattern) bound
  to a free key — proposed **`o`** (offset) — that prompts for an audio offset in Hz (200–2900),
  stores it into the held-offset state, and implies `TxFreqMode::Hold`. Blank/clear → Auto.
- Show the held offset + mode in the status/TX strip ("TX offset: 1500 Hz (HOLD)" vs "TX: auto").
- The held offset is a coordinator-level value (an atomic or small shared state) the manual call path
  reads at QSO open — mirroring how `split_tx_frequency_hz` / `operating_frequency_hz` are shared.
  (The dead `app.tx_offset_hz` TUI field gets wired through `TuiCommand` → bus → coordinator state.)

*(Open: `o` vs another key; modal vs +/- nudge keys. See Open questions.)*

### Where it hooks (from the trace)

- `pancetta-qso/src/qso_manager.rs`: the manual-open offset latch (`respond_to_cq_with` /
  `respond_to_caller`) takes the chosen `tx_off` + sets `partner_freq` like `engage_hound` does. Add
  the selection logic (or compute it in the coordinator and pass `tx_off` + `dx_freq` in).
- `pancetta/src/coordinator/qso.rs`: the `StartQso` / `RespondToCaller` handlers compute `tx_off` from
  the held-offset state + the active-QSO offsets (de-conflict) before calling the manager. (Cleaner to
  decide in the coordinator, which already knows the active set, and pass both freqs to the manager.)
- `pancetta-tui` + `coordinator/tui_relay.rs`: the `o` modal → `TuiCommand::SetTxOffset` → bus →
  coordinator held-offset state; status display.
- Held-offset shared state: a new `tx_offset_hold_hz: Arc<AtomicU64>` (0 = Auto/unset) on the
  coordinator, beside `split_tx_frequency_hz`.

## Scope / non-goals (v1)

- **Manual path only.** Autonomous offset selection (the `SmartFrequencyAllocator`) is unchanged; it
  already honors held offset + de-conflicts. (Optionally refactor its #7 to share `deconflict_offset`
  later.)
- **Pounce stays Tx=Rx.** The autonomous pounce already overwrites to the DX freq; not touched here.
- **No waterfall.** Offset is set numerically (modal/keys), not by clicking a spectrum (deferred).
- **No rig-level split interaction.** This is an *audio-offset* concern within the passband; the RX≠TX
  rig-split feature is separate.
- **Hound unaffected.** Hound sets its own offsets (call-low / QSY-high) and ignores the held offset.

## Risks / careful points

1. **Regression:** Auto + no-collision must stay exactly Tx=Rx with `partner_freq=None`. Property/unit
   test guards it (mirrors the Hound `partner_freq=None` regression guard).
2. **`partner_freq` now set on NORMAL QSOs** (when held/de-conflicted): relies on the Hound relevance-
   gate split (already shipped + regression-tested). Confirm the Fox-less normal case routes the DX's
   replies via `partner_freq` correctly (same code path Hound uses).
3. **Completed-QSO RF stamp** uses `metadata.frequency` (our TX offset) → correct (our real TX RF), as
   for Hound. The DX's freq (`partner_freq`) is display/routing only. Audit the stamp sites.
4. **De-confliction determinism:** no RNG; deterministic outward search (test).
5. **Operator-offset out of band:** clamp/validate to `[200, 2900]`; ignore + warn otherwise.

## Testing

- **Unit:** `deconflict_offset` (clear passes; collision nudges ≥75 Hz; deterministic; in-range).
- **Engine/coord:** held offset honored for a single manual QSO (TX keys on operator offset,
  `partner_freq=DX`); two concurrent manual QSOs at near offsets → de-conflicted (distinct keyed
  offsets ≥75 Hz apart); **regression**: Auto + single + no-collision → Tx=Rx, `partner_freq=None`.
- **coord_sim:** rig-level — set held offset → manual QSO keys on it; start two close DX → both key on
  distinct offsets.
- The DX's reply (at its own freq) still advances each QSO (relevance via `partner_freq`).

## Open questions for review

1. **Set-offset UX:** modal on `o` (proposed) vs. `+`/`-` nudge keys vs. both? Any key conflict with
   `o`? (I'll verify `o` is free.)
2. **Does honoring the held offset apply to the *first* concurrent QSO only** (subsequent ones
   de-conflict around it), or should even the first de-conflict if it collides with an existing one?
   (Proposed: first honors; the active set is checked so it won't collide with an existing QSO — if the
   held offset itself collides with an active QSO, de-conflict it too.)
3. **Should `respond_to_caller` (answering someone calling US) honor the held offset too**, or only
   `StartQso` (us calling out)? (Proposed: both honor it — it's the operator's TX placement either way.)
