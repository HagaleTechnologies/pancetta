# PAN-61: Saved Rig-Config Bookmarks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the operator save the rig picker's 4 fields (model/port/baud/PTT) as a named bookmark and load one back into the picker form later, without retyping.

**Architecture:** A new `RigBookmark` list nested in `pancetta-config`'s `RigConfig` (mirroring the existing `MemoryChannelConfig` precedent), persisted via a new targeted-write helper. The TUI's existing `i`-opened rig picker modal (PAN-59) grows two modal-scoped keys — `b` (load-from-bookmark overlay) and `s` (save-current-form-as-bookmark) — that never touch the existing `SelectRig` → Hamlib-reconnect path. Two new `TuiCommand`s (`SaveRigBookmark`, `DeleteRigBookmark`) are pure config-list mutations in the coordinator, upserting/removing by name and persisting the whole list.

**Tech Stack:** Rust, ratatui (TUI), serde + toml (config), tokio (async coordinator), ticket PAN-61.

**Spec:** `docs/superpowers/specs/2026-09-02-pan-61-saved-rig-bookmarks-design.md`

## Global Constraints

- Bookmark shape is exactly the 4 fields the picker already edits: `name`, `model`, `port`, `baud_rate`, `ptt_method` — no other `RigConfig` sub-section.
- Save semantics: save-as-name, **overwrite** if the name already exists (no reject-duplicate path).
- Soft cap of 20 bookmarks: **advisory only** — a save past the cap always succeeds, with a warning appended to the status message. Never a hard block.
- Picking a bookmark loads its 4 values into the existing form fields; it must **never** send `TuiCommand::SelectRig` or otherwise apply anything live by itself. Only the form's own (unmodified) Enter key applies live.
- **No new code may call `hamlib_reconnect_tx`, `run_main_loop`, `teardown_hamlib`, or `start_hamlib_component`.** Every new handler in this plan is a pure `Vec<RigBookmark>` mutation plus a targeted TOML write — the same category of change as `SelectDevice`'s audio-device persistence, never Hamlib-adjacent.
- No new top-level keybinding — `b`/`s` are modal-scoped (only meaningful while `rig_selection.visible`), so `pancetta-tui/src/keymap.rs` and `docs/KEYBINDINGS.md` are untouched.
- Every new field added to a `ConfigSection`-implementing struct (`RigConfig`) needs a corresponding line in that struct's `merge_with` — enforced by the `merge_with_carries_every_field` guardrail test in `pancetta-config/src/lib.rs`.
- Run tests scoped to the crate you're editing (`cargo test -p pancetta-config`, `cargo test -p pancetta-tui`, `cargo test -p pancetta`); run `cargo test --workspace --features transmit` once at the end, per `AGENTS.md`.
- **Compile-atomicity within `pancetta-tui`:** `App::apply_rig_config_update`'s signature and its one call site (in `tui_runner.rs`'s `process_messages`) live in the same crate and must change together in the same task — a task that changes one without the other leaves the crate (and therefore every test in it) unable to compile.

---

## File Structure

- `pancetta-config/src/rig.rs` — new `RigBookmark` struct, `RigConfig.bookmarks` field, `merge_with` line, unit tests. (Task 1)
- `pancetta-config/src/lib.rs` — new `Config::set_rig_bookmarks_in_file`, its tests, and one line changed in the existing `merge_with_carries_every_field` test. (Tasks 1, 2)
- `pancetta-tui/src/app.rs` — `RigSelectionState` new fields/methods, `App::apply_rig_config_update` signature change, tests. (Task 3)
- `pancetta-tui/src/tui_runner.rs` — `TuiMessage`/`TuiCommand` new variants, `process_messages` wiring, key handling, rendering, tests. (Tasks 3, 4, 5)
- `pancetta/src/coordinator/tui_relay.rs` — pure upsert/status-message helpers + tests, new `SaveRigBookmark`/`DeleteRigBookmark` match arms, extended startup `RigConfigUpdate` push. (Tasks 6, 7)

---

### Task 1: `RigBookmark` type + `RigConfig.bookmarks` field + merge

**Files:**
- Modify: `pancetta-config/src/rig.rs`
- Modify: `pancetta-config/src/lib.rs:1041` (guardrail test)

**Interfaces:**
- Produces: `pancetta_config::rig::RigBookmark { name: String, model: String, port: String, baud_rate: u32, ptt_method: PttMethod }` (`Debug, Clone, Serialize, Deserialize`); `RigConfig.bookmarks: Vec<RigBookmark>` (defaults empty).

- [ ] **Step 1: Write the failing tests in `pancetta-config/src/rig.rs`**

Add to the `#[cfg(test)] mod tests` block at the bottom of the file (after `operating_mode_cycle_order`):

```rust
    #[test]
    fn rig_bookmarks_default_to_empty() {
        let config = RigConfig::default();
        assert!(config.bookmarks.is_empty());
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn merge_with_replaces_bookmarks_wholesale_when_other_nonempty() {
        let mut base = RigConfig::default();
        base.bookmarks = vec![RigBookmark {
            name: "Old".to_string(),
            model: "OldRig".to_string(),
            port: "/dev/ttyUSB0".to_string(),
            baud_rate: 9600,
            ptt_method: PttMethod::None,
        }];

        let mut other = RigConfig::default();
        other.bookmarks = vec![RigBookmark {
            name: "New".to_string(),
            model: "FTdx10".to_string(),
            port: "/dev/ttyUSB1".to_string(),
            baud_rate: 38400,
            ptt_method: PttMethod::Cat,
        }];

        base.merge_with(other);
        assert_eq!(base.bookmarks.len(), 1);
        assert_eq!(base.bookmarks[0].name, "New");
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn merge_with_keeps_bookmarks_when_other_empty() {
        let mut base = RigConfig::default();
        base.bookmarks = vec![RigBookmark {
            name: "Keep".to_string(),
            model: "FTdx10".to_string(),
            port: "/dev/ttyUSB0".to_string(),
            baud_rate: 38400,
            ptt_method: PttMethod::None,
        }];
        let other = RigConfig::default();
        base.merge_with(other);
        assert_eq!(base.bookmarks.len(), 1);
        assert_eq!(base.bookmarks[0].name, "Keep");
    }
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cargo test -p pancetta-config rig_bookmarks_default_to_empty`
Expected: FAIL — `no field \`bookmarks\` on type \`RigConfig\`` / `cannot find struct \`RigBookmark\``

- [ ] **Step 3: Add `RigBookmark` and the `bookmarks` field**

In `pancetta-config/src/rig.rs`, immediately above `/// Rig control configuration` (the `RigConfig` doc comment), add:

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

In `RigConfig`, add a field after `custom_commands` (still inside the struct body, before the closing `}`):

```rust
    /// Saved rig-config bookmarks (PAN-61) — named shortcuts for the 4
    /// fields the `i` picker edits. Empty by default; grows only via
    /// explicit operator "save" actions in the TUI.
    #[serde(default)]
    pub bookmarks: Vec<RigBookmark>,
```

In `impl Default for RigConfig`, add a line to the struct literal (after `custom_commands: HashMap::new(),`):

```rust
            bookmarks: Vec::new(),
```

- [ ] **Step 4: Add the `merge_with` line**

In `impl ConfigSection for RigConfig`'s `merge_with`, add after the `mode` block and before the `// Merge complex configurations` comment:

```rust
        // PAN-61: bookmarks replace wholesale (matches every sibling nested
        // config's merge_with below) rather than per-entry union — an unset
        // higher-priority layer (empty list) leaves the lower layer's saved
        // bookmarks untouched.
        if !other.bookmarks.is_empty() {
            self.bookmarks = other.bookmarks;
        }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p pancetta-config rig_bookmarks_default_to_empty merge_with_replaces_bookmarks_wholesale_when_other_nonempty merge_with_keeps_bookmarks_when_other_empty`
Expected: PASS (3 tests)

- [ ] **Step 6: Close the `merge_with_carries_every_field` guardrail gap for `bookmarks`**

The generic `assert_carries_all` helper (`pancetta-config/src/lib.rs`) perturbs each JSON *leaf* it finds in `RigConfig::default()`'s serialized form. An empty `Vec` has no leaves, so the existing bare `assert_carries_all::<rig::RigConfig>("RigConfig", &[], |a, b| a.merge_with(b));` call (line 1041) never actually exercises `bookmarks` — it needs an explicit override, the same mechanism `CatInterfaceConfig`'s enum-as-string fields already use a few lines below it.

