# PAN-61: Saved rig-config bookmarks — design

**Ticket:** PAN-61 — "Operators should be able to save and load named rig-config bookmarks."
Follow-on to PAN-59 (live rig-config switch, PR #332, merged 2026-09-02), which added a
dedicated `i`-opened modal for editing the *single* live rig config (model / serial port / baud
rate / PTT method) and reconnecting Hamlib without restarting. PAN-59 explicitly deferred
named/saved profiles as a separate, larger ticket — this is that ticket.

## Decision: nest bookmarks inside `RigConfig`, mirror `MemoryChannelConfig`'s precedent

`pancetta-config::RigConfig` (`pancetta-config/src/rig.rs`) already has a same-domain precedent
for "a named list the operator builds up over time": `FrequencyConfig.memory_channels:
MemoryChannelConfig`, whose `quick_memories: Vec<MemoryChannel>` is exactly this shape — a
`Vec` of small named structs, defaulting empty, validated/merged like any other section.

Bookmarks get the same treatment, one level shallower (no wrapper struct needed — there's no
other bookmark-related config to group with it):

```rust
/// A saved rig-config bookmark — the same 4 fields the `i` picker edits
/// (PAN-59), named so the operator can save-as and load-later instead of
/// re-typing them each time (PAN-61).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RigBookmark {
    pub name: String,
    pub model: String,
    pub port: String,
    pub baud_rate: u32,
    pub ptt_method: PttMethod,
}
```

Added to `RigConfig` as a new field:

```rust
/// Saved rig-config bookmarks (PAN-61) — named shortcuts for the 4 fields
/// the `i` picker edits. Empty by default; grows only via explicit
/// operator "save" actions in the TUI.
#[serde(default)]
pub bookmarks: Vec<RigBookmark>,
```

Flat 4-field shape only — matches PAN-59's own scope note ("the live-switch modal covers
exactly the same 4 fields as the wizard, nothing more"). No nested `CatInterfaceConfig`/`PttConfig`,
no coverage of the other ~8 `RigConfig` sub-sections.

**Merge:** `RigConfig::merge_with` (which the `merge_with_carries_every_field` guardrail test in
`lib.rs` requires a line for on every new field) gets:

```rust
if !other.bookmarks.is_empty() {
    self.bookmarks = other.bookmarks;
}
```

Same "always take the other layer's value unless it's empty" idiom already used for `model` and
`mode` in the same function — a higher-priority config layer (e.g. an env-var or CLI-flag layer,
if one is ever built) that doesn't specify bookmarks leaves the lower layer's list untouched;
one that does specify any replaces the whole list, not a per-entry union. This mirrors every
sibling nested config's wholesale-replace `merge_with`.

**Validation:** no new `validate_section` rule. An empty `name` or a bookmark referencing an
unrecognized model is allowed to persist (bookmarks are inert data until loaded+applied) — the
existing `model_recognized` check in the `SelectRig` handler (`tui_relay.rs`) already gates
*application*, and reusing it there means bookmarks need no separate validation path.

## Persistence: `Config::set_rig_bookmarks_in_file`

New method on `Config` (`pancetta-config/src/lib.rs`), structurally identical to the existing
`set_rig_in_file` (added by PAN-59, same file): read the config file as a `toml::Table` (empty
table if the file doesn't exist yet), get-or-insert the `[rig]` table, set its `bookmarks` key to
a TOML array-of-tables built from the given `&[RigBookmark]`, write back via the same
`write_secure_atomic` (owner-only, atomic) used by every other targeted-write helper in the file.

```rust
pub fn set_rig_bookmarks_in_file<P: AsRef<std::path::Path>>(
    &self,
    path: P,
    bookmarks: &[crate::rig::RigBookmark],
) -> ConfigResult<()>
```

Writes the **whole current list** every time (not a diff/append) — called after every in-memory
mutation (save or delete) with the full post-mutation `cfg.rig.bookmarks`, same "read whole file,
patch one subtree, write back" shape `set_rig_in_file` and `set_audio_devices_in_file` already
use, so every other TOML key in the file is preserved untouched.

## UI: extends the existing `i` modal — no new top-level keybinding

PAN-59's modal-scoped keys (Tab/Up/Down/Esc/Enter inside the rig picker) are deliberately *not*
in the top-level `KEYBINDINGS` table (`keymap.rs`'s own comment: "Modal-scoped keys ... are
documented by each modal's own footer, not here"). Bookmarks add two more modal-scoped keys,
`b` and `s`, with zero top-level keymap/README/docs churn.

**From the 4-field form** (`RigSelectionState`, unchanged Tab-cycle over Model/Port/Baud/Ptt):

- **`b`** — opens a stacked "load bookmark" overlay on top of the form:
  - Up/Down selects among `rig_selection.bookmarks` (each row shows name + `model @ port,
    baud, ptt`)
  - **Enter** loads the highlighted bookmark's 4 values into the underlying form's
    Model/Port/Baud/Ptt fields and closes the overlay — it does **not** apply anything live. The
    operator reviews the now-populated form and presses the form's own Enter to apply, exactly
    as they would for a hand-typed edit today.
  - **Esc** closes the overlay without loading anything.
  - **`x`** deletes the highlighted bookmark: sends `TuiCommand::DeleteRigBookmark`, list
    refreshes in place from the coordinator's response.
- **`s`** — opens an inline name-input (reuses the Model field's existing char-push/pop text-edit
  idiom, new `RigSelectionState` field `bookmark_name_input: String` + `naming_bookmark: bool`):
  captures the form's **current** 4 values (whatever's displayed right now — pre-filled from live
  config when the modal opened, or mid-edit values if the operator changed something first,
  applied or not) under the typed name.
  - **Enter** confirms: sends `TuiCommand::SaveRigBookmark { name, model, port, baud_rate,
    ptt_method }`.
  - **Esc** cancels naming, returns to the form with no changes.

Deliberately *not* merged into the `RigField` Tab-cycle (no 5th `Bookmarks` field): overloading
Enter's meaning per-field ("apply live" on Model/Port/Baud/Ptt vs. "load" on a hypothetical
Bookmarks field) would make the same key do two different, unrelated things depending on focus —
a footgun PAN-59's review process would flag. A separate overlay keeps Enter's meaning constant
in every state.

## Wiring: two new `TuiCommand`s, zero new reconnect logic

New variants on `pancetta_tui::tui_runner::TuiCommand`:

```rust
SaveRigBookmark {
    name: String,
    model: String,
    port: String,
    baud_rate: u32,
    ptt_method: pancetta_config::rig::PttMethod,
},
DeleteRigBookmark {
    name: String,
},
```

New match arms in `tui_relay.rs`, alongside (not replacing) the existing `SelectRig` arm:

- **`SaveRigBookmark`**: write-lock `cmd_config`, upsert into `cfg.rig.bookmarks` by `name`
  (replace the existing entry if the name matches, else push) — this is the "save-as, overwrite
  if name exists" semantics. If the resulting list length is `>= 20`, the status message includes
  an advisory note ("consider deleting unused ones") but the save always succeeds — no hard cap.
  Persist via `set_rig_bookmarks_in_file`, push `TuiMessage::RigBookmarksUpdate { bookmarks:
  cfg.rig.bookmarks.clone() }`, report outcome via the existing `TuiMessage::StatusUpdate`
  pattern (component `"rig"`, same as `SelectRig`).
- **`DeleteRigBookmark`**: write-lock, remove the entry matching `name` (no-op with a status
  message if not found), persist, push `RigBookmarksUpdate`, `StatusUpdate`.

**Neither handler touches `hamlib_reconnect_tx`, `run_main_loop`, or `teardown_hamlib`/
`start_hamlib_component` at all.** They are pure `Vec<RigBookmark>` list mutations plus a
targeted TOML write — the same category of change as `SelectDevice`'s audio-device persistence,
not a Hamlib-adjacent change. Loading a bookmark and applying it live is *only* ever done by the
operator's own subsequent Enter press on the form, which sends the existing, already-hardened
`TuiCommand::SelectRig` — completely unmodified by this ticket. This was the explicit design
constraint from the ticket (PAN-59's execution surfaced 2 Critical + 7 Important findings mostly
around Hamlib reconnect safety; PAN-61 must not reopen that surface).

## Data flow

1. **Startup** (`tui_relay.rs`, same site as PAN-59's `RigConfigUpdate` push): coordinator reads
   `cfg.rig.bookmarks` and includes it in the existing `TuiMessage::RigConfigUpdate` push (new
   field: `bookmarks: Vec<pancetta_config::rig::RigBookmark>`). `App::apply_rig_config_update`
   seeds `rig_selection.bookmarks` from it, same call site that already seeds the 4 live-value
   fields.
2. **Operator presses `i`**: existing modal opens, pre-populated with live values (unchanged).
3. **Operator presses `b`**, picks a bookmark, presses Enter on the overlay: overlay closes, the
   4 underlying form fields now show the bookmark's values (local `App` state only — no message
   sent yet).
4. **Operator presses the form's Enter**: unchanged `TuiCommand::SelectRig` path — validates the
   model, persists via `set_rig_in_file`, requests a live Hamlib reconnect exactly as PAN-59
   built it.
5. **Operator presses `s`**, types a name, presses Enter: `TuiCommand::SaveRigBookmark` sent →
   coordinator upserts + persists + pushes `RigBookmarksUpdate` → `App` refreshes
   `rig_selection.bookmarks` (same handling as any other `TuiMessage` in the TUI's receive loop).
6. **Operator presses `x` on a bookmark row**: `TuiCommand::DeleteRigBookmark` → same
   upsert/persist/push cycle, minus the entry.

## Non-goals

- `pancetta setup` CLI wizard integration — bookmarks are a TUI-only feature; the blocking
  stdin-based wizard (`setup_rig`/`setup_ptt`, `pancetta/src/main.rs`) is untouched, same
  reasoning PAN-59 used for why the wizard's prompts can't be reused live.
- The other ~8 `RigConfig` sub-sections (frequency limits, band/antenna switching, power control,
  filters, calibration, quirks) — unchanged scope from PAN-59.
- PAN-60 (re-enumerate serial ports when the picker opens) — related but separate; not folded in.
- Per-entry/field-level bookmark merging across config layers — `merge_with` replaces the whole
  list wholesale, consistent with every sibling nested `RigConfig` section.