In `pancetta-config/src/lib.rs`, replace:

```rust
        assert_carries_all::<rig::RigConfig>("RigConfig", &[], |a, b| a.merge_with(b));
```

with:

```rust
        assert_carries_all::<rig::RigConfig>(
            "RigConfig",
            &[(
                "bookmarks",
                json!([{
                    "name": "Shack",
                    "model": "FTdx10",
                    "port": "/dev/ttyUSB0",
                    "baud_rate": 38400,
                    "ptt_method": "cat",
                }]),
            )],
            |a, b| a.merge_with(b),
        );
```

- [ ] **Step 7: Run the guardrail test to verify it still passes**

Run: `cargo test -p pancetta-config merge_with_carries_every_field`
Expected: PASS

- [ ] **Step 8: Run the full `pancetta-config` test suite**

Run: `cargo test -p pancetta-config`
Expected: PASS (no regressions)

- [ ] **Step 9: Commit**

```bash
git add pancetta-config/src/rig.rs pancetta-config/src/lib.rs
git commit -m "feat(config): PAN-61 add RigBookmark list to RigConfig"
```

---

### Task 2: `Config::set_rig_bookmarks_in_file`

**Files:**
- Modify: `pancetta-config/src/lib.rs`

**Interfaces:**
- Consumes: `crate::rig::RigBookmark` (Task 1).
- Produces: `Config::set_rig_bookmarks_in_file<P: AsRef<Path>>(&self, path: P, bookmarks: &[crate::rig::RigBookmark]) -> ConfigResult<()>` — writes the **whole** given list (replace, not append/diff) into `[rig].bookmarks`, preserving every other TOML key, atomically.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `pancetta-config/src/lib.rs`, immediately after `set_rig_in_file_creates_minimal_file`'s closing `}`:

```rust
    #[test]
    fn set_rig_bookmarks_in_file_writes_array_and_preserves_other_keys() {
        let temp = NamedTempFile::new().unwrap();
        std::fs::write(
            temp.path(),
            "[station]\ncallsign = \"K5ARH\"\n\n[rig]\nmodel = \"FTdx10\"\n",
        )
        .unwrap();

        let config = Config::default();
        let bookmarks = vec![
            crate::rig::RigBookmark {
                name: "Shack".to_string(),
                model: "FTdx10".to_string(),
                port: "/dev/ttyUSB0".to_string(),
                baud_rate: 38400,
                ptt_method: crate::rig::PttMethod::Cat,
            },
            crate::rig::RigBookmark {
                name: "Portable".to_string(),
                model: "IC-7300".to_string(),
                port: "/dev/ttyUSB1".to_string(),
                baud_rate: 19200,
                ptt_method: crate::rig::PttMethod::Vox,
            },
        ];
        config
            .set_rig_bookmarks_in_file(temp.path(), &bookmarks)
            .unwrap();

        let written = std::fs::read_to_string(temp.path()).unwrap();
        let parsed: toml::Table = written.parse().unwrap();
        let rig = parsed["rig"].as_table().unwrap();
        // Unrelated rig key preserved.
        assert_eq!(rig["model"].as_str(), Some("FTdx10"));

        let bookmarks_array = rig["bookmarks"].as_array().unwrap();
        assert_eq!(bookmarks_array.len(), 2);
        let shack = bookmarks_array[0].as_table().unwrap();
        assert_eq!(shack["name"].as_str(), Some("Shack"));
        assert_eq!(shack["port"].as_str(), Some("/dev/ttyUSB0"));
        assert_eq!(shack["baud_rate"].as_integer(), Some(38400));
        assert_eq!(shack["ptt_method"].as_str(), Some("cat"));

        // Unrelated top-level key preserved.
        assert_eq!(
            parsed["station"].as_table().unwrap()["callsign"].as_str(),
            Some("K5ARH")
        );
    }

    #[test]
    fn set_rig_bookmarks_in_file_creates_minimal_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("pancetta.toml");
        let config = Config::default();
        let bookmarks = vec![crate::rig::RigBookmark {
            name: "Shack".to_string(),
            model: "FTdx10".to_string(),
            port: "/dev/ttyUSB0".to_string(),
            baud_rate: 38400,
            ptt_method: crate::rig::PttMethod::None,
        }];
        config
            .set_rig_bookmarks_in_file(&path, &bookmarks)
            .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Table = written.parse().unwrap();
        let rig = parsed["rig"].as_table().unwrap();
        let bookmarks_array = rig["bookmarks"].as_array().unwrap();
        assert_eq!(bookmarks_array.len(), 1);
        assert_eq!(
            bookmarks_array[0].as_table().unwrap()["name"].as_str(),
            Some("Shack")
        );
    }

    #[test]
    fn set_rig_bookmarks_in_file_overwrites_previous_list() {
        let temp = NamedTempFile::new().unwrap();
        let config = Config::default();
        config
            .set_rig_bookmarks_in_file(
                temp.path(),
                &[crate::rig::RigBookmark {
                    name: "Old".to_string(),
                    model: "OldRig".to_string(),
                    port: "/dev/ttyUSB0".to_string(),
                    baud_rate: 9600,
                    ptt_method: crate::rig::PttMethod::None,
                }],
            )
            .unwrap();
        config.set_rig_bookmarks_in_file(temp.path(), &[]).unwrap();

        let written = std::fs::read_to_string(temp.path()).unwrap();
        let parsed: toml::Table = written.parse().unwrap();
        let rig = parsed["rig"].as_table().unwrap();
        assert_eq!(rig["bookmarks"].as_array().unwrap().len(), 0);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pancetta-config set_rig_bookmarks_in_file`
Expected: FAIL — `no method named \`set_rig_bookmarks_in_file\` found`

- [ ] **Step 3: Implement `set_rig_bookmarks_in_file`**

In `pancetta-config/src/lib.rs`, add immediately after `set_rig_in_file`'s closing `}` (same `impl Config` block):

```rust
    /// Persist the operator's saved rig-config bookmarks (PAN-61) into the
    /// config file at `path`, preserving every other key. Writes the WHOLE
    /// given list every call (replace, not append/diff) — callers always
    /// pass the full post-mutation list. Mirrors [`Config::set_rig_in_file`]'s
    /// targeted-write pattern.
    pub fn set_rig_bookmarks_in_file<P: AsRef<std::path::Path>>(
        &self,
        path: P,
        bookmarks: &[crate::rig::RigBookmark],
    ) -> ConfigResult<()> {
        let path = path.as_ref();

        let mut root: toml::Table = match std::fs::read_to_string(path) {
            Ok(contents) => contents
                .parse::<toml::Table>()
                .map_err(|e| ConfigError::Validation(format!("Failed to parse config: {}", e)))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
            Err(e) => return Err(e.into()),
        };

        let rig = root
            .entry("rig".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let rig_table = rig
            .as_table_mut()
            .ok_or_else(|| ConfigError::Validation("[rig] in config is not a table".to_string()))?;

        let bookmarks_value = toml::Value::try_from(bookmarks).map_err(|e| {
            ConfigError::Validation(format!("Failed to serialize rig bookmarks: {}", e))
        })?;
        rig_table.insert("bookmarks".to_string(), bookmarks_value);

        let serialized = toml::to_string_pretty(&root)
            .map_err(|e| ConfigError::Validation(format!("Failed to serialize config: {}", e)))?;
        Self::write_secure_atomic(path, &serialized)?;
        info!("Rig bookmarks persisted to: {}", path.display());
        Ok(())
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pancetta-config set_rig_bookmarks_in_file`
Expected: PASS (3 tests)

- [ ] **Step 5: Run the full `pancetta-config` test suite**

Run: `cargo test -p pancetta-config`
Expected: PASS (no regressions)

- [ ] **Step 6: Commit**

```bash
git add pancetta-config/src/lib.rs
git commit -m "feat(config): PAN-61 add Config::set_rig_bookmarks_in_file"
```

---

### Task 3: State + message/command plumbing for bookmarks

`RigSelectionState`'s new bookmark fields/methods, `App::apply_rig_config_update`'s
signature change, the `TuiMessage`/`TuiCommand` bookmark variants, and the
`process_messages` wiring for them are one atomic unit: `apply_rig_config_update`'s
only caller lives in the same crate (`tui_runner.rs`), so a task that changed one
without the other would leave `pancetta-tui` unable to compile at the checkpoint —
see the Global Constraints note above. This task lands all of it together.

**Files:**
- Modify: `pancetta-tui/src/app.rs`
- Modify: `pancetta-tui/src/tui_runner.rs`

**Interfaces:**
- Consumes: `pancetta_config::rig::RigBookmark` (Task 1).
- Produces on `RigSelectionState`: fields `bookmarks: Vec<pancetta_config::rig::RigBookmark>`, `bookmark_overlay_visible: bool`, `selected_bookmark_idx: usize`, `naming_bookmark: bool`, `bookmark_name_input: String`; methods `load_bookmark(&mut self, idx: usize)`, `move_bookmark_selection_up(&mut self)`, `move_bookmark_selection_down(&mut self)`, `push_bookmark_name_char(&mut self, c: char)`, `pop_bookmark_name_char(&mut self)`.
- Produces on `App`: `apply_rig_config_update` gains a 6th parameter `bookmarks: Vec<pancetta_config::rig::RigBookmark>` (after `current_ptt_method`), seeding `self.rig_selection.bookmarks`.
- Produces on `TuiMessage`: `RigConfigUpdate` gains a `bookmarks: Vec<pancetta_config::rig::RigBookmark>` field; new `RigBookmarksUpdate { bookmarks: Vec<pancetta_config::rig::RigBookmark> }`.
- Produces on `TuiCommand`: new `SaveRigBookmark { name: String, model: String, port: String, baud_rate: u32, ptt_method: pancetta_config::rig::PttMethod }`; new `DeleteRigBookmark { name: String }`.

- [ ] **Step 1: Write the failing tests in `pancetta-tui/src/app.rs`**

Add to the `// === Rig config picker (PAN-59) ===` test section, after `rig_selection_state_model_field_text_editing`:

```rust
    #[test]
    fn rig_selection_state_bookmark_name_input_text_editing() {
        let mut state = RigSelectionState::default();
        state.push_bookmark_name_char('S');
        state.push_bookmark_name_char('K');
        assert_eq!(state.bookmark_name_input, "SK");
        state.pop_bookmark_name_char();
        assert_eq!(state.bookmark_name_input, "S");
    }

    #[test]
    fn rig_selection_state_move_bookmark_selection_clamps_at_bounds() {
        let mut state = RigSelectionState {
            bookmarks: vec![
                pancetta_config::rig::RigBookmark {
                    name: "A".to_string(),
                    model: "FTdx10".to_string(),
                    port: "/dev/ttyUSB0".to_string(),
                    baud_rate: 38400,
                    ptt_method: pancetta_config::rig::PttMethod::None,
                },
                pancetta_config::rig::RigBookmark {
                    name: "B".to_string(),
                    model: "IC-7300".to_string(),
                    port: "/dev/ttyUSB1".to_string(),
                    baud_rate: 19200,
                    ptt_method: pancetta_config::rig::PttMethod::Cat,
                },
            ],
            ..Default::default()
        };
        state.move_bookmark_selection_up();
        assert_eq!(state.selected_bookmark_idx, 0, "must clamp at 0, not wrap");

        state.move_bookmark_selection_down();
        assert_eq!(state.selected_bookmark_idx, 1);
        state.move_bookmark_selection_down();
        assert_eq!(
            state.selected_bookmark_idx, 1,
            "must clamp at the last index, not wrap"
        );
    }

    #[test]
    fn rig_selection_state_load_bookmark_populates_form_fields() {
        let mut state = RigSelectionState {
            available_ports: vec!["/dev/ttyUSB0".to_string()],
            bookmarks: vec![pancetta_config::rig::RigBookmark {
                name: "Shack".to_string(),
                model: "FTdx10".to_string(),
                port: "/dev/ttyUSB0".to_string(),
                baud_rate: 38400,
                ptt_method: pancetta_config::rig::PttMethod::Cat,
            }],
            ..Default::default()
        };
        state.load_bookmark(0);
        assert_eq!(state.model, "FTdx10");
        assert_eq!(state.selected_port(), "/dev/ttyUSB0");
        assert_eq!(state.selected_baud(), 38400);
        assert!(matches!(
            state.selected_ptt(),
            pancetta_config::rig::PttMethod::Cat
        ));
    }

    #[test]
    fn rig_selection_state_load_bookmark_prepends_unenumerated_port() {
        let mut state = RigSelectionState {
            available_ports: vec!["/dev/ttyUSB0".to_string()],
            bookmarks: vec![pancetta_config::rig::RigBookmark {
                name: "Remote".to_string(),
                model: "FTdx10".to_string(),
                port: "remote-rig.example:4532".to_string(),
                baud_rate: 38400,
                ptt_method: pancetta_config::rig::PttMethod::None,
            }],
            ..Default::default()
        };
        state.load_bookmark(0);
        assert_eq!(state.selected_port(), "remote-rig.example:4532");
        assert!(state
            .available_ports
            .contains(&"remote-rig.example:4532".to_string()));
    }

    #[test]
    fn rig_selection_state_load_bookmark_out_of_range_is_a_no_op() {
        let mut state = RigSelectionState {
            model: "Unchanged".to_string(),
            ..Default::default()
        };
        state.load_bookmark(5);
        assert_eq!(state.model, "Unchanged");
    }

    #[tokio::test]
    async fn apply_rig_config_update_seeds_bookmarks() {
        let mut app = fixture_app().await;
        app.apply_rig_config_update(
            vec!["/dev/ttyUSB0".to_string()],
            "FTdx10".to_string(),
            "/dev/ttyUSB0".to_string(),
            38400,
            pancetta_config::rig::PttMethod::None,
            vec![pancetta_config::rig::RigBookmark {
                name: "Shack".to_string(),
                model: "FTdx10".to_string(),
                port: "/dev/ttyUSB0".to_string(),
                baud_rate: 38400,
                ptt_method: pancetta_config::rig::PttMethod::None,
            }],
        );
        assert_eq!(app.rig_selection.bookmarks.len(), 1);
        assert_eq!(app.rig_selection.bookmarks[0].name, "Shack");
    }
```

Update the two pre-existing `apply_rig_config_update` tests, `apply_rig_config_update_preselects_current_values` and `apply_rig_config_update_preserves_unenumerated_current_port`, appending a trailing `vec![]` argument to each existing call, e.g.:

```rust
        app.apply_rig_config_update(
            vec!["/dev/ttyUSB0".to_string(), "/dev/ttyUSB1".to_string()],
            "FTdx10".to_string(),
            "/dev/ttyUSB1".to_string(),
            38400,
            pancetta_config::rig::PttMethod::Serial,
            vec![],
        );
```

(apply the same trailing `vec![]` to the second test's call).

- [ ] **Step 2: Write the failing tests in `pancetta-tui/src/tui_runner.rs`**

Add to `mod pan_59_command_tests`, after `rig_config_update_message_carries_ports_and_current_values`:

```rust
    #[test]
    fn rig_config_update_message_carries_bookmarks() {
        let msg = TuiMessage::RigConfigUpdate {
            available_ports: vec![],
            current_model: "FTdx10".to_string(),
            current_port: "/dev/ttyUSB0".to_string(),
            current_baud_rate: 38400,
            current_ptt_method: pancetta_config::rig::PttMethod::None,
            bookmarks: vec![pancetta_config::rig::RigBookmark {
                name: "Shack".to_string(),
                model: "FTdx10".to_string(),
                port: "/dev/ttyUSB0".to_string(),
                baud_rate: 38400,
                ptt_method: pancetta_config::rig::PttMethod::None,
            }],
        };
        match msg {
            TuiMessage::RigConfigUpdate { bookmarks, .. } => {
                assert_eq!(bookmarks.len(), 1);
                assert_eq!(bookmarks[0].name, "Shack");
            }
            _ => panic!("expected RigConfigUpdate"),
        }
    }

    #[test]
    fn rig_bookmarks_update_message_carries_bookmarks() {
        let msg = TuiMessage::RigBookmarksUpdate {
            bookmarks: vec![pancetta_config::rig::RigBookmark {
                name: "Portable".to_string(),
                model: "IC-7300".to_string(),
                port: "/dev/ttyUSB1".to_string(),
                baud_rate: 19200,
                ptt_method: pancetta_config::rig::PttMethod::Vox,
            }],
        };
        match msg {
            TuiMessage::RigBookmarksUpdate { bookmarks } => {
                assert_eq!(bookmarks.len(), 1);
                assert_eq!(bookmarks[0].name, "Portable");
            }
            _ => panic!("expected RigBookmarksUpdate"),
        }
    }

    #[test]
    fn save_rig_bookmark_command_round_trips_all_fields() {
        let cmd = TuiCommand::SaveRigBookmark {
            name: "Shack".to_string(),
            model: "FTdx10".to_string(),
            port: "/dev/ttyUSB0".to_string(),
            baud_rate: 38400,
            ptt_method: pancetta_config::rig::PttMethod::Cat,
        };
        match cmd {
            TuiCommand::SaveRigBookmark {
                name,
                model,
                port,
                baud_rate,
                ptt_method,
            } => {
                assert_eq!(name, "Shack");
                assert_eq!(model, "FTdx10");
                assert_eq!(port, "/dev/ttyUSB0");
                assert_eq!(baud_rate, 38400);
                assert!(matches!(ptt_method, pancetta_config::rig::PttMethod::Cat));
            }
            _ => panic!("expected SaveRigBookmark"),
        }
    }

    #[test]
    fn delete_rig_bookmark_command_carries_name() {
        let cmd = TuiCommand::DeleteRigBookmark {
            name: "Shack".to_string(),
        };
        match cmd {
            TuiCommand::DeleteRigBookmark { name } => assert_eq!(name, "Shack"),
            _ => panic!("expected DeleteRigBookmark"),
        }
    }
```

- [ ] **Step 3: Run the tests to verify they fail to compile**

Run: `cargo test -p pancetta-tui rig_selection_state_bookmark_name_input_text_editing`
Expected: FAIL — `no field \`bookmarks\` on type \`RigSelectionState\`` (and similar for every other new field/method/variant referenced above)

- [ ] **Step 4: Add the new `RigSelectionState` fields**

In `pancetta-tui/src/app.rs`, add to the `RigSelectionState` struct body (after `committed_ptt_idx: usize,`):

```rust
    /// Saved rig-config bookmarks (PAN-61), seeded from the coordinator's
    /// `RigConfigUpdate`/`RigBookmarksUpdate` pushes.
    pub bookmarks: Vec<pancetta_config::rig::RigBookmark>,
    /// Whether the `b`-opened "load bookmark" overlay is showing.
    pub bookmark_overlay_visible: bool,
    /// Index into `bookmarks` currently highlighted in the overlay.
    pub selected_bookmark_idx: usize,
    /// Whether the `s`-opened "name this bookmark" input is showing.
    pub naming_bookmark: bool,
    /// Text typed into the bookmark-name input.
    pub bookmark_name_input: String,
```

Add matching lines to `impl Default for RigSelectionState`'s struct literal (after `committed_ptt_idx: 0,`):

```rust
            bookmarks: Vec::new(),
            bookmark_overlay_visible: false,
            selected_bookmark_idx: 0,
            naming_bookmark: false,
            bookmark_name_input: String::new(),
```

- [ ] **Step 5: Add the new `RigSelectionState` methods**

In `impl RigSelectionState`, add after `pop_model_char`:

```rust
    /// Append a character to the bookmark-name input (PAN-61), meaningful
    /// only while `naming_bookmark` is true.
    pub fn push_bookmark_name_char(&mut self, c: char) {
        self.bookmark_name_input.push(c);
    }

    /// Backspace the bookmark-name input.
    pub fn pop_bookmark_name_char(&mut self) {
        self.bookmark_name_input.pop();
    }

    /// Move the bookmark-overlay selection up (Up arrow). Clamps at 0.
    pub fn move_bookmark_selection_up(&mut self) {
        if self.selected_bookmark_idx > 0 {
            self.selected_bookmark_idx -= 1;
        }
    }

    /// Move the bookmark-overlay selection down (Down arrow). Clamps at the
    /// last index.
    pub fn move_bookmark_selection_down(&mut self) {
        if self.selected_bookmark_idx + 1 < self.bookmarks.len() {
            self.selected_bookmark_idx += 1;
        }
    }

    /// Load the bookmark at `idx` into the 4 form fields (PAN-61). Does
    /// **not** apply anything live — only the form's own Enter does that.
    /// Out-of-range `idx` is a no-op. Mirrors
    /// `App::apply_rig_config_update`'s port/baud/PTT index-resolution
    /// logic so an unenumerated bookmarked port is never silently dropped.
    pub fn load_bookmark(&mut self, idx: usize) {
        let Some(bookmark) = self.bookmarks.get(idx).cloned() else {
            return;
        };
        self.model = bookmark.model;
        self.selected_port_idx = match self
            .available_ports
            .iter()
            .position(|p| *p == bookmark.port)
        {
            Some(pos) => pos,
            None => {
                if !bookmark.port.is_empty() {
                    self.available_ports.insert(0, bookmark.port.clone());
                }
                0
            }
        };
        self.selected_baud_idx = RIG_BAUD_RATES
            .iter()
            .position(|b| *b == bookmark.baud_rate)
            .unwrap_or(1);
        self.selected_ptt_idx = rig_ptt_methods()
            .iter()
            .position(|m| format!("{:?}", m) == format!("{:?}", bookmark.ptt_method))
            .unwrap_or(0);
    }
```

- [ ] **Step 6: Update `App::apply_rig_config_update`'s signature and body**

In `pancetta-tui/src/app.rs`, change the `apply_rig_config_update` signature:

```rust
    pub fn apply_rig_config_update(
        &mut self,
        mut available_ports: Vec<String>,
        current_model: String,
        current_port: String,
        current_baud_rate: u32,
        current_ptt_method: pancetta_config::rig::PttMethod,
        bookmarks: Vec<pancetta_config::rig::RigBookmark>,
    ) {
```

(signature only — the body is unchanged except for one new line). Add, right before the closing `}` of the function (after the existing `self.rig_selection.snapshot_committed();` line):

```rust
        self.rig_selection.bookmarks = bookmarks;
```

- [ ] **Step 7: Extend `TuiMessage::RigConfigUpdate` and add `RigBookmarksUpdate`**

In `pancetta-tui/src/tui_runner.rs`, change the `RigConfigUpdate` variant:

```rust
    RigConfigUpdate {
        available_ports: Vec<String>,
        current_model: String,
        current_port: String,
        current_baud_rate: u32,
        current_ptt_method: pancetta_config::rig::PttMethod,
        /// Saved rig-config bookmarks (PAN-61), pushed once at startup
        /// alongside the live values so the `b` load overlay has data from
        /// frame 1.
        bookmarks: Vec<pancetta_config::rig::RigBookmark>,
    },
```

Add a new variant immediately after it (before `RigStatusUpdate`):

```rust
    /// Saved rig-config bookmarks changed (PAN-61: after a `SaveRigBookmark`
    /// or `DeleteRigBookmark` command). Full resync, mirroring
    /// `DxWatchlistUpdate`'s bulk-replace style — never a diff.
    RigBookmarksUpdate {
        bookmarks: Vec<pancetta_config::rig::RigBookmark>,
    },
```

- [ ] **Step 8: Add the new `TuiCommand` variants**

In `pancetta-tui/src/tui_runner.rs`, add immediately after the `SelectRig { .. }` variant (before `/// User requested quit`):

```rust
    /// Save the rig picker's current 4 form values as a named bookmark
    /// (PAN-61). Save-as-name semantics: overwrites an existing bookmark
    /// with the same name, else appends. Pure config-list mutation — never
    /// touches the live rig connection or Hamlib reconnect path.
    SaveRigBookmark {
        name: String,
        model: String,
        port: String,
        baud_rate: u32,
        ptt_method: pancetta_config::rig::PttMethod,
    },
    /// Delete a saved rig-config bookmark by name (PAN-61). No-op if the
    /// name doesn't match any saved bookmark.
    DeleteRigBookmark { name: String },
```

- [ ] **Step 9: Update `process_messages`' `RigConfigUpdate` arm and add a `RigBookmarksUpdate` arm**

In `pancetta-tui/src/tui_runner.rs`, change the existing arm:

```rust
            TuiMessage::RigConfigUpdate {
                available_ports,
                current_model,
                current_port,
                current_baud_rate,
                current_ptt_method,
                bookmarks,
            } => {
                app.apply_rig_config_update(
                    available_ports,
                    current_model,
                    current_port,
                    current_baud_rate,
                    current_ptt_method,
                    bookmarks,
                );
```

(the line after it, closing the arm, is unchanged). Add a new arm immediately after it:

```rust
            TuiMessage::RigBookmarksUpdate { bookmarks } => {
                if app.rig_selection.selected_bookmark_idx >= bookmarks.len() {
                    app.rig_selection.selected_bookmark_idx = 0;
                }
                app.rig_selection.bookmarks = bookmarks;
            }
```

- [ ] **Step 10: Run every test written in this task to verify they pass**

Run: `cargo test -p pancetta-tui rig_selection_state_ apply_rig_config_update_ rig_config_update_message_carries_bookmarks rig_bookmarks_update_message_carries_bookmarks save_rig_bookmark_command_round_trips_all_fields delete_rig_bookmark_command_carries_name`
Expected: PASS (all of them)

- [ ] **Step 11: Run the full `pancetta-tui` test suite**

Run: `cargo test -p pancetta-tui`
Expected: PASS (no regressions)

- [ ] **Step 12: Commit**

```bash
git add pancetta-tui/src/app.rs pancetta-tui/src/tui_runner.rs
git commit -m "feat(tui): PAN-61 add bookmark state and message/command plumbing"
```

---

### Task 4: Key handling — `b` load overlay, `s` save-name input

**Files:**
- Modify: `pancetta-tui/src/tui_runner.rs`

**Interfaces:**
- Consumes: `RigSelectionState::{load_bookmark, move_bookmark_selection_up, move_bookmark_selection_down, push_bookmark_name_char, pop_bookmark_name_char}` (Task 3); `TuiCommand::{SaveRigBookmark, DeleteRigBookmark}` (Task 3).
- Produces: `TuiRunner::handle_key_event` recognizes `b`/`s` (form view), plus overlay/name-input key handling, all gated behind `app.rig_selection.visible`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `pancetta-tui/src/tui_runner.rs`, after `rig_modal_esc_then_reopen_and_enter_applies_original_values_not_abandoned_edit`:

```rust
    #[tokio::test]
    async fn rig_modal_b_opens_bookmark_overlay_and_enter_loads_without_applying() {
        let (mut r, cmd_rx, app) = make_runner().await;
        {
            let mut app = app.write().await;
            app.rig_selection.visible = true;
            app.rig_selection.available_ports = vec!["/dev/ttyUSB0".to_string()];
            app.rig_selection.bookmarks = vec![pancetta_config::rig::RigBookmark {
                name: "Shack".to_string(),
                model: "FTdx10".to_string(),
                port: "/dev/ttyUSB0".to_string(),
                baud_rate: 38400,
                ptt_method: pancetta_config::rig::PttMethod::Cat,
            }];
        }

        r.handle_key_event(key('b')).await.unwrap();
        assert!(app.read().await.rig_selection.bookmark_overlay_visible);

        r.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();

        let app = app.read().await;
        assert!(
            !app.rig_selection.bookmark_overlay_visible,
            "Enter must close the overlay"
        );
        assert_eq!(app.rig_selection.model, "FTdx10", "must load into the form");
        assert!(
            cmd_rx.try_recv().is_err(),
            "loading a bookmark must never send SelectRig itself"
        );
    }

    #[tokio::test]
    async fn rig_modal_b_overlay_esc_cancels_without_loading() {
        let (mut r, _cmd_rx, app) = make_runner().await;
        {
            let mut app = app.write().await;
            app.rig_selection.visible = true;
            app.rig_selection.model = "Original".to_string();
            app.rig_selection.bookmarks = vec![pancetta_config::rig::RigBookmark {
                name: "Shack".to_string(),
                model: "FTdx10".to_string(),
                port: "/dev/ttyUSB0".to_string(),
                baud_rate: 38400,
                ptt_method: pancetta_config::rig::PttMethod::None,
            }];
        }
        r.handle_key_event(key('b')).await.unwrap();
        r.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();

        let app = app.read().await;
        assert!(!app.rig_selection.bookmark_overlay_visible);
        assert!(
            app.rig_selection.visible,
            "Esc on the overlay must return to the form, not close the whole modal"
        );
        assert_eq!(app.rig_selection.model, "Original", "must not have loaded anything");
    }

    #[tokio::test]
    async fn rig_modal_b_overlay_x_sends_delete_rig_bookmark() {
        let (mut r, cmd_rx, app) = make_runner().await;
        {
            let mut app = app.write().await;
            app.rig_selection.visible = true;
            app.rig_selection.bookmarks = vec![pancetta_config::rig::RigBookmark {
                name: "Shack".to_string(),
                model: "FTdx10".to_string(),
                port: "/dev/ttyUSB0".to_string(),
                baud_rate: 38400,
                ptt_method: pancetta_config::rig::PttMethod::None,
            }];
        }
        r.handle_key_event(key('b')).await.unwrap();
        r.handle_key_event(key('x')).await.unwrap();

        match cmd_rx.try_recv() {
            Ok(TuiCommand::DeleteRigBookmark { name }) => assert_eq!(name, "Shack"),
            other => panic!("expected DeleteRigBookmark, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn rig_modal_s_opens_name_input_and_enter_sends_save_rig_bookmark() {
        let (mut r, cmd_rx, app) = make_runner().await;
        {
            let mut app = app.write().await;
            app.rig_selection.visible = true;
            app.rig_selection.active_field = crate::app::RigField::Ptt;
            app.rig_selection.model = "FTdx10".to_string();
            app.rig_selection.available_ports = vec!["/dev/ttyUSB0".to_string()];
        }

        r.handle_key_event(key('s')).await.unwrap();
        assert!(app.read().await.rig_selection.naming_bookmark);

        r.handle_key_event(key('S')).await.unwrap();
        r.handle_key_event(key('K')).await.unwrap();
        r.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();

        assert!(!app.read().await.rig_selection.naming_bookmark);
        match cmd_rx.try_recv() {
            Ok(TuiCommand::SaveRigBookmark { name, model, .. }) => {
                assert_eq!(name, "SK");
                assert_eq!(model, "FTdx10");
            }
            other => panic!("expected SaveRigBookmark, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn rig_modal_s_while_editing_model_types_the_letter_instead() {
        let (mut r, _cmd_rx, app) = make_runner().await;
        {
            let mut app = app.write().await;
            app.rig_selection.visible = true;
            app.rig_selection.active_field = crate::app::RigField::Model;
            app.rig_selection.model = "FTdx10".to_string();
        }
        r.handle_key_event(key('s')).await.unwrap();
        let app = app.read().await;
        assert_eq!(
            app.rig_selection.model, "FTdx10s",
            "'s' typed into the Model field must edit the model, not open naming"
        );
        assert!(!app.rig_selection.naming_bookmark);
    }

    #[tokio::test]
    async fn rig_modal_s_name_input_enter_with_empty_name_refuses() {
        let (mut r, cmd_rx, app) = make_runner().await;
        {
            let mut app = app.write().await;
            app.rig_selection.visible = true;
        }
        r.handle_key_event(key('s')).await.unwrap();
        r.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();

        assert!(
            app.read().await.rig_selection.naming_bookmark,
            "must stay open on an empty name"
        );
        assert!(cmd_rx.try_recv().is_err());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pancetta-tui rig_modal_b_ rig_modal_s_`
Expected: FAIL (behavior not wired yet — 'b'/'s' currently no-op, so `bookmark_overlay_visible`/`naming_bookmark` never flip)

- [ ] **Step 3: Wire the key handling**

In `pancetta-tui/src/tui_runner.rs`, replace the whole `if app.rig_selection.visible { match key.code { ... } return Ok(true); }` block with:

```rust
        // If rig config picker modal is visible, route keys there (PAN-59;
        // bookmark overlay/name-input sub-states added PAN-61)
        if app.rig_selection.visible {
            if app.rig_selection.naming_bookmark {
                match key.code {
                    KeyCode::Esc => {
                        app.rig_selection.naming_bookmark = false;
                        app.rig_selection.bookmark_name_input.clear();
                        app.status_message = "Save bookmark cancelled".to_string();
                    }
                    KeyCode::Backspace => {
                        app.rig_selection.pop_bookmark_name_char();
                    }
                    KeyCode::Char(c) if !c.is_control() => {
                        app.rig_selection.push_bookmark_name_char(c);
                    }
                    KeyCode::Enter => {
                        let name = app.rig_selection.bookmark_name_input.trim().to_string();
                        if name.is_empty() {
                            app.status_message = "Bookmark name cannot be empty".to_string();
                        } else {
                            app.rig_selection.naming_bookmark = false;
                            app.rig_selection.bookmark_name_input.clear();
                            self.message_tx.send(TuiCommand::SaveRigBookmark {
                                name,
                                model: app.rig_selection.model.clone(),
                                port: app.rig_selection.selected_port(),
                                baud_rate: app.rig_selection.selected_baud(),
                                ptt_method: app.rig_selection.selected_ptt(),
                            })?;
                            app.status_message = "Saving bookmark…".to_string();
                        }
                    }
                    _ => {}
                }
                return Ok(true);
            }

            if app.rig_selection.bookmark_overlay_visible {
                match key.code {
                    KeyCode::Esc => {
                        app.rig_selection.bookmark_overlay_visible = false;
                        app.status_message = "Load bookmark cancelled".to_string();
                    }
                    KeyCode::Up => {
                        app.rig_selection.move_bookmark_selection_up();
                    }
                    KeyCode::Down => {
                        app.rig_selection.move_bookmark_selection_down();
                    }
                    KeyCode::Enter => {
                        if app.rig_selection.bookmarks.is_empty() {
                            app.status_message = "No saved bookmarks".to_string();
                        } else {
                            let idx = app.rig_selection.selected_bookmark_idx;
                            app.rig_selection.load_bookmark(idx);
                            app.rig_selection.bookmark_overlay_visible = false;
                            app.status_message = "Bookmark loaded — Enter to apply".to_string();
                        }
                    }
                    KeyCode::Char('x') => {
                        if let Some(bookmark) = app
                            .rig_selection
                            .bookmarks
                            .get(app.rig_selection.selected_bookmark_idx)
                        {
                            let name = bookmark.name.clone();
                            self.message_tx
                                .send(TuiCommand::DeleteRigBookmark { name })?;
                            app.status_message = "Deleting bookmark…".to_string();
                        }
                    }
                    _ => {}
                }
                return Ok(true);
            }

            match key.code {
                KeyCode::Esc => {
                    // I-3 fix (PAN-59 final review): restore the live
                    // fields from the committed snapshot BEFORE hiding the
                    // modal, so an abandoned edit (e.g. garbage typed into
                    // Model) doesn't linger in `RigSelectionState` to be
                    // picked up by a LATER, unrelated Enter press the next
                    // time the modal opens.
                    app.rig_selection.restore_committed();
                    app.rig_selection.visible = false;
                    app.status_message = "Rig config picker cancelled".to_string();
                }
                KeyCode::Tab => {
                    app.rig_selection.next_field();
                }
                KeyCode::Up => {
                    app.rig_selection.move_up();
                }
                KeyCode::Down => {
                    app.rig_selection.move_down();
                }
                KeyCode::Backspace
                    if app.rig_selection.active_field == crate::app::RigField::Model =>
                {
                    app.rig_selection.pop_model_char();
                }
                KeyCode::Char(c)
                    if app.rig_selection.active_field == crate::app::RigField::Model
                        && !c.is_control() =>
                {
                    app.rig_selection.push_model_char(c);
                }
                KeyCode::Char('b') => {
                    app.rig_selection.bookmark_overlay_visible = true;
                    app.rig_selection.selected_bookmark_idx = 0;
                    app.status_message =
                        "Load bookmark (Up/Down: select, Enter: load, x: delete, Esc: cancel)"
                            .to_string();
                }
                KeyCode::Char('s') => {
                    app.rig_selection.naming_bookmark = true;
                    app.rig_selection.bookmark_name_input.clear();
                    app.status_message = "Name this bookmark (Enter: save, Esc: cancel)".to_string();
                }
                KeyCode::Enter => {
                    let port = app.rig_selection.selected_port();
                    if port.is_empty() {
                        // I-2b fix (PAN-59 final review): no ports were
                        // enumerated (or none is selected) -- submitting
                        // now would persist `port = ""` into
                        // ~/.pancetta/pancetta.toml, destroying whatever
                        // was previously configured there. Refuse and keep
                        // the modal open instead.
                        app.status_message = "No port selected — cannot apply".to_string();
                    } else {
                        app.rig_selection.visible = false;
                        self.message_tx.send(TuiCommand::SelectRig {
                            model: app.rig_selection.model.clone(),
                            port,
                            baud_rate: app.rig_selection.selected_baud(),
                            ptt_method: app.rig_selection.selected_ptt(),
                        })?;
                        // I-3 fix (PAN-59 final review): sync the committed
                        // snapshot to the just-applied values so a later
                        // open/cancel cycle doesn't revert past them.
                        app.rig_selection.snapshot_committed();
                        app.status_message = "Applying rig config…".to_string();
                    }
                }
                _ => {}
            }
            return Ok(true);
```

(the closing brace of the outer `if app.rig_selection.visible { ... }` block, and everything after it, is unchanged — only the block's contents changed).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pancetta-tui rig_modal_`
Expected: PASS (all `rig_modal_*` tests, old and new)

- [ ] **Step 5: Run the full `pancetta-tui` test suite**

Run: `cargo test -p pancetta-tui`
Expected: PASS (no regressions)

- [ ] **Step 6: Commit**

```bash
git add pancetta-tui/src/tui_runner.rs
git commit -m "feat(tui): PAN-61 wire b/s keys for load/save rig bookmarks"
```

---

### Task 5: Rendering — bookmark overlay + name-input modals

**Files:**
- Modify: `pancetta-tui/src/tui_runner.rs`

**Interfaces:**
- Consumes: `RigSelectionState.{bookmarks, bookmark_overlay_visible, selected_bookmark_idx, naming_bookmark, bookmark_name_input}` (Tasks 3–4).
- Produces: `render_rig_selection_modal` dispatches to two new private helpers depending on state; no change to its own call site.

- [ ] **Step 1: Update `render_rig_selection_modal`'s footer and add the dispatch**

In `pancetta-tui/src/tui_runner.rs`, at the top of `render_rig_selection_modal` (right after the `use crate::app::RigField; use ratatui::text::{Line, Span};` lines), add:

```rust
        if state.naming_bookmark {
            Self::render_bookmark_name_input(f, area, state);
            return;
        }
        if state.bookmark_overlay_visible {
            Self::render_bookmark_overlay(f, area, state);
            return;
        }
```

Change the footer line inside the same function from:

```rust
        lines.push(Line::from(Span::styled(
            "Tab: next field | Up/Down: change | type: edit model | Enter: apply | Esc: cancel",
            Style::default().fg(Color::DarkGray),
        )));
```

to:

```rust
        lines.push(Line::from(Span::styled(
            "Tab: next | Up/Down: change | b: load | s: save | Enter: apply | Esc: cancel",
            Style::default().fg(Color::DarkGray),
        )));
```

- [ ] **Step 2: Add the two new render helpers**

In `pancetta-tui/src/tui_runner.rs`, add immediately after `render_rig_selection_modal`'s closing `}`:

```rust
    /// Render the "save as bookmark" name-input overlay (PAN-61), stacked
    /// on top of the rig-config modal. Mirrors
    /// `render_rig_selection_modal`'s sizing/centering/clear idiom.
    fn render_bookmark_name_input(f: &mut Frame, area: Rect, state: &crate::app::RigSelectionState) {
        use ratatui::text::{Line, Span};

        if area.width < 10 || area.height < 4 {
            return;
        }
        let modal_width = (area.width * 2 / 5).clamp(30, 50).min(area.width);
        let modal_height = 5u16.min(area.height);

        let modal_area = Rect {
            x: (area.width.saturating_sub(modal_width)) / 2,
            y: (area.height.saturating_sub(modal_height)) / 2,
            width: modal_width,
            height: modal_height,
        };

        f.render_widget(ratatui::widgets::Clear, modal_area);

        let outer_block = Block::default()
            .title(" Save Bookmark ")
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .style(Style::default().bg(Color::Black).fg(Color::White));

        let inner = outer_block.inner(modal_area);
        f.render_widget(outer_block, modal_area);

        let lines = vec![
            Line::from(vec![
                Span::raw("Name: "),
                Span::styled(
                    state.bookmark_name_input.clone(),
                    Style::default()
                        .bg(Color::Cyan)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Enter: save | Esc: cancel",
                Style::default().fg(Color::DarkGray),
            )),
        ];

        let paragraph = Paragraph::new(lines);
        f.render_widget(paragraph, inner);
    }

    /// Render the "load bookmark" list overlay (PAN-61), stacked on top of
    /// the rig-config modal. Mirrors `render_device_selection_modal`'s
    /// list-select idiom.
    fn render_bookmark_overlay(f: &mut Frame, area: Rect, state: &crate::app::RigSelectionState) {
        use ratatui::text::{Line, Span};

        if area.width < 10 || area.height < 4 {
            return;
        }
        let modal_width = (area.width * 3 / 5).clamp(40, 70).min(area.width);
        let modal_height = (state.bookmarks.len() as u16 + 5).clamp(6, area.height);

        let modal_area = Rect {
            x: (area.width.saturating_sub(modal_width)) / 2,
            y: (area.height.saturating_sub(modal_height)) / 2,
            width: modal_width,
            height: modal_height,
        };

        f.render_widget(ratatui::widgets::Clear, modal_area);

        let outer_block = Block::default()
            .title(" Load Bookmark ")
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .style(Style::default().bg(Color::Black).fg(Color::White));

        let inner = outer_block.inner(modal_area);
        f.render_widget(outer_block, modal_area);

        let mut lines: Vec<Line> = Vec::with_capacity(state.bookmarks.len().max(1) + 2);
        if state.bookmarks.is_empty() {
            lines.push(Line::from(
                "(no saved bookmarks — press 's' from the form to save one)",
            ));
        } else {
            for (idx, bookmark) in state.bookmarks.iter().enumerate() {
                let selected = idx == state.selected_bookmark_idx;
                let style = if selected {
                    Style::default()
                        .bg(Color::Cyan)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::from(Span::styled(
                    format!(
                        "{} — {} @ {}, {}, {:?}",
                        bookmark.name,
                        bookmark.model,
                        bookmark.port,
                        bookmark.baud_rate,
                        bookmark.ptt_method
                    ),
                    style,
                )));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Up/Down: select | Enter: load | x: delete | Esc: cancel",
            Style::default().fg(Color::DarkGray),
        )));

        let paragraph = Paragraph::new(lines);
        f.render_widget(paragraph, inner);
    }
```

- [ ] **Step 3: Update the `i`-key status message to mention the new keys**

In `pancetta-tui/src/tui_runner.rs`, change the `KeyCode::Char('i')` arm's status message from:

```rust
                app.status_message =
                    "Edit rig config (Tab: next field, Up/Down: change, type: edit model, Enter: apply, Esc: cancel)"
                        .to_string();
```

to:

```rust
                app.status_message =
                    "Edit rig config (Tab: next, Up/Down: change, b: load bookmark, s: save bookmark, Enter: apply, Esc: cancel)"
                        .to_string();
```

- [ ] **Step 4: Build and run the full `pancetta-tui` test suite**

Run: `cargo build -p pancetta-tui && cargo test -p pancetta-tui`
Expected: builds clean, all tests PASS (no automated render/snapshot coverage exists for this modal — `render_rig_selection_modal` and its siblings are exercised at runtime only, matching the existing untested-render-path convention for this modal family)

- [ ] **Step 5: Commit**

```bash
git add pancetta-tui/src/tui_runner.rs
git commit -m "feat(tui): PAN-61 render bookmark load/save overlays"
```

---

### Task 6: Coordinator pure helpers — upsert + soft-cap status message

**Files:**
- Modify: `pancetta/src/coordinator/tui_relay.rs`

**Interfaces:**
- Consumes: `pancetta_config::rig::RigBookmark` (Task 1).
- Produces: `upsert_rig_bookmark(bookmarks: &mut Vec<pancetta_config::rig::RigBookmark>, bookmark: pancetta_config::rig::RigBookmark)`; `rig_bookmark_saved_status(name: &str, count: usize) -> String`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `pancetta/src/coordinator/tui_relay.rs`, after `not_in_flight`'s closing `}` (the test-fixture-helper cluster, before the `#[test] fn ptt_on_refusal_...` tests):

```rust
    #[test]
    fn upsert_rig_bookmark_appends_when_name_is_new() {
        let mut bookmarks = vec![pancetta_config::rig::RigBookmark {
            name: "Shack".to_string(),
            model: "FTdx10".to_string(),
            port: "/dev/ttyUSB0".to_string(),
            baud_rate: 38400,
            ptt_method: pancetta_config::rig::PttMethod::Cat,
        }];
        upsert_rig_bookmark(
            &mut bookmarks,
            pancetta_config::rig::RigBookmark {
                name: "Portable".to_string(),
                model: "IC-7300".to_string(),
                port: "/dev/ttyUSB1".to_string(),
                baud_rate: 19200,
                ptt_method: pancetta_config::rig::PttMethod::Vox,
            },
        );
        assert_eq!(bookmarks.len(), 2);
        assert_eq!(bookmarks[1].name, "Portable");
    }

    #[test]
    fn upsert_rig_bookmark_overwrites_when_name_matches() {
        let mut bookmarks = vec![pancetta_config::rig::RigBookmark {
            name: "Shack".to_string(),
            model: "FTdx10".to_string(),
            port: "/dev/ttyUSB0".to_string(),
            baud_rate: 38400,
            ptt_method: pancetta_config::rig::PttMethod::Cat,
        }];
        upsert_rig_bookmark(
            &mut bookmarks,
            pancetta_config::rig::RigBookmark {
                name: "Shack".to_string(),
                model: "IC-7300".to_string(),
                port: "/dev/ttyUSB1".to_string(),
                baud_rate: 19200,
                ptt_method: pancetta_config::rig::PttMethod::Vox,
            },
        );
        assert_eq!(bookmarks.len(), 1, "must overwrite, not append a duplicate name");
        assert_eq!(bookmarks[0].model, "IC-7300");
    }

    #[test]
    fn rig_bookmark_saved_status_under_cap_has_no_warning() {
        let status = rig_bookmark_saved_status("Shack", 5);
        assert_eq!(status, "Saved bookmark 'Shack'");
    }

    #[test]
    fn rig_bookmark_saved_status_at_cap_warns_but_still_reports_success() {
        let status = rig_bookmark_saved_status("Shack", 20);
        assert!(status.starts_with("Saved bookmark 'Shack'"));
        assert!(status.contains("consider deleting"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pancetta upsert_rig_bookmark rig_bookmark_saved_status`
Expected: FAIL — `cannot find function \`upsert_rig_bookmark\``/`\`rig_bookmark_saved_status\` in this scope`

- [ ] **Step 3: Implement the two helpers**

In `pancetta/src/coordinator/tui_relay.rs`, add near the top of the file, alongside other free functions (or immediately above the `#[cfg(test)] mod tests` block if the file has no other top-level free-function cluster — place them at module scope, outside `impl` blocks, so both production code and tests can call them unqualified):

```rust
/// Upsert a bookmark into a list by name (PAN-61: save-as, overwrite if the
/// name already exists). Extracted as a pure function so the semantics are
/// unit-testable outside the coordinator's spawned command-relay task —
/// mirrors the `ptt_on_refusal`/`map_qso_snapshot_item` pure-helper pattern
/// already used in this file's test module.
fn upsert_rig_bookmark(
    bookmarks: &mut Vec<pancetta_config::rig::RigBookmark>,
    bookmark: pancetta_config::rig::RigBookmark,
) {
    if let Some(existing) = bookmarks.iter_mut().find(|b| b.name == bookmark.name) {
        *existing = bookmark;
    } else {
        bookmarks.push(bookmark);
    }
}

/// Build the operator-facing status message after a bookmark save
/// (PAN-61). The 20-bookmark soft cap is advisory only: `count` past the
/// cap never blocks the save, it just appends a warning.
fn rig_bookmark_saved_status(name: &str, count: usize) -> String {
    if count >= 20 {
        format!(
            "Saved bookmark '{}' ({} bookmarks saved — consider deleting unused ones)",
            name, count
        )
    } else {
        format!("Saved bookmark '{}'", name)
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pancetta upsert_rig_bookmark rig_bookmark_saved_status`
Expected: PASS (4 tests)

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/coordinator/tui_relay.rs
git commit -m "feat(coordinator): PAN-61 add pure bookmark upsert/status helpers"
```

---

### Task 7: Coordinator wiring — `SaveRigBookmark`/`DeleteRigBookmark` handlers + startup push

**Files:**
- Modify: `pancetta/src/coordinator/tui_relay.rs`

**Interfaces:**
- Consumes: `upsert_rig_bookmark`, `rig_bookmark_saved_status` (Task 6); `TuiCommand::{SaveRigBookmark, DeleteRigBookmark}` (Task 3); `Config::set_rig_bookmarks_in_file` (Task 2); `TuiMessage::RigBookmarksUpdate` (Task 3).
- Produces: the coordinator persists and live-syncs bookmark saves/deletes; the startup `RigConfigUpdate` push carries the operator's saved bookmarks.

- [ ] **Step 1: Extend the startup `RigConfigUpdate` push**

In `pancetta/src/coordinator/tui_relay.rs`, in the startup block that builds and sends `TuiMessage::RigConfigUpdate` (the one reading `cfg.rig.model`, `.interface.port`, etc.), add `cfg.rig.bookmarks.clone()` to the tuple read under the config read-lock and pass it through:

```rust
                let (current_model, current_port, current_baud_rate, current_ptt_method, bookmarks) = {
                    let cfg = cmd_config.read().await;
                    (
                        cfg.rig.model.clone(),
                        cfg.rig.interface.port.clone(),
                        cfg.rig.interface.baud_rate,
                        cfg.rig.ptt.method.clone(),
                        cfg.rig.bookmarks.clone(),
                    )
                };
```

and add `bookmarks,` to the `TuiMessage::RigConfigUpdate { ... }` construction below it:

```rust
                if let Err(e) =
                    cmd_tui_msg_tx.send(pancetta_tui::tui_runner::TuiMessage::RigConfigUpdate {
                        available_ports,
                        current_model,
                        current_port,
                        current_baud_rate,
                        current_ptt_method,
                        bookmarks,
                    })
                {
                    debug!("Failed to send initial rig config to TUI: {}", e);
                }
```

- [ ] **Step 2: `cargo build -p pancetta` to confirm the startup-push change compiles**

Run: `cargo build -p pancetta 2>&1 | tail -40`
Expected: builds clean (this is the only other `TuiMessage::RigConfigUpdate` construction site in the coordinator; if the build surfaces another one, fix it the same way — read `cfg.rig.bookmarks.clone()` and pass it through)

- [ ] **Step 3: Add the `SaveRigBookmark` and `DeleteRigBookmark` match arms**

In `pancetta/src/coordinator/tui_relay.rs`, add immediately after the `TuiCommand::SelectRig { .. } => { ... }` arm's closing `}` (before the `TuiCommand::ToggleFoxMode` arm):

```rust
                        pancetta_tui::tui_runner::TuiCommand::SaveRigBookmark {
                            name,
                            model,
                            port,
                            baud_rate,
                            ptt_method,
                        } => {
                            info!(
                                "TUI SaveRigBookmark: name={} model={} port={} baud={} ptt={:?}",
                                name, model, port, baud_rate, ptt_method
                            );
                            let bookmark = pancetta_config::rig::RigBookmark {
                                name: name.clone(),
                                model,
                                port,
                                baud_rate,
                                ptt_method,
                            };
                            let bookmarks = {
                                let mut cfg = cmd_config.write().await;
                                upsert_rig_bookmark(&mut cfg.rig.bookmarks, bookmark);
                                cfg.rig.bookmarks.clone()
                            };
                            let config_path = dirs::home_dir()
                                .unwrap_or_else(|| std::path::PathBuf::from("."))
                                .join(".pancetta")
                                .join("pancetta.toml");
                            let persist_result = {
                                let cfg = cmd_config.read().await;
                                cfg.set_rig_bookmarks_in_file(&config_path, &bookmarks)
                            };
                            let status = if let Err(e) = persist_result {
                                warn!("Failed to persist rig bookmark: {}", e);
                                format!("Failed to save bookmark '{}': {}", name, e)
                            } else {
                                info!(
                                    "Persisted rig bookmark '{}' to {}",
                                    name,
                                    config_path.display()
                                );
                                rig_bookmark_saved_status(&name, bookmarks.len())
                            };
                            let _ = cmd_tui_msg_tx.send(
                                pancetta_tui::tui_runner::TuiMessage::RigBookmarksUpdate {
                                    bookmarks: bookmarks.clone(),
                                },
                            );
                            let _ = cmd_tui_msg_tx.send(
                                pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                    component: "rig".to_string(),
                                    status,
                                },
                            );
                        }
                        pancetta_tui::tui_runner::TuiCommand::DeleteRigBookmark { name } => {
                            info!("TUI DeleteRigBookmark: name={}", name);
                            let (bookmarks, found) = {
                                let mut cfg = cmd_config.write().await;
                                let before = cfg.rig.bookmarks.len();
                                cfg.rig.bookmarks.retain(|b| b.name != name);
                                let found = cfg.rig.bookmarks.len() != before;
                                (cfg.rig.bookmarks.clone(), found)
                            };
                            if !found {
                                let _ = cmd_tui_msg_tx.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: "rig".to_string(),
                                        status: format!("No bookmark named '{}'", name),
                                    },
                                );
                            } else {
                                let config_path = dirs::home_dir()
                                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                                    .join(".pancetta")
                                    .join("pancetta.toml");
                                let persist_result = {
                                    let cfg = cmd_config.read().await;
                                    cfg.set_rig_bookmarks_in_file(&config_path, &bookmarks)
                                };
                                let status = if let Err(e) = persist_result {
                                    warn!("Failed to persist rig bookmark deletion: {}", e);
                                    format!("Failed to delete bookmark '{}': {}", name, e)
                                } else {
                                    info!(
                                        "Persisted rig bookmark deletion of '{}' to {}",
                                        name,
                                        config_path.display()
                                    );
                                    format!("Deleted bookmark '{}'", name)
                                };
                                let _ = cmd_tui_msg_tx.send(
                                    pancetta_tui::tui_runner::TuiMessage::RigBookmarksUpdate {
                                        bookmarks: bookmarks.clone(),
                                    },
                                );
                                let _ = cmd_tui_msg_tx.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: "rig".to_string(),
                                        status,
                                    },
                                );
                            }
                        }
```

- [ ] **Step 4: Build**

Run: `cargo build -p pancetta 2>&1 | tail -60`
Expected: builds clean

- [ ] **Step 5: Run the full `pancetta` crate test suite**

Run: `cargo test -p pancetta`
Expected: PASS (no regressions; this includes the Task 6 pure-helper tests and every pre-existing `tui_relay.rs`/coordinator test)

- [ ] **Step 6: Commit**

```bash
git add pancetta/src/coordinator/tui_relay.rs
git commit -m "feat(coordinator): PAN-61 wire SaveRigBookmark/DeleteRigBookmark handlers"
```

---

### Task 8: Workspace-wide verification + docs

**Files:**
- Modify: `CHANGELOG.md` (add an entry)
- No other files — this task is verification + changelog only.

- [ ] **Step 1: Format and lint**

Run: `cargo fmt --all -- --check`
Expected: no diff (if it finds one, run `cargo fmt --all` and re-check)

Run: `cargo clippy --workspace --all-targets --features transmit -- -D warnings`
Expected: no warnings

- [ ] **Step 2: Full workspace test suite**

Run: `cargo test --workspace --features transmit`
Expected: PASS (all crates, including the loopback and hamlib suites)

- [ ] **Step 3: Add a `CHANGELOG.md` entry**

Add a new `### Added` bullet under the current unreleased section (top of the file) — read the file's existing top few entries first to match its exact heading style, then add:

```markdown
- Saved rig-config bookmarks (PAN-61): the `i` rig picker can now save the current model/port/baud/PTT as a named bookmark (`s`) and load one back into the form later (`b`), without retyping. Builds on PAN-59's live rig-config switch.
```

- [ ] **Step 4: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs: PAN-61 changelog entry for saved rig-config bookmarks"
```

---

## After implementation

- Move PAN-61 to **In Review** in Linear once the PR is open (`linearis issues update PAN-61 --status "In Review"`), and to **Done** once merged.
- Open the PR via `/catalyst-dev:create-pr` (or `gh pr create` if that plugin isn't installed in this session), then use the `land-pr` skill to merge once CI is green and review threads are resolved (Mergify queue — `@Mergifyio queue`, never `gh pr merge` directly).
- On-air validation (saving a bookmark, restarting pancetta, reloading it) is operator-owed, same as PAN-59's own on-air re-verify — note it as outstanding, don't block the PR on it.
